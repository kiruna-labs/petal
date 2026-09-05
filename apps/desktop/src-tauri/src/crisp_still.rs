//! "Crisp mode" (issue #384) -- Phase 1 SPIKE only. Native-to-native still-
//! image path for static shared windows, so syntax-highlighted text can be
//! read at full color resolution instead of through the 4:2:0 chroma
//! subsampling every WebRTC video codec path in this app uses
//! (`transport/publisher.rs`'s NV12 pipeline).
//!
//! ## What THIS module implements (Phase 1 scope, per the issue)
//!
//! 1. A static-trigger gate ([`StillSendGate`]) that REUSES the existing
//!    dirty-rect skip-run counter from `session/share.rs`'s
//!    `DirtyRectPumpState::skip_run_length()` (#381) -- this module does not
//!    reimplement static detection, it only decides when that existing
//!    signal has been "static long enough" to justify one encode.
//! 2. A still-image encoder ([`encode_captured_frame_still`]) that reads the
//!    frame buffer `session/share.rs`'s pump loop is already holding
//!    (`crate::capture::CapturedFrame`) and produces a lossless WebP.
//! 3. A wire format ([`encode_packet`]/[`decode_packet`]) and publish/receive
//!    pair ([`publish_still`]/[`start_receiver_for_room`]) over a LiveKit
//!    data-channel topic (`petal.crisp-still`), following the exact
//!    `DataPacket`/`RoomEvent::DataReceived` pattern `telepointer.rs` already
//!    uses for a different topic -- no new transport infrastructure.
//! 4. The correctness-critical invalidation logic ([`StillValidity`]): a
//!    still is only ever considered current if NO video frame newer than the
//!    one it was derived from has been observed. This is a plain integer
//!    comparison performed unconditionally on every video frame, not a timer
//!    or heuristic, so a stale still cannot structurally survive motion
//!    resuming (see that type's doc comment for the exact argument).
//!
//! ## What is NOT implemented yet (explicitly out of scope for this pass)
//!
//! - **The actual receiver-side blit.** [`start_receiver_for_room`] decodes
//!   and stores each received still (keyed by `window_id`, versioned by
//!   [`StillValidity`]) in [`received_stills`], but nothing yet paints those
//!   bytes into a native layer. The issue asks for a CALayer sibling above
//!   `compositor.rs`'s `AVSampleBufferDisplayLayer`
//!   (see `compositor.rs`'s `attach_display_layer`); wiring that up --
//!   creating the layer, decoding the WebP to a `CGImage`/bitmap, and
//!   showing/hiding it in step with [`StillValidity::should_show_still`] --
//!   is real, non-trivial AppKit/CALayer work this pass did not attempt,
//!   per the issue's own explicit escape hatch ("if the byte-stream/
//!   receiver-blit plumbing turns out too large for one pass, implement and
//!   verify the SENDER side ... in isolation with unit tests, and clearly
//!   document that the receiver-side blit is an unfinished follow-up").
//! - **Feeding `StillValidity::observe_video_frame` from real decoded video
//!   frames.** The natural integration point already exists and needs no new
//!   plumbing: `transport/subscriber.rs`'s real compositor feed already
//!   extracts `capture_timestamp_us` from each decoded frame's LiveKit
//!   `FrameMetadata.user_timestamp` (see that file, the `let Some((capture_timestamp_us,
//!   frame_id)) = frame.frame_metadata...` block, used today only for
//!   glass-to-glass latency). A follow-up should call
//!   `crisp_still::observe_video_frame(window_id, capture_timestamp_us)`
//!   right there. Not wired up yet -- this module's video-frame-id axis
//!   currently only advances from unit tests.
//! - **web-harness / browser receivers, color management, kill switch,
//!   diagnostics** -- all explicitly Phase 2 per the issue.
//!
//! ## An important open question surfaced by this spike (read before Phase 2 go/no-go)
//!
//! `session/capture.rs`'s SCK stream (and its `SCScreenshotManager`
//! snapshot-pull fallback) are BOTH pinned to `'420v'` NV12
//! (`FMT_NV12_VIDEO_RANGE`, see `capture.rs`) -- there is currently no
//! production code path that captures a genuinely 4:4:4 BGRA buffer at all;
//! `CapturedFramePayload::Bgra` is, per that module's own doc comment, "a
//! parked fallback for tests and any future" use, not something the live
//! stream ever actually produces today. That means encoding the
//! *already-held* NV12 buffer losslessly (what this module's sender
//! integration does, in `session/share.rs`) reproduces the SAME
//! already-chroma-subsampled pixels the video track would show, one 1fps
//! keepalive frame later -- it avoids the ADDITIONAL lossy H.264 encode
//! pass, but it does NOT recover the true 4:4:4 detail the issue's "4:2:0
//! text ceiling" problem statement is about. Achieving that would need a
//! NEW capture-layer path (e.g. a BGRA-format one-shot `SCScreenshotManager`
//! capture triggered on the same static signal), which is a bigger lift than
//! "read the already-held buffer" and was deliberately left out of this
//! bounded spike. [`encode_captured_frame_still`] does support the `Bgra`
//! payload variant end-to-end (real, tested code, not a stub) for exactly
//! this reason -- so the day a BGRA capture path exists, this module needs
//! no changes to consume it -- but as of this commit nothing feeds it BGRA
//! data. Recommend Phase 2 planning explicitly account for this rather than
//! assume "reuse the already-held buffer" alone delivers the promised
//! crispness.
//!
//! ## Why WebP lossless, not PNG
//!
//! `image` 0.25 (already resolved in this workspace's dependency graph via
//! `arboard`/`tauri-plugin-clipboard-manager`, see `Cargo.toml`) ships a
//! pure-Rust lossless WebP encoder (`image::codecs::webp::WebPEncoder`,
//! backed by the `image-webp` crate) with no C/libwebp toolchain dependency
//! at all -- so PNG's "acceptable fallback" clause in the issue never had to
//! be exercised. This matters specifically in this codebase, which has
//! fought real `-ObjC`/duplicate-Swift-symbol linker battles before from new
//! native-code dependencies (`transport/mod.rs`'s M0 writeup); a C libwebp
//! binding would have been a materially riskier choice for a bounded spike
//! than a pure-Rust codec already reachable from an existing dependency.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::capture::{CapturedFrame, CapturedFramePayload};
use crate::session::RoomGeneration;
use crate::transport::publisher::RoomConnection;
use std::sync::Arc;

/// LiveKit data-channel topic for crisp-still packets -- mirrors
/// `telepointer.rs`'s `TOPIC` pattern so a receiver can filter
/// `RoomEvent::DataReceived` without a second connection/channel.
pub(crate) const TOPIC: &str = "petal.crisp-still";

/// Opt-IN env var, default OFF (Fable review, #384). Unlike this repo's
/// other PETAL_DISABLE_* kill switches, this one must default to disabled:
/// there is no receiver-side blit yet (Phase 2, not implemented), so a
/// still that fires today is pure wasted CPU (a synchronous WebP encode on
/// the pump thread) and a wasted reliable-channel publish for zero visible
/// benefit -- not a safe default for a build that ships to users.
const CRISP_STILL_SPIKE_ENV: &str = "PETAL_CRISP_STILL_SPIKE";

fn crisp_still_spike_enabled() -> bool {
    std::env::var(CRISP_STILL_SPIKE_ENV).as_deref() == Ok("1")
}

/// Consecutive dirty-rect-clean skipped frames
/// (`DirtyRectPumpState::skip_run_length()`, #381) before a shared window is
/// considered static long enough to justify paying one still-image encode.
/// Deliberately small for the spike -- real tuning needs a live two-machine
/// measurement of how quickly a real static run reaches this count (this
/// sandbox cannot drive ScreenCaptureKit), so this is a documented guess,
/// not a measured value.
const STATIC_TRIGGER_SKIP_FRAMES: u64 = 3;

/// After the first still of a static run is sent, resend every this-many
/// additional skipped frames as a resilience measure against a lost
/// reliable-channel delivery (belt-and-braces; `reliable: true` below should
/// make this redundant in practice). Matches the order of magnitude of the
/// existing `run_length % 300` log-throttle cadence already used for this
/// same skip-run counter a few lines away in `session/share.rs`.
const STATIC_TRIGGER_REPEAT_INTERVAL: u64 = 300;

// --- Static-trigger gate (reuses #381's signal, does not reimplement it) ---

/// Per-pump-loop gate deciding when to fire a still encode+publish, driven
/// entirely by the skip-run length `session/share.rs` already computes.
#[derive(Debug, Default)]
pub(crate) struct StillSendGate {
    last_triggered_run_length: u64,
}

impl StillSendGate {
    /// `skip_run_length` is `DirtyRectPumpState::skip_run_length()` --
    /// `session/share.rs` calls this once per `DirtyRectFrameDecision::Skip`.
    /// Fires exactly once when a run first reaches
    /// [`STATIC_TRIGGER_SKIP_FRAMES`], then again only every
    /// [`STATIC_TRIGGER_REPEAT_INTERVAL`] frames after that. A run resetting
    /// (a real push happened, so `skip_run_length` drops back towards 0)
    /// re-arms the gate for the next static period with no extra
    /// bookkeeping -- the reset is entirely inferred from the counter going
    /// backwards.
    pub(crate) fn should_trigger(&mut self, skip_run_length: u64) -> bool {
        if skip_run_length < self.last_triggered_run_length {
            // A new push happened since we last fired (the caller's counter
            // reset) -- forget the old high-water mark so the next threshold
            // crossing in this fresh run fires again.
            self.last_triggered_run_length = 0;
        }
        if skip_run_length < STATIC_TRIGGER_SKIP_FRAMES {
            return false;
        }
        if self.last_triggered_run_length > 0
            && skip_run_length
                < self
                    .last_triggered_run_length
                    .saturating_add(STATIC_TRIGGER_REPEAT_INTERVAL)
        {
            return false;
        }
        self.last_triggered_run_length = skip_run_length;
        true
    }
}

// --- Pixel conversion (real capture is NV12 today; BGRA kept for when a
// genuinely 4:4:4 capture path exists -- see module doc comment) ---

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CrispStillError {
    #[error("nv12 planes too small for a {0}x{1} frame")]
    Nv12PlaneTooSmall(u32, u32),
    #[error("bgra plane too small for a {0}x{1} frame")]
    BgraPlaneTooSmall(u32, u32),
    #[error(
        "crisp-still encode not implemented for the Native (CVPixelBuffer passthrough) capture payload -- see crisp_still.rs module doc comment"
    )]
    NativePayloadUnsupported,
    #[error("webp encode failed: {0}")]
    Encode(String),
    #[error("packet too short to contain a crisp-still header")]
    PacketTooShort,
}

/// BT.601 limited-range (video-range) NV12 -> RGB8, nearest-neighbor chroma
/// upsampling (each 2x2 luma block shares its one U/V sample -- 4:2:0 stays
/// 4:2:0 through this conversion; see the module doc comment's "open
/// question" section on why this alone does not deliver true 4:4:4).
/// Pure and unit-tested below with known reference triples.
pub(crate) fn nv12_to_rgb8(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CrispStillError> {
    let (w, h) = (width as usize, height as usize);
    if h == 0 || y_stride < w || y_plane.len() < y_stride * h {
        return Err(CrispStillError::Nv12PlaneTooSmall(width, height));
    }
    let uv_rows = h.div_ceil(2);
    let uv_cols = w.div_ceil(2);
    if uv_stride < uv_cols * 2 || uv_plane.len() < uv_stride * uv_rows {
        return Err(CrispStillError::Nv12PlaneTooSmall(width, height));
    }

    let mut out = vec![0u8; w * h * 3];
    for row in 0..h {
        let y_row = &y_plane[row * y_stride..row * y_stride + w];
        let uv_row = &uv_plane[(row / 2) * uv_stride..];
        for col in 0..w {
            let y = y_row[col] as i32;
            let u = uv_row[(col / 2) * 2] as i32;
            let v = uv_row[(col / 2) * 2 + 1] as i32;

            let c = y - 16;
            let d = u - 128;
            let e = v - 128;

            let r = (298 * c + 409 * e + 128) >> 8;
            let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
            let b = (298 * c + 516 * d + 128) >> 8;

            let out_idx = (row * w + col) * 3;
            out[out_idx] = r.clamp(0, 255) as u8;
            out[out_idx + 1] = g.clamp(0, 255) as u8;
            out[out_idx + 2] = b.clamp(0, 255) as u8;
        }
    }
    Ok(out)
}

/// BGRA (32 bits/pixel, B,G,R,A byte order -- `CapturedFramePayload::Bgra`'s
/// documented memory layout) -> RGB8, dropping alpha. Pure and unit-tested.
pub(crate) fn bgra_to_rgb8(
    data: &[u8],
    bytes_per_row: usize,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CrispStillError> {
    let (w, h) = (width as usize, height as usize);
    if h == 0 || bytes_per_row < w * 4 || data.len() < bytes_per_row * h {
        return Err(CrispStillError::BgraPlaneTooSmall(width, height));
    }
    let mut out = vec![0u8; w * h * 3];
    for row in 0..h {
        let src_row = &data[row * bytes_per_row..row * bytes_per_row + w * 4];
        for col in 0..w {
            let px = &src_row[col * 4..col * 4 + 4];
            let out_idx = (row * w + col) * 3;
            out[out_idx] = px[2]; // R
            out[out_idx + 1] = px[1]; // G
            out[out_idx + 2] = px[0]; // B
        }
    }
    Ok(out)
}

/// One successfully encoded still, plus the measurements the issue's
/// Definition of Done asks this spike to record.
pub(crate) struct EncodedStill {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Wall-clock capture time of the SOURCE frame this still was derived
    /// from (`CapturedFrame` doesn't carry this itself; callers pass the
    /// same `capture_wall_time_us` `session/share.rs`'s pump loop already
    /// has for that frame). This value doubles as the still's invalidation
    /// axis -- see [`StillValidity`]'s doc comment for why reusing the
    /// existing SPEC §7 capture-timestamp metadata (already threaded through
    /// the video pipeline end-to-end for glass-to-glass latency) needs no
    /// new plumbing through `compositor.rs`.
    pub capture_wall_time_us: u64,
    pub encode_duration: Duration,
}

fn rgb8_to_webp_lossless(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CrispStillError> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|e| CrispStillError::Encode(e.to_string()))?;
    Ok(out)
}

/// Encode one still from a captured frame's currently-held pixel buffer.
/// Reads whatever `CapturedFramePayload` variant the frame actually carries
/// -- in production today that's always `Nv12` (`capture.rs` pins the SCK
/// stream to `'420v'`); `Bgra` is supported too (real conversion, not a
/// stub) for when a genuinely 4:4:4 capture path exists. `Native`
/// (CVPixelBuffer passthrough) is not implemented -- see
/// [`CrispStillError::NativePayloadUnsupported`].
pub(crate) fn encode_captured_frame_still(
    frame: &CapturedFrame,
    capture_wall_time_us: u64,
) -> Result<EncodedStill, CrispStillError> {
    let started = Instant::now();
    let rgb = match &frame.payload {
        CapturedFramePayload::Nv12 {
            y,
            y_stride,
            uv,
            uv_stride,
        } => nv12_to_rgb8(
            y,
            *y_stride as usize,
            uv,
            *uv_stride as usize,
            frame.width,
            frame.height,
        )?,
        CapturedFramePayload::Bgra {
            data,
            bytes_per_row,
        } => bgra_to_rgb8(data, *bytes_per_row, frame.width, frame.height)?,
        CapturedFramePayload::Native { .. } => {
            return Err(CrispStillError::NativePayloadUnsupported);
        }
    };
    let bytes = rgb8_to_webp_lossless(&rgb, frame.width, frame.height)?;
    Ok(EncodedStill {
        bytes,
        width: frame.width,
        height: frame.height,
        capture_wall_time_us,
        encode_duration: started.elapsed(),
    })
}

// --- Wire format: small fixed header + WebP bytes, no JSON/base64 overhead
// (unlike telepointer.rs's JSON messages -- worth it here since a still's
// whole point is a small wire size, and base64 alone would cost +33%). ---

const HEADER_LEN: usize = 4 + 8 + 4 + 4 + 1;
const FORMAT_WEBP: u8 = 1;

pub(crate) fn encode_packet(
    window_id: u32,
    capture_wall_time_us: u64,
    width: u32,
    height: u32,
    image_bytes: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + image_bytes.len());
    buf.extend_from_slice(&window_id.to_le_bytes());
    buf.extend_from_slice(&capture_wall_time_us.to_le_bytes());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.push(FORMAT_WEBP);
    buf.extend_from_slice(image_bytes);
    buf
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedStillPacket {
    pub window_id: u32,
    pub capture_wall_time_us: u64,
    pub width: u32,
    pub height: u32,
    pub format: u8,
    pub image_bytes: Vec<u8>,
}

pub(crate) fn decode_packet(bytes: &[u8]) -> Result<DecodedStillPacket, CrispStillError> {
    if bytes.len() < HEADER_LEN {
        return Err(CrispStillError::PacketTooShort);
    }
    let window_id = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let capture_wall_time_us = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
    let width = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let height = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let format = bytes[20];
    Ok(DecodedStillPacket {
        window_id,
        capture_wall_time_us,
        width,
        height,
        format,
        image_bytes: bytes[HEADER_LEN..].to_vec(),
    })
}

// --- Invalidation: the correctness-critical piece ---

/// Per-window invalidation state. The contract: a still is current IFF no
/// video frame newer than the one it was derived from has been observed.
/// `observe_video_frame` is meant to be called unconditionally on EVERY
/// decoded video frame (not on a timer, not sampled) -- that is what makes
/// "stale still after motion resumes" structurally impossible rather than
/// merely unlikely: the very next real video frame's timestamp, whatever it
/// is, immediately makes `should_show_still` false for any still at or
/// before the previous static period, with no window where both could be
/// considered "current" at once. Both sides compare the SAME axis: LiveKit's
/// existing `FrameMetadata.user_timestamp` (SPEC §7's glass-to-glass
/// capture-timestamp metadata, already carried end-to-end through the video
/// pipeline for a different purpose -- see the module doc comment) doubles
/// as this monotonic id, so no new plumbing through the video pipeline is
/// needed to obtain it.
///
/// Not constructed by any non-test code yet -- `observe_video_frame` has no
/// live call site until a follow-up wires it in from
/// `transport/subscriber.rs`'s real decoded-frame loop (see that file's
/// doc-comment pointer next to its `capture_timestamp_us` extraction). Kept
/// un-gutted (not `#[cfg(test)]`) so that follow-up is a pure addition, not a
/// re-add of deleted code; `#[allow(dead_code)]` documents that this is
/// deliberate, not an oversight.
#[allow(dead_code)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StillValidity {
    latest_video_capture_us: u64,
}

#[allow(dead_code)]
impl StillValidity {
    pub(crate) fn observe_video_frame(&mut self, capture_wall_time_us: u64) {
        self.latest_video_capture_us = self.latest_video_capture_us.max(capture_wall_time_us);
    }

    /// Whether a still tagged with `still_capture_wall_time_us` should
    /// currently be shown. `<=`, not `<`: a still IS allowed to be shown
    /// having been derived from exactly the last video frame observed (that
    /// is the normal, expected case immediately after the static trigger
    /// fires) -- it stops being valid only once a STRICTLY newer frame
    /// arrives.
    pub(crate) fn should_show_still(&self, still_capture_wall_time_us: u64) -> bool {
        self.latest_video_capture_us <= still_capture_wall_time_us
    }
}

// --- Sender side: publish over the data channel (mirrors telepointer.rs's
// `publish_pointer`) ---

#[cfg(target_os = "macos")]
pub(crate) fn publish_still(
    room_connection: &Arc<RoomConnection>,
    window_id: u32,
    capture_wall_time_us: u64,
    width: u32,
    height: u32,
    image_bytes: Vec<u8>,
) {
    let room = room_connection.room();
    let payload = encode_packet(window_id, capture_wall_time_us, width, height, &image_bytes);
    tauri::async_runtime::spawn(async move {
        let packet = livekit::DataPacket {
            payload,
            topic: Some(TOPIC.to_string()),
            // Unlike telepointer's continuous 45Hz stream, a crisp still is a
            // one-shot, must-not-drop message -- there is no "next sample
            // 22ms later" to supersede a lost one. Reliable delivery's
            // retransmit/ordering overhead is a non-issue at this rate (one
            // per static period, not per frame).
            reliable: true,
            destination_identities: Vec::new(),
        };
        if let Err(e) = room.local_participant().publish_data(packet).await {
            log::warn!("crisp_still: publish_data failed for window {window_id}: {e}");
        }
    });
}

/// Called from `session/share.rs`'s pump loop on every
/// `DirtyRectFrameDecision::Skip` -- encodes and publishes at most one still
/// per static run (plus the [`STATIC_TRIGGER_REPEAT_INTERVAL`] resends), and
/// otherwise does nothing (no allocation, no encode) on the overwhelming
/// majority of skip ticks. Logs + returns the measured encode duration and
/// wire size on the ticks it actually fires, so a caller/log reader can see
/// the Definition of Done numbers directly from a live run.
#[cfg(target_os = "macos")]
pub(crate) fn maybe_trigger_still(
    gate: &mut StillSendGate,
    window_id: u32,
    skip_run_length: u64,
    frame: &CapturedFrame,
    capture_wall_time_us: u64,
    room_connection: &Arc<RoomConnection>,
) {
    if !crisp_still_spike_enabled() {
        return;
    }
    if !gate.should_trigger(skip_run_length) {
        return;
    }
    match encode_captured_frame_still(frame, capture_wall_time_us) {
        Ok(still) => {
            let wire_len = HEADER_LEN + still.bytes.len();
            log::info!(
                "crisp_still: window {window_id} encoded {}x{} still in {:.2}ms, {} bytes on the wire (skip_run_length={skip_run_length})",
                still.width,
                still.height,
                still.encode_duration.as_secs_f64() * 1000.0,
                wire_len
            );
            publish_still(
                room_connection,
                window_id,
                still.capture_wall_time_us,
                still.width,
                still.height,
                still.bytes,
            );
        }
        Err(e) => {
            // Not fatal -- the video track's own ~1fps keepalive already
            // covers this window; a still is a pure quality add-on.
            log::debug!(
                "crisp_still: window {window_id} skipped still encode (skip_run_length={skip_run_length}): {e}"
            );
        }
    }
}

// --- Receiver side: decode + versioned storage (mirrors
// telepointer.rs's `start_receiver_for_room`). NOT wired to any native blit
// -- see module doc comment. ---

pub(crate) struct ReceivedStill {
    pub capture_wall_time_us: u64,
    pub width: u32,
    pub height: u32,
    pub format: u8,
    pub image_bytes: Vec<u8>,
    // Not read yet -- reserved for the future blit implementation (e.g. an
    // "evict a still nobody ever consumed after N seconds" cleanup).
    #[allow(dead_code)]
    pub received_at: Instant,
}

// Fable review, #384, Phase 2 follow-up (not fixed here -- nothing consumes
// this map yet, so it's inert): keyed by bare window_id, which is a
// CGWindowID scoped to its OWNING PROCESS, not globally unique across
// senders in a room. Two different participants sharing a window that
// happens to share a CGWindowID could clobber each other's stored still.
// Before Phase 2 wires an actual receiver blit, key this the same way
// other per-window receiver state in this app is keyed: by
// (owner_identity, window_id), not window_id alone. Also unbounded --
// never cleared on room leave/window-close; needs a cleanup hook.
fn received_stills() -> &'static Mutex<HashMap<u32, ReceivedStill>> {
    static STILLS: OnceLock<Mutex<HashMap<u32, ReceivedStill>>> = OnceLock::new();
    STILLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns the currently stored still for `window_id`, if any -- the
/// attachment point a future compositor blit implementation would read from.
/// Exposed now so that follow-up work is additive (new caller in
/// `compositor.rs`) rather than needing to change this module's shape.
#[allow(dead_code)]
pub(crate) fn latest_still_for_window(window_id: u32) -> Option<(u64, u32, u32, u8, Vec<u8>)> {
    let stills = received_stills().lock().ok()?;
    let still = stills.get(&window_id)?;
    Some((
        still.capture_wall_time_us,
        still.width,
        still.height,
        still.format,
        still.image_bytes.clone(),
    ))
}

/// Start the receiver-side task for one room connection: subscribes to
/// `petal.crisp-still` data packets and stores the newest still per window
/// (newest by `capture_wall_time_us`, guarding against an out-of-order
/// arrival on the reliable channel making a stale still clobber a fresher
/// one). Same one-room-connection seam as `telepointer::start_receiver_for_room`
/// (see `session/room.rs`'s call site).
#[cfg(target_os = "macos")]
pub(crate) fn start_receiver_for_room(
    app: &tauri::AppHandle,
    room: Arc<livekit::Room>,
    generation: RoomGeneration,
) {
    let _ = app; // Not yet used -- reserved for the future blit hookup.
    let mut events = room.subscribe();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("crisp_still: receiver exiting for stale room generation");
                break;
            }
            if let livekit::RoomEvent::DataReceived { payload, topic, .. } = event {
                if topic.as_deref() != Some(TOPIC) {
                    continue;
                }
                let packet = match decode_packet(&payload) {
                    Ok(packet) => packet,
                    Err(e) => {
                        log::warn!("crisp_still: dropped malformed still packet: {e}");
                        continue;
                    }
                };
                let mut stills = received_stills().lock_unpoisoned_or_recover();
                let replace = stills.get(&packet.window_id).is_none_or(|existing| {
                    packet.capture_wall_time_us > existing.capture_wall_time_us
                });
                if replace {
                    log::info!(
                        "crisp_still: window {} received still {}x{} ({} bytes) -- receiver-side blit not yet implemented, see crisp_still.rs module doc comment",
                        packet.window_id,
                        packet.width,
                        packet.height,
                        packet.image_bytes.len()
                    );
                    stills.insert(
                        packet.window_id,
                        ReceivedStill {
                            capture_wall_time_us: packet.capture_wall_time_us,
                            width: packet.width,
                            height: packet.height,
                            format: packet.format,
                            image_bytes: packet.image_bytes,
                            received_at: Instant::now(),
                        },
                    );
                }
            }
        }
    });
}

// No `#[cfg(not(target_os = "macos"))]` stub needed: this whole module is
// only compiled on macOS (see its `mod crisp_still;` declaration in lib.rs),
// same as `capture.rs`, which this module's types depend on directly.

/// Small extension so this module's one lock-poisoning recovery site doesn't
/// need to match `sync_ext::MutexExt`'s exact signature (that trait is
/// tailored to the `Arc<Mutex<T>>` call sites elsewhere in this crate); a
/// poisoned crisp-still cache is not worth crashing the receiver task over.
trait RecoverLock<T> {
    fn lock_unpoisoned_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> RecoverLock<T> for Mutex<T> {
    fn lock_unpoisoned_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- StillSendGate ---

    #[test]
    fn gate_does_not_fire_before_threshold() {
        let mut gate = StillSendGate::default();
        assert!(!gate.should_trigger(1));
        assert!(!gate.should_trigger(2));
    }

    #[test]
    fn gate_fires_exactly_once_at_threshold() {
        let mut gate = StillSendGate::default();
        assert!(!gate.should_trigger(1));
        assert!(!gate.should_trigger(2));
        assert!(gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES));
        // Same run continuing past threshold: no immediate resend.
        assert!(!gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES + 1));
        assert!(!gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES + 10));
    }

    #[test]
    fn gate_resends_after_repeat_interval_in_a_long_run() {
        let mut gate = StillSendGate::default();
        assert!(gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES));
        let next = STATIC_TRIGGER_SKIP_FRAMES + STATIC_TRIGGER_REPEAT_INTERVAL;
        assert!(!gate.should_trigger(next - 1));
        assert!(gate.should_trigger(next));
    }

    #[test]
    fn gate_rearms_after_run_resets() {
        let mut gate = StillSendGate::default();
        assert!(gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES));
        // A real push happened: session/share.rs's skip_run_length drops
        // back down (a fresh run starting from 1 again).
        assert!(!gate.should_trigger(1));
        assert!(!gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES - 1));
        assert!(gate.should_trigger(STATIC_TRIGGER_SKIP_FRAMES));
    }

    // --- pixel conversion ---

    #[test]
    fn nv12_black_frame_converts_to_rgb_black() {
        // Y=16, U=V=128 is video-range black.
        let y = vec![16u8; 4 * 2];
        let uv = vec![128u8; 2 * 1 * 2]; // 2x1 UV pairs for a 4x2 frame
        let rgb = nv12_to_rgb8(&y, 4, &uv, 4, 4, 2).unwrap();
        assert_eq!(rgb, vec![0u8; 4 * 2 * 3]);
    }

    #[test]
    fn nv12_white_frame_converts_to_rgb_white() {
        // Y=235, U=V=128 is video-range white.
        let y = vec![235u8; 4 * 2];
        let uv = vec![128u8; 4 * 2];
        let rgb = nv12_to_rgb8(&y, 4, &uv, 4, 4, 2).unwrap();
        assert_eq!(rgb, vec![255u8; 4 * 2 * 3]);
    }

    #[test]
    fn nv12_rejects_undersized_planes() {
        let y = vec![0u8; 2];
        let uv = vec![0u8; 2];
        assert_eq!(
            nv12_to_rgb8(&y, 4, &uv, 4, 4, 2),
            Err(CrispStillError::Nv12PlaneTooSmall(4, 2))
        );
    }

    #[test]
    fn bgra_round_trips_channel_order() {
        // One pixel: B=10, G=20, R=30, A=255.
        let data = [10u8, 20, 30, 255];
        let rgb = bgra_to_rgb8(&data, 4, 1, 1).unwrap();
        assert_eq!(rgb, vec![30, 20, 10]);
    }

    #[test]
    fn bgra_rejects_undersized_buffer() {
        let data = [0u8; 2];
        assert_eq!(
            bgra_to_rgb8(&data, 4, 1, 1),
            Err(CrispStillError::BgraPlaneTooSmall(1, 1))
        );
    }

    // --- wire format ---

    #[test]
    fn packet_round_trips() {
        let image_bytes = vec![1u8, 2, 3, 4, 5];
        let packet = encode_packet(42, 123_456_789, 1920, 1080, &image_bytes);
        let decoded = decode_packet(&packet).unwrap();
        assert_eq!(decoded.window_id, 42);
        assert_eq!(decoded.capture_wall_time_us, 123_456_789);
        assert_eq!(decoded.width, 1920);
        assert_eq!(decoded.height, 1080);
        assert_eq!(decoded.format, FORMAT_WEBP);
        assert_eq!(decoded.image_bytes, image_bytes);
    }

    #[test]
    fn packet_too_short_is_rejected() {
        assert_eq!(
            decode_packet(&[0u8; 5]),
            Err(CrispStillError::PacketTooShort)
        );
    }

    // --- end-to-end still encode ---

    #[test]
    fn encode_nv12_frame_produces_a_valid_still_tagged_with_its_capture_time() {
        let width = 64u32;
        let height = 32u32;
        let y = vec![120u8; (width * height) as usize];
        let uv = vec![128u8; (width * (height / 2)) as usize];
        let frame = CapturedFrame {
            width,
            height,
            payload: CapturedFramePayload::Nv12 {
                y: crate::capture::PooledFrameData::from_vec(y),
                y_stride: width,
                uv: crate::capture::PooledFrameData::from_vec(uv),
                uv_stride: width,
            },
            source_scale: 1.0,
            layout_validated: true,
            color_profile: crate::video_color::VideoColorProfile::BT601_VIDEO,
            sequence: 1,
            frame_status: None,
            dirty_rect_count: 0,
            dirty_area_px: 0,
            dirty_rects_known: true,
            lock_copy_ms: 0.0,
            region_generation: None,
        };
        let still = encode_captured_frame_still(&frame, 777).unwrap();
        assert_eq!(still.width, width);
        assert_eq!(still.height, height);
        assert_eq!(still.capture_wall_time_us, 777);
        assert!(!still.bytes.is_empty());
        // Must actually decode back to the right dimensions (real
        // correctness check, not just "encoder didn't error").
        let decoded = image::load_from_memory(&still.bytes).expect("valid webp bytes");
        assert_eq!(decoded.width(), width);
        assert_eq!(decoded.height(), height);
    }

    /// Manual benchmark for the Definition of Done's "encode time at 4K,
    /// wire size" numbers -- `#[ignore]`d (not part of the default `cargo
    /// test --lib` run) since it's a measurement, not a correctness check.
    /// Run with:
    /// `cargo test --lib crisp_still::tests::benchmark -- --ignored --nocapture`
    ///
    /// IMPORTANT caveat (data-accuracy honesty): this sandbox cannot drive a
    /// real ScreenCaptureKit capture, so the input is a SYNTHETIC frame
    /// approximating a code editor's structure (a fine vertical-stripe
    /// pattern, closer to text edges than either a solid color or random
    /// noise -- WebP's entropy coding is very sensitive to which of these
    /// three a source resembles). The numbers below are real measurements OF
    /// THIS CODE on this synthetic input, not measurements of real captured
    /// text -- live two-machine validation with an actual shared code editor
    /// window is still required before treating these as representative.
    #[test]
    #[ignore = "manual benchmark, not a correctness check -- see doc comment"]
    fn benchmark_4k_representative_frame_encode() {
        let width = 3840u32;
        let height = 2160u32;
        // Fine vertical stripes (alternating near-black/near-white every 2px)
        // approximate monospace text's high horizontal frequency content
        // better than a flat color, which would make WebP's compression
        // ratio look unrealistically good.
        let mut y = vec![0u8; (width * height) as usize];
        for row in 0..height as usize {
            for col in 0..width as usize {
                y[row * width as usize + col] = if (col / 2) % 2 == 0 { 235 } else { 16 };
            }
        }
        let uv = vec![128u8; (width * (height / 2)) as usize];
        let frame = CapturedFrame {
            width,
            height,
            payload: CapturedFramePayload::Nv12 {
                y: crate::capture::PooledFrameData::from_vec(y),
                y_stride: width,
                uv: crate::capture::PooledFrameData::from_vec(uv),
                uv_stride: width,
            },
            source_scale: 1.0,
            layout_validated: true,
            color_profile: crate::video_color::VideoColorProfile::BT601_VIDEO,
            sequence: 1,
            frame_status: None,
            dirty_rect_count: 0,
            dirty_area_px: 0,
            dirty_rects_known: true,
            lock_copy_ms: 0.0,
            region_generation: None,
        };
        let still = encode_captured_frame_still(&frame, 0).unwrap();
        println!(
            "crisp_still benchmark: {}x{} synthetic vertical-stripe frame -> {:.2}ms encode, {} bytes wire size ({} header + {} image)",
            width,
            height,
            still.encode_duration.as_secs_f64() * 1000.0,
            HEADER_LEN + still.bytes.len(),
            HEADER_LEN,
            still.bytes.len()
        );
    }

    #[test]
    fn native_payload_is_explicitly_unsupported_not_silently_wrong() {
        // Can't construct a real NativeCapturedPixelBuffer without a live
        // CVPixelBuffer; this documents the encode-side contract via the
        // NV12/BGRA paths above plus this crate-visible error variant. The
        // exhaustive match in `encode_captured_frame_still` is the actual
        // compile-time guarantee that a `Native` frame never falls through
        // to (incorrectly) treating garbage bytes as pixels.
        assert_eq!(
            CrispStillError::NativePayloadUnsupported.to_string(),
            "crisp-still encode not implemented for the Native (CVPixelBuffer passthrough) capture payload -- see crisp_still.rs module doc comment"
        );
    }

    // --- StillValidity: the correctness-critical invalidation logic ---

    #[test]
    fn fresh_tracker_shows_any_still_from_before_the_first_video_frame() {
        let validity = StillValidity::default();
        assert!(validity.should_show_still(100));
    }

    #[test]
    fn still_derived_from_the_last_seen_video_frame_is_still_valid() {
        let mut validity = StillValidity::default();
        validity.observe_video_frame(100);
        assert!(validity.should_show_still(100));
    }

    #[test]
    fn a_newer_video_frame_immediately_invalidates_an_older_still() {
        let mut validity = StillValidity::default();
        validity.observe_video_frame(100);
        assert!(validity.should_show_still(100));
        validity.observe_video_frame(150);
        assert!(!validity.should_show_still(100));
    }

    #[test]
    fn a_still_from_after_the_latest_video_frame_is_valid() {
        // The still-derived-from-frame case: the still's own source frame
        // hasn't been "seen again" as a video frame (it never will be --
        // it's the frame the static run started from), so this models a
        // still whose capture time is ahead of the last plain video frame.
        let mut validity = StillValidity::default();
        validity.observe_video_frame(90);
        assert!(validity.should_show_still(100));
    }

    #[test]
    fn out_of_order_or_duplicate_video_frames_cannot_resurrect_a_stale_still() {
        let mut validity = StillValidity::default();
        validity.observe_video_frame(200);
        assert!(!validity.should_show_still(100));
        // A late-arriving, older-timestamped frame (network reorder) must
        // not move the high-water mark backwards and "un-invalidate" the
        // still.
        validity.observe_video_frame(120);
        assert!(!validity.should_show_still(100));
        // A duplicate of the same frame is likewise a no-op.
        validity.observe_video_frame(200);
        assert!(!validity.should_show_still(100));
    }

    #[test]
    fn every_still_strictly_before_the_watermark_is_invalidated_not_just_the_matching_one() {
        // Structural check: invalidation is a plain `<` comparison against a
        // single watermark, so ANY still tagged with a timestamp strictly
        // before a newly observed video frame is invalid -- not just a
        // still that happens to share its exact frame id. This is what
        // makes "some stale still slips through" structurally impossible
        // rather than a matter of matching identifiers correctly. The still
        // tagged with EXACTLY the watermark's own timestamp remains valid
        // (see `should_show_still`'s doc comment: a still derived from the
        // very last video frame observed is the expected, normal case right
        // after the static trigger fires).
        let mut validity = StillValidity::default();
        validity.observe_video_frame(500);
        for still_ts in [0u64, 1, 100, 499] {
            assert!(
                !validity.should_show_still(still_ts),
                "still at {still_ts} should be invalid once video frame 500 was observed"
            );
        }
        assert!(validity.should_show_still(500));
        assert!(validity.should_show_still(501));
    }
}
