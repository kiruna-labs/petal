//! Network/system conditions diagnostics (issue #19, Phase A).
//!
//! Two per-room-connection background tasks, started from
//! `session::join_room` on the exact same once-per-connection seam the
//! telepointer/presence/audio/resilience watchers already use:
//!
//! 1. **Stats poller** -- every ~1s, calls the real `get_stats()` on every
//!    published local track and every subscribed remote track on the live
//!    LiveKit room connection (the same `OutboundRtpStats`/
//!    `RemoteInboundRtpStats`/`InboundRtpStats` dictionaries the M0 latency
//!    work and the mic probe already read), folds the reports into one
//!    `StatsSample` (RTT / jitter / send+recv bandwidth / packet loss) plus a
//!    per-track `TrackHealth` list, and appends the sample to a bounded ring
//!    buffer (`HISTORY_CAP` = 120 samples ~= 2 minutes). While the cockpit
//!    panel is open (`set_cockpit_open(true)` -- an explicit gate so a closed
//!    cockpit costs nothing beyond the 1s poll itself), each tick also pushes
//!    a `network-stats` event with the full current snapshot to the main
//!    webview.
//! 2. **Event journal** -- a bounded in-memory `VecDeque` (`JOURNAL_CAP` =
//!    500 entries) of timestamped meeting events, appended from this module's
//!    own `room.subscribe()` event loop rather than by editing every seam
//!    that produces them: joined/left room, participant joined/left, share
//!    (track) published/unpublished/subscribed, mute/unmute, reconnecting/
//!    reconnected, connection-quality changes. `get_event_journal` returns
//!    it; every append also pushes a `journal-appended` event (rare,
//!    human-timescale -- not gated on the cockpit being open).
//!
//! ## Teardown (the #18 lesson: watchers must provably stop on leave)
//!
//! Both tasks self-terminate without `leave_room` needing to know this
//! module exists (deliberate -- a parallel task owns the leave path):
//! the event loop breaks on `RoomEvent::Disconnected` (fired by our own
//! `Room::close()` on leave, the same signal `presence.rs` already relies
//! on) and then bumps the shared **generation counter**, which the poller
//! checks every tick and exits on. Joining a different room also bumps the
//! generation (via a fresh `start_for_room`), so a stale poller from a
//! previous connection can never keep polling a closed room or clobber the
//! new connection's state. Both exits log a `diagnostics: ... stopped` line
//! so "the poller provably stops on leave" is checkable in petal.log.
//!
//! ## What's real vs. approximated (honesty notes)
//!
//! - RTT/jitter/loss for the SEND path come from `RemoteInboundRtpStats` --
//!   the receiver's own RTCP report about our outbound stream (real wire
//!   measurements, not estimates). When multiple tracks are published the
//!   sample takes the WORST (max) value across tracks, since they share one
//!   transport and the worst report is the honest headline number.
//! - When nothing is published (receive-only participant), the sample's
//!   jitter falls back to the max jitter across received tracks, and RTT is
//!   `None` (shown as absent, never fabricated -- no remote-inbound report
//!   exists to read RTT from without publishing).
//! - Receive-side jitter-buffer delay is the cumulative average
//!   (`jitter_buffer_delay / jitter_buffer_emitted_count`), labeled as such
//!   in the cockpit -- not a windowed "current" value. The pinned public
//!   LiveKit/libwebrtc Rust API does not expose a playout-delay RTP header
//!   extension setter, so #182 records actual/target/minimum buffer stats
//!   instead of pretending to tune a hidden receiver buffer.
//! - Bandwidth is derived from byte-counter deltas between polls (the
//!   standard webrtc-internals technique), so the first tick after
//!   connect/publish reports 0 until a second counter reading exists.
//! - Measured glass-to-glass latency is recorded only after the reliable
//!   data-channel probe has established a fresh sender<->receiver wall-clock
//!   offset. Raw `receive_us - capture_us` across two Macs is clock offset,
//!   not latency (#182), so uncalibrated frames are skipped and the cockpit
//!   falls back to the RTT/jitter-buffer estimate.
//! - The final receive-side pipeline stage is "enqueued to display": frames
//!   that reached the main-thread `AVSampleBufferDisplayLayer` enqueue call.
//!   It is not a paint/vsync completion signal.
//! - On Windows the stats poller and event journal run live (they read the
//!   same cross-platform LiveKit stats); the macOS-native display-stage
//!   feeds (glass-to-glass calibration, decoder/render stage sampling) are
//!   macOS-gated inside this module, so those per-track fields are honestly
//!   absent (null) there and the cockpit renders them as "—".

use crate::sync_ext::MutexExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::pipeline_stats::PipelineLifecycle;

/// Ring-buffer capacity for metric history: 120 samples at ~1s per sample
/// (~2 minutes), per the issue's own sizing.
const HISTORY_CAP: usize = 120;
/// Bounded in-memory event journal size, per the issue's own sizing.
const JOURNAL_CAP: usize = 500;
/// Stats poll cadence.
const POLL_INTERVAL_MS: u64 = 1000;
const HIGH_RTT_MS: f64 = 150.0;
const HIGH_JITTER_MS: f64 = 30.0;
const HIGH_LOSS_PCT: f64 = 2.0;
const FLAPPING_RECONNECTS: u32 = 2;
const HIGH_JITTER_BUFFER_MS: f64 = 80.0;
const HIGH_DROPPED_FRAMES: u32 = 30;
const LATENCY_STALE_MS: u64 = 3_000;
const CLOCK_OFFSET_STALE_MS: u64 = 10_000;
const REMOTE_PIPELINE_STALE_MS: u64 = 5_000;
const REMOTE_PIPELINE_LIFECYCLE_CAP: usize = 200;
const RENDER_PIPELINE_ESTIMATE_MS: f64 = 8.0;
/// Consecutive ~1s stats-poll ticks with zero cumulative `frames_decoded`
/// progress required before the stats-derived poller journals "stalled"
/// (#358). The sender's ~1.0-1.1s idle-refresh cadence
/// (`STATIC_REFRESH_INTERVAL_US` in `session/share.rs`) structurally aliases
/// against this poller's own ~1s tick, so a single empty sample is not
/// evidence of a real freeze -- production logs showed real (non-freeze)
/// gaps spanning up to 3 consecutive ticks, so 5 is comfortably clear of
/// that. Applies ONLY to this stats-derived path, never to
/// `record_video_stream_state_by_key`'s other, authoritative callers
/// (livekit-js `stream-state` events, `record_native_video_stream_state`).
const STALL_DEBOUNCE_TICKS: u32 = 5;

/// One ~1s aggregate sample across the whole room connection. All `Option`
/// fields are honestly absent (`null` in JSON) when no underlying stats
/// report carries them -- never zero-filled to look healthy.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsSample {
    /// Wall-clock timestamp, epoch milliseconds.
    pub t_ms: u64,
    /// Round-trip time to the SFU in ms (worst across published tracks),
    /// from the receiver's RTCP report (`RemoteInboundRtp.round_trip_time`).
    pub rtt_ms: Option<f64>,
    /// Jitter in ms (worst across tracks; send-path report preferred,
    /// receive-path fallback -- see module doc comment).
    pub jitter_ms: Option<f64>,
    /// Total send bandwidth, kbit/s (byte-counter delta across all
    /// published tracks).
    pub send_kbps: f64,
    /// Total receive bandwidth, kbit/s (delta across all subscribed tracks).
    pub recv_kbps: f64,
    /// Packet loss %, from `RemoteInboundRtp.fraction_lost` (worst across
    /// published tracks).
    pub loss_pct: Option<f64>,
    /// #683: process-wide physical memory footprint in MB
    /// (`platform::mem::process_footprint_bytes_throttled`, the same
    /// throttled reading `capture-diag` uses), sampled at this ~1s tick.
    /// `None` if the underlying platform read failed -- never zero-filled.
    pub phys_footprint_mb: Option<u32>,
    /// #683: live count of this app's own `native_display::
    /// OwnedCVPixelBuffer` decode-output buffers
    /// (`platform::mem::live_pixel_buffer_count`). `Some` only on macOS --
    /// see that function's doc comment for why this is `None`, not a
    /// plausible-looking zero, on any other platform. This counter's own
    /// blind spot (it cannot see framework-internal ScreenCaptureKit/
    /// libwebrtc buffers) is documented at its declaration in
    /// `platform::mem`.
    pub live_pixel_buffers: Option<u32>,
}

/// Current health of one published or subscribed track (no history -- the
/// per-track view is a live table, only the aggregate sample is ring-buffered).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackHealth {
    #[serde(skip_serializing)]
    pub latency_key: String,
    pub sid: String,
    /// LiveKit track name (`petal-window-<id>` / `petal-camera-<slug>` /
    /// mic tracks) -- the same name contract the compositor already keys on.
    pub name: String,
    /// Raw LiveKit track publication name before display decoration. The
    /// pipeline view uses this plus owner/window fields for exact grouping.
    pub raw_track_name: Option<String>,
    /// Remote participant identity for subscribed tracks.
    pub owner_identity: Option<String>,
    /// Parsed `petal-window-<id>` for native window shares.
    pub window_id: Option<u32>,
    /// "video" | "audio" (from the RTP stream kind).
    pub kind: String,
    /// "send" (published by us) | "recv" (subscribed remote track).
    pub direction: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Encoder implementation for send tracks (the VideoToolbox
    /// confirmation, same field `log_encoder_once` reads) / decoder
    /// implementation for recv tracks. Empty until libwebrtc reports it.
    pub codec_impl: String,
    /// `quality_limitation_reason` for send video ("none"/"cpu"/
    /// "bandwidth"/"other"); empty for recv/audio.
    pub quality_limitation: String,
    /// True when outbound stats suggest the requested VideoToolbox hardware
    /// encoder fell back to software (issue #231).
    pub software_encoder: bool,
    /// Encoder target bitrate (send only), kbit/s.
    pub target_kbps: f64,
    /// Actual measured throughput this tick, kbit/s (byte delta).
    pub actual_kbps: f64,
    pub packets_lost: i64,
    /// Send only: encoder-reported encoded frames (cumulative).
    pub frames_encoded: u32,
    /// Send only: encoder-reported keyframes (cumulative).
    pub key_frames_encoded: u32,
    /// Receive only: decoder-reported decoded frames (cumulative).
    pub frames_decoded: u32,
    /// Receive only: decoder-reported keyframes (cumulative).
    pub key_frames_decoded: u32,
    /// Receive only: decoder-reported dropped frames (cumulative).
    pub frames_dropped: u32,
    /// RTCP NACK count for this RTP stream (cumulative).
    pub nack_count: u32,
    /// RTCP FIR count for this RTP stream (cumulative).
    pub fir_count: u32,
    /// RTCP PLI count for this RTP stream (cumulative).
    pub pli_count: u32,
    /// Receive only: cumulative-average jitter buffer delay, ms.
    pub jitter_buffer_ms: Option<f64>,
    /// Receive only: cumulative-average target jitter buffer delay, ms.
    pub jitter_buffer_target_ms: Option<f64>,
    /// Receive only: cumulative-average minimum jitter buffer delay, ms.
    pub jitter_buffer_minimum_ms: Option<f64>,
    /// Best known measured glass-to-glass latency for this receive track.
    pub glass_to_glass_ms: Option<f64>,
    /// Fallback estimate: RTT/2 + receive jitter buffer + render budget.
    pub glass_to_glass_estimate_ms: Option<f64>,
    /// "calibrated" when `glass_to_glass_ms` uses a fresh data-channel
    /// clock offset, "clock-sync-pending" when raw cross-machine timestamps
    /// are deliberately not reported (#182), or "" for non-video/non-recv.
    pub glass_to_glass_status: String,
    /// "active" | "stalled" | "unknown". The Rust SDK currently does not
    /// expose SFU pause events directly, so `stalled` is a stats-derived
    /// receive-side fallback.
    pub stream_state: String,
    /// Locally measured capture stage for a shared window we are publishing.
    /// `None` serializes as `null`; a measured zero fps stays `0.0`.
    pub grabbed: Option<PipelineStageMetrics>,
    /// Merged encode/send stage. There is no separate wire probe in v1.
    pub encoded_sent: Option<PipelineStageMetrics>,
    /// Locally measured inbound RTP stage for a window we are viewing.
    pub received: Option<PipelineStageMetrics>,
    /// Locally measured decoder stage for a window we are viewing.
    pub decoded: Option<PipelineStageMetrics>,
    /// Locally measured main-thread display-layer enqueue stage for a window
    /// we are viewing. This is not a paint callback.
    pub display_enqueued: Option<PipelineStageMetrics>,
    pub frames_received: u64,
    pub frames_display_enqueued: u64,
    pub display_drop_pct: Option<f64>,
    pub display_drop_flag: bool,
    pub software_decode_fallbacks: u64,
    /// Locally measured sender-side capture state for a shared window we are
    /// publishing.
    pub capture_state: Option<CaptureStateReport>,
    /// Locally measured receiver-side freeze/drop counters for a shared
    /// window we are viewing.
    pub receiver_freeze: Option<ReceiverFreezeMetrics>,
    /// Sender-side grab stage reported by a remote peer over the data channel.
    pub remote_grabbed: Option<PipelineStageReport>,
    /// Sender-side encode/send stage reported by a remote peer over the data
    /// channel.
    pub remote_encoded_sent: Option<PipelineStageReport>,
    /// Receiver-side inbound RTP stage reported by a remote peer over the data
    /// channel.
    pub remote_received: Option<PipelineStageReport>,
    /// Receiver-side decoder stage reported by a remote peer over the data
    /// channel.
    pub remote_decoded: Option<PipelineStageReport>,
    /// Sender-side capture state reported by the remote owner.
    pub remote_capture_state: Option<RemoteCaptureStateReport>,
    /// Receiver-side freeze/drop counters reported by a remote viewer.
    pub remote_receiver_freeze: Option<RemoteReceiverFreezeReport>,
    pub remote_lifecycle: Option<RemotePipelineLifecycleReport>,
}

/// Nullable per-stage metrics for the Network Cockpit pipeline view.
/// Individual fields remain nullable because some reports carry one real
/// metric before another; absent stages render as `null`, never as zero.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineStageMetrics {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub kbps: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureStateKind {
    Live,
    Idle,
    Occluded,
    Wedged,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCpuMetrics {
    pub lock_copy_ms: Option<f64>,
    pub convert_ms: Option<f64>,
    pub capture_frame_return_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStateReport {
    pub state: CaptureStateKind,
    pub fps: Option<f64>,
    pub dirty_rect_count: Option<u32>,
    pub dirty_area_px: Option<u64>,
    pub occlusion_pct: Option<f64>,
    pub cpu: CaptureCpuMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverFreezeMetrics {
    pub freeze_count: u32,
    pub frames_dropped: u32,
    pub quality_limitation_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeStartupStageKind {
    StartRequested,
    CaptureAttemptStarted,
    FirstFrame,
    FirstFrameTimeout,
    MetadataStarted,
    MetadataWithinBudget,
    MetadataBudgetExpired,
    PublishStarted,
    PublishSucceeded,
    PublishFailed,
    FirstFramePushed,
    SnapshotPullStarted,
    SnapshotPullPushed,
    SnapshotPullFailed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeStartupStage {
    pub stage: NativeStartupStageKind,
    pub elapsed_ms: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub resolution: Option<String>,
    pub capture_path: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeStartupTimelineReport {
    pub window_id: u32,
    pub started_seq: Option<u64>,
    pub restart_generation: Option<u64>,
    pub capture_path: String,
    pub requested_fps: Option<u32>,
    pub requested_resolution: Option<String>,
    pub publication_sid: Option<String>,
    pub outcome: String,
    pub stages: Vec<NativeStartupStage>,
}

/// A stage measured by a different peer and reported over
/// `petal.pipeline-stats`. Kept separate from local stages so UI can label the
/// network/staleness boundary honestly.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineStageReport {
    pub reporter_id: String,
    pub sent_at_ms: u64,
    pub received_at_ms: u64,
    pub metrics: PipelineStageMetrics,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCaptureStateReport {
    pub reporter_id: String,
    pub sent_at_ms: u64,
    pub received_at_ms: u64,
    pub state: CaptureStateReport,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteReceiverFreezeReport {
    pub reporter_id: String,
    pub sent_at_ms: u64,
    pub received_at_ms: u64,
    pub metrics: ReceiverFreezeMetrics,
}

/// Latest lifecycle fact reported by the opposite peer. The publication SID is
/// retained only in the internal reducer, never surfaced to the cockpit.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePipelineLifecycleReport {
    pub reporter_id: String,
    pub lifecycle: String,
    pub received_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PipelineStageKind {
    Grabbed,
    EncodedSent,
    Received,
    Decoded,
}

#[derive(Debug, Clone, Eq)]
struct RemotePipelineStageKey {
    owner_identity: String,
    window_id: u32,
    reporter_id: String,
    stage: PipelineStageKind,
    publication_sid: Option<String>,
    share_epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RemoteWindowReportKey {
    owner_identity: String,
    window_id: u32,
    reporter_id: String,
    publication_sid: Option<String>,
    share_epoch: String,
}

/// Correlation key for the additive pipeline-health reducer. The epoch is
/// opaque; it is never a title, URL, pixel value, or LiveKit credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RemoteShareEpochKey {
    owner_identity: String,
    window_id: u32,
    reporter_id: String,
    publication_sid: Option<String>,
    share_epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OwnerPublicationKey {
    owner_identity: String,
    window_id: u32,
    publication_sid: String,
}

#[derive(Debug, Clone)]
struct RemoteLifecycleReport {
    lifecycle: PipelineLifecycle,
    received_at_ms: u64,
}

#[derive(Debug, Clone)]
struct RemoteSequenceReport {
    seq: u64,
    received_at_ms: u64,
}

impl PartialEq for RemotePipelineStageKey {
    fn eq(&self, other: &Self) -> bool {
        self.owner_identity == other.owner_identity
            && self.window_id == other.window_id
            && self.reporter_id == other.reporter_id
            && self.stage == other.stage
            && self.publication_sid == other.publication_sid
            && self.share_epoch == other.share_epoch
    }
}

impl Hash for RemotePipelineStageKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.owner_identity.hash(state);
        self.window_id.hash(state);
        self.reporter_id.hash(state);
        self.stage.hash(state);
        self.publication_sid.hash(state);
        self.share_epoch.hash(state);
    }
}

#[derive(Debug, Clone)]
struct LatencyObservation {
    latency_ms: f64,
    frame_id: Option<u32>,
    t_ms: u64,
}

#[derive(Debug, Clone)]
struct ClockOffsetObservation {
    /// Offset to add to a sender-wall-clock timestamp to express it in this
    /// receiver's wall-clock domain (`receiver_clock - sender_clock`), in us.
    sender_to_receiver_offset_us: i64,
    rtt_ms: f64,
    t_ms: u64,
}

/// Privacy-safe clock evidence for one caller-supplied authenticated peer.
/// Ordering matches the caller's peer list; identities are intentionally not
/// copied into this projection or serialized into cockpit artifacts.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClockCalibrationEvidence {
    pub calibrated: bool,
    pub uncertainty_ms: Option<f64>,
    pub age_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct StreamStateObservation {
    state: String,
    source: String,
}

#[derive(Debug, Clone)]
struct CaptureFrameObservation {
    sequence: u64,
    width: u32,
    height: u32,
    state: CaptureStateReport,
}

#[derive(Debug, Clone)]
struct CapturePipelineSample {
    stage: PipelineStageMetrics,
    state: CaptureStateReport,
}

#[derive(Debug, Clone, Default)]
struct ReceiverFreezeObservation {
    last_frame_id: Option<u32>,
    freeze_count: u32,
}

#[derive(Debug, Clone, Copy)]
struct CaptureRateSample {
    sequence: u64,
    t_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct DisplayEnqueueRateSample {
    frames: u64,
    t_ms: u64,
    received: u64,
}

/// Per-window grab-rate sampler. The hot `on_frame` path records only the
/// latest sequence; diagnostics ticks compute deltas so stopped callbacks
/// become a real measured 0 fps instead of preserving a healthy old value.
#[derive(Debug, Default)]
struct CapturePipelineSampler {
    latest: HashMap<u32, CaptureFrameObservation>,
    sampled: HashMap<u32, CaptureRateSample>,
}

impl CapturePipelineSampler {
    fn record_frame(
        &mut self,
        window_id: u32,
        sequence: u64,
        width: u32,
        height: u32,
        state: CaptureStateReport,
    ) {
        self.latest.insert(
            window_id,
            CaptureFrameObservation {
                sequence,
                width,
                height,
                state,
            },
        );
    }

    fn record_push_timing(
        &mut self,
        window_id: u32,
        convert_ms: f64,
        capture_frame_return_ms: f64,
    ) {
        let Some(current) = self.latest.get_mut(&window_id) else {
            return;
        };
        current.state.cpu.convert_ms = present_nonnegative_finite(convert_ms);
        current.state.cpu.capture_frame_return_ms =
            present_nonnegative_finite(capture_frame_return_ms);
    }

    fn mark_wedged(&mut self, window_id: u32) {
        if let Some(current) = self.latest.get_mut(&window_id) {
            current.state.state = CaptureStateKind::Wedged;
            current.state.fps = Some(0.0);
        }
    }

    fn clear_window(&mut self, window_id: u32) {
        self.latest.remove(&window_id);
        self.sampled.remove(&window_id);
    }

    fn clear_all(&mut self) {
        self.latest.clear();
        self.sampled.clear();
    }

    fn sample_stage(&mut self, window_id: u32, now_ms: u64) -> Option<CapturePipelineSample> {
        let current = self.latest.get(&window_id)?.clone();
        let previous = self.sampled.insert(
            window_id,
            CaptureRateSample {
                sequence: current.sequence,
                t_ms: now_ms,
            },
        );
        let fps = match previous {
            Some(prev) if now_ms > prev.t_ms && current.sequence >= prev.sequence => Some(
                (current.sequence - prev.sequence) as f64 * 1000.0 / (now_ms - prev.t_ms) as f64,
            ),
            Some(_) => Some(0.0),
            None => None,
        };

        let stage = PipelineStageMetrics {
            width: Some(current.width),
            height: Some(current.height),
            fps,
            kbps: None,
        };
        let mut state = current.state.clone();
        if state.state != CaptureStateKind::Wedged {
            state.fps = fps;
        }

        Some(CapturePipelineSample { stage, state })
    }
}

/// Per-received-window display-enqueue sampler. The compositor owns the hot
/// per-frame atomics; diagnostics samples those cumulative counters once per
/// poll so no main-thread UI event is emitted per frame.
#[derive(Debug, Default)]
struct DisplayPipelineSampler {
    sampled: HashMap<String, DisplayEnqueueRateSample>,
    drop_baselines: HashMap<String, DisplayEnqueueRateSample>,
    /// Per-track latest closed 5s window: (drop percentage, window sequence).
    /// The seq increments once per CLOSED window so consumers polling faster
    /// than the 5s window cadence (the ~1s stats tick) can tell a fresh
    /// window from a stale re-read -- see `EnqueueBackoffTrackState::
    /// last_window_seq` (#882 review).
    last_drop_pct: HashMap<String, (f64, u64)>,
    next_drop_window_seq: u64,
}

impl DisplayPipelineSampler {
    fn clear_all(&mut self) {
        self.sampled.clear();
        self.drop_baselines.clear();
        self.last_drop_pct.clear();
        // next_drop_window_seq deliberately NOT reset: consumers key
        // staleness off seq equality, and a seq that restarts at the old
        // value after a clear would alias a pre-clear window.
    }

    #[cfg(target_os = "macos")]
    fn sample_stage(
        &mut self,
        key: &str,
        snapshot: crate::compositor::DisplayEnqueueSnapshot,
        now_ms: u64,
    ) -> Option<PipelineStageMetrics> {
        if snapshot.frames_display_enqueued == 0 && snapshot.frames_received == 0 {
            self.sampled.remove(key);
            self.drop_baselines.remove(key);
            // Fable nit: a reopened/reused window must not report a stale
            // drop percentage measured before the reset.
            self.last_drop_pct.remove(key);
            return None;
        }

        let previous = self.sampled.insert(
            key.to_string(),
            DisplayEnqueueRateSample {
                frames: snapshot.frames_display_enqueued,
                t_ms: now_ms,
                received: snapshot.frames_received,
            },
        );
        let baseline =
            self.drop_baselines
                .entry(key.to_string())
                .or_insert(DisplayEnqueueRateSample {
                    frames: snapshot.frames_display_enqueued,
                    t_ms: now_ms,
                    received: snapshot.frames_received,
                });
        if now_ms >= baseline.t_ms + 5_000 {
            if snapshot.frames_received >= baseline.received
                && snapshot.frames_display_enqueued >= baseline.frames
            {
                let received = snapshot.frames_received - baseline.received;
                let displayed = snapshot.frames_display_enqueued - baseline.frames;
                if received > 0 {
                    let seq = self.next_drop_window_seq;
                    self.next_drop_window_seq += 1;
                    self.last_drop_pct.insert(
                        key.to_string(),
                        (
                            ((received.saturating_sub(displayed) as f64 / received as f64) * 100.0)
                                .clamp(0.0, 100.0),
                            seq,
                        ),
                    );
                }
            }
            *baseline = DisplayEnqueueRateSample {
                frames: snapshot.frames_display_enqueued,
                t_ms: now_ms,
                received: snapshot.frames_received,
            };
        }
        let fps = match previous {
            Some(prev) if snapshot.frames_display_enqueued < prev.frames => None,
            Some(prev) if now_ms > prev.t_ms => Some(
                (snapshot.frames_display_enqueued - prev.frames) as f64 * 1000.0
                    / (now_ms - prev.t_ms) as f64,
            ),
            Some(_) => Some(0.0),
            None => None,
        };

        Some(PipelineStageMetrics {
            width: snapshot.source_pixel_width,
            height: snapshot.source_pixel_height,
            fps,
            kbps: None,
        })
    }

    fn drop_pct(&self, key: &str) -> Option<f64> {
        self.last_drop_pct.get(key).map(|(pct, _)| *pct)
    }

    /// Latest closed drop window with its sequence number, for consumers
    /// that must distinguish a fresh window from a stale re-read (#882
    /// review; see `enqueue_backoff_decide`).
    fn drop_sample(&self, key: &str) -> Option<(f64, u64)> {
        self.last_drop_pct.get(key).copied()
    }
}

/// A sustained >30% display-enqueue drop rate re-warns at most this often
/// per track, so a long storm still shows a curve in petal.log instead of
/// a single first-crossing value (#878).
const DISPLAY_DROP_REWARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Pure decision: should a still-degraded (>30%) track warn again right
/// now, given when it last warned this episode? `None` means "no warning
/// recorded for this episode" (a fresh crossing, or a track that recovered
/// and had its entry cleared) -- always allow in that case (#878).
fn display_drop_rewarn_allowed(
    last_warned: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match last_warned {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= DISPLAY_DROP_REWARN_INTERVAL,
    }
}

/// #884: cadence of the in-room memory-curve log line, in ~1s poll ticks.
const MEMORY_LOG_EVERY_N_TICKS: u32 = 60;

/// #884: one line per minute while in a room -- own phys footprint plus the
/// OS memory-pressure level -- so a field log carries a memory CURVE.
fn log_in_room_memory_curve() {
    let footprint_mb = crate::platform::mem::process_footprint_bytes_throttled()
        .map(|bytes| format!("{:.0}", bytes as f64 / (1024.0 * 1024.0)))
        .unwrap_or_else(|| "unknown".into());
    let pressure = crate::platform::mem::memory_pressure_level()
        .map(|level| level.to_string())
        .unwrap_or_else(|| "unknown".into());
    log::info!(
        "diagnostics: memory curve -- phys_footprint_mb={footprint_mb} os_pressure_level={pressure}"
    );
}

/// #884: pure decision over one pressure reading -- report only genuine
/// upward TRANSITIONS into warn (2) or critical (4), never steady state and
/// never downward recovery (the log line above carries those). `None`
/// previous means first reading: report only if already elevated, so a
/// meeting joined mid-pressure still records it.
fn memory_pressure_transition(previous: Option<u32>, current: u32) -> Option<u32> {
    let elevated = |level: u32| level >= 2;
    match previous {
        None => elevated(current).then_some(current),
        Some(previous) => (elevated(current) && current > previous).then_some(current),
    }
}

fn observe_memory_pressure_transition(last_level: &mut Option<u32>) {
    let Some(current) = crate::platform::mem::memory_pressure_level() else {
        return;
    };
    if let Some(transitioned_to) = memory_pressure_transition(*last_level, current) {
        let tag = if transitioned_to >= 4 {
            crate::logging::PressureLevelTag::Critical
        } else {
            crate::logging::PressureLevelTag::Warn
        };
        log::warn!(
            "diagnostics: OS memory pressure transitioned to level {transitioned_to} \
             ({:?}) -- see #878's leak->pressure->teardown chain (#884)",
            tag
        );
        crate::logging::capture_sentry_diagnostic(
            crate::logging::SentryDiagnosticEvent::MemoryPressure(
                crate::logging::MemoryPressureDiagnostic { level: tag },
            ),
        );
    }
    *last_level = Some(current);
}

/// Sustained rejection at the display layer means continuing to enqueue is
/// mostly wasted work adding to window-server load; conservative on purpose
/// (#878 Phase 2 item 2) -- this is Petal's contribution to reducing that
/// load, not a general defense, so both thresholds favor staying enqueued
/// unless the rejection is severe and repeated.
const ENQUEUE_BACKOFF_PAUSE_DROP_PCT: f64 = 80.0;
const ENQUEUE_BACKOFF_PAUSE_CONSECUTIVE: u32 = 3;
/// A backoff pause is TIME-BOXED, not recovery-gated. While enqueue is
/// paused every 5s drop window reads ~100% by construction (`push_frame`
/// still counts `frames_received` but enqueues nothing), so a "resume when
/// the rate recovers" condition can never observe recovery -- the original
/// <=30% resume branch was unreachable and every pause silently ran a 30s
/// failsafe (review of #882, same self-measurement class as the sleep-gate
/// finding it shipped with). Fixed duration + fresh post-resume evidence is
/// the only sound scheme without probe enqueues.
const ENQUEUE_BACKOFF_PAUSE_DURATION: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueBackoffAction {
    None,
    Pause,
    Resume,
}

/// Per-track state for the enqueue-backoff decision. Lives per receive
/// track (keyed the same as the rest of `DiagInner`'s per-track maps) so one
/// noisy track's streak can't trip or reset another's.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EnqueueBackoffTrackState {
    consecutive_high: u32,
    paused: bool,
    paused_since: Option<std::time::Instant>,
    /// Sequence number of the last 5s drop window this track's streak
    /// consumed. The stats poller ticks ~1s but `DisplayPipelineSampler`
    /// closes a window every 5s -- counting every TICK let three re-reads
    /// of one stale window trip the "3 consecutive samples" threshold in
    /// ~3s instead of the intended 15s (review of #882). Only a window seq
    /// this state has not seen yet counts as evidence.
    last_window_seq: Option<u64>,
    /// Fresh windows to discard before counting evidence again. Set to 1 on
    /// every resume: the window in flight when enqueue resumes spans paused
    /// time and reads artificially high -- it is the pause's own residue,
    /// not new distress.
    skip_windows: u32,
}

/// Pure decision over one 5s drop-rate window, mutating the caller-owned
/// per-track state and returning what (if anything) the compositor gate
/// should do. `window_seq` identifies the sampler window `drop_pct` came
/// from; a seq already consumed contributes nothing (see
/// `EnqueueBackoffTrackState::last_window_seq`). Isolated from the
/// compositor call so the threshold/streak/duration logic is unit-testable
/// without any real display layer (#878).
fn enqueue_backoff_decide(
    state: &mut EnqueueBackoffTrackState,
    drop_pct: f64,
    window_seq: u64,
    now: std::time::Instant,
) -> EnqueueBackoffAction {
    if state.paused {
        // Checked every tick (not only on fresh windows) so the resume
        // lands promptly at expiry. Drop evidence is unreadable while
        // paused -- see ENQUEUE_BACKOFF_PAUSE_DURATION's doc comment.
        let expired = state.paused_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= ENQUEUE_BACKOFF_PAUSE_DURATION
        });
        if expired {
            state.paused = false;
            state.paused_since = None;
            state.consecutive_high = 0;
            state.skip_windows = 1;
            return EnqueueBackoffAction::Resume;
        }
        return EnqueueBackoffAction::None;
    }

    if state.last_window_seq == Some(window_seq) {
        // Same 5s window as last tick -- no new evidence either way.
        return EnqueueBackoffAction::None;
    }
    state.last_window_seq = Some(window_seq);

    if state.skip_windows > 0 {
        state.skip_windows -= 1;
        return EnqueueBackoffAction::None;
    }

    if drop_pct >= ENQUEUE_BACKOFF_PAUSE_DROP_PCT {
        state.consecutive_high = state.consecutive_high.saturating_add(1);
        if state.consecutive_high >= ENQUEUE_BACKOFF_PAUSE_CONSECUTIVE {
            state.paused = true;
            state.paused_since = Some(now);
            state.consecutive_high = 0;
            return EnqueueBackoffAction::Pause;
        }
    } else {
        state.consecutive_high = 0;
    }
    EnqueueBackoffAction::None
}

/// Artifact-aware wrapper around `enqueue_backoff_decide` (#878
/// adversarial-review finding 1, extended by the #882 review): while EITHER
/// gate is pausing enqueue -- the #259/#264 sleep gate, or another track's
/// backoff pause (the flag is global, so every track's window reads ~100%
/// while any track holds it) -- drop percentages are the pause's own
/// artifact and no evidence may accumulate toward a NEW pause. A track that
/// is itself paused still runs its expiry check. The wake handler
/// (`compositor::set_display_enqueue_paused(false)`) clears the global flag
/// itself.
fn apply_enqueue_backoff(
    state: &mut EnqueueBackoffTrackState,
    drop_pct: f64,
    window_seq: u64,
    now: std::time::Instant,
    sleep_paused: bool,
    global_backoff_paused: bool,
) -> EnqueueBackoffAction {
    if sleep_paused {
        *state = EnqueueBackoffTrackState::default();
        return EnqueueBackoffAction::None;
    }
    if global_backoff_paused && !state.paused {
        // Another track's pause is poisoning this track's metric: consume
        // the window without counting it, drop any streak built beforehand,
        // and discard the first window after the global resume too (it
        // spans paused time).
        state.last_window_seq = Some(window_seq);
        state.consecutive_high = 0;
        state.skip_windows = 1;
        return EnqueueBackoffAction::None;
    }
    enqueue_backoff_decide(state, drop_pct, window_seq, now)
}

/// Per-participant server-computed connection quality
/// (`RoomEvent::ConnectionQualityChanged`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantQuality {
    pub identity: String,
    /// "excellent" | "good" | "poor" | "lost"
    pub quality: String,
}

/// Plain-language cockpit analysis finding. Produced by a pure rule engine
/// over the same metric ring buffer and per-track health table the cockpit
/// already renders. No extra native dependency, no hidden guesses: each
/// finding carries the concrete evidence that triggered it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisFinding {
    /// "info" | "warn"
    pub severity: String,
    pub title: String,
    pub evidence: String,
    pub recommendation: String,
}

/// One timestamped journal entry. `category` is one of the filter-chip
/// buckets the cockpit groups by: "connection" | "presence" | "shares" |
/// "media" | "error".
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub t_ms: u64,
    pub category: String,
    pub message: String,
}

/// Everything the cockpit renders, in one payload: current connection info,
/// per-participant quality, the metric history ring buffer (newest last),
/// and the live per-track health table. Also the `network-stats` event
/// payload while the cockpit is open.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSnapshot {
    pub connected: bool,
    pub room_name: Option<String>,
    /// Host portion of the LiveKit server URL (never the full URL with
    /// credentials-adjacent query strings; host only).
    pub server_host: Option<String>,
    pub local_identity: Option<String>,
    /// Full reconnect cycles observed this connection
    /// (`RoomEvent::Reconnecting` count).
    pub reconnect_count: u32,
    pub quality: Vec<ParticipantQuality>,
    /// Latest measured peer-to-peer data-channel round-trip time in ms.
    pub peer_rtt_ms: Option<f64>,
    pub history: Vec<StatsSample>,
    pub tracks: Vec<TrackHealth>,
    pub native_startup: Vec<NativeStartupTimelineReport>,
    pub analysis: Vec<AnalysisFinding>,
}

#[derive(Default)]
struct DiagInner {
    connected: bool,
    room_name: Option<String>,
    server_host: Option<String>,
    local_identity: Option<String>,
    reconnect_count: u32,
    quality: Vec<ParticipantQuality>,
    peer_rtt_ms: Option<f64>,
    history: VecDeque<StatsSample>,
    tracks: Vec<TrackHealth>,
    latency: HashMap<String, LatencyObservation>,
    peer_clock_offsets: HashMap<String, ClockOffsetObservation>,
    latency_clock_sync_needed: HashSet<String>,
    stream_states: HashMap<String, StreamStateObservation>,
    capture_pipeline: CapturePipelineSampler,
    display_pipeline: DisplayPipelineSampler,
    software_decode_fallbacks: HashMap<String, u64>,
    /// Last time each track's >30% display-enqueue-drop warning fired.
    /// Absent (or a recovered track that was removed) means the next
    /// crossing warns immediately; present means it re-warns only after
    /// `DISPLAY_DROP_REWARN_INTERVAL` so a sustained storm still shows a
    /// curve in petal.log instead of a single first-crossing value (#878).
    display_drop_last_warned: HashMap<String, std::time::Instant>,
    /// Per-track enqueue-backoff state (#878 Phase 2 item 2).
    enqueue_backoff: HashMap<String, EnqueueBackoffTrackState>,
    receiver_freeze: HashMap<String, ReceiverFreezeObservation>,
    remote_pipeline: HashMap<RemotePipelineStageKey, PipelineStageReport>,
    remote_capture_states: HashMap<RemoteWindowReportKey, RemoteCaptureStateReport>,
    remote_receiver_freezes: HashMap<RemoteWindowReportKey, RemoteReceiverFreezeReport>,
    remote_pipeline_sequences: HashMap<RemoteShareEpochKey, RemoteSequenceReport>,
    remote_pipeline_lifecycles: HashMap<RemoteShareEpochKey, RemoteLifecycleReport>,
    remote_pipeline_terminal: HashMap<RemoteShareEpochKey, u64>,
    canonical_owner_epochs: HashMap<OwnerPublicationKey, String>,
    canonical_owner_epoch_seq: HashMap<OwnerPublicationKey, u64>,
    native_startup: HashMap<u32, NativeStartupTimeline>,
    journal: VecDeque<JournalEntry>,
}

const NATIVE_STARTUP_MAX_STAGES: usize = 64;

#[derive(Debug, Clone)]
struct NativeStartupTimeline {
    window_id: u32,
    started_at_ms: u64,
    started_seq: Option<u64>,
    restart_generation: Option<u64>,
    capture_path: String,
    requested_fps: Option<u32>,
    requested_resolution: Option<String>,
    publication_sid: Option<String>,
    outcome: NativeStartupOutcome,
    stages: Vec<NativeStartupStage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeStartupOutcome {
    InProgress,
    Published,
    PublishFailed,
    CaptureFailed,
}

impl NativeStartupOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in-progress",
            Self::Published => "published",
            Self::PublishFailed => "publish-failed",
            Self::CaptureFailed => "capture-failed",
        }
    }
}

impl NativeStartupTimeline {
    fn new(window_id: u32, started_at_ms: u64, capture_path: impl Into<String>) -> Self {
        Self {
            window_id,
            started_at_ms,
            started_seq: None,
            restart_generation: None,
            capture_path: capture_path.into(),
            requested_fps: None,
            requested_resolution: None,
            publication_sid: None,
            outcome: NativeStartupOutcome::InProgress,
            stages: Vec::new(),
        }
    }

    fn push_stage(
        &mut self,
        stage: NativeStartupStageKind,
        now_ms: u64,
        width: Option<u32>,
        height: Option<u32>,
        detail: Option<String>,
    ) {
        if self.stages.len() >= NATIVE_STARTUP_MAX_STAGES {
            let remove_index =
                if self.stages.first().is_some_and(|existing| {
                    existing.stage == NativeStartupStageKind::StartRequested
                }) && self.stages.len() > 1
                {
                    1
                } else {
                    0
                };
            self.stages.remove(remove_index);
        }
        self.stages.push(NativeStartupStage {
            stage,
            elapsed_ms: now_ms.saturating_sub(self.started_at_ms),
            width,
            height,
            fps: self.requested_fps,
            resolution: self.requested_resolution.clone(),
            capture_path: Some(self.capture_path.clone()),
            detail,
        });
    }

    fn report(&self) -> NativeStartupTimelineReport {
        NativeStartupTimelineReport {
            window_id: self.window_id,
            started_seq: self.started_seq,
            restart_generation: self.restart_generation,
            capture_path: self.capture_path.clone(),
            requested_fps: self.requested_fps,
            requested_resolution: self.requested_resolution.clone(),
            publication_sid: self.publication_sid.clone(),
            outcome: self.outcome.as_str().to_string(),
            stages: self.stages.clone(),
        }
    }
}

/// App-wide diagnostics state (Tauri-managed in `lib.rs`). Cheap-to-clone
/// `Arc` wrapper so the per-connection background tasks can hold it without
/// re-resolving managed state every tick.
#[derive(Default, Clone)]
pub struct DiagnosticsState {
    shared: Arc<DiagShared>,
}

#[derive(Default)]
struct DiagShared {
    inner: Mutex<DiagInner>,
    /// Push-gate for the ~1s `network-stats` event: only emitted while the
    /// cockpit panel is actually open (see module doc comment).
    cockpit_open: AtomicBool,
    /// Connection generation counter -- bumped by every `start_for_room` and
    /// by the event loop on `RoomEvent::Disconnected`, checked by the poller
    /// every tick so stale tasks provably stop (see module doc comment).
    generation: AtomicU64,
    /// One-shot release-readiness warning for #231. The per-track boolean is
    /// still live in `TrackHealth`; this only prevents log/journal spam.
    software_encoder_reported: AtomicBool,
}

fn clear_remote_pipeline_health(inner: &mut DiagInner) {
    inner.remote_pipeline.clear();
    inner.remote_capture_states.clear();
    inner.remote_receiver_freezes.clear();
    inner.remote_pipeline_sequences.clear();
    inner.remote_pipeline_lifecycles.clear();
    inner.remote_pipeline_terminal.clear();
    inner.canonical_owner_epochs.clear();
    inner.canonical_owner_epoch_seq.clear();
}

fn bounded_detail(value: impl Into<String>) -> Option<String> {
    let mut value = value.into();
    if value.trim().is_empty() {
        return None;
    }
    if value.len() > 160 {
        let mut byte_len = 160;
        while byte_len > 0 && !value.is_char_boundary(byte_len) {
            byte_len -= 1;
        }
        value.truncate(byte_len);
    }
    Some(value)
}

impl DiagnosticsState {
    fn lock(&self) -> std::sync::MutexGuard<'_, DiagInner> {
        self.shared.inner.lock_unpoisoned()
    }

    pub(crate) fn clock_calibration_evidence<'a>(
        &self,
        peer_identities: impl IntoIterator<Item = &'a str>,
    ) -> Vec<ClockCalibrationEvidence> {
        self.clock_calibration_evidence_at(peer_identities, now_ms())
    }

    fn clock_calibration_evidence_at<'a>(
        &self,
        peer_identities: impl IntoIterator<Item = &'a str>,
        now: u64,
    ) -> Vec<ClockCalibrationEvidence> {
        let inner = self.lock();
        peer_identities
            .into_iter()
            .map(|identity| {
                let observation = inner.peer_clock_offsets.get(identity);
                let age_ms = observation.map(|value| now.saturating_sub(value.t_ms));
                let fresh =
                    observation.filter(|_| age_ms.is_some_and(|age| age <= CLOCK_OFFSET_STALE_MS));
                ClockCalibrationEvidence {
                    calibrated: fresh.is_some(),
                    // NTP-style offset error is bounded by half the measured RTT.
                    uncertainty_ms: fresh.map(|value| value.rtt_ms / 2.0),
                    age_ms,
                }
            })
            .collect()
    }

    pub(crate) fn record_canonical_owner_epoch(
        &self,
        owner_identity: &str,
        window_id: u32,
        publication_sid: Option<&str>,
        epoch: &str,
        seq: u64,
    ) {
        let Some(publication_sid) = publication_sid.filter(|sid| !sid.trim().is_empty()) else {
            return;
        };
        if owner_identity.trim().is_empty() || window_id == 0 || epoch.is_empty() {
            return;
        }
        let key = OwnerPublicationKey {
            owner_identity: owner_identity.to_string(),
            window_id,
            publication_sid: publication_sid.to_string(),
        };
        let mut inner = self.lock();
        if inner
            .canonical_owner_epoch_seq
            .get(&key)
            .is_some_and(|current| *current >= seq)
        {
            return;
        }
        inner
            .canonical_owner_epochs
            .insert(key.clone(), epoch.to_string());
        inner.canonical_owner_epoch_seq.insert(key, seq);
        // A receiver can subscribe before the owner's first sender report.
        // Preserve those useful lifecycle facts by upgrading their SID-only
        // provisional key as soon as trusted owner evidence establishes epoch.
        let provisional = format!("provisional:{publication_sid}");
        let upgrades = inner
            .remote_pipeline_lifecycles
            .iter()
            .filter(|(existing, _)| {
                existing.owner_identity == owner_identity
                    && existing.window_id == window_id
                    && existing.publication_sid.as_deref() == Some(publication_sid)
                    && existing.share_epoch == provisional
            })
            .map(|(existing, report)| (existing.clone(), report.clone()))
            .collect::<Vec<_>>();
        for (old_key, report) in upgrades {
            inner.remote_pipeline_lifecycles.remove(&old_key);
            inner.remote_pipeline_lifecycles.insert(
                RemoteShareEpochKey {
                    share_epoch: epoch.to_string(),
                    ..old_key
                },
                report,
            );
        }
    }

    pub(crate) fn canonical_or_provisional_epoch(
        &self,
        owner_identity: &str,
        window_id: u32,
        publication_sid: Option<&str>,
        declared_epoch: &str,
    ) -> String {
        if !declared_epoch.is_empty() {
            return declared_epoch.to_string();
        }
        if let Some(sid) = publication_sid.filter(|sid| !sid.trim().is_empty()) {
            if let Some(epoch) = self
                .lock()
                .canonical_owner_epochs
                .get(&OwnerPublicationKey {
                    owner_identity: owner_identity.to_string(),
                    window_id,
                    publication_sid: sid.to_string(),
                })
                .cloned()
            {
                return epoch;
            }
            return format!("provisional:{sid}");
        }
        "provisional:legacy".to_string()
    }

    pub fn snapshot(&self) -> NetworkSnapshot {
        let inner = self.lock();
        let history: Vec<StatsSample> = inner.history.iter().cloned().collect();
        let tracks = inner.tracks.clone();
        let native_startup = inner
            .native_startup
            .values()
            .map(NativeStartupTimeline::report)
            .collect();
        let analysis = analyze_conditions(&history, &tracks, inner.reconnect_count, &inner.quality);
        NetworkSnapshot {
            connected: inner.connected,
            room_name: inner.room_name.clone(),
            server_host: inner.server_host.clone(),
            local_identity: inner.local_identity.clone(),
            reconnect_count: inner.reconnect_count,
            quality: inner.quality.clone(),
            peer_rtt_ms: inner.peer_rtt_ms,
            history,
            tracks,
            native_startup,
            analysis,
        }
    }

    pub fn journal(&self) -> Vec<JournalEntry> {
        self.lock().journal.iter().cloned().collect()
    }

    fn record_latency(&self, key: String, latency_ms: f64, frame_id: Option<u32>) -> bool {
        let mut inner = self.lock();
        let first = !inner.latency.contains_key(&key);
        inner.latency.insert(
            key,
            LatencyObservation {
                latency_ms,
                frame_id,
                t_ms: now_ms(),
            },
        );
        first
    }

    fn record_uncalibrated_latency_skip(&self, key: String) -> bool {
        let mut inner = self.lock();
        inner.latency_clock_sync_needed.insert(key)
    }

    fn corrected_glass_to_glass_ms(
        &self,
        owner_identity: &str,
        capture_timestamp_us: u64,
        receive_timestamp_us: u64,
    ) -> Option<f64> {
        let now = now_ms();
        let offset_us = {
            let mut inner = self.lock();
            inner
                .peer_clock_offsets
                .retain(|_, obs| now.saturating_sub(obs.t_ms) <= CLOCK_OFFSET_STALE_MS);
            inner
                .peer_clock_offsets
                .get(owner_identity)
                .map(|obs| obs.sender_to_receiver_offset_us)
        }?;
        corrected_latency_ms(capture_timestamp_us, receive_timestamp_us, offset_us)
    }

    pub fn record_peer_rtt(&self, rtt_ms: f64) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        let mut inner = self.lock();
        inner.peer_rtt_ms = Some(rtt_ms);
    }

    pub fn record_peer_clock_offset(
        &self,
        peer_identity: String,
        sender_to_receiver_offset_us: i64,
        rtt_ms: f64,
    ) {
        if peer_identity.trim().is_empty() || !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        let now = now_ms();
        let mut inner = self.lock();
        inner.peer_rtt_ms = Some(rtt_ms);
        let should_replace = inner
            .peer_clock_offsets
            .get(&peer_identity)
            .map(|existing| {
                now.saturating_sub(existing.t_ms) > CLOCK_OFFSET_STALE_MS
                    || rtt_ms <= existing.rtt_ms
            })
            .unwrap_or(true);
        if should_replace {
            inner.peer_clock_offsets.insert(
                peer_identity,
                ClockOffsetObservation {
                    sender_to_receiver_offset_us,
                    rtt_ms,
                    t_ms: now,
                },
            );
        }
    }

    pub fn is_cockpit_open(&self) -> bool {
        self.shared.cockpit_open.load(Ordering::SeqCst)
    }

    pub(crate) fn record_capture_frame(
        &self,
        window_id: u32,
        sequence: u64,
        width: u32,
        height: u32,
        state: CaptureStateReport,
    ) {
        let mut inner = self.lock();
        inner
            .capture_pipeline
            .record_frame(window_id, sequence, width, height, state);
    }

    pub(crate) fn record_capture_push_timing(
        &self,
        window_id: u32,
        convert_ms: f64,
        capture_frame_return_ms: f64,
    ) {
        let mut inner = self.lock();
        inner
            .capture_pipeline
            .record_push_timing(window_id, convert_ms, capture_frame_return_ms);
    }

    pub(crate) fn mark_capture_wedged(&self, window_id: u32) {
        let mut inner = self.lock();
        inner.capture_pipeline.mark_wedged(window_id);
    }

    pub(crate) fn reset_capture_pipeline(&self, window_id: u32) {
        let mut inner = self.lock();
        inner.capture_pipeline.clear_window(window_id);
        inner.native_startup.remove(&window_id);
    }

    pub(crate) fn clear_native_startup(&self, window_id: u32) {
        self.lock().native_startup.remove(&window_id);
    }

    pub(crate) fn begin_native_startup(
        &self,
        window_id: u32,
        capture_path: impl Into<String>,
        requested_fps: Option<u32>,
        requested_resolution: Option<String>,
    ) {
        let now = now_ms();
        let mut timeline = NativeStartupTimeline::new(window_id, now, capture_path);
        timeline.requested_fps = requested_fps;
        timeline.requested_resolution = requested_resolution;
        timeline.push_stage(
            NativeStartupStageKind::StartRequested,
            now,
            None,
            None,
            None,
        );
        self.lock().native_startup.insert(window_id, timeline);
    }

    pub(crate) fn set_native_startup_correlation(
        &self,
        window_id: u32,
        started_seq: u64,
        restart_generation: u64,
    ) {
        let mut inner = self.lock();
        let timeline = inner
            .native_startup
            .entry(window_id)
            .or_insert_with(|| NativeStartupTimeline::new(window_id, now_ms(), "unknown"));
        timeline.started_seq = Some(started_seq);
        timeline.restart_generation = Some(restart_generation);
    }

    pub(crate) fn record_native_startup_stage(
        &self,
        window_id: u32,
        stage: NativeStartupStageKind,
        width: Option<u32>,
        height: Option<u32>,
        detail: Option<String>,
    ) {
        let now = now_ms();
        let mut inner = self.lock();
        let timeline = inner
            .native_startup
            .entry(window_id)
            .or_insert_with(|| NativeStartupTimeline::new(window_id, now, "unknown"));
        match stage {
            NativeStartupStageKind::PublishSucceeded => {
                timeline.outcome = NativeStartupOutcome::Published
            }
            NativeStartupStageKind::PublishFailed => {
                timeline.outcome = NativeStartupOutcome::PublishFailed
            }
            NativeStartupStageKind::FirstFrameTimeout => {
                timeline.outcome = NativeStartupOutcome::CaptureFailed
            }
            _ => {}
        }
        timeline.push_stage(stage, now, width, height, detail.and_then(bounded_detail));
    }

    pub(crate) fn record_native_startup_publication(
        &self,
        window_id: u32,
        publication_sid: Option<String>,
    ) {
        let mut inner = self.lock();
        let timeline = inner
            .native_startup
            .entry(window_id)
            .or_insert_with(|| NativeStartupTimeline::new(window_id, now_ms(), "unknown"));
        timeline.publication_sid = publication_sid.filter(|sid| !sid.trim().is_empty());
    }

    pub(crate) fn record_receiver_frame(&self, key: String, frame_id: Option<u32>) {
        let mut inner = self.lock();
        let entry = inner.receiver_freeze.entry(key).or_default();
        if let Some(current) = frame_id {
            match entry.last_frame_id {
                Some(previous) if current > previous.saturating_add(1) => {
                    entry.freeze_count = entry.freeze_count.saturating_add(1);
                    entry.last_frame_id = Some(current);
                }
                Some(previous) if current <= previous => {}
                _ => {
                    entry.last_frame_id = Some(current);
                }
            }
        }
    }

    /// The authoritative reducer for cross-peer pipeline observations. Receipt
    /// order (not cross-machine wall clocks) decides conflicts. A terminal
    /// epoch remains tombstoned for the bounded diagnostic TTL so late packets
    /// cannot resurrect an already-cleared share.
    pub(crate) fn accept_remote_pipeline_observation(
        &self,
        owner_identity: &str,
        window_id: u32,
        reporter_id: &str,
        publication_sid: Option<&str>,
        share_epoch: &str,
        seq: u64,
    ) -> bool {
        if owner_identity.trim().is_empty() || reporter_id.trim().is_empty() || window_id == 0 {
            return false;
        }
        let now = now_ms();
        let key = RemoteShareEpochKey {
            owner_identity: owner_identity.to_string(),
            window_id,
            reporter_id: reporter_id.to_string(),
            publication_sid: publication_sid.map(str::to_string),
            share_epoch: share_epoch.to_string(),
        };
        let mut inner = self.lock();
        inner.remote_pipeline_sequences.retain(|_, value| {
            now.saturating_sub(value.received_at_ms) <= REMOTE_PIPELINE_STALE_MS
        });
        inner.remote_pipeline_terminal.retain(|_, received_at_ms| {
            now.saturating_sub(*received_at_ms) <= REMOTE_PIPELINE_STALE_MS
        });
        if inner.remote_pipeline_terminal.contains_key(&key) {
            return false;
        }
        if inner
            .remote_pipeline_sequences
            .get(&key)
            .is_some_and(|previous| seq <= previous.seq)
        {
            return false;
        }
        inner.remote_pipeline_sequences.insert(
            key,
            RemoteSequenceReport {
                seq,
                received_at_ms: now,
            },
        );
        true
    }

    pub(crate) fn record_remote_pipeline_lifecycle(
        &self,
        owner_identity: String,
        window_id: u32,
        reporter_id: String,
        share_epoch: String,
        publication_sid: Option<String>,
        lifecycle: PipelineLifecycle,
        _seq: u64,
    ) {
        let now = now_ms();
        let key = RemoteShareEpochKey {
            owner_identity: owner_identity.clone(),
            window_id,
            reporter_id: reporter_id.clone(),
            publication_sid: publication_sid.clone(),
            share_epoch,
        };
        let mut inner = self.lock();
        let terminal = matches!(
            lifecycle,
            PipelineLifecycle::Unpublished | PipelineLifecycle::TerminalFailure
        );
        if terminal {
            // Never let a late terminal event clear a successor: only an
            // exact SID+epoch identity may remove its own observations.
            if let Some(publication_sid) = publication_sid.as_deref() {
                let epoch = &key.share_epoch;
                inner.remote_pipeline.retain(|existing, _| {
                    !(existing.owner_identity == owner_identity
                        && existing.window_id == window_id
                        && existing.reporter_id == reporter_id
                        && existing.publication_sid.as_deref() == Some(publication_sid)
                        && &existing.share_epoch == epoch)
                });
                inner.remote_capture_states.retain(|existing, _| {
                    !(existing.owner_identity == owner_identity
                        && existing.window_id == window_id
                        && existing.reporter_id == reporter_id
                        && existing.publication_sid.as_deref() == Some(publication_sid)
                        && &existing.share_epoch == epoch)
                });
                inner.remote_receiver_freezes.retain(|existing, _| {
                    !(existing.owner_identity == owner_identity
                        && existing.window_id == window_id
                        && existing.reporter_id == reporter_id
                        && existing.publication_sid.as_deref() == Some(publication_sid)
                        && &existing.share_epoch == epoch)
                });
            }
            if inner.remote_pipeline_terminal.len() >= REMOTE_PIPELINE_LIFECYCLE_CAP {
                if let Some(oldest) = inner
                    .remote_pipeline_terminal
                    .iter()
                    .min_by_key(|(_, received_at_ms)| *received_at_ms)
                    .map(|(key, _)| key.clone())
                {
                    inner.remote_pipeline_terminal.remove(&oldest);
                }
            }
            inner.remote_pipeline_terminal.insert(key.clone(), now);
        }
        if inner.remote_pipeline_lifecycles.len() >= REMOTE_PIPELINE_LIFECYCLE_CAP
            && !inner.remote_pipeline_lifecycles.contains_key(&key)
        {
            if let Some(oldest) = inner
                .remote_pipeline_lifecycles
                .iter()
                .min_by_key(|(_, report)| report.received_at_ms)
                .map(|(key, _)| key.clone())
            {
                inner.remote_pipeline_lifecycles.remove(&oldest);
            }
        }
        inner.remote_pipeline_lifecycles.insert(
            key,
            RemoteLifecycleReport {
                lifecycle,
                received_at_ms: now,
            },
        );
    }

    pub(crate) fn record_remote_pipeline_stage(
        &self,
        owner_identity: String,
        window_id: u32,
        reporter_id: String,
        publication_sid: Option<String>,
        share_epoch: String,
        stage: PipelineStageKind,
        metrics: PipelineStageMetrics,
        sent_at_ms: u64,
    ) {
        if owner_identity.trim().is_empty()
            || reporter_id.trim().is_empty()
            || window_id == 0
            || !pipeline_stage_has_signal(&metrics)
        {
            return;
        }
        let received_at_ms = now_ms();
        let mut inner = self.lock();
        inner.remote_pipeline.insert(
            RemotePipelineStageKey {
                owner_identity,
                window_id,
                reporter_id: reporter_id.clone(),
                stage,
                publication_sid,
                share_epoch,
            },
            PipelineStageReport {
                reporter_id,
                sent_at_ms,
                received_at_ms,
                metrics,
            },
        );
    }

    pub(crate) fn record_remote_capture_state(
        &self,
        owner_identity: String,
        window_id: u32,
        reporter_id: String,
        publication_sid: Option<String>,
        share_epoch: String,
        state: CaptureStateReport,
        sent_at_ms: u64,
    ) {
        if owner_identity.trim().is_empty()
            || reporter_id.trim().is_empty()
            || window_id == 0
            || reporter_id != owner_identity
        {
            return;
        }
        let received_at_ms = now_ms();
        let mut inner = self.lock();
        inner.remote_capture_states.insert(
            RemoteWindowReportKey {
                owner_identity,
                window_id,
                reporter_id: reporter_id.clone(),
                publication_sid,
                share_epoch,
            },
            RemoteCaptureStateReport {
                reporter_id,
                sent_at_ms,
                received_at_ms,
                state,
            },
        );
    }

    pub(crate) fn record_remote_receiver_freeze(
        &self,
        owner_identity: String,
        window_id: u32,
        reporter_id: String,
        publication_sid: Option<String>,
        share_epoch: String,
        metrics: ReceiverFreezeMetrics,
        sent_at_ms: u64,
    ) {
        if owner_identity.trim().is_empty() || reporter_id.trim().is_empty() || window_id == 0 {
            return;
        }
        let received_at_ms = now_ms();
        let mut inner = self.lock();
        inner.remote_receiver_freezes.insert(
            RemoteWindowReportKey {
                owner_identity,
                window_id,
                reporter_id: reporter_id.clone(),
                publication_sid,
                share_epoch,
            },
            RemoteReceiverFreezeReport {
                reporter_id,
                sent_at_ms,
                received_at_ms,
                metrics,
            },
        );
    }

    fn apply_remote_pipeline_overlays(&self, tracks: &mut [TrackHealth]) {
        let now = now_ms();
        let (
            local_identity,
            reports,
            capture_states,
            receiver_freezes,
            lifecycles,
            canonical_epochs,
        ) = {
            let mut inner = self.lock();
            inner.remote_pipeline.retain(|_, report| {
                now.saturating_sub(report.received_at_ms) <= REMOTE_PIPELINE_STALE_MS
            });
            inner.remote_capture_states.retain(|_, report| {
                now.saturating_sub(report.received_at_ms) <= REMOTE_PIPELINE_STALE_MS
            });
            inner.remote_receiver_freezes.retain(|_, report| {
                now.saturating_sub(report.received_at_ms) <= REMOTE_PIPELINE_STALE_MS
            });
            inner.remote_pipeline_lifecycles.retain(|_, report| {
                now.saturating_sub(report.received_at_ms) <= REMOTE_PIPELINE_STALE_MS
            });
            (
                inner.local_identity.clone().unwrap_or_default(),
                inner.remote_pipeline.clone(),
                inner.remote_capture_states.clone(),
                inner.remote_receiver_freezes.clone(),
                inner.remote_pipeline_lifecycles.clone(),
                inner.canonical_owner_epochs.clone(),
            )
        };
        if local_identity.is_empty()
            || (reports.is_empty()
                && capture_states.is_empty()
                && receiver_freezes.is_empty()
                && lifecycles.is_empty())
        {
            return;
        }

        for track in tracks.iter_mut().filter(|track| track.kind == "video") {
            let Some(window_id) = track.window_id else {
                continue;
            };

            match track.direction.as_str() {
                "send" => {
                    track.remote_received = latest_remote_pipeline_report(
                        &reports,
                        &local_identity,
                        window_id,
                        &track.sid,
                        PipelineStageKind::Received,
                        |reporter| reporter != local_identity,
                    );
                    track.remote_decoded = latest_remote_pipeline_report(
                        &reports,
                        &local_identity,
                        window_id,
                        &track.sid,
                        PipelineStageKind::Decoded,
                        |reporter| reporter != local_identity,
                    );
                    track.remote_receiver_freeze = latest_remote_receiver_freeze_report(
                        &receiver_freezes,
                        &local_identity,
                        window_id,
                        &track.sid,
                        |reporter| reporter != local_identity,
                    );
                    track.remote_lifecycle = latest_remote_lifecycle_report(
                        &lifecycles,
                        &local_identity,
                        window_id,
                        &track.sid,
                        canonical_epochs
                            .get(&OwnerPublicationKey {
                                owner_identity: local_identity.clone(),
                                window_id,
                                publication_sid: track.sid.clone(),
                            })
                            .map(String::as_str),
                        |reporter| reporter != local_identity,
                    );
                }
                "recv" => {
                    let Some(owner_identity) = track.owner_identity.as_deref() else {
                        continue;
                    };
                    track.remote_grabbed = latest_remote_pipeline_report(
                        &reports,
                        owner_identity,
                        window_id,
                        &track.sid,
                        PipelineStageKind::Grabbed,
                        |reporter| reporter == owner_identity,
                    );
                    track.remote_encoded_sent = latest_remote_pipeline_report(
                        &reports,
                        owner_identity,
                        window_id,
                        &track.sid,
                        PipelineStageKind::EncodedSent,
                        |reporter| reporter == owner_identity,
                    );
                    track.remote_capture_state = latest_remote_capture_state_report(
                        &capture_states,
                        owner_identity,
                        window_id,
                        &track.sid,
                        |reporter| reporter == owner_identity,
                    );
                    track.remote_lifecycle = latest_remote_lifecycle_report(
                        &lifecycles,
                        owner_identity,
                        window_id,
                        &track.sid,
                        canonical_epochs
                            .get(&OwnerPublicationKey {
                                owner_identity: owner_identity.to_string(),
                                window_id,
                                publication_sid: track.sid.clone(),
                            })
                            .map(String::as_str),
                        |reporter| reporter == owner_identity,
                    );
                }
                _ => {}
            }
        }
    }

    fn apply_track_overlays(&self, tracks: &mut [TrackHealth], rtt_ms: Option<f64>) {
        let now = now_ms();
        let (latency, stream_states, peer_clock_offsets, receiver_freeze) = {
            let mut inner = self.lock();
            inner
                .peer_clock_offsets
                .retain(|_, obs| now.saturating_sub(obs.t_ms) <= CLOCK_OFFSET_STALE_MS);
            (
                inner.latency.clone(),
                inner.stream_states.clone(),
                inner.peer_clock_offsets.clone(),
                inner.receiver_freeze.clone(),
            )
        };
        #[cfg(target_os = "macos")]
        let display_snapshots: HashMap<String, crate::compositor::DisplayEnqueueSnapshot> = tracks
            .iter()
            .filter(|track| track.direction == "recv" && track.kind == "video")
            .filter_map(|track| {
                let owner_identity = track.owner_identity.as_deref()?;
                let window_id = track.window_id?;
                crate::compositor::display_enqueue_snapshot(owner_identity, window_id)
                    .map(|snapshot| (track.latency_key.clone(), snapshot))
            })
            .collect();
        for track in tracks.iter_mut() {
            if track.direction != "recv" || track.kind != "video" {
                continue;
            }
            if let Some(stream_state) = stream_states.get(&track.latency_key) {
                let exact_livekit_state = stream_state.source == "livekit-js-stream-state";
                if exact_livekit_state || track.stream_state == "unknown" {
                    track.stream_state = stream_state.state.clone();
                }
                if track.stream_state != "active" {
                    track.quality_limitation = stream_state.source.clone();
                }
            }
            track.receiver_freeze = Some(ReceiverFreezeMetrics {
                freeze_count: receiver_freeze
                    .get(&track.latency_key)
                    .map(|obs| obs.freeze_count)
                    .unwrap_or(0),
                frames_dropped: track.frames_dropped,
                quality_limitation_reason: nonempty_string(&track.quality_limitation),
            });
            if let Some(obs) = latency
                .get(&track.latency_key)
                .filter(|obs| now.saturating_sub(obs.t_ms) <= LATENCY_STALE_MS)
            {
                track.glass_to_glass_ms = Some(obs.latency_ms);
                track.glass_to_glass_status = "calibrated".to_string();
                let _ = obs.frame_id;
                continue;
            }
            track.glass_to_glass_status = match track.owner_identity.as_deref() {
                Some(owner) if peer_clock_offsets.contains_key(owner) => "calibrated".to_string(),
                Some(_) => "clock-sync-pending".to_string(),
                None => String::new(),
            };

            let mut estimate = RENDER_PIPELINE_ESTIMATE_MS;
            let mut has_basis = false;
            if let Some(rtt) = rtt_ms {
                estimate += rtt / 2.0;
                has_basis = true;
            }
            if let Some(jitter_buffer) = track.jitter_buffer_ms {
                estimate += jitter_buffer;
                has_basis = true;
            }
            if has_basis {
                track.glass_to_glass_estimate_ms = Some(estimate);
            }
        }

        let mut inner = self.lock();
        for track in tracks
            .iter_mut()
            .filter(|track| track.direction == "send" && track.kind == "video")
        {
            let Some(window_id) = track.window_id else {
                continue;
            };
            if let Some(sample) = inner.capture_pipeline.sample_stage(window_id, now) {
                track.grabbed = Some(sample.stage);
                track.capture_state = Some(sample.state);
            }
        }
        // #878 adversarial-review finding 1: while the sleep gate holds enqueue,
        // every recv track reads ~100% drop by construction. Feeding that into
        // the backoff detector (or the drop-rate warn) manufactures distress
        // out of a sleeping display and leaves a self-sustaining pause behind
        // at wake. Sample the gate once per pass and suspend both consumers.
        #[cfg(target_os = "macos")]
        let sleep_paused = crate::compositor::display_enqueue_sleep_paused();
        #[cfg(not(target_os = "macos"))]
        let sleep_paused = false;
        // #882 review: the backoff pause has the same self-measurement
        // artifact as the sleep gate -- while ANY track holds the global
        // pause, every track's window reads ~100% drop. Sampled once per
        // pass and fed to `apply_enqueue_backoff` so no track accumulates
        // pause-poisoned evidence toward a new pause.
        #[cfg(target_os = "macos")]
        let global_backoff_paused = crate::compositor::display_enqueue_backoff_paused();
        #[cfg(not(target_os = "macos"))]
        let global_backoff_paused = false;
        for track in tracks
            .iter_mut()
            .filter(|track| track.direction == "recv" && track.kind == "video")
        {
            #[cfg(target_os = "macos")]
            if let Some(snapshot) = display_snapshots.get(&track.latency_key) {
                track.frames_received = snapshot.frames_received;
                track.frames_display_enqueued = snapshot.frames_display_enqueued;
                track.display_enqueued =
                    inner
                        .display_pipeline
                        .sample_stage(&track.latency_key, *snapshot, now);
                track.display_drop_pct = inner.display_pipeline.drop_pct(&track.latency_key);
            }
            track.software_decode_fallbacks = inner
                .software_decode_fallbacks
                .get(&track.latency_key)
                .copied()
                .unwrap_or(0);
            let drop_sample = inner.display_pipeline.drop_sample(&track.latency_key);
            if let Some((drop_pct, drop_window_seq)) = drop_sample {
                let warn_now = std::time::Instant::now();
                // The warn suspends under BOTH pauses for the same reason
                // the backoff does: a gated enqueue makes 100% drop an
                // artifact, not a report-worthy rate (#882 review).
                if drop_pct > 30.0 && !sleep_paused && !global_backoff_paused {
                    let last_warned = inner.display_drop_last_warned.get(&track.latency_key).copied();
                    if display_drop_rewarn_allowed(last_warned, warn_now) {
                        inner
                            .display_drop_last_warned
                            .insert(track.latency_key.clone(), warn_now);
                        // warn!, not error!: Sentry is for crashes, not
                        // quality rates. This still lands in petal.log; a
                        // >30% drop is a future PostHog event
                        // (POSTHOG_EVENT_ALLOWLIST.md), not an issue.
                        // Rate-limited re-report (not once-per-track) so a
                        // sustained storm still shows a curve, not just its
                        // first-crossing value (#878).
                        log::warn!(
                            "diagnostics: receiver display enqueue drop rate {:.1}% over 5s for {}",
                            drop_pct,
                            track.name
                        );
                    }
                } else {
                    // Recovered: clear so the next crossing (a NEW episode)
                    // warns immediately rather than inheriting the old
                    // episode's rate limit.
                    inner.display_drop_last_warned.remove(&track.latency_key);
                }

                let backoff_state = inner
                    .enqueue_backoff
                    .entry(track.latency_key.clone())
                    .or_default();
                match apply_enqueue_backoff(
                    backoff_state,
                    drop_pct,
                    drop_window_seq,
                    warn_now,
                    sleep_paused,
                    global_backoff_paused,
                ) {
                    EnqueueBackoffAction::Pause => {
                        log::warn!(
                            "diagnostics: {} sustained display-enqueue drop rate ({:.1}%) -- \
                             pausing display-layer enqueue for {}s (#878)",
                            track.name,
                            drop_pct,
                            ENQUEUE_BACKOFF_PAUSE_DURATION.as_secs()
                        );
                        #[cfg(target_os = "macos")]
                        crate::compositor::set_display_enqueue_backoff_paused(true);
                    }
                    EnqueueBackoffAction::Resume => {
                        log::info!(
                            "diagnostics: {} display-enqueue backoff pause expired -- resuming \
                             display-layer enqueue (#878)",
                            track.name
                        );
                        #[cfg(target_os = "macos")]
                        crate::compositor::set_display_enqueue_backoff_paused(false);
                    }
                    EnqueueBackoffAction::None => {}
                }
            }
            track.display_drop_flag = track.display_drop_pct.is_some_and(|drop| drop > 30.0);
        }
    }

    fn record_video_stream_state(
        &self,
        app: &tauri::AppHandle,
        participant_identity: String,
        track_name: String,
        state: String,
        source: String,
    ) {
        self.record_video_stream_state_by_key(
            app,
            latency_key(&participant_identity, &track_name),
            format!(
                "{} from {}",
                describe_track(&track_name),
                participant_identity
            ),
            state,
            source,
            None,
        );
    }

    /// `metrics`, when present, is appended to the file-log line only (never
    /// the cockpit journal message -- see repo rule against ever truncating
    /// UI text, and this is meant for a post-hoc `petal.log` read, not the
    /// cockpit). Only the stats-derived poller (`start_for_room`'s Task 2)
    /// currently supplies it, with decoded/displayed-stage numbers (#358);
    /// the other, authoritative callers (livekit-js `stream-state`,
    /// `record_native_video_stream_state`) pass `None`.
    fn record_video_stream_state_by_key(
        &self,
        app: &tauri::AppHandle,
        key: String,
        display_label: String,
        state: String,
        source: String,
        metrics: Option<String>,
    ) {
        let (changed, first) = {
            let mut inner = self.lock();
            let first = !inner.stream_states.contains_key(&key);
            let changed = match inner.stream_states.get(&key) {
                Some(previous) => previous.state != state,
                None => true,
            };
            inner.stream_states.insert(
                key,
                StreamStateObservation {
                    state: state.clone(),
                    source: source.clone(),
                },
            );
            (changed, first)
        };
        // First observation for an active track is normal startup, not a
        // recovery event. Pauses/stalls are still journaled immediately.
        if !changed {
            return;
        }
        if first && state == "active" {
            return;
        }

        let message = match state.as_str() {
            "active" => format!("Video resumed for {display_label}"),
            "paused" => format!("Video paused — weak connection for {display_label}"),
            _ => format!("Video possibly paused or stalled for {display_label}"),
        };
        // Also persist to the file log (petal.log), not just the in-memory
        // journal/Network Cockpit UI: a "so-and-so's video was janky"
        // report filed after the fact is otherwise undiagnosable from the
        // log alone -- this was the actual gap hit investigating a camera
        // jank report where the cockpit window hadn't been open live, and
        // it applies equally to `petal-camera-*` tracks (whose only quality
        // signal comes from the webview gallery bridge's `state=paused`
        // reports here -- the native compositor path never sees them) and
        // to native window-share stalls.
        // warn!, not error!: a stalled/paused track is a quality signal,
        // not a crash. error! opens a Sentry issue, and the stall
        // classifier flaps around a near-dead decoder (~11s) so paging
        // here drowned real failures. petal.log + Network Cockpit still
        // get the line; rates go to PostHog (`analytics.rs`, allowlist).
        let metrics_suffix = metrics.map(|m| format!(" {m}")).unwrap_or_default();
        match state.as_str() {
            "active" => log::info!("diagnostics: {message} (source={source}){metrics_suffix}"),
            _ => {
                log::warn!("diagnostics: {message} (source={source}){metrics_suffix}");
                if state == "stalled"
                    || source.contains("stats-frame-starvation")
                    || source.contains("gallery-bridge-freeze")
                    || source.contains("native-no-frame")
                {
                    crate::analytics::remote_video_stalled(&source);
                }
            }
        }
        self.journal_append(app, "media", message);
    }

    /// Append a journal entry (bounded) and push it to the main webview.
    /// Public within the crate so other seams (e.g. a future share-error
    /// hook) can journal without owning a room-event loop.
    pub(crate) fn journal_append(&self, app: &tauri::AppHandle, category: &str, message: String) {
        let entry = JournalEntry {
            t_ms: now_ms(),
            category: category.to_string(),
            message,
        };
        {
            let mut inner = self.lock();
            push_bounded(&mut inner.journal, entry.clone(), JOURNAL_CAP);
        }
        // Global emit (not emit_to("main")): the Network Cockpit is a
        // dedicated top-level window (label "network-cockpit", issue #37),
        // NOT an overlay in the main webview, so a "main"-targeted event never
        // reaches its listener. Global `emit` delivers to every top-level
        // webview (the cockpit window included); webviews without a listener
        // for this event simply ignore it. Same pattern as presence-update/
        // room-left/hover-tab-update.
        let _ = tauri::Emitter::emit(app, "journal-appended", entry);
    }
}

/// Production compositor-side glass-to-glass sampler. Native publishers and
/// browser harness publishers that enable LiveKit frame metadata carry a
/// sender timestamp on every decoded frame. We only turn that into a measured
/// latency after the data-channel probe has supplied a fresh sender/receiver
/// clock offset; raw cross-machine wall-clock subtraction is invalid (#182).
#[cfg(target_os = "macos")]
pub(crate) fn record_glass_to_glass_frame_timing(
    app: &tauri::AppHandle,
    owner_identity: &str,
    track_name: &str,
    capture_timestamp_us: u64,
    receive_timestamp_us: u64,
    frame_id: Option<u32>,
) {
    use tauri::Manager;
    let Some(state) = app.try_state::<DiagnosticsState>() else {
        return;
    };
    let state: DiagnosticsState = state.inner().clone();
    let key = latency_key(owner_identity, track_name);
    let Some(latency_ms) = state.corrected_glass_to_glass_ms(
        owner_identity,
        capture_timestamp_us,
        receive_timestamp_us,
    ) else {
        if state.record_uncalibrated_latency_skip(key) {
            state.journal_append(
                app,
                "media",
                format!(
                    "Skipped measured glass-to-glass latency for {} from {} until clock sync is calibrated",
                    describe_track(track_name),
                    owner_identity
                ),
            );
        }
        return;
    };
    let first = state.record_latency(key, latency_ms, frame_id);
    if first {
        state.journal_append(
            app,
            "media",
            format!(
                "Measured calibrated glass-to-glass latency for {} from {}",
                describe_track(track_name),
                owner_identity
            ),
        );
    }
}

pub(crate) fn record_video_stream_state_internal(
    app: &tauri::AppHandle,
    participant_identity: String,
    track_name: String,
    state: String,
    source: String,
) {
    use tauri::Manager;
    let Some(state_handle) = app.try_state::<DiagnosticsState>() else {
        return;
    };
    let diagnostics: DiagnosticsState = state_handle.inner().clone();
    diagnostics.record_video_stream_state(app, participant_identity, track_name, state, source);
}

#[cfg(target_os = "macos")]
pub(crate) fn record_native_video_stream_state(
    app: &tauri::AppHandle,
    participant_identity: &str,
    track_name: &str,
    state: &str,
    source: &str,
) {
    let normalized = match state {
        "active" | "paused" | "stalled" | "unknown" => state,
        _ => "unknown",
    };
    record_video_stream_state_internal(
        app,
        participant_identity.to_string(),
        track_name.to_string(),
        normalized.to_string(),
        source.to_string(),
    );
}

#[cfg(target_os = "macos")]
pub(crate) fn record_native_receiver_frame(
    app: &tauri::AppHandle,
    owner_identity: &str,
    track_name: &str,
    frame_id: Option<u32>,
) {
    use tauri::Manager;
    let Some(state_handle) = app.try_state::<DiagnosticsState>() else {
        return;
    };
    let diagnostics: DiagnosticsState = state_handle.inner().clone();
    diagnostics.record_receiver_frame(latency_key(owner_identity, track_name), frame_id);
}

#[cfg(target_os = "macos")]
pub(crate) fn record_native_receiver_software_fallback(
    app: &tauri::AppHandle,
    owner_identity: &str,
    track_name: &str,
    count: u64,
) {
    use tauri::Manager;
    let Some(state) = app.try_state::<DiagnosticsState>() else {
        return;
    };
    state
        .inner()
        .lock()
        .software_decode_fallbacks
        .insert(latency_key(owner_identity, track_name), count);
}

/// Exact stream-state bridge for the in-webview LiveKit client, which sees
/// SFU pause/resume updates that the pinned Rust SDK does not expose as
/// `RoomEvent`s. The Rust stats path still supplies a "stalled" fallback for
/// native compositor windows.
#[tauri::command]
pub fn record_video_stream_state(
    app: tauri::AppHandle,
    participant_identity: String,
    track_name: String,
    state: String,
    source: String,
) {
    let normalized = match state.as_str() {
        "active" | "paused" | "stalled" | "unknown" => state,
        _ => "unknown".to_string(),
    };
    record_video_stream_state_internal(&app, participant_identity, track_name, normalized, source);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn record_glass_to_glass_frame_timing(
    _app: &tauri::AppHandle,
    _owner_identity: &str,
    _track_name: &str,
    _capture_timestamp_us: u64,
    _receive_timestamp_us: u64,
    _frame_id: Option<u32>,
) {
}

/// Debounce for the stats-derived stall classification only (#358). Lives as
/// local state inside the stats-poller task (constructed alongside
/// `prev_bytes`/`prev_tick` in `start_for_room`'s Task 2), never inside
/// `DiagnosticsState` -- it must never influence or delay
/// `record_video_stream_state_by_key`'s other, authoritative callers.
/// `gate` decides, per poll tick, whether the poller should even ATTEMPT a
/// "stalled" transition against that shared sink.
#[derive(Default)]
struct StallDebounce {
    consecutive_zero_decoded_ticks: HashMap<String, u32>,
}

impl StallDebounce {
    /// `raw_state` is this tick's own stats-derived classification
    /// (`TrackHealth::stream_state`, from the `fps`/`actual_kbps` gauges).
    /// `decoder_fps` must be the `frames_decoded` counter-delta rate (the
    /// same value already computed for the decoded-stage metric) -- never
    /// libwebrtc's smoothed `fps` gauge, which can lag across polls.
    ///
    /// Returns `Some(state)` when this tick's observation should actually be
    /// attempted against `record_video_stream_state_by_key`, or `None` to
    /// suppress it entirely (still inside the debounce window). Non-stalled
    /// classifications ("active"/"muted"/"unknown") always pass through
    /// instantly and reset the counter -- there is no debounce on recovery.
    fn gate(&mut self, key: &str, raw_state: &str, decoder_fps: Option<f64>) -> Option<String> {
        if raw_state != "stalled" {
            self.consecutive_zero_decoded_ticks.remove(key);
            return Some(raw_state.to_string());
        }
        // Only a real, measured zero-progress tick on the decoded-frame
        // counter counts toward the debounce; a missing baseline (brand new
        // subscription, no previous sample yet) must not.
        let zero_progress = matches!(decoder_fps, Some(fps) if fps <= 0.0);
        if !zero_progress {
            self.consecutive_zero_decoded_ticks.remove(key);
            return None;
        }
        let count = self
            .consecutive_zero_decoded_ticks
            .entry(key.to_string())
            .or_insert(0);
        *count = count.saturating_add(1);
        if *count >= STALL_DEBOUNCE_TICKS {
            Some("stalled".to_string())
        } else {
            None
        }
    }
}

/// Start the diagnostics tasks for a freshly-joined room. Called once per
/// room connection from `session::join_room` (same seam as the presence/
/// telepointer/resilience watchers). `server_url` is the LiveKit URL the
/// connection was made to -- only its host is kept.
pub fn start_for_room(
    app: &tauri::AppHandle,
    room: Arc<livekit::Room>,
    room_display_name: String,
    server_url: String,
    local_identity: String,
) {
    use tauri::Manager;
    let Some(state) = app.try_state::<DiagnosticsState>() else {
        log::warn!("diagnostics: DiagnosticsState not managed -- cockpit disabled this run");
        return;
    };
    let state: DiagnosticsState = state.inner().clone();

    // New connection generation: any still-running tasks from a previous
    // connection see the bump and exit.
    let my_gen = state.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;

    {
        let mut inner = state.lock();
        inner.connected = true;
        inner.room_name = Some(room_display_name.clone());
        inner.server_host = host_from_url(&server_url);
        inner.local_identity = Some(local_identity.clone());
        inner.reconnect_count = 0;
        inner.quality.clear();
        inner.peer_rtt_ms = None;
        inner.history.clear();
        inner.tracks.clear();
        inner.latency.clear();
        inner.peer_clock_offsets.clear();
        inner.latency_clock_sync_needed.clear();
        inner.stream_states.clear();
        inner.capture_pipeline.clear_all();
        inner.display_pipeline.clear_all();
        inner.software_decode_fallbacks.clear();
        inner.display_drop_last_warned.clear();
        inner.enqueue_backoff.clear();
        // #878 adversarial-review finding 2: clearing per-track backoff state
        // without resetting the GLOBAL pause flag strands it true across the
        // room lifecycle -- the next meeting's shares would be invisible for
        // up to ~45s (the #416 state-vs-lifecycle class). Same reset at every
        // clear site.
        #[cfg(target_os = "macos")]
        crate::compositor::set_display_enqueue_backoff_paused(false);
        inner.receiver_freeze.clear();
        clear_remote_pipeline_health(&mut inner);
        // The journal deliberately survives across connections (a session-
        // level log, not a per-connection one) -- only metrics reset.
    }
    state.journal_append(
        app,
        "connection",
        format!(
            "Joined room '{}' as '{}'",
            crate::logging::log_safe_quoted(&room_display_name),
            crate::logging::log_safe_quoted(&local_identity)
        ),
    );
    log::info!(
        "diagnostics: started for room '{}' (generation {my_gen})",
        crate::logging::log_safe_quoted(&room_display_name)
    );

    // ---- Task 1: room-event journal + quality/reconnect bookkeeping ----
    {
        let state = state.clone();
        let app = app.clone();
        let mut events = room.subscribe();
        tauri::async_runtime::spawn(async move {
            use livekit::RoomEvent;
            while let Some(event) = events.recv().await {
                if state.shared.generation.load(Ordering::SeqCst) != my_gen {
                    break; // a newer connection took over
                }
                match event {
                    RoomEvent::ParticipantConnected(p) => {
                        let display = display_of(&p.name(), &p.identity().to_string());
                        log::info!(
                            "diagnostics: {display} joined '{}' (generation {my_gen})",
                            crate::logging::log_safe_quoted(&room_display_name)
                        );
                        state.journal_append(&app, "presence", format!("{display} joined"));
                    }
                    RoomEvent::ParticipantDisconnected(p) => {
                        let identity = p.identity().to_string();
                        let display = display_of(&p.name(), &identity);
                        // journal_append only reaches the in-app Network Cockpit
                        // journal + a Tauri event -- it never calls `log::`, so
                        // without this line a participant disconnect is
                        // completely invisible in petal.log (found while
                        // investigating a live disconnect-detection report where
                        // grepping the log for "disconnect" came back with zero
                        // hits despite four RoomEvent::ParticipantDisconnected
                        // handler sites across the codebase).
                        log::warn!(
                            "diagnostics: {display} disconnected from '{}' \
                             (generation {my_gen})",
                            crate::logging::log_safe_quoted(&room_display_name)
                        );
                        state.journal_append(&app, "presence", format!("{display} left"));
                        let mut inner = state.lock();
                        inner.quality.retain(|q| q.identity != identity);
                        inner.peer_clock_offsets.remove(&identity);
                        inner.remote_pipeline.retain(|key, _| {
                            key.owner_identity != identity && key.reporter_id != identity
                        });
                        inner.remote_capture_states.retain(|key, _| {
                            key.owner_identity != identity && key.reporter_id != identity
                        });
                        inner.remote_receiver_freezes.retain(|key, _| {
                            key.owner_identity != identity && key.reporter_id != identity
                        });
                        inner.remote_pipeline_sequences.retain(|key, _| {
                            key.owner_identity != identity && key.reporter_id != identity
                        });
                        inner.remote_pipeline_lifecycles.retain(|key, _| {
                            key.owner_identity != identity && key.reporter_id != identity
                        });
                        inner.remote_pipeline_terminal.retain(|key, _| {
                            key.owner_identity != identity && key.reporter_id != identity
                        });
                    }
                    RoomEvent::LocalTrackPublished { publication, .. } => {
                        state.journal_append(
                            &app,
                            "shares",
                            format!("Started publishing {}", describe_track(&publication.name())),
                        );
                    }
                    RoomEvent::LocalTrackUnpublished { publication, .. } => {
                        state.journal_append(
                            &app,
                            "shares",
                            format!("Stopped publishing {}", describe_track(&publication.name())),
                        );
                    }
                    RoomEvent::TrackSubscribed {
                        publication,
                        participant,
                        ..
                    } => {
                        state.journal_append(
                            &app,
                            "shares",
                            format!(
                                "Receiving {} from {}",
                                describe_track(&publication.name()),
                                display_of(
                                    &participant.name(),
                                    &participant.identity().to_string()
                                )
                            ),
                        );
                    }
                    RoomEvent::TrackUnsubscribed {
                        publication,
                        participant,
                        ..
                    } => {
                        state.journal_append(
                            &app,
                            "shares",
                            format!(
                                "Stopped receiving {} from {}",
                                describe_track(&publication.name()),
                                display_of(
                                    &participant.name(),
                                    &participant.identity().to_string()
                                )
                            ),
                        );
                    }
                    RoomEvent::TrackMuted {
                        participant,
                        publication,
                    } => {
                        state.journal_append(
                            &app,
                            "media",
                            format!(
                                "{} muted {}",
                                display_of(
                                    &participant.name(),
                                    &participant.identity().to_string()
                                ),
                                describe_track(&publication.name())
                            ),
                        );
                    }
                    RoomEvent::TrackUnmuted {
                        participant,
                        publication,
                    } => {
                        state.journal_append(
                            &app,
                            "media",
                            format!(
                                "{} unmuted {}",
                                display_of(
                                    &participant.name(),
                                    &participant.identity().to_string()
                                ),
                                describe_track(&publication.name())
                            ),
                        );
                    }
                    RoomEvent::ConnectionQualityChanged {
                        quality,
                        participant,
                    } => {
                        let identity = participant.identity().to_string();
                        let quality = quality_str(quality);
                        {
                            let mut inner = state.lock();
                            match inner.quality.iter_mut().find(|q| q.identity == identity) {
                                Some(q) => q.quality = quality.to_string(),
                                None => inner.quality.push(ParticipantQuality {
                                    identity: identity.clone(),
                                    quality: quality.to_string(),
                                }),
                            }
                        }
                        // Only journal degradations/recoveries humans care
                        // about, not every excellent<->good flicker.
                        if quality == "poor" || quality == "lost" {
                            state.journal_append(
                                &app,
                                "connection",
                                format!(
                                    "Connection quality for {} dropped to {}",
                                    display_of(&participant.name(), &identity),
                                    quality
                                ),
                            );
                        }
                    }
                    RoomEvent::Reconnecting => {
                        {
                            let mut inner = state.lock();
                            inner.reconnect_count += 1;
                        }
                        state.journal_append(&app, "connection", "Reconnecting…".to_string());
                    }
                    RoomEvent::Reconnected => {
                        state.journal_append(&app, "connection", "Reconnected".to_string());
                        crate::analytics::reconnect_recovered();
                    }
                    RoomEvent::Disconnected { reason } => {
                        if reason != livekit::DisconnectReason::ClientInitiated {
                            crate::analytics::reconnect_failed();
                        }
                        state.journal_append(&app, "connection", format!("Left room ({reason:?})"));
                        {
                            let mut inner = state.lock();
                            inner.connected = false;
                            inner.quality.clear();
                            inner.peer_rtt_ms = None;
                            inner.tracks.clear();
                            inner.latency.clear();
                            inner.peer_clock_offsets.clear();
                            inner.latency_clock_sync_needed.clear();
                            inner.stream_states.clear();
                            inner.capture_pipeline.clear_all();
                            inner.display_pipeline.clear_all();
                            inner.software_decode_fallbacks.clear();
                            inner.display_drop_last_warned.clear();
                            inner.enqueue_backoff.clear();
                            // #878 review finding 2: also reset the global
                            // pause flag -- see the start_for_room clear site.
                            #[cfg(target_os = "macos")]
                            crate::compositor::set_display_enqueue_backoff_paused(false);
                            inner.receiver_freeze.clear();
                            clear_remote_pipeline_health(&mut inner);
                        }
                        // Kill the poller too (it checks the generation every
                        // tick) -- but only if no newer connection already
                        // bumped it.
                        let _ = state.shared.generation.compare_exchange(
                            my_gen,
                            my_gen + 1,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        break;
                    }
                    _ => {}
                }
            }
            log::info!("diagnostics: event journal loop stopped (generation {my_gen})");
        });
    }

    // ---- Task 2: ~1s stats poller ----
    {
        let state = state.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            // Previous cumulative byte counters, keyed by "<sid>:<dir>", for
            // rate derivation (see module doc comment).
            let mut prev_bytes: HashMap<String, u64> = HashMap::new();
            let mut prev_tick: Option<std::time::Instant> = None;
            // Debounce for the "stalled" classification only (#358); see
            // `StallDebounce`'s doc comment.
            let mut stall_debounce = StallDebounce::default();
            // #884: in-room memory curve + pressure-transition watch. The
            // footprint line every MEMORY_LOG_EVERY_N_TICKS gives field logs
            // a curve instead of anomaly snapshots (#878's 1301->1987MB jump
            // was only visible because two ad-hoc lines happened to bracket
            // it); the pressure transition is the earliest system-visible
            // stage of the leak -> pressure -> allocation-failure -> session-
            // teardown chain and captures a rate-limited Sentry diagnostic.
            let mut memory_tick = 0u32;
            let mut last_pressure_level: Option<u32> = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
                if state.shared.generation.load(Ordering::SeqCst) != my_gen {
                    break;
                }
                let now = std::time::Instant::now();
                let dt_ms = prev_tick.map(|t| now.duration_since(t).as_millis() as u64);
                prev_tick = Some(now);

                memory_tick = memory_tick.wrapping_add(1);
                if memory_tick % MEMORY_LOG_EVERY_N_TICKS == 0 {
                    log_in_room_memory_curve();
                }
                observe_memory_pressure_transition(&mut last_pressure_level);

                let (sample, mut tracks) = collect_tick(&room, &mut prev_bytes, dt_ms).await;
                state.apply_track_overlays(&mut tracks, sample.rtt_ms);
                crate::pipeline_stats::publish_for_tracks(
                    room.clone(),
                    &local_identity,
                    &tracks,
                    &state,
                )
                .await;
                state.apply_remote_pipeline_overlays(&mut tracks);
                if let Some(track) = tracks.iter().find(|track| {
                    track.direction == "send" && track.kind == "video" && track.software_encoder
                }) {
                    if !state
                        .shared
                        .software_encoder_reported
                        .swap(true, Ordering::SeqCst)
                    {
                        log::warn!(
                            "diagnostics: software encoder detected for {} (encoder_implementation='{}'); reduced performance mode recommended until Intel quality step-down is measured",
                            track.name,
                            track.codec_impl,
                        );
                        state.journal_append(
                            &app,
                            "media",
                            format!(
                                "Reduced performance mode: hardware encoder unavailable for {}",
                                describe_track(&track.name)
                            ),
                        );
                    }
                }
                for track in tracks
                    .iter()
                    .filter(|track| track.direction == "recv" && track.kind == "video")
                {
                    // The debounce counter is keyed on the same
                    // `frames_decoded` counter-delta rate already computed
                    // for the decoded-stage metric -- never the smoothed
                    // `fps` gauge `track.stream_state` itself was classified
                    // from (#358: the gauge can lag across polls).
                    let decoder_fps = track.decoded.as_ref().and_then(|stage| stage.fps);
                    let Some(gated_state) =
                        stall_debounce.gate(&track.latency_key, &track.stream_state, decoder_fps)
                    else {
                        // Still inside the debounce window -- suppress this
                        // tick's "stalled" attempt entirely; do not touch
                        // the shared sink at all.
                        continue;
                    };
                    let source = if gated_state == "stalled" {
                        "stats-frame-starvation"
                    } else {
                        "stats-frame-flow"
                    };
                    let display_enqueued_fps = track.display_enqueued.as_ref().and_then(|s| s.fps);
                    // When available, keep the sender's capture-freeze
                    // verdict beside receiver counters. This distinguishes a
                    // sender that stopped producing content from decode or
                    // display starvation (#358).
                    let sender_capture = track.remote_capture_state.as_ref().map(|report| {
                        let state = match report.state.state {
                            CaptureStateKind::Live => "live",
                            CaptureStateKind::Idle => "idle",
                            CaptureStateKind::Occluded => "occluded",
                            CaptureStateKind::Wedged => "wedged",
                        };
                        let fps = report
                            .state
                            .fps
                            .map_or_else(|| "n/a".to_string(), |value| format!("{value:.2}"));
                        let dirty = report
                            .state
                            .dirty_rect_count
                            .map_or_else(|| "n/a".to_string(), |value| value.to_string());
                        let occlusion = report
                            .state
                            .occlusion_pct
                            .map_or_else(|| "n/a".to_string(), |value| format!("{value:.0}%"));
                        format!("{state},fps={fps},dirty={dirty},occlusion={occlusion}")
                    });
                    let metrics = format!(
                        "decoder_fps={} sdk_smoothed_fps={} display_enqueued_fps={} frames_decoded={} sender_capture={}",
                        decoder_fps.map_or_else(|| "n/a".to_string(), |v| format!("{v:.2}")),
                        present_finite(track.fps)
                            .map_or_else(|| "n/a".to_string(), |v| format!("{v:.2}")),
                        display_enqueued_fps
                            .map_or_else(|| "n/a".to_string(), |v| format!("{v:.2}")),
                        track.frames_decoded,
                        sender_capture.unwrap_or_else(|| "n/a".to_string()),
                    );
                    state.record_video_stream_state_by_key(
                        &app,
                        track.latency_key.clone(),
                        track.name.clone(),
                        gated_state,
                        source.to_string(),
                        Some(metrics),
                    );
                }

                let push = state.shared.cockpit_open.load(Ordering::SeqCst);
                {
                    let mut inner = state.lock();
                    push_bounded(&mut inner.history, sample, HISTORY_CAP);
                    inner.tracks = tracks;
                }
                if push {
                    // Global emit -> reaches the dedicated "network-cockpit"
                    // window (issue #37); "main"-targeted would never
                    // reach it. See the journal_append emit above.
                    let _ = tauri::Emitter::emit(&app, "network-stats", state.snapshot());
                }
            }
            log::info!("diagnostics: stats poller stopped (generation {my_gen})");
        });
    }
}

/// One poll tick: read `get_stats()` from every local + remote track on the
/// room and fold into (aggregate sample, per-track health). `dt_ms` is the
/// elapsed time since the previous tick (None on the first tick -- rates
/// report 0 until a second counter reading exists).
///
/// `pub` (not private) so `examples/`-style probes can exercise the exact
/// collection path the in-app poller runs against a real LiveKit
/// connection, without needing an `AppHandle` (which can't be constructed
/// headless in this environment -- see CLAUDE.md's `tauri::Builder` note).
pub async fn collect_tick(
    room: &livekit::Room,
    prev_bytes: &mut HashMap<String, u64>,
    dt_ms: Option<u64>,
) -> (StatsSample, Vec<TrackHealth>) {
    use livekit::webrtc::stats::RtcStats;

    let mut tracks: Vec<TrackHealth> = Vec::new();
    let mut rtts: Vec<f64> = Vec::new();
    let mut send_jitters: Vec<f64> = Vec::new();
    let mut recv_jitters: Vec<f64> = Vec::new();
    let mut losses: Vec<f64> = Vec::new();
    let mut send_kbps_total = 0.0;
    let mut recv_kbps_total = 0.0;

    // --- Published (send) tracks ---
    for (sid, publication) in room.local_participant().track_publications() {
        let Some(track) = publication.track() else {
            continue;
        };
        let Ok(stats) = track.get_stats().await else {
            continue;
        };
        let track_name = publication.name();
        let mut health = TrackHealth {
            sid: sid.to_string(),
            name: track_name.clone(),
            raw_track_name: Some(track_name.clone()),
            window_id: crate::transport::publisher::window_id_from_track_name(&track_name),
            direction: "send".to_string(),
            stream_state: "unknown".to_string(),
            ..Default::default()
        };
        let mut bytes_sent: u64 = 0;
        let mut packets_lost_total: i64 = 0;
        // A Full-tier share publishes 2 simulcast layers (full-res + a throttled
        // half-res layer, transport/publisher.rs:673-683 `full_share_simulcast_
        // layers`), so `stats` carries one `OutboundRtp` entry PER LAYER. Naively
        // overwriting `health.*` on every entry ("last write wins") meant the
        // debug panel could silently end up showing whichever layer's report
        // happened to be last in the vec -- e.g. "Shared" resolution/fps/frame
        // counts from the small, 12fps-capped half layer instead of the primary
        // one actually being offered to viewers. Deterministically prefer the
        // largest ACTIVE layer instead (falling back to the first entry seen so
        // a single non-simulcast layer, or all-inactive layers during startup,
        // still populates the panel).
        let mut have_primary_outbound = false;
        let mut primary_outbound_area: u64 = 0;
        let mut primary_outbound_active = false;
        for stat in &stats {
            match stat {
                RtcStats::OutboundRtp(o) => {
                    bytes_sent += o.sent.bytes_sent;
                    let area =
                        u64::from(o.outbound.frame_width) * u64::from(o.outbound.frame_height);
                    let is_primary = is_more_primary_layer(
                        have_primary_outbound,
                        o.outbound.active,
                        area,
                        primary_outbound_active,
                        primary_outbound_area,
                    );
                    if !is_primary {
                        continue;
                    }
                    have_primary_outbound = true;
                    primary_outbound_area = area;
                    primary_outbound_active = o.outbound.active;
                    health.kind = o.stream.kind.clone();
                    health.width = o.outbound.frame_width;
                    health.height = o.outbound.frame_height;
                    health.fps = o.outbound.frames_per_second;
                    health.codec_impl = o.outbound.encoder_implementation.clone();
                    if o.stream.kind == "video" {
                        health.encoded_sent = Some(PipelineStageMetrics {
                            width: present_dimension(o.outbound.frame_width),
                            height: present_dimension(o.outbound.frame_height),
                            fps: None,
                            kbps: None,
                        });
                        health.quality_limitation =
                            format!("{:?}", o.outbound.quality_limitation_reason).to_lowercase();
                        health.software_encoder =
                            crate::transport::publisher::encoder_looks_software(
                                &o.outbound.encoder_implementation,
                                o.outbound.power_efficient_encoder,
                            );
                    }
                    health.target_kbps = o.outbound.target_bitrate / 1000.0;
                    health.frames_encoded = o.outbound.frames_encoded;
                    health.key_frames_encoded = o.outbound.key_frames_encoded;
                    health.nack_count = o.outbound.nack_count;
                    health.fir_count = o.outbound.fir_count;
                    health.pli_count = o.outbound.pli_count;
                }
                RtcStats::RemoteInboundRtp(r) => {
                    // The receiver's RTCP report about OUR outbound stream:
                    // real RTT/jitter/loss for the send path. Under simulcast
                    // there is one report per layer/SSRC -- sum them (like
                    // bytes_sent above) so "cumulative" loss covers the whole
                    // track, not just whichever layer's report was read last.
                    if r.remote_inbound.round_trip_time > 0.0 {
                        rtts.push(r.remote_inbound.round_trip_time * 1000.0);
                    }
                    send_jitters.push(r.received.jitter * 1000.0);
                    losses.push(r.remote_inbound.fraction_lost * 100.0);
                    packets_lost_total += r.received.packets_lost;
                }
                _ => {}
            }
        }
        health.packets_lost = packets_lost_total;
        let encoded_fps = if health.kind == "video" {
            rate_per_second(
                prev_bytes,
                &format!("{sid}:frames_encoded"),
                u64::from(health.frames_encoded),
                dt_ms,
            )
        } else {
            None
        };
        health.actual_kbps = rate_kbps(prev_bytes, &format!("{sid}:send"), bytes_sent, dt_ms);
        if let Some(stage) = health.encoded_sent.as_mut() {
            stage.fps = encoded_fps;
            stage.kbps = dt_ms.map(|_| health.actual_kbps);
        }
        send_kbps_total += health.actual_kbps;
        tracks.push(health);
    }

    // --- Subscribed (recv) tracks ---
    for (identity, participant) in room.remote_participants() {
        for (sid, publication) in participant.track_publications() {
            let Some(track) = publication.track() else {
                continue;
            };
            // Track type is the AUTHORITATIVE kind. For recv AUDIO, get_stats()
            // frequently yields no InboundRtp (or an empty stream.kind), leaving
            // `kind` unset -- which made remote audio invisible to any consumer
            // filtering on kind=="audio" (e.g. the AUD cockpit assertion). Derive
            // kind from the subscribed track itself so it is always correct.
            let track_kind = match &track {
                livekit::prelude::RemoteTrack::Audio(_) => "audio",
                livekit::prelude::RemoteTrack::Video(_) => "video",
            };
            let Ok(stats) = track.get_stats().await else {
                continue;
            };
            let mut health = TrackHealth {
                latency_key: latency_key(&identity.to_string(), &publication.name()),
                sid: sid.to_string(),
                name: format!("{} ({})", publication.name(), identity),
                raw_track_name: Some(publication.name()),
                owner_identity: Some(identity.to_string()),
                window_id: crate::transport::publisher::window_id_from_track_name(
                    &publication.name(),
                ),
                direction: "recv".to_string(),
                kind: track_kind.to_string(),
                stream_state: "unknown".to_string(),
                ..Default::default()
            };
            let mut bytes_received: u64 = 0;
            // A simulcast subscription has one `InboundRtp` entry per layer.
            // Sum byte counters across those entries, but select the primary
            // layer for dimensions, frame counters, and the SDK's smoothed
            // gauge just as the send path does. Inbound stats do not expose
            // the outbound `active` bit, so the largest layer wins and equal
            // layers retain the first entry.
            let mut have_primary_inbound = false;
            let mut primary_inbound_area: u64 = 0;
            for stat in &stats {
                if let RtcStats::InboundRtp(i) = stat {
                    recv_jitters.push(i.received.jitter * 1000.0);
                    bytes_received += i.inbound.bytes_received;
                    apply_primary_inbound_layer(
                        &mut health,
                        i,
                        &mut have_primary_inbound,
                        &mut primary_inbound_area,
                    );
                }
            }
            let decoder_fps = if health.kind == "video" {
                rate_per_second(
                    prev_bytes,
                    &format!("{sid}:frames_decoded"),
                    u64::from(health.frames_decoded),
                    dt_ms,
                )
            } else {
                None
            };
            health.actual_kbps =
                rate_kbps(prev_bytes, &format!("{sid}:recv"), bytes_received, dt_ms);
            if let Some(stage) = health.received.as_mut() {
                stage.kbps = dt_ms.map(|_| health.actual_kbps);
            }
            if let Some(stage) = health.decoded.as_mut() {
                stage.fps = decoder_fps;
            }
            if health.kind == "video"
                && health.width > 0
                && health.height > 0
                && health.fps < 0.5
                && health.actual_kbps < 1.0
                && dt_ms.is_some()
            {
                health.stream_state = "stalled".to_string();
            } else if health.kind == "video" {
                health.stream_state = "active".to_string();
            } else if health.kind == "audio" {
                // Recv audio has no frame/fps signal, and get_stats() often yields
                // no InboundRtp bytes for it, so neither the video "active" path nor
                // an actual_kbps>0 signal fires. A subscribed, unmuted remote audio
                // track IS being played out -- surface that as "active" (and
                // "muted" when the remote muted it) so audio flow is observable.
                health.stream_state = if publication.is_muted() {
                    "muted".to_string()
                } else {
                    "active".to_string()
                };
            }
            recv_kbps_total += health.actual_kbps;
            tracks.push(health);
        }
    }

    let max_of = |v: &[f64]| {
        v.iter().cloned().fold(None::<f64>, |acc, x| {
            Some(match acc {
                Some(a) if a >= x => a,
                _ => x,
            })
        })
    };

    let sample = StatsSample {
        t_ms: now_ms(),
        rtt_ms: max_of(&rtts),
        // Send-path jitter preferred (paired with RTT source); receive-path
        // fallback for receive-only participants.
        jitter_ms: max_of(&send_jitters).or_else(|| max_of(&recv_jitters)),
        send_kbps: send_kbps_total,
        recv_kbps: recv_kbps_total,
        loss_pct: max_of(&losses),
        // #683: rides this existing ~1s tick for free -- see `StatsSample`'s
        // doc comments for the honest-`None` contract on each.
        phys_footprint_mb: crate::platform::mem::process_footprint_bytes_throttled()
            .map(|bytes| (bytes / 1_000_000) as u32),
        live_pixel_buffers: crate::platform::mem::live_pixel_buffer_count(),
    };
    (sample, tracks)
}

/// Byte-counter -> kbit/s conversion with previous-counter bookkeeping.
/// Counter resets (new track SID reusing a key, stats restart) clamp to 0
/// rather than reporting a huge negative-wrap rate.
fn rate_kbps(
    prev: &mut HashMap<String, u64>,
    key: &str,
    current_bytes: u64,
    dt_ms: Option<u64>,
) -> f64 {
    rate_per_second(prev, key, current_bytes, dt_ms)
        .map(|bytes_per_second| bytes_per_second * 8.0 / 1000.0)
        .unwrap_or(0.0)
}

/// Generic cumulative-counter rate. A first sample or counter reset is absent
/// (`None`), while a real no-delta interval is `Some(0.0)` so a resubscribe
/// cannot be mistaken for a measured stalled stream.
fn rate_per_second(
    prev: &mut HashMap<String, u64>,
    key: &str,
    current: u64,
    dt_ms: Option<u64>,
) -> Option<f64> {
    let previous = prev.insert(key.to_string(), current);
    match (previous, dt_ms) {
        (Some(p), Some(dt)) if dt > 0 && current >= p => {
            Some((current - p) as f64 * 1000.0 / dt as f64)
        }
        (Some(_), Some(_)) => None,
        _ => None,
    }
}

/// Whether a newly-seen simulcast layer report should replace the currently-
/// selected "primary" layer for `TrackHealth`. The send path supplies the
/// real `active` bit; inbound reports treat observed layers as active.
/// Pulled out of `collect_tick`'s per-tick reduction so the fix for the
/// "last write wins across simulcast layers" bug (a Full-tier share's debug
/// panel could silently show whichever layer's stats happened to be read
/// last, e.g. the throttled half-resolution layer instead of the primary
/// one) is directly unit-testable without a live LiveKit connection.
///
/// Rule: an active layer always beats an inactive one (regardless of size);
/// among equally-active layers, the larger (by pixel area) wins; the very
/// first candidate seen is always accepted so a non-simulcast single layer,
/// or a report with no active layers yet, still populates the panel.
fn is_more_primary_layer(
    have_primary: bool,
    candidate_active: bool,
    candidate_area: u64,
    primary_active: bool,
    primary_area: u64,
) -> bool {
    !have_primary
        || (candidate_active && !primary_active)
        || (candidate_active == primary_active && candidate_area > primary_area)
}

/// Fold an `InboundRtp` entry into receive health only when it represents the
/// selected primary simulcast layer. Byte counters remain summed by the
/// caller, because they are cumulative across every layer.
fn apply_primary_inbound_layer(
    health: &mut TrackHealth,
    inbound: &livekit::webrtc::stats::InboundRtpStats,
    have_primary: &mut bool,
    primary_area: &mut u64,
) {
    let candidate_area =
        u64::from(inbound.inbound.frame_width) * u64::from(inbound.inbound.frame_height);
    // Inbound stats do not carry `active`; treat every observed layer as
    // active, leaving the shared primary-layer rule to choose the largest.
    if !is_more_primary_layer(*have_primary, true, candidate_area, true, *primary_area) {
        return;
    }
    *have_primary = true;
    *primary_area = candidate_area;

    // Only let stats override kind if they carry one; never clobber the
    // authoritative track-type kind with an empty string.
    if !inbound.stream.kind.is_empty() {
        health.kind = inbound.stream.kind.clone();
    }
    health.width = inbound.inbound.frame_width;
    health.height = inbound.inbound.frame_height;
    health.fps = inbound.inbound.frames_per_second;
    health.codec_impl = inbound.inbound.decoder_implementation.clone();
    health.packets_lost = inbound.received.packets_lost;
    health.frames_decoded = inbound.inbound.frames_decoded;
    health.key_frames_decoded = inbound.inbound.key_frames_decoded;
    health.frames_dropped = inbound.inbound.frames_dropped;
    health.received = Some(PipelineStageMetrics {
        width: present_dimension(inbound.inbound.frame_width),
        height: present_dimension(inbound.inbound.frame_height),
        fps: present_finite(inbound.inbound.frames_per_second),
        kbps: None,
    });
    health.decoded = Some(PipelineStageMetrics {
        width: present_dimension(inbound.inbound.frame_width),
        height: present_dimension(inbound.inbound.frame_height),
        fps: None,
        kbps: None,
    });
    health.nack_count = inbound.inbound.nack_count;
    health.fir_count = inbound.inbound.fir_count;
    health.pli_count = inbound.inbound.pli_count;
    if inbound.inbound.jitter_buffer_emitted_count > 0 {
        // Cumulative average, labeled as such in the UI -- see module docs.
        health.jitter_buffer_ms = Some(
            inbound.inbound.jitter_buffer_delay
                / inbound.inbound.jitter_buffer_emitted_count as f64
                * 1000.0,
        );
        health.jitter_buffer_target_ms = Some(
            inbound.inbound.jitter_buffer_target_delay
                / inbound.inbound.jitter_buffer_emitted_count as f64
                * 1000.0,
        );
        health.jitter_buffer_minimum_ms = Some(
            inbound.inbound.jitter_buffer_minimum_delay
                / inbound.inbound.jitter_buffer_emitted_count as f64
                * 1000.0,
        );
    }
}

fn present_dimension(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

fn present_finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn present_nonnegative_finite(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn nonempty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty() && trimmed != "none").then(|| trimmed.to_string())
}

fn pipeline_stage_has_signal(stage: &PipelineStageMetrics) -> bool {
    stage.width.is_some() || stage.height.is_some() || stage.fps.is_some() || stage.kbps.is_some()
}

fn latest_remote_pipeline_report(
    reports: &HashMap<RemotePipelineStageKey, PipelineStageReport>,
    owner_identity: &str,
    window_id: u32,
    publication_sid: &str,
    stage: PipelineStageKind,
    reporter_matches: impl Fn(&str) -> bool,
) -> Option<PipelineStageReport> {
    reports
        .iter()
        .filter(|(key, _)| {
            key.owner_identity == owner_identity
                && key.window_id == window_id
                && key
                    .publication_sid
                    .as_deref()
                    .is_none_or(|sid| sid == publication_sid)
                && key.stage == stage
                && reporter_matches(&key.reporter_id)
        })
        .max_by_key(|(_, report)| report.received_at_ms)
        .map(|(_, report)| report.clone())
}

fn latest_remote_capture_state_report(
    reports: &HashMap<RemoteWindowReportKey, RemoteCaptureStateReport>,
    owner_identity: &str,
    window_id: u32,
    publication_sid: &str,
    reporter_matches: impl Fn(&str) -> bool,
) -> Option<RemoteCaptureStateReport> {
    reports
        .iter()
        .filter(|(key, _)| {
            key.owner_identity == owner_identity
                && key.window_id == window_id
                && key
                    .publication_sid
                    .as_deref()
                    .is_none_or(|sid| sid == publication_sid)
                && reporter_matches(&key.reporter_id)
        })
        .max_by_key(|(_, report)| report.received_at_ms)
        .map(|(_, report)| report.clone())
}

fn latest_remote_receiver_freeze_report(
    reports: &HashMap<RemoteWindowReportKey, RemoteReceiverFreezeReport>,
    owner_identity: &str,
    window_id: u32,
    publication_sid: &str,
    reporter_matches: impl Fn(&str) -> bool,
) -> Option<RemoteReceiverFreezeReport> {
    reports
        .iter()
        .filter(|(key, _)| {
            key.owner_identity == owner_identity
                && key.window_id == window_id
                && key
                    .publication_sid
                    .as_deref()
                    .is_none_or(|sid| sid == publication_sid)
                && reporter_matches(&key.reporter_id)
        })
        .max_by_key(|(_, report)| report.received_at_ms)
        .map(|(_, report)| report.clone())
}

fn latest_remote_lifecycle_report(
    reports: &HashMap<RemoteShareEpochKey, RemoteLifecycleReport>,
    owner_identity: &str,
    window_id: u32,
    publication_sid: &str,
    canonical_epoch: Option<&str>,
    reporter_matches: impl Fn(&str) -> bool,
) -> Option<RemotePipelineLifecycleReport> {
    reports
        .iter()
        .filter(|(key, report)| {
            key.owner_identity == owner_identity
                && key.window_id == window_id
                && key
                    .publication_sid
                    .as_deref()
                    .is_none_or(|sid| sid == publication_sid)
                && canonical_epoch.map_or(
                    !matches!(
                        report.lifecycle,
                        PipelineLifecycle::Unpublished | PipelineLifecycle::TerminalFailure
                    ),
                    |epoch| key.share_epoch == epoch,
                )
                && reporter_matches(&key.reporter_id)
        })
        .max_by_key(|(_, report)| report.received_at_ms)
        .map(|(key, report)| RemotePipelineLifecycleReport {
            reporter_id: key.reporter_id.clone(),
            lifecycle: format!("{:?}", report.lifecycle),
            received_at_ms: report.received_at_ms,
        })
}

/// Human label for a Petal track name: `petal-window-<id>` -> "window <id>
/// share", `petal-camera-*` -> "webcam", audio/mic names pass through.
fn describe_track(name: &str) -> String {
    if let Some(id) = crate::transport::publisher::window_id_from_track_name(name) {
        return format!("window {id} share");
    }
    if name.starts_with(crate::transport::publisher::CAMERA_TRACK_PREFIX) {
        return "webcam".to_string();
    }
    // The assistant's published voice (#657). Audio names reach this function,
    // so without a branch here a `petal-ai-*` track is reported as a raw name
    // and reads like an unrecognised track in diagnostics.
    #[cfg(target_os = "macos")]
    if crate::ai_chat::wire::is_ai_track(name) {
        return "AI assistant voice".to_string();
    }
    if name.is_empty() {
        return "a track".to_string();
    }
    format!("'{name}'")
}

fn latency_key(identity: &str, track_name: &str) -> String {
    format!("{identity}\u{1f}{track_name}")
}

fn corrected_latency_ms(
    capture_timestamp_us: u64,
    receive_timestamp_us: u64,
    sender_to_receiver_offset_us: i64,
) -> Option<f64> {
    let corrected_capture_us =
        i128::from(capture_timestamp_us) + i128::from(sender_to_receiver_offset_us);
    let latency_us = i128::from(receive_timestamp_us) - corrected_capture_us;
    if latency_us < 0 {
        return None;
    }
    Some(latency_us as f64 / 1000.0)
}

/// Prefer the display name; fall back to the identity when the name is empty.
fn display_of(name: &str, identity: &str) -> String {
    if name.trim().is_empty() {
        identity.to_string()
    } else {
        name.to_string()
    }
}

fn quality_str(q: livekit::participant::ConnectionQuality) -> &'static str {
    use livekit::participant::ConnectionQuality;
    match q {
        ConnectionQuality::Excellent => "excellent",
        ConnectionQuality::Good => "good",
        ConnectionQuality::Poor => "poor",
        ConnectionQuality::Lost => "lost",
    }
}

/// Host portion of a ws(s)/http(s) URL -- never the full URL (query strings
/// can carry credential-adjacent material; the cockpit only needs the host).
fn host_from_url(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split(['/', '?']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn push_bounded<T>(buf: &mut VecDeque<T>, item: T, cap: usize) {
    while buf.len() >= cap {
        buf.pop_front();
    }
    buf.push_back(item);
}

fn recent_samples(history: &[StatsSample]) -> &[StatsSample] {
    let start = history.len().saturating_sub(10);
    &history[start..]
}

fn avg_present(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0;
    for value in values.flatten() {
        sum += value;
        count += 1;
    }
    (count > 0).then_some(sum / count as f64)
}

fn finding(severity: &str, title: &str, evidence: String, recommendation: &str) -> AnalysisFinding {
    AnalysisFinding {
        severity: severity.to_string(),
        title: title.to_string(),
        evidence,
        recommendation: recommendation.to_string(),
    }
}

fn analyze_conditions(
    history: &[StatsSample],
    tracks: &[TrackHealth],
    reconnect_count: u32,
    quality: &[ParticipantQuality],
) -> Vec<AnalysisFinding> {
    let recent = recent_samples(history);
    let mut findings = Vec::new();

    if let Some(avg) = avg_present(recent.iter().map(|s| s.rtt_ms)) {
        if avg >= HIGH_RTT_MS {
            findings.push(finding(
                "warn",
                "High latency to the media server",
                format!("RTT averaged {avg:.0} ms over the latest samples."),
                "Expect delayed reactions in shared windows; disable VPN, prefer wired Ethernet, or choose a closer LiveKit region.",
            ));
        }
    }

    if let Some(avg) = avg_present(recent.iter().map(|s| s.jitter_ms)) {
        if avg >= HIGH_JITTER_MS {
            findings.push(finding(
                "warn",
                "Unstable network timing",
                format!("Jitter averaged {avg:.0} ms over the latest samples."),
                "Move closer to the router, switch to Ethernet, or stop other high-bandwidth Wi-Fi activity.",
            ));
        }
    }

    if let Some(avg) = avg_present(recent.iter().map(|s| s.loss_pct)) {
        if avg >= HIGH_LOSS_PCT {
            findings.push(finding(
                "warn",
                "Packet loss is degrading media",
                format!("Loss averaged {avg:.1}% over the latest samples."),
                "Expect artifacts or stutter; check Wi-Fi interference, VPNs, and upstream congestion.",
            ));
        }
    }

    if reconnect_count > FLAPPING_RECONNECTS {
        findings.push(finding(
            "warn",
            "Connection is flapping",
            format!("{reconnect_count} reconnects in this meeting."),
            "Use the event log to correlate reconnects with network changes; prefer a stable interface before continuing.",
        ));
    }

    if quality
        .iter()
        .any(|q| q.quality == "poor" || q.quality == "lost")
    {
        let names = quality
            .iter()
            .filter(|q| q.quality == "poor" || q.quality == "lost")
            .map(|q| format!("{}={}", q.identity, q.quality))
            .collect::<Vec<_>>()
            .join(", ");
        findings.push(finding(
            "warn",
            "LiveKit reports poor participant quality",
            names,
            "Check whether the affected participant is on Wi-Fi or bandwidth-constrained; their experience may be worse than local stats suggest.",
        ));
    }

    for track in tracks {
        if track.direction == "send" && track.quality_limitation == "cpu" {
            findings.push(finding(
                "warn",
                "This Mac is encode-limited",
                format!("{} reports quality limitation = cpu.", track.name),
                "Close CPU-heavy apps or share a smaller/lower-motion window.",
            ));
        } else if track.direction == "send" && track.quality_limitation == "bandwidth" {
            findings.push(finding(
                "warn",
                "Upload bandwidth is capping quality",
                format!("{} reports quality limitation = bandwidth.", track.name),
                "Stop other uploads or move to a faster network before increasing share quality.",
            ));
        }

        if track.direction == "send" && track.kind == "video" && track.software_encoder {
            findings.push(finding(
                "warn",
                "Hardware encoder is unavailable",
                format!("{} is using '{}'.", track.name, track.codec_impl),
                "Expect higher CPU and lower quality on this Mac; share a smaller window or use reduced quality until Intel step-down tuning lands.",
            ));
        }

        if track.direction == "send"
            && track.target_kbps > 0.0
            && track.actual_kbps > 0.0
            && track.actual_kbps < track.target_kbps * 0.55
        {
            findings.push(finding(
                "info",
                "Actual send bitrate is below target",
                format!(
                    "{} is sending {:.0} kbps against a {:.0} kbps target.",
                    track.name, track.actual_kbps, track.target_kbps
                ),
                "If the picture looks soft, network adaptation is likely active; check bandwidth and packet loss first.",
            ));
        }

        if track.direction == "recv"
            && track.jitter_buffer_ms.unwrap_or_default() >= HIGH_JITTER_BUFFER_MS
        {
            findings.push(finding(
                "warn",
                "Receive buffer is absorbing jitter",
                format!(
                    "{} jitter-buffer delay is {:.0} ms.",
                    track.name,
                    track.jitter_buffer_ms.unwrap_or_default()
                ),
                "The remote stream may feel delayed or uneven; check local Wi-Fi stability and remote sender quality.",
            ));
        }

        if track.direction == "recv" && track.frames_dropped >= HIGH_DROPPED_FRAMES {
            findings.push(finding(
                "warn",
                "Frames are being dropped on receive",
                format!(
                    "{} has {} dropped frames.",
                    track.name, track.frames_dropped
                ),
                "Close GPU/CPU-heavy apps or reduce the number of visible remote windows.",
            ));
        }

        if track.direction == "recv" && track.stream_state == "stalled" {
            findings.push(finding(
                "info",
                "Remote video is paused or stalled",
                format!("{} is receiving no fresh video frames.", track.name),
                "Petal keeps the last frame visible and will resume automatically when bandwidth recovers.",
            ));
        }

        let latency_value = track.glass_to_glass_ms.or(track.glass_to_glass_estimate_ms);
        if track.direction == "recv"
            && track.kind == "video"
            && latency_value.unwrap_or_default() >= 150.0
        {
            let kind = if track.glass_to_glass_ms.is_some() {
                "measured"
            } else {
                "estimated"
            };
            findings.push(finding(
                "info",
                "Shared-window latency is elevated",
                format!(
                    "{} has {kind} glass-to-glass latency of {:.0} ms.",
                    track.name,
                    latency_value.unwrap_or_default()
                ),
                "Prefer a closer media server or wired network if shared windows feel delayed.",
            ));
        }
    }

    if findings.is_empty() && !history.is_empty() {
        findings.push(finding(
            "info",
            "No network bottleneck detected",
            "Recent RTT, jitter, packet loss, reconnects, and media health are within Petal's warning thresholds.".to_string(),
            "If the experience still feels wrong, capture a screenshot of this cockpit and check the event log timing.",
        ));
    }

    findings
}

use crate::time_util::now_ms;

// =============================================================================
// Tauri commands (registered in lib.rs)
// =============================================================================

/// Full current snapshot: connection info, per-participant quality, metric
/// history (ring buffer, newest last), per-track health. The cockpit calls
/// this once on open; live updates then arrive via the `network-stats` event.
#[tauri::command]
pub fn get_network_snapshot(state: tauri::State<'_, DiagnosticsState>) -> NetworkSnapshot {
    state.snapshot()
}

/// The bounded event journal, oldest first (the cockpit reverses for
/// newest-first display). Live appends arrive via `journal-appended`.
#[tauri::command]
pub fn get_event_journal(state: tauri::State<'_, DiagnosticsState>) -> Vec<JournalEntry> {
    state.journal()
}

/// Gate for the ~1s `network-stats` push: the cockpit sets `true` on open
/// and `false` on close, so a closed cockpit costs no event traffic (the
/// poller itself keeps running to maintain history -- that's the point of
/// the ring buffer).
#[tauri::command]
pub fn set_cockpit_open(state: tauri::State<'_, DiagnosticsState>, open: bool) {
    state.shared.cockpit_open.store(open, Ordering::SeqCst);
    log::info!(
        "diagnostics: cockpit {}",
        if open { "opened" } else { "closed" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_capture_state() -> CaptureStateReport {
        CaptureStateReport {
            state: CaptureStateKind::Live,
            fps: None,
            dirty_rect_count: Some(1),
            dirty_area_px: Some(100),
            occlusion_pct: None,
            cpu: CaptureCpuMetrics {
                lock_copy_ms: Some(0.5),
                convert_ms: None,
                capture_frame_return_ms: None,
            },
        }
    }

    #[test]
    fn bounded_detail_truncates_on_char_boundary_not_mid_codepoint() {
        // "中" (U+4E2D) is 3 UTF-8 bytes per char. 60 of them is 180 bytes,
        // and since 160 is not a multiple of 3, byte offset 160 lands
        // exactly mid-codepoint -- this reproduces the panic in
        // `String::truncate(160)` that `bounded_detail` must guard against.
        let value: String = std::iter::repeat('\u{4e2d}').take(60).collect();
        assert!(value.len() > 160);
        assert!(
            !value.is_char_boundary(160),
            "test fixture must land mid-codepoint at byte 160 to exercise the panic path"
        );

        let result = bounded_detail(value);

        let result = result.expect("non-empty input must produce Some");
        assert!(result.len() <= 160, "result must be bounded to <=160 bytes, got {}", result.len());
        // Merely constructing/using `result` as a &str proves it is valid UTF-8;
        // String::truncate itself guarantees this once it doesn't panic.
        assert!(result.chars().all(|c| c == '\u{4e2d}'));
    }

    #[test]
    fn native_startup_timeline_serializes_ordered_sender_evidence() {
        let state = DiagnosticsState::default();
        state.begin_native_startup(42, "direct-window-id", Some(30), Some("Auto".to_string()));
        state.set_native_startup_correlation(42, 7, 0);
        state.record_native_startup_stage(
            42,
            NativeStartupStageKind::CaptureAttemptStarted,
            None,
            None,
            Some("attempt 1/3".to_string()),
        );
        state.record_native_startup_stage(
            42,
            NativeStartupStageKind::FirstFrame,
            Some(1920),
            Some(1200),
            None,
        );
        state.record_native_startup_stage(
            42,
            NativeStartupStageKind::MetadataBudgetExpired,
            Some(1920),
            Some(1200),
            Some("3100ms > budget".to_string()),
        );
        state.record_native_startup_publication(42, Some("TR_native".to_string()));
        state.record_native_startup_stage(
            42,
            NativeStartupStageKind::PublishSucceeded,
            Some(1920),
            Some(1200),
            None,
        );
        state.record_native_startup_stage(
            42,
            NativeStartupStageKind::FirstFramePushed,
            Some(1920),
            Some(1200),
            Some("push_changed".to_string()),
        );

        let snapshot = state.snapshot();
        assert_eq!(snapshot.native_startup.len(), 1);
        let report = &snapshot.native_startup[0];
        assert_eq!(report.window_id, 42);
        assert_eq!(report.started_seq, Some(7));
        assert_eq!(report.capture_path, "direct-window-id");
        assert_eq!(report.requested_fps, Some(30));
        assert_eq!(report.publication_sid.as_deref(), Some("TR_native"));
        assert_eq!(report.outcome, "published");
        assert_eq!(
            report
                .stages
                .iter()
                .map(|stage| stage.stage)
                .collect::<Vec<_>>(),
            vec![
                NativeStartupStageKind::StartRequested,
                NativeStartupStageKind::CaptureAttemptStarted,
                NativeStartupStageKind::FirstFrame,
                NativeStartupStageKind::MetadataBudgetExpired,
                NativeStartupStageKind::PublishSucceeded,
                NativeStartupStageKind::FirstFramePushed,
            ]
        );
        assert!(report
            .stages
            .iter()
            .all(|stage| stage.capture_path.as_deref() == Some("direct-window-id")));
    }

    #[test]
    fn native_startup_timeline_resets_with_capture_pipeline() {
        let state = DiagnosticsState::default();
        state.begin_native_startup(9, "system-picker", Some(30), Some("P1080".to_string()));
        state.record_native_startup_stage(
            9,
            NativeStartupStageKind::FirstFrameTimeout,
            None,
            None,
            Some("timed out".to_string()),
        );
        assert_eq!(state.snapshot().native_startup[0].outcome, "capture-failed");
        state.reset_capture_pipeline(9);
        assert!(state.snapshot().native_startup.is_empty());
    }

    #[test]
    fn native_startup_timeline_is_bounded_and_keeps_start_anchor() {
        let state = DiagnosticsState::default();
        state.begin_native_startup(77, "window", Some(30), Some("Auto".to_string()));
        for i in 0..100 {
            state.record_native_startup_stage(
                77,
                NativeStartupStageKind::SnapshotPullStarted,
                None,
                None,
                Some(format!("pull {i}")),
            );
        }

        let snapshot = state.snapshot();
        let stages = &snapshot.native_startup[0].stages;
        assert_eq!(stages.len(), NATIVE_STARTUP_MAX_STAGES);
        assert_eq!(stages[0].stage, NativeStartupStageKind::StartRequested);
        assert_eq!(
            stages.last().and_then(|stage| stage.detail.as_deref()),
            Some("pull 99")
        );
    }

    #[test]
    fn native_startup_timeline_clears_on_stop_cleanup() {
        let state = DiagnosticsState::default();
        state.begin_native_startup(88, "window", Some(30), Some("Auto".to_string()));
        assert_eq!(state.snapshot().native_startup.len(), 1);
        state.clear_native_startup(88);
        assert!(state.snapshot().native_startup.is_empty());
    }

    #[test]
    fn simulcast_primary_layer_prefers_larger_active_over_smaller_active() {
        // Reproduces the reported bug: a Full-tier share publishes a full-res
        // layer and a throttled half-res layer; the full layer must win
        // regardless of which report is read first.
        assert!(is_more_primary_layer(
            true, true, 1_282_988, true, 356_580
        ));
        assert!(!is_more_primary_layer(
            true, true, 356_580, true, 1_282_988
        ));
    }

    #[test]
    fn simulcast_primary_layer_prefers_active_over_larger_inactive() {
        // An active-but-smaller layer beats an inactive-but-larger one -- a
        // stale/paused layer's report should never win just by being bigger.
        assert!(is_more_primary_layer(
            true, true, 100, false, 1_000_000
        ));
        assert!(!is_more_primary_layer(
            true, false, 1_000_000, true, 100
        ));
    }

    #[test]
    fn simulcast_primary_layer_accepts_first_candidate_unconditionally() {
        // No primary selected yet: the first report seen is always accepted,
        // so a single non-simulcast layer (or an all-inactive-so-far report
        // during share startup) still populates the debug panel.
        assert!(is_more_primary_layer(false, false, 0, false, 0));
        assert!(is_more_primary_layer(false, true, 500, false, 0));
    }

    #[test]
    fn simulcast_primary_layer_is_stable_for_equal_candidates() {
        // Equal active-ness and equal area: not "more primary" -- keeps the
        // first one seen instead of flip-flopping every tick.
        assert!(!is_more_primary_layer(true, true, 500, true, 500));
        assert!(!is_more_primary_layer(true, false, 0, false, 0));
    }

    fn inbound_video_layer(
        width: u32,
        height: u32,
        sdk_smoothed_fps: f64,
        frames_decoded: u32,
    ) -> livekit::webrtc::stats::InboundRtpStats {
        use livekit::webrtc::stats::{
            dictionaries::{InboundRtpStreamStats, RtpStreamStats},
            InboundRtpStats,
        };

        InboundRtpStats {
            stream: RtpStreamStats {
                kind: "video".to_string(),
                ..Default::default()
            },
            inbound: InboundRtpStreamStats {
                frame_width: width,
                frame_height: height,
                frames_per_second: sdk_smoothed_fps,
                frames_decoded,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn inbound_simulcast_uses_the_primary_layer_for_decoder_rate() {
        let primary = inbound_video_layer(1920, 1080, 30.0, 300);
        // The lower layer arrives last, reproducing last-write-wins before
        // primary-layer reduction was applied to the receive path.
        let lower = inbound_video_layer(960, 540, 12.0, 120);
        let mut health = TrackHealth::default();
        let mut have_primary = false;
        let mut primary_area = 0;
        apply_primary_inbound_layer(&mut health, &primary, &mut have_primary, &mut primary_area);
        apply_primary_inbound_layer(&mut health, &lower, &mut have_primary, &mut primary_area);

        assert_eq!(
            (health.width, health.height, health.fps),
            (1920, 1080, 30.0)
        );
        let mut previous = HashMap::from([("frames".to_string(), 270)]);
        assert_eq!(
            rate_per_second(
                &mut previous,
                "frames",
                u64::from(health.frames_decoded),
                Some(1000)
            ),
            Some(30.0)
        );
    }

    #[test]
    fn history_ring_buffer_is_bounded() {
        let mut buf: VecDeque<u32> = VecDeque::new();
        for i in 0..(HISTORY_CAP as u32 + 50) {
            push_bounded(&mut buf, i, HISTORY_CAP);
        }
        assert_eq!(buf.len(), HISTORY_CAP);
        // Oldest entries evicted, newest kept.
        assert_eq!(*buf.front().unwrap(), 50);
        assert_eq!(*buf.back().unwrap(), HISTORY_CAP as u32 + 49);
    }

    #[test]
    fn journal_cap_matches_issue_sizing() {
        assert_eq!(JOURNAL_CAP, 500);
        assert_eq!(HISTORY_CAP, 120);
    }

    #[test]
    fn rate_kbps_needs_two_readings_and_a_dt() {
        let mut prev = HashMap::new();
        // First reading: no previous counter -> 0.
        assert_eq!(rate_kbps(&mut prev, "t:send", 1000, None), 0.0);
        // Second reading 1s later, +125000 bytes = 1000 kbit over 1000 ms.
        let kbps = rate_kbps(&mut prev, "t:send", 126_000, Some(1000));
        assert!((kbps - 1000.0).abs() < 1e-9, "got {kbps}");
    }

    #[test]
    fn rate_kbps_clamps_counter_resets_to_zero() {
        let mut prev = HashMap::new();
        assert_eq!(rate_kbps(&mut prev, "t:send", 5000, Some(1000)), 0.0);
        // Counter went BACKWARDS (track republished, stats reset) -> 0, not
        // a wrapped huge value.
        assert_eq!(rate_kbps(&mut prev, "t:send", 100, Some(1000)), 0.0);
        // And recovers on the next normal delta.
        let kbps = rate_kbps(&mut prev, "t:send", 12_600, Some(1000));
        assert!((kbps - 100.0).abs() < 1e-9, "got {kbps}");
    }

    #[test]
    fn counter_rate_distinguishes_unmeasured_from_measured_zero() {
        let mut prev = HashMap::new();
        assert_eq!(rate_per_second(&mut prev, "frames", 10, Some(1000)), None);
        assert_eq!(
            rate_per_second(&mut prev, "frames", 10, Some(1000)),
            Some(0.0)
        );
    }

    #[test]
    fn counter_reset_is_unknown_and_does_not_debounce_as_a_stall() {
        let key = "alice:petal-window-1";
        let mut previous = HashMap::from([(key.to_string(), 100)]);
        let reset_rate = rate_per_second(&mut previous, key, 4, Some(1000));
        assert_eq!(reset_rate, None);

        let mut debounce = StallDebounce::default();
        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        // A resubscribe reset is not a measured zero, so it clears the
        // tentative stall rather than becoming the fifth zero-progress tick.
        assert_eq!(debounce.gate(key, "stalled", reset_rate), None);
        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        assert_eq!(
            debounce.gate(key, "stalled", Some(0.0)),
            Some("stalled".to_string())
        );
    }

    #[test]
    fn capture_sequence_sampler_reports_zero_when_frames_stop() {
        let configured_fps = 30_u32;
        let mut sampler = CapturePipelineSampler::default();

        sampler.record_frame(42, 1, 1280, 720, test_capture_state());
        let first = sampler.sample_stage(42, 0).unwrap();
        assert_eq!(first.stage.fps, None);
        assert_eq!(first.state.fps, None);

        sampler.record_frame(42, 31, 1280, 720, test_capture_state());
        let flowing = sampler.sample_stage(42, 1000).unwrap();
        assert!(
            (flowing.stage.fps.unwrap() - 30.0).abs() < 0.001,
            "got {:?}",
            flowing.stage.fps
        );
        assert_eq!(flowing.state.fps, flowing.stage.fps);

        let stopped = sampler.sample_stage(42, 2000).unwrap();
        assert_eq!(configured_fps, 30);
        assert_eq!(stopped.stage.fps, Some(0.0));
        assert_eq!(stopped.state.fps, Some(0.0));
    }

    // macOS-only: the snapshot type comes from the native compositor.
    #[cfg(target_os = "macos")]
    #[test]
    fn display_enqueue_sampler_distinguishes_absent_first_and_stalled_samples() {
        let mut sampler = DisplayPipelineSampler::default();
        let empty = crate::compositor::DisplayEnqueueSnapshot {
            source_pixel_width: Some(1280),
            source_pixel_height: Some(720),
            last_display_enqueued_ms: None,
            frames_display_enqueued: 0,
            frames_received: 0,
        };

        assert_eq!(sampler.sample_stage("alice:7", empty, 1000), None);

        let first = crate::compositor::DisplayEnqueueSnapshot {
            last_display_enqueued_ms: Some(1000),
            frames_display_enqueued: 12,
            ..empty
        };
        assert_eq!(
            sampler.sample_stage("alice:7", first, 1000),
            Some(PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: None,
                kbps: None,
            })
        );

        let flowing = crate::compositor::DisplayEnqueueSnapshot {
            last_display_enqueued_ms: Some(2000),
            frames_display_enqueued: 42,
            ..empty
        };
        assert_eq!(
            sampler.sample_stage("alice:7", flowing, 2000).unwrap().fps,
            Some(30.0)
        );

        let stalled = crate::compositor::DisplayEnqueueSnapshot {
            last_display_enqueued_ms: Some(3000),
            frames_display_enqueued: 42,
            ..empty
        };
        assert_eq!(
            sampler.sample_stage("alice:7", stalled, 3000).unwrap().fps,
            Some(0.0)
        );

        let reset = crate::compositor::DisplayEnqueueSnapshot {
            last_display_enqueued_ms: Some(4000),
            frames_display_enqueued: 4,
            ..empty
        };
        assert_eq!(
            sampler.sample_stage("alice:7", reset, 4000).unwrap().fps,
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn display_enqueue_sampler_measures_drops_over_real_poll_cadence() {
        let mut sampler = DisplayPipelineSampler::default();
        let first = crate::compositor::DisplayEnqueueSnapshot {
            source_pixel_width: Some(1280),
            source_pixel_height: Some(720),
            last_display_enqueued_ms: Some(0),
            frames_display_enqueued: 0,
            frames_received: 1,
        };
        sampler.sample_stage("alice:7", first, 0);
        assert_eq!(sampler.drop_pct("alice:7"), None);
        for second in 1..=4 {
            let snapshot = crate::compositor::DisplayEnqueueSnapshot {
                last_display_enqueued_ms: Some(second * 1000),
                frames_display_enqueued: 0,
                frames_received: second + 1,
                ..first
            };
            assert!(sampler
                .sample_stage("alice:7", snapshot, second * 1000)
                .is_some());
            assert_eq!(sampler.drop_pct("alice:7"), None);
        }
        let fifth = crate::compositor::DisplayEnqueueSnapshot {
            last_display_enqueued_ms: Some(5000),
            frames_display_enqueued: 0,
            frames_received: 6,
            ..first
        };
        assert!(sampler.sample_stage("alice:7", fifth, 5000).is_some());
        assert_eq!(sampler.drop_pct("alice:7"), Some(100.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn display_enqueue_sampler_bumps_the_window_seq_only_when_a_window_closes() {
        // #882 review: the seq is what lets the backoff tell a fresh 5s
        // window from the ~1s tick re-reading a stale one. It must advance
        // exactly once per CLOSED window, not per poll.
        let mut sampler = DisplayPipelineSampler::default();
        let base = crate::compositor::DisplayEnqueueSnapshot {
            source_pixel_width: Some(1280),
            source_pixel_height: Some(720),
            last_display_enqueued_ms: Some(0),
            frames_display_enqueued: 0,
            frames_received: 1,
        };
        sampler.sample_stage("alice:7", base, 0);
        assert_eq!(sampler.drop_sample("alice:7"), None);
        // First window closes at t=5000.
        let w1 = crate::compositor::DisplayEnqueueSnapshot {
            frames_received: 10,
            ..base
        };
        sampler.sample_stage("alice:7", w1, 5000);
        let (_, seq1) = sampler.drop_sample("alice:7").expect("window closed");
        // Intra-window polls (t=6000, t=7000) must NOT advance the seq.
        for t in [6000, 7000] {
            let poll = crate::compositor::DisplayEnqueueSnapshot {
                frames_received: 12,
                ..base
            };
            sampler.sample_stage("alice:7", poll, t);
            let (_, seq) = sampler.drop_sample("alice:7").unwrap();
            assert_eq!(seq, seq1, "seq must hold steady between window closes");
        }
        // Second window closes at t=10000 -> fresh seq.
        let w2 = crate::compositor::DisplayEnqueueSnapshot {
            frames_received: 20,
            ..base
        };
        sampler.sample_stage("alice:7", w2, 10_000);
        let (_, seq2) = sampler.drop_sample("alice:7").unwrap();
        assert_ne!(seq2, seq1, "a closed window must carry a fresh seq");
    }

    // #878: a sustained >30% display-enqueue drop rate must re-warn on a
    // cadence, not just once per episode, so the log carries a curve.
    #[test]
    fn display_drop_rewarn_allowed_second_report_after_window() {
        let start = std::time::Instant::now();
        let later = start + DISPLAY_DROP_REWARN_INTERVAL;
        assert!(display_drop_rewarn_allowed(Some(start), later));
    }

    #[test]
    fn display_drop_rewarn_not_allowed_within_window() {
        let start = std::time::Instant::now();
        let soon = start + std::time::Duration::from_secs(1);
        assert!(!display_drop_rewarn_allowed(Some(start), soon));
    }

    #[test]
    fn display_drop_rewarn_allowed_with_no_prior_warning() {
        assert!(display_drop_rewarn_allowed(None, std::time::Instant::now()));
    }

    // #878 Phase 2 item 2 / #882 review: enqueue backoff decision
    // boundaries. Every call passes an explicit window seq -- distinct seqs
    // model fresh 5s windows, a repeated seq models the ~1s stats tick
    // re-reading a window that has not closed yet.
    #[test]
    fn enqueue_backoff_stays_below_threshold_never_pauses() {
        let mut state = EnqueueBackoffTrackState::default();
        let now = std::time::Instant::now();
        for seq in 0..10 {
            assert_eq!(
                enqueue_backoff_decide(&mut state, 79.9, seq, now),
                EnqueueBackoffAction::None
            );
        }
    }

    #[test]
    fn enqueue_backoff_pauses_after_three_consecutive_fresh_windows() {
        let mut state = EnqueueBackoffTrackState::default();
        let now = std::time::Instant::now();
        assert_eq!(
            enqueue_backoff_decide(&mut state, 80.0, 0, now),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 95.0, 1, now),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 100.0, 2, now),
            EnqueueBackoffAction::Pause
        );
        assert!(state.paused);
    }

    #[test]
    fn enqueue_backoff_stale_window_rereads_are_not_evidence() {
        // #882 review defect A: the stats poller ticks ~1s but the sampler
        // closes a window every 5s. Re-reading ONE high window on three
        // consecutive ticks must not trip the 3-window threshold.
        let mut state = EnqueueBackoffTrackState::default();
        let now = std::time::Instant::now();
        for _ in 0..10 {
            assert_eq!(
                enqueue_backoff_decide(&mut state, 100.0, 7, now),
                EnqueueBackoffAction::None,
                "the same window seq re-read must never accumulate strikes"
            );
        }
        // Two more genuinely fresh high windows complete the streak.
        assert_eq!(
            enqueue_backoff_decide(&mut state, 100.0, 8, now),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 100.0, 9, now),
            EnqueueBackoffAction::Pause
        );
    }

    #[test]
    fn enqueue_backoff_consecutive_count_resets_on_a_good_window() {
        let mut state = EnqueueBackoffTrackState::default();
        let now = std::time::Instant::now();
        assert_eq!(
            enqueue_backoff_decide(&mut state, 90.0, 0, now),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 90.0, 1, now),
            EnqueueBackoffAction::None
        );
        // A single good window resets the streak -- two highs then a good
        // window must not carry over toward the next pause.
        assert_eq!(
            enqueue_backoff_decide(&mut state, 10.0, 2, now),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 90.0, 3, now),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 90.0, 4, now),
            EnqueueBackoffAction::None,
            "streak must have restarted after the reset, not carried the earlier two"
        );
    }

    #[test]
    fn enqueue_backoff_pause_is_time_boxed_and_ignores_the_poisoned_metric() {
        // #882 review defect B: while paused, every window reads ~100% drop
        // by construction (push_frame counts received, enqueues nothing), so
        // recovery is unobservable and the pause must end on TIME, never on
        // the metric. Before expiry no drop value -- however low -- may
        // resume it (a low value while paused would itself be an artifact).
        let mut state = EnqueueBackoffTrackState::default();
        let start = std::time::Instant::now();
        for seq in 0..3 {
            enqueue_backoff_decide(&mut state, 95.0, seq, start);
        }
        assert!(state.paused);
        let before_expiry =
            start + ENQUEUE_BACKOFF_PAUSE_DURATION - std::time::Duration::from_millis(1);
        assert_eq!(
            enqueue_backoff_decide(&mut state, 0.0, 3, before_expiry),
            EnqueueBackoffAction::None,
            "no metric value may resume a pause early -- the metric measures the pause itself"
        );
        let at_expiry = start + ENQUEUE_BACKOFF_PAUSE_DURATION;
        assert_eq!(
            enqueue_backoff_decide(&mut state, 100.0, 4, at_expiry),
            EnqueueBackoffAction::Resume,
            "the pause must expire on schedule regardless of the (unreadable) drop rate"
        );
        assert!(!state.paused);
    }

    #[test]
    fn enqueue_backoff_discards_the_first_window_after_resume() {
        // The window in flight when enqueue resumes spans paused time and
        // reads artificially high -- counting it as strike 1 biases the
        // next trip. It must be consumed without counting.
        let mut state = EnqueueBackoffTrackState::default();
        let start = std::time::Instant::now();
        for seq in 0..3 {
            enqueue_backoff_decide(&mut state, 95.0, seq, start);
        }
        let after = start + ENQUEUE_BACKOFF_PAUSE_DURATION;
        assert_eq!(
            enqueue_backoff_decide(&mut state, 100.0, 3, after),
            EnqueueBackoffAction::Resume
        );
        // Post-resume: poisoned window (skipped), then three genuine highs.
        assert_eq!(
            enqueue_backoff_decide(&mut state, 100.0, 4, after),
            EnqueueBackoffAction::None,
            "first post-resume window is pause residue, not evidence"
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 95.0, 5, after),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 95.0, 6, after),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            enqueue_backoff_decide(&mut state, 95.0, 7, after),
            EnqueueBackoffAction::Pause,
            "three genuine post-resume windows must still re-trip"
        );
    }

    #[test]
    fn sleep_paused_windows_never_accumulate_toward_a_backoff_pause() {
        // #878 adversarial-review finding 1: a sleeping display reads 100%
        // drop by construction. Those windows must reset, not accumulate --
        // otherwise three sleep windows trip a pause that self-sustains
        // after wake.
        let now = std::time::Instant::now();
        let mut state = EnqueueBackoffTrackState::default();
        for seq in 0..5 {
            assert_eq!(
                apply_enqueue_backoff(&mut state, 100.0, seq, now, true, false),
                EnqueueBackoffAction::None
            );
        }
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 5, now, false, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 6, now, false, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 7, now, false, false),
            EnqueueBackoffAction::Pause
        );
    }

    #[test]
    fn sleep_pause_resets_progress_made_while_awake() {
        // Two awake strikes, then sleep: the streak must not survive the
        // sleep window and complete on the first awake window after wake.
        let now = std::time::Instant::now();
        let mut state = EnqueueBackoffTrackState::default();
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 0, now, false, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 1, now, false, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 100.0, 2, now, true, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 3, now, false, false),
            EnqueueBackoffAction::None
        );
    }

    #[test]
    fn another_tracks_global_pause_never_feeds_this_tracks_streak() {
        // #882 review: the pause flag is global, so while track A holds it
        // track B's windows read ~100% -- track B accumulating those
        // poisoned strikes would re-trip a fresh global pause the moment A
        // resumes, chaining pauses forever. B must discard evidence while
        // the global pause is held AND skip its first window after resume.
        let now = std::time::Instant::now();
        let mut state = EnqueueBackoffTrackState::default();
        // Global pause held by another track: three poisoned windows.
        for seq in 0..3 {
            assert_eq!(
                apply_enqueue_backoff(&mut state, 100.0, seq, now, false, true),
                EnqueueBackoffAction::None
            );
        }
        assert!(!state.paused, "poisoned windows must not trip this track");
        // Global resume: the first window still spans paused time.
        assert_eq!(
            apply_enqueue_backoff(&mut state, 100.0, 3, now, false, false),
            EnqueueBackoffAction::None,
            "first window after the global resume is pause residue"
        );
        // Genuine post-resume distress must still trip after 3 windows.
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 4, now, false, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 5, now, false, false),
            EnqueueBackoffAction::None
        );
        assert_eq!(
            apply_enqueue_backoff(&mut state, 95.0, 6, now, false, false),
            EnqueueBackoffAction::Pause
        );
    }

    #[test]
    fn a_paused_tracks_expiry_still_runs_under_the_global_flag() {
        // The track that OWNS the pause sees global_backoff_paused=true too;
        // that must not divert it from its own expiry check, or the global
        // flag never clears.
        let start = std::time::Instant::now();
        let mut state = EnqueueBackoffTrackState::default();
        for seq in 0..3 {
            apply_enqueue_backoff(&mut state, 95.0, seq, start, false, false);
        }
        assert!(state.paused);
        let at_expiry = start + ENQUEUE_BACKOFF_PAUSE_DURATION;
        assert_eq!(
            apply_enqueue_backoff(&mut state, 100.0, 3, at_expiry, false, true),
            EnqueueBackoffAction::Resume,
            "the owning track must resume at expiry even while the global flag is up"
        );
    }

    #[test]
    fn host_from_url_strips_scheme_path_and_query() {
        assert_eq!(
            host_from_url("ws://localhost:7880"),
            Some("localhost:7880".into())
        );
        assert_eq!(
            host_from_url("wss://petal.livekit.cloud/rtc?token=abc"),
            Some("petal.livekit.cloud".into())
        );
        assert_eq!(host_from_url("not-a-url"), None);
        assert_eq!(host_from_url("wss://"), None);
    }

    #[test]
    fn describe_track_covers_window_camera_and_other_shapes() {
        assert_eq!(describe_track("petal-window-4242"), "window 4242 share");
        // The assistant's voice must read as itself, not as an unknown track.
        #[cfg(target_os = "macos")]
        assert_eq!(
            describe_track("petal-ai-window-4242"),
            "AI assistant voice"
        );
        assert_eq!(describe_track("petal-camera-web-tester"), "webcam");
        assert_eq!(describe_track("microphone"), "'microphone'");
        assert_eq!(describe_track(""), "a track");
    }

    #[test]
    fn display_of_falls_back_to_identity() {
        assert_eq!(display_of("Sana", "sana-1"), "Sana");
        assert_eq!(display_of("  ", "sana-1"), "sana-1");
    }

    #[test]
    fn recent_samples_returns_only_the_last_ten_samples() {
        let history = (0..15)
            .map(|i| StatsSample {
                t_ms: i,
                ..Default::default()
            })
            .collect::<Vec<_>>();

        let recent = recent_samples(&history);

        assert_eq!(recent.len(), 10);
        assert_eq!(recent.first().unwrap().t_ms, 5);
        assert_eq!(recent.last().unwrap().t_ms, 14);
    }

    #[test]
    fn avg_present_ignores_missing_values_and_empty_input() {
        let avg = avg_present([Some(10.0), None, Some(20.0), Some(30.0)].into_iter());
        assert_eq!(avg, Some(20.0));

        let empty = avg_present([None::<f64>, None].into_iter());
        assert_eq!(empty, None);
    }

    #[test]
    fn stall_debounce_requires_five_consecutive_zero_decoded_ticks() {
        let mut debounce = StallDebounce::default();
        let key = "alice:petal-window-1";

        // Four consecutive zero-progress "stalled" ticks: still suppressed.
        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        // The 5th consecutive zero-progress tick crosses the threshold.
        assert_eq!(
            debounce.gate(key, "stalled", Some(0.0)),
            Some("stalled".to_string())
        );
        // It stays "stalled" as long as decoded frames keep making zero
        // progress.
        assert_eq!(
            debounce.gate(key, "stalled", Some(0.0)),
            Some("stalled".to_string())
        );
    }

    #[test]
    fn stall_debounce_healthy_tick_resets_the_counter() {
        let mut debounce = StallDebounce::default();
        let key = "alice:petal-window-1";

        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        // A tick where frames_decoded actually advanced resets the counter,
        // even though the raw fps/kbps gauge still classified it "stalled"
        // (the exact gauge-lag scenario #358 calls out).
        assert_eq!(debounce.gate(key, "stalled", Some(2.0)), None);

        // Four more zero-progress ticks after the reset are still not
        // enough -- confirms the counter actually went back to zero, not
        // just failed to advance.
        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        assert_eq!(
            debounce.gate(key, "stalled", Some(0.0)),
            Some("stalled".to_string())
        );
    }

    #[test]
    fn stall_debounce_missing_decoded_baseline_does_not_count() {
        let mut debounce = StallDebounce::default();
        let key = "alice:petal-window-1";

        // A brand-new subscription with no frames_decoded baseline yet
        // (decoder_fps = None) must not start counting toward a stall, even
        // if the raw gauge already says "stalled".
        for _ in 0..10 {
            assert_eq!(debounce.gate(key, "stalled", None), None);
        }
    }

    #[test]
    fn stall_debounce_resumed_is_instant_no_debounce() {
        let mut debounce = StallDebounce::default();
        let key = "alice:petal-window-1";

        // Get right up to the edge of the threshold.
        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        // Recovery ("active") is reported the very same tick it happens,
        // with no debounce delay in that direction.
        assert_eq!(
            debounce.gate(key, "active", Some(30.0)),
            Some("active".to_string())
        );

        // And a subsequent stall has to start counting from zero again.
        for _ in 0..4 {
            assert_eq!(debounce.gate(key, "stalled", Some(0.0)), None);
        }
        assert_eq!(
            debounce.gate(key, "stalled", Some(0.0)),
            Some("stalled".to_string())
        );
    }

    #[test]
    fn stall_debounce_tracks_multiple_keys_independently() {
        let mut debounce = StallDebounce::default();
        let a = "alice:petal-window-1";
        let b = "bob:petal-window-2";

        for _ in 0..4 {
            assert_eq!(debounce.gate(a, "stalled", Some(0.0)), None);
        }
        // `b`'s own counter is independent and starts fresh.
        assert_eq!(debounce.gate(b, "stalled", Some(0.0)), None);
        assert_eq!(
            debounce.gate(a, "stalled", Some(0.0)),
            Some("stalled".to_string())
        );
    }

    #[test]
    fn corrected_latency_applies_sender_to_receiver_clock_offset() {
        // Sender clock is 25ms ahead of receiver. A frame captured at sender
        // 1_000_000us is receiver 975_000us; receiving it at 1_040_000us is
        // therefore 65ms glass-to-glass, not the naive 40ms subtraction.
        assert_eq!(
            corrected_latency_ms(1_000_000, 1_040_000, -25_000),
            Some(65.0)
        );
        assert_eq!(corrected_latency_ms(1_000_000, 900_000, 0), None);
    }

    #[test]
    fn calibrated_clock_offset_is_required_for_measured_latency_overlay() {
        let state = DiagnosticsState::default();
        let key = latency_key("remote", "petal-window-7");

        assert!(state
            .corrected_glass_to_glass_ms("remote", 1_000_000, 1_040_000)
            .is_none());

        let mut tracks = [TrackHealth {
            latency_key: key.clone(),
            owner_identity: Some("remote".into()),
            name: "petal-window-7".into(),
            direction: "recv".into(),
            kind: "video".into(),
            ..Default::default()
        }];
        state.apply_track_overlays(&mut tracks, None);
        assert_eq!(tracks[0].glass_to_glass_status, "clock-sync-pending");
        assert_eq!(tracks[0].glass_to_glass_ms, None);

        state.record_peer_clock_offset("remote".into(), -25_000, 20.0);
        let latency = state
            .corrected_glass_to_glass_ms("remote", 1_000_000, 1_040_000)
            .unwrap();
        assert!((latency - 65.0).abs() < 1e-9);
    }

    #[test]
    fn analysis_uses_recent_metric_window_not_stale_history() {
        let mut history = vec![
            StatsSample {
                rtt_ms: Some(500.0),
                jitter_ms: Some(100.0),
                loss_pct: Some(20.0),
                ..Default::default()
            };
            5
        ];
        history.extend(vec![
            StatsSample {
                rtt_ms: Some(20.0),
                jitter_ms: Some(2.0),
                loss_pct: Some(0.0),
                ..Default::default()
            };
            10
        ]);

        let findings = analyze_conditions(&history, &[], 0, &[]);
        let titles: Vec<_> = findings.iter().map(|f| f.title.as_str()).collect();

        assert!(!titles.contains(&"High latency to the media server"));
        assert!(!titles.contains(&"Unstable network timing"));
        assert!(!titles.contains(&"Packet loss is degrading media"));
        assert_eq!(titles, vec!["No network bottleneck detected"]);
    }

    #[test]
    fn stats_stalled_overlay_does_not_mask_current_active_tick() {
        let state = DiagnosticsState::default();
        let key = latency_key("remote", "petal-window-7");
        {
            let mut inner = state.lock();
            inner.stream_states.insert(
                key.clone(),
                StreamStateObservation {
                    state: "stalled".into(),
                    source: "stats-frame-starvation".into(),
                },
            );
        }

        let mut tracks = [TrackHealth {
            latency_key: key,
            name: "remote window".into(),
            direction: "recv".into(),
            kind: "video".into(),
            stream_state: "active".into(),
            ..Default::default()
        }];

        state.apply_track_overlays(&mut tracks, None);

        assert_eq!(tracks[0].stream_state, "active");
        assert!(tracks[0].quality_limitation.is_empty());
    }

    #[test]
    fn exact_js_stream_state_overlay_overrides_current_stats_tick() {
        let state = DiagnosticsState::default();
        let key = latency_key("remote", "petal-camera-remote");
        {
            let mut inner = state.lock();
            inner.stream_states.insert(
                key.clone(),
                StreamStateObservation {
                    state: "paused".into(),
                    source: "livekit-js-stream-state".into(),
                },
            );
        }

        let mut tracks = [TrackHealth {
            latency_key: key,
            name: "remote camera".into(),
            direction: "recv".into(),
            kind: "video".into(),
            stream_state: "active".into(),
            ..Default::default()
        }];

        state.apply_track_overlays(&mut tracks, None);

        assert_eq!(tracks[0].stream_state, "paused");
        assert_eq!(tracks[0].quality_limitation, "livekit-js-stream-state");
    }

    #[test]
    fn stats_sample_round_trips_the_683_memory_fields() {
        // #683: `phys_footprint_mb`/`live_pixel_buffers` ride `StatsSample`
        // through the existing `get_network_snapshot` command -- confirm
        // both the `Some` and honest-`None` shapes actually reach JSON with
        // the camelCase names the TS `StatsSample` type expects.
        let present = StatsSample {
            t_ms: 1,
            phys_footprint_mb: Some(487),
            live_pixel_buffers: Some(3),
            ..Default::default()
        };
        let json = serde_json::to_value(&present).unwrap();
        assert_eq!(json.get("physFootprintMb").unwrap(), &serde_json::json!(487));
        assert_eq!(json.get("livePixelBuffers").unwrap(), &serde_json::json!(3));

        let absent = StatsSample {
            t_ms: 2,
            phys_footprint_mb: None,
            live_pixel_buffers: None,
            ..Default::default()
        };
        let json = serde_json::to_value(&absent).unwrap();
        assert!(json.get("physFootprintMb").unwrap().is_null());
        assert!(json.get("livePixelBuffers").unwrap().is_null());
    }

    #[test]
    fn snapshot_and_journal_serialize_camel_case() {
        let sample = StatsSample {
            t_ms: 1,
            rtt_ms: Some(42.0),
            ..Default::default()
        };
        let json = serde_json::to_value(&sample).unwrap();
        assert!(json.get("tMs").is_some());
        assert!(json.get("rttMs").is_some());
        assert!(json.get("sendKbps").is_some());
        assert!(json.get("lossPct").is_some());

        let entry = JournalEntry {
            t_ms: 2,
            category: "shares".into(),
            message: "m".into(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("tMs").is_some());
        assert!(json.get("category").is_some());

        let snap = NetworkSnapshot::default();
        let json = serde_json::to_value(&snap).unwrap();
        assert!(json.get("serverHost").is_some());
        assert!(json.get("reconnectCount").is_some());
        assert!(json.get("localIdentity").is_some());
        assert!(json.get("peerRttMs").is_some());
        assert!(json.get("analysis").is_some());
    }

    #[test]
    fn peer_rtt_records_latest_finite_value() {
        let state = DiagnosticsState::default();
        assert_eq!(state.snapshot().peer_rtt_ms, None);

        state.record_peer_rtt(24.5);
        assert_eq!(state.snapshot().peer_rtt_ms, Some(24.5));

        state.record_peer_rtt(f64::NAN);
        assert_eq!(state.snapshot().peer_rtt_ms, Some(24.5));
    }

    #[test]
    fn track_health_serializes_keyframe_and_rtcp_counters() {
        let track = TrackHealth {
            name: "petal-window-7".into(),
            raw_track_name: Some("petal-window-7".into()),
            owner_identity: Some("remote".into()),
            window_id: Some(7),
            direction: "recv".into(),
            kind: "video".into(),
            frames_encoded: 10,
            key_frames_encoded: 2,
            frames_decoded: 9,
            key_frames_decoded: 1,
            nack_count: 4,
            fir_count: 3,
            pli_count: 5,
            ..Default::default()
        };
        let json = serde_json::to_value(&track).unwrap();

        assert!(json.get("latencyKey").is_none());
        assert_eq!(json["rawTrackName"], "petal-window-7");
        assert_eq!(json["ownerIdentity"], "remote");
        assert_eq!(json["windowId"], 7);
        assert_eq!(json["framesEncoded"], 10);
        assert_eq!(json["keyFramesEncoded"], 2);
        assert_eq!(json["framesDecoded"], 9);
        assert_eq!(json["keyFramesDecoded"], 1);
        assert_eq!(json["nackCount"], 4);
        assert_eq!(json["firCount"], 3);
        assert_eq!(json["pliCount"], 5);
    }

    #[test]
    fn cockpit_clock_evidence_is_ordered_bounded_and_identity_free() {
        let state = DiagnosticsState::default();
        let recorded_at = now_ms();
        state.record_peer_clock_offset("peer-a".into(), -25_000, 20.0);

        let fresh = state.clock_calibration_evidence(["peer-a", "peer-missing"]);
        assert_eq!(fresh[0].calibrated, true);
        assert_eq!(fresh[0].uncertainty_ms, Some(10.0));
        assert_eq!(fresh[1].calibrated, false);
        assert_eq!(fresh[1].uncertainty_ms, None);
        assert!(!serde_json::to_string(&fresh).unwrap().contains("peer-a"));

        let stale = state
            .clock_calibration_evidence_at(["peer-a"], recorded_at + CLOCK_OFFSET_STALE_MS + 2);
        assert_eq!(stale[0].calibrated, false);
        assert_eq!(stale[0].uncertainty_ms, None);
    }

    #[test]
    fn absent_pipeline_stage_serializes_as_null() {
        let track = TrackHealth {
            name: "petal-window-7".into(),
            direction: "send".into(),
            kind: "video".into(),
            grabbed: None,
            encoded_sent: None,
            display_enqueued: None,
            ..Default::default()
        };

        let json = serde_json::to_value(&track).unwrap();

        assert!(json["grabbed"].is_null());
        assert!(json["encodedSent"].is_null());
        assert!(json["displayEnqueued"].is_null());
    }

    #[test]
    fn measured_zero_pipeline_stage_does_not_serialize_as_absent() {
        let track = TrackHealth {
            name: "petal-window-7".into(),
            direction: "send".into(),
            kind: "video".into(),
            grabbed: Some(PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: Some(0.0),
                kbps: Some(0.0),
            }),
            display_enqueued: Some(PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: Some(0.0),
                kbps: None,
            }),
            ..Default::default()
        };

        let json = serde_json::to_value(&track).unwrap();

        assert!(json["grabbed"].is_object());
        assert_eq!(json["grabbed"]["width"], 1280);
        assert_eq!(json["grabbed"]["height"], 720);
        assert_eq!(json["grabbed"]["fps"], 0.0);
        assert_eq!(json["grabbed"]["kbps"], 0.0);
        assert_eq!(json["displayEnqueued"]["width"], 1280);
        assert_eq!(json["displayEnqueued"]["height"], 720);
        assert_eq!(json["displayEnqueued"]["fps"], 0.0);
        assert!(json["displayEnqueued"]["kbps"].is_null());
    }

    #[test]
    fn remote_pipeline_reports_merge_into_opposite_side_track_fields() {
        let state = DiagnosticsState::default();
        {
            let mut inner = state.lock();
            inner.local_identity = Some("native-1".into());
        }
        state.record_remote_pipeline_stage(
            "native-1".into(),
            42,
            "web-1".into(),
            Some("TR_current".into()),
            "e1".into(),
            PipelineStageKind::Received,
            PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: Some(29.0),
                kbps: Some(1700.0),
            },
            1000,
        );
        state.record_remote_pipeline_stage(
            "native-1".into(),
            42,
            "web-1".into(),
            Some("TR_current".into()),
            "e1".into(),
            PipelineStageKind::Decoded,
            PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: Some(28.0),
                kbps: None,
            },
            1000,
        );

        let mut tracks = [TrackHealth {
            sid: "TR_current".into(),
            name: "petal-window-42".into(),
            raw_track_name: Some("petal-window-42".into()),
            window_id: Some(42),
            direction: "send".into(),
            kind: "video".into(),
            ..Default::default()
        }];

        state.apply_remote_pipeline_overlays(&mut tracks);

        assert_eq!(
            tracks[0].remote_received.as_ref().unwrap().reporter_id,
            "web-1"
        );
        assert_eq!(
            tracks[0].remote_received.as_ref().unwrap().metrics.kbps,
            Some(1700.0)
        );
        assert_eq!(
            tracks[0].remote_decoded.as_ref().unwrap().metrics.fps,
            Some(28.0)
        );
    }

    #[test]
    fn pipeline_health_reducer_is_epoch_scoped_ordered_and_terminal() {
        use crate::pipeline_stats::PipelineLifecycle;
        let state = DiagnosticsState::default();
        assert!(state.accept_remote_pipeline_observation(
            "owner",
            42,
            "viewer",
            Some("TR"),
            "e1",
            2
        ));
        assert!(!state.accept_remote_pipeline_observation(
            "owner",
            42,
            "viewer",
            Some("TR"),
            "e1",
            1
        ));
        // A new share epoch is independent, even for the same window id.
        assert!(state.accept_remote_pipeline_observation(
            "owner",
            42,
            "viewer",
            Some("TR"),
            "e2",
            1
        ));
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            "e1".into(),
            Some("TR".into()),
            PipelineLifecycle::Unpublished,
            3,
        );
        // Late packets from the terminal epoch cannot resurrect its tile.
        assert!(!state.accept_remote_pipeline_observation(
            "owner",
            42,
            "viewer",
            Some("TR"),
            "e1",
            4
        ));
        assert!(state.accept_remote_pipeline_observation(
            "owner",
            42,
            "viewer",
            Some("TR"),
            "e2",
            2
        ));
    }

    #[test]
    fn pipeline_health_is_cleared_at_room_generation_boundaries() {
        use crate::pipeline_stats::PipelineLifecycle;
        let state = DiagnosticsState::default();
        assert!(state.accept_remote_pipeline_observation(
            "owner",
            42,
            "viewer",
            Some("TR"),
            "e1",
            1
        ));
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            "e1".into(),
            None,
            PipelineLifecycle::Subscribed,
            1,
        );
        let mut inner = state.lock();
        assert_eq!(inner.remote_pipeline_lifecycles.len(), 1);
        clear_remote_pipeline_health(&mut inner);
        assert!(inner.remote_pipeline_lifecycles.is_empty());
        assert!(inner.remote_pipeline_sequences.is_empty());
        assert!(inner.remote_pipeline_terminal.is_empty());
    }

    #[test]
    fn terminal_epoch_cannot_clear_successor_stage_with_same_publication_sid() {
        use crate::pipeline_stats::PipelineLifecycle;
        let state = DiagnosticsState::default();
        for (epoch, fps) in [("e1", 8.0), ("e2", 30.0)] {
            state.record_remote_pipeline_stage(
                "owner".into(),
                42,
                "viewer".into(),
                Some("TR_same".into()),
                epoch.into(),
                PipelineStageKind::Received,
                PipelineStageMetrics {
                    width: Some(1280),
                    height: Some(720),
                    fps: Some(fps),
                    kbps: None,
                },
                1,
            );
        }
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            "e1".into(),
            Some("TR_same".into()),
            PipelineLifecycle::Unpublished,
            2,
        );
        let reports = state.lock().remote_pipeline.clone();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports.values().next().unwrap().metrics.fps, Some(30.0));
    }

    #[test]
    fn lifecycle_overlay_uses_current_publication_sid_not_late_old_terminal() {
        use crate::pipeline_stats::PipelineLifecycle;
        let state = DiagnosticsState::default();
        {
            state.lock().local_identity = Some("owner".into());
        }
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            "e2".into(),
            Some("TR_current".into()),
            PipelineLifecycle::FirstPresented,
            2,
        );
        // Deliberately later receipt: it must not overwrite the current track.
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            "e1".into(),
            Some("TR_old".into()),
            PipelineLifecycle::Unpublished,
            3,
        );
        let mut tracks = [TrackHealth {
            sid: "TR_current".into(),
            name: "petal-window-42".into(),
            window_id: Some(42),
            direction: "send".into(),
            kind: "video".into(),
            ..Default::default()
        }];
        state.apply_remote_pipeline_overlays(&mut tracks);
        assert_eq!(
            tracks[0].remote_lifecycle.as_ref().unwrap().lifecycle,
            "FirstPresented"
        );
    }

    #[test]
    fn sid_only_receiver_lifecycle_binds_to_current_owner_epoch_after_same_sid_restart() {
        use crate::pipeline_stats::PipelineLifecycle;
        let state = DiagnosticsState::default();
        {
            state.lock().local_identity = Some("owner".into());
        }
        state.record_canonical_owner_epoch("owner", 42, Some("TR_same"), "e1", 1);
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            "e1".into(),
            Some("TR_same".into()),
            PipelineLifecycle::Unpublished,
            1,
        );
        state.record_canonical_owner_epoch("owner", 42, Some("TR_same"), "e2", 2);
        // A late owner e1 packet is older than accepted e2 and cannot roll
        // canonical correlation backwards.
        state.record_canonical_owner_epoch("owner", 42, Some("TR_same"), "e1", 1);
        let receiver_epoch = state.canonical_or_provisional_epoch("owner", 42, Some("TR_same"), "");
        assert_eq!(receiver_epoch, "e2");
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            receiver_epoch,
            Some("TR_same".into()),
            PipelineLifecycle::FirstPresented,
            2,
        );
        let mut tracks = [TrackHealth {
            sid: "TR_same".into(),
            name: "petal-window-42".into(),
            window_id: Some(42),
            direction: "send".into(),
            kind: "video".into(),
            ..Default::default()
        }];
        state.apply_remote_pipeline_overlays(&mut tracks);
        assert_eq!(
            tracks[0].remote_lifecycle.as_ref().unwrap().lifecycle,
            "FirstPresented"
        );
    }

    #[test]
    fn owner_epoch_arrival_rekeys_earlier_sid_only_receiver_lifecycle() {
        use crate::pipeline_stats::PipelineLifecycle;
        let state = DiagnosticsState::default();
        {
            state.lock().local_identity = Some("owner".into());
        }
        let provisional = state.canonical_or_provisional_epoch("owner", 42, Some("TR"), "");
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            provisional.clone(),
            Some("TR".into()),
            PipelineLifecycle::Subscribed,
            1,
        );
        state.record_remote_pipeline_lifecycle(
            "owner".into(),
            42,
            "viewer".into(),
            provisional,
            Some("TR".into()),
            PipelineLifecycle::FirstPresented,
            2,
        );
        state.record_canonical_owner_epoch("owner", 42, Some("TR"), "e2", 2);
        let mut tracks = [TrackHealth {
            sid: "TR".into(),
            name: "petal-window-42".into(),
            window_id: Some(42),
            direction: "send".into(),
            kind: "video".into(),
            ..Default::default()
        }];
        state.apply_remote_pipeline_overlays(&mut tracks);
        assert_eq!(
            tracks[0].remote_lifecycle.as_ref().unwrap().lifecycle,
            "FirstPresented"
        );
    }

    #[test]
    fn receiver_frame_gaps_increment_freeze_count_once() {
        let state = DiagnosticsState::default();
        let key = "native-1\u{1f}petal-window-42".to_string();
        state.record_receiver_frame(key.clone(), Some(10));
        state.record_receiver_frame(key.clone(), Some(10));
        state.record_receiver_frame(key.clone(), Some(9));
        state.record_receiver_frame(key.clone(), Some(13));
        state.record_receiver_frame(key.clone(), Some(14));

        let mut tracks = [TrackHealth {
            latency_key: key,
            name: "petal-window-42 (native-1)".into(),
            raw_track_name: Some("petal-window-42".into()),
            owner_identity: Some("native-1".into()),
            window_id: Some(42),
            direction: "recv".into(),
            kind: "video".into(),
            frames_dropped: 3,
            ..Default::default()
        }];

        state.apply_track_overlays(&mut tracks, None);

        let freeze = tracks[0].receiver_freeze.as_ref().unwrap();
        assert_eq!(freeze.freeze_count, 1);
        assert_eq!(freeze.frames_dropped, 3);
    }

    #[test]
    fn analysis_reports_high_latency_jitter_and_loss() {
        let history = vec![
            StatsSample {
                rtt_ms: Some(180.0),
                jitter_ms: Some(42.0),
                loss_pct: Some(3.5),
                ..Default::default()
            };
            10
        ];
        let findings = analyze_conditions(&history, &[], 0, &[]);
        let titles: Vec<_> = findings.iter().map(|f| f.title.as_str()).collect();
        assert!(titles.contains(&"High latency to the media server"));
        assert!(titles.contains(&"Unstable network timing"));
        assert!(titles.contains(&"Packet loss is degrading media"));
    }

    #[test]
    fn analysis_reports_reconnects_and_poor_quality() {
        let findings = analyze_conditions(
            &[],
            &[],
            3,
            &[ParticipantQuality {
                identity: "sana".into(),
                quality: "poor".into(),
            }],
        );
        let titles: Vec<_> = findings.iter().map(|f| f.title.as_str()).collect();
        assert!(titles.contains(&"Connection is flapping"));
        assert!(titles.contains(&"LiveKit reports poor participant quality"));
    }

    #[test]
    fn analysis_reports_track_health_limitations() {
        let tracks = vec![
            TrackHealth {
                name: "petal-window-7".into(),
                direction: "send".into(),
                kind: "video".into(),
                quality_limitation: "cpu".into(),
                target_kbps: 4000.0,
                actual_kbps: 1200.0,
                codec_impl: "OpenH264".into(),
                software_encoder: true,
                ..Default::default()
            },
            TrackHealth {
                name: "remote camera".into(),
                direction: "recv".into(),
                frames_dropped: 42,
                jitter_buffer_ms: Some(120.0),
                ..Default::default()
            },
        ];
        let findings = analyze_conditions(&[], &tracks, 0, &[]);
        let titles: Vec<_> = findings.iter().map(|f| f.title.as_str()).collect();
        assert!(titles.contains(&"This Mac is encode-limited"));
        assert!(titles.contains(&"Hardware encoder is unavailable"));
        assert!(titles.contains(&"Actual send bitrate is below target"));
        assert!(titles.contains(&"Receive buffer is absorbing jitter"));
        assert!(titles.contains(&"Frames are being dropped on receive"));
    }

    #[test]
    fn analysis_reports_healthy_when_history_exists_without_findings() {
        let history = vec![StatsSample {
            rtt_ms: Some(20.0),
            jitter_ms: Some(2.0),
            loss_pct: Some(0.0),
            send_kbps: 900.0,
            recv_kbps: 700.0,
            ..Default::default()
        }];
        let findings = analyze_conditions(&history, &[], 0, &[]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, "info");
        assert_eq!(findings[0].title, "No network bottleneck detected");
    }
    // #884: memory-pressure transition reporting -- upward transitions into
    // warn/critical only; steady state, recovery, and normal are silent.
    #[test]
    fn memory_pressure_transition_reports_only_upward_elevations() {
        assert_eq!(memory_pressure_transition(None, 1), None, "normal first reading is silent");
        assert_eq!(memory_pressure_transition(None, 2), Some(2), "joining mid-pressure reports");
        assert_eq!(memory_pressure_transition(Some(1), 2), Some(2));
        assert_eq!(memory_pressure_transition(Some(2), 4), Some(4));
        assert_eq!(memory_pressure_transition(Some(2), 2), None, "steady warn is silent");
        assert_eq!(memory_pressure_transition(Some(4), 2), None, "recovery is silent");
        assert_eq!(memory_pressure_transition(Some(2), 1), None);
    }

}
