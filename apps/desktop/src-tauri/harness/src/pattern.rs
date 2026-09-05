//! Synthetic test-pattern generator (SPEC.md §7 point 2: "Instrumented
//! source pattern").
//!
//! Per this task's brief, we do NOT burn a visible pixel-encoded counter
//! into the frame -- M0 already proved out LiveKit's own
//! `FrameMetadataFeatures` (`user_timestamp` + `frame_id`, see
//! `desktop_lib::transport::publisher`'s module doc comment) as the
//! measurement-metadata carrier, and reusing that is explicitly what this
//! task asks for instead of inventing a second, redundant mechanism. So this
//! module's job is narrower than SPEC.md §7's literal wording: it only needs
//! to produce a **known, high-contrast, deterministic reference frame** --
//! stable pixel content a subscriber can compare against for spatial-quality
//! scoring (SSIM/PSNR, see `metrics.rs`'s note on why that part is a
//! follow-up) -- while the actual timestamp/counter rides in the existing
//! LiveKit metadata trailer, stamped by the caller when it calls
//! `PublishedTrack::push_frame` (see `bot.rs`).
//!
//! ## Output format
//!
//! Produces tightly-packed BGRA bytes (`bytes_per_row == width * 4`, no row
//! padding). Real ScreenCaptureKit shares now arrive as `420v` NV12, while
//! harness frames intentionally keep exercising the parked BGRA fallback used
//! for synthetic sources.

/// A deterministic high-contrast test pattern: a fixed color background plus
/// a "content" block whose position encodes `frame_index` (a slow-moving bar
/// that sweeps left-to-right, wrapping over ~2 seconds at 30fps) so a
/// snapshot of the frame content is visually distinguishable from other
/// frame indices without needing OCR -- useful for a human sanity-check
/// (screenshot of the receiver's decoded window) even though the *real*
/// measurement path uses the embedded LiveKit metadata, not pixel decoding.
///
/// Bot identity is baked into the background color (a stable hash of the
/// bot id -> hue) purely so that, in a multi-bot room, a human glancing at
/// several receiver windows can visually distinguish which bot's stream is
/// which without cross-referencing track names.
#[derive(Debug, Clone, Copy)]
pub struct TestPattern {
    pub width: u32,
    pub height: u32,
    bg: [u8; 3],
}

impl TestPattern {
    pub fn new(width: u32, height: u32, bot_id: &str) -> Self {
        Self {
            width,
            height,
            bg: color_for_bot(bot_id),
        }
    }

    /// Render one BGRA frame for `frame_index`. Tightly packed (no stride
    /// padding): `bytes_per_row = width * 4`.
    pub fn render(&self, frame_index: u64) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut buf = vec![0u8; w * h * 4];

        let [r, g, b] = self.bg;
        for px in buf.chunks_exact_mut(4) {
            // BGRA byte order (matches CapturedFrame's documented format).
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 0xFF;
        }

        // High-contrast sweeping bar: white, full-height, ~5% of width wide,
        // position = frame_index mod (a 2s-at-30fps period), so its x
        // position alone is a coarse visual proxy for "how far into the loop
        // are we" -- not the precision timing source (that's the embedded
        // LiveKit metadata), just a human-visible liveness indicator.
        let period = 60u64.max(1);
        let bar_w = (w / 20).max(2);
        let x0 = ((frame_index % period) as usize * w) / period as usize;
        for y in 0..h {
            for dx in 0..bar_w {
                let x = (x0 + dx).min(w - 1);
                let i = (y * w + x) * 4;
                buf[i] = 0xFF;
                buf[i + 1] = 0xFF;
                buf[i + 2] = 0xFF;
                buf[i + 3] = 0xFF;
            }
        }

        // Corner markers (small solid squares in all 4 corners) -- a fixed
        // reference a spatial-quality check (SSIM/PSNR, future work per
        // `metrics.rs`) could align against even if the stream is cropped or
        // scaled.
        let marker = (w.min(h) / 16).max(2);
        paint_square(&mut buf, w, 0, 0, marker, [0, 0, 0]);
        paint_square(&mut buf, w, w - marker, 0, marker, [0, 0, 0]);
        paint_square(&mut buf, w, 0, h - marker, marker, [0, 0, 0]);
        paint_square(&mut buf, w, w - marker, h - marker, marker, [0, 0, 0]);

        buf
    }
}

fn paint_square(buf: &mut [u8], stride_px: usize, x0: usize, y0: usize, size: usize, bgr: [u8; 3]) {
    let h = buf.len() / 4 / stride_px;
    for y in y0..(y0 + size).min(h) {
        for x in x0..(x0 + size).min(stride_px) {
            let i = (y * stride_px + x) * 4;
            buf[i] = bgr[0];
            buf[i + 1] = bgr[1];
            buf[i + 2] = bgr[2];
            buf[i + 3] = 0xFF;
        }
    }
}

/// Stable, cheap string->color hash so each bot's reference pattern has a
/// visually distinct (but deterministic across runs) background tint.
fn color_for_bot(bot_id: &str) -> [u8; 3] {
    let mut hash: u32 = 2166136261; // FNV-1a offset basis
    for b in bot_id.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    // Map to a mid-brightness, high-saturation-ish palette so the corner
    // markers/sweep bar stay high-contrast against it.
    let r = 40 + (hash & 0xFF) % 160;
    let g = 40 + ((hash >> 8) & 0xFF) % 160;
    let b = 40 + ((hash >> 16) & 0xFF) % 160;
    [r as u8, g as u8, b as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_expected_buffer_size_tightly_packed() {
        let pattern = TestPattern::new(64, 48, "bot-1");
        let frame = pattern.render(0);
        assert_eq!(frame.len(), 64 * 48 * 4);
    }

    #[test]
    fn different_frame_indices_produce_different_pixels() {
        let pattern = TestPattern::new(64, 48, "bot-1");
        let f0 = pattern.render(0);
        let f30 = pattern.render(30);
        assert_ne!(f0, f30, "sweeping bar should move between frame 0 and frame 30");
    }

    #[test]
    fn same_frame_index_is_deterministic() {
        let pattern = TestPattern::new(64, 48, "bot-1");
        assert_eq!(pattern.render(17), pattern.render(17));
    }

    #[test]
    fn different_bot_ids_get_different_background_colors() {
        let a = TestPattern::new(16, 16, "bot-a");
        let b = TestPattern::new(16, 16, "bot-b");
        // Sample a background pixel far from markers/bar (frame index that
        // puts the bar elsewhere, and a pixel away from all four corners).
        let fa = a.render(999);
        let fb = b.render(999);
        let mid = (8 * 16 + 8) * 4; // roughly center pixel, BGRA
        assert_ne!(
            &fa[mid..mid + 3],
            &fb[mid..mid + 3],
            "distinct bot ids should hash to distinct background colors"
        );
    }

    #[test]
    fn corner_markers_are_present() {
        let pattern = TestPattern::new(32, 32, "bot-x");
        let frame = pattern.render(0);
        // Top-left marker pixel should be black (0,0,0) in BGR.
        assert_eq!(&frame[0..3], &[0, 0, 0]);
    }
}
