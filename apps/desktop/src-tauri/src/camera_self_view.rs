#![cfg(target_os = "windows")]

//! Native camera → webview self-view feed: the Media Foundation
//! camera's NV12 frames are copied into a single latest-wins buffer and
//! pulled by the meeting route's self-view canvas via `next_self_view_frame`.
//!
//! This replaces the old SFU round trip (MF capture → H.264 publish → SFU
//! relay → hidden webview subscriber → `<video>` tile) with a same-process
//! feed: no encode/SFU/decode/network latency, no freeze watchdog, and the
//! camera light is on exactly once (one capture client).
//!
//! Buffer layout (little-endian, all planes tightly packed, stride == width):
//! `[width: u32][height: u32][capture_wall_time_us: u64][y: w*h][uv: w*h/2]`
//! — 16-byte header + NV12 payload. The webview builds a WebCodecs
//! `VideoFrame` directly from the payload with `format: 'NV12'` and the two
//! strides in the header's dimensions.

use std::sync::Mutex;

use crate::sync_ext::MutexExt;

/// Largest long edge a self-view frame may have before it is downscaled. The
/// preview tile is a few hundred pixels wide; shipping full capture
/// resolution (1.4 MB at 720p, 3.1 MB at 1080p) through the buffer + IPC at
/// 60 fps was ~83-186 MB/s of copies the preview cannot use.
const PREVIEW_MAX_LONG_EDGE: u32 = 640;

/// Halve an NV12 frame in both dimensions with a 2x2 box filter. Planes are
/// tightly packed (stride == width, the Windows MF contract this module
/// already relies on); the chroma plane holds one interleaved U/V pair per
/// 2x2 luma block, so a luma halving pairs with a chroma halving.
fn halve_nv12(y: &[u8], uv: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>, u32, u32) {
    let out_width = width / 2;
    let out_height = height / 2;
    let w = width as usize;

    let mut out_y = Vec::with_capacity((out_width * out_height) as usize);
    for row in 0..out_height as usize {
        for col in 0..out_width as usize {
            let i = row * 2 * w + col * 2;
            let sum = y[i] as u32 + y[i + 1] as u32 + y[i + w] as u32 + y[i + w + 1] as u32;
            out_y.push((sum / 4) as u8);
        }
    }

    // Chroma: one interleaved U,V pair per source 2x2 luma block; a 2x2 block
    // of pairs averages down to one pair.
    let chroma_width = w / 2; // pairs per source row
    let out_chroma_width = out_width as usize / 2;
    let out_chroma_height = out_height as usize / 2;
    let mut out_uv = Vec::with_capacity(out_chroma_width * out_chroma_height * 2);
    for row in 0..out_chroma_height {
        for col in 0..out_chroma_width {
            // Source pair columns 2c and 2c+1, two bytes each: byte offset 4c.
            let i = row * 2 * chroma_width * 2 + col * 4;
            let u = uv[i] as u32
                + uv[i + 2] as u32
                + uv[i + chroma_width * 2] as u32
                + uv[i + chroma_width * 2 + 2] as u32;
            let v = uv[i + 1] as u32
                + uv[i + 3] as u32
                + uv[i + chroma_width * 2 + 1] as u32
                + uv[i + chroma_width * 2 + 3] as u32;
            out_uv.push((u / 4) as u8);
            out_uv.push((v / 4) as u8);
        }
    }

    (out_y, out_uv, out_width, out_height)
}

/// Latest-wins single frame slot; `None` while the camera is off.
static LATEST: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Copy one captured frame into the self-view slot. Called from the camera
/// frame callback (capture thread); replaces any unconsumed previous frame.
///
/// Full capture resolution is downscaled to preview size first (2x2 box
/// filter, halving until the long edge fits [`PREVIEW_MAX_LONG_EDGE`]) so the
/// buffer, IPC transfer, and webview decode all work on a few hundred KB
/// instead of 1.4-3.1 MB per frame. MF delivers packed NV12 with stride ==
/// width (`frame_from_packed_nv12`), so the planes are exactly w*h and w*h/2
/// bytes at every halving.
pub(crate) fn feed_frame(frame: &crate::transport::camera::CameraFrame) {
    let (y, uv, width, height) = if frame.width.max(frame.height) > PREVIEW_MAX_LONG_EDGE {
        let (mut y, mut uv, mut width, mut height) =
            halve_nv12(&frame.y, &frame.uv, frame.width, frame.height);
        while width.max(height) > PREVIEW_MAX_LONG_EDGE {
            let (next_y, next_uv, next_width, next_height) = halve_nv12(&y, &uv, width, height);
            y = next_y;
            uv = next_uv;
            width = next_width;
            height = next_height;
        }
        (y, uv, width, height)
    } else {
        (frame.y.clone(), frame.uv.clone(), frame.width, frame.height)
    };

    let y_len = width as usize * height as usize;
    let uv_len = y_len / 2;
    let mut buffer = Vec::with_capacity(16 + y_len + uv_len);
    buffer.extend_from_slice(&width.to_le_bytes());
    buffer.extend_from_slice(&height.to_le_bytes());
    buffer.extend_from_slice(&frame.capture_wall_time_us.to_le_bytes());
    buffer.extend_from_slice(&y[..y_len]);
    buffer.extend_from_slice(&uv[..uv_len]);
    *LATEST.lock_unpoisoned() = Some(buffer);
}

/// Drop the stored frame (camera off / leave). The webview's next pull sees
/// an empty response and stops drawing.
pub(crate) fn clear() {
    *LATEST.lock_unpoisoned() = None;
}

/// Pull the latest frame as a raw IPC response — `invoke` resolves it to an
/// `ArrayBuffer` (no JSON bloat for ~1.4 MB/frame). An empty body means no
/// frame is available (webview checks `byteLength === 0`).
#[tauri::command]
pub fn next_self_view_frame() -> Result<tauri::ipc::Response, String> {
    let bytes = LATEST.lock_unpoisoned().take();
    Ok(tauri::ipc::Response::new(bytes.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::camera::CameraFrame;

    #[test]
    fn halve_nv12_averages_2x2_luma_blocks() {
        // 8x4 luma with distinct values per 2x2 block.
        let y = vec![
            0, 4, 8, 12, 100, 104, 108, 112, //
            16, 20, 24, 28, 116, 120, 124, 128, //
            32, 36, 40, 44, 132, 136, 140, 144, //
            48, 52, 56, 60, 148, 152, 156, 160,
        ];
        // 4x2 pairs of interleaved U,V.
        let uv = vec![
            0, 1, 2, 3, 4, 5, 6, 7, //
            8, 9, 10, 11, 12, 13, 14, 15,
        ];
        let (out_y, out_uv, width, height) = halve_nv12(&y, &uv, 8, 4);
        assert_eq!((width, height), (4, 2));
        assert_eq!(out_y, vec![10, 18, 110, 118, 42, 50, 142, 150]);
        assert_eq!(out_uv, vec![5, 6, 9, 10]);
        assert_eq!(out_uv.len(), (width * height / 2) as usize);
    }

    #[test]
    fn feed_frame_downscales_full_res_before_buffering() {
        let frame = CameraFrame {
            width: 1280,
            height: 720,
            y: vec![128; 1280 * 720],
            y_stride: 1280,
            uv: vec![64; 1280 * 720 / 2],
            uv_stride: 1280,
            capture_wall_time_us: 42,
        };
        feed_frame(&frame);
        let buffer = LATEST.lock_unpoisoned().take().expect("buffered frame");
        let width = u32::from_le_bytes(buffer[0..4].try_into().unwrap());
        let height = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
        let timestamp = u64::from_le_bytes(buffer[8..16].try_into().unwrap());
        assert_eq!((width, height), (640, 360));
        assert_eq!(timestamp, 42);
        assert_eq!(buffer.len(), 16 + (640 * 360 * 3 / 2) as usize);
    }
}
