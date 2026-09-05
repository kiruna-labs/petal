//! Per-window capture (SPEC.md §4.1, M0 spike).
//!
//! Wraps a single `SCStream` targeting one window by `CGWindowID`, delivering
//! real captured frames as owned `420v` NV12 `CVPixelBuffer`s to a
//! caller-supplied callback.
//!
//! ## Scope (M0 spike)
//!
//! This is intentionally narrow: one window, one stream, pinned `420v` NV12
//! pixel format (video-range Y + interleaved UV). M1 will need to
//! generalize this to N concurrent streams (one per shared window) behind
//! the window-tab picker, but the per-stream plumbing here is already
//! independent-lifecycle per SPEC.md §4.1 ("One `SCStream` per shared
//! window... Independent lifecycle"), so that generalization is additive.
//!
//! ## Zero-copy note
//!
//! `SCStream` delivers `CMSampleBuffer`s that are `IOSurface`-backed
//! end-to-end (per SPEC.md §4.1's "gives you `CMSampleBuffer`s with
//! `IOSurface` backing"). The live window path keeps the sample's owned
//! `CVPixelBuffer` reference and lets the LiveKit/libwebrtc native buffer path
//! hand that IOSurface straight to VideoToolbox. The `420v` NV12 copy path is
//! still present and used automatically by the publisher if native delivery is
//! disabled or fails.

use std::ffi::c_void;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use screencapturekit::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
use screencapturekit::prelude::*;
use screencapturekit::shareable_content::SCDisplay;
use screencapturekit::stream::configuration::PixelFormat;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::delegate_trait::ErrorHandler;

use crate::sync_ext::MutexExt;
use crate::transport::publisher::CaptureResolution;
use crate::video_color::{
    ColorPrimaries, MatrixCoefficients, PixelRange, TransferFunction, VideoColorProfile,
};

const CAPTURE_BUFFER_POOL_LIMIT: usize = 3;
const FMT_NV12_VIDEO_RANGE: u32 = 0x3432_3076; // '420v'
/// The only layout-integrity detail that crosses the capture/session boundary.
pub(crate) const CAPTURE_LAYOUT_INVALID: &str = "capture-layout-invalid";
pub(crate) const CAPTURE_LAYOUT_RECONFIGURE_PREFIX: &str = "capture-layout-reconfigure:";
/// ScreenCaptureKit rounds the output-space content ROI. Two physical pixels
/// covers that rounding without confusing ordinary letterbox/pillarbox ROI
/// metadata with corrupt geometry.
const CONTENT_RECT_EDGE_TOLERANCE_PX: f64 = 2.0;
/// Do not emit a timeout warning on every 50ms geometry check. A retry itself
/// is already bounded by `REGION_PROOF_TIMEOUT`; this wider interval keeps a
/// wedged native stream diagnosable without turning a resize into a log storm.
const REGION_PROOF_WARNING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) type CaptureBufferPool = Arc<Mutex<Vec<Vec<u8>>>>;

/// Slice-like owned frame bytes that return their allocation to the capture
/// stream's pool when dropped. This keeps the `CapturedFrame` contract
/// ("bytes stay valid after the SCK callback returns") without allocating a
/// fresh `Vec` for every frame.
pub struct PooledFrameData {
    bytes: Vec<u8>,
    pool: Option<CaptureBufferPool>,
}

impl PooledFrameData {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes, pool: None }
    }

    fn from_pool(bytes: Vec<u8>, pool: CaptureBufferPool) -> Self {
        Self {
            bytes,
            pool: Some(pool),
        }
    }
}

impl Deref for PooledFrameData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl Drop for PooledFrameData {
    fn drop(&mut self) {
        let Some(pool) = self.pool.as_ref() else {
            return;
        };
        let mut bytes = std::mem::take(&mut self.bytes);
        bytes.clear();

        let Ok(mut pool) = pool.lock() else {
            return;
        };
        if pool.len() < CAPTURE_BUFFER_POOL_LIMIT {
            pool.push(bytes);
        }
    }
}

/// One owned SCK `CVPixelBuffer` kept alive after the callback returns.
pub struct NativeCapturedPixelBuffer {
    pixel_buffer: CVPixelBuffer,
}

impl NativeCapturedPixelBuffer {
    pub(crate) fn new(pixel_buffer: CVPixelBuffer) -> Self {
        Self { pixel_buffer }
    }

    pub fn pixel_format(&self) -> u32 {
        self.pixel_buffer.pixel_format()
    }

    pub fn copy_nv12_payload(&self) -> Result<CapturedFramePayload, CaptureError> {
        self.copy_nv12_payload_with_pool(None)
    }

    pub(crate) fn copy_nv12_payload_with_pool(
        &self,
        pool: Option<&CaptureBufferPool>,
    ) -> Result<CapturedFramePayload, CaptureError> {
        let fmt = self.pixel_format();
        if fmt != FMT_NV12_VIDEO_RANGE {
            return Err(CaptureError::ScreenCaptureKit(format!(
                "native fallback: unexpected pixel format 0x{fmt:08x} (wanted '420v' NV12)"
            )));
        }
        copy_nv12_payload(&self.pixel_buffer, pool)
    }

    /// Returns a +1 retained raw pointer for LiveKit's consuming native-buffer
    /// bridge. The caller must pass it exactly once to
    /// `NativeBuffer::from_cv_pixel_buffer`; that bridge releases this retain.
    pub unsafe fn retained_ptr_for_consuming_native_buffer(&self) -> *mut c_void {
        let retained = self.pixel_buffer.clone();
        let ptr = retained.as_ptr();
        std::mem::forget(retained);
        ptr
    }
}

/// Owned pixel payload for one captured frame.
///
/// The BGRA arm is retained as a parked fallback for tests and any future
/// non-SCK source. The live ScreenCaptureKit window path should produce
/// `Native` after issue #179, with `Nv12` retained for snapshot/copy fallback.
pub enum CapturedFramePayload {
    Bgra {
        data: PooledFrameData,
        bytes_per_row: usize,
    },
    Nv12 {
        y: PooledFrameData,
        y_stride: u32,
        uv: PooledFrameData,
        uv_stride: u32,
    },
    Native {
        pixel_buffer: NativeCapturedPixelBuffer,
    },
}

impl CapturedFramePayload {
    pub fn primary_plane(&self) -> Option<(&[u8], usize)> {
        match self {
            Self::Bgra {
                data,
                bytes_per_row,
            } => Some((data, *bytes_per_row)),
            Self::Nv12 { y, y_stride, .. } => Some((y, *y_stride as usize)),
            Self::Native { .. } => None,
        }
    }

    pub fn payload_kind(&self) -> &'static str {
        match self {
            Self::Bgra { .. } => "BGRA",
            Self::Nv12 { .. } => "NV12",
            Self::Native { .. } => "Native",
        }
    }
}

/// A single captured frame, handed to the caller's callback.
///
/// Native window frames own the retained `CVPixelBuffer` returned by
/// `sample.image_buffer()`, so the IOSurface survives the callback. Snapshot
/// and fallback frames may still carry copied BGRA/NV12 planes.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub payload: CapturedFramePayload,
    /// Source capture scale: captured pixels per source logical point. Used
    /// by receivers to present Retina captures at the sharer's logical size.
    pub source_scale: f64,
    /// True only after the capture-owned geometry gate accepted this raster.
    /// Snapshot force-push must fail closed when this proof is absent.
    pub layout_validated: bool,
    /// Source capture color profile for this frame. NV12 frames prefer
    /// CoreVideo's delivered YCbCr/color attachments and fall back to the
    /// configured display profile; H.264 VUI emission is still not exposed by
    /// the current encoder path.
    pub color_profile: VideoColorProfile,
    /// Monotonic frame index within this capture session, 1-based. Purely a
    /// local capture-side counter (NOT the SPEC.md §7 embedded/published
    /// timestamp counter — that one travels through LiveKit's own frame
    /// metadata trailer, see `transport.rs`). Useful for this module's own
    /// frame-count/rate logging.
    pub sequence: u64,
    /// SCK's own per-frame status attachment (`SCStreamFrameInfo.status`):
    /// `Complete`/`Idle`/`Blank`/`Suspended`/`Started`/`Stopped`. This is the
    /// authoritative "is this a fresh frame or a resend of unchanged content"
    /// signal from ScreenCaptureKit itself — used by the capture-freeze
    /// diagnostics in `session/share.rs` to tell an occluded/idle source
    /// (source stopped drawing) apart from a genuinely wedged stream. `None`
    /// if the attachment wasn't present (older macOS / non-screen output).
    pub frame_status: Option<screencapturekit::cm::SCFrameStatus>,
    /// Number of dirty rects SCK reported for this frame
    /// (`SCStreamFrameInfo.dirtyRects`). 0 means "nothing changed since the
    /// last frame" — the strongest signal that the source app is not redrawing
    /// (e.g. occluded/idle), independent of pixel hashing -- but ONLY when
    /// `dirty_rects_known` is true; see that field.
    pub dirty_rect_count: usize,
    /// Total area (in captured px²) of this frame's dirty rects. Diagnostic
    /// only; pairs with `dirty_rect_count`.
    pub dirty_area_px: u64,
    /// Whether SCK actually delivered a `dirtyRects` attachment for this
    /// frame (`sample.dirty_rects()` returned `Some(_)`, even if the array
    /// was empty) vs. the attachment being absent or unparseable (bridge
    /// falls back to `None` on a missing/empty attachment or on ANY rect
    /// failing `CGRect(dictionaryRepresentation:)`). `dirty_rect_count == 0`
    /// is only an affirmative "nothing changed" signal when this is `true`.
    /// The dirty-rect-skip gate in `session/share.rs` MUST treat
    /// `dirty_rects_known == false` as "unknown, assume changed" (force a
    /// push) -- collapsing "unknown" into "no change" reintroduces the exact
    /// blind-spot failure class (#38) this signal was chosen to avoid.
    pub dirty_rects_known: bool,
    /// CPU time in the ScreenCaptureKit callback from attempting the
    /// read-only CVPixelBuffer lock through copying bytes into Petal's pooled
    /// frame buffer. Native frames do no callback-side lock/copy and set this to
    /// zero.
    pub lock_copy_ms: f64,
    /// Native display-region generation that produced this frame. `None`
    /// denotes a non-region capture; region frames are rejected at the pump
    /// boundary if this no longer matches the applied generation.
    pub region_generation: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("Screen Recording permission has not been granted")]
    PermissionDenied,
    #[error("window {0} not found in current shareable content (closed, or invalid id)")]
    WindowNotFound(u32),
    /// #712: a picker-selected *display* share's id lives in the same `u32`
    /// slot as a window id (`SharedSourceKind::Display`). Kept distinct from
    /// `WindowNotFound` so a genuinely-disconnected display doesn't get
    /// misreported with window phrasing, and so callers that must
    /// distinguish "which shareable-content list did we search" can.
    #[error("display {0} not found in current shareable content (disconnected, or invalid id)")]
    DisplayNotFound(u32),
    #[error("ScreenCaptureKit error: {0}")]
    ScreenCaptureKit(String),
}

/// Opt-in, privacy-safe accounting for why a stream has not yielded an
/// accepted frame. Normal product capture does not create or retain this.
#[derive(Clone, Default)]
pub struct CaptureDiagnostics(Arc<CaptureDiagnosticsState>);

#[derive(Default)]
struct CaptureDiagnosticsState {
    accepted_frames: AtomicU64,
    no_buffer_frames: AtomicU64,
    layout_rejections: AtomicU64,
    pixel_format_rejections: AtomicU64,
    stream_errors: AtomicU64,
    last_pixel_format: Mutex<Option<u32>>,
    last_stream_error: Mutex<Option<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureDiagnosticsSnapshot {
    pub accepted_frames: u64,
    pub no_buffer_frames: u64,
    pub layout_rejections: u64,
    pub pixel_format_rejections: u64,
    pub stream_errors: u64,
    pub last_pixel_format: Option<u32>,
    pub last_stream_error: Option<String>,
}

impl CaptureDiagnostics {
    pub fn snapshot(&self) -> CaptureDiagnosticsSnapshot {
        CaptureDiagnosticsSnapshot {
            accepted_frames: self.0.accepted_frames.load(Ordering::Relaxed),
            no_buffer_frames: self.0.no_buffer_frames.load(Ordering::Relaxed),
            layout_rejections: self.0.layout_rejections.load(Ordering::Relaxed),
            pixel_format_rejections: self.0.pixel_format_rejections.load(Ordering::Relaxed),
            stream_errors: self.0.stream_errors.load(Ordering::Relaxed),
            last_pixel_format: *self.0.last_pixel_format.lock_unpoisoned(),
            last_stream_error: self.0.last_stream_error.lock_unpoisoned().clone(),
        }
    }

    fn record_stream_error(&self, error: String) {
        self.0.stream_errors.fetch_add(1, Ordering::Relaxed);
        *self.0.last_stream_error.lock_unpoisoned() = Some(error);
    }

    fn record_no_buffer(&self) {
        self.0.no_buffer_frames.fetch_add(1, Ordering::Relaxed);
    }

    fn record_layout_rejection(&self) {
        self.0.layout_rejections.fetch_add(1, Ordering::Relaxed);
    }

    fn record_pixel_format_rejection(&self, pixel_format: u32) {
        self.0
            .pixel_format_rejections
            .fetch_add(1, Ordering::Relaxed);
        *self.0.last_pixel_format.lock_unpoisoned() = Some(pixel_format);
    }

    fn record_accepted_frame(&self) {
        self.0.accepted_frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// Proof-phase decision for one delivered region sample. Returns the
/// generation to stamp (`Accept`) or `Drop` for a stale pre-reconfiguration
/// frame. Same-size moves accept the first delivered frame after
/// `update_configuration` returns -- SCK has no per-sample configuration ack;
/// that residual window is the documented waiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionFenceDecision {
    Accept(u64),
    Drop,
}

pub(crate) fn region_fence_decision(
    pending: Option<crate::region_window::PendingRegionConfiguration>,
    current_generation: u64,
    delivered: (u32, u32),
    is_region: bool,
) -> RegionFenceDecision {
    if !is_region {
        return RegionFenceDecision::Accept(0);
    }
    match pending {
        Some(pending) => {
            if delivered != (pending.expected_width, pending.expected_height) {
                RegionFenceDecision::Drop
            } else {
                RegionFenceDecision::Accept(pending.generation)
            }
        }
        None if current_generation == 0 => RegionFenceDecision::Drop,
        None => RegionFenceDecision::Accept(current_generation),
    }
}

pub(crate) fn region_frame_generation_is_current(
    active_generation: Option<u64>,
    frame_generation: Option<u64>,
) -> bool {
    match (active_generation, frame_generation) {
        (None, None) => true,
        (Some(generation), Some(frame_generation)) => {
            generation != 0 && generation == frame_generation
        }
        _ => false,
    }
}

/// Capture configuration proof state. The native stream may emit old-size
/// samples after `update_configuration`; keep the generation pending until a
/// matching output proves the new ROI is live.
#[derive(Debug, Clone)]
struct PendingRegionConfigurationState {
    configuration: crate::region_window::PendingRegionConfiguration,
    started_at: std::time::Instant,
}

/// A live capture session for one window. Dropping this stops the stream
/// (see `SCStream`'s own `Drop`, which calls `sc_stream_release`).
pub struct WindowCapture {
    stream: SCStream,
    window_id: u32,
    fps: Arc<AtomicU32>,
    color_profile: VideoColorProfile,
    source_layout: Arc<Mutex<CaptureSourceLayout>>,
    resolution: Arc<Mutex<CaptureResolution>>,
    /// Receiver-driven maximum long edge in physical pixels. This is
    /// independent of the user's resolution preference: the smaller of the
    /// two limits wins, so a manual P1080/P1440/UHD4K choice remains a hard
    /// cap while small remote windows avoid needless capture/encode work.
    demand_long_edge: Arc<Mutex<Option<u32>>>,
    /// Retained copy of the stream's content filter, reused for on-demand
    /// snapshot pulls (`WindowCaptureConfig::snapshot_frame`).
    filter: Arc<Mutex<SCContentFilter>>,
    source_rect: Arc<Mutex<Option<screencapturekit::cg::CGRect>>>,
    region_display_id: Arc<Mutex<Option<u32>>>,
    region_generation: Arc<AtomicU64>,
    /// Proof-phase fence for the newest applied ROI configuration.
    pending_region_generation: Arc<Mutex<Option<PendingRegionConfigurationState>>>,
    region_proof_warning_at: Arc<Mutex<Option<std::time::Instant>>>,
    region_transaction: Arc<Mutex<()>>,
    origin: CaptureSourceOrigin,
    layout_gate: LayoutIntegrityGate,
    layout_error: Arc<dyn Fn(String) + Send + Sync>,
    /// Last successfully committed output size and its effective source scale.
    /// Callbacks read them atomically so a new surface is never paired with
    /// scale metadata from the prior configuration.
    configured_state: Arc<Mutex<ConfiguredCaptureState>>,
    /// #905 review (Finding 6): a no-image-buffer streak still active when
    /// capture stops would otherwise never get its EXIT log line (nothing
    /// ever supplies a real buffer again to trigger it) -- `stop()` reads
    /// these to emit a final summary instead of silently losing the count.
    no_buffer_streak_start_us: Arc<AtomicU64>,
    no_buffer_streak_samples: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct WindowCaptureConfig {
    stream: SCStream,
    window_id: u32,
    fps: Arc<AtomicU32>,
    color_profile: VideoColorProfile,
    source_layout: Arc<Mutex<CaptureSourceLayout>>,
    resolution: Arc<Mutex<CaptureResolution>>,
    demand_long_edge: Arc<Mutex<Option<u32>>>,
    filter: Arc<Mutex<SCContentFilter>>,
    source_rect: Arc<Mutex<Option<screencapturekit::cg::CGRect>>>,
    region_display_id: Arc<Mutex<Option<u32>>>,
    region_generation: Arc<AtomicU64>,
    /// Proof-phase fence for the newest applied ROI configuration.
    pending_region_generation: Arc<Mutex<Option<PendingRegionConfigurationState>>>,
    region_proof_warning_at: Arc<Mutex<Option<std::time::Instant>>>,
    region_transaction: Arc<Mutex<()>>,
    origin: CaptureSourceOrigin,
    layout_gate: LayoutIntegrityGate,
    layout_error: Arc<dyn Fn(String) + Send + Sync>,
    configured_state: Arc<Mutex<ConfiguredCaptureState>>,
}

#[derive(Debug, Clone, Copy)]
struct CaptureSourceLayout {
    logical_width: f64,
    logical_height: f64,
    backing_scale: f64,
}

impl CaptureSourceLayout {
    fn backing_pixel_size(self) -> (u32, u32, f64) {
        capture_pixel_size(self.logical_width, self.logical_height, self.backing_scale)
    }

    /// The capture scale `update_stream_configuration` records for a
    /// configured output size. Derived from the LONG axis only, so the short
    /// axis has no self-consistency -- which is why dividing a delivered
    /// frame by it cannot round-trip (#841, see `observe_delivered_frame`).
    fn source_scale_for_configured_size(&self, width: u32, height: u32) -> f64 {
        let logical_w = self.logical_width.max(1.0);
        let logical_h = self.logical_height.max(1.0);
        if logical_w >= logical_h {
            (width as f64 / logical_w).max(0.01)
        } else {
            (height as f64 / logical_h).max(0.01)
        }
    }

    /// #841: a delivered frame must NEVER redefine source geometry.
    ///
    /// This used to assign `logical_* = frame_pixels / source_scale`. The
    /// layout gate only accepts a frame once `buffer_size ==
    /// configured_output` (`layout_decision`), and `source_scale` is
    /// `configured_long / logical_long`, so the long axis was an exact
    /// identity and the short axis was pure rounding noise -- which flipped
    /// the computed target 1654<->1652 forever and republished a live display
    /// share ~3x/second until it died. Genuine content-geometry changes reach
    /// the size authority through the layout gate's ROI memo
    /// (`roi_adjusted_size`), never through this struct.
    fn observe_delivered_frame(
        &mut self,
        frame: (u32, u32),
        configured_output: (u32, u32),
        _source_scale: f64,
    ) {
        debug_assert_eq!(
            frame, configured_output,
            "the layout gate must reject a frame that does not match the configured output \
             before it reaches the source layout"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ConfiguredCaptureState {
    output: (u32, u32),
    source_scale: f64,
}

#[derive(Debug)]
struct PreparedCaptureSource {
    filter: SCContentFilter,
    logical_width: f64,
    logical_height: f64,
    backing_scale: f64,
    color_profile: VideoColorProfile,
    source_rect: Option<screencapturekit::cg::CGRect>,
    source_display_id: Option<u32>,
    source_generation: u64,
    origin: CaptureSourceOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureSourceOrigin {
    DirectWindowId,
    SystemPicker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutDecision {
    Accept { width: u32, height: u32 },
    Reconfigure { width: u32, height: u32 },
    Defer,
    Invalid,
}

#[derive(Debug, Default)]
struct LayoutIntegrityState {
    failed: bool,
    pending_target: Option<(u32, u32)>,
    /// The ROI currently in force, keyed by the capture-size authority's own
    /// computed size that produced it (#804). Without this key the two size
    /// authorities disagree forever: `capture_size_for_resolution` recomputes
    /// the padded size, `apply_quality` sees it differ from the published ROI
    /// size and republishes back to the padded size, and the gate asks for the
    /// ROI again -- an endless stop/start loop on a live share.
    roi_adjustment: Option<LayoutRoiAdjustment>,
    /// The ONE ROI target that has been requested `LAYOUT_ROI_MAX_ATTEMPTS`
    /// times without ever being acknowledged. Its padded raster is accepted
    /// as-is from then on: a couple of pillarbox pixels beat tearing a live
    /// publication down forever (CLAUDE.md, "never show a black frame").
    ///
    /// Scoped to the target, NOT a blanket flag. `Reconfigure` is also the
    /// live-resize follow mechanism, so a blanket "accept everything" would
    /// silently pin the share at a stale output size for the rest of the
    /// meeting -- a guard that is correct until the window lifecycle moves
    /// under it (the #416 shape). A real resize produces a different target
    /// and takes the normal path.
    abandoned_roi_target: Option<(u32, u32)>,
}

/// One acknowledged content-rect ROI plus the computed capture size it
/// replaces, so both size authorities can agree on one number (#804).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutRoiAdjustment {
    pub(crate) base: (u32, u32),
    pub(crate) adjusted: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutGateAction {
    Accept,
    Reconfigure { width: u32, height: u32 },
    Defer,
    FailFirst,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutObservationRoute {
    Stream,
    Snapshot,
}

/// Privacy-safe gate shared by stream callbacks and snapshot pulls. A
/// well-formed ROI inside a fixed output is recoverable: request an output
/// resize and defer that padded raster. A live resize emits a stream of
/// distinct ROI targets faster than SCStream acknowledges them -- each newer
/// target SUPERSEDES the pending one and the share settles on the newest
/// (2026-07-30 defect A: three targets in 87ms used to be terminal and
/// unpublished a healthy share). Only malformed geometry fails the gate, and
/// even that is recovered by the monitor's in-place capture restart.
#[derive(Clone, Default)]
pub(crate) struct LayoutIntegrityGate(Arc<Mutex<LayoutIntegrityState>>);

impl LayoutIntegrityGate {
    pub(crate) fn is_failed(&self) -> bool {
        self.0.lock_unpoisoned().failed
    }

    pub(crate) fn observe(
        &self,
        decision: LayoutDecision,
        route: LayoutObservationRoute,
    ) -> LayoutGateAction {
        let mut state = self.0.lock_unpoisoned();
        if state.failed {
            return LayoutGateAction::Failed;
        }
        match decision {
            LayoutDecision::Accept { width, height } => {
                if state
                    .pending_target
                    .is_some_and(|target| target != (width, height))
                {
                    return LayoutGateAction::Defer;
                }
                // A screenshot may be valid at the requested size while an
                // older stream surface is still queued. Only a matching
                // SCStream callback acknowledges the stream reconfiguration.
                if state.pending_target.is_some() && route == LayoutObservationRoute::Snapshot {
                    return LayoutGateAction::Accept;
                }
                state.pending_target = None;
                LayoutGateAction::Accept
            }
            LayoutDecision::Invalid => {
                state.failed = true;
                LayoutGateAction::FailFirst
            }
            LayoutDecision::Defer => LayoutGateAction::Defer,
            LayoutDecision::Reconfigure { width, height } => {
                let target = (width, height);
                // #804: THIS target was requested repeatedly and never
                // acknowledged, so take its padded raster as-is rather than
                // restarting capture forever. Any other target -- a real
                // resize -- still follows the normal path.
                if state.abandoned_roi_target == Some(target) {
                    state.pending_target = None;
                    return LayoutGateAction::Accept;
                }
                if state.pending_target == Some(target) {
                    return LayoutGateAction::Defer;
                }
                // A newer ROI target supersedes the pending one -- NEVER a
                // terminal verdict (2026-07-30 defect A: a resize drag's
                // third distinct target tore down a healthy live share). A
                // target that is never acknowledged is the monitor's ack
                // timeout, which restarts the capture in place.
                state.pending_target = Some(target);
                LayoutGateAction::Reconfigure { width, height }
            }
        }
    }

    pub(crate) fn fail(&self) {
        let mut state = self.0.lock_unpoisoned();
        state.failed = true;
    }

    pub(crate) fn pending_reconfiguration(&self) -> Option<(u32, u32)> {
        self.0.lock_unpoisoned().pending_target
    }

    /// Record the ROI now being requested together with the capture-size
    /// authority's own computed size it supersedes (#804). Both sides then
    /// read one number instead of fighting over two.
    pub(crate) fn record_roi_adjustment(&self, base: (u32, u32), adjusted: (u32, u32)) {
        let mut state = self.0.lock_unpoisoned();
        state.roi_adjustment = if base == adjusted {
            None
        } else {
            Some(LayoutRoiAdjustment { base, adjusted })
        };
    }

    /// The ROI in force for `base`, if the authority's computed size still
    /// matches the one that produced it. A resolution, receiver-demand, or
    /// source-size change moves `base` and the adjustment simply stops
    /// applying -- no explicit invalidation to forget.
    pub(crate) fn roi_adjusted_size(&self, base: (u32, u32)) -> Option<(u32, u32)> {
        self.0
            .lock_unpoisoned()
            .roi_adjustment
            .filter(|adjustment| adjustment.base == base)
            .map(|adjustment| adjustment.adjusted)
    }

    /// Give up on one unacknowledgeable ROI target and accept its padded
    /// raster from here on (#804). Returns false when that same target was
    /// already abandoned, so the caller logs once. The adjustment is dropped
    /// with it: the stream stays at the padded size, so the size authority
    /// must go back to computing that same size.
    pub(crate) fn abandon_roi(&self, target: (u32, u32)) -> bool {
        let mut state = self.0.lock_unpoisoned();
        state.pending_target = None;
        state.roi_adjustment = None;
        let newly = state.abandoned_roi_target != Some(target);
        state.abandoned_roi_target = Some(target);
        newly
    }

    #[cfg(test)]
    pub(crate) fn seed_pending_reconfiguration(&self, width: u32, height: u32) {
        self.0.lock_unpoisoned().pending_target = Some((width, height));
    }

    /// Serialize the terminal layout verdict with ActiveShare insertion.
    /// A callback that fails the gate before this lock wins prevents
    /// activation; one that fails afterward observes an already-owned share
    /// that the monitor can retire.
    pub(crate) fn activate_if_valid<T>(&self, activate: impl FnOnce() -> T) -> Option<T> {
        let state = self.0.lock_unpoisoned();
        if state.failed {
            None
        } else {
            Some(activate())
        }
    }
}

/// Fold an in-force content-rect ROI into the size authority's own computed
/// size (#804), carrying the source scale proportionally so a frame is never
/// paired with scale metadata from the size it replaced.
fn roi_adjusted_capture_size(
    computed: (u32, u32, f64),
    roi: Option<(u32, u32)>,
) -> (u32, u32, f64) {
    let (width, height, scale) = computed;
    let Some((roi_width, roi_height)) = roi else {
        return computed;
    };
    let long_edge = width.max(height).max(1) as f64;
    let roi_long_edge = roi_width.max(roi_height) as f64;
    (
        roi_width,
        roi_height,
        (scale * roi_long_edge / long_edge).max(0.01),
    )
}

fn even_capture_dimension(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 2.0 || value > u32::MAX as f64 {
        return None;
    }
    let rounded = value.round() as u32;
    Some(rounded.max(2) & !1)
}

pub(crate) fn layout_decision(
    origin: CaptureSourceOrigin,
    buffer_size: (u32, u32),
    configured_output: (u32, u32),
    content_rect: Option<screencapturekit::cg::CGRect>,
    scale_factor: Option<f64>,
    content_scale: Option<f64>,
) -> LayoutDecision {
    let (width, height) = buffer_size;
    if width == 0 || height == 0 || configured_output.0 == 0 || configured_output.1 == 0 {
        return LayoutDecision::Invalid;
    }

    match (content_rect, scale_factor, content_scale) {
        (None, None, None) if origin == CaptureSourceOrigin::DirectWindowId => {
            if buffer_size == configured_output {
                LayoutDecision::Accept { width, height }
            } else {
                LayoutDecision::Defer
            }
        }
        (Some(rect), Some(scale_factor), Some(content_scale)) => {
            let x = rect.origin.x;
            let y = rect.origin.y;
            let w = rect.size.width;
            let h = rect.size.height;
            if ![x, y, w, h, scale_factor, content_scale]
                .into_iter()
                .all(f64::is_finite)
                || scale_factor <= 0.0
                || content_scale <= 0.0
                || w <= 0.0
                || h <= 0.0
            {
                return LayoutDecision::Invalid;
            }
            // ScreenCaptureKit reports contentRect in points. Convert it with
            // scaleFactor, not contentScale. The latter already describes
            // configured capture downscaling; multiplying by it here caused
            // the #548 1920x1200 -> 960x600 -> 240x150 failure.
            let x = x * scale_factor;
            let y = y * scale_factor;
            let w = w * scale_factor;
            let h = h * scale_factor;
            let right = x + w;
            let bottom = y + h;
            if ![x, y, w, h, right, bottom].into_iter().all(f64::is_finite)
                || x < -CONTENT_RECT_EDGE_TOLERANCE_PX
                || y < -CONTENT_RECT_EDGE_TOLERANCE_PX
                || right > width as f64 + CONTENT_RECT_EDGE_TOLERANCE_PX
                || bottom > height as f64 + CONTENT_RECT_EDGE_TOLERANCE_PX
            {
                return LayoutDecision::Invalid;
            }

            if buffer_size != configured_output {
                return LayoutDecision::Defer;
            }

            let full_width = (w - width as f64).abs() <= CONTENT_RECT_EDGE_TOLERANCE_PX;
            let full_height = (h - height as f64).abs() <= CONTENT_RECT_EDGE_TOLERANCE_PX;
            let at_origin = x.abs() <= CONTENT_RECT_EDGE_TOLERANCE_PX
                && y.abs() <= CONTENT_RECT_EDGE_TOLERANCE_PX;
            if at_origin && full_width && full_height {
                LayoutDecision::Accept { width, height }
            } else {
                match (even_capture_dimension(w), even_capture_dimension(h)) {
                    (Some(width), Some(height)) => LayoutDecision::Reconfigure { width, height },
                    _ => LayoutDecision::Invalid,
                }
            }
        }
        _ => LayoutDecision::Invalid,
    }
}

/// Keep callbacks on the prior committed size until SCK accepts the update.
/// This makes queued old surfaces defer instead of becoming new ROI targets
/// during the configuration boundary (#548).
fn commit_configured_state_after<T, E>(
    configured_state: &Mutex<ConfiguredCaptureState>,
    new_state: ConfiguredCaptureState,
    update: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let mut committed = configured_state.lock_unpoisoned();
    let value = update()?;
    *committed = new_state;
    Ok(value)
}

pub(crate) fn layout_event(action: LayoutGateAction) -> Option<String> {
    match action {
        LayoutGateAction::Reconfigure { width, height } => Some(format!(
            "{CAPTURE_LAYOUT_RECONFIGURE_PREFIX}{width}x{height}"
        )),
        LayoutGateAction::FailFirst => Some(CAPTURE_LAYOUT_INVALID.to_string()),
        LayoutGateAction::Accept | LayoutGateAction::Defer | LayoutGateAction::Failed => None,
    }
}

pub(crate) fn parse_layout_reconfigure(error: &str) -> Option<(u32, u32)> {
    let dimensions = error.strip_prefix(CAPTURE_LAYOUT_RECONFIGURE_PREFIX)?;
    let (width, height) = dimensions.split_once('x')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

impl WindowCapture {
    /// Start capturing `window_id` at its current backing-store size,
    /// invoking `on_frame` for every real captured frame.
    ///
    /// `on_frame` runs on ScreenCaptureKit's dedicated output dispatch queue
    /// (see the `screencapturekit` crate's `add_output_handler` docs) --
    /// NOT the calling thread -- so it must be `Send + Sync` and should not
    /// block for long (mirrors the crate's own guidance: "a slow handler...
    /// cannot block callbacks on another [queue]", though here there's only
    /// one queue since we only register a Screen handler).
    pub fn start(
        window_id: u32,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        Self::start_with_error_handler(window_id, 30, on_frame, |_| {})
    }

    /// Start capturing with a ScreenCaptureKit stop/error callback. The
    /// callback fires only for unexpected stream stops reported by SCK, not
    /// for our own clean `stop_capture()` calls.
    pub fn start_with_error_handler(
        window_id: u32,
        fps: u32,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        Self::start_with_error_handler_at_resolution(
            window_id,
            fps,
            CaptureResolution::default(),
            on_frame,
            on_error,
        )
    }

    /// Start capture with opt-in diagnostic accounting. This is intentionally
    /// separate from the production entry points so diagnostics do not change
    /// their behavior or allocate state unless a caller explicitly asks.
    pub fn start_with_error_handler_and_diagnostics(
        window_id: u32,
        fps: u32,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
        diagnostics: CaptureDiagnostics,
    ) -> Result<Self, CaptureError> {
        Self::start_with_error_handler_at_resolution_and_diagnostics(
            window_id,
            fps,
            CaptureResolution::default(),
            on_frame,
            on_error,
            Some(diagnostics),
        )
    }

    pub fn start_with_error_handler_at_resolution(
        window_id: u32,
        fps: u32,
        resolution: CaptureResolution,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        Self::start_with_error_handler_at_resolution_and_diagnostics(
            window_id, fps, resolution, on_frame, on_error, None,
        )
    }

    fn start_with_error_handler_at_resolution_and_diagnostics(
        window_id: u32,
        fps: u32,
        resolution: CaptureResolution,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
        diagnostics: Option<CaptureDiagnostics>,
    ) -> Result<Self, CaptureError> {
        if !screen_recording_preflight(window_id) {
            return Err(CaptureError::PermissionDenied);
        }
        let source = prepare_direct_window_source(window_id)?;
        Self::start_prepared(
            window_id,
            fps,
            resolution,
            source,
            on_frame,
            on_error,
            diagnostics,
        )
    }

    /// Display-share counterpart to `start_with_error_handler_at_resolution`
    /// (#712). `display_id` is the same `u32` a `SharedSourceKind::Display`
    /// share stores in its `window_id` slot (see `session/share.rs`'s
    /// `ActiveShare::source_kind`) -- it is passed through to
    /// `start_prepared` unchanged, exactly as `prepare_direct_window_source`'s
    /// window id is, since nothing downstream of `PreparedCaptureSource`
    /// (stream config, snapshot pulls, logging) treats the id as anything
    /// more than an opaque label once the filter is built.
    pub fn start_display_region_with_error_handler_at_resolution(
        selector_window_id: u32,
        region: crate::region_window::RegionRect,
        fps: u32,
        resolution: CaptureResolution,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        if !region.is_positive() {
            return Err(CaptureError::WindowNotFound(selector_window_id));
        }
        if !crate::window_source::has_screen_recording_access() {
            return Err(CaptureError::PermissionDenied);
        }
        let source = prepare_direct_display_region_source(selector_window_id, region)?;
        Self::start_prepared(
            selector_window_id,
            fps,
            resolution,
            source,
            on_frame,
            on_error,
            None,
        )
    }

    pub fn start_display_with_error_handler_at_resolution(
        display_id: u32,
        fps: u32,
        resolution: CaptureResolution,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        if !screen_recording_preflight(display_id) {
            return Err(CaptureError::PermissionDenied);
        }
        let source = prepare_direct_display_source(display_id)?;
        Self::start_prepared(
            display_id, fps, resolution, source, on_frame, on_error, None,
        )
    }

    /// Start capturing from a system `SCContentSharingPicker` result. This
    /// keeps the picker-owned `SCContentFilter` intact; rebuilding a filter
    /// from the selected window id would put us back on the private direct
    /// capture path that macOS 15 warns about.
    pub fn start_with_picker_filter(
        window_id: u32,
        filter: SCContentFilter,
        logical_width: f64,
        logical_height: f64,
        point_pixel_scale: f64,
        color_profile: VideoColorProfile,
        fps: u32,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        Self::start_with_picker_filter_at_resolution(
            window_id,
            filter,
            logical_width,
            logical_height,
            point_pixel_scale,
            color_profile,
            fps,
            CaptureResolution::default(),
            on_frame,
            on_error,
        )
    }

    pub fn start_with_picker_filter_at_resolution(
        window_id: u32,
        filter: SCContentFilter,
        logical_width: f64,
        logical_height: f64,
        point_pixel_scale: f64,
        color_profile: VideoColorProfile,
        fps: u32,
        resolution: CaptureResolution,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, CaptureError> {
        if !screen_recording_preflight(window_id) {
            return Err(CaptureError::PermissionDenied);
        }

        let source = PreparedCaptureSource {
            filter,
            logical_width,
            logical_height,
            backing_scale: point_pixel_scale,
            color_profile,
            source_rect: None,
            source_display_id: None,
            source_generation: 0,
            origin: CaptureSourceOrigin::SystemPicker,
        };
        Self::start_prepared(window_id, fps, resolution, source, on_frame, on_error, None)
    }

    fn start_prepared(
        window_id: u32,
        fps: u32,
        resolution: CaptureResolution,
        source: PreparedCaptureSource,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
        diagnostics: Option<CaptureDiagnostics>,
    ) -> Result<Self, CaptureError> {
        if !crate::window_source::has_screen_recording_access() {
            return Err(CaptureError::PermissionDenied);
        }

        let PreparedCaptureSource {
            filter,
            logical_width,
            logical_height,
            backing_scale,
            color_profile,
            source_rect,
            source_display_id,
            source_generation,
            origin,
        } = source;
        let source_layout = CaptureSourceLayout {
            logical_width,
            logical_height,
            backing_scale: backing_scale.max(1.0),
        };
        let (backing_width, backing_height, backing_scale) = source_layout.backing_pixel_size();
        let (width, height, initial_source_scale) =
            cap_capture_size(backing_width, backing_height, backing_scale, resolution);

        let fps = sanitize_capture_fps(fps);
        let filter = Arc::new(Mutex::new(filter));
        let capture_path = if source_display_id.is_some() {
            "DirectDisplayRegion"
        } else {
            match origin {
                CaptureSourceOrigin::DirectWindowId => "DirectWindowId",
                CaptureSourceOrigin::SystemPicker => "SystemPicker",
            }
        };
        let source_rect = Arc::new(Mutex::new(source_rect));
        let region_display_id = Arc::new(Mutex::new(source_display_id));
        let region_generation = Arc::new(AtomicU64::new(source_generation));
        let pending_region_generation: Arc<Mutex<Option<PendingRegionConfigurationState>>> =
            Arc::new(Mutex::new(None));
        let region_proof_warning_at = Arc::new(Mutex::new(None));
        let region_transaction = Arc::new(Mutex::new(()));
        let config = stream_configuration(
            width,
            height,
            fps,
            color_profile,
            *source_rect.lock_unpoisoned(),
        );
        let source_layout = Arc::new(Mutex::new(source_layout));
        let demand_long_edge = Arc::new(Mutex::new(None));
        let configured_state = Arc::new(Mutex::new(ConfiguredCaptureState {
            output: (width, height),
            source_scale: initial_source_scale,
        }));

        log::info!(
            "capture: window {window_id} configured {width}x{height}px via {capture_path} (layout origin {origin:?}, resolution {resolution:?}, backing {backing_width}x{backing_height}px, scale {initial_source_scale:.2}, color_profile {color_profile:?})"
        );
        log::info!(
            "capture: window {window_id} creating SCStream via {capture_path} (layout origin {origin:?})"
        );

        let layout_gate = LayoutIntegrityGate::default();
        let layout_error: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_error);
        let delegate_error = layout_error.clone();
        let diagnostics_for_delegate = diagnostics.clone();
        let mut stream = SCStream::new_with_delegate(
            &filter.lock_unpoisoned(),
            &config,
            ErrorHandler::new(move |error| {
                let error = error.to_string();
                if let Some(diagnostics) = &diagnostics_for_delegate {
                    diagnostics.record_stream_error(error.clone());
                }
                delegate_error(error);
            }),
        );

        let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let logged_unexpected_format = Arc::new(AtomicU32::new(0));
        // Diagnostics (#capture-freeze): count SCK samples that carry no image
        // buffer (typically `Idle`/`Blank` status) so a genuinely-alive stream
        // sending only "nothing changed" frames is visible in the log rather
        // than looking identical to a dead stream.
        let no_buffer_frames = Arc::new(AtomicU64::new(0));
        // #905: 0 means "not currently in a no-image-buffer streak". Holds
        // the `now_us()` timestamp of the streak's first sample otherwise --
        // used to log on STATE CHANGE (streak start/end) plus a rolled-up
        // count, instead of the old ~1/sec-per-stream heartbeat, which alone
        // produced 277,633 lines (24.5%) of a real 263 MB field log.
        let no_buffer_streak_start_us = Arc::new(AtomicU64::new(0));
        let no_buffer_streak_samples = Arc::new(AtomicU64::new(0));
        let no_buffer_streak_start_us_for_handler = no_buffer_streak_start_us.clone();
        let no_buffer_streak_samples_for_handler = no_buffer_streak_samples.clone();
        let source_layout_for_handler = source_layout.clone();
        let configured_state_for_handler = configured_state.clone();
        let source_rect_for_handler = source_rect.clone();
        let region_generation_for_handler = region_generation.clone();
        let pending_region_generation_for_handler = pending_region_generation.clone();
        let region_transaction_for_handler = region_transaction.clone();
        let layout_gate_for_handler = layout_gate.clone();
        let layout_error_for_handler = layout_error.clone();
        let diagnostics_for_handler = diagnostics.clone();
        stream.add_output_handler(
            move |sample: screencapturekit::cm::CMSampleBuffer, of_type| {
                use screencapturekit::cm::CMSampleBufferSCExt;
                let _region_transaction = region_transaction_for_handler.lock_unpoisoned();
                if of_type != SCStreamOutputType::Screen {
                    return;
                }
                // A region reconfiguration is a transaction: no frame is
                // accepted while the native source is being updated. The
                // zero-generation gate is applied AFTER the pixel-buffer
                // dimensions are known (region_fence_decision below) so the
                // proof phase can distinguish stale pre-reconfiguration
                // frames from frames of the live configuration.
                let mut frame_region_generation: Option<u64> = None;
                // Read SCK's own change signals BEFORE the image-buffer check,
                // so idle/blank frames (which often carry no image buffer) are
                // still observed by the freeze diagnostics.
                let frame_status = sample.frame_status();
                let dirty_rects = sample.dirty_rects();
                let dirty_rects_known = dirty_rects.is_some();
                let (dirty_rect_count, dirty_area_px) = match dirty_rects {
                    Some(rects) => {
                        let area: f64 = rects
                            .iter()
                            .map(|r| r.size.width.max(0.0) * r.size.height.max(0.0))
                            .sum();
                        (rects.len(), area.round().max(0.0) as u64)
                    }
                    None => (0, 0),
                };
                let Some(pixel_buffer) = sample.image_buffer() else {
                    // Alive but no pixels. #905: log on STATE CHANGE (streak
                    // start), not per sample -- the old ~1/sec-per-stream
                    // heartbeat here alone was 277,633 lines / 24.5% of a
                    // real 263 MB field log. The streak's END (with a
                    // rolled-up sample count) is logged below, at the first
                    // sample that has a real image buffer again.
                    let count = no_buffer_frames.fetch_add(1, Ordering::Relaxed) + 1;
                    no_buffer_streak_samples_for_handler.fetch_add(1, Ordering::Relaxed);
                    if let Some(diagnostics) = &diagnostics_for_handler {
                        diagnostics.record_no_buffer();
                    }
                    let now = crate::time_util::now_us();
                    if no_buffer_streak_start_us_for_handler
                        .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        log::info!(
                            "capture-diag: window {window_id} SCK sample with NO image buffer status={:?} dirty_rects={} (stream alive, source not drawing) -- streak start [{count} such frames total this stream]",
                            frame_status,
                            dirty_rect_count
                        );
                    }
                    return;
                };
                let streak_start = no_buffer_streak_start_us_for_handler.swap(0, Ordering::Relaxed);
                if streak_start != 0 {
                    let samples = no_buffer_streak_samples_for_handler.swap(0, Ordering::Relaxed);
                    let duration_s =
                        crate::time_util::now_us().saturating_sub(streak_start) as f64 / 1_000_000.0;
                    log::info!(
                        "capture-diag: window {window_id} NO-image-buffer streak ended after {duration_s:.1}s ({samples} samples)"
                    );
                }
                let frame_info = sample.frame_info();
                let configured_state = *configured_state_for_handler.lock_unpoisoned();
                let layout_action = layout_gate_for_handler.observe(
                    layout_decision(
                        origin,
                        (
                            pixel_buffer.width() as u32,
                            pixel_buffer.height() as u32,
                        ),
                        configured_state.output,
                        frame_info.as_ref().and_then(|info| info.content_rect),
                        frame_info.as_ref().and_then(|info| info.scale_factor),
                        frame_info.as_ref().and_then(|info| info.content_scale),
                    ),
                    LayoutObservationRoute::Stream,
                );
                if layout_action != LayoutGateAction::Accept {
                    if let Some(diagnostics) = &diagnostics_for_handler {
                        diagnostics.record_layout_rejection();
                    }
                    if let Some(event) = layout_event(layout_action) {
                        layout_error_for_handler(event);
                    }
                    return;
                }
                let fmt = pixel_buffer.pixel_format();
                if fmt != FMT_NV12_VIDEO_RANGE {
                    if let Some(diagnostics) = &diagnostics_for_handler {
                        diagnostics.record_pixel_format_rejection(fmt);
                    }
                    if logged_unexpected_format.swap(fmt, Ordering::Relaxed) != fmt {
                        log::warn!(
                            "capture: window {window_id} unexpected pixel format 0x{fmt:08x} (wanted '420v' NV12) -- dropping frames of this format"
                        );
                    }
                    return;
                }
                let seq = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let source_scale = configured_state.source_scale;
                source_layout_for_handler
                    .lock_unpoisoned()
                    .observe_delivered_frame(
                        (pixel_buffer.width() as u32, pixel_buffer.height() as u32),
                        configured_state.output,
                        source_scale,
                    );
                let frame_color_profile =
                    color_profile_for_nv12_pixel_buffer(&pixel_buffer, color_profile);
                let width = pixel_buffer.width() as u32;
                let height = pixel_buffer.height() as u32;
                // Proof phase: a pending generation is stamped only by a
                // frame whose dimensions match the NEW configuration; frames
                // of the old dimensions are stale pre-reconfiguration samples
                // and are dropped. When old and new dimensions are identical
                // (same-size selector move), the first delivered frame after
                // `update_configuration` returns is accepted -- SCK exposes
                // no per-sample configuration ack, so that residual window
                // is the documented waiver (see verification doc).
                let pending = pending_region_generation_for_handler
                    .lock_unpoisoned()
                    .as_ref()
                    .map(|pending| pending.configuration);
                let fence = region_fence_decision(
                    pending,
                    region_generation_for_handler.load(Ordering::Acquire),
                    (width, height),
                    source_rect_for_handler.lock_unpoisoned().is_some(),
                );
                match fence {
                    RegionFenceDecision::Accept(generation) => {
                        if pending.is_some() {
                            region_generation_for_handler.store(generation, Ordering::Release);
                            *pending_region_generation_for_handler.lock_unpoisoned() = None;
                        }
                        frame_region_generation = (generation != 0).then_some(generation);
                    }
                    RegionFenceDecision::Drop => return,
                }
                if let Some(generation) = frame_region_generation {
                    if generation == 0
                        || region_generation_for_handler.load(Ordering::Acquire) != generation
                    {
                        return;
                    }
                }
                if let Some(diagnostics) = &diagnostics_for_handler {
                    diagnostics.record_accepted_frame();
                }
                on_frame(CapturedFrame {
                    width,
                    height,
                    payload: CapturedFramePayload::Native {
                        pixel_buffer: NativeCapturedPixelBuffer::new(pixel_buffer),
                    },
                    source_scale,
                    layout_validated: true,
                    color_profile: frame_color_profile,
                    sequence: seq,
                    frame_status,
                    dirty_rect_count,
                    dirty_area_px,
                    dirty_rects_known,
                    lock_copy_ms: 0.0,
                    region_generation: frame_region_generation,
                });
            },
            SCStreamOutputType::Screen,
        );

        stream
            .start_capture()
            .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;

        Ok(Self {
            stream,
            window_id,
            fps: Arc::new(AtomicU32::new(fps)),
            color_profile,
            source_layout,
            resolution: Arc::new(Mutex::new(resolution)),
            demand_long_edge,
            filter,
            source_rect,
            region_display_id,
            region_generation,
            pending_region_generation,
            region_proof_warning_at,
            region_transaction,
            origin,
            layout_gate,
            layout_error,
            configured_state,
            no_buffer_streak_start_us,
            no_buffer_streak_samples,
        })
    }

    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    pub fn stop(&self) -> Result<(), CaptureError> {
        // #905 review (Finding 6): flush a still-open no-image-buffer
        // streak's summary before tearing down -- otherwise a share that
        // ends mid-streak (a common real shape: hide/close while the
        // source isn't drawing) never gets an EXIT line and its sample
        // count is silently lost.
        let streak_start = self.no_buffer_streak_start_us.swap(0, Ordering::Relaxed);
        if streak_start != 0 {
            let samples = self.no_buffer_streak_samples.swap(0, Ordering::Relaxed);
            let duration_s =
                crate::time_util::now_us().saturating_sub(streak_start) as f64 / 1_000_000.0;
            log::info!(
                "capture-diag: window {} NO-image-buffer streak ended after {duration_s:.1}s ({samples} samples) -- capture stopped mid-streak",
                self.window_id
            );
        }
        self.stream
            .stop_capture()
            .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))
    }

    pub fn configuration_handle(&self) -> WindowCaptureConfig {
        WindowCaptureConfig {
            stream: self.stream.clone(),
            source_rect: self.source_rect.clone(),
            region_generation: self.region_generation.clone(),
            pending_region_generation: self.pending_region_generation.clone(),
            region_proof_warning_at: self.region_proof_warning_at.clone(),
            region_transaction: self.region_transaction.clone(),
            window_id: self.window_id,
            fps: self.fps.clone(),
            color_profile: self.color_profile,
            source_layout: self.source_layout.clone(),
            resolution: self.resolution.clone(),
            demand_long_edge: self.demand_long_edge.clone(),
            filter: self.filter.clone(),
            region_display_id: self.region_display_id.clone(),
            origin: self.origin,
            layout_gate: self.layout_gate.clone(),
            layout_error: self.layout_error.clone(),
            configured_state: self.configured_state.clone(),
        }
    }

    pub(crate) fn layout_gate(&self) -> LayoutIntegrityGate {
        self.layout_gate.clone()
    }

    pub fn fps(&self) -> u32 {
        self.fps.load(Ordering::Relaxed)
    }

    pub fn color_profile(&self) -> VideoColorProfile {
        self.color_profile
    }
}

fn screen_recording_preflight(window_id: u32) -> bool {
    let granted = crate::window_source::has_screen_recording_access();
    log::info!(
        "capture: window {window_id} permission check -- Screen Recording {}",
        if granted { "GRANTED" } else { "DENIED" }
    );
    granted
}

fn prepare_direct_window_source(window_id: u32) -> Result<PreparedCaptureSource, CaptureError> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;

    let window = content
        .windows()
        .into_iter()
        .find(|w| w.window_id() == window_id)
        .ok_or(CaptureError::WindowNotFound(window_id))?;

    let frame = window.frame();
    let color_profile = color_profile_for_window_rect(
        CaptureRect::new(
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        ),
        &content.displays(),
    )
    .unwrap_or_else(VideoColorProfile::legacy_publish_default);
    let filter = SCContentFilter::create().with_window(&window).build();
    Ok(PreparedCaptureSource {
        backing_scale: f64::from(filter.point_pixel_scale()).max(1.0),
        logical_width: frame.size.width,
        logical_height: frame.size.height,
        color_profile,
        filter,
        source_rect: None,
        source_display_id: None,
        source_generation: 0,
        origin: CaptureSourceOrigin::DirectWindowId,
    })
}

/// Display counterpart to `prepare_direct_window_source` (#712). Mirrors it
/// exactly except it searches `content.displays()` -- which
/// `prepare_direct_window_source` never does, since `content.windows()` can
/// never contain a display -- and builds the filter via
/// `SCContentFilter::create().with_display(..)`.
///
/// `origin` is still `CaptureSourceOrigin::DirectWindowId`, not a dedicated
/// display variant: that field only distinguishes "no picker-supplied content
/// rect, so the layout gate expects an exact buffer/output match"
/// (`layout_decision`'s `(None, None, None)` arm) from a `SystemPicker`
/// filter's content-rect reporting. A direct, non-picker whole-display filter
/// (`with_display(..).with_excluding_windows(&[])`, same as the window path's
/// unwindowed direct filter) has that same no-content-rect shape, so reusing
/// the existing variant is correct, not a shortcut.
fn prepare_direct_display_region_source(
    selector_window_id: u32,
    region: crate::region_window::RegionRect,
) -> Result<PreparedCaptureSource, CaptureError> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;
    let display = content
        .displays()
        .into_iter()
        .find(|display| {
            let frame = display.frame();
            region.x >= frame.origin.x
                && region.y >= frame.origin.y
                && region.right() <= frame.origin.x + frame.size.width
                && region.bottom() <= frame.origin.y + frame.size.height
        })
        .ok_or(CaptureError::DisplayNotFound(0))?;
    if !content
        .windows()
        .iter()
        .any(|window| window.window_id() == selector_window_id)
    {
        return Err(CaptureError::WindowNotFound(selector_window_id));
    }
    let display_frame = display.frame();
    let source_rect = screencapturekit::cg::CGRect::new(
        region.x - display_frame.origin.x,
        region.y - display_frame.origin.y,
        region.width,
        region.height,
    );
    let excluded: Vec<_> = content
        .windows()
        .into_iter()
        .filter(|window| {
            window.window_id() == selector_window_id
                || window
                    .owning_application()
                    .is_some_and(|app| app.process_id() == std::process::id() as i32)
        })
        .collect();
    let excluded_refs: Vec<_> = excluded.iter().collect();
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&excluded_refs)
        .build();
    let scale = f64::from(filter.point_pixel_scale()).max(1.0);
    crate::region_window::update_display(
        selector_window_id,
        Some(crate::region_window::RegionDisplay {
            id: display.display_id(),
            frame: crate::region_window::RegionRect::new(
                display_frame.origin.x,
                display_frame.origin.y,
                display_frame.size.width,
                display_frame.size.height,
            ),
            scale,
        }),
    );
    Ok(PreparedCaptureSource {
        filter,
        logical_width: region.width,
        logical_height: region.height,
        backing_scale: scale,
        color_profile: color_profile_for_display_id(display.display_id())
            .unwrap_or_else(VideoColorProfile::legacy_publish_default),
        source_rect: Some(source_rect),
        source_display_id: Some(display.display_id()),
        source_generation: crate::region_window::resolve(selector_window_id)
            .map(|source| source.generation.0)
            .unwrap_or(1),
        origin: CaptureSourceOrigin::DirectWindowId,
    })
}

fn prepare_direct_display_source(display_id: u32) -> Result<PreparedCaptureSource, CaptureError> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;

    let display = content
        .displays()
        .into_iter()
        .find(|d| d.display_id() == display_id)
        .ok_or(CaptureError::DisplayNotFound(display_id))?;

    let color_profile = color_profile_for_display_id(display_id)
        .unwrap_or_else(VideoColorProfile::legacy_publish_default);
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();
    Ok(PreparedCaptureSource {
        backing_scale: f64::from(filter.point_pixel_scale()).max(1.0),
        logical_width: f64::from(display.width()),
        logical_height: f64::from(display.height()),
        color_profile,
        filter,
        source_rect: None,
        source_display_id: None,
        source_generation: 0,
        origin: CaptureSourceOrigin::DirectWindowId,
    })
}

impl WindowCaptureConfig {
    pub(crate) fn layout_gate(&self) -> LayoutIntegrityGate {
        self.layout_gate.clone()
    }

    pub(crate) fn fps(&self) -> u32 {
        self.fps.load(Ordering::Relaxed)
    }

    /// Change ONLY the capture cadence.
    ///
    /// #804: this used to take the caller's published width/height, and every
    /// caller passed the *published* size. When a content-rect ROI had just
    /// been requested but not yet acknowledged, an fps update 29ms later
    /// re-applied the pre-ROI size, so no frame could ever match the pending
    /// target, the 2s ack timeout fired, and capture restarted -- forever.
    /// The configured output is the stream's own truth; an fps change must
    /// never resize.
    ///
    /// The size is resolved INSIDE the commit's critical section, not read
    /// first and applied after. Reading it separately leaves the original
    /// two-writer window open in miniature: this task reads the pre-ROI size,
    /// the monitor commits and applies the ROI, and this task then re-applies
    /// its stale size -- byte for byte the #804 signature, just microseconds
    /// wide instead of 29ms.
    pub fn update_fps(&self, fps: u32) -> Result<(), CaptureError> {
        let _region_transaction = self.region_transaction.lock_unpoisoned();
        let fps = sanitize_capture_fps(fps);
        let resolution = self.resolution();
        let mut committed = self.configured_state.lock_unpoisoned();
        let (width, height) = committed.output;
        let config = stream_configuration(
            width,
            height,
            fps,
            self.color_profile,
            *self.source_rect.lock_unpoisoned(),
        );
        self.stream
            .update_configuration(&config)
            .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;
        committed.source_scale = self.source_scale_for_configured_size_locked(width, height);
        drop(committed);
        self.fps.store(fps, Ordering::Relaxed);
        *self.resolution.lock_unpoisoned() = resolution;
        log::info!(
            "capture: window {} updated stream cadence to {}fps at the committed {}x{} ({:?})",
            self.window_id,
            fps,
            width,
            height,
            resolution
        );
        Ok(())
    }

    pub(crate) fn configured_output(&self) -> (u32, u32) {
        self.configured_state.lock_unpoisoned().output
    }

    pub(crate) fn current_region_generation(&self) -> Option<u64> {
        self.source_rect
            .lock_unpoisoned()
            .as_ref()
            .map(|_| self.region_generation.load(Ordering::Acquire))
    }

    pub(crate) fn frame_matches_current_region(&self, frame: &CapturedFrame) -> bool {
        region_frame_generation_is_current(
            self.current_region_generation(),
            frame.region_generation,
        )
    }

    pub(crate) fn with_current_region_frame<T>(
        &self,
        frame: &CapturedFrame,
        publish: impl FnOnce() -> T,
    ) -> Option<T> {
        let _region_transaction = self.region_transaction.lock_unpoisoned();
        self.frame_matches_current_region(frame).then(publish)
    }

    fn region_proof_warning_is_due(&self, now: std::time::Instant) -> bool {
        let mut last_warning = self.region_proof_warning_at.lock_unpoisoned();
        if last_warning.is_some_and(|last| {
            now.duration_since(last) < REGION_PROOF_WARNING_INTERVAL
        }) {
            return false;
        }
        *last_warning = Some(now);
        true
    }

    /// Apply the newest registered region before the next frame is accepted.
    /// The zero generation is the in-flight fence: the output callback drops
    /// samples while ScreenCaptureKit applies the new native source rectangle.
    pub fn refresh_region_source(&self) -> Result<bool, CaptureError> {
        let _region_transaction = self.region_transaction.lock_unpoisoned();
        if self.source_rect.lock_unpoisoned().is_none() {
            return Ok(false);
        }
        let Some(source) = crate::region_window::resolve(self.window_id) else {
            return Err(CaptureError::WindowNotFound(self.window_id));
        };
        let Some(display) = source.display else {
            return Ok(false);
        };
        let current_generation = self.region_generation.load(Ordering::Acquire);
        let now = std::time::Instant::now();
        let pending = self.pending_region_generation.lock_unpoisoned().clone();
        let pending_for_decision = pending.as_ref().map(|pending| pending.configuration);
        let pending_elapsed = pending
            .as_ref()
            .map(|pending| now.duration_since(pending.started_at));
        match crate::region_window::region_update_decision(
            current_generation,
            source.generation.0,
            pending_for_decision,
            pending_elapsed,
        ) {
            crate::region_window::RegionUpdateDecision::Noop
            | crate::region_window::RegionUpdateDecision::WaitForProof { .. } => {
                return Ok(false);
            }
            crate::region_window::RegionUpdateDecision::ApplyLatest { .. } => {}
            crate::region_window::RegionUpdateDecision::RetryLatest { generation } => {
                if self.region_proof_warning_is_due(now) {
                    log::warn!(
                        "capture: window {} ROI generation proof timed out; retrying latest generation {}",
                        self.window_id,
                        generation
                    );
                }
            }
        }
        let display_changed = *self.region_display_id.lock_unpoisoned() != Some(display.id);
        let replacement_filter = if display_changed {
            let content = SCShareableContent::create()
                .with_on_screen_windows_only(true)
                .with_exclude_desktop_windows(true)
                .get()
                .map_err(|error| CaptureError::ScreenCaptureKit(error.to_string()))?;
            let native_display = content
                .displays()
                .into_iter()
                .find(|item| item.display_id() == display.id)
                .ok_or(CaptureError::DisplayNotFound(display.id))?;
            let excluded: Vec<_> = content
                .windows()
                .into_iter()
                .filter(|window| {
                    window.window_id() == self.window_id
                        || window
                            .owning_application()
                            .is_some_and(|app| app.process_id() == std::process::id() as i32)
                })
                .collect();
            let excluded_refs: Vec<_> = excluded.iter().collect();
            Some(
                SCContentFilter::create()
                    .with_display(&native_display)
                    .with_excluding_windows(&excluded_refs)
                    .build(),
            )
        } else {
            None
        };
        let Some(local) = display.local_roi(source.frame) else {
            return Ok(false);
        };
        let rect = screencapturekit::cg::CGRect::new(local.x, local.y, local.width, local.height);
        let layout = CaptureSourceLayout {
            logical_width: local.width,
            logical_height: local.height,
            backing_scale: display.scale.max(1.0),
        };
        let (backing_width, backing_height, backing_scale) = layout.backing_pixel_size();
        let (width, height, source_scale) = cap_capture_size_for_limits(
            backing_width,
            backing_height,
            backing_scale,
            *self.resolution.lock_unpoisoned(),
            *self.demand_long_edge.lock_unpoisoned(),
        );
        let fps = self.fps.load(Ordering::Relaxed).max(1);
        let config = stream_configuration(width, height, fps, self.color_profile, Some(rect));
        let previous_generation = current_generation;
        let previous_filter = self.filter.clone();
        let reconfiguration_started = std::time::Instant::now();
        self.region_generation.store(0, Ordering::Release);
        if let Some(filter) = replacement_filter.as_ref() {
            if let Err(error) = self.stream.update_content_filter(filter) {
                self.region_generation
                    .store(previous_generation, Ordering::Release);
                return Err(CaptureError::ScreenCaptureKit(error.to_string()));
            }
        }
        if let Err(error) = self.stream.update_configuration(&config) {
            if replacement_filter.is_some() {
                let old_filter = previous_filter.lock_unpoisoned();
                let _ = self.stream.update_content_filter(&old_filter);
            }
            self.region_generation
                .store(previous_generation, Ordering::Release);
            return Err(CaptureError::ScreenCaptureKit(error.to_string()));
        }
        if let Some(filter) = replacement_filter {
            *self.filter.lock_unpoisoned() = filter;
            *self.region_display_id.lock_unpoisoned() = Some(display.id);
        }
        *self.source_rect.lock_unpoisoned() = Some(rect);
        *self.source_layout.lock_unpoisoned() = layout;
        *self.configured_state.lock_unpoisoned() = ConfiguredCaptureState {
            output: (width, height),
            source_scale,
        };
        // Proof-based fence: hold the zero generation until a delivered frame
        // proves the new configuration is live by matching its output
        // dimensions (see the proof phase in the output handler). Same-size
        // moves accept the next delivered frame -- documented waiver.
        *self.pending_region_generation.lock_unpoisoned() = Some(
            PendingRegionConfigurationState {
                configuration: crate::region_window::PendingRegionConfiguration {
                    generation: source.generation.0,
                    expected_width: width,
                    expected_height: height,
                },
                started_at: reconfiguration_started,
            },
        );
        log::info!(
            "capture: window {} ROI generation {} applied display={} sourceRect=({:.1},{:.1},{:.1},{:.1}) output={}x{} display_changed={} native_reconfigure_ms={:.3}",
            self.window_id,
            source.generation.0,
            display.id,
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
            width,
            height,
            display_changed,
            reconfiguration_started.elapsed().as_secs_f64() * 1000.0,
        );
        Ok(true)
    }

    pub fn update_stream_configuration(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        resolution: CaptureResolution,
    ) -> Result<(), CaptureError> {
        let _region_transaction = self.region_transaction.lock_unpoisoned();
        let fps = sanitize_capture_fps(fps);
        let config = stream_configuration(
            width,
            height,
            fps,
            self.color_profile,
            *self.source_rect.lock_unpoisoned(),
        );
        let source_scale = self.source_scale_for_configured_size(width, height);
        commit_configured_state_after(
            &self.configured_state,
            ConfiguredCaptureState {
                output: (width, height),
                source_scale,
            },
            || {
                self.stream
                    .update_configuration(&config)
                    .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))
            },
        )?;
        self.fps.store(fps, Ordering::Relaxed);
        *self.resolution.lock_unpoisoned() = resolution;
        log::info!(
            "capture: window {} updated stream configuration to {}x{} at {}fps ({:?}, scale {:.2})",
            self.window_id,
            width,
            height,
            fps,
            resolution,
            source_scale
        );
        Ok(())
    }

    pub fn set_resolution_preference(&self, resolution: CaptureResolution) {
        *self.resolution.lock_unpoisoned() = resolution;
    }

    pub fn resolution(&self) -> CaptureResolution {
        *self.resolution.lock_unpoisoned()
    }

    pub fn set_demand_long_edge(&self, long_edge: Option<u32>) {
        *self.demand_long_edge.lock_unpoisoned() = long_edge.filter(|edge| *edge > 0);
    }

    pub fn demand_long_edge(&self) -> Option<u32> {
        *self.demand_long_edge.lock_unpoisoned()
    }

    /// The capture output size to publish and configure at.
    ///
    /// #804: when the layout gate has an acknowledged content-rect ROI for
    /// exactly this computed size, that ROI IS the answer. Returning the
    /// unadjusted size here is what made `apply_quality` republish back to the
    /// padded size on every reconcile while the gate asked for the ROI again --
    /// two authorities, one stream, an endless stop/start loop.
    pub fn capture_size_for_resolution(&self, resolution: CaptureResolution) -> (u32, u32, f64) {
        let computed = self.computed_capture_size_for_resolution(resolution);
        roi_adjusted_capture_size(
            computed,
            self.layout_gate.roi_adjusted_size((computed.0, computed.1)),
        )
    }

    /// The size authority's own computation, before any ROI adjustment.
    /// This is the key the adjustment is stored under, so it must stay
    /// adjustment-free.
    pub(crate) fn computed_capture_size_for_resolution(
        &self,
        resolution: CaptureResolution,
    ) -> (u32, u32, f64) {
        let (backing_width, backing_height, backing_scale) =
            self.source_layout.lock_unpoisoned().backing_pixel_size();
        cap_capture_size_for_limits(
            backing_width,
            backing_height,
            backing_scale,
            resolution,
            *self.demand_long_edge.lock_unpoisoned(),
        )
    }

    pub fn source_scale(&self) -> f64 {
        self.configured_state.lock_unpoisoned().source_scale
    }

    /// Same as `source_scale_for_configured_size`, named to make it explicit at
    /// the call site that the caller already holds `configured_state`. It takes
    /// only `source_layout`, so there is no reentrancy on that lock (the
    /// non-reentrant `std::sync::Mutex` hazard this repo documents).
    fn source_scale_for_configured_size_locked(&self, width: u32, height: u32) -> f64 {
        self.source_scale_for_configured_size(width, height)
    }

    fn source_scale_for_configured_size(&self, width: u32, height: u32) -> f64 {
        self.source_layout
            .lock_unpoisoned()
            .source_scale_for_configured_size(width, height)
    }

    /// On-demand pull of the window's CURRENT composited content via
    /// `SCScreenshotManager` (#183 snapshot-pull fallback). This retrieves
    /// fresh pixels even when the change-driven `SCStream` has gone silent —
    /// measured live: an occluded Chrome window playing audible video stops
    /// emitting stream frames entirely, but its backing content keeps
    /// advancing, and every on-demand capture returns the advanced frame.
    ///
    /// BLOCKING (SyncCompletion.wait inside the vendored crate) — call from
    /// `tokio::task::spawn_blocking`, never the main thread. Requires
    /// macOS 14.0+; on 13.x the call returns an error and callers should
    /// disable the pull path for the share (log once, keep the stream path).
    pub fn snapshot_frame(&self) -> Result<CapturedFrame, CaptureError> {
        let _region_transaction = self.region_transaction.lock_unpoisoned();
        use screencapturekit::screenshot_manager::SCScreenshotManager;
        let configured_state = *self.configured_state.lock_unpoisoned();
        let region_generation = self.current_region_generation();
        if region_generation == Some(0) {
            return Err(CaptureError::ScreenCaptureKit(
                "snapshot: region reconfiguration in progress".into(),
            ));
        }
        let (width, height) = configured_state.output;
        if width == 0 || height == 0 {
            return Err(CaptureError::ScreenCaptureKit(
                "snapshot: no capture size recorded".into(),
            ));
        }
        let config = stream_configuration(
            width,
            height,
            self.fps.load(Ordering::Relaxed).max(1),
            self.color_profile,
            *self.source_rect.lock_unpoisoned(),
        );
        let sample =
            SCScreenshotManager::capture_sample_buffer(&self.filter.lock_unpoisoned(), &config)
                .map_err(|e| CaptureError::ScreenCaptureKit(format!("snapshot: {e}")))?;
        if self.current_region_generation() != region_generation {
            return Err(CaptureError::ScreenCaptureKit(
                "snapshot: region generation changed during capture".into(),
            ));
        }
        let Some(pixel_buffer) = sample.image_buffer() else {
            return Err(CaptureError::ScreenCaptureKit(
                "snapshot: sample has no image buffer".into(),
            ));
        };
        use screencapturekit::cm::CMSampleBufferSCExt;
        let frame_info = sample.frame_info();
        let layout_action = self.layout_gate.observe(
            layout_decision(
                self.origin,
                (pixel_buffer.width() as u32, pixel_buffer.height() as u32),
                (width, height),
                frame_info.as_ref().and_then(|info| info.content_rect),
                frame_info.as_ref().and_then(|info| info.scale_factor),
                frame_info.as_ref().and_then(|info| info.content_scale),
            ),
            LayoutObservationRoute::Snapshot,
        );
        if let Some(event) = layout_event(layout_action) {
            (self.layout_error)(event.clone());
            return Err(CaptureError::ScreenCaptureKit(event));
        }
        if layout_action != LayoutGateAction::Accept {
            return Err(CaptureError::ScreenCaptureKit(
                "capture-layout-deferred".into(),
            ));
        }
        let lock_copy_started = std::time::Instant::now();
        let fmt = pixel_buffer.pixel_format();
        if fmt != FMT_NV12_VIDEO_RANGE {
            return Err(CaptureError::ScreenCaptureKit(format!(
                "snapshot: unexpected pixel format 0x{fmt:08x} (wanted '420v' NV12)"
            )));
        }
        let payload = copy_nv12_payload(&pixel_buffer, None)?;
        let lock_copy_ms = lock_copy_started.elapsed().as_secs_f64() * 1000.0;
        Ok(CapturedFrame {
            width: pixel_buffer.width() as u32,
            height: pixel_buffer.height() as u32,
            payload,
            source_scale: configured_state.source_scale,
            layout_validated: true,
            color_profile: color_profile_for_nv12_pixel_buffer(&pixel_buffer, self.color_profile),
            sequence: 0,
            frame_status: None,
            dirty_rect_count: 0,
            dirty_area_px: 0,
            // A manually-pulled screenshot has no SCK dirty-rects concept at
            // all (this bypasses the dirty-rect-skip gate via force_push_frame
            // anyway, but mark it honestly rather than "known-zero").
            dirty_rects_known: false,
            lock_copy_ms,
            region_generation,
        })
    }
}

fn reusable_capture_buffer(pool: &CaptureBufferPool, len: usize) -> Vec<u8> {
    let Ok(mut pool) = pool.lock() else {
        return Vec::with_capacity(len);
    };

    if let Some(index) = pool.iter().position(|buffer| buffer.capacity() >= len) {
        let mut buffer = pool.swap_remove(index);
        buffer.clear();
        buffer
    } else {
        Vec::with_capacity(len)
    }
}

pub(crate) fn copy_nv12_payload(
    pixel_buffer: &CVPixelBuffer,
    pool: Option<&CaptureBufferPool>,
) -> Result<CapturedFramePayload, CaptureError> {
    let guard = pixel_buffer
        .lock(CVPixelBufferLockFlags::READ_ONLY)
        .map_err(|_| CaptureError::ScreenCaptureKit("pixel buffer lock failed".into()))?;
    if guard.plane_count() < 2 {
        return Err(CaptureError::ScreenCaptureKit(
            "NV12 pixel buffer has fewer than 2 planes".into(),
        ));
    }

    let Some(y_plane) = guard.plane_data(0).filter(|plane| !plane.is_empty()) else {
        return Err(CaptureError::ScreenCaptureKit(
            "NV12 pixel buffer has empty Y plane".into(),
        ));
    };
    let Some(uv_plane) = guard.plane_data(1).filter(|plane| !plane.is_empty()) else {
        return Err(CaptureError::ScreenCaptureKit(
            "NV12 pixel buffer has empty UV plane".into(),
        ));
    };

    Ok(CapturedFramePayload::Nv12 {
        y: copy_frame_plane(y_plane, pool),
        y_stride: guard.bytes_per_row_of_plane(0) as u32,
        uv: copy_frame_plane(uv_plane, pool),
        uv_stride: guard.bytes_per_row_of_plane(1) as u32,
    })
}

fn copy_frame_plane(bytes: &[u8], pool: Option<&CaptureBufferPool>) -> PooledFrameData {
    if let Some(pool) = pool {
        let mut data = reusable_capture_buffer(pool, bytes.len());
        data.extend_from_slice(bytes);
        PooledFrameData::from_pool(data, pool.clone())
    } else {
        let mut data = Vec::with_capacity(bytes.len());
        data.extend_from_slice(bytes);
        PooledFrameData::from_vec(data)
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    // `-> bool`, not `-> u8`: must match remote_control.rs's existing
    // `CFEqual` declaration for the same C symbol -- two conflicting extern
    // "C" signatures for one symbol in the same crate is UB (rustc warns
    // `clashing_extern_declarations`), even though both happen to work on
    // this ABI since CFEqual's `Boolean` return is always exactly 0/1.
    fn CFEqual(cf1: *const c_void, cf2: *const c_void) -> bool;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVBufferCopyAttachment(
        buffer: *mut c_void,
        key: *const c_void,
        attachment_mode: *mut u32,
    ) -> *const c_void;

    static kCVImageBufferColorPrimariesKey: *const c_void;
    static kCVImageBufferTransferFunctionKey: *const c_void;
    static kCVImageBufferYCbCrMatrixKey: *const c_void;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: *const c_void;
    static kCVImageBufferColorPrimaries_EBU_3213: *const c_void;
    static kCVImageBufferColorPrimaries_SMPTE_C: *const c_void;
    static kCVImageBufferColorPrimaries_P3_D65: *const c_void;
    static kCVImageBufferTransferFunction_ITU_R_709_2: *const c_void;
    static kCVImageBufferTransferFunction_sRGB: *const c_void;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: *const c_void;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: *const c_void;
}

struct CopiedAttachment(*const c_void);

impl CopiedAttachment {
    fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

impl Drop for CopiedAttachment {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn copy_cv_attachment(
    pixel_buffer: &CVPixelBuffer,
    key: *const c_void,
) -> Option<CopiedAttachment> {
    if key.is_null() {
        return None;
    }
    let mut mode = 0_u32;
    let value = unsafe { CVBufferCopyAttachment(pixel_buffer.as_ptr(), key, &mut mode) };
    (!value.is_null()).then_some(CopiedAttachment(value))
}

fn cf_attachment_equals(value: &CopiedAttachment, target: *const c_void) -> bool {
    !target.is_null() && unsafe { CFEqual(value.as_ptr(), target) }
}

pub(crate) fn color_profile_for_nv12_pixel_buffer(
    pixel_buffer: &CVPixelBuffer,
    fallback: VideoColorProfile,
) -> VideoColorProfile {
    profile_from_cv_attachment_components(
        cv_color_primaries_attachment(pixel_buffer),
        cv_transfer_function_attachment(pixel_buffer),
        cv_ycbcr_matrix_attachment(pixel_buffer),
        fallback,
        PixelRange::Video,
    )
}

fn profile_from_cv_attachment_components(
    primaries: Option<ColorPrimaries>,
    transfer: Option<TransferFunction>,
    matrix: Option<MatrixCoefficients>,
    fallback: VideoColorProfile,
    range: PixelRange,
) -> VideoColorProfile {
    VideoColorProfile {
        primaries: primaries.unwrap_or(fallback.primaries),
        transfer: transfer.unwrap_or(fallback.transfer),
        matrix: matrix.unwrap_or(fallback.matrix),
        range,
    }
}

fn cv_color_primaries_attachment(pixel_buffer: &CVPixelBuffer) -> Option<ColorPrimaries> {
    let value = copy_cv_attachment(pixel_buffer, unsafe { kCVImageBufferColorPrimariesKey })?;
    if cf_attachment_equals(&value, unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 }) {
        Some(ColorPrimaries::Bt709)
    } else if cf_attachment_equals(&value, unsafe { kCVImageBufferColorPrimaries_EBU_3213 }) {
        Some(ColorPrimaries::Bt601Pal)
    } else if cf_attachment_equals(&value, unsafe { kCVImageBufferColorPrimaries_SMPTE_C }) {
        Some(ColorPrimaries::Bt601Ntsc)
    } else if cf_attachment_equals(&value, unsafe { kCVImageBufferColorPrimaries_P3_D65 }) {
        Some(ColorPrimaries::DisplayP3)
    } else {
        None
    }
}

fn cv_transfer_function_attachment(pixel_buffer: &CVPixelBuffer) -> Option<TransferFunction> {
    let value = copy_cv_attachment(pixel_buffer, unsafe { kCVImageBufferTransferFunctionKey })?;
    if cf_attachment_equals(&value, unsafe { kCVImageBufferTransferFunction_sRGB }) {
        Some(TransferFunction::Srgb)
    } else if cf_attachment_equals(&value, unsafe {
        kCVImageBufferTransferFunction_ITU_R_709_2
    }) {
        Some(TransferFunction::Bt709)
    } else {
        None
    }
}

fn cv_ycbcr_matrix_attachment(pixel_buffer: &CVPixelBuffer) -> Option<MatrixCoefficients> {
    let value = copy_cv_attachment(pixel_buffer, unsafe { kCVImageBufferYCbCrMatrixKey })?;
    if cf_attachment_equals(&value, unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 }) {
        Some(MatrixCoefficients::Bt709)
    } else if cf_attachment_equals(&value, unsafe { kCVImageBufferYCbCrMatrix_ITU_R_601_4 }) {
        Some(MatrixCoefficients::Bt601)
    } else {
        None
    }
}

/// #781: the configured capture size must be EVEN in both dimensions.
///
/// Every frame this sizing produces can reach the `NV12->I420` publish
/// fallback, and I420 is 4:2:0 chroma-subsampled -- an odd dimension cannot
/// form a chroma block, so the conversion fails for EVERY frame and the share
/// publishes no video at all while the sharer still sees a normally shared
/// window with its border drawn. Measured live: a 422pt-tall window at
/// `scale 1.95` configured `1280x823`, then logged
/// `NV12->I420 conversion failed for 1280x823` at frame rate until teardown.
///
/// `even_capture_dimension` (above) already existed for `layout_decision`;
/// this sizing path simply never used it. Reuse it rather than adding a second
/// rounding rule -- two independent notions of "even enough" is how they drift.
fn capture_pixel_size(logical_w: f64, logical_h: f64, scale: f64) -> (u32, u32, f64) {
    let scale = scale.max(1.0);
    let width = logical_w.round().max(1.0) as u32;
    let height = logical_h.round().max(1.0) as u32;
    (
        even_capture_dimension((width as f64) * scale).unwrap_or(2),
        even_capture_dimension((height as f64) * scale).unwrap_or(2),
        scale,
    )
}

fn sanitize_capture_fps(fps: u32) -> u32 {
    fps.max(1)
}

/// Keep enough surfaces for ScreenCaptureKit to absorb short scheduling
/// jitter without allowing the old depth-8 stale-frame reservoir. #285 made
/// depth 3 the measured latency-mode starting point; dropped-frame validation
/// remains part of #290's live matrix.
const CAPTURE_QUEUE_DEPTH: u32 = 3;

fn stream_configuration(
    width: u32,
    height: u32,
    fps: u32,
    color_profile: VideoColorProfile,
    source_rect: Option<screencapturekit::cg::CGRect>,
) -> SCStreamConfiguration {
    let mut config = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_color_space_name(color_profile.capture_color_space_name())
        // showsCursor=false per SPEC.md §4.5 ("Sharer's own cursor... so
        // the OS cursor isn't burned into the capture").
        .with_shows_cursor(false)
        .with_queue_depth(CAPTURE_QUEUE_DEPTH)
        .with_fps(sanitize_capture_fps(fps));
    if let Some(source_rect) = source_rect {
        config
            .set_source_rect(source_rect)
            .set_destination_rect(screencapturekit::cg::CGRect::new(
                0.0,
                0.0,
                width as f64,
                height as f64,
            ))
            .set_scales_to_fit(true);
    }
    config
}

pub(crate) fn detected_color_profile_for_window(
    window_id: u32,
) -> Result<VideoColorProfile, CaptureError> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;

    let window = content
        .windows()
        .into_iter()
        .find(|w| w.window_id() == window_id)
        .ok_or(CaptureError::WindowNotFound(window_id))?;

    let frame = window.frame();
    Ok(color_profile_for_window_rect(
        CaptureRect::new(
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        ),
        &content.displays(),
    )
    .unwrap_or_else(VideoColorProfile::legacy_publish_default))
}

/// Display counterpart to `detected_color_profile_for_window` (#712 point 3).
/// A display share routed here through the window-only lookup always missed
/// (a display id is never a member of `content.windows()`) and silently fell
/// back to the legacy default -- this searches `content.displays()` directly
/// via the same `color_profile_for_display_id` plumbing
/// `prepare_direct_display_source` uses.
pub(crate) fn detected_color_profile_for_display(
    display_id: u32,
) -> Result<VideoColorProfile, CaptureError> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;

    if !content
        .displays()
        .iter()
        .any(|d| d.display_id() == display_id)
    {
        return Err(CaptureError::DisplayNotFound(display_id));
    }

    Ok(color_profile_for_display_id(display_id)
        .unwrap_or_else(VideoColorProfile::legacy_publish_default))
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CaptureRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl CaptureRect {
    const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

fn color_profile_for_window_rect(
    window_rect: CaptureRect,
    displays: &[SCDisplay],
) -> Option<VideoColorProfile> {
    let display_id = source_display_id_for_window_rect(
        window_rect,
        displays.iter().map(|display| {
            let frame = display.frame();
            (
                display.display_id(),
                CaptureRect::new(
                    frame.origin.x,
                    frame.origin.y,
                    frame.size.width,
                    frame.size.height,
                ),
            )
        }),
    )?;
    color_profile_for_display_id(display_id)
}

fn source_display_id_for_window_rect(
    window_rect: CaptureRect,
    displays: impl IntoIterator<Item = (u32, CaptureRect)>,
) -> Option<u32> {
    displays
        .into_iter()
        .filter_map(|(display_id, display_rect)| {
            let area = intersection_area(window_rect, display_rect);
            (area > 0.0 && area.is_finite()).then_some((display_id, area))
        })
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(display_id, _)| display_id)
}

fn intersection_area(a: CaptureRect, b: CaptureRect) -> f64 {
    if a.width <= 0.0 || a.height <= 0.0 || b.width <= 0.0 || b.height <= 0.0 {
        return 0.0;
    }
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    ((right - left).max(0.0)) * ((bottom - top).max(0.0))
}

#[cfg(target_os = "macos")]
fn color_profile_for_display_id(display_id: u32) -> Option<VideoColorProfile> {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDisplayCopyColorSpace(display: u32) -> *mut c_void;
        fn CGColorSpaceCopyName(space: *const c_void) -> CFStringRef;
    }

    unsafe {
        let color_space = CGDisplayCopyColorSpace(display_id);
        if color_space.is_null() {
            return None;
        }

        let name = CGColorSpaceCopyName(color_space);
        CFRelease(color_space);
        if name.is_null() {
            return None;
        }

        let name = CFString::wrap_under_create_rule(name).to_string();
        crate::video_color::profile_for_cg_color_space_name(&name)
    }
}

#[cfg(not(target_os = "macos"))]
fn color_profile_for_display_id(_display_id: u32) -> Option<VideoColorProfile> {
    None
}

pub(crate) fn cap_capture_size(
    width: u32,
    height: u32,
    scale: f64,
    resolution: CaptureResolution,
) -> (u32, u32, f64) {
    let long_edge = width.max(height);
    let max_long_edge = resolution.long_edge_cap_for_native(long_edge);
    cap_capture_size_to_long_edge(width, height, scale, max_long_edge)
}

pub(crate) fn cap_capture_size_for_limits(
    width: u32,
    height: u32,
    scale: f64,
    resolution: CaptureResolution,
    demand_long_edge: Option<u32>,
) -> (u32, u32, f64) {
    let native_long_edge = width.max(height);
    let manual_cap = resolution.long_edge_cap_for_native(native_long_edge);
    let max_long_edge = demand_long_edge
        .filter(|edge| *edge > 0)
        .map(|edge| edge.min(manual_cap))
        .unwrap_or(manual_cap);
    if demand_long_edge.is_some() && native_long_edge > max_long_edge {
        // Receiver demand is already expressed in physical display pixels.
        // Land exactly on that budget so the receiver can display 1:1; the
        // integer source-scale snapping used by manual/Auto capture can
        // otherwise turn a requested 3840px Retina share into 2560px and
        // force an avoidable receiver-side upscale.
        let ratio = max_long_edge as f64 / native_long_edge as f64;
        // #781: even in BOTH axes. This branch rounds each axis by the demand
        // ratio independently, which is exactly how the live incident produced
        // an odd height: 1312x844 capped to 1280 gives 844*(1280/1312)=823.41
        // -> 823, and every NV12->I420 fallback conversion then failed forever,
        // publishing no video at all.
        return (
            even_capture_dimension(width as f64 * ratio).unwrap_or(2),
            even_capture_dimension(height as f64 * ratio).unwrap_or(2),
            scale.max(1.0) * ratio,
        );
    }
    cap_capture_size_to_long_edge(width, height, scale, max_long_edge)
}

fn cap_capture_size_to_long_edge(
    width: u32,
    height: u32,
    scale: f64,
    max_long_edge: u32,
) -> (u32, u32, f64) {
    let long_edge = width.max(height);
    let scale = scale.max(1.0);
    let max_long_edge = max_long_edge.max(1);
    if long_edge <= max_long_edge {
        // #781: even here too -- an odd native size under the cap was
        // previously passed through untouched.
        return (
            even_capture_dimension(width as f64).unwrap_or(2),
            even_capture_dimension(height as f64).unwrap_or(2),
            scale.round().max(1.0),
        );
    }

    let logical_w = (width as f64 / scale).max(1.0);
    let logical_h = (height as f64 / scale).max(1.0);
    let logical_long = logical_w.max(logical_h);
    let max_scale_for_cap = (max_long_edge as f64 / logical_long).floor();
    // Known #187 trade-off: integer effective-scale snapping keeps Retina
    // Auto/Uhd4k captures under the H.264 guardrail, so 5K@2x lands at
    // 2560px instead of a fractional scale closer to 4K.
    let effective_scale = scale.floor().min(max_scale_for_cap).max(1.0);
    // #781: even in both axes -- an odd logical size times an integer scale
    // still lands odd.
    let capped_w = even_capture_dimension(logical_w * effective_scale).unwrap_or(2);
    let capped_h = even_capture_dimension(logical_h * effective_scale).unwrap_or(2);
    if capped_w.max(capped_h) <= max_long_edge {
        return (capped_w, capped_h, effective_scale);
    }

    let ratio = max_long_edge as f64 / long_edge as f64;
    (
        even_capture_dimension(width as f64 * ratio).unwrap_or(2),
        even_capture_dimension(height as f64 * ratio).unwrap_or(2),
        scale * ratio,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_fence_stamps_only_dimension_proven_frames() {
        // Proof phase: a pending generation is stamped only by a delivered
        // frame whose dimensions match the NEW configuration.
        assert_eq!(
            region_fence_decision(
                Some(crate::region_window::PendingRegionConfiguration {
                    generation: 7,
                    expected_width: 640,
                    expected_height: 400,
                }),
                0,
                (640, 400),
                true,
            ),
            RegionFenceDecision::Accept(7)
        );
        // A frame of the OLD dimensions delivered after reconfiguration began
        // is a stale pre-update sample and must be dropped, never stamped.
        assert_eq!(
            region_fence_decision(
                Some(crate::region_window::PendingRegionConfiguration {
                    generation: 7,
                    expected_width: 640,
                    expected_height: 400,
                }),
                0,
                (800, 600),
                true,
            ),
            RegionFenceDecision::Drop
        );
        // Mid-reconfiguration with no proof available yet: zero generation
        // always drops (never publishes an unproven frame).
        assert_eq!(
            region_fence_decision(None, 0, (640, 400), true),
            RegionFenceDecision::Drop
        );
        // Settled configuration accepts under its live generation.
        assert_eq!(
            region_fence_decision(None, 7, (640, 400), true),
            RegionFenceDecision::Accept(7)
        );
        // Same-size selector move (documented waiver): old and new output
        // dimensions are identical, so the first post-update frame matches
        // and is accepted -- SCK exposes no per-sample configuration ack.
        assert_eq!(
            region_fence_decision(
                Some(crate::region_window::PendingRegionConfiguration {
                    generation: 9,
                    expected_width: 640,
                    expected_height: 400,
                }),
                0,
                (640, 400),
                true,
            ),
            RegionFenceDecision::Accept(9)
        );
        // Non-region captures bypass the fence entirely.
        assert_eq!(
            region_fence_decision(None, 0, (800, 600), false),
            RegionFenceDecision::Accept(0)
        );
    }

    #[test]
    fn opt_in_capture_diagnostics_report_each_preflight_outcome_without_pixels() {
        let diagnostics = CaptureDiagnostics::default();
        diagnostics.record_no_buffer();
        diagnostics.record_layout_rejection();
        diagnostics.record_pixel_format_rejection(0x3432_3066); // '420f'
        diagnostics.record_stream_error("stream stopped".to_string());
        diagnostics.record_accepted_frame();

        assert_eq!(
            diagnostics.snapshot(),
            CaptureDiagnosticsSnapshot {
                accepted_frames: 1,
                no_buffer_frames: 1,
                layout_rejections: 1,
                pixel_format_rejections: 1,
                stream_errors: 1,
                last_pixel_format: Some(0x3432_3066),
                last_stream_error: Some("stream stopped".to_string()),
            }
        );
    }

    #[test]
    fn capture_pixel_size_uses_filter_point_pixel_scale() {
        assert_eq!(capture_pixel_size(1512.0, 844.0, 2.0), (3024, 1688, 2.0));
    }

    /// #804 REGRESSION GUARD -- the exact live livelock, replayed.
    ///
    /// Measured on `main` (2026-08-14): a 1312x844 window capped to a
    /// receiver demand of 1280 configures 1280x822, SCK pillarboxes the
    /// content to 1278x822, the gate asks for that ROI -- and 29ms later an
    /// fps update re-applies the pre-ROI 1280x822. From then on every frame
    /// re-derives the SAME target, the gate dedups it to `Defer`, nothing can
    /// ever acknowledge, and the 2s ack timeout restarts capture. 22 restarts
    /// across 6 windows in one session.
    ///
    /// The whole point is the state the gate is left in DURING that window,
    /// so this drives the sequence rather than calling the pure decision in
    /// isolation: request, clobber, re-observe, and only then recover.
    #[test]
    fn an_unacknowledgeable_roi_stops_looping_once_abandoned_804() {
        let gate = LayoutIntegrityGate::default();

        // The gate asks for the content-rect ROI inside the padded output.
        assert_eq!(
            gate.observe(
                LayoutDecision::Reconfigure {
                    width: 1278,
                    height: 822
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Reconfigure {
                width: 1278,
                height: 822
            }
        );
        assert_eq!(gate.pending_reconfiguration(), Some((1278, 822)));

        // The clobber: the configuration goes back to 1280x822, so frames keep
        // arriving padded and re-derive the identical target. This is the
        // livelock -- it must stay a `Defer`, never a silent accept.
        for _ in 0..5 {
            assert_eq!(
                gate.observe(
                    LayoutDecision::Reconfigure {
                        width: 1278,
                        height: 822
                    },
                    LayoutObservationRoute::Stream,
                ),
                LayoutGateAction::Defer,
                "an identical unacknowledged target must not re-request"
            );
        }
        assert_eq!(gate.pending_reconfiguration(), Some((1278, 822)));

        // Bounded recovery: after LAYOUT_ROI_MAX_ATTEMPTS the share stops
        // asking and keeps the padded raster. A couple of pillarbox pixels
        // beat tearing a live publication down forever.
        assert!(
            gate.abandon_roi((1278, 822)),
            "first abandon reports itself"
        );
        assert!(
            !gate.abandon_roi((1278, 822)),
            "the same target abandoned twice is silent"
        );
        assert_eq!(gate.pending_reconfiguration(), None);
        assert_eq!(
            gate.observe(
                LayoutDecision::Reconfigure {
                    width: 1278,
                    height: 822
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Accept,
            "an abandoned ROI accepts the padded frame -- the share stays published"
        );
        assert!(
            !gate.is_failed(),
            "abandoning a ROI must never fail the gate"
        );

        // A REAL resize after abandonment must still be followed. A blanket
        // "accept everything" flag would pin the share at the stale output
        // size for the rest of the meeting -- worse than the loop it replaced.
        assert_eq!(
            gate.observe(
                LayoutDecision::Reconfigure {
                    width: 960,
                    height: 616
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Reconfigure {
                width: 960,
                height: 616
            },
            "abandonment is scoped to one target, not to the resize mechanism"
        );
    }

    /// #804: the two capture-size authorities must read ONE number. Without
    /// this, `capture_size_for_resolution` recomputes the padded size,
    /// `apply_quality` sees it differ from the published ROI size, republishes
    /// back to padded, and the gate asks for the ROI again -- forever.
    #[test]
    fn an_acknowledged_roi_overrides_the_computed_capture_size_804() {
        let gate = LayoutIntegrityGate::default();
        assert_eq!(gate.roi_adjusted_size((1280, 822)), None);

        gate.record_roi_adjustment((1280, 822), (1278, 822));
        assert_eq!(
            gate.roi_adjusted_size((1280, 822)),
            Some((1278, 822)),
            "the size authority must yield to the ROI it just asked for"
        );

        // Keyed by the computed size, so a genuine resolution / receiver-demand
        // / source-size change moves the key and the adjustment stops applying
        // on its own -- there is no stale override to forget to invalidate.
        assert_eq!(gate.roi_adjusted_size((1280, 720)), None);
        assert_eq!(gate.roi_adjusted_size((640, 410)), None);

        // A ROI equal to the computed size is not an adjustment at all.
        gate.record_roi_adjustment((1280, 822), (1280, 822));
        assert_eq!(gate.roi_adjusted_size((1280, 822)), None);

        // Abandoning drops it: the stream stays padded, so the authority must
        // go back to computing that same padded size.
        gate.record_roi_adjustment((1280, 822), (1278, 822));
        gate.abandon_roi((1278, 822));
        assert_eq!(gate.roi_adjusted_size((1280, 822)), None);
    }

    /// #804: the scale that travels with a size must describe THAT size.
    #[test]
    fn roi_adjusted_capture_size_carries_the_scale_proportionally_804() {
        assert_eq!(
            roi_adjusted_capture_size((1280, 822, 1.9512), None),
            (1280, 822, 1.9512)
        );
        let (width, height, scale) =
            roi_adjusted_capture_size((1280, 822, 1.9512), Some((1278, 822)));
        assert_eq!((width, height), (1278, 822));
        assert!(
            (scale - 1.9512 * 1278.0 / 1280.0).abs() < 1e-9,
            "scale {scale} must follow the ROI, not the size it replaced"
        );
    }

    /// #781 REGRESSION GUARD -- the exact live incident.
    ///
    /// Sentinel window 656x422pt @2x -> backing 1312x844 (already even), then a
    /// receiver demand cap of 1280 took the DEMAND branch of
    /// `cap_capture_size_for_limits`: 844 * (1280/1312) = 823.41 -> 823.
    /// Every `NV12->I420` fallback conversion then failed, forever, and the
    /// share published no video while looking healthy to the sharer.
    ///
    /// An earlier draft of this fix only evened `capture_pixel_size`, whose
    /// input here (1312x844) was ALREADY even -- it would not have prevented
    /// the incident at all. Pin the real path.
    #[test]
    fn demand_cap_never_produces_an_odd_dimension_781() {
        let (w, h, _) =
            cap_capture_size_for_limits(1312, 844, 2.0, CaptureResolution::Auto, Some(1280));
        assert_eq!(h % 2, 0, "height {h} must be even -- 823 killed the share");
        assert_eq!(w % 2, 0, "width {w} must be even");
        assert_eq!((w, h), (1280, 822));

        // Sweep the demand branch: no (native size, demand cap) pair may yield
        // an odd axis. One odd value is a total loss of video, not a
        // degradation, so this must hold everywhere.
        for h_native in [800u32, 823, 844, 900, 1080, 1200] {
            for w_native in [1280u32, 1312, 1440, 1920] {
                for demand in [640u32, 720, 1280, 1600] {
                    let (w, h, _) = cap_capture_size_for_limits(
                        w_native,
                        h_native,
                        2.0,
                        CaptureResolution::Auto,
                        Some(demand),
                    );
                    assert_eq!(
                        w % 2,
                        0,
                        "odd width {w} for {w_native}x{h_native} demand={demand}"
                    );
                    assert_eq!(
                        h % 2,
                        0,
                        "odd height {h} for {w_native}x{h_native} demand={demand}"
                    );
                }
            }
        }
    }

    /// #781: an odd capture dimension makes the NV12->I420 publish fallback
    /// fail for EVERY frame (I420 is 4:2:0), so the share silently publishes
    /// no video. This is the exact live case: a 422pt-tall window at scale
    /// 1.95 produced 1280x823 and killed the share.
    #[test]
    fn capture_pixel_size_is_always_even_so_i420_conversion_can_succeed() {
        let (w, h, _) = capture_pixel_size(656.0, 422.0, 1.95);
        assert_eq!(h % 2, 0, "height {h} must be even (#781); 823 broke I420");
        assert_eq!(w % 2, 0, "width {w} must be even (#781)");
        assert_eq!((w, h), (1278, 822));

        // Sweep: no logical size at any plausible scale may produce an odd
        // dimension. A single odd value here is a total loss of video.
        for logical in 1..400u32 {
            for scale in [1.0f64, 1.25, 1.5, 1.95, 2.0, 3.0] {
                let (w, h, _) = capture_pixel_size(logical as f64, logical as f64, scale);
                assert_eq!(w % 2, 0, "odd width for logical={logical} scale={scale}");
                assert_eq!(h % 2, 0, "odd height for logical={logical} scale={scale}");
                assert!(w >= 2 && h >= 2, "dimension collapsed below 2");
            }
        }
    }

    #[test]
    fn capture_pixel_size_never_reports_sub_one_scale() {
        assert_eq!(capture_pixel_size(640.0, 400.0, 0.0), (640, 400, 1.0));
    }

    #[test]
    fn capture_fps_is_never_zero() {
        assert_eq!(sanitize_capture_fps(0), 1);
        assert_eq!(sanitize_capture_fps(4), 4);
        assert_eq!(sanitize_capture_fps(30), 30);
    }

    #[test]
    fn stream_config_carries_capture_color_space_name() {
        let config = stream_configuration(
            1280,
            720,
            30,
            VideoColorProfile::DISPLAY_P3_BT709_FULL,
            None,
        );
        assert_eq!(config.pixel_format(), PixelFormat::YCbCr_420v);
        assert_eq!(config.queue_depth(), CAPTURE_QUEUE_DEPTH);
        assert_eq!(
            config.color_space_name().as_deref(),
            Some("kCGColorSpaceDisplayP3")
        );

        let config = stream_configuration(1280, 720, 30, VideoColorProfile::SRGB_BT709_FULL, None);
        assert_eq!(
            config.color_space_name().as_deref(),
            Some("kCGColorSpaceSRGB")
        );
    }

    #[test]
    fn nv12_attachment_profile_forces_420v_video_range() {
        let profile = profile_from_cv_attachment_components(
            Some(ColorPrimaries::DisplayP3),
            Some(TransferFunction::Srgb),
            Some(MatrixCoefficients::Bt709),
            VideoColorProfile::BT601_VIDEO,
            PixelRange::Video,
        );

        assert_eq!(
            profile,
            VideoColorProfile {
                range: PixelRange::Video,
                ..VideoColorProfile::DISPLAY_P3_BT709_FULL
            }
        );
    }

    #[test]
    fn nv12_attachment_profile_falls_back_without_legacy_bt601_mistag() {
        let profile = profile_from_cv_attachment_components(
            None,
            None,
            None,
            VideoColorProfile::SRGB_BT709_FULL,
            PixelRange::Video,
        );

        assert_eq!(
            profile,
            VideoColorProfile {
                range: PixelRange::Video,
                ..VideoColorProfile::SRGB_BT709_FULL
            }
        );
    }

    #[test]
    fn region_generation_fence_rejects_missing_zero_stale_and_mismatched_frames() {
        assert!(region_frame_generation_is_current(None, None));
        assert!(!region_frame_generation_is_current(Some(0), Some(1)));
        assert!(!region_frame_generation_is_current(Some(9), None));
        assert!(!region_frame_generation_is_current(Some(9), Some(8)));
        assert!(region_frame_generation_is_current(Some(9), Some(9)));
    }

    #[test]
    fn source_display_selection_uses_largest_window_intersection() {
        let window = CaptureRect::new(1800.0, 100.0, 400.0, 400.0);
        let displays = [
            (1, CaptureRect::new(0.0, 0.0, 1920.0, 1080.0)),
            (2, CaptureRect::new(1920.0, 0.0, 1920.0, 1080.0)),
        ];

        assert_eq!(source_display_id_for_window_rect(window, displays), Some(2));
    }

    #[test]
    fn source_display_selection_ignores_non_intersecting_displays() {
        let window = CaptureRect::new(4000.0, 100.0, 400.0, 400.0);
        let displays = [
            (1, CaptureRect::new(0.0, 0.0, 1920.0, 1080.0)),
            (2, CaptureRect::new(1920.0, 0.0, 1920.0, 1080.0)),
        ];

        assert_eq!(source_display_id_for_window_rect(window, displays), None);
    }

    #[test]
    fn capture_size_cap_snaps_to_integer_effective_source_scale() {
        let (width, height, scale) = cap_capture_size(3024, 1688, 2.0, CaptureResolution::P1440);
        assert_eq!((width, height), (1512, 844));
        assert_eq!(scale, 1.0);
    }

    #[test]
    fn capture_size_cap_uses_largest_integer_scale_under_cap() {
        let (width, height, scale) = cap_capture_size(3600, 2400, 3.0, CaptureResolution::P1440);
        assert_eq!((width, height), (2400, 1600));
        assert_eq!(scale, 2.0);
    }

    #[test]
    fn capture_resolution_auto_is_dynamic_not_p1440_alias() {
        let auto = cap_capture_size(3840, 2160, 2.0, CaptureResolution::Auto);
        let p1440 = cap_capture_size(3840, 2160, 2.0, CaptureResolution::P1440);

        assert_eq!(auto, (3840, 2160, 2.0));
        assert_eq!(p1440, (1920, 1080, 1.0));
        assert_ne!(auto, p1440);
    }

    #[test]
    fn capture_resolution_4k_cap_stays_under_h264_guardrail() {
        assert_eq!(
            CaptureResolution::Uhd4k.long_edge_cap_for_native(8192),
            3840
        );
        let (width, height, _scale) = cap_capture_size(5120, 2880, 1.0, CaptureResolution::Auto);
        assert!(width.max(height) <= crate::transport::publisher::VIDEO_TOOLBOX_H264_MAX_LONG_EDGE);
    }

    #[test]
    fn receiver_demand_lands_on_requested_physical_long_edge() {
        let captured =
            cap_capture_size_for_limits(5120, 2880, 2.0, CaptureResolution::Auto, Some(3840));
        assert_eq!(captured, (3840, 2160, 1.5));
    }

    #[test]
    fn manual_resolution_remains_hard_cap_over_receiver_demand() {
        let captured =
            cap_capture_size_for_limits(5120, 2880, 2.0, CaptureResolution::P1080, Some(3840));
        assert_eq!(captured, (1920, 1080, 0.75));
    }

    #[test]
    fn pooled_frame_data_returns_allocation_on_drop() {
        let pool: CaptureBufferPool = Arc::new(Mutex::new(Vec::new()));
        {
            let data = PooledFrameData::from_pool(vec![1, 2, 3, 4], pool.clone());
            assert_eq!(&*data, &[1, 2, 3, 4]);
        }

        let pooled = pool.lock().unwrap();
        assert_eq!(pooled.len(), 1);
        assert_eq!(pooled[0].len(), 0);
        assert!(pooled[0].capacity() >= 4);
    }

    fn test_layout_decision(
        origin: CaptureSourceOrigin,
        buffer_size: (u32, u32),
        configured_output: (u32, u32),
        rect: Option<(f64, f64, f64, f64)>,
        scale_factor: Option<f64>,
        content_scale: Option<f64>,
    ) -> LayoutDecision {
        layout_decision(
            origin,
            buffer_size,
            configured_output,
            rect.map(|(x, y, width, height)| {
                screencapturekit::cg::CGRect::new(x, y, width, height)
            }),
            scale_factor,
            content_scale,
        )
    }

    #[test]
    fn layout_validator_accepts_1x_2x_and_capped_full_fill() {
        for (buffer, rect, scale_factor, content_scale) in [
            ((800, 600), (800.0, 600.0), 1.0, 1.0),
            ((1600, 1200), (800.0, 600.0), 2.0, 1.0),
            ((960, 540), (480.0, 270.0), 2.0, 0.375),
        ] {
            assert_eq!(
                test_layout_decision(
                    CaptureSourceOrigin::SystemPicker,
                    buffer,
                    buffer,
                    Some((0.0, 0.0, rect.0, rect.1)),
                    Some(scale_factor),
                    Some(content_scale),
                ),
                LayoutDecision::Accept {
                    width: buffer.0,
                    height: buffer.1,
                }
            );
        }
    }

    #[test]
    fn layout_validator_a3_never_recursively_targets_or_accepts_240x150() {
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                (1920, 1200),
                (1920, 1200),
                Some((0.0, 0.0, 960.0, 600.0)),
                Some(2.0),
                Some(1.0),
            ),
            LayoutDecision::Accept {
                width: 1920,
                height: 1200,
            }
        );
        let downscaled = test_layout_decision(
            CaptureSourceOrigin::DirectWindowId,
            (960, 600),
            (960, 600),
            Some((0.0, 0.0, 480.0, 300.0)),
            Some(2.0),
            Some(0.5),
        );
        assert_eq!(
            downscaled,
            LayoutDecision::Accept {
                width: 960,
                height: 600,
            }
        );
        assert_ne!(
            downscaled,
            LayoutDecision::Reconfigure {
                width: 240,
                height: 150,
            }
        );
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                (240, 150),
                (960, 600),
                Some((0.0, 0.0, 120.0, 75.0)),
                Some(2.0),
                Some(0.25),
            ),
            LayoutDecision::Defer
        );
    }

    #[test]
    fn stale_surface_defers_without_consuming_recovery_then_matching_surface_accepts() {
        let stale = test_layout_decision(
            CaptureSourceOrigin::DirectWindowId,
            (1920, 1200),
            (960, 600),
            Some((0.0, 0.0, 960.0, 600.0)),
            Some(2.0),
            Some(1.0),
        );
        assert_eq!(stale, LayoutDecision::Defer);
        let gate = LayoutIntegrityGate::default();
        for _ in 0..3 {
            assert_eq!(
                gate.observe(stale, LayoutObservationRoute::Stream),
                LayoutGateAction::Defer
            );
        }
        assert_eq!(gate.pending_reconfiguration(), None);

        let matching = test_layout_decision(
            CaptureSourceOrigin::DirectWindowId,
            (960, 600),
            (960, 600),
            Some((0.0, 0.0, 480.0, 300.0)),
            Some(2.0),
            Some(0.5),
        );
        assert_eq!(
            gate.observe(matching, LayoutObservationRoute::Stream),
            LayoutGateAction::Accept
        );
    }

    #[test]
    fn padded_roi_requests_reconfiguration_and_newest_target_supersedes() {
        let decision = test_layout_decision(
            CaptureSourceOrigin::SystemPicker,
            (1000, 600),
            (1000, 600),
            Some((50.0, 0.0, 400.0, 300.0)),
            Some(2.0),
            Some(1.0),
        );
        assert_eq!(
            decision,
            LayoutDecision::Reconfigure {
                width: 800,
                height: 600
            }
        );
        let gate = LayoutIntegrityGate::default();
        assert_eq!(
            gate.observe(decision, LayoutObservationRoute::Stream),
            LayoutGateAction::Reconfigure {
                width: 800,
                height: 600
            }
        );
        assert_eq!(
            gate.observe(decision, LayoutObservationRoute::Stream),
            LayoutGateAction::Defer
        );
        // A queued full-size frame from the old configuration cannot
        // acknowledge the requested 800x600 output.
        assert_eq!(
            gate.observe(
                LayoutDecision::Accept {
                    width: 1000,
                    height: 600
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Defer
        );
        assert_eq!(
            gate.observe(
                LayoutDecision::Accept {
                    width: 800,
                    height: 600
                },
                LayoutObservationRoute::Snapshot,
            ),
            LayoutGateAction::Accept
        );
        // The valid snapshot did not acknowledge the stream generation, so
        // a queued old stream surface remains deferred.
        assert_eq!(
            gate.observe(
                LayoutDecision::Accept {
                    width: 1000,
                    height: 600
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Defer
        );
        assert_eq!(
            gate.observe(
                LayoutDecision::Accept {
                    width: 800,
                    height: 600
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Accept
        );
        assert!(!gate.is_failed());
        assert!(matches!(
            gate.observe(decision, LayoutObservationRoute::Stream),
            LayoutGateAction::Reconfigure { .. }
        ));

        // 2026-07-30 defect A regression: a live resize emits a stream of
        // DISTINCT ROI targets before any is acknowledged. Every newer
        // target supersedes the pending one; none may be terminal.
        let storm_gate = LayoutIntegrityGate::default();
        assert_eq!(
            storm_gate.observe(decision, LayoutObservationRoute::Stream),
            LayoutGateAction::Reconfigure {
                width: 800,
                height: 600
            }
        );
        for width in [780u32, 760, 740, 720, 700] {
            assert_eq!(
                storm_gate.observe(
                    LayoutDecision::Reconfigure { width, height: 600 },
                    LayoutObservationRoute::Stream,
                ),
                LayoutGateAction::Reconfigure { width, height: 600 },
                "a newer ROI target must supersede, never fail, the gate"
            );
            assert!(!storm_gate.is_failed());
            assert_eq!(storm_gate.pending_reconfiguration(), Some((width, 600)));
        }
        // Once SCK delivers a frame matching the NEWEST target, the share
        // settles and the gate is fully reset.
        assert_eq!(
            storm_gate.observe(
                LayoutDecision::Accept {
                    width: 700,
                    height: 600
                },
                LayoutObservationRoute::Stream,
            ),
            LayoutGateAction::Accept
        );
        assert!(!storm_gate.is_failed());
        assert_eq!(storm_gate.pending_reconfiguration(), None);
    }

    #[test]
    fn layout_validator_enforces_attachment_policy() {
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::SystemPicker,
                (800, 600),
                (800, 600),
                None,
                None,
                None,
            ),
            LayoutDecision::Invalid
        );
        assert!(matches!(
            test_layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                (800, 600),
                (800, 600),
                None,
                None,
                None,
            ),
            LayoutDecision::Accept { .. }
        ));
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                (1600, 1200),
                (800, 600),
                None,
                None,
                None,
            ),
            LayoutDecision::Defer
        );
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                (800, 600),
                (800, 600),
                None,
                None,
                Some(1.0),
            ),
            LayoutDecision::Invalid
        );
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                (800, 600),
                (800, 600),
                Some((0.0, 0.0, 800.0, 600.0)),
                Some(1.0),
                None,
            ),
            LayoutDecision::Invalid
        );
    }

    #[test]
    fn layout_validator_rejects_malformed_metadata_and_bounds() {
        let decide = |rect, scale_factor, content_scale| {
            test_layout_decision(
                CaptureSourceOrigin::SystemPicker,
                (800, 600),
                (800, 600),
                Some(rect),
                Some(scale_factor),
                Some(content_scale),
            )
        };

        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::SystemPicker,
                (0, 600),
                (800, 600),
                None,
                None,
                None,
            ),
            LayoutDecision::Invalid
        );
        assert_eq!(
            decide((0.0, 0.0, 800.0, 600.0), f64::NAN, 1.0),
            LayoutDecision::Invalid
        );
        assert_eq!(
            decide((0.0, 0.0, 800.0, 600.0), 1.0, 0.0),
            LayoutDecision::Invalid
        );
        assert_eq!(
            decide((-3.0, 0.0, 800.0, 600.0), 1.0, 1.0),
            LayoutDecision::Invalid
        );
        assert_eq!(
            decide((0.0, 0.0, 803.0, 600.0), 1.0, 1.0),
            LayoutDecision::Invalid
        );
        assert_eq!(
            decide((0.0, 0.0, 0.0, 600.0), 1.0, 1.0),
            LayoutDecision::Invalid
        );
    }

    #[test]
    fn valid_live_square_aspect_change_uses_bounded_reconfiguration() {
        assert_eq!(
            test_layout_decision(
                CaptureSourceOrigin::SystemPicker,
                (1000, 600),
                (1000, 600),
                Some((200.0, 0.0, 300.0, 300.0)),
                Some(2.0),
                Some(1.0),
            ),
            LayoutDecision::Reconfigure {
                width: 600,
                height: 600,
            }
        );
    }

    /// #531: a portrait stream configuration on a landscape source is the
    /// exact malformed-raster signature ("portrait raster, landscape content,
    /// black padding"). ScreenCaptureKit does not fail such a configuration --
    /// it letterboxes, rendering the aspect-fit content at the buffer's
    /// TOP-LEFT and leaving the rest black, then truthfully reports the useful
    /// sub-rect in `contentRect` and the fit ratio in `contentScale`.
    ///
    /// Every number below was measured live on 2026-07-28 by
    /// `examples/capture_probe --geometry`, capturing a real 1512x844pt
    /// landscape window at pointPixelScale 2.0 through
    /// `start_with_picker_filter` with the logical dimensions deliberately
    /// swapped (see that probe's "swapped" pass). Pre-#574 builds (the 0.7.12
    /// build 07fb3a18 that produced #531's 323x415 report) had no gate here at
    /// all and published this raster verbatim.
    ///
    /// Note `content_scale` is 0.558, NOT 1.0: it is the aspect-fit ratio, so
    /// the pixel conversion must use `scale_factor` ALONE. Multiplying by
    /// `content_scale` as well is the #548 recursive-shrink bug.
    #[test]
    fn portrait_letterbox_of_a_landscape_source_never_reaches_a_consumer_531() {
        // Frame 1: the malformed raster itself. 1688x942px of real content
        // sits at the top of a 1688x3024 portrait buffer; the lower 2082px
        // are black padding.
        let malformed = test_layout_decision(
            CaptureSourceOrigin::SystemPicker,
            (1688, 3024),
            (1688, 3024),
            Some((0.0, 0.0, 844.0000247955322, 471.12170696258545)),
            Some(2.0),
            Some(0.5582010746002197),
        );
        assert_eq!(
            malformed,
            LayoutDecision::Reconfigure {
                width: 1688,
                height: 942,
            },
            "a padded portrait raster must be refused and its useful landscape \
             ROI requested, never accepted"
        );

        // The requested output must restore the source's own orientation and
        // aspect -- reconfiguring to something still portrait would only
        // re-pad.
        let LayoutDecision::Reconfigure { width, height } = malformed else {
            unreachable!()
        };
        assert!(
            width > height,
            "recovery target {width}x{height} must be landscape like the source"
        );
        let source_aspect = 1512.0 / 844.0;
        let target_aspect = f64::from(width) / f64::from(height);
        assert!(
            (target_aspect - source_aspect).abs() / source_aspect < 0.01,
            "recovery target aspect {target_aspect} must match source aspect {source_aspect}"
        );

        // Frame 2: what the stream actually delivered after that
        // reconfiguration was applied. Content now fills the buffer, so the
        // gate accepts and the recovery terminates (no recursive shrink).
        let recovered = test_layout_decision(
            CaptureSourceOrigin::SystemPicker,
            (1688, 942),
            (1688, 942),
            Some((0.0, 0.0, 843.7820191383362, 471.0000159740448)),
            Some(2.0),
            Some(0.5580568909645081),
        );
        assert_eq!(
            recovered,
            LayoutDecision::Accept {
                width: 1688,
                height: 942,
            }
        );

        // And through the real gate: the malformed frame yields a
        // reconfiguration request rather than an accepted frame, and the
        // recovered frame clears the pending target instead of shrinking again.
        let gate = LayoutIntegrityGate::default();
        assert_eq!(
            gate.observe(malformed, LayoutObservationRoute::Stream),
            LayoutGateAction::Reconfigure {
                width: 1688,
                height: 942,
            }
        );
        assert_eq!(gate.pending_reconfiguration(), Some((1688, 942)));
        assert_eq!(
            gate.observe(recovered, LayoutObservationRoute::Stream),
            LayoutGateAction::Accept
        );
        assert_eq!(gate.pending_reconfiguration(), None);
        assert!(!gate.is_failed());
    }

    #[test]
    fn failed_stream_update_keeps_prior_output_and_source_scale() {
        let prior = ConfiguredCaptureState {
            output: (1920, 1200),
            source_scale: 2.0,
        };
        let next = ConfiguredCaptureState {
            output: (960, 600),
            source_scale: 1.0,
        };
        let configured_state = Mutex::new(prior);
        let result: Result<(), &'static str> =
            commit_configured_state_after(&configured_state, next, || Err("update failed"));
        assert_eq!(result, Err("update failed"));
        assert_eq!(*configured_state.lock_unpoisoned(), prior);

        assert_eq!(
            commit_configured_state_after(&configured_state, next, || Ok::<_, ()>(7)),
            Ok(7)
        );
        assert_eq!(*configured_state.lock_unpoisoned(), next);
    }

    #[test]
    fn active_share_insertion_is_serialized_with_terminal_layout_failure() {
        let valid_gate = LayoutIntegrityGate::default();
        let mut activated = false;
        assert_eq!(
            valid_gate.activate_if_valid(|| {
                activated = true;
                7
            }),
            Some(7)
        );
        assert!(activated);

        let failed_gate = LayoutIntegrityGate::default();
        failed_gate.fail();
        assert_eq!(failed_gate.activate_if_valid(|| 9), None);
        assert!(failed_gate.is_failed());
    }

    /// The #841 incident's source: a 16" MBP built-in display shared whole --
    /// 1728x1117pt logical at a true 2.0 backing scale (3456x2234 native).
    const INCIDENT_SOURCE: CaptureSourceLayout = CaptureSourceLayout {
        logical_width: 1728.0,
        logical_height: 1117.0,
        backing_scale: 2.0,
    };

    /// The receiver demand cap the share had stepped down to when the loop
    /// began (`Some(3456)` -> `Some(2560)`), after which it never moved.
    const INCIDENT_DEMAND_CAP: Option<u32> = Some(2560);

    /// The size authority's own answer for a given source layout, through the
    /// real `backing_pixel_size` -> `cap_capture_size_for_limits` chain.
    fn incident_computed_target(layout: &CaptureSourceLayout) -> (u32, u32) {
        let (backing_width, backing_height, backing_scale) = layout.backing_pixel_size();
        let (width, height, _) = cap_capture_size_for_limits(
            backing_width,
            backing_height,
            backing_scale,
            CaptureResolution::Auto,
            INCIDENT_DEMAND_CAP,
        );
        (width, height)
    }

    /// #841: a live display share republished its video track ~3x/second for
    /// 73 seconds (232 track sids) and then died, because the SCK frame
    /// callback re-derived `source_layout.logical_*` from every delivered
    /// frame as `frame_pixels / source_scale`.
    ///
    /// `source_scale` is the CAPPED capture scale taken from the long axis
    /// only, so that division is an identity on the long axis and pure
    /// rounding noise on the short one. The delivered frame alternated
    /// between the computed target and SCK's padded content-rect ROI (whose
    /// exact-tuple memo the oscillation invalidated every other cycle), and
    /// those two widths gave two different scales:
    ///
    /// * 2560-wide frame -> scale 1.481481 -> logical_h 1116.45 -> backing
    ///   2232 -> computed target height **1652**
    /// * 2556-wide frame -> scale 1.479167 -> logical_h 1116.85 -> backing
    ///   2234 -> computed target height **1654**
    ///
    /// A 1654-tall frame made the authority ask for 1652 and vice versa, with
    /// a fixed source and a fixed demand cap, so `quality_change_requires_
    /// republish`'s exact inequality could never be satisfied.
    ///
    /// The invariant that kills it: whatever size the last frame was
    /// delivered at, the computed target must not move.
    #[test]
    fn a_delivered_frame_never_moves_the_computed_capture_target_841() {
        // The three published sizes the live stream actually cycled between.
        const DELIVERED: [(u32, u32); 3] = [(2560, 1654), (2560, 1652), (2556, 1652)];

        let expected = incident_computed_target(&INCIDENT_SOURCE);
        assert_eq!(
            expected,
            (2560, 1654),
            "the size authority's answer for the incident source and demand cap"
        );

        // Start from each delivered size in turn -- crucially from a
        // 1654-tall frame AND a 1652-tall one, since the bug's whole shape is
        // that each makes the authority compute the other.
        for start in 0..DELIVERED.len() {
            let mut layout = INCIDENT_SOURCE;
            let mut targets = Vec::new();
            for step in 0..16 {
                let frame = DELIVERED[(start + step) % DELIVERED.len()];
                // What `update_stream_configuration` records for the size the
                // stream is running at, then what the frame callback does
                // with the frame the layout gate just accepted at that size.
                let source_scale = layout.source_scale_for_configured_size(frame.0, frame.1);
                layout.observe_delivered_frame(frame, frame, source_scale);
                targets.push(incident_computed_target(&layout));
            }
            assert!(
                targets.iter().all(|target| *target == expected),
                "delivered frames must not move the capture target (start {:?}): got {:?}",
                DELIVERED[start],
                targets
            );
            assert_eq!(
                layout.logical_width, INCIDENT_SOURCE.logical_width,
                "source geometry is the source's, not the last frame's"
            );
            assert_eq!(layout.logical_height, INCIDENT_SOURCE.logical_height);
            assert_eq!(layout.backing_scale, INCIDENT_SOURCE.backing_scale);
        }
    }
}
