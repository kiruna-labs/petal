use crate::sync_ext::MutexExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::capture::WindowCapture;
use crate::diagnostics::NativeStartupStageKind;
use crate::logging::{
    CaptureLayoutDiagnostic, CaptureLayoutStage, DiagnosticRole, EncoderImplementationClass,
    GeometryBucket, PixelFormatClass, ScaleBucket, SentryDiagnosticEvent, SourceSelectionClass,
};
use crate::share_priority::SharePriority;
use crate::time_util::now_us;
use crate::transport::publisher::{
    CaptureResolution, PostWakeEncoderFallbackRecovery, RoomConnection, ShareQuality,
    SharedSourceKind,
};
use crate::video_color::VideoColorProfile;
use screencapturekit::stream::content_filter::SCContentFilter;

use super::{RoomGeneration, SessionInner, SessionState, ShareSessionError};

/// SPEC.md §4.3: "Concurrent-share cap: 4 windows per user."
pub(crate) const MAX_CONCURRENT_SHARES: usize = 4;
/// Window live-resize can produce a few transitional frames at short-lived
/// sizes. Wait for the same new size to appear repeatedly before paying the
/// LiveKit unpublish/republish cost.
const RESIZE_REPUBLISH_STABLE_FRAMES: u8 = 6;
const CAPTURE_STALL_THRESHOLD_US: u64 = 2_000_000;
const CAPTURE_WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const CAPTURE_HEARTBEAT_INTERVAL_US: u64 = 5_000_000;
const PUMP_STALL_THRESHOLD_US: u64 = 6_000_000;
/// Secondary, much longer-threshold safety net (see issue #60): there is no
/// API-level signal that distinguishes "ScreenCaptureKit healthy, content
/// genuinely never changed" from "the stream silently died with no error" --
/// both look identical from here (zero raw frame callbacks). A short
/// threshold (like `PUMP_STALL_THRESHOLD_US`) false-positives on legitimately
/// idle shared windows (confirmed live -- see the pump-liveness fix this
/// constant sits next to). A long threshold is a pragmatic compromise: real
/// collaborative screen-sharing essentially never has zero visual change
/// (cursor blink, hover states, overlapping windows) for a full 45s, so this
/// rarely misfires on idle content while still eventually self-healing a
/// genuinely wedged stream, confirmed to recur independently of window
/// content (frame pump silent 50s+ on a freshly shared, freshly launched
/// TextEdit window during live validation).
const RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US: u64 = 45_000_000;
/// Absolute safety-net silence before we restart even a source that looks
/// idle/static. Long, because an idle window legitimately produces no frames
/// for minutes and restarting it just republishes the track for nothing (the
/// old 45s cadence caused a 336×-restart storm in one session). A genuinely
/// wedged stream on static content shows the correct last frame until this
/// fires. #capture-idle-restart-loop.
const RAW_CAPTURE_HARD_RESTART_THRESHOLD_US: u64 = 300_000_000;
/// How long the Stalled arm's "snapshot pulls are fresh, don't churn a
/// restart" tolerance is allowed to hold, once a NON-idle stall (the
/// genuinely-wedged path, not the idle absolute-backstop path) has already
/// fired at 45s. Live testing 2026-07-14 found this hold was otherwise a
/// one-way ratchet -- pulls on a still-changing window stay fresh
/// indefinitely (that's the whole point of the #183 fallback), so an
/// unbounded hold meant the wedged raw stream never recovered, and the
/// pull-only path's own backoff ceiling (`SNAPSHOT_PULL_BACKOFF_MAX_US`, ~5s)
/// became the shared window's permanent worst-case lag -- matching a real
/// report of lag climbing to 5s and blur only clearing after ~1 minute.
/// Longer than the 45s trigger (so a brief false-non-idle blip still gets
/// some grace, guarding against the restart-storm history noted above) but
/// well short of the 300s hard backstop, which stays reserved for the
/// idle-but-maybe-wrong case.
const RAW_CAPTURE_STALL_HOLD_GRACE_US: u64 = 90_000_000;
const ADAPTIVE_IDLE_TICK_BASE: Duration = Duration::from_millis(100);
const ADAPTIVE_IDLE_TICK_MAX: Duration = Duration::from_millis(500);
/// Snapshot-pull fallback (#183): when the change-driven SCK stream has been
/// silent this long, start pulling the window's CURRENT content on demand via
/// `SCScreenshotManager`. Measured live: a covered Chrome window playing
/// audible video stops emitting stream frames entirely while its backing
/// content keeps advancing — every on-demand capture returns the advanced
/// frame. Pulling turns "frozen until uncovered" into a real live feed.
const SNAPSHOT_PULL_AFTER_SILENCE_US: u64 = 1_500_000;
/// Minimum spacing between snapshot pulls (~10fps ceiling; each pull costs a
/// WindowServer composite + copy, tens of ms at typical share sizes).
const SNAPSHOT_PULL_MIN_INTERVAL_US: u64 = 100_000;
/// #905: if snapshot-pull re-engages within this long of the last
/// disengage, treat it as the SAME flapping episode -- suppress the
/// individual "ENGAGED" line and fold it into the next real "disengaged"
/// line's rolled-up flap count, rather than logging every rapid
/// engage/disengage cycle.
const PULL_FLAP_DEBOUNCE_US: u64 = 2_000_000;
/// After host input injection, give the change-driven SCK stream one short
/// chance to deliver a post-input image before paying for a screenshot. The
/// #285 measurement showed 43-46ms snapshot cost, so a full frame-period wait
/// would already consume most of the <100ms p95 budget.
const INTERACTION_RAW_CAPTURE_RACE: Duration = Duration::from_millis(6);
/// Ceiling for the error-backoff interval (below). Live-observed
/// (2026-07-08): after ~2 minutes of sustained 10fps pulling, SCK's
/// screenshot backend can start rejecting new one-shot captures
/// ("Stream failed to start") for tens of seconds at a time, then recover on
/// its own -- NOT a permanent condition (macOS < 14 or a genuinely gone
/// source fails on the very FIRST pull, not after hundreds of successes).
/// So errors widen the pull interval instead of disabling the fallback
/// outright, keeping retries cheap while waiting for the backend to recover.
const SNAPSHOT_PULL_BACKOFF_MAX_US: u64 = 5_000_000;
const PUMP_SILENT_LOG_THRESHOLD_US: u64 = 2_000_000;
const REPUBLISH_AWAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
const REPUBLISH_RETRY_ATTEMPTS: usize = 4;
const REPUBLISH_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(150);
/// #841 containment: a reconcile-driven republish must never run hotter than
/// this. A share that "needs" more than one republish every few seconds is
/// oscillating, not adapting -- suppress it (loudly) and let the next demand
/// packet or the resize pump re-evaluate once the interval has passed.
///
/// This BOUNDS the #841 storm (which ran at ~3/s until the share died); it
/// does not cure it. The cure is making the two capture-size authorities
/// agree, which stays open on #841 -- and a size tolerance is NOT the lever:
/// `publisher::push_frame` gates the zero-copy path on captured == published
/// exactly, so tolerating a permanent mismatch would silently disable
/// zero-copy and letterbox every frame instead.
const REPUBLISH_RECONCILE_MIN_INTERVAL: Duration = Duration::from_secs(3);
const REPUBLISH_RETRY_MAX_DELAY: Duration = Duration::from_secs(2);
const DEFERRED_RECONNECT_OLD_UNPUBLISH_ATTEMPTS: usize = 2;
const DEFERRED_RECONNECT_OLD_UNPUBLISH_RETRY_DELAY: Duration = Duration::from_millis(100);
/// VideoToolbox can be transiently unavailable while macOS wake work is still
/// settling. Wait a few seconds after stats prove the fallback, then make one
/// encoder-recreation attempt; the replacement is an ordinary publication so
/// a second software result cannot schedule another retry (#769).
const POST_WAKE_SOFTWARE_ENCODER_RETRY_DELAY: Duration = Duration::from_secs(3);
// Legacy hash pacer remains disabled; the production pump uses SCK dirty rects.
const STATIC_SKIP_AFTER_IDENTICAL_FRAMES: u32 = 3;
const STATIC_REFRESH_INTERVAL_US: u64 = 1_000_000;
const STATIC_FRAME_DEDUP_ENABLED: bool = false;
const DIRTY_RECT_SKIP_ENV: &str = "PETAL_DISABLE_DIRTY_RECT_SKIP";
pub(crate) const VIEWER_DEMAND_STALE_AFTER: Duration = Duration::from_secs(8);
const FIRST_FRAME_ATTEMPTS: usize = 3;
const FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LAYOUT_RECONFIGURE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Minimum spacing between SCStream ROI reconfiguration applies. A live
/// resize emits distinct ROI targets every frame (2026-07-30 defect A);
/// newer targets coalesce onto the deferred slot and only the newest is
/// applied once the spacing elapses.
const LAYOUT_RECONFIGURE_MIN_SPACING: std::time::Duration = std::time::Duration::from_millis(150);
const FIRST_FRAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(400);
/// ScreenCaptureKit stop can block indefinitely in the macOS framework. Never
/// let that call occupy the async runtime or a session-state lock (#415).
const CAPTURE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PUMP_FAILURE_RECOVERY_RESTARTS: u64 = 3;
/// issue #249: cap on how long start-share waits for the source-metadata
/// signaling round-trip (`set_shared_window_info` -> LiveKit `set_metadata`)
/// before publishing the media track anyway. A stalled/reconnecting signaling
/// channel once held the video track UNpublished for ~30s while capture ran,
/// discarded ~800 frames (the "capture pump lag ... overwritten queued frame(s)"
/// spam), and the optimistic share border showed "live" -- the viewer saw
/// nothing. The common fast path still lands metadata FIRST (keeps the
/// receiver's title/color correct, since `color_profile` is only read at
/// track-subscribe time -- see `subscriber.rs`); past the budget the media
/// publish wins and the metadata push completes in the background
/// (`ParticipantMetadataChanged` still corrects late receivers for
/// title/kind/url).
const SHARE_METADATA_PUBLISH_BUDGET: Duration = Duration::from_secs(3);
const VIEWER_DEMAND_DOWNSIZE_HOLD: Duration = Duration::from_secs(6);
/// A demand-driven RAISE must also be sustained before it republishes. This is
/// what makes the 0.8.x republish oscillation structurally impossible on the
/// sender: every republish makes LiveKit announce the track anew, every
/// announcement makes each viewer emit one publication-open demand packet that
/// is NOT derived from a rendered tile (viewport-sized before this fix,
/// presence-only 0x0 after), and raises used to apply INSTANTLY -- so one
/// packet reversed a downsize the sender had just held for 6s, forever,
/// phase-locked to the 2s viewer heartbeat at an 8.0s period. With this hold,
/// a raise only applies if no contradicting demand arrives for 3s -- and the
/// steady tile heartbeat (2s period) always contradicts a phantom spike first.
const VIEWER_DEMAND_UPSIZE_HOLD: Duration = Duration::from_secs(3);
/// A raise that REVERSES a downsize applied less than this long ago must meet
/// the full `VIEWER_DEMAND_DOWNSIZE_HOLD` instead: flip-flopping demand around
/// a rung boundary then costs at least 6s of sustained contradiction per
/// direction, bounding worst-case republish frequency even against a
/// misbehaving viewer. A republish is a deliberate, rare event.
const VIEWER_DEMAND_REVERSAL_DWELL: Duration = Duration::from_secs(30);
const VIEWER_DEMAND_RESOLUTION_RUNGS: [u32; 6] = [960, 1280, 1920, 2560, 3840, 4096];

fn published_metadata_color_profile(capture_color_profile: VideoColorProfile) -> VideoColorProfile {
    crate::transport::publisher::published_window_color_profile(capture_color_profile)
}

/// Whether the source-metadata publish beat the budget before the media publish
/// (issue #249). Extracted as a pure decision so the "wait for metadata vs.
/// publish the track now" boundary is unit-tested independently of tokio timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataPublishOutcome {
    /// Metadata landed within budget -- receivers get title/color before the track.
    WithinBudget,
    /// Metadata publish exceeded the budget; publish the track now (and let the
    /// metadata push finish in the background) rather than keep the viewer dark.
    ExceededBudget,
}

fn metadata_publish_outcome(elapsed: Duration, budget: Duration) -> MetadataPublishOutcome {
    if elapsed >= budget {
        MetadataPublishOutcome::ExceededBudget
    } else {
        MetadataPublishOutcome::WithinBudget
    }
}

/// Start media publication without adding the metadata signaling RTT to its
/// critical path. Metadata is still given a bounded wait so the common fast
/// path can be observed as such, but both operations make progress together.
/// If the timeout fires, dropping the handle detaches the metadata task; it
/// does not cancel the LiveKit update (#299, building on #249).
async fn publish_media_while_metadata_runs<T, E, F>(
    metadata_task: tokio::task::JoinHandle<()>,
    media_publish: F,
) -> (Result<T, E>, MetadataPublishOutcome, Duration)
where
    F: Future<Output = Result<T, E>>,
{
    let metadata_started = Instant::now();
    let metadata_wait = tokio::time::timeout(SHARE_METADATA_PUBLISH_BUDGET, metadata_task);
    let (metadata_result, media_result) = tokio::join!(metadata_wait, media_publish);
    let metadata_elapsed = metadata_started.elapsed();
    let outcome = if metadata_result.is_err() {
        MetadataPublishOutcome::ExceededBudget
    } else {
        metadata_publish_outcome(metadata_elapsed, SHARE_METADATA_PUBLISH_BUDGET)
    };
    (media_result, outcome, metadata_elapsed)
}

/// Run the network teardown only after the local capture-stopped boundary has
/// updated its visible state. Keeping this small sequencing seam shared by the
/// real stop path and the test prevents a slow or failed unpublish from
/// regressing the #420 indicator ordering.
async fn unpublish_after_capture_boundary<T, E, MakeUnpublish, F>(
    on_capture_stopped: impl FnOnce(),
    make_unpublish: MakeUnpublish,
) -> Result<T, E>
where
    MakeUnpublish: FnOnce() -> F,
    F: Future<Output = Result<T, E>>,
{
    on_capture_stopped();
    make_unpublish().await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpFailureRecoveryDecision {
    Restart,
    CircuitOpen,
}

/// #807: this used to read `restart_generation`, which only ever increments
/// for the life of a share. Three capture restarts -- even fully successful
/// ones, even hours apart in a 90-minute meeting -- stopped the share for
/// good. "Three restarts in ten seconds" is a wedge; "three restarts in
/// ninety minutes" is a healthy self-healing share, and the breaker could not
/// tell them apart. It now counts failures within
/// `PUMP_RECOVERY_FAILURE_WINDOW_US` of each other.
///
/// `restart_generation` keeps its own job -- it is the share's staleness
/// token, compared for equality by `is_share_restart_generation_active`, and
/// resetting THAT would silently drop real recoveries as stale.
fn pump_failure_recovery_decision(recent_failures: u32) -> PumpFailureRecoveryDecision {
    if u64::from(recent_failures) <= MAX_PUMP_FAILURE_RECOVERY_RESTARTS {
        PumpFailureRecoveryDecision::Restart
    } else {
        PumpFailureRecoveryDecision::CircuitOpen
    }
}

/// A recovery this long after the previous one is a fresh incident, not a
/// continuation. Deliberately SHORTER than
/// `RAW_CAPTURE_HARD_RESTART_THRESHOLD_US` so the 300s defensive restart of a
/// genuinely static window can never accumulate into a teardown (#806).
const PUMP_RECOVERY_FAILURE_WINDOW_US: u64 = 180_000_000;

fn pump_recovery_failures() -> &'static Mutex<std::collections::HashMap<u32, (u64, u32)>> {
    static FAILURES: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, (u64, u32)>>> =
        std::sync::OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Record one pump-recovery failure and return how many have happened in the
/// current burst (1 for the first, or the first after a quiet window).
fn record_pump_recovery_failure(window_id: u32, now_us: u64) -> u32 {
    let mut failures = pump_recovery_failures().lock_unpoisoned();
    let entry = failures.entry(window_id).or_insert((now_us, 0));
    if now_us.saturating_sub(entry.0) > PUMP_RECOVERY_FAILURE_WINDOW_US {
        entry.1 = 0;
    }
    entry.0 = now_us;
    entry.1 += 1;
    entry.1
}

fn clear_pump_recovery_failures(window_id: u32) {
    pump_recovery_failures()
        .lock_unpoisoned()
        .remove(&window_id);
}

enum ShareCaptureSource {
    DirectWindowId,
    /// #712: the display-share counterpart to `DirectWindowId`, used ONLY on
    /// the in-place-restart / post-wake-restart paths for a share whose
    /// `source_kind()` is `SharedSourceKind::Display`. The id threaded
    /// through `start_capture_for_share` is the display id (same `u32` slot
    /// a window id would occupy -- see `ActiveShare::source_kind`), resolved
    /// via `capture::prepare_direct_display_source` /
    /// `WindowCapture::start_display_with_error_handler_at_resolution`
    /// instead of the window-only `content.windows()` lookup.
    DirectDisplayId,
    SystemPicker {
        filter: SCContentFilter,
        logical_width: f64,
        logical_height: f64,
        point_pixel_scale: f64,
        source_kind: crate::transport::publisher::SharedSourceKind,
        source_title: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharePublishOrigin {
    Ordinary,
    PostWakeRestart,
}

impl ShareCaptureSource {
    fn source_kind(&self) -> crate::transport::publisher::SharedSourceKind {
        match self {
            Self::DirectWindowId => crate::transport::publisher::SharedSourceKind::Window,
            Self::DirectDisplayId => crate::transport::publisher::SharedSourceKind::Display,
            Self::SystemPicker { source_kind, .. } => *source_kind,
        }
    }

    /// Pick the direct (non-picker) restart source that matches a share's
    /// OWN tracked `source_kind` -- the #712 fix. Never infer this from the
    /// error content or guess; the share already knows what it is.
    fn direct_for_kind(source_kind: crate::transport::publisher::SharedSourceKind) -> Self {
        match source_kind {
            crate::transport::publisher::SharedSourceKind::Window => Self::DirectWindowId,
            crate::transport::publisher::SharedSourceKind::Display
            | crate::transport::publisher::SharedSourceKind::DisplayRegion => Self::DirectDisplayId,
        }
    }

    fn source_title_override(&self) -> Option<String> {
        match self {
            Self::DirectWindowId | Self::DirectDisplayId => None,
            Self::SystemPicker { source_title, .. } => source_title.clone(),
        }
    }
}

/// One actively-shared window: its live capture stream + its published
/// LiveKit track. Dropping `capture` stops the `SCStream` (see
/// `WindowCapture`'s own `Drop`/`stop` doc comment); the frame-pump task is
/// aborted explicitly on stop since it's a detached `tokio::spawn`, not tied
/// to any value's `Drop`.
///
/// `published` is behind its own `Mutex` (not just the outer `SessionInner`
/// one) because the frame-pump task holds a long-lived clone of this
/// `Arc<ActiveShare>`-reachable slot and reads it on every single captured
/// frame (~30fps) -- swapping in a re-published track at a new
/// `ShareQuality` (see `apply_quality` below) must not require the pump to
/// be torn down and restarted, just to briefly hand it a new
/// `PublishedTrack` to push into.
pub(super) struct ActiveShare {
    capture: Arc<WindowCapture>,
    published: Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>,
    pump_abort: tokio::task::AbortHandle,
    monitor: tokio::task::JoinHandle<()>,
    /// Incremented every time the capture/pump is rebuilt in place. Watchdog
    /// recovery tasks compare this with their captured value so a stale
    /// restart cannot replace a newer capture while preserving `started_seq`.
    restart_generation: u64,
    /// Owning process id captured at share start. Remote control reads this
    /// instead of enumerating all shareable windows on every input packet.
    pid: Option<i32>,
    /// Monotonic insertion counter (see `SessionInner::next_share_seq`) --
    /// "most recently toggled-on share is focused" (this module's doc
    /// comment) is implemented as "highest `started_seq` among current
    /// shares," so this is the only piece of ordering state a share needs.
    pub(super) started_seq: u64,
    /// The shared window's last-known on-screen frame (global, top-left
    /// logical points -- same `WindowFrame` shape `hover_tab.rs`/
    /// `share_border.rs` already use). Used by `telepointer.rs`'s cursor-poll
    /// loop to hit-test the local cursor against each currently-shared
    /// window (SPEC.md §4.5). Seeded from the frame captured at
    /// `start_share` time and kept LIVE-REFRESHED (issue #30) by that same
    /// telepointer loop (~9Hz, via one shared `CGWindowList` snapshot --
    /// see `telepointer.rs`'s module doc comment and
    /// [`SessionState::update_share_frames`]), so hit-testing keeps landing
    /// on the right spot after the shared window moves/resizes. If the
    /// window is momentarily off-screen (minimized / other Space) the last
    /// on-screen frame is retained rather than cleared -- the cursor can't
    /// meaningfully be "over" an off-screen window anyway, and the frame
    /// snaps back to fresh on the first refresh tick after it reappears.
    frame: Mutex<crate::hover_tab::WindowFrame>,
    /// Updated by the telepointer poll's shared on-screen window snapshot.
    /// Remote control reads this bit to pause input when a share leaves the
    /// on-screen list without doing its own per-packet WindowServer query.
    visible_on_screen: AtomicBool,
    /// Updated off the input path for invisible shares so remote control can
    /// distinguish minimized/other-Space windows from closed windows without
    /// calling `CGWindowListCopyWindowInfo` per event.
    known_closed: AtomicBool,
    source_kind: crate::transport::publisher::SharedSourceKind,
    source_title: String,
    /// The share border's color, snapshotted when this share started. #764:
    /// the post-wake restart must redraw the border in the user's own color,
    /// and no other per-share record of it exists.
    border_color: String,
    priority: Arc<Mutex<SharePriority>>,
    interaction_signal: Arc<InteractionSignal>,
    resolution: CaptureResolution,
    demand_resolution: Mutex<ViewerDemandResolutionState>,
    republish_intent: RepublishIntent,
    /// Whether remote peers may CONTROL this specific share. Seeded from the
    /// user's global default at share start, flippable mid-share from the
    /// hover tab. This is REAL authorization -- `remote_control.rs` re-reads
    /// it on every input packet. The matching `petalWindowRemoteControl`
    /// metadata is only a hint so peers know whether to show the affordance;
    /// a peer that ignores the hint still gets refused here.
    allow_remote_control: AtomicBool,
    /// #915: cancellable background poller that fills in `petalWindowUrls`
    /// for a shared browser window off the share-start path. `None` when
    /// this share isn't eligible (not a window, a `source_title_override`
    /// share, or a non-browser bundle id) -- see `spawn_share_url_refresh`.
    /// Every teardown/replacement path that removes an `ActiveShare` MUST
    /// call `.stop()` on this (mirrors `session_stub.rs`'s Windows
    /// `start_share_url_refresh`/`url_refresh` field): a dropped, un-stopped
    /// `UrlRefreshTask` merely detaches its task, leaving the poller running
    /// forever.
    url_refresh: Option<UrlRefreshTask>,
}

type PublishedTrackSlot = Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>;
type RepublishIntent = Arc<RepublishCoordinator>;
type LatestCapturedFrame = Arc<Mutex<Option<(crate::capture::CapturedFrame, u64)>>>;

/// Cancellable lifecycle token carried from the `Reconnected` event through
/// every asynchronous repair mutation. A later reconnect or disconnect bumps
/// its epoch; leaving/rejoining invalidates its room generation (#298).
#[derive(Clone)]
pub(crate) struct ReconnectRepairGuard {
    room_generation: RoomGeneration,
    epoch: Arc<AtomicU64>,
    expected_epoch: u64,
}

impl ReconnectRepairGuard {
    pub(crate) fn new(
        room_generation: RoomGeneration,
        epoch: Arc<AtomicU64>,
        expected_epoch: u64,
    ) -> Self {
        Self {
            room_generation,
            epoch,
            expected_epoch,
        }
    }

    /// `pub(super)` (not private) so #713's mic/camera reconnect repair --
    /// which lives in `session::mod` and `crate::camera_session` -- can take
    /// the exact same currency snapshot the window-share repair path already
    /// uses, instead of a second, possibly-drifting currency check.
    pub(super) fn is_current_with_inner(&self, inner: &SessionInner) -> bool {
        reconnect_repair_lifecycle_is_current(
            self.room_generation.is_current(),
            self.epoch.load(Ordering::SeqCst) == self.expected_epoch,
            inner.joined.is_some(),
        )
    }
}

fn reconnect_repair_lifecycle_is_current(
    room_generation_current: bool,
    repair_epoch_current: bool,
    joined: bool,
) -> bool {
    room_generation_current && repair_epoch_current && joined
}

/// Metadata is scoped by window id, so a stopped generation may clear it only
/// while its original room remains joined and no re-share has claimed that id
/// during the awaited old-track unpublish (#298).
fn stopped_share_metadata_cleanup_is_current(
    reconnect_lifecycle_current: bool,
    original_room_is_current: bool,
    current_share_started_seq: Option<u64>,
) -> bool {
    reconnect_lifecycle_current && original_room_is_current && current_share_started_seq.is_none()
}

fn terminal_reconnect_effects_are_current(
    active_share_present: bool,
    last_stopped_share_seq: Option<u64>,
    expected_started_seq: u64,
) -> bool {
    !active_share_present && last_stopped_share_seq == Some(expected_started_seq)
}

/// Preserve the latest terminal generation for each native window id. A
/// reconnect task can outlive stop → re-share → stop; only the final stop's
/// generation may apply terminal UI/control effects (#298).
fn record_last_stopped_share_generation(
    last_stopped_share_seq: &mut HashMap<u32, u64>,
    window_id: u32,
    started_seq: u64,
) {
    match last_stopped_share_seq.get(&window_id) {
        Some(previous) if *previous >= started_seq => {}
        _ => {
            last_stopped_share_seq.insert(window_id, started_seq);
        }
    }
}

#[derive(Default)]
struct RepublishCoordinator {
    generation: AtomicU64,
    apply_lock: tokio::sync::Mutex<()>,
}

struct StartedShareCapture {
    capture: WindowCapture,
    width: u32,
    height: u32,
    source_scale: f64,
    color_profile: VideoColorProfile,
    latest_frame: LatestCapturedFrame,
    latest_frame_notify: Arc<tokio::sync::Notify>,
    last_capture_wall_time_us: Arc<AtomicU64>,
    layout_gate: crate::capture::LayoutIntegrityGate,
    capture_error_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

struct SharePumpRuntime {
    pump_abort: tokio::task::AbortHandle,
    monitor: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharePumpWake {
    RawFrame,
    Interaction,
    IdleTick,
    RegionTick,
}

struct ShareRestartSnapshot {
    published: PublishedTrackSlot,
    room_connection: Arc<RoomConnection>,
    restart_generation: u64,
    priority: Arc<Mutex<SharePriority>>,
    interaction_signal: Arc<InteractionSignal>,
    resolution: CaptureResolution,
    demand_long_edge: Option<u32>,
    republish_intent: RepublishIntent,
    diagnostic_source: SourceSelectionClass,
    /// #712: the share's OWN kind, captured alongside `share.capture` at
    /// snapshot time so `spawn_pump_failure_recovery`'s in-place restart can
    /// pick `ShareCaptureSource::DirectWindowId` vs `DirectDisplayId`
    /// correctly instead of always assuming a window.
    source_kind: crate::transport::publisher::SharedSourceKind,
}

struct SharePublicationRepairSnapshot {
    published: PublishedTrackSlot,
    room_connection: Arc<RoomConnection>,
    priority: Arc<Mutex<SharePriority>>,
    republish_intent: RepublishIntent,
    capture_config: crate::capture::WindowCaptureConfig,
}

/// What the local SDK still knows about the publication that a live share
/// intends to own after reconnect. This deliberately distinguishes a healthy
/// resume from a missing publication: a resume keeps the original publication
/// and must not cause a needless renegotiation just because it emitted the
/// same public `Reconnected` event as a full reconnect.
/// `pub(super)`: shared by #713's mic/camera reconnect repair (`session::mod`
/// / `crate::camera_session`), not just the window-share repair path in this
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReconnectPublicationHealth {
    CurrentSidPresent,
    ReplacementAlreadyPresent,
    Missing,
}

/// The recoverable result of publishing a replacement after reconnect.  A
/// successful replacement stays usable even when the obsolete publication's
/// best-effort cleanup times out; receivers dedupe that overlap by window id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepublishOutcome {
    Replaced,
    ReplacedWithOldCleanupPending,
    ReplacedWithOldCleanupDeferred,
    Cancelled,
    Failed,
}

impl RepublishOutcome {
    fn replaced(self) -> bool {
        matches!(
            self,
            Self::Replaced
                | Self::ReplacedWithOldCleanupPending
                | Self::ReplacedWithOldCleanupDeferred
        )
    }
}

struct ReconnectRepublishGuard<'a> {
    state: &'a SessionState,
    lifecycle: &'a ReconnectRepairGuard,
    window_id: u32,
    started_seq: u64,
}

impl ReconnectRepublishGuard<'_> {
    fn is_current(&self) -> bool {
        self.state.reconnect_repair_guard_is_current(self.lifecycle)
            && self
                .state
                .is_share_generation_active(self.window_id, self.started_seq)
    }
}

/// A reconnect transaction that has just published its replacement but whose
/// lifecycle was invalidated must retire only that new publication. It must
/// not swap the slot, reconfigure capture, or unpublish the old share.
fn reconnect_republish_must_cleanup_new_after_publish(lifecycle_current: bool) -> bool {
    !lifecycle_current
}

fn reconnect_republish_invalidation_outcome(committed_target: bool) -> RepublishOutcome {
    if committed_target {
        RepublishOutcome::ReplacedWithOldCleanupDeferred
    } else {
        RepublishOutcome::Cancelled
    }
}

/// Once a new publication is committed, failure to retire the captured old
/// publication must never roll back the replacement. Keep the replacement
/// authoritative and schedule bounded cleanup of only the old track (#298).
fn committed_republish_needs_deferred_old_cleanup(
    committed_target: bool,
    old_cleanup_succeeded: bool,
) -> bool {
    committed_target && !old_cleanup_succeeded
}

/// This is evaluated only after scheduling any failed committed old-track
/// cleanup. An invalidation in that exact post-await gap must not turn a
/// replacement back into a leak (#298).
fn reconnect_republish_post_old_cleanup_early_outcome(
    committed_target: bool,
    lifecycle_current: bool,
    intent_current: bool,
) -> Option<RepublishOutcome> {
    if !lifecycle_current {
        return Some(reconnect_republish_invalidation_outcome(committed_target));
    }
    if !intent_current {
        return Some(RepublishOutcome::Cancelled);
    }
    None
}

/// After the capture-scale signaling await, the replacement already owns the
/// slot. Any reconnect lifecycle or intent exit must first schedule cleanup of
/// the captured original old track (#298).
fn reconnect_republish_post_capture_scale_early_outcome(
    lifecycle_current: bool,
    intent_current: bool,
) -> Option<RepublishOutcome> {
    if !lifecycle_current {
        return Some(RepublishOutcome::ReplacedWithOldCleanupDeferred);
    }
    if !intent_current {
        return Some(RepublishOutcome::Cancelled);
    }
    None
}

/// A non-reconnect quality/resize request can quietly yield to a newer
/// request, but reconnect repair must not call that a success: its caller
/// would otherwise restore FPS and log a stale replacement after an await.
fn reconnect_republish_superseded_outcome(
    reconnect_guard: Option<&ReconnectRepublishGuard<'_>>,
) -> RepublishOutcome {
    if reconnect_guard.is_some() {
        RepublishOutcome::Cancelled
    } else {
        RepublishOutcome::Replaced
    }
}

/// `pub(super)`: shared by #713's mic/camera reconnect repair.
pub(super) fn reconnect_publication_health<'a>(
    current_sid: &str,
    expected_track_name: &str,
    publications: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> ReconnectPublicationHealth {
    let mut replacement_present = false;
    for (sid, name) in publications {
        if sid == current_sid {
            return ReconnectPublicationHealth::CurrentSidPresent;
        }
        if name == expected_track_name {
            replacement_present = true;
        }
    }
    if replacement_present {
        ReconnectPublicationHealth::ReplacementAlreadyPresent
    } else {
        ReconnectPublicationHealth::Missing
    }
}

/// Only the exact tracked SID proves that the capture pump's publication is
/// still live. A different same-name publication is unbound SDK state: this
/// process has no supported way to adopt it into `PublishedTrack`, so leaving
/// the old slot untouched would preserve an invisible/black share after a
/// reconnect. It must take the existing guarded replacement path (#298).
/// `pub(super)`: shared by #713's mic/camera reconnect repair.
pub(super) fn reconnect_publication_requires_repair(health: ReconnectPublicationHealth) -> bool {
    !matches!(health, ReconnectPublicationHealth::CurrentSidPresent)
}

fn begin_republish_intent(intent: &RepublishIntent) -> u64 {
    intent
        .generation
        .fetch_add(1, Ordering::SeqCst)
        .saturating_add(1)
}

fn republish_intent_is_current(intent: &RepublishIntent, generation: u64) -> bool {
    intent.generation.load(Ordering::SeqCst) == generation
}

/// Hysteresis for demand-driven capture-resolution changes. Every change of
/// `applied_long_edge` costs a full track republish (`apply_quality` ->
/// `republish_for_quality_reconcile`), so a change in EITHER direction
/// requires the demanded rung to be sustained -- i.e. not contradicted by any
/// other demand sample -- for a hold period first. A single spurious packet
/// (e.g. the publication-open demand every viewer emits when a republish
/// re-announces the track) can therefore start a pending change but never
/// commit one: the steady 2s tile heartbeat contradicts it well inside the
/// shortest hold. See the constants above for the loop this kills (#627).
#[derive(Debug, Clone, Copy, Default)]
struct ViewerDemandResolutionState {
    applied_long_edge: Option<u32>,
    pending_change: Option<(u32, Instant)>,
    /// When the last change applied and whether it was a raise, so a prompt
    /// reversal can be held to the stricter dwell.
    last_change: Option<(Instant, bool)>,
}

impl ViewerDemandResolutionState {
    fn required_hold(&self, raising: bool, now: Instant) -> Duration {
        if !raising {
            return VIEWER_DEMAND_DOWNSIZE_HOLD;
        }
        let reverses_recent_lower = self.last_change.is_some_and(|(at, was_raise)| {
            !was_raise && now.saturating_duration_since(at) < VIEWER_DEMAND_REVERSAL_DWELL
        });
        if reverses_recent_lower {
            VIEWER_DEMAND_DOWNSIZE_HOLD
        } else {
            VIEWER_DEMAND_UPSIZE_HOLD
        }
    }

    fn reconcile(
        &mut self,
        requested_long_edge: Option<u32>,
        current_long_edge: u32,
        now: Instant,
    ) -> Option<u32> {
        if self.applied_long_edge.is_none() {
            self.applied_long_edge = Some(current_long_edge.max(1));
        }
        let Some(target) = viewer_demand_resolution_rung(requested_long_edge) else {
            // No live demand carries any resolution information: keep what is
            // applied and abandon any pending change -- silence must never
            // mature into a republish.
            self.pending_change = None;
            return self.applied_long_edge;
        };
        let applied = self.applied_long_edge.unwrap_or(1);
        if target == applied {
            self.pending_change = None;
            return self.applied_long_edge;
        }
        match self.pending_change {
            Some((pending, since)) if pending == target => {
                if now.saturating_duration_since(since) >= self.required_hold(target > applied, now)
                {
                    self.applied_long_edge = Some(target);
                    self.pending_change = None;
                    self.last_change = Some((now, target > applied));
                }
            }
            // A different target (or none) restarts the clock: the demand was
            // contradicted, so it was not sustained.
            _ => {
                self.pending_change = Some((target, now));
            }
        }
        self.applied_long_edge
    }
}

fn viewer_demand_resolution_rung(requested_long_edge: Option<u32>) -> Option<u32> {
    let requested = requested_long_edge.filter(|edge| *edge > 0)?;
    VIEWER_DEMAND_RESOLUTION_RUNGS
        .iter()
        .copied()
        .find(|rung| *rung >= requested)
        .or_else(|| VIEWER_DEMAND_RESOLUTION_RUNGS.last().copied())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ViewerDemandKey {
    pub(super) window_id: u32,
    pub(super) viewer_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct PassiveViewerDemand {
    pub(super) seq: u64,
    pub(super) updated_at: Instant,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) scale: f64,
    pub(super) pixel_width: u32,
    pub(super) pixel_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerDemandEvent {
    Open,
    Closed,
    Heartbeat,
}

#[derive(Debug, Clone)]
pub struct ViewerDemandUpdate {
    pub event: ViewerDemandEvent,
    pub viewer_id: String,
    pub window_id: u32,
    pub seq: u64,
    pub visible: bool,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub received_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureWatchdogDecision {
    Healthy,
    StalledPermissionDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpWatchdogDecision {
    Healthy,
    Stalled { silent_for_us: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawCaptureWatchdogDecision {
    Healthy,
    /// Raw frames have gone silent past the threshold, but the source was
    /// idle/static when it stopped (SCK is change-driven, so an unchanging
    /// window legitimately produces no frames). The stream is healthy — do NOT
    /// tear it down and republish the track; the parked-frame re-push keeps the
    /// viewer's image intact. This is the fix for the idle restart-loop that
    /// fired 336× in one session on windows that were simply not redrawing.
    IdleHealthy {
        silent_for_us: u64,
    },
    Stalled {
        silent_for_us: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SharedWindowScreenStatus {
    NotShared,
    OnScreen(crate::hover_tab::WindowFrame),
    OffScreen,
    Closed,
}

/// "Does this window still exist anywhere?" -- the one CoreGraphics question
/// `update_share_frames_and_visibility` asks.
///
/// A seam (#742), copying the `SessionTapBackend` pattern in `remote_control`.
/// It exists so the off-screen-vs-closed decision can be tested through the
/// REAL production path instead of through a `#[cfg(test)]` copy of it: this
/// function previously had a test-only twin that could not fail when
/// production regressed.
///
/// Note for the window-registry migration: the live implementation is
/// `cg::window_exists`, which costs ~2315us of WindowServer CPU per call
/// (plan §9.6) and is invoked PER SHARED WINDOW at ~9Hz. It is the single
/// most expensive WindowServer consumer in the app.
pub(crate) trait WindowExistence {
    fn window_exists(&self, window_id: u32) -> bool;
}

pub(crate) struct CgWindowExistence;

impl WindowExistence for CgWindowExistence {
    fn window_exists(&self, window_id: u32) -> bool {
        crate::platform::cg::window_exists(window_id)
    }
}

/// Map a share's stored flags to its reported screen status.
///
/// This is THE implementation `SessionState::shared_window_screen_status` uses
/// -- not a test-only restatement of it. It exists as a free function only
/// because `ActiveShare` owns capture handles and abort tokens and so cannot be
/// constructed in a unit test (#742: the previous test asserted on a
/// `#[cfg(test)]` duplicate of these rules, which could not fail when
/// production regressed).
fn screen_status_from_flags(
    frame: crate::hover_tab::WindowFrame,
    visible_on_screen: bool,
    known_closed: bool,
) -> SharedWindowScreenStatus {
    if visible_on_screen {
        SharedWindowScreenStatus::OnScreen(frame)
    } else if known_closed {
        SharedWindowScreenStatus::Closed
    } else {
        SharedWindowScreenStatus::OffScreen
    }
}

/// Decide `(visible_on_screen, known_closed)` per shared source.
///
/// THE implementation used by `update_share_frames_and_visibility_with`, split
/// out for the same reason as `screen_status_from_flags`. Two behaviours are
/// pinned by test and must survive the window-registry migration:
/// - **Display shares are always "visible" and never "closed"** -- a display is
///   not in the window list, so the window-presence signal is meaningless for
///   it and would otherwise mark every display share closed.
/// - **`window_exists` is only consulted when a window share is absent from the
///   visible set** -- the expensive path (~2315us of WindowServer CPU per call,
///   plan §9.6) is already short-circuited for visible windows. A registry must
///   not make it unconditional.
fn visibility_decisions(
    shared_sources: &[(u32, crate::transport::publisher::SharedSourceKind)],
    visible_window_ids: &[u32],
    existence: &dyn WindowExistence,
) -> Vec<(u32, bool, bool)> {
    shared_sources
        .iter()
        .map(|(window_id, source_kind)| {
            let visible = visible_window_ids.contains(window_id);
            let is_display = matches!(
                *source_kind,
                crate::transport::publisher::SharedSourceKind::Display
                    | crate::transport::publisher::SharedSourceKind::DisplayRegion
            );
            (
                *window_id,
                visible || is_display,
                !is_display && !visible && !existence.window_exists(*window_id),
            )
        })
        .collect()
}

fn capture_watchdog_decision(
    now_us: u64,
    last_capture_wall_time_us: u64,
    has_screen_recording_permission: bool,
) -> CaptureWatchdogDecision {
    let age_us = now_us.saturating_sub(last_capture_wall_time_us);
    if age_us > CAPTURE_STALL_THRESHOLD_US && !has_screen_recording_permission {
        CaptureWatchdogDecision::StalledPermissionDenied
    } else {
        CaptureWatchdogDecision::Healthy
    }
}

fn pump_watchdog_decision(
    now_us: u64,
    last_pump_activity_wall_time_us: u64,
) -> PumpWatchdogDecision {
    let silent_for_us = now_us.saturating_sub(last_pump_activity_wall_time_us);
    if silent_for_us > PUMP_STALL_THRESHOLD_US {
        PumpWatchdogDecision::Stalled { silent_for_us }
    } else {
        PumpWatchdogDecision::Healthy
    }
}

/// See `RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US`'s doc comment (issue #60):
/// a long-threshold safety net keyed on the RAW ScreenCaptureKit callback
/// timestamp (`last_capture_wall_time_us`), not the pump task's own
/// liveness -- the latter is intentionally always-fresh now (see the pump
/// loop's polling fix) and can no longer detect a genuinely dead stream.
/// Per-window "the source stopped drawing (idle/occluded)" flag, set by the
/// capture callback and read by the raw-capture watchdog to decide whether a
/// silence is a healthy-idle source (don't restart) or a wedge (restart).
/// Keyed by CGWindowID; a stale bool per window is negligible so entries are
/// left in place across shares (overwritten on the next frame). Global rather
/// than threaded because `last_capture_wall_time_us`'s plumbing already spans
/// seven call sites and the watchdog only needs a single read.
fn source_idle_flags() -> &'static Mutex<std::collections::HashMap<u32, bool>> {
    static FLAGS: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, bool>>> =
        std::sync::OnceLock::new();
    FLAGS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn set_source_appears_idle(window_id: u32, idle: bool) {
    source_idle_flags()
        .lock_unpoisoned()
        .insert(window_id, idle);
}

fn source_appears_idle(window_id: u32) -> bool {
    source_idle_flags()
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .unwrap_or(false)
}

/// Record one SUCCESSFUL snapshot pull and report whether the content moved.
///
/// #806: both signals the raw-capture watchdog reads used to be written only
/// when the content CHANGED, so a genuinely static window -- the exact case
/// the pull fallback exists for -- was indistinguishable from a wedged stream.
/// Measured live: 358 consecutive successful pulls, zero freshness recorded,
/// the 45s watchdog restarting anyway, and the recovery circuit breaker
/// stopping the share ~2m15s after the content stopped moving.
fn observe_snapshot_pull(window_id: u32, previous: Option<u64>, current: u64, at_us: u64) -> bool {
    mark_snapshot_pull_fresh(window_id, at_us);
    let changed = snapshot_hash_changed(window_id, previous, current);
    if !changed {
        // The only signal a silent raw stream can give that its source is
        // merely idle: `set_source_appears_idle` is otherwise written from the
        // raw-frame callback, which by definition never runs here.
        set_source_appears_idle(window_id, true);
    }
    changed
}

fn snapshot_hash_changed(window_id: u32, previous: Option<u64>, current: u64) -> bool {
    let changed = previous != Some(current);
    if previous.is_some() && changed {
        set_source_appears_idle(window_id, false);
    }
    changed
}

/// How many times one identical content-rect ROI may be requested, and go
/// unacknowledged, before the share stops asking and keeps the padded raster
/// (#804). Two restarts is already generous: each one is a publication
/// teardown on a live share, which is exactly the disruption CLAUDE.md's
/// "never show a black frame" rule exists to prevent.
const LAYOUT_ROI_MAX_ATTEMPTS: u32 = 3;

/// Consecutive unacknowledged attempts at one ROI target, per window.
///
/// Global rather than monitor-local BECAUSE the monitor does not survive the
/// restart it triggers: a per-task counter resets on every restart and can
/// never reach a bound, which is the shape of the #804 livelock itself.
fn layout_roi_ack_failures() -> &'static Mutex<std::collections::HashMap<u32, ((u32, u32), u32)>> {
    static FAILURES: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, ((u32, u32), u32)>>> =
        std::sync::OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Record one unacknowledged ROI attempt and return the consecutive count for
/// that exact target. A different target restarts the count -- a moving target
/// is a live resize converging, not a livelock.
fn record_layout_roi_ack_failure(window_id: u32, target: (u32, u32)) -> u32 {
    let mut failures = layout_roi_ack_failures().lock_unpoisoned();
    let entry = failures.entry(window_id).or_insert((target, 0));
    if entry.0 != target {
        *entry = (target, 0);
    }
    entry.1 += 1;
    entry.1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutAckFailureAction {
    /// Restart capture in place and try the ROI again.
    RestartCapture,
    /// Stop asking: keep the padded raster and stay published (#804).
    AbandonRoi,
}

/// What to do about the `attempts`-th consecutive unacknowledged ROI request.
///
/// This MUST abandon strictly before `MAX_PUMP_FAILURE_RECOVERY_RESTARTS`
/// restarts have accumulated. The recovery circuit breaker's terminal action
/// is `stop_share` -- so a budget that outlives it does not fix #804, it just
/// relabels an endless restart loop as a dead share (which is what the live
/// suite actually measured: `pump recovery circuit open at restart_generation
/// 3; stopping share`, then every remote-control request refused for an
/// inactive window).
fn layout_ack_failure_action(attempts: u32) -> LayoutAckFailureAction {
    if attempts >= LAYOUT_ROI_MAX_ATTEMPTS {
        LayoutAckFailureAction::AbandonRoi
    } else {
        LayoutAckFailureAction::RestartCapture
    }
}

fn clear_layout_roi_ack_failures(window_id: u32) {
    layout_roi_ack_failures()
        .lock_unpoisoned()
        .remove(&window_id);
}

/// Per-window timestamp of the last SUCCESSFUL snapshot pull (#183), read by
/// the raw-capture watchdog: while pulls are succeeding, the share is alive
/// even though the push stream is silent — restarting it would only churn.
fn snapshot_pull_fresh_us() -> &'static Mutex<std::collections::HashMap<u32, u64>> {
    static FRESH: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, u64>>> =
        std::sync::OnceLock::new();
    FRESH.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn mark_snapshot_pull_fresh(window_id: u32, at_us: u64) {
    snapshot_pull_fresh_us()
        .lock_unpoisoned()
        .insert(window_id, at_us);
}

fn snapshot_pull_fresh_within(window_id: u32, now_us: u64, max_age_us: u64) -> bool {
    snapshot_pull_fresh_us()
        .lock_unpoisoned()
        .get(&window_id)
        .is_some_and(|at| now_us.saturating_sub(*at) <= max_age_us)
}

#[derive(Default)]
struct InteractionSignal {
    epoch: AtomicU64,
    applied_at_us: AtomicU64,
    input_seq: AtomicU64,
    notify: tokio::sync::Notify,
}

fn interaction_signals() -> &'static Mutex<std::collections::HashMap<u32, Weak<InteractionSignal>>>
{
    static SIGNALS: std::sync::OnceLock<
        Mutex<std::collections::HashMap<u32, Weak<InteractionSignal>>>,
    > = std::sync::OnceLock::new();
    SIGNALS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn register_interaction_signal(window_id: u32, signal: &Arc<InteractionSignal>) {
    interaction_signals()
        .lock_unpoisoned()
        .insert(window_id, Arc::downgrade(signal));
}

fn unregister_interaction_signal(window_id: u32, signal: &Arc<InteractionSignal>) {
    let mut signals = interaction_signals().lock_unpoisoned();
    if signals
        .get(&window_id)
        .and_then(Weak::upgrade)
        .is_some_and(|registered| Arc::ptr_eq(&registered, signal))
    {
        signals.remove(&window_id);
    }
}

/// Called only after remote input was successfully applied to the host app.
/// The monotonically increasing epoch coalesces input arriving while a
/// snapshot is already in flight; `input_seq` is retained for diagnostics and
/// correlation with the press-to-display waterfall.
pub(crate) fn note_remote_interaction(window_id: u32, input_seq: u64) {
    let signal = interaction_signals()
        .lock_unpoisoned()
        .get(&window_id)
        .and_then(Weak::upgrade);
    let Some(signal) = signal else {
        return;
    };
    let applied_at_us = now_us();
    signal.input_seq.store(input_seq, Ordering::Relaxed);
    signal.applied_at_us.store(applied_at_us, Ordering::Release);
    let epoch = signal.epoch.fetch_add(1, Ordering::AcqRel) + 1;
    // Feeds the interaction-burst policy's hysteresis (#290 step 5) -- same
    // call site, no second signal path.
    mark_interaction_burst(window_id, applied_at_us);
    signal.notify.notify_one();
    log::debug!(
        "capture-assist: window {window_id} host input applied seq={input_seq} epoch={epoch} at_us={applied_at_us}"
    );
}

/// Pure pacing rule for the snapshot-pull fallback (#183): pull only when the
/// raw stream has been silent past the engage threshold AND the previous pull
/// is old enough to respect the pull-rate ceiling (`min_interval_us` --
/// normally `SNAPSHOT_PULL_MIN_INTERVAL_US`, widened under error backoff).
fn snapshot_pull_decision(
    now_us: u64,
    last_raw_frame_us: u64,
    last_pull_us: u64,
    min_interval_us: u64,
) -> bool {
    now_us.saturating_sub(last_raw_frame_us) >= SNAPSHOT_PULL_AFTER_SILENCE_US
        && now_us.saturating_sub(last_pull_us) >= min_interval_us
}

fn interaction_snapshot_decision(
    epoch: u64,
    handled_epoch: u64,
    last_raw_frame_us: u64,
    input_applied_at_us: u64,
) -> bool {
    epoch > handled_epoch && last_raw_frame_us <= input_applied_at_us
}

/// Per-window "last remote input applied" timestamp for the interaction-burst
/// policy (#290 step 5). Reuses `note_remote_interaction`'s existing
/// epoch/timestamp plumbing -- every typing/wheel/drag event that lands on
/// the host already calls that function, so no second signal path is added
/// here; this just remembers the most recent `applied_at_us` per window so
/// burst-active/inactive can be recomputed fresh, on demand, with hysteresis.
fn interaction_burst_last_applied_us() -> &'static Mutex<std::collections::HashMap<u32, u64>> {
    static LAST: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, u64>>> =
        std::sync::OnceLock::new();
    LAST.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn mark_interaction_burst(window_id: u32, applied_at_us: u64) {
    interaction_burst_last_applied_us()
        .lock_unpoisoned()
        .insert(window_id, applied_at_us);
}

fn clear_interaction_burst_state(window_id: u32) {
    interaction_burst_last_applied_us()
        .lock_unpoisoned()
        .remove(&window_id);
}

/// How long an interaction burst is considered "active" after the most
/// recent applied remote typing/wheel/drag input. This single trailing
/// window supplies BOTH halves of the hysteresis the policy needs: it
/// re-extends on every new interaction, so a steady stream of
/// keystrokes/wheel-ticks/drag-move events never flaps the burst off between
/// individual inputs (which land well under a second apart), and it lapses
/// on genuine idle, so a control session that goes quiet (the controller is
/// reading, not typing) correctly releases the floor instead of holding it
/// forever. 900ms was picked to comfortably cover a normal thinking pause
/// between words or scroll gestures while remaining well short of
/// "genuinely idle" (multiple seconds) -- see the #285/#288 press-to-photon
/// evidence this issue's earlier slices are built on for typical
/// inter-event timing.
const INTERACTION_BURST_ACTIVE_WINDOW_US: u64 = 900_000;

/// Pure hysteresis rule -- mirrors `viewer_demand.rs`'s
/// `update_occlusion_hysteresis` in spirit (one small, directly testable
/// function with no locking), just time-windowed instead of count-based
/// since interaction cadence is naturally timestamp-driven rather than
/// sample-driven.
fn interaction_burst_is_active(now_us: u64, last_applied_at_us: u64) -> bool {
    last_applied_at_us != 0
        && now_us.saturating_sub(last_applied_at_us) <= INTERACTION_BURST_ACTIVE_WINDOW_US
}

fn interaction_burst_active_for_window(window_id: u32, now_us: u64) -> bool {
    interaction_burst_last_applied_us()
        .lock_unpoisoned()
        .get(&window_id)
        .is_some_and(|&last| interaction_burst_is_active(now_us, last))
}

/// The floor the interaction-burst policy asks quality-tier selection
/// (`apply_quality`/`effective_share_quality`) to respect while a burst is
/// active for a window. `quality` is the minimum `ShareQuality` tier: never
/// below `Full` while a burst is active, so cadence (frame rate) never
/// degrades below a usable floor while someone is actively typing/
/// scrolling/dragging in the shared window -- EXCEPT `DataSaver`, whose
/// entire purpose is a resource cap the burst policy must never raise past.
///
/// `resolution_ceiling` records the OPPOSITE-of-steady-state resolution/
/// frame-rate tradeoff this policy makes when something has to give:
/// instead of protecting resolution and letting cadence degrade (the
/// steady-state default, and `SharpText`'s whole premise), an active burst
/// protects cadence and is willing to trade resolution for it -- resolution
/// is degraded before frame rate. `SharpText` opts out of the resolution
/// trim entirely (its whole point is preserving text sharpness), so it gets
/// `None` (no burst-imposed cap beyond its own steady-state choice).
/// `Responsive` and `Automatic` get the trade: `Responsive` already leans
/// this way at steady state (`SharePriority::capture_resolution` caps it to
/// `P1080`), and `Automatic`'s steady state otherwise defaults to native/
/// `Auto` resolution, which is the case most likely to actually need
/// trimming under an active burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InteractionBurstFloor {
    quality: ShareQuality,
    resolution_ceiling: Option<CaptureResolution>,
}

const INTERACTION_BURST_RESOLUTION_CEILING: CaptureResolution = CaptureResolution::P1080;

fn interaction_burst_floor(
    priority: SharePriority,
    burst_active: bool,
) -> Option<InteractionBurstFloor> {
    if !burst_active {
        return None;
    }
    match priority {
        // DataSaver's whole point is a resource cap; the burst policy must
        // never raise usage above it.
        SharePriority::DataSaver => None,
        SharePriority::SharpText => Some(InteractionBurstFloor {
            quality: ShareQuality::Full,
            resolution_ceiling: None,
        }),
        SharePriority::Automatic | SharePriority::Responsive => Some(InteractionBurstFloor {
            quality: ShareQuality::Full,
            resolution_ceiling: Some(INTERACTION_BURST_RESOLUTION_CEILING),
        }),
    }
}

/// Whichever of two capture-resolution caps is spatially tighter, treating
/// `Auto` (no explicit cap -- native size) as the loosest possible option.
/// Used to intersect a burst-imposed ceiling with whatever the steady-state
/// priority/resolution preference already selected, without needing to
/// change `CaptureResolution`'s own variant ordering or anything in
/// `transport/publisher.rs`.
fn tighter_capture_resolution(a: CaptureResolution, b: CaptureResolution) -> CaptureResolution {
    match (a.explicit_long_edge_cap(), b.explicit_long_edge_cap()) {
        (None, None) => a,
        (None, Some(_)) => b,
        (Some(_), None) => a,
        (Some(edge_a), Some(edge_b)) => {
            if edge_a <= edge_b {
                a
            } else {
                b
            }
        }
    }
}

/// Resolution-before-frame-rate degradation ordering (#290 step 5c): while a
/// burst is active, prefer trimming resolution over trimming frame rate --
/// intersect the steady-state resolution with the burst's ceiling (if the
/// priority has one) rather than letting cadence take the hit instead.
/// Returns `steady_state` unchanged when no burst is active or the priority
/// opts out (`SharpText`, `DataSaver`).
fn burst_effective_capture_resolution(
    priority: SharePriority,
    steady_state: CaptureResolution,
    burst_active: bool,
) -> CaptureResolution {
    match interaction_burst_floor(priority, burst_active).and_then(|floor| floor.resolution_ceiling)
    {
        Some(ceiling) => tighter_capture_resolution(steady_state, ceiling),
        None => steady_state,
    }
}

fn startup_capture_fps(priority: SharePriority) -> u32 {
    apply_startup_cadence_floor(priority.capture_fps(), priority.startup_cadence_floor())
}

fn apply_startup_cadence_floor(capture_fps: u32, startup_cadence_floor: u32) -> u32 {
    capture_fps.max(startup_cadence_floor)
}

/// Whether a `Stalled` verdict should be held rather than restarted.
///
/// `Stalled` means the source was CHANGING and then abruptly stopped, or the
/// 300s absolute backstop fired -- an idle source between 45s and 300s is
/// `IdleHealthy` and never reaches here (`raw_capture_watchdog_decision`).
/// So this predicate deliberately takes no idle input: an earlier draft of
/// #806 exempted an idle source here too, which was dead in the 45-300s band
/// (the decision has already diverted it) and actively wrong above 300s (it
/// repealed the very backstop that exists for a WRONG idle signal -- e.g.
/// hardware-accelerated or DRM content that `SCScreenshotManager` captures as
/// a static placeholder, so a changing window hashes identical forever and
/// looks idle).
///
/// What remains are the two real bounds: fresh pulls prove the capture path is
/// alive, and the 90s grace bounds how long a still-changing source may be
/// served by pulls alone at ~5s worst-case lag (2026-07-14) before its raw
/// stream must be recovered.
fn raw_capture_stall_hold(silent_for_us: u64, pulls_fresh: bool) -> bool {
    pulls_fresh
        && silent_for_us <= RAW_CAPTURE_STALL_HOLD_GRACE_US
        && silent_for_us <= RAW_CAPTURE_HARD_RESTART_THRESHOLD_US
}

fn raw_capture_watchdog_decision(
    now_us: u64,
    last_capture_wall_time_us: u64,
    source_appears_idle: bool,
) -> RawCaptureWatchdogDecision {
    let silent_for_us = now_us.saturating_sub(last_capture_wall_time_us);
    if silent_for_us > RAW_CAPTURE_HARD_RESTART_THRESHOLD_US {
        // Absolute safety net: even an "idle" stream gets one defensive restart
        // after a very long silence, in case the source-idle signal is wrong
        // and the stream really is wedged. Far rarer than the old 45s cadence.
        RawCaptureWatchdogDecision::Stalled { silent_for_us }
    } else if silent_for_us > RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US {
        if source_appears_idle {
            // Source stopped drawing (occluded/idle) — the stream is fine.
            RawCaptureWatchdogDecision::IdleHealthy { silent_for_us }
        } else {
            // Content was actively changing and then abruptly stopped with no
            // idle transition — that looks like a genuinely wedged stream.
            RawCaptureWatchdogDecision::Stalled { silent_for_us }
        }
    } else {
        RawCaptureWatchdogDecision::Healthy
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramePaceDecision {
    PushChanged,
    PushChangedAfterStatic,
    PushWarmup,
    PushRefresh,
    SkipStatic,
}

impl FramePaceDecision {
    fn as_log_label(self) -> &'static str {
        match self {
            Self::PushChanged => "push_changed",
            Self::PushChangedAfterStatic => "push_changed_after_static",
            Self::PushWarmup => "push_warmup",
            Self::PushRefresh => "push_refresh",
            Self::SkipStatic => "skip_static",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameFingerprint {
    width: u32,
    height: u32,
    bytes_per_row: usize,
    hash: u64,
}

#[derive(Debug)]
struct StaticFramePacer {
    /// Whether static-frame dedup is active. Defaults from
    /// `STATIC_FRAME_DEDUP_ENABLED` (currently off, per user directive); tests
    /// that exercise the dedup logic construct the pacer with this forced on.
    dedup_enabled: bool,
    last_fingerprint: Option<FrameFingerprint>,
    identical_frames: u32,
    last_pushed_capture_wall_time_us: u64,
    last_pushed_at: Option<Instant>,
}

impl Default for StaticFramePacer {
    fn default() -> Self {
        Self {
            dedup_enabled: STATIC_FRAME_DEDUP_ENABLED,
            last_fingerprint: None,
            identical_frames: 0,
            last_pushed_capture_wall_time_us: 0,
            last_pushed_at: None,
        }
    }
}

impl StaticFramePacer {
    /// A pacer with static-frame dedup forced ON, for tests that validate the
    /// dedup logic regardless of the production `STATIC_FRAME_DEDUP_ENABLED`.
    #[cfg(test)]
    fn with_dedup() -> Self {
        Self {
            dedup_enabled: true,
            ..Self::default()
        }
    }

    fn record_push(&mut self, capture_wall_time_us: u64, monotonic_now: Instant) {
        self.last_pushed_capture_wall_time_us = capture_wall_time_us;
        self.last_pushed_at = Some(monotonic_now);
    }

    fn observe(
        &mut self,
        frame: &crate::capture::CapturedFrame,
        capture_wall_time_us: u64,
        monotonic_now: Instant,
    ) -> FramePaceDecision {
        // Dedup disabled (see STATIC_FRAME_DEDUP_ENABLED): skip the full-frame
        // FNV fingerprint entirely. Perf-2 measured it as the single largest
        // per-frame CPU cost (~9-12 ms at 2376x1446 = ~30-40% of a P-core),
        // and with dedup off its result is never used to skip a frame. Keep a
        // cheap placeholder fingerprint populated so the idle-refresh re-push
        // still arms during SCK silence (see should_refresh_static_at).
        if !self.dedup_enabled {
            let bytes_per_row = frame
                .payload
                .primary_plane()
                .map(|(_data, bytes_per_row)| bytes_per_row)
                .unwrap_or(0);
            self.last_fingerprint = Some(FrameFingerprint {
                width: frame.width,
                height: frame.height,
                bytes_per_row,
                hash: 0,
            });
            self.identical_frames = 0;
            self.record_push(capture_wall_time_us, monotonic_now);
            return FramePaceDecision::PushChanged;
        }
        let fingerprint = frame_fingerprint(frame);
        if self.last_fingerprint != Some(fingerprint) {
            let was_static = self.identical_frames >= STATIC_SKIP_AFTER_IDENTICAL_FRAMES;
            self.last_fingerprint = Some(fingerprint);
            self.identical_frames = 0;
            self.record_push(capture_wall_time_us, monotonic_now);
            return if was_static {
                FramePaceDecision::PushChangedAfterStatic
            } else {
                FramePaceDecision::PushChanged
            };
        }

        self.identical_frames = self.identical_frames.saturating_add(1);
        if self.identical_frames < STATIC_SKIP_AFTER_IDENTICAL_FRAMES {
            self.record_push(capture_wall_time_us, monotonic_now);
            return FramePaceDecision::PushWarmup;
        }

        if capture_wall_time_us.saturating_sub(self.last_pushed_capture_wall_time_us)
            >= STATIC_REFRESH_INTERVAL_US
        {
            self.record_push(capture_wall_time_us, monotonic_now);
            FramePaceDecision::PushRefresh
        } else {
            FramePaceDecision::SkipStatic
        }
    }

    fn should_refresh_static_at(&mut self, monotonic_now: Instant, push_wall_time_us: u64) -> bool {
        let Some(last_pushed_at) = self.last_pushed_at else {
            return false;
        };
        if self.last_fingerprint.is_some()
            && monotonic_now.duration_since(last_pushed_at)
                >= Duration::from_micros(STATIC_REFRESH_INTERVAL_US)
        {
            self.record_push(push_wall_time_us, monotonic_now);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticPumpFrameDecision {
    Push(FramePaceDecision),
    SkipStatic,
}

#[derive(Default)]
struct StaticFramePumpState {
    pacer: StaticFramePacer,
    parked_static_frame: Option<(crate::capture::CapturedFrame, u64)>,
}

impl StaticFramePumpState {
    /// Pump state with static-frame dedup forced ON, for tests that validate
    /// the dedup/parking logic regardless of `STATIC_FRAME_DEDUP_ENABLED`.
    #[cfg(test)]
    fn with_dedup() -> Self {
        Self {
            pacer: StaticFramePacer::with_dedup(),
            parked_static_frame: None,
        }
    }

    fn parked_frame(&self) -> Option<(&crate::capture::CapturedFrame, u64)> {
        self.parked_static_frame
            .as_ref()
            .map(|(frame, capture_wall_time_us)| (frame, *capture_wall_time_us))
    }

    fn park_pushed_frame(
        &mut self,
        frame: crate::capture::CapturedFrame,
        capture_wall_time_us: u64,
    ) {
        self.parked_static_frame = Some((frame, capture_wall_time_us));
    }

    fn observe_captured_frame(
        &mut self,
        frame: crate::capture::CapturedFrame,
        capture_wall_time_us: u64,
        monotonic_now: Instant,
    ) -> StaticPumpFrameDecision {
        let decision = self
            .pacer
            .observe(&frame, capture_wall_time_us, monotonic_now);
        self.park_pushed_frame(frame, capture_wall_time_us);
        match decision {
            FramePaceDecision::SkipStatic => StaticPumpFrameDecision::SkipStatic,
            decision => StaticPumpFrameDecision::Push(decision),
        }
    }

    fn idle_refresh_frame_at(
        &mut self,
        monotonic_now: Instant,
        push_wall_time_us: u64,
    ) -> Option<&crate::capture::CapturedFrame> {
        self.parked_static_frame.as_ref()?;
        if self
            .pacer
            .should_refresh_static_at(monotonic_now, push_wall_time_us)
        {
            self.parked_static_frame
                .as_ref()
                .map(|(frame, _capture_wall_time_us)| frame)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirtyRectPushReason {
    FirstFrame,
    RemoteControl,
    DirtyRect,
    DirtyRectAfterSkip { skipped_frames: u64 },
    NonNormalStatus,
    SizeChanged,
    TierChanged,
    Disabled,
    // Fable finding 3: SCK did not deliver a dirtyRects attachment for this
    // frame (attachment missing, empty, or every rect failed to parse) --
    // this is NOT the same as SCK affirmatively reporting zero changed
    // pixels, and must not be treated as skippable. See
    // CapturedFrame::dirty_rects_known.
    DirtyRectsUnknown,
    // Fable finding 4: observe_captured_frame's own wall-clock refresh
    // floor. Without this, the ~1fps keepalive lives ONLY on the idle-tick
    // path (idle_refresh_frame_at), which never runs while raw frames keep
    // arriving faster than the tick interval -- exactly the regime this
    // feature targets (clean frames arriving that we choose not to encode).
    // A sustained >10fps clean-but-unencoded stream would then starve the
    // keepalive indefinitely, with no watchdog trip (the pump is alive, not
    // wedged) -- an unbounded silent freeze for late joiners/receivers.
    RefreshFloor,
}

impl DirtyRectPushReason {
    fn as_log_label(self) -> &'static str {
        match self {
            Self::FirstFrame => "push_first_frame",
            Self::RemoteControl => "push_remote_control",
            Self::DirtyRect => "push_dirty_rect",
            Self::DirtyRectAfterSkip { .. } => "push_dirty_rect_after_skip",
            Self::NonNormalStatus => "push_non_normal_status",
            Self::SizeChanged => "push_size_changed",
            Self::TierChanged => "push_tier_changed",
            Self::Disabled => "push_dirty_rect_skip_disabled",
            Self::DirtyRectsUnknown => "push_dirty_rects_unknown",
            Self::RefreshFloor => "push_refresh_floor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirtyRectFrameDecision {
    Push(DirtyRectPushReason),
    Skip { run_length: u64 },
}

/// #622: how many affirmatively-changed frame pushes are required before the
/// pump logs the `share liveness confirmed` marker that
/// `scripts/release-smoke.sh` asserts on. One changed push can be the window
/// merely appearing; a handful proves the content is actually moving.
const MOVING_FRAME_LIVENESS_THRESHOLD: u64 = 5;

/// #622: only push decisions that carry affirmative evidence of CHANGED
/// content count toward the release-smoke liveness marker: an SCK
/// dirty-rect-confirmed push, or a snapshot pull whose dense content hash
/// changed. `idle_static_refresh`, `push_refresh_floor`, `push_first_frame`,
/// remote-control force pushes, and the dirty-rect kill-switch path can all
/// fire on a completely static share, so they must never satisfy "the remote
/// peer receives moving frames".
fn push_decision_is_motion_evidence(decision: &str) -> bool {
    matches!(
        decision,
        "push_dirty_rect" | "push_dirty_rect_after_skip" | "pull_snapshot"
    )
}

/// #622: once-per-share latch for the `share liveness confirmed` log marker.
/// Pure state machine so the threshold/latch logic is unit-testable without
/// capturing log output.
#[derive(Default)]
struct MovingFrameLiveness {
    moving_pushes: u64,
    logged: bool,
}

impl MovingFrameLiveness {
    /// Returns `Some(count)` exactly once, on the push that crosses the
    /// threshold; the caller logs the marker then.
    fn observe(&mut self, decision: &str) -> Option<u64> {
        if !push_decision_is_motion_evidence(decision) {
            return None;
        }
        self.moving_pushes += 1;
        if !self.logged && self.moving_pushes >= MOVING_FRAME_LIVENESS_THRESHOLD {
            self.logged = true;
            return Some(self.moving_pushes);
        }
        None
    }
}

// CapturedFrame does not implement Debug (it holds a raw platform frame
// handle), so this struct cannot derive it; nothing formats it today.
#[derive(Default)]
struct DirtyRectPumpState {
    parked_frame: Option<(crate::capture::CapturedFrame, u64)>,
    last_pushed_capture_wall_time_us: u64,
    last_pushed_at: Option<Instant>,
    last_pushed_size: Option<(u32, u32)>,
    last_pushed_quality: Option<ShareQuality>,
    skipped_frames: u64,
}

fn dirty_rect_skip_enabled() -> bool {
    std::env::var(DIRTY_RECT_SKIP_ENV).as_deref() != Ok("1")
}

fn frame_status_allows_dirty_rect_skip(
    frame_status: Option<screencapturekit::cm::SCFrameStatus>,
) -> bool {
    !matches!(
        frame_status,
        Some(
            screencapturekit::cm::SCFrameStatus::Idle
                | screencapturekit::cm::SCFrameStatus::Blank
                | screencapturekit::cm::SCFrameStatus::Suspended
        )
    )
}

impl DirtyRectPumpState {
    fn parked_frame(&self) -> Option<(&crate::capture::CapturedFrame, u64)> {
        self.parked_frame
            .as_ref()
            .map(|(frame, capture_wall_time_us)| (frame, *capture_wall_time_us))
    }

    fn skip_run_length(&self) -> u64 {
        self.skipped_frames
    }

    fn mark_pushed(
        &mut self,
        frame_size: (u32, u32),
        capture_wall_time_us: u64,
        monotonic_now: Instant,
        quality: ShareQuality,
    ) {
        self.last_pushed_capture_wall_time_us = capture_wall_time_us;
        self.last_pushed_at = Some(monotonic_now);
        self.last_pushed_size = Some(frame_size);
        self.last_pushed_quality = Some(quality);
        self.skipped_frames = 0;
    }

    fn force_push_frame(
        &mut self,
        frame: crate::capture::CapturedFrame,
        capture_wall_time_us: u64,
        monotonic_now: Instant,
        quality: ShareQuality,
    ) {
        self.parked_frame = Some((frame, capture_wall_time_us));
        let frame_size = self
            .parked_frame
            .as_ref()
            .map(|(frame, _)| (frame.width, frame.height))
            .expect("frame was parked");
        self.mark_pushed(frame_size, capture_wall_time_us, monotonic_now, quality);
    }

    fn observe_captured_frame(
        &mut self,
        frame: crate::capture::CapturedFrame,
        capture_wall_time_us: u64,
        monotonic_now: Instant,
        quality: ShareQuality,
        skip_enabled: bool,
        remote_control_active: bool,
    ) -> DirtyRectFrameDecision {
        let frame_size = (frame.width, frame.height);
        let dirty_rect_count = frame.dirty_rect_count;
        let dirty_rects_known = frame.dirty_rects_known;
        let normal_status = frame_status_allows_dirty_rect_skip(frame.frame_status);
        self.parked_frame = Some((frame, capture_wall_time_us));

        let reason = if self.last_pushed_at.is_none() {
            Some(DirtyRectPushReason::FirstFrame)
        } else if remote_control_active {
            Some(DirtyRectPushReason::RemoteControl)
        } else if self.last_pushed_size != Some(frame_size) {
            Some(DirtyRectPushReason::SizeChanged)
        } else if self.last_pushed_quality != Some(quality) {
            Some(DirtyRectPushReason::TierChanged)
        } else if !skip_enabled {
            Some(DirtyRectPushReason::Disabled)
        } else if dirty_rect_count > 0 {
            if self.skipped_frames > 0 {
                Some(DirtyRectPushReason::DirtyRectAfterSkip {
                    skipped_frames: self.skipped_frames,
                })
            } else {
                Some(DirtyRectPushReason::DirtyRect)
            }
        } else if !dirty_rects_known {
            // Fable finding 3: dirty_rect_count == 0 here means EITHER "SCK
            // affirmed nothing changed" OR "the dirtyRects attachment was
            // missing/unparseable" -- these must not be treated the same.
            // Fail safe: unknown is always pushed, never silently skipped.
            Some(DirtyRectPushReason::DirtyRectsUnknown)
        } else if !normal_status {
            Some(DirtyRectPushReason::NonNormalStatus)
        } else if capture_wall_time_us.saturating_sub(self.last_pushed_capture_wall_time_us)
            >= STATIC_REFRESH_INTERVAL_US
        {
            // Fable finding 4: force the ~1fps refresh floor here too, not
            // only on the idle-tick path -- see DirtyRectPushReason::RefreshFloor.
            Some(DirtyRectPushReason::RefreshFloor)
        } else {
            None
        };

        let Some(reason) = reason else {
            self.skipped_frames = self.skipped_frames.saturating_add(1);
            return DirtyRectFrameDecision::Skip {
                run_length: self.skipped_frames,
            };
        };

        self.mark_pushed(frame_size, capture_wall_time_us, monotonic_now, quality);
        DirtyRectFrameDecision::Push(reason)
    }

    fn idle_refresh_frame_at(
        &mut self,
        monotonic_now: Instant,
        push_wall_time_us: u64,
        quality: ShareQuality,
    ) -> Option<&crate::capture::CapturedFrame> {
        let (frame, _) = self.parked_frame.as_ref()?;
        let frame_size = (frame.width, frame.height);
        let last_pushed_at = self.last_pushed_at?;
        if monotonic_now.duration_since(last_pushed_at)
            < Duration::from_micros(STATIC_REFRESH_INTERVAL_US)
            && push_wall_time_us.saturating_sub(self.last_pushed_capture_wall_time_us)
                < STATIC_REFRESH_INTERVAL_US
        {
            return None;
        }
        // Fable finding 2: record the wall time of THIS refresh push
        // (push_wall_time_us), not the parked frame's original (frozen)
        // capture timestamp. During genuine SCK silence the parked frame is
        // never replaced, so re-recording its frozen capture time here would
        // make the wall-clock disjunct above permanently true after the
        // first refresh -- firing a full re-encode push on every idle tick
        // forever instead of once per STATIC_REFRESH_INTERVAL_US.
        self.mark_pushed(frame_size, push_wall_time_us, monotonic_now, quality);
        self.parked_frame.as_ref().map(|(frame, _)| frame)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdaptiveIdleTick {
    interval: Duration,
}

impl Default for AdaptiveIdleTick {
    fn default() -> Self {
        Self {
            interval: ADAPTIVE_IDLE_TICK_BASE,
        }
    }
}

impl AdaptiveIdleTick {
    fn interval(self) -> Duration {
        self.interval
    }

    fn reset_on_wake(&mut self) {
        self.interval = ADAPTIVE_IDLE_TICK_BASE;
    }

    fn back_off_after_empty_idle_tick(&mut self, slot_empty: bool, snapshot_pull_armed: bool) {
        // #381 fix (Fable finding 1): a completed snapshot pull (#183's
        // fallback) is itself a form of forward progress and must reset the
        // interval back to the base pace, not merely freeze further backoff.
        // Without this, an idle window that backs off BEFORE the pull
        // engages (SNAPSHOT_PULL_AFTER_SILENCE_US) stays backed off for the
        // pull's entire lifetime, permanently degrading the ~10fps fallback
        // to as low as 2fps with nothing to ever reset it.
        if snapshot_pull_armed {
            self.reset_on_wake();
        } else if slot_empty {
            self.interval = (self.interval * 2).min(ADAPTIVE_IDLE_TICK_MAX);
        }
    }
}

fn frame_fingerprint(frame: &crate::capture::CapturedFrame) -> FrameFingerprint {
    let Some((data, bytes_per_row)) = frame.payload.primary_plane() else {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = fnv1a_u64(hash, &frame.width.to_le_bytes());
        hash = fnv1a_u64(hash, &frame.height.to_le_bytes());
        hash = fnv1a_u64(hash, &(frame.dirty_rect_count as u64).to_le_bytes());
        hash = fnv1a_u64(hash, &frame.dirty_area_px.to_le_bytes());
        let status = frame.frame_status.map(|status| status as i32).unwrap_or(-1);
        hash = fnv1a_u64(hash, &status.to_le_bytes());
        return FrameFingerprint {
            width: frame.width,
            height: frame.height,
            bytes_per_row: 0,
            hash,
        };
    };
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    hash = fnv1a_u64(hash, &frame.width.to_le_bytes());
    hash = fnv1a_u64(hash, &frame.height.to_le_bytes());
    hash = fnv1a_u64(hash, &bytes_per_row.to_le_bytes());

    let height = frame.height as usize;
    let visible_row_bytes = match &frame.payload {
        crate::capture::CapturedFramePayload::Bgra { .. } => {
            (frame.width as usize).saturating_mul(4)
        }
        crate::capture::CapturedFramePayload::Nv12 { .. } => frame.width as usize,
        crate::capture::CapturedFramePayload::Native { .. } => 0,
    };
    if height == 0 || visible_row_bytes == 0 || bytes_per_row == 0 {
        return FrameFingerprint {
            width: frame.width,
            height: frame.height,
            bytes_per_row,
            hash,
        };
    }

    for row in 0..height {
        hash = hash_frame_row(hash, data, bytes_per_row, row, visible_row_bytes);
    }

    FrameFingerprint {
        width: frame.width,
        height: frame.height,
        bytes_per_row,
        hash,
    }
}

fn hash_frame_row(
    hash: u64,
    data: &[u8],
    bytes_per_row: usize,
    row: usize,
    visible_row_bytes: usize,
) -> u64 {
    let Some(start) = row.checked_mul(bytes_per_row) else {
        return hash;
    };
    let Some(end) = start.checked_add(visible_row_bytes) else {
        return hash;
    };
    let end = end.min(data.len());
    if start >= end || start >= data.len() {
        return hash;
    }
    fnv1a_u64(hash, &data[start..end])
}

fn fnv1a_u64(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Live capture-freeze diagnostics (#capture-freeze). Tracks whether the raw
/// SCK frames arriving in `on_frame` actually carry NEW content, so a viewer
/// seeing a frozen image can be explained without guessing: the source app is
/// not drawing (occluded/idle -- frames still arrive but SCK reports 0 dirty
/// rects and the pixel hash stops moving) vs. a genuinely dead/wedged stream
/// (no frames at all -- caught separately by the raw-capture watchdog).
#[derive(Default)]
struct CaptureFreezeDiag {
    last_hash: u64,
    unchanged_run: u64,
    last_log_us: u64,
    last_log_captured: u64,
    reported_frozen: bool,
    last_occlusion_fraction: Option<f64>,
}

impl CaptureFreezeDiag {
    fn observe_sample(&mut self, sample: CaptureFreezeSample) -> bool {
        if sample.hash != self.last_hash {
            self.unchanged_run = 0;
            self.last_hash = sample.hash;
        } else if sample.pixels_sampled {
            self.unchanged_run = self.unchanged_run.saturating_add(1);
        } else {
            self.unchanged_run = 0;
        }
        self.unchanged_run >= CAPTURE_FREEZE_RUN_THRESHOLD
    }
}

fn capture_state_report(
    frame: &crate::capture::CapturedFrame,
    frozen_now: bool,
    occlusion_fraction: Option<f64>,
) -> crate::diagnostics::CaptureStateReport {
    use crate::diagnostics::{CaptureCpuMetrics, CaptureStateKind, CaptureStateReport};
    use screencapturekit::cm::SCFrameStatus;

    let idle_status = matches!(
        frame.frame_status,
        Some(SCFrameStatus::Idle | SCFrameStatus::Blank | SCFrameStatus::Suspended)
    );
    let occluded = frozen_now && occlusion_fraction.is_some_and(|fraction| fraction >= 0.95);
    let state = if occluded {
        CaptureStateKind::Occluded
    } else if frozen_now || idle_status {
        CaptureStateKind::Idle
    } else {
        CaptureStateKind::Live
    };

    CaptureStateReport {
        state,
        fps: None,
        dirty_rect_count: Some(frame.dirty_rect_count.min(u32::MAX as usize) as u32),
        dirty_area_px: Some(frame.dirty_area_px),
        occlusion_pct: occlusion_fraction.map(|fraction| (fraction.clamp(0.0, 1.0)) * 100.0),
        cpu: CaptureCpuMetrics {
            lock_copy_ms: Some(frame.lock_copy_ms),
            convert_ms: None,
            capture_frame_return_ms: None,
        },
    }
}

/// How many consecutive identical frames count as "frozen" for the diagnostic
/// verdict (~0.5s at 10fps; capture targets up to 30fps).
const CAPTURE_FREEZE_RUN_THRESHOLD: u64 = 5;

/// Cheap strided FNV hash over a captured frame's primary/Y plane -- samples ~1 byte per
/// 2KB so a multi-MB frame hashes in microseconds. Freshness signal only, not
/// an exact-equality check (a change smaller than the stride can be missed;
/// SCK's `dirty_rect_count` is the exact companion signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaptureFreezeSample {
    hash: u64,
    // Native payloads are not locked here, so their metadata hash cannot prove
    // pixel equality and must never drive idle inference (#548).
    pixels_sampled: bool,
}

fn capture_freeze_sample(frame: &crate::capture::CapturedFrame) -> CaptureFreezeSample {
    let Some((data, _bytes_per_row)) = frame.payload.primary_plane() else {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = fnv1a_u64(hash, &frame.width.to_le_bytes());
        hash = fnv1a_u64(hash, &frame.height.to_le_bytes());
        hash = fnv1a_u64(hash, &(frame.dirty_rect_count as u64).to_le_bytes());
        hash = fnv1a_u64(hash, &frame.dirty_area_px.to_le_bytes());
        let status = frame.frame_status.map(|status| status as i32).unwrap_or(-1);
        return CaptureFreezeSample {
            hash: fnv1a_u64(hash, &status.to_le_bytes()),
            pixels_sampled: false,
        };
    };
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0usize;
    while i < data.len() {
        hash ^= u64::from(data[i]);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 2048;
    }
    CaptureFreezeSample {
        hash,
        pixels_sampled: true,
    }
}

fn capture_source_appears_idle(
    frame: &crate::capture::CapturedFrame,
    frozen_now: bool,
    pixels_sampled: bool,
) -> bool {
    use screencapturekit::cm::SCFrameStatus;

    if matches!(
        frame.frame_status,
        Some(SCFrameStatus::Idle | SCFrameStatus::Blank | SCFrameStatus::Suspended)
    ) {
        return true;
    }
    if frame.dirty_rects_known && frame.dirty_rect_count > 0 {
        return false;
    }
    frozen_now && pixels_sampled
}

#[cfg(test)]
fn capture_freeze_hash(frame: &crate::capture::CapturedFrame) -> u64 {
    capture_freeze_sample(frame).hash
}

/// Pure version of `SessionInner::focused_window`'s "highest `started_seq`
/// wins" rule, taking `(window_id, started_seq)` pairs directly so the
/// policy is unit-testable without constructing a real `ActiveShare` (which
/// owns a live `WindowCapture`/`JoinHandle` and can't be faked without a
/// running capture session).
pub(super) fn focused_window_of(shares: impl Iterator<Item = (u32, u64)>) -> Option<u32> {
    shares.max_by_key(|(_, seq)| *seq).map(|(id, _)| id)
}

#[derive(Debug, Clone)]
struct SourceWindowInfo {
    title: String,
    url: Option<String>,
    /// #915: bundle id and unformatted on-screen title, captured from the
    /// SAME `window_source::list()` lookup as `title`/`url` above --
    /// `spawn_share_url_refresh` reuses these at the fresh-start call site
    /// instead of re-enumerating every on-screen window a second time on
    /// the share-start path (the exact blocking-enumeration class #915
    /// removes; see `browser_extraction_target`'s doc comment for the one
    /// remaining caller that still needs a standalone lookup).
    bundle_id: Option<String>,
    raw_title: Option<String>,
}

/// #712 Fable follow-up (non-blocking): `window_id` here can be a TAGGED
/// display source id (`DISPLAY_SOURCE_MARKER | CGDirectDisplayID`), not just
/// a raw `CGWindowID` -- normally `crate::window_source::list()` below
/// already resolves that to a friendly "Screen N — Display" (it lists
/// displays with exactly this tagged id, see `window_source::list`'s
/// "Displays first" block), but the two fallback branches (enumeration
/// failed, or this specific id wasn't in the list -- e.g. a transient
/// enumeration race right after system wake) used to unconditionally print
/// the raw tagged `u32` as `"Window 1073741825"`, which is both an ugly
/// number and the wrong noun for a display. Label it "Screen <raw id>"
/// instead when the tag bit says so.
fn fallback_source_title(window_id: u32) -> String {
    if crate::window_source::is_display_source_id(window_id) {
        format!(
            "Screen {}",
            crate::window_source::display_id_from_source_id(window_id)
        )
    } else {
        format!("Window {window_id}")
    }
}

fn source_info_for_window(window_id: u32) -> SourceWindowInfo {
    match crate::window_source::list() {
        Ok(windows) => {
            let Some(w) = windows.into_iter().find(|w| w.window_id == window_id) else {
                return SourceWindowInfo {
                    title: fallback_source_title(window_id),
                    url: None,
                    bundle_id: None,
                    raw_title: None,
                };
            };
            let raw_title = w
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            let title = match raw_title.as_deref() {
                Some(title) => format!("{} — {}", title, w.app_name),
                None => w.app_name,
            };
            // #915: URL extraction no longer runs on the share-start path --
            // it used to block a tokio worker on a synchronous `osascript`
            // call (900ms, soon up to 60s for a cold browser/consent
            // prompt) for every browser share. Metadata is published with
            // `url: None` immediately; `spawn_share_url_refresh` fills it in
            // later, off this path, once the share is registered.
            SourceWindowInfo {
                title,
                url: None,
                bundle_id: Some(w.app_bundle_id),
                raw_title,
            }
        }
        Err(e) => {
            log::warn!("session: couldn't resolve source title for window {window_id}: {e}");
            SourceWindowInfo {
                title: fallback_source_title(window_id),
                url: None,
                bundle_id: None,
                raw_title: None,
            }
        }
    }
}

/// The (bundle id, raw on-screen title) pair `browser_url` needs to attempt
/// URL extraction for a shared window, when neither is already on hand.
/// `spawn_share_url_refresh`'s FRESH-start caller reuses
/// `source_info_for_window`'s own lookup (via `SourceWindowInfo::bundle_id`/
/// `raw_title`) instead of calling this a second time -- re-enumerating
/// every on-screen window twice on the share-start path is exactly the
/// blocking-enumeration class #915 removes. The in-place-capture-restart
/// caller has no `SourceWindowInfo` handy, so it calls this instead, off
/// the async path via `spawn_blocking` (the window's bundle id never
/// changes across a restart, but calling this again is simpler than
/// threading a cached bundle id through `ShareRestartSnapshot`, and the
/// title only matters as a first-tick fallback now that the poller reads
/// the live title on every attempt -- see `effective_title`).
fn browser_extraction_target(window_id: u32) -> Option<(String, Option<String>)> {
    let windows = crate::window_source::list().ok()?;
    let w = windows.into_iter().find(|w| w.window_id == window_id)?;
    let raw_title = w
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Some((w.app_bundle_id, raw_title))
}

/// #915: resolve the title to use for one refresh-poller tick. `live` is a
/// fresh `platform::cg::name_for_window_id` read (`None` when the window
/// currently has no title, e.g. transiently during a resize, or the id no
/// longer resolves); `start` is the title captured once at share start (or
/// at the last in-place capture restart), kept only as a fallback for that
/// case. `live` wins whenever it has one -- that's what lets
/// `run_url_refresh`'s freshness-skip check notice a REAL title change
/// (Chrome retitles the window on every navigation) instead of being fed a
/// value that can never change, which would make the exact-title AppleScript
/// match miss forever after the sharer's first navigation.
fn effective_title(live: Option<String>, start: Option<&str>) -> Option<String> {
    live.or_else(|| start.map(str::to_string))
}

/// #915: an in-flight browser-URL refresh poller, together with the
/// `CancellationToken` `run_url_refresh` checks between/during extraction
/// attempts. `stop()` signals both: `cancel` first, so a live `run_url_refresh`
/// iteration unwinds cleanly at its own `select!` points (the polite path,
/// matching the never-cancelled token this used to carry before this fix);
/// then `handle.abort()` as the hard backstop for the one case `cancel`
/// cannot reach -- a `spawn_blocking` extraction already dispatched to a
/// blocking-pool thread, which `run_url_refresh`'s `select!` cannot preempt
/// (the child `osascript` process is left to finish on its own thread
/// regardless; only its result is discarded).
struct UrlRefreshTask {
    handle: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

impl UrlRefreshTask {
    fn stop(&self) {
        self.cancel.cancel();
        self.handle.abort();
    }
}

/// Spawn the macOS twin of Windows' `start_share_url_refresh`
/// (`session_stub.rs`) (#915). `eligible` is the caller's precomputed "this
/// share is a browser-window share that wasn't given an explicit
/// `source_title_override`" gate -- callers decide it differently at fresh
/// start (kind + override) vs. in-place capture restart (whether a poller
/// was already running before the restart), so it isn't recomputed here.
/// `bundle_id`/`start_title` are the caller's own already-resolved
/// `window_source::list()` lookup (`SourceWindowInfo::bundle_id`/
/// `raw_title` at fresh start; a `spawn_blocking`'d
/// `browser_extraction_target` at in-place capture restart) -- this
/// function never enumerates on-screen windows itself, so it never blocks
/// its caller's async task.
///
/// The URL is intentionally NOT extracted on the share-start path (see
/// `source_info_for_window`) -- this poller fills it in afterward, off that
/// path, so a slow/denied/failed extraction never blocks `start_share`. The
/// sink is generation-checked
/// (`RoomConnection::set_shared_window_url_for_generation`), so a poll that
/// lands after this exact generation stops sharing is a guaranteed no-op
/// even if this task is somehow not yet stopped (#298) -- see the `#915`
/// comment on `ActiveShare::url_refresh` for why every teardown path must
/// still stop it anyway (a leaked poller keeps running and keeps calling
/// `osascript` for no reason). A `Denied` outcome (`-1743`, no Automation
/// consent) is terminal per SHARE, not per process: each new browser share
/// re-runs exactly one fresh attempt (fast -1743 exit, one warn, one
/// rate-limited diagnostic event) before that share's own poller gives up
/// -- intended, since Automation consent can be granted or revoked between
/// shares and there is no cheap way to know which without asking.
fn spawn_share_url_refresh(
    room_connection: Arc<RoomConnection>,
    metadata_apply_lock: Arc<tokio::sync::Mutex<()>>,
    window_id: u32,
    started_seq: u64,
    eligible: bool,
    bundle_id: Option<String>,
    start_title: Option<String>,
) -> Option<UrlRefreshTask> {
    if !eligible {
        return None;
    }
    let Some(bundle_id) = bundle_id else {
        log::warn!("session: window {window_id} has no browser URL target; no refresh poller");
        return None;
    };
    if !crate::browser_url::is_supported_bundle_id(&bundle_id) {
        return None;
    }
    log::debug!(
        "session: window {window_id} spawning browser URL refresh (seq {started_seq}, bundle {bundle_id})"
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let run_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let cfg = crate::session::url_refresh::UrlRefreshConfig {
            first_attempt_timeout: crate::browser_url::FIRST_ATTEMPT_TIMEOUT,
            poll_timeout: crate::browser_url::POLL_TIMEOUT,
            poll_interval: crate::browser_url::POLL_INTERVAL,
            fresh_url_ttl: crate::browser_url::FRESH_URL_TTL,
        };
        let extract = {
            let bundle_id = bundle_id.clone();
            let start_title = start_title.clone();
            move |attempt: crate::session::url_refresh::Attempt| {
                let bundle_id = bundle_id.clone();
                // Re-read the LIVE title on every attempt, not just the one
                // captured at spawn time: Chrome retitles its window on
                // every navigation, and the AppleScript match is exact-title
                // (#97), so feeding it a stale start-time title would make
                // every extraction after the sharer's first navigation come
                // back `Empty` for the rest of the share.
                let title = effective_title(
                    crate::platform::cg::name_for_window_id(window_id),
                    start_title.as_deref(),
                );
                async move {
                    // Mandatory: the runner sleeps on a thread
                    // (`osascript` child process, up to 60s on the first
                    // attempt) -- never call this inline on the async
                    // runtime (see CLAUDE.md's crash-class #3).
                    tokio::task::spawn_blocking(move || {
                        crate::browser_url::extract_url_for_window(
                            &bundle_id,
                            title.as_deref(),
                            attempt.timeout,
                        )
                    })
                    .await
                    .unwrap_or_else(|error| {
                        crate::browser_url::UrlExtraction::Spawn(error.to_string())
                    })
                }
            }
        };
        // Re-read the live title each poll too (same `effective_title`
        // fallback as `extract` above), so `run_url_refresh`'s
        // freshness-skip check can actually observe a real title change
        // instead of comparing the start-time title against itself forever.
        // `platform::cg::name_for_window_id` is the cheap single-window
        // CGWindowID query (checked -- no cheaper accessor exists in
        // `window_registry.rs`); `window_source::list()`/
        // `platform::cg::onscreen_windows_lean()` walk every on-screen
        // window and are exactly the per-poll cost this refresh loop exists
        // to avoid paying at a 3s cadence.
        let current_title = {
            let start_title = start_title.clone();
            move || {
                effective_title(
                    crate::platform::cg::name_for_window_id(window_id),
                    start_title.as_deref(),
                )
            }
        };
        let sink = {
            let room_connection = room_connection.clone();
            let metadata_apply_lock = metadata_apply_lock.clone();
            move |url: Option<String>| {
                let room_connection = room_connection.clone();
                let metadata_apply_lock = metadata_apply_lock.clone();
                async move {
                    let _metadata_apply_guard = metadata_apply_lock.lock().await;
                    room_connection
                        .set_shared_window_url_for_generation(window_id, started_seq, url)
                        .await
                }
            }
        };
        let on_failure = |outcome: &crate::browser_url::UrlExtraction, first_for_share: bool| {
            crate::browser_url::log_extraction_failure(window_id, outcome, first_for_share);
        };
        let exit = crate::session::url_refresh::run_url_refresh(
            cfg,
            extract,
            current_title,
            sink,
            run_cancel,
            on_failure,
        )
        .await;
        log::debug!(
            "session: window {window_id} browser URL refresh exited ({exit:?}) (seq {started_seq})"
        );
    });
    Some(UrlRefreshTask { handle, cancel })
}

fn warn_if_denylisted_share_target(window_id: u32) {
    let Some(pid) = crate::window_registry::global()
        .map(|r| r.owner_pid_fresh(window_id))
        .unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(window_id))
    else {
        return;
    };
    let Some(bundle_id) = crate::share_target::bundle_id_for_pid(pid) else {
        return;
    };
    if crate::share_target::is_denylisted_bundle_id(&bundle_id) {
        log::warn!(
            "session: start_share(window {window_id}) targeted denylisted bundle {bundle_id}; this should be unreachable"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeDecision {
    MatchingPublishedSize,
    WaitingForStableSize { width: u32, height: u32, frames: u8 },
    StableResize { width: u32, height: u32 },
}

/// Whether the pump should fall through to the normal push path after
/// observing a `ResizeDecision`, or `continue` past it and drop this frame
/// entirely (#714).
///
/// Before #714, `WaitingForStableSize` mapped to `SkipThisFrame` -- every
/// captured frame during an in-progress window resize was discarded, with
/// nothing reaching the receiver, for as long as the resize took to
/// stabilize (`RESIZE_REPUBLISH_STABLE_FRAMES` consecutive identical
/// frames) plus the unpublish/republish round trip. A slow or continuous
/// drag-resize gesture can easily run past that stabilization window with
/// the debounce candidate never settling, producing a multi-second
/// viewer-visible freeze -- the real, still-reachable defect issue #714
/// tracks. (On current `main` this skip alone can't trip the pump-stall
/// watchdog: `pump_activity_wall_time_us` is refreshed every loop
/// iteration including skipped ones, and again on the idle tick during
/// total silence. The "frame pump stalled for 6.0s; restarting capture in
/// place" watchdog log Sentry originally recorded came from a shipped
/// build whose NV12 routing predates the refactor that made that log-line
/// path dead for window shares -- see `push_nv12`'s doc comment. This
/// fix's value is the freeze itself, independent of that specific log
/// line's provenance.)
///
/// Every variant now pushes: `PublishedTrack::push_frame`
/// (`transport/publisher.rs`) letterbox-scales a mismatched-size frame to
/// the currently published size instead of either dropping it or pushing
/// it raw, so the encoder's input size stays constant while this debounce
/// is still deciding whether/when to republish.
///
/// Kept as an explicit, separately-tested mapping (and threaded through the
/// real pump loop below, not just inlined into the match arms) so a future
/// change that reintroduces a frame-skipping `continue` during a resize
/// breaks this function's own test instead of silently regressing inside
/// the ~2,000-line pump loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizePumpAction {
    Push,
    SkipThisFrame,
}

fn resize_pump_action(decision: ResizeDecision) -> ResizePumpAction {
    match decision {
        ResizeDecision::MatchingPublishedSize
        | ResizeDecision::WaitingForStableSize { .. }
        | ResizeDecision::StableResize { .. } => ResizePumpAction::Push,
    }
}

#[derive(Debug, Default)]
struct ResizeDebounce {
    candidate: Option<(u32, u32)>,
    frames: u8,
}

impl ResizeDebounce {
    fn observe(
        &mut self,
        published_width: u32,
        published_height: u32,
        frame_width: u32,
        frame_height: u32,
    ) -> ResizeDecision {
        if frame_width == published_width && frame_height == published_height {
            self.candidate = None;
            self.frames = 0;
            return ResizeDecision::MatchingPublishedSize;
        }

        let size = (frame_width, frame_height);
        if self.candidate == Some(size) {
            self.frames = self.frames.saturating_add(1);
        } else {
            self.candidate = Some(size);
            self.frames = 1;
        }

        if self.frames >= RESIZE_REPUBLISH_STABLE_FRAMES {
            ResizeDecision::StableResize {
                width: frame_width,
                height: frame_height,
            }
        } else {
            ResizeDecision::WaitingForStableSize {
                width: frame_width,
                height: frame_height,
                frames: self.frames,
            }
        }
    }

    fn reset(&mut self) {
        self.candidate = None;
        self.frames = 0;
    }
}

impl SessionState {
    /// Snapshot of `(window_id, frame)` for every currently-shared window,
    /// plus the joined room connection and the real local
    /// identity, if this process has joined a room. Read by
    /// `telepointer.rs`'s cursor-poll loop (SPEC.md §4.5) to hit-test the
    /// local cursor against shared-window bounds and to publish pointer
    /// updates (tagged with the real identity, not `DEV_USER_ID`) on the same
    /// room every other share already uses -- no new connection, no new
    /// state, just a read-only view onto what `join_room`/`start_share`/
    /// `stop_share` already track.
    pub(crate) fn shared_windows_snapshot(
        &self,
    ) -> (
        Option<Arc<RoomConnection>>,
        Option<String>,
        Vec<(u32, crate::hover_tab::WindowFrame)>,
    ) {
        let guard = self.inner.lock_unpoisoned();
        let frames = guard
            .shares
            .iter()
            .map(|(id, share)| (*id, *share.frame.lock_unpoisoned()))
            .collect();
        let room_connection = guard.joined.as_ref().map(|j| j.room_connection.clone());
        let identity = guard.joined.as_ref().map(|j| j.identity.clone());
        (room_connection, identity, frames)
    }

    /// Current frame of one active local share, if `window_id` is one of this
    /// process's real shared windows. Used by remote control to reject camera
    /// synthetic ids and stale/off-target packets before replaying input.
    pub(crate) fn active_share_frame(
        &self,
        window_id: u32,
    ) -> Option<crate::hover_tab::WindowFrame> {
        let guard = self.inner.lock_unpoisoned();
        guard
            .shares
            .get(&window_id)
            .map(|share| *share.frame.lock_unpoisoned())
    }

    /// Human title of a LOCAL active share (the consent prompt's
    /// "<name> wants to control <title>"). None when not sharing that id.
    pub(crate) fn active_share_source_title(&self, window_id: u32) -> Option<String> {
        let guard = self.inner.lock_unpoisoned();
        guard
            .shares
            .get(&window_id)
            .map(|share| share.source_title.clone())
            .filter(|title| !title.trim().is_empty())
    }

    pub(crate) fn active_share_pid(&self, window_id: u32) -> Option<i32> {
        let guard = self.inner.lock_unpoisoned();
        guard.shares.get(&window_id).and_then(|share| share.pid)
    }

    pub(crate) fn shared_window_screen_status(&self, window_id: u32) -> SharedWindowScreenStatus {
        let share_status = {
            let guard = self.inner.lock_unpoisoned();
            guard.shares.get(&window_id).map(|share| {
                (
                    *share.frame.lock_unpoisoned(),
                    share.visible_on_screen.load(Ordering::Relaxed),
                    share.known_closed.load(Ordering::Relaxed),
                )
            })
        };
        let Some((frame, visible_on_screen, known_closed)) = share_status else {
            return SharedWindowScreenStatus::NotShared;
        };
        screen_status_from_flags(frame, visible_on_screen, known_closed)
    }

    /// Apply fresh on-screen frames to currently-shared windows (issue #30:
    /// keep `ActiveShare::frame` from going stale after a move/resize).
    /// `fresh` is `(window_id, current_frame)` pairs -- typically the output
    /// of `telepointer::frames_to_apply` over one `CGWindowList` snapshot.
    /// Ids in `fresh` that aren't currently shared are ignored (the share
    /// may have stopped between snapshot and apply); shared ids absent from
    /// `fresh` keep their last-known frame (see `ActiveShare::frame`'s doc
    /// comment for the off-screen rationale). Pure state write -- no AppKit,
    /// safe from any thread.
    pub(crate) fn update_share_frames(&self, fresh: &[(u32, crate::hover_tab::WindowFrame)]) {
        if fresh.is_empty() {
            return;
        }
        let guard = self.inner.lock_unpoisoned();
        for (window_id, frame) in fresh {
            if let Some(share) = guard.shares.get(window_id) {
                let mut stored = share.frame.lock_unpoisoned();
                if *stored != *frame {
                    *stored = *frame;
                    crate::remote_control::update_control_frame(*window_id, *frame);
                }
            }
        }
    }

    pub(crate) fn update_share_frames_and_visibility(
        &self,
        fresh: &[(u32, crate::hover_tab::WindowFrame)],
        visible_window_ids: &[u32],
    ) {
        self.update_share_frames_and_visibility_with(fresh, visible_window_ids, &CgWindowExistence);
    }

    /// Seam-injected form of [`Self::update_share_frames_and_visibility`] (#742).
    /// Identical logic; the existence oracle is a parameter so tests drive the
    /// real production path rather than a test-only copy of its rules.
    pub(crate) fn update_share_frames_and_visibility_with(
        &self,
        fresh: &[(u32, crate::hover_tab::WindowFrame)],
        visible_window_ids: &[u32],
        existence: &dyn WindowExistence,
    ) {
        let shared_sources = {
            let guard = self.inner.lock_unpoisoned();
            guard
                .shares
                .iter()
                .map(|(window_id, share)| (*window_id, share.source_kind))
                .collect::<Vec<_>>()
        };
        let closed_statuses = visibility_decisions(&shared_sources, visible_window_ids, existence);
        let guard = self.inner.lock_unpoisoned();
        for (window_id, visible, known_closed) in closed_statuses {
            if let Some(share) = guard.shares.get(&window_id) {
                share.visible_on_screen.store(visible, Ordering::Relaxed);
                share.known_closed.store(known_closed, Ordering::Relaxed);
            }
        }
        for (window_id, frame) in fresh {
            if let Some(share) = guard.shares.get(window_id) {
                let mut stored = share.frame.lock_unpoisoned();
                if *stored != *frame {
                    *stored = *frame;
                    crate::remote_control::update_control_frame(*window_id, *frame);
                }
            }
        }
    }

    pub(crate) fn active_share_ids(&self) -> Vec<u32> {
        let guard = self.inner.lock_unpoisoned();
        guard.shares.keys().copied().collect()
    }

    pub(crate) fn active_share_sources(
        &self,
    ) -> Vec<(u32, crate::transport::publisher::SharedSourceKind)> {
        let guard = self.inner.lock_unpoisoned();
        guard
            .shares
            .iter()
            .map(|(source_id, share)| (*source_id, share.source_kind))
            .collect()
    }

    pub(crate) fn active_share_restart_plan(
        &self,
    ) -> Vec<(
        u32,
        crate::hover_tab::WindowFrame,
        u64,
        CaptureResolution,
        crate::transport::publisher::SharedSourceKind,
        String,
    )> {
        let guard = self.inner.lock_unpoisoned();
        let mut shares: Vec<_> = guard
            .shares
            .iter()
            .map(|(id, share)| {
                (
                    *id,
                    *share.frame.lock_unpoisoned(),
                    share.started_seq,
                    share.resolution,
                    share.source_kind,
                    share.border_color.clone(),
                )
            })
            .collect();
        shares.sort_by_key(|(_, _, started_seq, _, _, _)| *started_seq);
        shares
    }

    pub(crate) fn is_share_active(&self, window_id: u32) -> bool {
        let guard = self.inner.lock_unpoisoned();
        guard.shares.contains_key(&window_id)
    }

    /// Whether this share currently permits remote control.
    ///
    /// Fails CLOSED for an unknown window id: no live share means nothing to
    /// authorize. That is the opposite of the metadata decoder's fail-open
    /// default, and deliberately so -- this one IS the security decision.
    pub(crate) fn share_allows_remote_control(&self, window_id: u32) -> bool {
        let guard = self.inner.lock_unpoisoned();
        guard
            .shares
            .get(&window_id)
            .is_some_and(|share| share.allow_remote_control.load(Ordering::Relaxed))
    }

    /// Set the per-share lock. Returns `Some(previous)` when the share exists.
    pub(crate) fn set_share_allows_remote_control(
        &self,
        window_id: u32,
        allowed: bool,
    ) -> Option<bool> {
        let guard = self.inner.lock_unpoisoned();
        let share = guard.shares.get(&window_id)?;
        Some(share.allow_remote_control.swap(allowed, Ordering::Relaxed))
    }

    /// The live room connection, for publishing per-share metadata.
    pub(crate) fn room_connection(&self) -> Option<Arc<RoomConnection>> {
        let guard = self.inner.lock_unpoisoned();
        Some(guard.joined.as_ref()?.room_connection.clone())
    }

    pub(crate) fn is_display_share(&self, window_id: u32) -> bool {
        let guard = self.inner.lock_unpoisoned();
        guard.shares.get(&window_id).is_some_and(|share| {
            matches!(
                share.source_kind,
                SharedSourceKind::Display | SharedSourceKind::DisplayRegion
            )
        })
    }

    fn active_share_restart_snapshot(
        &self,
        window_id: u32,
        started_seq: u64,
        restart_generation: u64,
    ) -> Option<ShareRestartSnapshot> {
        let guard = self.inner.lock_unpoisoned();
        let share = guard.shares.get(&window_id)?;
        if share.started_seq != started_seq || share.restart_generation != restart_generation {
            return None;
        }
        let room_connection = guard.joined.as_ref()?.room_connection.clone();
        Some(ShareRestartSnapshot {
            published: share.published.clone(),
            room_connection,
            restart_generation: share.restart_generation,
            priority: share.priority.clone(),
            interaction_signal: share.interaction_signal.clone(),
            resolution: share.resolution,
            demand_long_edge: share.capture.configuration_handle().demand_long_edge(),
            republish_intent: share.republish_intent.clone(),
            diagnostic_source: diagnostic_source_for_kind(share.source_kind),
            source_kind: share.source_kind,
        })
    }

    fn is_share_restart_generation_active(
        &self,
        window_id: u32,
        started_seq: u64,
        restart_generation: u64,
    ) -> bool {
        let guard = self.inner.lock_unpoisoned();
        guard.shares.get(&window_id).is_some_and(|share| {
            share.started_seq == started_seq && share.restart_generation == restart_generation
        })
    }

    /// Apply the user-visible terminal state only while no newer generation
    /// owns this window id. The session lock remains held through the
    /// synchronous UI/control effects, so a concurrent re-share cannot slip
    /// between the final generation check and those effects (#298).
    fn apply_terminal_reconnect_failure_if_current(
        &self,
        reconnect_guard: &ReconnectRepairGuard,
        app: &tauri::AppHandle,
        window_id: u32,
        started_seq: u64,
        error: ShareSessionError,
    ) -> bool {
        let guard = self.inner.lock_unpoisoned();
        if !reconnect_guard.is_current_with_inner(&guard)
            || !terminal_reconnect_effects_are_current(
                guard.shares.contains_key(&window_id),
                guard.last_stopped_share_seq.get(&window_id).copied(),
                started_seq,
            )
        {
            return false;
        }
        crate::hover_tab::clear_share_state_for_window(app, window_id);
        crate::remote_control::revoke_window(app, window_id, "reconnect publication repair failed");
        crate::hover_tab::emit_share_error(app, window_id, false, error);
        drop(guard);
        log::info!(
            "session: window {window_id} reconnect publication repair terminal effects applied for generation {started_seq}"
        );
        true
    }

    /// `pub(crate)`: #713's mic/camera reconnect repair (`session::mod` /
    /// `crate::camera_session`) reuses this exact currency check rather than
    /// a second copy that could drift from the window-share repair's.
    pub(crate) fn reconnect_repair_guard_is_current(
        &self,
        reconnect_guard: &ReconnectRepairGuard,
    ) -> bool {
        reconnect_guard.is_current_with_inner(&self.inner.lock_unpoisoned())
    }

    fn is_share_generation_active(&self, window_id: u32, started_seq: u64) -> bool {
        self.inner
            .lock_unpoisoned()
            .shares
            .get(&window_id)
            .is_some_and(|share| share.started_seq == started_seq)
    }

    fn active_share_publication_repair_plan(&self) -> Vec<(u32, u64)> {
        let guard = self.inner.lock_unpoisoned();
        let mut shares: Vec<_> = guard
            .shares
            .iter()
            .map(|(id, share)| (*id, share.started_seq))
            .collect();
        shares.sort_by_key(|(_, started_seq)| *started_seq);
        shares
    }

    fn active_share_publication_repair_snapshot(
        &self,
        window_id: u32,
        started_seq: u64,
        reconnect_guard: &ReconnectRepairGuard,
    ) -> Option<SharePublicationRepairSnapshot> {
        let guard = self.inner.lock_unpoisoned();
        if !reconnect_guard.is_current_with_inner(&guard) {
            return None;
        }
        let share = guard.shares.get(&window_id)?;
        if share.started_seq != started_seq {
            return None;
        }
        let room_connection = guard.joined.as_ref()?.room_connection.clone();
        let capture_config = share.capture.configuration_handle();
        capture_config.set_resolution_preference(share.resolution);
        Some(SharePublicationRepairSnapshot {
            published: share.published.clone(),
            room_connection,
            priority: share.priority.clone(),
            republish_intent: share.republish_intent.clone(),
            capture_config,
        })
    }

    /// Claim a repair intent only after the lightweight local-publication
    /// health check says it is missing. Claiming earlier would cancel an
    /// unrelated quality/resize republish even for a healthy fast resume.
    fn begin_reconnect_publication_repair_intent(
        &self,
        window_id: u32,
        started_seq: u64,
        reconnect_guard: &ReconnectRepairGuard,
    ) -> Option<u64> {
        let guard = self.inner.lock_unpoisoned();
        if !reconnect_guard.is_current_with_inner(&guard) {
            return None;
        }
        let share = guard.shares.get(&window_id)?;
        (share.started_seq == started_seq).then(|| begin_republish_intent(&share.republish_intent))
    }

    /// The most recently toggled-on-or-off shared window, if any window has
    /// ever been shared this session. Read by `shortcuts.rs`'s global
    /// shortcut handler (SPEC.md §4.2).
    pub(crate) fn last_toggled_window(&self) -> Option<u32> {
        *self.last_toggled_window.lock_unpoisoned()
    }

    /// Record `window_id` as the most recently toggled window. Called from
    /// both `start_share` and `stop_share` below.
    pub(super) fn set_last_toggled_window(&self, window_id: u32) {
        *self.last_toggled_window.lock_unpoisoned() = Some(window_id);
    }
}

/// Start sharing `window_id`: requires this process to already be joined to
/// a room (`join_room` -- see module doc comment; this NO LONGER connects a
/// room lazily on first share), starts a real `SCStream` capture via
/// `capture.rs`, publishes it as a new LiveKit video track, and pumps
/// captured frames into that track.
///
/// `frame` is the window's on-screen frame at share-start time (the same
/// `WindowFrame` the hover-tab hit-test already computed for its pill
/// positioning -- see `hover_tab::toggle_window_share`'s call site), stored
/// so `telepointer.rs` can hit-test the local cursor against it. See
/// `ActiveShare::frame`'s doc comment for the "not live-updated on move"
/// caveat.
pub async fn start_share(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    frame: crate::hover_tab::WindowFrame,
) -> Result<(), ShareSessionError> {
    let capture_source = if crate::region_window::resolve(window_id).is_some() {
        // Region tokens use the display adapter below. They must never fall
        // through to DirectWindowId, which would publish the hollow selector.
        ShareCaptureSource::DirectDisplayId
    } else {
        ShareCaptureSource::DirectWindowId
    };
    start_share_with_capture_source(
        app,
        state,
        window_id,
        frame,
        capture_source,
        CaptureResolution::default(),
        SharePublishOrigin::Ordinary,
    )
    .await
}

pub(crate) async fn start_share_with_system_picker_filter(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    frame: crate::hover_tab::WindowFrame,
    filter: SCContentFilter,
    logical_width: f64,
    logical_height: f64,
    point_pixel_scale: f64,
    source_kind: crate::transport::publisher::SharedSourceKind,
    source_title: Option<String>,
) -> Result<(), ShareSessionError> {
    start_share_with_capture_source(
        app,
        state,
        window_id,
        frame,
        ShareCaptureSource::SystemPicker {
            filter,
            logical_width,
            logical_height,
            point_pixel_scale,
            source_kind,
            source_title,
        },
        CaptureResolution::default(),
        SharePublishOrigin::Ordinary,
    )
    .await
}

fn capture_attempt_error_channel() -> (
    tokio::sync::mpsc::UnboundedSender<String>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

#[derive(Clone)]
struct CaptureAttemptGuard {
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl CaptureAttemptGuard {
    fn is_current(&self) -> bool {
        self.generation.load(Ordering::SeqCst) == self.expected
    }
}

fn first_frame_timeout_error(layout_gate: &crate::capture::LayoutIntegrityGate) -> String {
    if layout_gate.pending_reconfiguration().is_some() {
        layout_gate.fail();
        crate::capture::CAPTURE_LAYOUT_INVALID.to_string()
    } else {
        "timed out waiting for first captured frame".to_string()
    }
}

fn pending_layout_ack_expired(
    pending: Option<(u32, u32)>,
    expected: (u32, u32),
    deadline_reached: bool,
) -> bool {
    pending == Some(expected) && deadline_reached
}

/// One drained batch of capture-error events. A reconfigure storm coalesces
/// to its NEWEST ROI target (each supersedes the previous -- 2026-07-30
/// defect A); a non-reconfigure error ends the batch and wins, because it
/// makes any queued target moot for the current capture instance.
#[derive(Debug, PartialEq, Eq)]
enum CoalescedCaptureEvent {
    Reconfigure { width: u32, height: u32 },
    Failure(String),
}

fn coalesce_capture_error_events(
    first: String,
    mut try_next: impl FnMut() -> Option<String>,
) -> CoalescedCaptureEvent {
    let Some((mut width, mut height)) = crate::capture::parse_layout_reconfigure(&first) else {
        return CoalescedCaptureEvent::Failure(first);
    };
    while let Some(next) = try_next() {
        match crate::capture::parse_layout_reconfigure(&next) {
            Some((next_width, next_height)) => {
                width = next_width;
                height = next_height;
            }
            None => return CoalescedCaptureEvent::Failure(next),
        }
    }
    CoalescedCaptureEvent::Reconfigure { width, height }
}

/// 2026-07-30 defect A: `capture-layout-invalid` on a LIVE share is never
/// terminal. The capture restarts in place (`spawn_pump_failure_recovery`)
/// and only genuine window disappearance (`WindowNotFound` during the
/// restart) or the recovery circuit opening stops the share.
///
/// #734: a capture stall while the display/system is asleep (or the exact
/// ScreenCaptureKit "no capture source" error that lid-close emits) is also
/// never terminal — restart in place after wake, do not permanently tear
/// the share down. Genuine source loss still ends at `StopShare` when the
/// in-place restart itself fails with `WindowNotFound`/`DisplayNotFound`.
#[derive(Debug, PartialEq, Eq)]
enum CaptureFailureAction {
    RestartInPlace { message: String },
    StopShare { message: String },
}

/// ScreenCaptureKit's lid-close / display-sleep signature (see #734
/// petal.log): stream still "alive" but the capture source is gone for the
/// duration of sleep. Classifying this as restartable is safe even without
/// a sleep flag — genuine permanent loss still fails closed on the
/// subsequent `WindowNotFound`/`DisplayNotFound` from the restart attempt
/// (#637/#712).
fn is_sleep_style_sck_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("no capture source provided")
        || lower.contains("failed to find any displays or windows to capture")
}

fn capture_failure_action(error: String, sleep_correlated: bool) -> CaptureFailureAction {
    if error == crate::capture::CAPTURE_LAYOUT_INVALID
        || sleep_correlated
        || is_sleep_style_sck_error(&error)
    {
        let message = if error == crate::capture::CAPTURE_LAYOUT_INVALID {
            error
        } else if sleep_correlated || is_sleep_style_sck_error(&error) {
            format!("capture interrupted by display/system sleep: {error}")
        } else {
            error
        };
        CaptureFailureAction::RestartInPlace { message }
    } else {
        CaptureFailureAction::StopShare {
            message: format!("capture stalled -- ScreenCaptureKit stopped the stream: {error}"),
        }
    }
}

fn sleep_correlated_for_capture_failure() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::resilience::is_sleep_correlated_capture_window()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn capture_restart_should_wait_for_wake() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::resilience::capture_restart_should_wait_for_wake()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn final_first_frame_failure_message(
    started_without_frame: bool,
    permission_ok: bool,
    last_timeout: Option<&ShareSessionError>,
) -> String {
    if matches!(
        last_timeout,
        Some(ShareSessionError::Capture(message))
            if message == crate::capture::CAPTURE_LAYOUT_INVALID
    ) {
        return crate::capture::CAPTURE_LAYOUT_INVALID.to_string();
    }
    if started_without_frame && permission_ok {
        "macOS screen-capture stalled (window path unresponsive) - try again, or restart if it persists"
            .to_string()
    } else {
        last_timeout
            .map(ToString::to_string)
            .unwrap_or_else(|| "timed out waiting for first captured frame".to_string())
    }
}

fn diagnostic_source_for_kind(source_kind: SharedSourceKind) -> SourceSelectionClass {
    match source_kind {
        SharedSourceKind::Window => SourceSelectionClass::Window,
        SharedSourceKind::Display | SharedSourceKind::DisplayRegion => {
            SourceSelectionClass::Display
        }
    }
}

fn capture_layout_diagnostic(
    source: SourceSelectionClass,
    stage: CaptureLayoutStage,
) -> SentryDiagnosticEvent {
    SentryDiagnosticEvent::CaptureLayoutInvalid(CaptureLayoutDiagnostic {
        role: DiagnosticRole::Sharer,
        source,
        capture_geometry: GeometryBucket::Unknown,
        configured_geometry: GeometryBucket::Unknown,
        // Unknown, not Bgra: this constructor has no frame in hand, and macOS
        // capture is pinned to 420v NV12 -- so "bgra" was a value that is
        // never true, reported on every capture-layout-invalid event and in
        // its Sentry title. Missing data is hidden, never guessed.
        pixel_format: PixelFormatClass::Unknown,
        scale: ScaleBucket::Unknown,
        encoder: EncoderImplementationClass::NotApplicable,
        stage,
    })
}

fn emit_capture_layout_invalid(source: SourceSelectionClass, stage: CaptureLayoutStage) -> bool {
    crate::logging::capture_sentry_diagnostic(capture_layout_diagnostic(source, stage))
}

async fn clear_failed_start_metadata<F, Fut>(
    metadata_apply_lock: &tokio::sync::Mutex<()>,
    clear: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    // Serialize behind the possibly-detached metadata publish task. The
    // generation-aware clear then removes only this failed start, never a
    // newer re-share of the same window.
    let _metadata_apply_guard = metadata_apply_lock.lock().await;
    clear().await
}

/// Startup diagnostic on the first few captured frames: sample the NV12 Y AND
/// chroma planes. A window whose Y plane carries content but whose U/V planes
/// are uniformly zero produces a SOLID-GREEN remote image (YCbCr Y>0, U=0,
/// V=0 renders green, not black) -- observed on macOS 26 x86_64 VMs where the
/// virtual GPU's captured backing store never populates chroma. The
/// per-second freeze diagnostics hash the Y plane only by design (#548), so
/// dead chroma is invisible everywhere else; this samples both on the first
/// few frames so a green share is attributable from the sharer's log alone.
fn log_first_frame_content_diagnostic(window_id: u32, frame: &crate::capture::CapturedFrame) {
    let crate::capture::CapturedFramePayload::Native { pixel_buffer } = &frame.payload else {
        return;
    };
    let Ok(payload) = pixel_buffer.copy_nv12_payload() else {
        return;
    };
    let crate::capture::CapturedFramePayload::Nv12 {
        y,
        y_stride,
        uv,
        uv_stride,
        ..
    } = payload
    else {
        return;
    };
    let y_stride = y_stride as usize;
    let uv_stride = uv_stride as usize;
    let height = frame.height as usize;
    let rows = [0usize, height / 2, height.saturating_sub(1)];

    // Sample the luma plane: uniform + very dark means an empty backing store.
    let mut y_uniform = true;
    let mut y_first = None;
    for row in rows {
        let offset = row.saturating_mul(y_stride);
        let Some(slice) = y.get(offset..(offset.saturating_add(32)).min(y.len())) else {
            continue;
        };
        for &byte in slice {
            match y_first {
                None => y_first = Some(byte),
                Some(seen) if seen != byte => y_uniform = false,
                Some(_) => {}
            }
        }
    }
    let Some(y_value) = y_first else {
        return;
    };

    // Sample the interleaved chroma plane (U at even offsets, V at odd):
    // uniform U=0/V=0 with real luma is the solid-green signature.
    let chroma_height = height.div_ceil(2);
    let chroma_rows = [0usize, chroma_height / 2, chroma_height.saturating_sub(1)];
    let mut u_uniform = true;
    let mut v_uniform = true;
    let mut u_first = None;
    let mut v_first = None;
    for row in chroma_rows {
        let offset = row.saturating_mul(uv_stride);
        let Some(slice) = uv.get(offset..(offset.saturating_add(32)).min(uv.len())) else {
            continue;
        };
        for (index, &byte) in slice.iter().enumerate() {
            if index % 2 == 0 {
                match u_first {
                    None => u_first = Some(byte),
                    Some(seen) if seen != byte => u_uniform = false,
                    Some(_) => {}
                }
            } else {
                match v_first {
                    None => v_first = Some(byte),
                    Some(seen) if seen != byte => v_uniform = false,
                    Some(_) => {}
                }
            }
        }
    }
    let (Some(u_value), Some(v_value)) = (u_first, v_first) else {
        return;
    };

    if y_uniform && y_value <= 32 && u_uniform && v_uniform && u_value == 0 && v_value == 0 {
        log::warn!(
            "capture-diag: window {window_id} first frames are all-zero (Y={y_value} U={u_value} V={v_value}) -- this renders GREEN on receivers, not black; capture backend or VM GPU is not rendering window backing content"
        );
    } else if u_uniform && v_uniform && u_value == 0 && v_value == 0 {
        log::warn!(
            "capture-diag: window {window_id} captured luma carries content but chroma planes are uniformly ZERO (U={u_value} V={v_value}) -- the remote image will be solid green; VM/capture chroma is dead"
        );
    } else if y_uniform && y_value <= 32 {
        log::warn!(
            "capture-diag: window {window_id} first frame content is uniform black (Y={y_value} U={u_value} V={v_value}); dimensions correct but the window backing store is empty -- capture backend or VM GPU is not rendering window content"
        );
    } else if y_uniform {
        log::info!(
            "capture-diag: window {window_id} first frame content is uniform (Y={y_value}); window may be genuinely blank"
        );
    } else {
        log::info!(
            "capture-diag: window {window_id} first frame content present (Y varies, sampled U={u_value} V={v_value})"
        );
    }
}

async fn start_capture_for_share(
    window_id: u32,
    capture_source: ShareCaptureSource,
    fps: u32,
    resolution: CaptureResolution,
    reason: &str,
    diagnostics: Option<crate::diagnostics::DiagnosticsState>,
) -> Result<StartedShareCapture, ShareSessionError> {
    // First captured frame tells us the real (window-backing-store) size --
    // same "wait for frame #1 before publishing" approach `publish_probe.rs`
    // uses, since a LiveKit video source needs a fixed resolution up front.
    let first_frame_sent = Arc::new(AtomicBool::new(false));
    let latest_frame: LatestCapturedFrame = Arc::new(Mutex::new(None));
    let latest_frame_notify = Arc::new(tokio::sync::Notify::new());
    let latest_frame_overwrites = Arc::new(AtomicU32::new(0));
    let last_capture_wall_time_us = Arc::new(AtomicU64::new(now_us()));
    let capture_freeze_diag = Arc::new(Mutex::new(CaptureFreezeDiag::default()));
    let capture_heartbeat_last_us = Arc::new(AtomicU64::new(now_us()));
    let capture_heartbeat_frames = Arc::new(AtomicU64::new(0));
    let capture_heartbeat_last_frame_count = Arc::new(AtomicU64::new(0));
    let capture_attempt_generation = Arc::new(AtomicU64::new(0));
    let capture_source_name = match &capture_source {
        ShareCaptureSource::DirectWindowId => "direct-window-id",
        ShareCaptureSource::DirectDisplayId => "direct-display-id",
        ShareCaptureSource::SystemPicker { .. } => "system-picker",
    };
    let diagnostic_source = match &capture_source {
        ShareCaptureSource::DirectWindowId => SourceSelectionClass::Window,
        ShareCaptureSource::DirectDisplayId => SourceSelectionClass::Display,
        ShareCaptureSource::SystemPicker { .. } => SourceSelectionClass::SystemPicker,
    };
    // #712: a `SystemPicker` share can be either kind -- route the picker's
    // OWN color-profile detection the same source_kind-aware way the restart
    // path now picks its capture source, instead of always assuming a
    // window. Detecting a display share's color profile through the
    // window-only `content.windows()` lookup always misses (a display is
    // never a member of that list) and silently falls back to the legacy
    // default, which is the paper-cut #712 point 3 calls out.
    let picker_color_profile = if matches!(&capture_source, ShareCaptureSource::SystemPicker { .. })
    {
        let detected = match capture_source.source_kind() {
            crate::transport::publisher::SharedSourceKind::Window => {
                crate::capture::detected_color_profile_for_window(window_id)
            }
            crate::transport::publisher::SharedSourceKind::Display
            | crate::transport::publisher::SharedSourceKind::DisplayRegion => {
                // Fable review of the original #712 fix caught this: `window_id`
                // is a TAGGED display source id here (`DISPLAY_SOURCE_MARKER |
                // CGDirectDisplayID`, see `window_source::display_source_id`) --
                // the same value `hover_tab::toggle_display_share_from_picker`
                // stores in `ActiveShare::window_id` for a picker-selected
                // display share. `content.displays()` compares raw
                // `CGDirectDisplayID`s and can never match the tagged form, so
                // this must decode back to the raw id before crossing into
                // ScreenCaptureKit, exactly like `prepare_direct_display_source`
                // below.
                let raw_display_id = crate::window_source::display_id_from_source_id(window_id);
                crate::capture::detected_color_profile_for_display(raw_display_id)
            }
        };
        match detected {
            Ok(profile) => profile,
            Err(e) => {
                log::warn!(
                    "session: window {window_id} could not detect picker source color profile ({e}); falling back to legacy publish default"
                );
                crate::video_color::VideoColorProfile::legacy_publish_default()
            }
        }
    } else {
        crate::video_color::VideoColorProfile::legacy_publish_default()
    };

    // Retry-with-rebuild on a first-frame timeout: the macOS per-window
    // capture path can accept `start_capture()` yet never deliver a frame
    // (SCK/WindowServer wedge). Tearing the stream down and rebuilding it
    // often clears a transient wedge. `DirectWindowId`/`DirectDisplayId` are
    // cheap to rebuild (just an id), so they get the full retry budget.
    // `SystemPicker`'s `SCContentFilter` is a one-shot handoff from the OS
    // picker UI (not `Clone`), so it gets exactly one attempt -- same
    // diagnostics either way.
    let max_attempts = match &capture_source {
        ShareCaptureSource::DirectWindowId | ShareCaptureSource::DirectDisplayId => {
            FIRST_FRAME_ATTEMPTS
        }
        ShareCaptureSource::SystemPicker { .. } => 1,
    };
    let mut capture_source_slot = Some(capture_source);
    let permission_state = if crate::window_source::has_screen_recording_access() {
        "GRANTED"
    } else {
        "DENIED"
    };
    log::info!(
        "session: {reason}(window {window_id}) permission check -- Screen Recording {permission_state}"
    );

    let (capture, (width, height, source_scale, color_profile), capture_error_rx) = {
        let mut last_timeout = None;
        let mut started_without_frame = false;
        let mut started_capture = None;
        for attempt in 1..=max_attempts {
            capture_attempt_generation.store(attempt as u64, Ordering::SeqCst);
            let attempt_guard = CaptureAttemptGuard {
                generation: capture_attempt_generation.clone(),
                expected: attempt as u64,
            };
            // Error ownership is attempt-local. Only the receiver belonging to
            // the winning capture can become the active monitor's receiver.
            let (capture_error_tx, mut capture_error_rx) = capture_attempt_error_channel();
            let source_for_attempt = capture_source_slot
                .take()
                .expect("capture source available for this attempt");
            let (size_tx, mut size_rx) =
                tokio::sync::oneshot::channel::<(u32, u32, f64, VideoColorProfile)>();
            let size_tx = Arc::new(Mutex::new(Some(size_tx)));
            first_frame_sent.store(false, Ordering::SeqCst);
            log::info!(
                "session: {reason}(window {window_id}) starting SCStream capture via {capture_source_name} (attempt {attempt}/{max_attempts})"
            );
            if let Some(diagnostics) = diagnostics.as_ref() {
                diagnostics.record_native_startup_stage(
                    window_id,
                    NativeStartupStageKind::CaptureAttemptStarted,
                    None,
                    None,
                    Some(format!(
                        "attempt {attempt}/{max_attempts} via {capture_source_name}"
                    )),
                );
            }
            let first_frame_sent_cb = first_frame_sent.clone();
            let latest_frame_cb = latest_frame.clone();
            let latest_frame_notify_cb = latest_frame_notify.clone();
            let latest_frame_overwrites_cb = latest_frame_overwrites.clone();
            let last_capture_wall_time_us_cb = last_capture_wall_time_us.clone();
            let capture_freeze_diag_cb = capture_freeze_diag.clone();
            let capture_heartbeat_last_us_cb = capture_heartbeat_last_us.clone();
            let capture_heartbeat_frames_cb = capture_heartbeat_frames.clone();
            let capture_heartbeat_last_frame_count_cb = capture_heartbeat_last_frame_count.clone();
            let size_tx_cb = size_tx.clone();
            let capture_error_tx_cb = capture_error_tx.clone();
            let diagnostics_cb = diagnostics.clone();
            let on_frame = move |frame: crate::capture::CapturedFrame| {
                if !attempt_guard.is_current() {
                    return;
                }
                let capture_wall_time_us = now_us();
                last_capture_wall_time_us_cb.store(capture_wall_time_us, Ordering::Relaxed);
                let captured = capture_heartbeat_frames_cb.fetch_add(1, Ordering::Relaxed) + 1;
                let last_heartbeat = capture_heartbeat_last_us_cb.load(Ordering::Relaxed);
                let heartbeat_elapsed_us = capture_wall_time_us.saturating_sub(last_heartbeat);
                if captured == 1 || heartbeat_elapsed_us >= CAPTURE_HEARTBEAT_INTERVAL_US {
                    capture_heartbeat_last_us_cb.store(capture_wall_time_us, Ordering::Relaxed);
                    let previous_count =
                        capture_heartbeat_last_frame_count_cb.swap(captured, Ordering::Relaxed);
                    let frames_since = captured.saturating_sub(previous_count);
                    let rate = if captured > 1 && heartbeat_elapsed_us > 0 {
                        frames_since as f64 / (heartbeat_elapsed_us as f64 / 1_000_000.0)
                    } else {
                        0.0
                    };
                    log::info!(
                        "session: window {window_id} capture heartbeat -- captured {captured} frame(s), last seq {}, approx {:.1}fps",
                        frame.sequence,
                        rate
                    );
                }
                // Capture-freeze diagnostics (#capture-freeze): explain a frozen
                // viewer image using SCK's own per-frame signals + a strided
                // content hash + live occlusion coverage. Emitted ~1x/sec, and
                // immediately on any frozen<->live transition. This is what
                // distinguishes "source app stopped drawing while covered"
                // (frames still arrive, dirty=0, hash unchanged, occlusion high)
                // from a wedged stream (no frames at all -> raw-capture watchdog).
                let capture_state = {
                    let freeze_sample = capture_freeze_sample(&frame);
                    let hash = freeze_sample.hash;
                    let (frozen_now, should_sample_occlusion, cached_occlusion) = {
                        let mut diag = capture_freeze_diag_cb.lock_unpoisoned();
                        let frozen_now = diag.observe_sample(freeze_sample);
                        let source_idle = capture_source_appears_idle(
                            &frame,
                            frozen_now,
                            freeze_sample.pixels_sampled,
                        );
                        set_source_appears_idle(window_id, source_idle);
                        let transition = frozen_now != diag.reported_frozen;
                        let elapsed = capture_wall_time_us.saturating_sub(diag.last_log_us);
                        (
                            frozen_now,
                            transition
                                || elapsed >= 1_000_000
                                || diag.last_occlusion_fraction.is_none(),
                            diag.last_occlusion_fraction,
                        )
                    };
                    let occ = if should_sample_occlusion {
                        // #744: read occlusion from the shared registry snapshot
                        // (raw f64 -> numerically identical to the old direct
                        // enumeration). Fall back to the direct CG path only
                        // before the registry global is set (early boot).
                        match crate::window_registry::global() {
                            Some(reg) => reg.occlusion(window_id, std::process::id() as i32),
                            None => crate::platform::cg::occlusion_fraction(window_id),
                        }
                    } else {
                        cached_occlusion
                    };
                    let capture_state = capture_state_report(&frame, frozen_now, occ);
                    let log_sample = {
                        let mut diag = capture_freeze_diag_cb.lock_unpoisoned();
                        if should_sample_occlusion {
                            diag.last_occlusion_fraction = occ;
                        }
                        let transition = frozen_now != diag.reported_frozen;
                        let elapsed = capture_wall_time_us.saturating_sub(diag.last_log_us);
                        let should_log = transition || elapsed >= 1_000_000;
                        if should_log {
                            let frames = captured.saturating_sub(diag.last_log_captured);
                            let fps = if elapsed > 0 {
                                frames as f64 / (elapsed as f64 / 1_000_000.0)
                            } else {
                                0.0
                            };
                            diag.last_log_us = capture_wall_time_us;
                            diag.last_log_captured = captured;
                            diag.reported_frozen = frozen_now;
                            Some((fps, diag.unchanged_run))
                        } else {
                            None
                        }
                    };
                    if let Some((fps, unchanged_run)) = log_sample {
                        let occ_str = match occ {
                            Some(f) => format!("{:.0}%", f * 100.0),
                            None => "n/a(offscreen/minimized/closed)".to_string(),
                        };
                        let verdict = if frozen_now {
                            match occ {
                                Some(f) if f >= 0.95 => {
                                    "FROZEN: source not drawing while fully covered (occlusion)"
                                }
                                Some(f) if f > 0.01 => "FROZEN: static content, partially covered",
                                _ => "FROZEN: static content, source idle (not covered)",
                            }
                        } else {
                            "LIVE: content updating"
                        };
                        // #683: process-wide (not per-window), so this reads
                        // the same value regardless of which window's
                        // capture callback happens to fire the log line.
                        // Throttled to a real syscall at most once/5s inside
                        // `process_footprint_bytes_throttled` -- this line
                        // itself fires roughly once/sec.
                        let mem_str =
                            match crate::platform::mem::process_footprint_bytes_throttled() {
                                Some(bytes) => format!("{}MB", bytes / 1_000_000),
                                None => "n/a".to_string(),
                            };
                        log::info!(
                            "capture-diag: window {window_id} seq={} ~{fps:.1}fps status={:?} dirty={}rects/{}px hash={hash:016x} unchanged_run={unchanged_run} occlusion={occ_str} mem={mem_str} -> {verdict}",
                            frame.sequence,
                            frame.frame_status,
                            frame.dirty_rect_count,
                            frame.dirty_area_px,
                        );
                    }
                    capture_state
                };
                if let Some(diagnostics) = diagnostics_cb.as_ref() {
                    diagnostics.record_capture_frame(
                        window_id,
                        frame.sequence,
                        frame.width,
                        frame.height,
                        capture_state,
                    );
                }
                if let Some((input_summary, injected_at_ms)) =
                    crate::remote_control::take_input_latency_marker(window_id)
                {
                    let frame_shown_at_ms = crate::time_util::now_ms();
                    log::info!(
                        "remote-control-latency: host frame_shown_ts_ms={} {} inject_to_frame_ms={} capture_seq={}",
                        frame_shown_at_ms,
                        input_summary,
                        frame_shown_at_ms.saturating_sub(injected_at_ms),
                        frame.sequence
                    );
                }
                if !first_frame_sent_cb.swap(true, Ordering::SeqCst) {
                    if let Some(size_tx) = size_tx_cb.lock_unpoisoned().take() {
                        let _ = size_tx.send((
                            frame.width,
                            frame.height,
                            frame.source_scale,
                            frame.color_profile,
                        ));
                    }
                }
                // The startup content diagnostic samples Y + chroma on the
                // first few frames: the first frame can be a transient black
                // flash (VM backing store not ready), so one sample is not
                // enough to judge dead chroma.
                if captured <= 3 {
                    log_first_frame_content_diagnostic(window_id, &frame);
                }
                {
                    let mut latest = latest_frame_cb.lock_unpoisoned();
                    if latest.is_some() {
                        let overwritten =
                            latest_frame_overwrites_cb.fetch_add(1, Ordering::Relaxed) + 1;
                        if overwritten == 1 || overwritten % 300 == 0 {
                            log::warn!(
                                "session: window {window_id} capture pump lag has overwritten {overwritten} queued frame(s)"
                            );
                        }
                    }
                    *latest = Some((frame, capture_wall_time_us));
                }
                latest_frame_notify_cb.notify_one();
            };
            let on_error = move |error| {
                let _ = capture_error_tx_cb.send(error);
            };
            let capture = match source_for_attempt {
                ShareCaptureSource::DirectWindowId => {
                    // Cheap unit variant -- restore it so a later attempt (if
                    // this one times out) can rebuild from `window_id` again.
                    capture_source_slot = Some(ShareCaptureSource::DirectWindowId);
                    WindowCapture::start_with_error_handler_at_resolution(
                        window_id, fps, resolution, on_frame, on_error,
                    )
                }
                ShareCaptureSource::DirectDisplayId => {
                    if let Some(region) = crate::region_window::resolve(window_id) {
                        capture_source_slot = Some(ShareCaptureSource::DirectDisplayId);
                        WindowCapture::start_display_region_with_error_handler_at_resolution(
                            window_id,
                            region.frame,
                            fps,
                            resolution,
                            on_frame,
                            on_error,
                        )
                    } else {
                    // #712: same cheap-rebuild shape as `DirectWindowId`, but
                    // `window_id` here is actually a TAGGED display source id
                    // (`DISPLAY_SOURCE_MARKER | CGDirectDisplayID` -- see
                    // `window_source::display_source_id`), the same value
                    // `hover_tab::toggle_display_share_from_picker` stores in
                    // `ActiveShare::window_id` for a display share and both
                    // restart call sites (`spawn_pump_failure_recovery`,
                    // `restart_active_shares_after_wake`) thread through
                    // unchanged. Fable review of the original #712 fix caught
                    // this: without decoding here, `content.displays().find(|d|
                    // d.display_id() == window_id)` inside
                    // `prepare_direct_display_source` can never match a real
                    // `CGDirectDisplayID` -- the restart kept failing exactly
                    // as before, just renamed from `WindowNotFound` to
                    // `DisplayNotFound`. Decode at this one boundary, where the
                    // id crosses from "value stored in ActiveShare" into "value
                    // handed to a raw ScreenCaptureKit API" (matches the
                    // existing convention at `hover_tab.rs`'s
                    // `toggle_display_share_from_picker`, which decodes before
                    // touching SCK the same way).
                    capture_source_slot = Some(ShareCaptureSource::DirectDisplayId);
                    let raw_display_id = crate::window_source::display_id_from_source_id(window_id);
                    WindowCapture::start_display_with_error_handler_at_resolution(
                        raw_display_id,
                        fps,
                        resolution,
                        on_frame,
                        on_error,
                    )
                    }
                }
                ShareCaptureSource::SystemPicker {
                    filter,
                    logical_width,
                    logical_height,
                    point_pixel_scale,
                    source_kind: _,
                    source_title: _,
                } => WindowCapture::start_with_picker_filter_at_resolution(
                    window_id,
                    filter,
                    logical_width,
                    logical_height,
                    point_pixel_scale,
                    picker_color_profile,
                    fps,
                    resolution,
                    on_frame,
                    on_error,
                ),
            }
            .map_err(|e| {
                log::error!(
                    "session: {reason}(window {window_id}) failed -- WindowCapture::start error: {e}"
                );
                e
            })?;
            started_without_frame = true;

            let first_frame_deadline = tokio::time::Instant::now() + FIRST_FRAME_TIMEOUT;
            // #183-family fallback for the FIRST frame: a wedged SCStream can
            // accept `start_capture()` and then deliver zero callbacks even
            // while the screenshot backend still works -- the exact condition
            // the post-pump snapshot-pull path rescues once the share is up,
            // but that path only starts after a first frame exists, so a
            // wedged stream used to fail the whole share at this wait ("the
            // receiver gets nothing"). If the stream has been silent for
            // `SNAPSHOT_PULL_AFTER_SILENCE_US`, race one-shot screenshot pulls
            // against the first-frame deadline so a working screenshot backend
            // can still establish the first frame. Healthy streams deliver
            // the first frame long before the silence threshold, so this arm
            // never engages for them.
            let mut snapshot_pull_interval = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_micros(SNAPSHOT_PULL_AFTER_SILENCE_US),
                Duration::from_micros(SNAPSHOT_PULL_MIN_INTERVAL_US),
            );
            snapshot_pull_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut snapshot_pull_errors = 0u32;
            let size_result = loop {
                let next = tokio::select! {
                    result = &mut size_rx => break result.map_err(|_| ShareSessionError::Capture(
                        "timed out waiting for first captured frame".to_string(),
                    )),
                    error = capture_error_rx.recv() => error.ok_or_else(|| ShareSessionError::Capture(
                        "timed out waiting for first captured frame".to_string(),
                    )),
                    _ = tokio::time::sleep_until(first_frame_deadline) => {
                        break Err(ShareSessionError::Capture(first_frame_timeout_error(
                            &capture.layout_gate(),
                        )));
                    },
                    _ = snapshot_pull_interval.tick() => {
                        // Same offload pattern as the post-pump snapshot path:
                        // each pull costs a WindowServer composite + copy
                        // (~45ms measured), so never run it on an async worker.
                        let capture_for_pull = capture.configuration_handle();
                        match tokio::task::spawn_blocking(move || {
                            capture_for_pull.snapshot_frame()
                        })
                        .await
                        {
                            Ok(Ok(frame)) if frame.layout_validated => {
                                log::info!(
                                    "session: {reason}(window {window_id}) first frame established via snapshot pull after the SCStream stayed silent {:.1}s (attempt {attempt}/{max_attempts})",
                                    SNAPSHOT_PULL_AFTER_SILENCE_US as f64 / 1_000_000.0
                                );
                                first_frame_sent.store(true, Ordering::SeqCst);
                                let snapshot_size = (
                                    frame.width,
                                    frame.height,
                                    frame.source_scale,
                                    frame.color_profile,
                                );
                                {
                                    let mut latest = latest_frame.lock_unpoisoned();
                                    *latest = Some((frame, now_us()));
                                }
                                latest_frame_notify.notify_one();
                                break Ok(snapshot_size);
                            }
                            Ok(Ok(_)) => {
                                // Unvalidated by the layout gate; keep waiting
                                // for the stream or the next pull.
                                continue;
                            }
                            Ok(Err(e)) => {
                                snapshot_pull_errors += 1;
                                if snapshot_pull_errors == 1 || snapshot_pull_errors % 20 == 0 {
                                    log::warn!(
                                        "session: {reason}(window {window_id}) first-frame snapshot pull failed ({snapshot_pull_errors}x): {e}"
                                    );
                                }
                                continue;
                            }
                            Err(join_error) => {
                                snapshot_pull_errors += 1;
                                log::warn!(
                                    "session: {reason}(window {window_id}) first-frame snapshot pull task panicked: {join_error}"
                                );
                                continue;
                            }
                        }
                    }
                };
                let error = match next {
                    Ok(error) => error,
                    Err(error) => break Err(error),
                };
                let Some((target_width, target_height)) =
                    crate::capture::parse_layout_reconfigure(&error)
                else {
                    break Err(ShareSessionError::Capture(error));
                };
                let config = capture.configuration_handle();
                if let Err(error) =
                    config.update_stream_configuration(target_width, target_height, fps, resolution)
                {
                    log::error!(
                        "session: {reason}(window {window_id}) capture ROI reconfiguration to {target_width}x{target_height} failed: {error}"
                    );
                    config.layout_gate().fail();
                    break Err(ShareSessionError::Capture(
                        crate::capture::CAPTURE_LAYOUT_INVALID.to_string(),
                    ));
                }
                log::info!(
                    "session: {reason}(window {window_id}) requested padded capture ROI output {target_width}x{target_height} (attempt {attempt}/{max_attempts}); awaiting a matching frame"
                );
            };

            match size_result {
                Ok(size) => {
                    if let Some(diagnostics) = diagnostics.as_ref() {
                        diagnostics.record_native_startup_stage(
                            window_id,
                            NativeStartupStageKind::FirstFrame,
                            Some(size.0),
                            Some(size.1),
                            Some(format!("attempt {attempt}/{max_attempts}")),
                        );
                    }
                    if capture.layout_gate().is_failed() {
                        let capture = Arc::new(capture);
                        let _ = stop_capture_with_timeout(
                            capture,
                            format!("{reason}(window {window_id}) capture-layout-invalid"),
                        )
                        .await;
                        emit_capture_layout_invalid(
                            diagnostic_source,
                            CaptureLayoutStage::Validation,
                        );
                        return Err(ShareSessionError::Capture(
                            crate::capture::CAPTURE_LAYOUT_INVALID.to_string(),
                        ));
                    }
                    started_capture = Some((capture, size, capture_error_rx));
                    break;
                }
                Err(e) => {
                    // Invalidate this callback epoch before stopping the
                    // losing stream. A queued late sample can no longer
                    // mutate the next attempt's startup/latest-frame state.
                    capture_attempt_generation.store(attempt as u64 + 1, Ordering::SeqCst);
                    if let Some(diagnostics) = diagnostics.as_ref() {
                        diagnostics.record_native_startup_stage(
                            window_id,
                            NativeStartupStageKind::FirstFrameTimeout,
                            None,
                            None,
                            Some(format!("attempt {attempt}/{max_attempts}: {e}")),
                        );
                    }
                    log::warn!(
                        "session: {reason}(window {window_id}) attempt {attempt}/{max_attempts} timed out waiting for first captured frame: {e} -- stopping capture"
                    );
                    let capture = Arc::new(capture);
                    if let Err(stop_error) = stop_capture_with_timeout(
                        capture,
                        format!("{reason}(window {window_id}) timed-out capture attempt {attempt}"),
                    )
                    .await
                    {
                        log::warn!("session: failed to stop capture: {stop_error}");
                    }
                    last_timeout = Some(e);
                    if attempt < max_attempts {
                        tokio::time::sleep(FIRST_FRAME_RETRY_DELAY).await;
                    }
                }
            }
        }
        if let Some(capture) = started_capture {
            capture
        } else {
            let permission_ok = crate::window_source::has_screen_recording_access();
            let message = final_first_frame_failure_message(
                started_without_frame,
                permission_ok,
                last_timeout.as_ref(),
            );
            log::error!(
                "session: {reason}(window {window_id}) failed after {max_attempts} first-frame attempt(s) via {capture_source_name}: {message}"
            );
            if message == crate::capture::CAPTURE_LAYOUT_INVALID {
                emit_capture_layout_invalid(diagnostic_source, CaptureLayoutStage::FirstFrame);
            }
            return Err(ShareSessionError::Capture(message));
        }
    };
    log::info!(
        "session: {reason}(window {window_id}) first frame received ({width}x{height}, scale {source_scale:.2})"
    );

    let layout_gate = capture.layout_gate();
    Ok(StartedShareCapture {
        capture,
        width,
        height,
        source_scale,
        color_profile,
        latest_frame,
        latest_frame_notify,
        last_capture_wall_time_us,
        layout_gate,
        capture_error_rx,
    })
}

async fn start_share_with_capture_source(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    frame: crate::hover_tab::WindowFrame,
    capture_source: ShareCaptureSource,
    requested_resolution: CaptureResolution,
    publish_origin: SharePublishOrigin,
) -> Result<(), ShareSessionError> {
    use tauri::Manager;

    // Begin log fires FIRST, before any early return, so every invocation of
    // this function is visible in the log (issue #13: a crash trace showed a
    // `stop_share ... begin` with no matching start begin -- see the audit
    // note on `leave_room`; the begin log itself previously also sat after
    // the permission check, hiding permission-refused attempts).
    log::info!("session: start_share(window {window_id}) begin");
    let analytics_source = match &capture_source {
        ShareCaptureSource::DirectWindowId => crate::analytics::ShareStartedSource::Window,
        ShareCaptureSource::DirectDisplayId => crate::analytics::ShareStartedSource::Display,
        ShareCaptureSource::SystemPicker { .. } => crate::analytics::ShareStartedSource::Picker,
    };
    let emit_share_started = publish_origin == SharePublishOrigin::Ordinary;
    // #804/#807: a new share never inherits the previous one's recovery
    // budgets, even if that share ended without a clean stop.
    clear_layout_roi_ack_failures(window_id);
    clear_pump_recovery_failures(window_id);
    warn_if_denylisted_share_target(window_id);
    if !crate::window_source::has_screen_recording_access() {
        log::warn!(
            "session: start_share(window {window_id}) refused -- Screen Recording permission DENIED"
        );
        return Err(ShareSessionError::PermissionDenied);
    }

    // Already sharing this window (shouldn't normally happen -- hover_tab's
    // own HashSet toggle should keep this call one-shot per window -- but
    // guard against a double-start rather than silently leaking a second
    // capture+publish for the same window_id).
    let room_connection = {
        let guard = state.inner.lock_unpoisoned();
        if guard.shares.contains_key(&window_id) {
            log::info!(
                "session: start_share(window {window_id}) no-op -- already sharing this window"
            );
            return Ok(());
        }
        // SPEC.md §4.3: "Concurrent-share cap: 4 windows per user." Refuse
        // outright rather than silently dropping an existing share -- see
        // module doc comment.
        if guard.shares.len() >= MAX_CONCURRENT_SHARES {
            log::warn!(
                "session: start_share(window {window_id}) refused -- already at the {MAX_CONCURRENT_SHARES}-share cap"
            );
            return Err(ShareSessionError::TooManyShares(MAX_CONCURRENT_SHARES));
        }
        // Sharing now REQUIRES an existing room join -- no more lazy
        // dev-room connect. A real, surfaced error (not a silent auto-join)
        // if the user somehow tries to share before joining a room (the
        // frontend's real flow always joins first -- see
        // `/meeting/[room]`'s `join_room` call on mount -- so this should be
        // unreachable in practice, but it's a real guard, not an assumption).
        let Some(joined) = guard.joined.as_ref() else {
            log::warn!(
                "session: start_share(window {window_id}) refused -- not currently in a room"
            );
            return Err(ShareSessionError::NotInRoom);
        };
        joined.room_connection.clone()
    };

    let priority_value = crate::share_priority::current();
    let priority = Arc::new(Mutex::new(priority_value));
    let resolution = if requested_resolution == CaptureResolution::Auto {
        priority_value.capture_resolution()
    } else {
        requested_resolution
    };
    let source_kind = if crate::region_window::resolve(window_id).is_some() {
        SharedSourceKind::DisplayRegion
    } else {
        capture_source.source_kind()
    };
    let diagnostic_source = diagnostic_source_for_kind(source_kind);
    let source_title_override = capture_source.source_title_override();
    // #915: captured before `source_title_override` is consumed below, so
    // the url-refresh spawn gate downstream still knows whether this share
    // had an override even after that `Option<String>` is moved out.
    let has_source_title_override = source_title_override.is_some();
    let fps = startup_capture_fps(priority_value);
    log::info!(
        "session: start_share(window {window_id}) applying {priority_value:?} preference at {fps}fps and {resolution:?} resolution"
    );
    let diagnostics = app
        .try_state::<crate::diagnostics::DiagnosticsState>()
        .map(|state| state.inner().clone());
    if let Some(diagnostics) = diagnostics.as_ref() {
        diagnostics.reset_capture_pipeline(window_id);
        let capture_path = match source_kind {
            SharedSourceKind::Window => "window",
            SharedSourceKind::Display | SharedSourceKind::DisplayRegion => "display",
        };
        diagnostics.begin_native_startup(
            window_id,
            capture_path,
            Some(fps),
            Some(format!("{resolution:?}")),
        );
    }
    let StartedShareCapture {
        capture,
        width,
        height,
        source_scale,
        color_profile,
        latest_frame,
        latest_frame_notify,
        last_capture_wall_time_us,
        layout_gate,
        capture_error_rx,
    } = start_capture_for_share(
        window_id,
        capture_source,
        fps,
        resolution,
        "start_share",
        diagnostics.clone(),
    )
    .await?;
    if layout_gate.is_failed() {
        let _ = stop_capture_with_timeout(
            Arc::new(capture),
            format!(
                "session: start_share(window {window_id}) capture-layout-invalid before publish"
            ),
        )
        .await;
        emit_capture_layout_invalid(diagnostic_source, CaptureLayoutStage::Validation);
        return Err(ShareSessionError::Capture(
            crate::capture::CAPTURE_LAYOUT_INVALID.to_string(),
        ));
    }
    let owner_pid = crate::window_registry::global()
        .map(|r| r.owner_pid_fresh(window_id))
        .unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(window_id));
    if owner_pid.is_none() {
        log::warn!("session: start_share(window {window_id}) could not resolve owner pid");
    }

    let mut source_info = source_info_for_window(window_id);
    if let Some(title) = source_title_override {
        source_info.title = title;
        source_info.url = None;
    }
    let source_title = source_info.title.clone();
    let color_profile = published_metadata_color_profile(color_profile);
    // Allocate the local share generation before spawning metadata work. The
    // metadata owner and the eventual ActiveShare must use the same generation
    // so a delayed old stop cannot clear a newer re-share's title (#298).
    let started_seq = {
        let mut guard = state.inner.lock_unpoisoned();
        let seq = guard.next_share_seq;
        guard.next_share_seq += 1;
        seq
    };
    // issue #249/#299: publish source metadata (title/color/kind receivers key
    // off), but do not serialize media publication behind its signaling
    // round-trip. Previously this wait sat directly in front of
    // `publish_window_at`, so a slow/reconnecting channel held the video track
    // unpublished while capture ran and discarded frames. The metadata task
    // keeps running past the timeout (dropping a JoinHandle detaches, it does
    // not abort), so late metadata still lands.
    if let Some(diagnostics) = diagnostics.as_ref() {
        diagnostics.set_native_startup_correlation(window_id, started_seq, 0);
        diagnostics.record_native_startup_stage(
            window_id,
            NativeStartupStageKind::MetadataStarted,
            Some(width),
            Some(height),
            None,
        );
    }
    let metadata_task = {
        let room_connection = room_connection.clone();
        let metadata_apply_lock = state.share_metadata_apply_lock.clone();
        let title = source_info.title;
        let url = source_info.url;
        tokio::spawn(async move {
            let _metadata_apply_guard = metadata_apply_lock.lock().await;
            room_connection
                .set_shared_window_info_for_generation(
                    window_id,
                    started_seq,
                    title,
                    source_scale,
                    url,
                    color_profile,
                    source_kind,
                )
                .await;
        })
    };
    // Metadata is useful before subscription, but it is not a correctness
    // prerequisite: subscriber.rs applies late title/kind/url/scale updates
    // and refreshes color_profile on ParticipantMetadataChanged (#251).
    // Start the LiveKit video publish immediately so signaling RTTs overlap.
    log::info!(
        "session: start_share(window {window_id}) publishing track ({width}x{height}, quality Full)"
    );
    if let Some(diagnostics) = diagnostics.as_ref() {
        diagnostics.record_native_startup_stage(
            window_id,
            NativeStartupStageKind::PublishStarted,
            Some(width),
            Some(height),
            Some("quality=Full".to_string()),
        );
    }
    let (published_result, metadata_outcome, metadata_elapsed) = match publish_origin {
        SharePublishOrigin::Ordinary => {
            publish_media_while_metadata_runs(
                metadata_task,
                room_connection.publish_window_at(
                    width,
                    height,
                    ShareQuality::Full,
                    Some(window_id),
                ),
            )
            .await
        }
        SharePublishOrigin::PostWakeRestart => {
            let app_for_recovery = app.clone();
            let recovery = PostWakeEncoderFallbackRecovery::new(
                POST_WAKE_SOFTWARE_ENCODER_RETRY_DELAY,
                move || {
                    Box::pin(async move {
                        let state = app_for_recovery.state::<SessionState>();
                        retry_post_wake_software_encoder_fallback(
                            state.inner(),
                            window_id,
                            started_seq,
                        )
                        .await;
                    })
                },
            );
            publish_media_while_metadata_runs(
                metadata_task,
                room_connection.publish_window_at_after_wake(
                    width,
                    height,
                    ShareQuality::Full,
                    window_id,
                    recovery,
                ),
            )
            .await
        }
    };
    // A layout failure can race the network publish. Never construct or insert
    // an ActiveShare for that track: retire a just-published track locally,
    // stop capture, and surface the same stable code as first-frame failure.
    if layout_gate.is_failed() {
        if let Ok(track) = &published_result {
            let _ = track.unpublish().await;
        }
        let _ = stop_capture_with_timeout(
            Arc::new(capture),
            format!(
                "session: start_share(window {window_id}) capture-layout-invalid during publish"
            ),
        )
        .await;
        clear_failed_start_metadata(&state.share_metadata_apply_lock, || {
            room_connection.clear_shared_window_title_for_generation(window_id, started_seq)
        })
        .await;
        emit_capture_layout_invalid(diagnostic_source, CaptureLayoutStage::Publish);
        return Err(ShareSessionError::Capture(
            crate::capture::CAPTURE_LAYOUT_INVALID.to_string(),
        ));
    }
    match metadata_outcome {
        MetadataPublishOutcome::WithinBudget => {
            if let Some(diagnostics) = diagnostics.as_ref() {
                diagnostics.record_native_startup_stage(
                    window_id,
                    NativeStartupStageKind::MetadataWithinBudget,
                    Some(width),
                    Some(height),
                    Some(format!("{}ms", metadata_elapsed.as_millis())),
                );
            }
            log::info!(
                "session: start_share(window {window_id}) source metadata published in {}ms; publishing track",
                metadata_elapsed.as_millis()
            );
        }
        MetadataPublishOutcome::ExceededBudget => {
            if let Some(diagnostics) = diagnostics.as_ref() {
                diagnostics.record_native_startup_stage(
                    window_id,
                    NativeStartupStageKind::MetadataBudgetExpired,
                    Some(width),
                    Some(height),
                    Some(format!(
                        "{}ms > {}ms budget",
                        metadata_elapsed.as_millis(),
                        SHARE_METADATA_PUBLISH_BUDGET.as_millis()
                    )),
                );
            }
            log::warn!(
                "session: start_share(window {window_id}) source metadata publish slow ({}ms > {}ms budget); publishing track now, metadata completing in background (issue #249)",
                metadata_elapsed.as_millis(),
                SHARE_METADATA_PUBLISH_BUDGET.as_millis()
            );
        }
    }

    // New shares always start at `Full` -- they immediately become the
    // focused share (see below), and starting any lower would just mean an
    // extra republish a few milliseconds later.
    let published = match published_result {
        Ok(p) => p,
        Err(e) => {
            if let Some(diagnostics) = diagnostics.as_ref() {
                diagnostics.record_native_startup_stage(
                    window_id,
                    NativeStartupStageKind::PublishFailed,
                    Some(width),
                    Some(height),
                    Some(e.to_string()),
                );
            }
            log::error!(
                "session: start_share(window {window_id}) failed -- publish_window_at error: {e}"
            );
            if let Err(stop_error) = stop_capture_with_timeout(
                Arc::new(capture),
                format!("session: start_share(window {window_id}) publish failure"),
            )
            .await
            {
                log::warn!("session: failed to stop capture after publish failure: {stop_error}");
            }
            clear_failed_start_metadata(&state.share_metadata_apply_lock, || {
                room_connection.clear_shared_window_title_for_generation(window_id, started_seq)
            })
            .await;
            return Err(e.into());
        }
    };
    if let Some(diagnostics) = diagnostics.as_ref() {
        diagnostics.record_native_startup_publication(window_id, Some(published.sid().to_string()));
        diagnostics.record_native_startup_stage(
            window_id,
            NativeStartupStageKind::PublishSucceeded,
            Some(width),
            Some(height),
            Some(format!("sid={}", published.sid())),
        );
    }
    log::info!("session: start_share(window {window_id}) publish succeeded");

    let published = Arc::new(Mutex::new(Arc::new(published)));
    let republish_intent = Arc::new(RepublishCoordinator::default());
    let interaction_signal = Arc::new(InteractionSignal::default());
    register_interaction_signal(window_id, &interaction_signal);
    let (monitor_activation_tx, monitor_activation_rx) = tokio::sync::oneshot::channel();
    let SharePumpRuntime {
        pump_abort,
        monitor,
    } = spawn_share_pump(
        app.clone(),
        window_id,
        started_seq,
        0,
        room_connection.clone(),
        published.clone(),
        republish_intent.clone(),
        capture.configuration_handle(),
        latest_frame,
        latest_frame_notify,
        last_capture_wall_time_us,
        capture_error_rx,
        diagnostics.clone(),
        diagnostic_source,
        interaction_signal.clone(),
        priority.clone(),
        Some(monitor_activation_rx),
    );

    // #915: reuse `source_info`'s own `window_source::list()` lookup
    // (`bundle_id`/`raw_title`, both untouched since `.title`/`.url` were
    // moved out above) instead of `spawn_share_url_refresh` re-enumerating
    // every on-screen window a second time on this async share-start path.
    let url_refresh = spawn_share_url_refresh(
        room_connection.clone(),
        state.share_metadata_apply_lock.clone(),
        window_id,
        started_seq,
        source_kind == SharedSourceKind::Window && !has_source_title_override,
        source_info.bundle_id,
        source_info.raw_title,
    );

    let mut candidate = Some(ActiveShare {
        // Seeded from the user's global default; the per-share toggle
        // overrides it from here on.
        allow_remote_control: AtomicBool::new(
            state.remote_control_policy() != crate::remote_control_core::RemoteControlPolicy::Off,
        ),
        capture: Arc::new(capture),
        published,
        pump_abort,
        monitor,
        restart_generation: 0,
        pid: owner_pid,
        started_seq,
        frame: Mutex::new(frame),
        visible_on_screen: AtomicBool::new(true),
        known_closed: AtomicBool::new(false),
        source_kind,
        source_title,
        border_color: crate::hover_core::share_color_or_default(None),
        priority,
        interaction_signal,
        resolution,
        demand_resolution: Mutex::new(ViewerDemandResolutionState::default()),
        republish_intent,
        url_refresh,
    });
    let window_id_to_demote = layout_gate.activate_if_valid(|| {
        let mut guard = state.inner.lock_unpoisoned();
        let previously_focused = guard.focused_window();
        guard.shares.insert(
            window_id,
            candidate.take().expect("active share candidate available"),
        );
        // The new share is the highest `started_seq` by construction, so it
        // is now the focused one -- demote whichever share was focused
        // before it (if any, and if different from this one -- can't happen
        // in practice since this window_id was just inserted, but the
        // `!=` guard makes the invariant explicit rather than assumed).
        let demote = match previously_focused {
            Some(id) if id != window_id => Some(id),
            _ => None,
        };
        // Seed a self-expiring startup-grace demand for the window we're about
        // to demote so it holds `Full` if a viewer is already watching it but
        // their first Open/Heartbeat hasn't reached us yet. Without this, a
        // rapid second share can drop a still-watched window to 4fps for up to
        // ~2s until the next heartbeat repromotes it. Expires via the normal
        // `expire_stale_viewer_demands` loop if no real demand ever refreshes
        // it, so an unwatched window still drops to `Reduced` correctly.
        if let Some(demote_id) = demote {
            seed_startup_grace_demand(&mut guard, demote_id, Instant::now());
        }
        demote
    });
    let window_id_to_demote = match window_id_to_demote {
        Some(demote) => demote,
        None => {
            let candidate = candidate
                .take()
                .expect("failed activation preserves active share candidate");
            candidate.pump_abort.abort();
            candidate.monitor.abort();
            if let Some(url_refresh) = &candidate.url_refresh {
                url_refresh.stop();
            }
            unregister_interaction_signal(window_id, &candidate.interaction_signal);
            let published = candidate.published.lock_unpoisoned().clone();
            let _ = published.unpublish().await;
            let _ = stop_capture_with_timeout(
                candidate.capture.clone(),
                format!(
                    "session: start_share(window {window_id}) capture-layout-invalid before activation"
                ),
            )
            .await;
            clear_failed_start_metadata(&state.share_metadata_apply_lock, || {
                room_connection.clear_shared_window_title_for_generation(window_id, started_seq)
            })
            .await;
            emit_capture_layout_invalid(diagnostic_source, CaptureLayoutStage::Publish);
            return Err(ShareSessionError::Capture(
                crate::capture::CAPTURE_LAYOUT_INVALID.to_string(),
            ));
        }
    };
    if crate::region_window::resolve(window_id).is_some() {
        crate::region_window::set_active_share(window_id, true);
    }
    if monitor_activation_tx.send(()).is_err() {
        log::error!(
            "session: start_share(window {window_id}) capture monitor exited before activation"
        );
    }

    if let Some(demote_id) = window_id_to_demote {
        apply_quality(state, demote_id, ShareQuality::Reduced).await;
    }

    // SPEC.md §4.2's global-shortcut target: record this as "the last window
    // toggled," so the shortcut has something to act on -- see
    // `shortcuts.rs`. In-memory only (not persisted), same as every other
    // piece of `SessionState` -- a fresh app launch has no "last shared
    // window" until something is shared again, which is the correct/honest
    // behavior (nothing to re-share yet).
    state.set_last_toggled_window(window_id);

    log::info!(
        "session: start_share(window {window_id}) done -- share active (pump running, bookkeeping recorded)"
    );
    if emit_share_started {
        crate::analytics::share_started(analytics_source);
    }
    Ok(())
}

/// Outside-display warning lifecycle for macOS region shares (parity with
/// Windows `session_stub::sync_region_warning`): classify the selector
/// against its latched display, persist the state, and emit `region-warning`
/// with the selector's native label only on transitions. The frontend banner
/// routes by that label because capture tokens diverge from selector numbers.
fn sync_region_outside_display(
    app: &tauri::AppHandle,
    window_id: u32,
    last: &mut Option<bool>,
) {
    use tauri::Emitter;

    let Some(source) = crate::region_window::resolve(window_id) else {
        return;
    };
    let Some(outside) =
        crate::region_window::classify_outside_display(source.display, source.frame)
    else {
        return;
    };
    if *last == Some(outside) {
        return;
    }
    // Persist first so other consumers (metadata/replay gates) see it even if
    // the emit fails; the registry dedupes no-op writes.
    let changed = crate::region_window::set_outside_display(window_id, outside);
    if changed.is_none() && last.is_none() {
        return;
    }
    *last = Some(outside);
    if !changed.unwrap_or(false) {
        return;
    }
    let payload = serde_json::json!({
        "windowId": window_id,
        "selectorLabel": crate::region_window::selector_label_from_title(&source.title),
        "outsideDisplay": outside,
    });
    if let Err(error) = app.emit("region-warning", payload) {
        log::warn!("session: region warning emit failed for {window_id}: {error}");
    }
}

fn record_published_frame_timing(
    diagnostics: &Option<crate::diagnostics::DiagnosticsState>,
    window_id: u32,
    timing: Option<crate::transport::publisher::PublishedFrameTiming>,
) {
    let (Some(diagnostics), Some(timing)) = (diagnostics.as_ref(), timing) else {
        return;
    };
    diagnostics.record_capture_push_timing(
        window_id,
        timing.convert_ms,
        timing.capture_frame_return_ms,
    );
}

fn validated_snapshot_for_force_push(
    frame: crate::capture::CapturedFrame,
) -> Result<crate::capture::CapturedFrame, &'static str> {
    frame
        .layout_validated
        .then_some(frame)
        .ok_or(crate::capture::CAPTURE_LAYOUT_INVALID)
}

fn spawn_share_pump(
    app: tauri::AppHandle,
    window_id: u32,
    started_seq: u64,
    restart_generation: u64,
    room_connection: Arc<RoomConnection>,
    published: PublishedTrackSlot,
    republish_intent: RepublishIntent,
    capture_config: crate::capture::WindowCaptureConfig,
    latest_frame: LatestCapturedFrame,
    latest_frame_notify: Arc<tokio::sync::Notify>,
    last_capture_wall_time_us: Arc<AtomicU64>,
    capture_error_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    diagnostics: Option<crate::diagnostics::DiagnosticsState>,
    diagnostic_source: SourceSelectionClass,
    interaction_signal: Arc<InteractionSignal>,
    priority: Arc<Mutex<SharePriority>>,
    monitor_activation: Option<tokio::sync::oneshot::Receiver<()>>,
) -> SharePumpRuntime {
    log::info!(
        "session: window {window_id} frame pump starting (seq {started_seq}, restart_generation {restart_generation})"
    );
    let pump_activity_wall_time_us = Arc::new(AtomicU64::new(now_us()));
    let pump_last_push_wall_time_us = Arc::new(AtomicU64::new(now_us()));
    let pump_pushed_frames = Arc::new(AtomicU64::new(0));
    let pump_published = published.clone();
    let pump_republish_intent = republish_intent;
    let pump_room_connection = room_connection.clone();
    let monitor_capture_config = capture_config.clone();
    let pump_capture_config = capture_config;
    let pump_diagnostics = diagnostics.clone();
    let pump_activity_for_task = pump_activity_wall_time_us.clone();
    let pump_last_push_for_task = pump_last_push_wall_time_us.clone();
    let pump_pushed_frames_for_task = pump_pushed_frames.clone();
    let last_raw_for_pump = last_capture_wall_time_us.clone();
    let monitor_diagnostics = diagnostics.clone();
    let pump_app = app.clone();
    let pump = tokio::spawn(async move {
        let mut resize = ResizeDebounce::default();
        let mut dirty_rect_pump = DirtyRectPumpState::default();
        let dirty_rect_skip_enabled = dirty_rect_skip_enabled();
        // Crisp mode (#384 Phase 1 spike) -- see crisp_still.rs.
        let mut crisp_still_gate = crate::crisp_still::StillSendGate::default();
        let mut adaptive_idle_tick = AdaptiveIdleTick::default();
        let mut region_tick = tokio::time::interval(
            crate::region_window::REGION_GEOMETRY_INTERVAL,
        );
        region_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let is_region_share = crate::region_window::resolve(window_id).is_some();
        // Outside-display warning lifecycle (Windows parity): deduplicated at
        // the registry, emitted here on transitions only.
        let mut last_region_warning: Option<bool> = None;
        let mut remote_control_static_pushes = 0u32;
        let mut last_push_decision = "none";
        // #622: latch for the release-smoke "moving frames" liveness marker.
        let mut moving_liveness = MovingFrameLiveness::default();
        // Snapshot-pull fallback state (#183). Self-disables on a hard API
        // error (e.g. macOS 13, where SCScreenshotManager doesn't exist).
        // PETAL_DISABLE_SNAPSHOT_PULL is a DEBUG-ONLY kill switch -- a shipped
        // build must not let the environment turn the fallback off.
        let mut pull_enabled = {
            #[cfg(debug_assertions)]
            {
                std::env::var("PETAL_DISABLE_SNAPSHOT_PULL").is_err()
            }
            #[cfg(not(debug_assertions))]
            {
                true
            }
        };
        let mut pull_engaged = false;
        // #905: debounce rapid engage/disengage flapping. A real field log
        // showed 164,216 ENGAGED lines (9.9% of a 263 MB file) -- already
        // edge-triggered (only logged on a real state transition, never per
        // sample), but the STATE itself was flapping fast enough that the
        // transitions alone dominated the file. If a re-engage happens
        // within `PULL_FLAP_DEBOUNCE_US` of the last disengage, the ENGAGED
        // line is suppressed and folded into a rolled-up count reported on
        // the next real "disengaged" line instead.
        let mut pull_flap_count: u64 = 0;
        let mut last_disengage_us: u64 = 0;
        let mut last_pull_us: u64 = 0;
        let mut last_pull_hash: Option<u64> = None;
        let mut pull_total: u64 = 0;
        let mut pull_changed: u64 = 0;
        let mut last_pull_log_us: u64 = 0;
        let mut pull_consecutive_errors: u32 = 0;
        let mut pull_backoff_us: u64 = SNAPSHOT_PULL_MIN_INTERVAL_US;
        let mut handled_interaction_epoch: u64 = 0;
        let mut first_pushed_frame_recorded = false;
        log::info!(
            "session: window {window_id} frame pump loop started (seq {started_seq}, restart_generation {restart_generation})"
        );
        loop {
            // Always poll with a timeout, even before the static-frame pacer
            // has ever parked a frame -- a genuinely idle shared window (no
            // on-screen changes at all) can leave ScreenCaptureKit silent
            // for many seconds with zero callbacks, not just "identical
            // frames." Blocking here on `notified().await` with no timeout
            // (the old `else` branch) meant `pump_activity_for_task` never
            // refreshed during that silence, so `pump_watchdog_decision`
            // misread "no screen changes" as "the pump died" and forced a
            // stop_share/start_share cycle every `PUMP_STALL_THRESHOLD_US` --
            // confirmed live: a static shared TextEdit window restarted its
            // share every ~7s, forever, with no user-visible change at all.
            // The actual idle tick is 100ms (`PUMP_IDLE_POLL_INTERVAL`), not
            // the stale 250ms value previously documented here. It backs off
            // after an empty slot, but any raw-frame
            // or interaction wake resets it so active shares retain the
            // 100ms response floor.
            let wake = tokio::select! {
                _ = latest_frame_notify.notified() => SharePumpWake::RawFrame,
                _ = interaction_signal.notify.notified() => SharePumpWake::Interaction,
                _ = region_tick.tick(), if is_region_share => SharePumpWake::RegionTick,
                _ = tokio::time::sleep(adaptive_idle_tick.interval()) => SharePumpWake::IdleTick,
            };
            if wake != SharePumpWake::IdleTick && wake != SharePumpWake::RegionTick {
                adaptive_idle_tick.reset_on_wake();
            }
            pump_activity_for_task.store(now_us(), Ordering::Relaxed);

            // Poll the selector geometry at the pump cadence. The capture
            // owner applies the newest immutable generation before accepting
            // another native frame; failures hold the existing last-good frame.
            // Geometry and ScreenCaptureKit ROI work are deliberately tied to
            // the shared region tick. Raw frame, interaction, and idle wakes
            // must continue publishing the already-proven source without
            // paying a WindowServer query or native reconfiguration.
            if is_region_share && wake == SharePumpWake::RegionTick {
                if let Some(frame) = crate::window_registry::global()
                    .and_then(|registry| registry.frame_fresh(window_id))
                {
                    crate::region_window::update_frame(
                        window_id,
                        crate::region_window::RegionRect::new(
                            frame.x as f64,
                            frame.y as f64,
                            frame.width as f64,
                            frame.height as f64,
                        ),
                    );
                }
                match pump_capture_config.refresh_region_source() {
                    Ok(true) => {
                        // Never let a queued pre-ROI sample cross the
                        // generation boundary. The published track retains
                        // its last-good frame until the new native source
                        // produces a fresh raster.
                        latest_frame.lock_unpoisoned().take();
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::warn!(
                            "session: region {window_id} ROI refresh deferred; holding last-good frame: {error}"
                        );
                    }
                }
                sync_region_outside_display(
                    &pump_app,
                    window_id,
                    &mut last_region_warning,
                );
            }

            let mut interaction_pull_epoch = None;
            if wake == SharePumpWake::Interaction {
                // Raw capture normally wins this very short race for visible
                // windows. Covered sources emit Idle/no-image buffers, so
                // they fall through to one immediate coalesced snapshot.
                tokio::time::sleep(INTERACTION_RAW_CAPTURE_RACE).await;
                let epoch = interaction_signal.epoch.load(Ordering::Acquire);
                let applied_at_us = interaction_signal.applied_at_us.load(Ordering::Acquire);
                if interaction_snapshot_decision(
                    epoch,
                    handled_interaction_epoch,
                    last_raw_for_pump.load(Ordering::Relaxed),
                    applied_at_us,
                ) {
                    interaction_pull_epoch = Some(epoch);
                } else {
                    handled_interaction_epoch = handled_interaction_epoch.max(epoch);
                }
            }

            if wake != SharePumpWake::RawFrame && wake != SharePumpWake::RegionTick {
                let refresh_at = Instant::now();
                let refresh_at_us = now_us();
                let current = pump_published.lock_unpoisoned().clone();
                if let Some(frame) = dirty_rect_pump.idle_refresh_frame_at(
                    refresh_at,
                    refresh_at_us,
                    current.quality(),
                ) {
                    last_push_decision = "idle_static_refresh";
                    log::debug!(
                        "session: window {window_id} pump decision={last_push_decision}; pushing parked static refresh frame after notify idle"
                    );
                    let timing = current.push_frame(frame, refresh_at_us);
                    record_published_frame_timing(&pump_diagnostics, window_id, timing);
                    if !first_pushed_frame_recorded {
                        first_pushed_frame_recorded = true;
                        if let Some(diagnostics) = pump_diagnostics.as_ref() {
                            diagnostics.record_native_startup_stage(
                                window_id,
                                NativeStartupStageKind::FirstFramePushed,
                                Some(current.width()),
                                Some(current.height()),
                                Some("idle_static_refresh".to_string()),
                            );
                        }
                    }
                    pump_last_push_for_task.store(refresh_at_us, Ordering::Relaxed);
                    let pushed = pump_pushed_frames_for_task.fetch_add(1, Ordering::Relaxed) + 1;
                    if pushed == 1 || pushed % 300 == 0 {
                        log::info!(
                            "session: window {window_id} frame pump heartbeat -- pushed {pushed} frame(s), last decision={last_push_decision}"
                        );
                    }
                }

                // Snapshot-pull fallback (#183): the change-driven stream is
                // silent, but the source's backing content can still be
                // advancing without emitting dirty events (measured live:
                // a covered Chrome window playing audible video). Pull the
                // CURRENT content on demand and push it when it changed, so
                // those shares stay genuinely live instead of frozen.
                if pull_enabled {
                    let now_v = now_us();
                    let last_raw = last_raw_for_pump.load(Ordering::Relaxed);
                    let background_pull =
                        snapshot_pull_decision(now_v, last_raw, last_pull_us, pull_backoff_us);
                    let interaction_pull = interaction_pull_epoch.is_some()
                        && (pull_consecutive_errors == 0
                            || now_v.saturating_sub(last_pull_us) >= pull_backoff_us);
                    if background_pull || interaction_pull {
                        // Record the START, not completion. #285 measured each
                        // screenshot at 43-46ms; completion+100ms spacing made
                        // the supposed 10fps fallback actually ~6.9fps.
                        last_pull_us = now_v;
                        if let Some(epoch) = interaction_pull_epoch {
                            handled_interaction_epoch = handled_interaction_epoch.max(epoch);
                            if let Some(diagnostics) = pump_diagnostics.as_ref() {
                                diagnostics.record_native_startup_stage(
                                    window_id,
                                    NativeStartupStageKind::SnapshotPullStarted,
                                    None,
                                    None,
                                    Some(format!(
                                        "interaction epoch={epoch} input_seq={}",
                                        interaction_signal.input_seq.load(Ordering::Relaxed)
                                    )),
                                );
                            }
                            log::info!(
                                "capture-assist: window {window_id} snapshot starting after {}ms raw race for input_seq={} epoch={epoch}",
                                INTERACTION_RAW_CAPTURE_RACE.as_millis(),
                                interaction_signal.input_seq.load(Ordering::Relaxed)
                            );
                        }
                        let cfg = pump_capture_config.clone();
                        let pull_started = Instant::now();
                        match tokio::task::spawn_blocking(move || cfg.snapshot_frame()).await {
                            Ok(Ok(frame)) => {
                                let frame = match validated_snapshot_for_force_push(frame) {
                                    Ok(frame) => frame,
                                    Err(error) => {
                                        log::error!(
                                            "capture-pull: window {window_id} rejected unvalidated snapshot before force-push: {error}"
                                        );
                                        continue;
                                    }
                                };
                                let pull_ms = pull_started.elapsed().as_millis();
                                let snapshot_completed_us = now_us();
                                pull_total += 1;
                                if pull_consecutive_errors > 0 {
                                    log::info!(
                                        "capture-pull: window {window_id} recovered after {pull_consecutive_errors} consecutive errors"
                                    );
                                }
                                pull_consecutive_errors = 0;
                                pull_backoff_us = SNAPSHOT_PULL_MIN_INTERVAL_US;
                                if !pull_engaged {
                                    pull_engaged = true;
                                    let rapid_reengage = last_disengage_us != 0
                                        && now_v.saturating_sub(last_disengage_us)
                                            < PULL_FLAP_DEBOUNCE_US;
                                    if rapid_reengage {
                                        // #905: suppressed -- folded into the
                                        // next real "disengaged" line's
                                        // rolled-up flap count instead.
                                        pull_flap_count += 1;
                                    } else {
                                        log::info!(
                                            "capture-pull: window {window_id} ENGAGED -- raw stream silent {:.1}s, pulling snapshots at <= {:.0}fps",
                                            now_v.saturating_sub(last_raw) as f64 / 1_000_000.0,
                                            1_000_000.0 / SNAPSHOT_PULL_MIN_INTERVAL_US as f64
                                        );
                                    }
                                }
                                let current = pump_published.lock_unpoisoned().clone();
                                let dims_match = frame.width == current.width()
                                    && frame.height == current.height();
                                // Use the DENSE per-row fingerprint here, not the
                                // strided `capture_freeze_hash` (~1 byte/2KB): this
                                // hash is the actual push gate for a viewer update
                                // (a miss means Bob never sees the change, not just
                                // sees it late), whereas capture_freeze_hash only
                                // feeds a diagnostic/log verdict on the >=30fps live
                                // on_frame path where its cheapness matters. At the
                                // snapshot-pull's own <=10fps ceiling (each pull
                                // already costs tens of ms for the WindowServer
                                // composite+copy), a full scan of the Y plane is
                                // negligible overhead. Confirmed live (2026-07-08,
                                // window 14): the strided hash samples the SAME
                                // deterministic byte offsets every pull (fixed by
                                // width/stride), so a localized edit that never
                                // lands on a sampled offset -- a blinking cursor, one
                                // updated line in a big document, a status indicator
                                // -- is invisible not just for one cycle but FOREVER
                                // while the window size stays constant.
                                let hash = frame_fingerprint(&frame).hash;
                                let hash_changed = observe_snapshot_pull(
                                    window_id,
                                    last_pull_hash,
                                    hash,
                                    snapshot_completed_us,
                                );
                                if hash_changed && dims_match {
                                    last_pull_hash = Some(hash);
                                    pull_changed += 1;
                                    dirty_rect_pump.force_push_frame(
                                        frame,
                                        snapshot_completed_us,
                                        Instant::now(),
                                        current.quality(),
                                    );
                                    if let Some((parked, parked_ts)) =
                                        dirty_rect_pump.parked_frame()
                                    {
                                        let timing = current.push_frame(parked, parked_ts);
                                        record_published_frame_timing(
                                            &pump_diagnostics,
                                            window_id,
                                            timing,
                                        );
                                        if let Some(diagnostics) = pump_diagnostics.as_ref() {
                                            let stage = if first_pushed_frame_recorded {
                                                NativeStartupStageKind::SnapshotPullPushed
                                            } else {
                                                first_pushed_frame_recorded = true;
                                                NativeStartupStageKind::FirstFramePushed
                                            };
                                            diagnostics.record_native_startup_stage(
                                                window_id,
                                                stage,
                                                Some(current.width()),
                                                Some(current.height()),
                                                Some("pull_snapshot".to_string()),
                                            );
                                        }
                                        pump_last_push_for_task
                                            .store(snapshot_completed_us, Ordering::Relaxed);
                                        let pushed = pump_pushed_frames_for_task
                                            .fetch_add(1, Ordering::Relaxed)
                                            + 1;
                                        last_push_decision = "pull_snapshot";
                                        if pushed == 1 || pushed % 300 == 0 {
                                            log::info!(
                                                "session: window {window_id} frame pump heartbeat -- pushed {pushed} frame(s), last decision={last_push_decision}"
                                            );
                                        }
                                        if let Some(moving) =
                                            moving_liveness.observe(last_push_decision)
                                        {
                                            log::info!(
                                                "session: window {window_id} share liveness confirmed -- {moving} moving frame(s) pushed (last decision={last_push_decision})"
                                            );
                                        }
                                    }
                                } else if hash_changed {
                                    // Content advanced but dims no longer match the
                                    // published track (resize while silent); a raw
                                    // frame will arrive and drive the resize path.
                                    last_pull_hash = Some(hash);
                                }
                                if now_us().saturating_sub(last_pull_log_us) >= 5_000_000 {
                                    last_pull_log_us = now_us();
                                    // last_pull_hash is logged so a future long
                                    // changed_pushed plateau is diagnosable after
                                    // the fact: if the hash is also flat across
                                    // several of these lines, the source was
                                    // genuinely idle (no bug); if the hash keeps
                                    // moving while changed_pushed doesn't, that
                                    // points at a real detection/gating bug (e.g.
                                    // dims_match failing) rather than idle content.
                                    let last_pull_hash = last_pull_hash.unwrap_or_default();
                                    log::info!(
                                        "capture-pull: window {window_id} snapshots={pull_total} changed_pushed={pull_changed} last_pull={pull_ms}ms last_hash={last_pull_hash:016x} (raw silent {:.1}s)",
                                        now_us().saturating_sub(
                                            last_raw_for_pump.load(Ordering::Relaxed)
                                        ) as f64
                                            / 1_000_000.0
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                if let Some(diagnostics) = pump_diagnostics.as_ref() {
                                    diagnostics.record_native_startup_stage(
                                        window_id,
                                        NativeStartupStageKind::SnapshotPullFailed,
                                        None,
                                        None,
                                        Some(e.to_string()),
                                    );
                                }
                                pull_consecutive_errors += 1;
                                // Widen the retry interval instead of giving up --
                                // live-observed: SCK's screenshot backend can
                                // reject captures ("Stream failed to start") for
                                // tens of seconds after sustained polling, then
                                // recover on its own. Backing off keeps retries
                                // cheap while we wait it out.
                                pull_backoff_us =
                                    (pull_backoff_us * 2).min(SNAPSHOT_PULL_BACKOFF_MAX_US);
                                if pull_consecutive_errors == 1 || pull_consecutive_errors % 50 == 0
                                {
                                    log::warn!(
                                        "capture-pull: window {window_id} snapshot failed ({pull_consecutive_errors}x, backoff now {:.1}s): {e}",
                                        pull_backoff_us as f64 / 1_000_000.0
                                    );
                                }
                                // Only truly disable if EVERY pull for this share
                                // has failed (never had a single success) -- that
                                // is the real "unsupported" signal (macOS < 14, or
                                // the capture source is fundamentally unusable),
                                // not a transient backend hiccup.
                                if pull_total == 0 && pull_consecutive_errors >= 5 {
                                    pull_enabled = false;
                                    log::warn!(
                                        "capture-pull: window {window_id} disabling snapshot pull -- {pull_consecutive_errors} consecutive failures with zero prior successes (macOS < 14, or capture source unusable)"
                                    );
                                }
                            }
                            Err(join_err) => {
                                log::warn!(
                                    "capture-pull: window {window_id} snapshot task failed: {join_err}"
                                );
                            }
                        }
                    }
                }
                adaptive_idle_tick.back_off_after_empty_idle_tick(
                    latest_frame.lock_unpoisoned().is_none(),
                    pull_engaged || interaction_pull_epoch.is_some(),
                );
                continue;
            }

            let Some((frame, ts)) = latest_frame.lock_unpoisoned().take() else {
                continue;
            };
            if !pump_capture_config.frame_matches_current_region(&frame) {
                log::debug!(
                    "session: window {window_id} dropped frame from stale region generation"
                );
                continue;
            }
            if pull_engaged {
                pull_engaged = false;
                last_disengage_us = now_us();
                if pull_flap_count > 0 {
                    log::info!(
                        "capture-pull: window {window_id} disengaged -- raw stream resumed ({pull_total} snapshots pulled, {pull_changed} pushed, {pull_flap_count} additional engage/disengage flaps suppressed, #905)"
                    );
                    pull_flap_count = 0;
                } else {
                    log::info!(
                        "capture-pull: window {window_id} disengaged -- raw stream resumed ({pull_total} snapshots pulled, {pull_changed} pushed)"
                    );
                }
            }
            let current = pump_published.lock_unpoisoned().clone();
            let resize_decision =
                resize.observe(current.width(), current.height(), frame.width, frame.height);
            // #714: `resize_pump_action` is the single, separately-tested
            // source of truth for whether a resize decision drops this
            // frame. See its doc comment -- every current variant pushes;
            // this call is what makes that a real, load-bearing invariant
            // of the pump loop rather than an implicit property of which
            // match arms happen to contain `continue`.
            if resize_pump_action(resize_decision) == ResizePumpAction::SkipThisFrame {
                continue;
            }
            match resize_decision {
                ResizeDecision::MatchingPublishedSize => {}
                ResizeDecision::WaitingForStableSize {
                    width,
                    height,
                    frames,
                } => {
                    // Still falls through to the normal push path below
                    // (see `resize_pump_action`'s doc comment). This
                    // debounce still decides WHEN to pay the unpublish/
                    // republish cost (unchanged); it must not also decide
                    // whether the receiver sees anything at all while it
                    // waits. `PublishedTrack::push_frame` letterbox-scales a
                    // mismatched-size frame to the currently published size
                    // (see its doc comment) rather than dropping it, so a
                    // resize gesture that runs long enough to trip the
                    // frame-pump-stall watchdog no longer produces a
                    // multi-second gap with nothing reaching the viewer.
                    if frames == 1 {
                        log::info!(
                            "session: window {window_id} captured resized frame {width}x{height}; waiting for stable size before republish (still pushing, letterboxed to the published size)"
                        );
                    }
                }
                ResizeDecision::StableResize { width, height } => {
                    // #869: claim BEFORE bumping the intent. A refused claim
                    // that still bumped cancelled whichever quality republish
                    // owned the slot, without replacing it. And `ResizeDebounce`
                    // re-reports StableResize every frame while suppressed, so
                    // both this claim and its log fire per frame if unguarded.
                    if !claim_republish_reconcile_slot(window_id, "resize") {
                        // Fall through to push -- push_frame letterboxes a
                        // mismatched frame (#714). Dropping here froze the
                        // viewer for the whole 3s suppression window.
                        // reset() so the next attempt needs a fresh run of
                        // stable frames instead of re-claiming (and re-logging)
                        // on every frame until the limiter opens.
                        resize.reset();
                    } else {
                        log::info!(
                        "session: window {window_id} resize stabilized at {width}x{height}; republishing track"
                    );
                        let intent_generation = begin_republish_intent(&pump_republish_intent);
                        if republish_window_for_resize(
                            pump_room_connection.clone(),
                            pump_published.clone(),
                            pump_republish_intent.clone(),
                            intent_generation,
                            &pump_capture_config,
                            window_id,
                        )
                        .await
                        {
                            let current = pump_published.lock_unpoisoned().clone();
                            let fps = current
                                .quality()
                                .capture_fps()
                                .min(priority.lock_unpoisoned().capture_fps());
                            if let Err(error) = pump_capture_config.update_fps(fps) {
                                log::warn!(
                                "session: window {window_id} could not restore preference fps cap after resize: {error}"
                            );
                            }
                            resize.reset();
                        } else {
                            // #869: do NOT `continue` here either. A failed
                            // republish is no reason to withhold the frame; the
                            // viewer gets the letterboxed frame and the next
                            // stable run retries.
                            resize.reset();
                        }
                    }
                }
            }
            let mut current = pump_published.lock_unpoisoned().clone();
            if crate::remote_control::window_has_active_controller(window_id) {
                if current.quality() != ShareQuality::Full {
                    if current.set_quality(ShareQuality::Full).await.is_ok() {
                        let fps = current
                            .quality()
                            .capture_fps()
                            .min(priority.lock_unpoisoned().capture_fps());
                        if let Err(error) = pump_capture_config.update_fps(fps) {
                            log::warn!(
                                "session: window {window_id} could not restore preference fps cap after remote-control promotion: {error}"
                            );
                        }
                    }
                }
                remote_control_static_pushes = remote_control_static_pushes.saturating_add(1);
                last_push_decision = "bypass_static_pacer_remote_control";
                if remote_control_static_pushes == 1 || remote_control_static_pushes % 120 == 0 {
                    log::info!(
                        "session: window {window_id} pump decision={last_push_decision}; bypassing static-frame pacer during active remote control ({} frame(s))",
                        remote_control_static_pushes
                    );
                }
                dirty_rect_pump.force_push_frame(frame, ts, Instant::now(), current.quality());
            } else {
                remote_control_static_pushes = 0;
                match dirty_rect_pump.observe_captured_frame(
                    frame,
                    ts,
                    Instant::now(),
                    current.quality(),
                    dirty_rect_skip_enabled,
                    false,
                ) {
                    DirtyRectFrameDecision::Push(reason) => {
                        last_push_decision = reason.as_log_label();
                        if let DirtyRectPushReason::DirtyRectAfterSkip { skipped_frames } = reason {
                            log::debug!(
                                "session: window {window_id} pump decision={last_push_decision}; dirty rect resumed immediately after {skipped_frames} skipped frame(s)"
                            );
                        }
                    }
                    DirtyRectFrameDecision::Skip { run_length } => {
                        last_push_decision = "skip_dirty_rect_clean";
                        if run_length == 1 || run_length % 300 == 0 {
                            log::debug!(
                                "session: window {window_id} pump decision={last_push_decision}; skipping dirty-rect-clean frame(s) ({run_length} in current run)"
                            );
                        }
                        // Crisp mode (#384 Phase 1 spike): this dirty-rect-clean
                        // skip run is EXACTLY the existing "window has been
                        // static" signal the still-image path reuses rather
                        // than reimplementing -- see crisp_still.rs's module
                        // doc comment for full scope (sender + wire protocol +
                        // invalidation logic only; no receiver-side blit yet).
                        // No-op on every tick except the rare one the gate
                        // actually fires on (no allocation, no encode).
                        if let Some((parked_frame, parked_ts)) = dirty_rect_pump.parked_frame() {
                            crate::crisp_still::maybe_trigger_still(
                                &mut crisp_still_gate,
                                window_id,
                                run_length,
                                parked_frame,
                                parked_ts,
                                &pump_room_connection,
                            );
                        }
                        continue;
                    }
                }
            }
            let Some((frame, ts)) = dirty_rect_pump.parked_frame() else {
                continue;
            };
            let Some(timing) = pump_capture_config
                .with_current_region_frame(frame, || current.push_frame(frame, ts))
            else {
                dirty_rect_pump.parked_frame = None;
                log::debug!(
                    "session: window {window_id} dropped parked frame from stale region generation"
                );
                continue;
            };
            if let Some(generation) = frame.region_generation {
                let pushed_so_far = pump_pushed_frames_for_task.load(Ordering::Relaxed);
                if pushed_so_far == 0 || pushed_so_far % 120 == 0 {
                    log::info!(
                        "session: window {window_id} published ROI frame generation={generation} dimensions={}x{}",
                        frame.width,
                        frame.height
                    );
                }
            }
            record_published_frame_timing(&pump_diagnostics, window_id, timing);
            if !first_pushed_frame_recorded {
                first_pushed_frame_recorded = true;
                if let Some(diagnostics) = pump_diagnostics.as_ref() {
                    diagnostics.record_native_startup_stage(
                        window_id,
                        NativeStartupStageKind::FirstFramePushed,
                        Some(current.width()),
                        Some(current.height()),
                        Some(last_push_decision.to_string()),
                    );
                }
            }
            let pushed_at_us = now_us();
            pump_last_push_for_task.store(pushed_at_us, Ordering::Relaxed);
            let pushed = pump_pushed_frames_for_task.fetch_add(1, Ordering::Relaxed) + 1;
            if pushed == 1 || pushed % 300 == 0 {
                log::info!(
                    "session: window {window_id} frame pump heartbeat -- pushed {pushed} frame(s), last decision={last_push_decision}"
                );
            }
            if let Some(moving) = moving_liveness.observe(last_push_decision) {
                log::info!(
                    "session: window {window_id} share liveness confirmed -- {moving} moving frame(s) pushed (last decision={last_push_decision})"
                );
            }
        }
    });
    let pump_abort = pump.abort_handle();
    let monitor = spawn_share_monitor(
        app,
        window_id,
        started_seq,
        restart_generation,
        last_capture_wall_time_us,
        capture_error_rx,
        monitor_capture_config,
        published,
        pump,
        pump_activity_wall_time_us,
        pump_last_push_wall_time_us,
        pump_pushed_frames,
        monitor_diagnostics,
        diagnostic_source,
        monitor_activation,
    );

    SharePumpRuntime {
        pump_abort,
        monitor,
    }
}

async fn await_monitor_activation(activation: Option<tokio::sync::oneshot::Receiver<()>>) -> bool {
    match activation {
        Some(activation) => activation.await.is_ok(),
        None => true,
    }
}

fn spawn_share_monitor(
    app: tauri::AppHandle,
    window_id: u32,
    started_seq: u64,
    restart_generation: u64,
    last_capture_wall_time_us: Arc<AtomicU64>,
    mut capture_error_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    capture_config: crate::capture::WindowCaptureConfig,
    published: PublishedTrackSlot,
    mut pump: tokio::task::JoinHandle<()>,
    pump_activity_wall_time_us: Arc<AtomicU64>,
    pump_last_push_wall_time_us: Arc<AtomicU64>,
    pump_pushed_frames: Arc<AtomicU64>,
    diagnostics: Option<crate::diagnostics::DiagnosticsState>,
    diagnostic_source: SourceSelectionClass,
    monitor_activation: Option<tokio::sync::oneshot::Receiver<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !await_monitor_activation(monitor_activation).await {
            pump.abort();
            return;
        }
        let mut interval = tokio::time::interval(CAPTURE_WATCHDOG_INTERVAL);
        let mut last_silent_log_us = 0;
        let mut layout_reconfigure_deadline: Option<(tokio::time::Instant, (u32, u32))> = None;
        // Debounced ROI reconfiguration: events stash the newest target here
        // and the timed apply branch below commits it. A resize storm
        // (2026-07-30 defect A) therefore collapses to at most one SCStream
        // reconfiguration per LAYOUT_RECONFIGURE_MIN_SPACING.
        let mut layout_apply_not_before: Option<tokio::time::Instant> = None;
        let mut deferred_layout_target: Option<(u32, u32)> = None;
        loop {
            tokio::select! {
                Some(error) = capture_error_rx.recv() => {
                    match coalesce_capture_error_events(
                        error,
                        || capture_error_rx.try_recv().ok(),
                    ) {
                        CoalescedCaptureEvent::Reconfigure { width, height } => {
                            // The newest target supersedes any deferred one;
                            // the apply branch below picks it up.
                            deferred_layout_target = Some((width, height));
                        }
                        CoalescedCaptureEvent::Failure(error) => {
                            if let Some(diagnostics) = diagnostics.as_ref() {
                                diagnostics.mark_capture_wedged(window_id);
                            }
                            let sleep_correlated = sleep_correlated_for_capture_failure();
                            match capture_failure_action(error, sleep_correlated) {
                                CaptureFailureAction::RestartInPlace { message } => {
                                    // Only emit the layout-invalid diagnostic for
                                    // the original layout-invalid path; sleep
                                    // interruption must not inflate
                                    // CAPTURE_LAYOUT_INVALID.
                                    if message == crate::capture::CAPTURE_LAYOUT_INVALID {
                                        emit_capture_layout_invalid(
                                            diagnostic_source,
                                            CaptureLayoutStage::Validation,
                                        );
                                    }
                                    log::warn!(
                                        "session: window {window_id} {message} -- restarting capture in place (share stays published)"
                                    );
                                    pump.abort();
                                    spawn_pump_failure_recovery(
                                        app.clone(),
                                        window_id,
                                        started_seq,
                                        restart_generation,
                                        message,
                                    );
                                    break;
                                }
                                CaptureFailureAction::StopShare { message } => {
                                    log::error!("session: window {window_id} {message}");
                                    spawn_capture_failure_cleanup(app.clone(), window_id, message);
                                    break;
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(
                    layout_apply_not_before.unwrap_or_else(tokio::time::Instant::now)
                ), if deferred_layout_target.is_some() => {
                    let Some((width, height)) = deferred_layout_target.take() else {
                        continue;
                    };
                    let fps = capture_config.fps();
                    let resolution = capture_config.resolution();
                    // #804: teach the capture-size authority about this ROI
                    // BEFORE applying it. `capture_size_for_resolution` then
                    // returns the ROI instead of the padded size it computes
                    // itself, so `apply_quality` stops republishing the share
                    // back to the padded size on every reconcile.
                    let (base_width, base_height, _) =
                        capture_config.computed_capture_size_for_resolution(resolution);
                    capture_config
                        .layout_gate()
                        .record_roi_adjustment((base_width, base_height), (width, height));
                    match capture_config
                        .update_stream_configuration(width, height, fps, resolution)
                    {
                        Ok(()) => {
                            let now = tokio::time::Instant::now();
                            layout_reconfigure_deadline =
                                Some((now + LAYOUT_RECONFIGURE_ACK_TIMEOUT, (width, height)));
                            layout_apply_not_before =
                                Some(now + LAYOUT_RECONFIGURE_MIN_SPACING);
                            log::info!(
                                "session: window {window_id} requested padded capture ROI output {width}x{height}; awaiting a matching frame"
                            );
                        }
                        Err(reconfigure_error) => {
                            log::warn!(
                                "session: window {window_id} capture ROI reconfiguration to {width}x{height} failed: {reconfigure_error} -- restarting capture in place"
                            );
                            capture_config.layout_gate().fail();
                            if let Some(diagnostics) = diagnostics.as_ref() {
                                diagnostics.mark_capture_wedged(window_id);
                            }
                            emit_capture_layout_invalid(
                                diagnostic_source,
                                CaptureLayoutStage::Reconfiguration,
                            );
                            pump.abort();
                            spawn_pump_failure_recovery(
                                app.clone(),
                                window_id,
                                started_seq,
                                restart_generation,
                                format!(
                                    "capture ROI reconfiguration to {width}x{height} failed: {reconfigure_error}"
                                ),
                            );
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    if let Some((deadline, target)) = layout_reconfigure_deadline {
                        let pending = capture_config.layout_gate().pending_reconfiguration();
                        if pending_layout_ack_expired(
                            pending,
                            target,
                            tokio::time::Instant::now() >= deadline,
                        ) {
                                // #804: bound the recovery. Restarting capture
                                // re-derives the same target, so an ROI that
                                // cannot be acknowledged restarts the share
                                // forever. After LAYOUT_ROI_MAX_ATTEMPTS, keep
                                // the padded raster and stay published -- a
                                // couple of pillarbox pixels beat an endless
                                // publication teardown.
                                let attempts =
                                    record_layout_roi_ack_failure(window_id, target);
                                if layout_ack_failure_action(attempts)
                                    == LayoutAckFailureAction::AbandonRoi
                                {
                                    if capture_config.layout_gate().abandon_roi(target) {
                                        log::warn!(
                                            "session: window {window_id} capture ROI {}x{} went unacknowledged {attempts}x -- keeping the padded capture output and staying published (no further restarts)",
                                            target.0,
                                            target.1
                                        );
                                    }
                                    layout_reconfigure_deadline = None;
                                    deferred_layout_target = None;
                                    continue;
                                }
                                capture_config.layout_gate().fail();
                                log::warn!(
                                    "session: window {window_id} capture ROI reconfiguration to {}x{} produced no matching stream frame within {}ms -- restarting capture in place",
                                    target.0,
                                    target.1,
                                    LAYOUT_RECONFIGURE_ACK_TIMEOUT.as_millis()
                                );
                                if let Some(diagnostics) = diagnostics.as_ref() {
                                    diagnostics.mark_capture_wedged(window_id);
                                }
                                emit_capture_layout_invalid(
                                    diagnostic_source,
                                    CaptureLayoutStage::Reconfiguration,
                                );
                                pump.abort();
                                spawn_pump_failure_recovery(
                                    app.clone(),
                                    window_id,
                                    started_seq,
                                    restart_generation,
                                    format!(
                                        "capture ROI reconfiguration to {}x{} was never acknowledged",
                                        target.0, target.1
                                    ),
                                );
                                break;
                        } else if pending != Some(target) {
                            // Acknowledged. #804: the attempt budget is for
                            // ONE unacknowledgeable target in a row -- a few
                            // transient failures spread across a long meeting
                            // must not add up to a pre-poisoned share.
                            clear_layout_roi_ack_failures(window_id);
                            layout_reconfigure_deadline = None;
                        }
                    }
                    let now = now_us();
                    let last = last_capture_wall_time_us.load(Ordering::Relaxed);
                    match capture_watchdog_decision(
                        now,
                        last,
                        crate::window_source::has_screen_recording_access(),
                    ) {
                        CaptureWatchdogDecision::Healthy => {}
                        CaptureWatchdogDecision::StalledPermissionDenied => {
                            let message = "capture stalled -- Screen Recording permission was revoked".to_string();
                            log::error!("session: window {window_id} {message}");
                            spawn_capture_failure_cleanup(app.clone(), window_id, message);
                            break;
                        }
                    }

                    let last_pushed = pump_last_push_wall_time_us.load(Ordering::Relaxed);
                    let push_silence_us = now.saturating_sub(last_pushed);
                    if push_silence_us > PUMP_SILENT_LOG_THRESHOLD_US
                        && now.saturating_sub(last_silent_log_us) > PUMP_SILENT_LOG_THRESHOLD_US
                    {
                        last_silent_log_us = now;
                        let pushed = pump_pushed_frames.load(Ordering::Relaxed);
                        log::warn!(
                            "session: window {window_id} frame pump has pushed no frames for {:.1}s (total pushed: {pushed})",
                            push_silence_us as f64 / 1_000_000.0
                        );
                    }

                    let last_activity = pump_activity_wall_time_us.load(Ordering::Relaxed);
                    match pump_watchdog_decision(now, last_activity) {
                        PumpWatchdogDecision::Healthy => {}
                        PumpWatchdogDecision::Stalled { silent_for_us } => {
                            let message = format!(
                                "frame pump stalled for {:.1}s",
                                silent_for_us as f64 / 1_000_000.0
                            );
                            log::warn!(
                                "session: window {window_id} {message}; restarting capture in place"
                            );
                            published
                                .lock_unpoisoned()
                                .disable_native_zero_copy("frame pump watchdog fired before recovery");
                            pump.abort();
                            spawn_pump_failure_recovery(
                                app.clone(),
                                window_id,
                                started_seq,
                                restart_generation,
                                message,
                            );
                            break;
                        }
                    }

                    match raw_capture_watchdog_decision(
                        now,
                        last,
                        source_appears_idle(window_id),
                    ) {
                        RawCaptureWatchdogDecision::Healthy => {}
                        RawCaptureWatchdogDecision::IdleHealthy { silent_for_us } => {
                            // Source is idle/occluded (not drawing), stream is
                            // healthy -- do NOT restart (that just republishes
                            // the track for nothing). Log a throttled heartbeat
                            // so the idle state is visible without churn.
                            if now.saturating_sub(last_silent_log_us)
                                > RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US
                            {
                                last_silent_log_us = now;
                                log::info!(
                                    "session: window {window_id} raw capture idle for {:.1}s -- source not drawing, stream healthy, holding last frame (no restart)",
                                    silent_for_us as f64 / 1_000_000.0
                                );
                            }
                        }
                        RawCaptureWatchdogDecision::Stalled { silent_for_us } => {
                            // Snapshot pulls succeeding (#183) prove the share is
                            // alive and delivering content even though the push
                            // stream is silent -- restarting would only churn the
                            // track. Hold as long as pulls stay fresh, but only
                            // for a bounded grace period -- see
                            // RAW_CAPTURE_STALL_HOLD_GRACE_US's doc comment for
                            // why an unbounded hold here was a one-way ratchet
                            // that left a wedged raw stream degrading forever.
                            if raw_capture_stall_hold(
                                silent_for_us,
                                snapshot_pull_fresh_within(window_id, now, 10_000_000),
                            ) {
                                if now.saturating_sub(last_silent_log_us)
                                    > RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US
                                {
                                    last_silent_log_us = now;
                                    log::info!(
                                        "session: window {window_id} raw capture silent {:.1}s but snapshot pulls are delivering fresh content -- no restart",
                                        silent_for_us as f64 / 1_000_000.0
                                    );
                                }
                                continue;
                            }
                            let message = format!(
                                "no raw ScreenCaptureKit frames for {:.1}s (source was active then stopped, or {:.0}s hard-restart net -- capture may be wedged, restarting)",
                                silent_for_us as f64 / 1_000_000.0,
                                RAW_CAPTURE_HARD_RESTART_THRESHOLD_US as f64 / 1_000_000.0
                            );
                            if let Some(diagnostics) = diagnostics.as_ref() {
                                diagnostics.mark_capture_wedged(window_id);
                            }
                            log::error!("session: window {window_id} {message}");
                            published
                                .lock_unpoisoned()
                                .disable_native_zero_copy("raw capture watchdog fired; possible IOSurface pool starvation");
                            pump.abort();
                            spawn_pump_failure_recovery(
                                app.clone(),
                                window_id,
                                started_seq,
                                restart_generation,
                                message,
                            );
                            break;
                        }
                    }
                }
                pump_result = &mut pump => {
                    let message = match pump_result {
                        Ok(()) => "frame pump exited unexpectedly".to_string(),
                        Err(error) if error.is_cancelled() => {
                            log::info!(
                                "session: window {window_id} frame pump aborted; monitor exiting"
                            );
                            break;
                        }
                        Err(error) if error.is_panic() => {
                            format!("frame pump panicked: {error}")
                        }
                        Err(error) => format!("frame pump failed: {error}"),
                    };
                    log::error!(
                        "session: window {window_id} {message}; restarting capture in place"
                    );
                    spawn_pump_failure_recovery(
                        app.clone(),
                        window_id,
                        started_seq,
                        restart_generation,
                        message,
                    );
                    break;
                }
                else => break,
            }
        }
    })
}

async fn stop_capture_with_timeout(
    capture: Arc<WindowCapture>,
    context: String,
) -> Result<(), String> {
    match tokio::time::timeout(
        CAPTURE_STOP_TIMEOUT,
        tokio::task::spawn_blocking(move || capture.stop()),
    )
    .await
    {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.to_string()),
        Ok(Err(error)) => Err(format!("stop task failed: {error}")),
        Err(_) => Err(format!("timed out after {:?}", CAPTURE_STOP_TIMEOUT)),
    }
    .map_err(|error| format!("{context}: {error}"))
}

fn spawn_pump_failure_recovery(
    app: tauri::AppHandle,
    window_id: u32,
    started_seq: u64,
    restart_generation: u64,
    message: String,
) {
    tokio::spawn(async move {
        use tauri::Manager;

        let Some(state) = app.try_state::<SessionState>() else {
            log::warn!("session: pump recovery for window {window_id} could not find SessionState");
            return;
        };
        // #734: do not call ScreenCaptureKit while the display/system is
        // still asleep — the restart would fail with the same "no capture
        // source" error and permanently tear down a share that only needed
        // to wait for wake. Poll until wake (or the share is gone / a
        // hard cap is hit), then proceed with the normal in-place restart.
        {
            const MAX_SLEEP_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
            let wait_started = std::time::Instant::now();
            let mut logged_wait = false;
            while capture_restart_should_wait_for_wake() {
                if !state.is_share_restart_generation_active(
                    window_id,
                    started_seq,
                    restart_generation,
                ) {
                    log::info!(
                        "session: abandoning sleep-deferred pump recovery for window {window_id} -- share no longer current"
                    );
                    return;
                }
                if wait_started.elapsed() > MAX_SLEEP_WAIT {
                    log::warn!(
                        "session: window {window_id} still sleep-correlated after {MAX_SLEEP_WAIT:?}; attempting capture restart anyway"
                    );
                    break;
                }
                if !logged_wait {
                    log::info!(
                        "session: window {window_id} deferring capture restart until display/system wake ({message})"
                    );
                    logged_wait = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        let Some(snapshot) =
            state.active_share_restart_snapshot(window_id, started_seq, restart_generation)
        else {
            log::info!(
                "session: ignoring stale pump recovery for window {window_id} seq {started_seq} restart_generation {restart_generation}"
            );
            return;
        };
        let recent_failures = record_pump_recovery_failure(window_id, now_us());
        if pump_failure_recovery_decision(recent_failures)
            == PumpFailureRecoveryDecision::CircuitOpen
        {
            log::error!(
                "session: window {window_id} pump recovery circuit open after {recent_failures} failures within {}s (restart_generation {restart_generation}); stopping share",
                PUMP_RECOVERY_FAILURE_WINDOW_US / 1_000_000
            );
            if let Err(error) = stop_share_explained(
                &app,
                state.inner(),
                window_id,
                StopShareAnalytics::CaptureFailed,
            )
            .await
            {
                log::warn!("session: failed to stop share after recovery circuit opened: {error}");
            }
            // Mirror every other autonomous (non-user-initiated) teardown
            // path in this function (e.g. the in-place-restart-failure arm
            // just below): stop_share() alone does not clear hover-tab
            // share state, revoke remote-control grants, or surface an
            // error -- without these the UI silently keeps showing the
            // window as "sharing" after it has actually died (issue #13's
            // documented stale-UI failure mode).
            crate::hover_tab::clear_share_state_for_window(&app, window_id);
            crate::remote_control::revoke_window(
                &app,
                window_id,
                "share pump recovery circuit open",
            );
            crate::hover_tab::emit_share_error(
                &app,
                window_id,
                true,
                crate::session::ShareSessionError::Capture(
                    "capture kept failing to restart and was stopped".to_string(),
                ),
            );
            return;
        }
        // A pump restart does not own a republish generation. Waiting for an
        // apply-lock-held transaction lets an in-flight demand/resize repair
        // commit, while bumping the generation here (the old
        // begin_republish_intent(&snapshot.republish_intent) call) would
        // cancel that repair with no follow-through (#417).
        let _republish_apply_guard = snapshot.republish_intent.apply_lock.lock().await;
        drop(_republish_apply_guard);

        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            let diagnostics: crate::diagnostics::DiagnosticsState = diagnostics.inner().clone();
            diagnostics.journal_append(
                &app,
                "warning",
                format!("Restarting shared window {window_id}: {message}"),
            );
        }

        let (old_capture, had_url_refresh) = {
            let guard = state.inner.lock_unpoisoned();
            let Some(share) = guard.shares.get(&window_id) else {
                log::info!(
                    "session: pump recovery for window {window_id} found share removed before capture stop"
                );
                return;
            };
            if share.started_seq != started_seq || share.restart_generation != restart_generation {
                log::info!(
                    "session: ignoring stale pump recovery for window {window_id} before capture stop (seq {started_seq}, restart_generation {restart_generation})"
                );
                return;
            }
            log::info!(
                "session: window {window_id} pump recovery aborting old capture monitor and frame pump"
            );
            share.monitor.abort();
            share.pump_abort.abort();
            // #915: the old poller is tied to the capture generation being
            // torn down here; abort it now and respawn a fresh one after
            // the capture is rebuilt below (see `new_url_refresh`), rather
            // than leaking it running detached.
            if let Some(url_refresh) = &share.url_refresh {
                url_refresh.stop();
            }
            (Arc::clone(&share.capture), share.url_refresh.is_some())
        };
        log::info!("session: window {window_id} pump recovery capture.stop() begin");
        if let Err(error) = stop_capture_with_timeout(
            old_capture,
            format!("session: stopping old capture for window {window_id} during pump recovery"),
        )
        .await
        {
            log::warn!("session: failed to stop old capture during pump recovery: {error}");
        }
        log::info!("session: window {window_id} pump recovery capture.stop() end");

        let capture_fps = snapshot
            .published
            .lock_unpoisoned()
            .quality()
            .capture_fps()
            .min(snapshot.priority.lock_unpoisoned().capture_fps());
        let diagnostics = app
            .try_state::<crate::diagnostics::DiagnosticsState>()
            .map(|state| state.inner().clone());
        if let Some(diagnostics) = diagnostics.as_ref() {
            diagnostics.reset_capture_pipeline(window_id);
        }
        let StartedShareCapture {
            capture,
            latest_frame,
            latest_frame_notify,
            last_capture_wall_time_us,
            capture_error_rx,
            ..
        } = match start_capture_for_share(
            window_id,
            // #712 fix: pick the restart source that matches the share's OWN
            // tracked kind instead of unconditionally assuming a window. A
            // Display share's id was never a member of `content.windows()`,
            // so the old unconditional `DirectWindowId` here always failed
            // with `WindowNotFound` for a display -- a live, healthy display
            // share misread as "genuinely gone" and torn down below.
            ShareCaptureSource::direct_for_kind(snapshot.source_kind),
            capture_fps,
            snapshot.resolution,
            "capture_restart",
            diagnostics.clone(),
        )
        .await
        {
            Ok(capture) => capture,
            Err(error) => {
                log::error!(
                    "session: window {window_id} in-place capture restart after pump failure failed: {error}"
                );
                crate::analytics::capture_restarted(crate::analytics::RestartOutcome::Failed);
                if let Err(stop_error) = stop_share_explained(
                    &app,
                    state.inner(),
                    window_id,
                    StopShareAnalytics::CaptureFailed,
                )
                .await
                {
                    log::warn!(
                        "session: stop_share(window {window_id}) after failed pump recovery failed: {stop_error}"
                    );
                }
                crate::hover_tab::clear_share_state_for_window(&app, window_id);
                crate::remote_control::revoke_window(&app, window_id, "share pump restart failed");
                crate::hover_tab::emit_share_error(&app, window_id, true, error);
                return;
            }
        };
        let restarted_capture_config = capture.configuration_handle();
        restarted_capture_config.set_demand_long_edge(snapshot.demand_long_edge);
        let current_track = snapshot.published.lock_unpoisoned().clone();
        if let Err(error) = restarted_capture_config.update_stream_configuration(
            current_track.width(),
            current_track.height(),
            current_track.quality().capture_fps(),
            snapshot.resolution,
        ) {
            log::warn!(
                "session: window {window_id} could not restore receiver-demand capture size after restart: {error}"
            );
        }

        if !state.is_share_restart_generation_active(window_id, started_seq, restart_generation) {
            log::info!(
                "session: discarding stale rebuilt capture for window {window_id} seq {started_seq} restart_generation {restart_generation}"
            );
            if let Err(error) = stop_capture_with_timeout(
                Arc::new(capture),
                format!("session: stopping stale rebuilt capture for window {window_id}"),
            )
            .await
            {
                log::warn!("session: failed to stop stale rebuilt capture: {error}");
            }
            return;
        }

        let owner_pid = crate::window_registry::global()
            .map(|r| r.owner_pid_fresh(window_id))
            .unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(window_id));
        if owner_pid.is_none() {
            log::warn!("session: capture_restart(window {window_id}) could not resolve owner pid");
        }
        let next_restart_generation = snapshot.restart_generation.saturating_add(1);
        let SharePumpRuntime {
            pump_abort,
            monitor,
        } = spawn_share_pump(
            app.clone(),
            window_id,
            started_seq,
            next_restart_generation,
            snapshot.room_connection.clone(),
            snapshot.published.clone(),
            snapshot.republish_intent.clone(),
            capture.configuration_handle(),
            latest_frame,
            latest_frame_notify,
            last_capture_wall_time_us,
            capture_error_rx,
            diagnostics.clone(),
            snapshot.diagnostic_source,
            snapshot.interaction_signal.clone(),
            snapshot.priority.clone(),
            None,
        );

        let mut new_capture = Some(capture);
        let mut new_pump_abort = Some(pump_abort);
        let mut new_monitor = Some(monitor);
        // #915: only respawn the poller if one was actually running before
        // this restart (`had_url_refresh`, captured alongside the abort
        // above) -- that already encodes "this was a browser-window share
        // without a `source_title_override`," so it doesn't need
        // re-deriving here. Skip the lookup entirely when there's nothing
        // to respawn.
        //
        // Resolve the target off the async runtime: calling
        // `browser_extraction_target` (a `window_source::list()`
        // enumeration) inline on this task would be exactly the
        // blocking-on-async-task class #915 removes from the share-start
        // path, just relocated to the restart path.
        let (restart_bundle_id, restart_start_title) = if had_url_refresh {
            tokio::task::spawn_blocking(move || browser_extraction_target(window_id))
                .await
                .ok()
                .flatten()
                .map(|(bundle_id, title)| (Some(bundle_id), title))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        let mut new_url_refresh = spawn_share_url_refresh(
            snapshot.room_connection.clone(),
            state.share_metadata_apply_lock.clone(),
            window_id,
            started_seq,
            had_url_refresh,
            restart_bundle_id,
            restart_start_title,
        );
        let replaced = {
            let mut guard = state.inner.lock_unpoisoned();
            if let Some(current) = guard.shares.get(&window_id) {
                if current.started_seq != started_seq
                    || current.restart_generation != restart_generation
                {
                    false
                } else {
                    let frame = *current.frame.lock_unpoisoned();
                    let visible_on_screen = current.visible_on_screen.load(Ordering::Relaxed);
                    let known_closed = current.known_closed.load(Ordering::Relaxed);
                    let source_kind = current.source_kind;
                    let source_title = current.source_title.clone();
                    let border_color = current.border_color.clone();
                    let resolution = current.resolution;
                    // Preserve the per-share lock across a republish: re-seeding
                    // from the global default here would silently undo a
                    // mid-share denial on the next quality/wake restart.
                    let allow_remote_control =
                        current.allow_remote_control.load(Ordering::Relaxed);
                    let demand_resolution = *current.demand_resolution.lock_unpoisoned();
                    guard.shares.insert(
                        window_id,
                        ActiveShare {
                            allow_remote_control: AtomicBool::new(allow_remote_control),
                            capture: Arc::new(
                                new_capture.take().expect("new capture inserted once"),
                            ),
                            published: snapshot.published.clone(),
                            pump_abort: new_pump_abort
                                .take()
                                .expect("new pump abort handle inserted once"),
                            monitor: new_monitor.take().expect("new monitor inserted once"),
                            restart_generation: next_restart_generation,
                            pid: owner_pid,
                            started_seq,
                            frame: Mutex::new(frame),
                            visible_on_screen: AtomicBool::new(visible_on_screen),
                            known_closed: AtomicBool::new(known_closed),
                            source_kind,
                            source_title,
                            border_color,
                            priority: snapshot.priority.clone(),
                            interaction_signal: snapshot.interaction_signal.clone(),
                            resolution,
                            demand_resolution: Mutex::new(demand_resolution),
                            republish_intent: snapshot.republish_intent.clone(),
                            url_refresh: new_url_refresh.take(),
                        },
                    );
                    true
                }
            } else {
                false
            }
        };

        if !replaced {
            log::info!(
                "session: discarding stale pump recovery replacement for window {window_id} seq {started_seq} restart_generation {restart_generation}"
            );
            if let Some(monitor) = new_monitor {
                monitor.abort();
            }
            if let Some(pump_abort) = new_pump_abort {
                pump_abort.abort();
            }
            if let Some(url_refresh) = new_url_refresh.take() {
                url_refresh.stop();
            }
            if let Some(capture) = new_capture {
                if let Err(error) = stop_capture_with_timeout(
                    Arc::new(capture),
                    format!("session: stopping stale replacement capture for window {window_id}"),
                )
                .await
                {
                    log::warn!("session: failed to stop stale replacement capture: {error}");
                }
            }
            return;
        }

        log::info!(
            "session: window {window_id} capture restarted in place after pump failure (seq {started_seq}, restart_generation {next_restart_generation})"
        );
        crate::analytics::capture_restarted(crate::analytics::RestartOutcome::Recovered);
    });
}

fn spawn_capture_failure_cleanup(app: tauri::AppHandle, window_id: u32, message: String) {
    tokio::spawn(async move {
        use tauri::Manager;

        crate::hover_tab::clear_share_state_for_window(&app, window_id);
        crate::remote_control::revoke_window(&app, window_id, "capture stalled");

        if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
            let diagnostics: crate::diagnostics::DiagnosticsState = diagnostics.inner().clone();
            diagnostics.journal_append(
                &app,
                "error",
                format!("Shared window {window_id} stopped: {message}"),
            );
        }

        let error = ShareSessionError::Capture(message);
        crate::hover_tab::emit_share_error(&app, window_id, false, error.clone());

        let Some(state) = app.try_state::<SessionState>() else {
            log::warn!(
                "session: capture failure cleanup for window {window_id} could not find SessionState"
            );
            return;
        };
        if let Err(stop_error) = stop_share_explained(
            &app,
            state.inner(),
            window_id,
            StopShareAnalytics::CaptureFailed,
        )
        .await
        {
            log::error!(
                "session: stop_share(window {window_id}) after capture failure failed: {stop_error}"
            );
            crate::hover_tab::emit_share_error(&app, window_id, false, stop_error);
        }
    });
}

/// Stop sharing `window_id`: stops its `SCStream` capture, aborts its frame
/// pump, and unpublishes its LiveKit track. Does NOT touch the room
/// connection itself -- room lifecycle is now owned by `join_room`/
/// `leave_room` (SPEC.md §4.6's explicit meeting lifecycle), independent of
/// how many windows (including zero) are shared. If the stopped share was
/// the focused one, promotes the next most-recently-started remaining share
/// (if any) back to `Full` quality (see module doc comment on the focus
/// model).
pub async fn stop_share(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
) -> Result<(), ShareSessionError> {
    stop_share_explained(app, state, window_id, StopShareAnalytics::User).await
}

/// Why this teardown should (or should not) count as a `share_stopped`
/// product event. Leave-room and post-wake refresh must not look like the
/// user unshared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopShareAnalytics {
    User,
    WindowGone,
    CaptureFailed,
    Silent,
}

pub(crate) async fn stop_share_explained(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    analytics: StopShareAnalytics,
) -> Result<(), ShareSessionError> {
    let removed = stop_share_with_started_seq(app, state, window_id, None, None).await?;
    if removed {
        if crate::region_window::resolve(window_id).is_some() {
            crate::region_window::set_active_share(window_id, false);
        }
        match analytics {
            StopShareAnalytics::User => {
                crate::analytics::share_stopped(crate::analytics::ShareStoppedReason::User)
            }
            StopShareAnalytics::WindowGone => {
                crate::analytics::share_stopped(crate::analytics::ShareStoppedReason::WindowGone)
            }
            StopShareAnalytics::CaptureFailed => {
                crate::analytics::share_stopped(crate::analytics::ShareStoppedReason::CaptureFailed)
            }
            StopShareAnalytics::Silent => {}
        }
    }
    Ok(())
}

/// Stop a share only when it is still the same share generation that asked
/// for teardown. A reconnect-repair task can outlive a stop/re-share of the
/// same window id; without this compare-and-remove boundary, its terminal
/// error path could tear down the new share (#298).
/// #872: session authority decides overlay lifetime. Every inner teardown
/// exit passes through this single reconciliation point.
async fn stop_share_with_started_seq(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    expected_started_seq: Option<u64>,
    reconnect_guard: Option<&ReconnectRepairGuard>,
) -> Result<bool, ShareSessionError> {
    let result = stop_share_with_started_seq_inner(
        app,
        state,
        window_id,
        expected_started_seq,
        reconnect_guard,
    )
    .await;
    reconcile_share_overlay_from_authority(app, state, window_id);
    result
}

pub(crate) fn reconcile_share_overlay_from_authority(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
) {
    if state.is_share_active(window_id) {
        return;
    }
    crate::share_overlay::retire_overlays_for_window(app, window_id);
}

async fn stop_share_with_started_seq_inner(
    app: &tauri::AppHandle,
    state: &SessionState,
    window_id: u32,
    expected_started_seq: Option<u64>,
    reconnect_guard: Option<&ReconnectRepairGuard>,
) -> Result<bool, ShareSessionError> {
    log::info!("session: stop_share(window {window_id}) begin");
    // #804/#807: both recovery budgets are per share, not per window id -- a
    // fresh share of the same window starts with a clean slate.
    clear_layout_roi_ack_failures(window_id);
    clear_pump_recovery_failures(window_id);
    let (share, promote_id, room_connection_at_removal) = {
        let mut guard = state.inner.lock_unpoisoned();
        if reconnect_guard
            .is_some_and(|reconnect_guard| !reconnect_guard.is_current_with_inner(&guard))
        {
            log::info!("session: stop_share(window {window_id}) skipped stale reconnect lifecycle");
            return Ok(false);
        }
        if let Some(expected_started_seq) = expected_started_seq {
            if !guard
                .shares
                .get(&window_id)
                .is_some_and(|share| share.started_seq == expected_started_seq)
            {
                log::info!(
                    "session: stop_share(window {window_id}) skipped stale generation {expected_started_seq}"
                );
                return Ok(false);
            }
        }
        let was_focused = guard.focused_window() == Some(window_id);
        let room_connection_at_removal = guard
            .joined
            .as_ref()
            .map(|joined| joined.room_connection.clone());
        let share = guard.shares.remove(&window_id);
        // A stopped share must not carry its #841 limiter cooldown into the
        // next start of the same window.
        republish_reconcile_last_by_window()
            .lock_unpoisoned()
            .remove(&window_id);
        if let Some(share) = share.as_ref() {
            begin_republish_intent(&share.republish_intent);
            record_last_stopped_share_generation(
                &mut guard.last_stopped_share_seq,
                window_id,
                share.started_seq,
            );
        }
        guard
            .viewer_demands
            .retain(|key, _| key.window_id != window_id);
        guard
            .viewer_demand_sequences
            .retain(|key, _| key.window_id != window_id);
        let promote_id = if share.is_some() && was_focused {
            guard.focused_window() // recomputed post-removal
        } else {
            None
        };
        (share, promote_id, room_connection_at_removal)
    };
    let Some(share) = share else {
        // Idempotent no-op -- but LOG it (issue #13): the crash-evidence
        // anomaly ("stop_share begin with no preceding start_share begin")
        // was exactly a stop request for a window the session no longer
        // tracked, and this path used to return in total silence, so the
        // log couldn't distinguish "stopped a real share" from "UI state was
        // stale." See `leave_room`'s hover-tab-state note for the stale-UI
        // root cause.
        log::info!(
            "session: stop_share(window {window_id}) no-op -- not currently shared (idempotent; stale UI toggle?)"
        );
        return Ok(false);
    };
    unregister_interaction_signal(window_id, &share.interaction_signal);
    clear_interaction_burst_state(window_id);
    use tauri::Manager;
    if let Some(diagnostics) = app.try_state::<crate::diagnostics::DiagnosticsState>() {
        let diagnostics: crate::diagnostics::DiagnosticsState = diagnostics.inner().clone();
        diagnostics.clear_native_startup(window_id);
    }

    // Same "last toggled" bookkeeping as `start_share` (SPEC.md §4.2) --
    // only recorded once we know a real share actually existed and was
    // removed, not on the idempotent no-op path above.
    state.set_last_toggled_window(window_id);

    // Step-bracketing (issue #13): one line around EACH teardown step, so a
    // future crash's last logged line pinpoints the fatal step -- this
    // stretch used to be a ~28-line silent gap between "begin" and
    // "unpublish succeeded", which is exactly where a real SIGABRT landed.
    log::info!("session: stop_share(window {window_id}) aborting capture monitor");
    share.monitor.abort();
    log::info!("session: stop_share(window {window_id}) aborting frame pump");
    share.pump_abort.abort();
    if let Some(url_refresh) = &share.url_refresh {
        // #915: this is the teardown path `leave_room` also goes through
        // (via `stop_share_explained` for every window id), so it covers
        // both an explicit stop and a room leave.
        url_refresh.stop();
        log::debug!("share: url refresh aborted for window {window_id}");
    }
    log::info!("session: stop_share(window {window_id}) capture.stop() begin");
    if let Err(error) = stop_capture_with_timeout(
        share.capture,
        format!("session: stopping capture for window {window_id}"),
    )
    .await
    {
        log::warn!("session: error stopping capture for window {window_id}: {error}");
    }
    let removed_started_seq = share.started_seq;
    log::info!("session: stop_share(window {window_id}) capture.stop() end");
    // Hide local indicators at the capture lifecycle boundary, before the
    // potentially slow LiveKit unpublish and metadata cleanup (#420).
    let unpublish_result = unpublish_after_capture_boundary(
        || {
            crate::hover_tab::clear_share_state_for_window(app, window_id);
            log::info!("session: stop_share(window {window_id}) unpublish begin");
        },
        || {
            let published = share.published.lock_unpoisoned().clone();
            async move { published.unpublish().await }
        },
    )
    .await;
    // This async lock is shared with start_share's metadata publish. Acquiring
    // it is an async boundary, so revalidate under SessionInner below before
    // issuing the clear; a new share then publishes its title after us.
    let _metadata_apply_guard = state.share_metadata_apply_lock.lock().await;
    let clear_metadata = {
        let guard = state.inner.lock_unpoisoned();
        let reconnect_lifecycle_current = reconnect_guard.map_or(true, |reconnect_guard| {
            reconnect_guard.is_current_with_inner(&guard)
        });
        let original_room_is_current =
            room_connection_at_removal
                .as_ref()
                .is_some_and(|original_room| {
                    guard
                        .joined
                        .as_ref()
                        .is_some_and(|joined| Arc::ptr_eq(&joined.room_connection, original_room))
                });
        let current_share_started_seq = guard.shares.get(&window_id).map(|share| share.started_seq);
        stopped_share_metadata_cleanup_is_current(
            reconnect_lifecycle_current,
            original_room_is_current,
            current_share_started_seq,
        )
    };
    let cleared_metadata = if clear_metadata {
        let room_connection = room_connection_at_removal.expect("checked current original room");
        room_connection
            .clear_shared_window_title_for_generation(window_id, removed_started_seq)
            .await
    } else {
        false
    };
    if !cleared_metadata {
        log::info!(
            "session: stop_share(window {window_id}) skipped stale title cleanup for generation {removed_started_seq}"
        );
    }
    match &unpublish_result {
        Ok(()) => log::info!("session: stop_share(window {window_id}) unpublish succeeded"),
        Err(e) => log::error!("session: stop_share(window {window_id}) unpublish failed: {e}"),
    }

    if let Some(promote_id) = promote_id {
        log::info!(
            "session: stop_share(window {window_id}) promoting window {promote_id} back to Full quality"
        );
        apply_quality(state, promote_id, ShareQuality::Full).await;
    }

    log::info!("session: stop_share(window {window_id}) done");
    unpublish_result.map_err(ShareSessionError::from)?;
    Ok(true)
}

pub(crate) async fn restart_active_shares_after_wake(app: &tauri::AppHandle, state: &SessionState) {
    let shares = state.active_share_restart_plan();
    restart_active_shares_after_wake_with(
        shares,
        |window_id| state.is_share_active(window_id),
        |window_id| stop_share_explained(app, state, window_id, StopShareAnalytics::Silent),
        |window_id, frame, source_kind, resolution, publish_origin| {
            // #712: preserve the original source kind. Display ids live in
            // the same u32 slot as window ids but require the display lookup.
            start_share_with_capture_source(
                app,
                state,
                window_id,
                frame,
                ShareCaptureSource::direct_for_kind(source_kind),
                resolution,
                publish_origin,
            )
        },
        |window_id, frame, source_kind, color| {
            #[cfg(target_os = "macos")]
            {
                // #764: the restarted share's live frame, not the pre-sleep one.
                let frame = state.active_share_frame(window_id).unwrap_or(frame);
                crate::hover_tab::restore_share_border_after_restart(
                    app,
                    state,
                    source_kind,
                    window_id,
                    frame,
                    color,
                );
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (window_id, frame, source_kind, color);
            }
        },
        capture_restart_should_wait_for_wake,
        |window_id, error| {
            crate::hover_tab::clear_share_state_for_window(app, window_id);
            crate::remote_control::revoke_window(app, window_id, "post-wake share restart failed");
            crate::hover_tab::emit_share_error(app, window_id, true, error);
        },
    )
    .await;
}

type ActiveShareWakeRestart = (
    u32,
    crate::hover_tab::WindowFrame,
    u64,
    CaptureResolution,
    SharedSourceKind,
    String,
);

/// Bound the sleep-deferred portion of one proactive wake refresh. This is
/// deliberately the same cap and 250 ms polling shape as #734's in-place pump
/// recovery. The cap starts after `stop_share`, because a reconnect-stalled
/// unpublish must not consume the display's entire wake-stabilization budget.
const MAX_POST_WAKE_CAPTURE_SLEEP_WAIT: Duration = Duration::from_secs(120);
const POST_WAKE_CAPTURE_SLEEP_POLL_INTERVAL: Duration = Duration::from_millis(250);

async fn wait_for_post_wake_capture_restart(
    window_id: u32,
    wait_started: Instant,
    should_wait_for_wake: &impl Fn() -> bool,
) -> bool {
    let mut logged_wait = false;
    while should_wait_for_wake() {
        if wait_started.elapsed() > MAX_POST_WAKE_CAPTURE_SLEEP_WAIT {
            log::warn!(
                "session: post-wake share {window_id} still sleep-correlated after {MAX_POST_WAKE_CAPTURE_SLEEP_WAIT:?}; attempting capture restart anyway"
            );
            return false;
        }
        if !logged_wait {
            log::info!(
                "session: post-wake share {window_id} deferring capture restart until display/system wake"
            );
            logged_wait = true;
        }
        tokio::time::sleep(POST_WAKE_CAPTURE_SLEEP_POLL_INTERVAL).await;
    }
    true
}

/// The effect-injected form of [`restart_active_shares_after_wake`]. This is
/// the real async handler chain used in production: active check -> stop ->
/// stable-wake gate -> capture start -> sleep-correlated retry or terminal
/// teardown. Injection keeps that wiring headlessly testable without a real
/// ScreenCaptureKit source or LiveKit room.
async fn restart_active_shares_after_wake_with<
    IsShareActive,
    StopShare,
    StopShareFuture,
    StartShare,
    StartShareFuture,
    OnRestartSuccess,
    ShouldWaitForWake,
    TerminalFailure,
>(
    shares: Vec<ActiveShareWakeRestart>,
    mut is_share_active: IsShareActive,
    mut stop_share_effect: StopShare,
    mut start_share_effect: StartShare,
    mut on_restart_success: OnRestartSuccess,
    should_wait_for_wake: ShouldWaitForWake,
    mut on_terminal_failure: TerminalFailure,
) where
    IsShareActive: FnMut(u32) -> bool,
    StopShare: FnMut(u32) -> StopShareFuture,
    StopShareFuture: Future<Output = Result<(), ShareSessionError>>,
    StartShare: FnMut(
        u32,
        crate::hover_tab::WindowFrame,
        SharedSourceKind,
        CaptureResolution,
        SharePublishOrigin,
    ) -> StartShareFuture,
    StartShareFuture: Future<Output = Result<(), ShareSessionError>>,
    OnRestartSuccess: FnMut(u32, crate::hover_tab::WindowFrame, SharedSourceKind, &str),
    ShouldWaitForWake: Fn() -> bool,
    TerminalFailure: FnMut(u32, ShareSessionError),
{
    if shares.is_empty() {
        return;
    }

    log::info!(
        "session: refreshing {} active share capture(s) after system wake",
        shares.len()
    );

    for (window_id, frame, _started_seq, resolution, source_kind, border_color) in shares {
        if !is_share_active(window_id) {
            continue;
        }
        if let Err(error) = stop_share_effect(window_id).await {
            log::warn!(
                "session: post-wake stop_share({window_id}) returned an error before restart: {error}"
            );
        }

        // #749: stop_share can spend seconds blocked on unpublish while the
        // lid closes again. Recheck sleep state here, at the capture boundary,
        // rather than trusting the wake notification that launched this task.
        let sleep_wait_started = Instant::now();
        loop {
            wait_for_post_wake_capture_restart(
                window_id,
                sleep_wait_started,
                &should_wait_for_wake,
            )
            .await;

            match start_share_effect(
                window_id,
                frame,
                source_kind,
                resolution,
                SharePublishOrigin::PostWakeRestart,
            )
            .await
            {
                Ok(()) => {
                    on_restart_success(window_id, frame, source_kind, &border_color);
                    break;
                }
                Err(error) => {
                    let source_missing = matches!(
                        &error,
                        ShareSessionError::WindowNotFound(_)
                            | ShareSessionError::DisplayNotFound(_)
                    );
                    if source_missing
                        && should_wait_for_wake()
                        && sleep_wait_started.elapsed() <= MAX_POST_WAKE_CAPTURE_SLEEP_WAIT
                    {
                        log::warn!(
                            "session: post-wake restart of share {window_id} lost its source during a new sleep-correlated window; waiting to retry: {error}"
                        );
                        continue;
                    }

                    log::error!("session: post-wake restart of share {window_id} failed: {error}");
                    on_terminal_failure(window_id, error);
                    break;
                }
            }
        }
    }
}

pub(crate) async fn repair_active_share_publications_after_reconnect(
    app: &tauri::AppHandle,
    state: &SessionState,
    reconnect_guard: ReconnectRepairGuard,
) {
    let shares = state.active_share_publication_repair_plan();
    if shares.is_empty() {
        return;
    }

    log::info!(
        "session: reconciling {} active share publication(s) after reconnect",
        shares.len()
    );

    for (window_id, started_seq) in shares {
        if !state.reconnect_repair_guard_is_current(&reconnect_guard) {
            log::info!(
                "session: cancelling stale reconnect publication repair before window {window_id}"
            );
            return;
        }
        let Some(snapshot) = state.active_share_publication_repair_snapshot(
            window_id,
            started_seq,
            &reconnect_guard,
        ) else {
            log::info!(
                "session: reconnect publication repair skipped window {window_id}; share intent changed before repair"
            );
            continue;
        };
        let current = snapshot.published.lock_unpoisoned().clone();
        let quality = current.quality();
        let old_sid = current.sid().to_string();
        let expected_track_name = crate::transport::publisher::track_name_for_window(window_id);
        let local_publications: Vec<_> = snapshot
            .room_connection
            .room()
            .local_participant()
            .track_publications()
            .values()
            .map(|publication| (publication.sid().to_string(), publication.name()))
            .collect();
        let health = reconnect_publication_health(
            &old_sid,
            &expected_track_name,
            local_publications
                .iter()
                .map(|(sid, name)| (sid.as_str(), name.as_str())),
        );

        if !reconnect_publication_requires_repair(health) {
            log::info!(
                "session: window {window_id} reconnect publication repair outcome=healthy-current-sid sid={old_sid}; no replacement needed"
            );
            continue;
        }
        match health {
            ReconnectPublicationHealth::ReplacementAlreadyPresent => {
                log::warn!(
                    "session: window {window_id} reconnect publication repair outcome=unbound-sdk-replacement old_sid={old_sid}; attempting one generation-gated replacement because the tracked SID is absent"
                );
            }
            ReconnectPublicationHealth::Missing => {
                log::warn!(
                    "session: window {window_id} reconnect publication repair outcome=missing-local-publication old_sid={old_sid}; attempting one replacement"
                );
            }
            ReconnectPublicationHealth::CurrentSidPresent => unreachable!("handled above"),
        }

        if !state.reconnect_repair_guard_is_current(&reconnect_guard) {
            log::info!(
                "session: cancelling stale reconnect publication repair before intent for window {window_id}"
            );
            return;
        }
        let Some(intent_generation) = state.begin_reconnect_publication_repair_intent(
            window_id,
            started_seq,
            &reconnect_guard,
        ) else {
            log::info!(
                "session: window {window_id} reconnect publication repair skipped; share intent changed before replacement"
            );
            continue;
        };
        crate::resilience::emit_share_publication_repair_recovering(app, window_id);

        if !state.reconnect_repair_guard_is_current(&reconnect_guard) {
            log::info!(
                "session: cancelling stale reconnect publication repair before replacement publish for window {window_id}"
            );
            crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
            return;
        }
        let repair = republish_window_for_quality_forced(
            state,
            &reconnect_guard,
            started_seq,
            snapshot.room_connection,
            snapshot.published.clone(),
            snapshot.republish_intent,
            intent_generation,
            &snapshot.capture_config,
            window_id,
            quality,
            "reconnect publication repair",
        )
        .await;

        if !state.reconnect_repair_guard_is_current(&reconnect_guard)
            || !state.is_share_generation_active(window_id, started_seq)
        {
            log::info!(
                "session: cancelling stale reconnect publication repair after replacement for window {window_id}"
            );
            crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
            return;
        }
        if matches!(repair, RepublishOutcome::Cancelled) {
            log::info!(
                "session: cancelling stale reconnect publication repair transaction for window {window_id}"
            );
            crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
            return;
        }
        if repair.replaced() {
            let current = snapshot.published.lock_unpoisoned().clone();
            let fps = current
                .quality()
                .capture_fps()
                .min(snapshot.priority.lock_unpoisoned().capture_fps());
            if let Err(error) = snapshot.capture_config.update_fps(fps) {
                log::warn!(
                    "session: window {window_id} could not restore fps cap after reconnect publication repair: {error}"
                );
            }
            match repair {
                RepublishOutcome::Replaced => log::info!(
                    "session: window {window_id} reconnect publication repair outcome=replaced old_sid={old_sid}, new_sid={}",
                    current.sid()
                ),
                RepublishOutcome::ReplacedWithOldCleanupPending => log::warn!(
                    "session: window {window_id} reconnect publication repair outcome=replaced-old-cleanup-pending old_sid={old_sid}, new_sid={}; receiver dedupe will retain one window tile",
                    current.sid()
                ),
                RepublishOutcome::ReplacedWithOldCleanupDeferred => log::warn!(
                    "session: window {window_id} reconnect publication repair outcome=replaced-old-cleanup-deferred old_sid={old_sid}, new_sid={}; lifecycle changed after commit, so receiver dedupe retains one tile",
                    current.sid()
                ),
                RepublishOutcome::Cancelled | RepublishOutcome::Failed => {
                    unreachable!("handled before replaced outcome")
                }
            }
            crate::resilience::emit_share_publication_repair_restored(app, window_id);
        } else {
            let repair_failed_message =
                "reconnect could not restore this window's LiveKit publication; stopped sharing so you can share it again to retry".to_string();
            let error = ShareSessionError::RoomConnect(repair_failed_message.clone());
            log::warn!(
                "session: window {window_id} reconnect publication repair outcome=replacement-failed-terminal sid={old_sid}; clearing stale share state"
            );
            if !state.reconnect_repair_guard_is_current(&reconnect_guard) {
                log::info!(
                    "session: cancelling stale reconnect publication repair before terminal cleanup for window {window_id}"
                );
                crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
                return;
            }
            let stopped_original = match stop_share_with_started_seq(
                app,
                state,
                window_id,
                Some(started_seq),
                Some(&reconnect_guard),
            )
            .await
            {
                Ok(stopped) => stopped,
                // The share was removed before its obsolete LiveKit
                // publication reported an error. It is still safe to report
                // the terminal state unless a new generation appeared while
                // that cleanup awaited the SDK.
                Err(stop_error) => {
                    log::warn!(
                        "session: window {window_id} terminal reconnect cleanup could not unpublish retained track: {stop_error}"
                    );
                    true
                }
            };
            if !stopped_original {
                log::info!(
                    "session: window {window_id} reconnect repair terminal state skipped; a newer share generation is active"
                );
                crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
                continue;
            }
            if !state.reconnect_repair_guard_is_current(&reconnect_guard) {
                log::info!(
                    "session: cancelling stale reconnect publication repair before terminal effects for window {window_id}"
                );
                crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
                return;
            }
            if state.apply_terminal_reconnect_failure_if_current(
                &reconnect_guard,
                app,
                window_id,
                started_seq,
                error,
            ) {
                crate::resilience::emit_share_publication_repair_failed(
                    app,
                    window_id,
                    repair_failed_message,
                );
            } else {
                log::info!(
                    "session: window {window_id} reconnect repair terminal effects skipped; a newer share generation is active"
                );
                crate::resilience::emit_share_publication_repair_cancelled(app, window_id);
            }
        }
    }
}

/// Outcome of one #713 mic/camera reconnect publication repair attempt.
/// `pub(super)` so both the production callers (`session::mod`'s mic repair,
/// `crate::camera_session`'s camera repair) and this module's own tests can
/// read it -- the tests assert on it directly to prove the repair attempt
/// fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalTrackRepairOutcome {
    /// The tracked SID is still a live local publication; no repair needed.
    Healthy,
    /// Repair was needed but the reconnect lifecycle (room generation /
    /// repair epoch / still joined) went stale before or after the attempt.
    Skipped,
    /// The republish attempt succeeded.
    Replaced,
    /// The republish attempt failed; `on_terminal_failure` was invoked
    /// (unless the lifecycle also went stale in the same window, in which
    /// case the failure is logged but not surfaced -- a newer reconnect
    /// already superseded this one).
    Failed(String),
}

/// Shared core of #713's mic/camera reconnect publication repair --
/// generalizes `repair_active_share_publications_after_reconnect`'s
/// generation-guarded, ONE-bounded-replacement-publish pattern to the local
/// track kinds window shares don't cover. Mic/camera have no quality ladder
/// or capture-size to reconcile (unlike a window share), so repair here is
/// just: is the tracked SID still a live local publication? If not, one
/// republish attempt; if that also fails, one user-visible notice.
///
/// Three steps are injected rather than reaching into `SessionState`
/// directly: `guard_is_current` (checked before AND after the republish
/// attempt, matching the window-share repair's own re-check-after-await
/// discipline), the actual republish call, and the actual failure notice.
/// In production all three drive real `SessionState`/LiveKit `Room` state
/// (see `session::mod`'s `repair_mic_publication_after_reconnect` and
/// `crate::camera_session`'s `repair_camera_publication_after_reconnect`); a
/// test can drive this EXACT function -- the real handler chain: health
/// check -> generation recheck -> retry -> generation recheck -> notice --
/// with plain closures, no live LiveKit server needed, the same "inject the
/// effectful call, keep the real branching" shape `crate::camera_session`'s
/// `drive_camera_publish_attempts` already uses for its own self-heal loop's
/// testability (see that function's own doc comment).
pub(crate) async fn repair_local_track_publication_after_reconnect<Republish, RepublishFut>(
    label: &str,
    current_sid: &str,
    expected_track_name: &str,
    local_publications: &[(String, String)],
    guard_is_current: impl Fn() -> bool,
    republish: Republish,
    on_terminal_failure: impl FnOnce(String),
) -> LocalTrackRepairOutcome
where
    Republish: FnOnce() -> RepublishFut,
    RepublishFut: Future<Output = Result<String, String>>,
{
    let health = reconnect_publication_health(
        current_sid,
        expected_track_name,
        local_publications
            .iter()
            .map(|(sid, name)| (sid.as_str(), name.as_str())),
    );
    if !reconnect_publication_requires_repair(health) {
        log::info!(
            "session: {label} reconnect publication repair outcome=healthy-current-sid sid={current_sid}; no replacement needed"
        );
        return LocalTrackRepairOutcome::Healthy;
    }
    log::warn!(
        "session: {label} reconnect publication repair outcome={health:?} old_sid={current_sid}; attempting one replacement"
    );
    if !guard_is_current() {
        log::info!("session: cancelling stale {label} reconnect publication repair");
        return LocalTrackRepairOutcome::Skipped;
    }
    match republish().await {
        Ok(new_sid) => {
            log::info!(
                "session: {label} reconnect publication repair outcome=replaced old_sid={current_sid}, new_sid={new_sid}"
            );
            LocalTrackRepairOutcome::Replaced
        }
        Err(error) => {
            log::warn!(
                "session: {label} reconnect publication repair outcome=replacement-failed-terminal old_sid={current_sid}: {error}"
            );
            if guard_is_current() {
                on_terminal_failure(error.clone());
            } else {
                log::info!(
                    "session: {label} reconnect publication repair failure notice skipped; a newer reconnect generation is active"
                );
            }
            LocalTrackRepairOutcome::Failed(error)
        }
    }
}

fn quality_change_requires_republish(
    current_width: u32,
    current_height: u32,
    target_width: u32,
    target_height: u32,
) -> bool {
    current_width != target_width || current_height != target_height
}

/// Switch `window_id`'s published quality tier to `quality` if it isn't
/// already there. Frame-rate/bitrate changes use the live sender-parameter
/// API; a full republish remains reserved for a capture-size/layout change.
/// No-ops quietly (just logs) if the window is no longer shared or the live
/// update fails -- this runs as a side effect of another share's start/stop,
/// so a failure here shouldn't fail that outer call.
async fn apply_quality(state: &SessionState, window_id: u32, quality: ShareQuality) {
    let (
        room_connection,
        slot,
        capture_config,
        current_quality,
        target_quality,
        demand_long_edge,
        already_at_quality,
        priority,
        target_size_changed,
    ) = {
        let guard = state.inner.lock_unpoisoned();
        let requested_long_edge = viewer_demand_requested_long_edge(&guard, window_id);
        let Some(share) = guard.shares.get(&window_id) else {
            return;
        };
        let Some(room_connection) = guard.joined.as_ref().map(|j| j.room_connection.clone()) else {
            return;
        };
        let current = share.published.lock_unpoisoned().clone();
        let current_long_edge = current.width().max(current.height());
        let capture_config = share.capture.configuration_handle();
        capture_config.set_resolution_preference(share.resolution);
        let demand_long_edge = share.demand_resolution.lock_unpoisoned().reconcile(
            requested_long_edge,
            current_long_edge,
            Instant::now(),
        );
        capture_config.set_demand_long_edge(demand_long_edge);
        let (target_width, target_height, _) =
            capture_config.capture_size_for_resolution(share.resolution);
        let current_quality = current.quality();
        let mut target_quality = effective_share_quality(
            quality,
            crate::remote_control::window_has_active_controller(window_id)
                || has_passive_viewer_demand(&guard, window_id),
        );
        // Interaction-burst subscription floor (#290 step 5): while remote
        // typing/wheel/drag input has landed on this window within the last
        // `INTERACTION_BURST_ACTIVE_WINDOW_US`, never let cadence degrade
        // below `Full`, even if the request that triggered this
        // `apply_quality` call (e.g. `reconcile_quality_after_remote_control_
        // release`, which fires the instant control is released -- often
        // mid-burst) would otherwise demote it immediately. This is a live,
        // fresh check on every call, not a cached decision, so it corrects
        // itself the next time `apply_quality` runs after the burst has
        // genuinely lapsed. `DataSaver` is exempt: this floor must never
        // raise usage above that priority's explicit cap.
        let burst_active = interaction_burst_active_for_window(window_id, now_us());
        if let Some(floor) =
            interaction_burst_floor(*share.priority.lock_unpoisoned(), burst_active)
        {
            target_quality = floor.quality;
        }
        (
            room_connection,
            share.published.clone(),
            capture_config,
            current_quality,
            target_quality,
            demand_long_edge,
            current_quality == target_quality
                && current.width() == target_width
                && current.height() == target_height,
            share.priority.clone(),
            quality_change_requires_republish(
                current.width(),
                current.height(),
                target_width,
                target_height,
            ),
        )
    };

    log::info!(
        "session: window {window_id} quality decision requested={quality:?} target={target_quality:?} current={current_quality:?} receiver_demand_cap={demand_long_edge:?} already_at_target={already_at_quality}"
    );
    if already_at_quality {
        return;
    }

    if quality != target_quality {
        log::info!(
            "session: window {window_id} keeping Full quality despite requested {quality:?} because live viewer demand is active"
        );
    }

    if target_size_changed {
        republish_for_quality_reconcile(
            state,
            window_id,
            room_connection,
            slot,
            capture_config,
            priority,
            target_quality,
            "capture-size/layout change",
        )
        .await;
        return;
    }

    if current_quality != target_quality {
        let current = slot.lock_unpoisoned().clone();
        if let Err(error) = current.set_quality(target_quality).await {
            log::warn!(
                "session: window {window_id} live quality update to {target_quality:?} failed, falling back to republish: {error}"
            );
            republish_for_quality_reconcile(
                state,
                window_id,
                room_connection,
                slot,
                capture_config,
                priority,
                target_quality,
                "live update rejected (layer-shape mismatch or send failure)",
            )
            .await;
            return;
        }
        let fps = target_quality
            .capture_fps()
            .min(priority.lock_unpoisoned().capture_fps());
        if let Err(error) = capture_config.update_fps(fps) {
            log::warn!(
                "session: window {window_id} could not apply capture fps cap after live quality update: {error}"
            );
        }
    }
}

/// The #841 rate limiter's clock: last allowed reconcile republish per window.
fn republish_reconcile_last_by_window() -> &'static Mutex<HashMap<u32, Instant>> {
    static LAST: OnceLock<Mutex<HashMap<u32, Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pure decision for the #841 limiter: how long a reconcile republish for this
/// window must still wait given the previous one, or `None` when allowed now.
fn republish_reconcile_wait(last: Option<Instant>, now: Instant) -> Option<Duration> {
    let elapsed = now.saturating_duration_since(last?);
    (elapsed < REPUBLISH_RECONCILE_MIN_INTERVAL).then(|| REPUBLISH_RECONCILE_MIN_INTERVAL - elapsed)
}

/// Take this window's republish slot, or refuse and say so.
///
/// #841: both republish paths that a size disagreement can drive -- the
/// quality reconcile and the resize pump -- share ONE per-window clock, so no
/// future divergence between them can exceed ~1 republish/3s. The resize path
/// was unlimited and was the ~3/sec republisher in the incident log.
/// A refusal must leave the caller's resize debounce un-reset so it re-fires;
/// `ResizeDebounce::observe` keeps returning `StableResize` while the frame
/// size still differs, so a suppressed attempt is retried, not lost.
fn claim_republish_reconcile_slot(window_id: u32, reason: &str) -> bool {
    let now = Instant::now();
    let mut last_by_window = republish_reconcile_last_by_window().lock_unpoisoned();
    if let Some(wait) = republish_reconcile_wait(last_by_window.get(&window_id).copied(), now) {
        log::warn!(
            "session: window {window_id} suppressing republish for {}ms (rate limit, #841) -- reason={reason}; next demand packet or the resize pump re-evaluates",
            wait.as_millis()
        );
        return false;
    }
    last_by_window.insert(window_id, now);
    true
}

/// Shared republish-and-restore-fps-cap path used both when a genuine
/// capture-size/layout change forces a republish, and as the fallback when
/// `PublishedTrack::set_quality`'s live-update path refuses to apply (e.g.
/// the live sender's actual encoding shape doesn't cover the RIDs a
/// quality-only update targets).
async fn republish_for_quality_reconcile(
    state: &SessionState,
    window_id: u32,
    room_connection: Arc<RoomConnection>,
    slot: PublishedTrackSlot,
    capture_config: crate::capture::WindowCaptureConfig,
    priority: Arc<Mutex<SharePriority>>,
    target_quality: ShareQuality,
    reason: &'static str,
) {
    if !claim_republish_reconcile_slot(window_id, reason) {
        return;
    }
    let intent_generation = {
        let guard = state.inner.lock_unpoisoned();
        guard
            .shares
            .get(&window_id)
            .map(|share| begin_republish_intent(&share.republish_intent))
    };
    let Some(intent_generation) = intent_generation else {
        return;
    };
    let republish_intent = {
        let guard = state.inner.lock_unpoisoned();
        let Some(share) = guard.shares.get(&window_id) else {
            return;
        };
        share.republish_intent.clone()
    };
    let republished = republish_window_for_quality(
        room_connection,
        slot.clone(),
        republish_intent,
        intent_generation,
        &capture_config,
        window_id,
        target_quality,
        reason,
    )
    .await;
    if republished {
        let current = slot.lock_unpoisoned().clone();
        let fps = current
            .quality()
            .capture_fps()
            .min(priority.lock_unpoisoned().capture_fps());
        if let Err(error) = capture_config.update_fps(fps) {
            log::warn!(
                "session: window {window_id} could not restore preference fps cap after quality reconcile: {error}"
            );
        }
    }
}

pub async fn set_share_resolution(
    state: &SessionState,
    window_id: u32,
    resolution: CaptureResolution,
) -> Result<(), ShareSessionError> {
    let (room_connection, slot, republish_intent, intent_generation, capture_config, priority) = {
        let mut guard = state.inner.lock_unpoisoned();
        let Some(room_connection) = guard.joined.as_ref().map(|j| j.room_connection.clone()) else {
            return Err(ShareSessionError::NotInRoom);
        };
        let Some(share) = guard.shares.get_mut(&window_id) else {
            return Err(ShareSessionError::WindowNotFound(window_id));
        };
        share.resolution = resolution;
        let intent_generation = begin_republish_intent(&share.republish_intent);
        let capture_config = share.capture.configuration_handle();
        capture_config.set_resolution_preference(resolution);
        (
            room_connection,
            share.published.clone(),
            share.republish_intent.clone(),
            intent_generation,
            capture_config,
            share.priority.clone(),
        )
    };

    let ok = republish_window_for_resolution(
        room_connection,
        slot.clone(),
        republish_intent,
        intent_generation,
        &capture_config,
        window_id,
        resolution,
    )
    .await;
    if ok {
        let current = slot.lock_unpoisoned().clone();
        let fps = current
            .quality()
            .capture_fps()
            .min(priority.lock_unpoisoned().capture_fps());
        capture_config
            .update_fps(fps)
            .map_err(|error| ShareSessionError::Capture(error.to_string()))?;
        Ok(())
    } else {
        Err(ShareSessionError::Capture(format!(
            "failed to republish window {window_id} at {resolution:?} resolution"
        )))
    }
}

pub async fn set_share_priority(
    state: &SessionState,
    window_id: u32,
    priority: SharePriority,
) -> Result<(), ShareSessionError> {
    let (current_resolution, capture_config) = {
        let mut guard = state.inner.lock_unpoisoned();
        let Some(share) = guard.shares.get_mut(&window_id) else {
            return Err(ShareSessionError::WindowNotFound(window_id));
        };
        *share.priority.lock_unpoisoned() = priority;
        (share.resolution, share.capture.configuration_handle())
    };

    // #290 step 5c: if the priority changes WHILE an interaction burst is
    // already active for this window (e.g. the user picks a new share
    // priority mid-remote-control-typing), apply the burst's resolution
    // ceiling immediately rather than waiting for a later reconcile --
    // resolution is what gives here, never cadence. This call site is
    // user-triggered (not polled), so there is no risk of a burst-imposed
    // cap sticking around after the burst ends with nothing to revert it:
    // the next explicit priority/resolution change simply recomputes fresh.
    let burst_active = interaction_burst_active_for_window(window_id, now_us());
    let target_resolution =
        burst_effective_capture_resolution(priority, priority.capture_resolution(), burst_active);
    if current_resolution != target_resolution {
        set_share_resolution(state, window_id, target_resolution).await?;
    }

    {
        let guard = state.inner.lock_unpoisoned();
        if !guard.shares.contains_key(&window_id) {
            return Err(ShareSessionError::WindowNotFound(window_id));
        }
    }
    capture_config
        .update_fps(priority.capture_fps())
        .map_err(|error| ShareSessionError::Capture(error.to_string()))?;
    log::info!(
        "session: window {window_id} applied {priority:?} preference live at {}fps and {target_resolution:?}",
        priority.capture_fps()
    );
    Ok(())
}

pub async fn promote_quality_for_remote_control(state: &SessionState, window_id: u32) {
    apply_quality(state, window_id, ShareQuality::Full).await;
}

pub async fn reconcile_quality_after_remote_control_release(state: &SessionState, window_id: u32) {
    reconcile_quality_for_window(state, window_id).await;
}

pub async fn reconcile_quality_for_window(state: &SessionState, window_id: u32) {
    let target_quality = {
        let guard = state.inner.lock_unpoisoned();
        if !guard.shares.contains_key(&window_id) {
            return;
        }
        if guard.focused_window() == Some(window_id) {
            ShareQuality::Full
        } else {
            ShareQuality::Reduced
        }
    };
    apply_quality(state, window_id, target_quality).await;
}

async fn retry_post_wake_software_encoder_fallback(
    state: &SessionState,
    window_id: u32,
    started_seq: u64,
) {
    let Some((room_connection, slot, republish_intent, intent_generation, capture_config)) = ({
        let guard = state.inner.lock_unpoisoned();
        let share = guard
            .shares
            .get(&window_id)
            .filter(|share| share.started_seq == started_seq);
        share.and_then(|share| {
            let room_connection = guard
                .joined
                .as_ref()
                .map(|joined| joined.room_connection.clone())?;
            let capture_config = share.capture.configuration_handle();
            let intent_generation = begin_republish_intent(&share.republish_intent);
            Some((
                room_connection,
                share.published.clone(),
                share.republish_intent.clone(),
                intent_generation,
                capture_config,
            ))
        })
    }) else {
        log::info!(
            "session: skipping delayed post-wake encoder retry for stale share {window_id}/{started_seq}"
        );
        return;
    };

    let outcome = republish_window_with_target(
        room_connection,
        slot,
        republish_intent,
        intent_generation,
        &capture_config,
        window_id,
        |current, _capture_config, resolution| RepublishTarget {
            width: current.width(),
            height: current.height(),
            quality: current.quality(),
            resolution,
        },
        "post-wake software encoder fallback",
        true,
        1,
        None,
    )
    .await;
    log::info!(
        "session: window {window_id} one-shot post-wake encoder republish {}",
        if outcome.replaced() {
            "succeeded"
        } else {
            "did not replace the active track"
        }
    );
}

/// Repair a publication retired by a receiver's no-frame watchdog. The
/// receiver intentionally asks for the current dimensions/quality, so this
/// does not alter focus or viewer-demand policy; it only replaces the missing
/// publication. The coordinator generation coalesces simultaneous repair and
/// demand requests, latest request wins (#417).
pub async fn repair_active_share_publication(state: &SessionState, window_id: u32) {
    let (room_connection, slot, republish_intent, capture_config, resolution) = {
        let guard = state.inner.lock_unpoisoned();
        let Some(share) = guard.shares.get(&window_id) else {
            return;
        };
        let Some(room_connection) = guard
            .joined
            .as_ref()
            .map(|joined| joined.room_connection.clone())
        else {
            return;
        };
        let capture_config = share.capture.configuration_handle();
        (
            room_connection,
            share.published.clone(),
            share.republish_intent.clone(),
            capture_config.clone(),
            capture_config.resolution(),
        )
    };
    let generation = begin_republish_intent(&republish_intent);
    let repaired = republish_window_with_target(
        room_connection,
        slot,
        republish_intent,
        generation,
        &capture_config,
        window_id,
        |current, _capture_config, resolution| RepublishTarget {
            width: current.width(),
            height: current.height(),
            quality: current.quality(),
            resolution,
        },
        "receiver watchdog repair",
        true,
        REPUBLISH_RETRY_ATTEMPTS,
        None,
    )
    .await
    .replaced();
    log::info!(
        "session: window {window_id} receiver watchdog repair {} (resolution {resolution:?})",
        if repaired { "succeeded" } else { "failed" }
    );
}

pub fn note_passive_viewer_demand(state: &SessionState, update: ViewerDemandUpdate) -> bool {
    let key = ViewerDemandKey {
        window_id: update.window_id,
        viewer_id: update.viewer_id,
    };
    let mut guard = state.inner.lock_unpoisoned();
    if !guard.shares.contains_key(&update.window_id) {
        return false;
    }
    if guard
        .viewer_demand_sequences
        .get(&key)
        .is_some_and(|last_seq| update.seq < *last_seq)
    {
        log::debug!(
            "session: ignoring stale viewer-demand seq {} for window {}",
            update.seq,
            update.window_id
        );
        return false;
    }
    guard
        .viewer_demand_sequences
        .insert(key.clone(), update.seq);

    if update.event == ViewerDemandEvent::Closed || !update.visible {
        guard.viewer_demands.remove(&key);
        return true;
    }

    guard.viewer_demands.insert(
        key,
        PassiveViewerDemand {
            seq: update.seq,
            updated_at: update.received_at,
            width: update.width,
            height: update.height,
            scale: update.scale,
            pixel_width: update.pixel_width,
            pixel_height: update.pixel_height,
        },
    );
    true
}

pub fn expire_stale_viewer_demands(state: &SessionState, now: Instant) -> Vec<u32> {
    let mut guard = state.inner.lock_unpoisoned();
    let mut expired_windows = HashSet::new();
    guard.viewer_demands.retain(|key, demand| {
        let stale = now.duration_since(demand.updated_at) > VIEWER_DEMAND_STALE_AFTER;
        if stale {
            expired_windows.insert(key.window_id);
            log::info!(
                "session: passive viewer demand for window {} from '{}' expired after {:.1}s (last seq {}, logical {}x{} @ {:.2}x, pixels {}x{})",
                key.window_id,
                key.viewer_id,
                now.duration_since(demand.updated_at).as_secs_f64(),
                demand.seq,
                demand.width,
                demand.height,
                demand.scale,
                demand.pixel_width,
                demand.pixel_height
            );
        }
        !stale
    });
    expired_windows.into_iter().collect()
}

fn has_passive_viewer_demand(guard: &super::SessionInner, window_id: u32) -> bool {
    guard
        .viewer_demands
        .keys()
        .any(|key| key.window_id == window_id)
}

fn viewer_demand_requested_long_edge(guard: &super::SessionInner, window_id: u32) -> Option<u32> {
    max_viewer_demand_long_edge(
        guard
            .viewer_demands
            .iter()
            .filter(|(key, _)| key.window_id == window_id)
            .map(|(_, demand)| (demand.pixel_width, demand.pixel_height)),
    )
}

fn max_viewer_demand_long_edge(dimensions: impl IntoIterator<Item = (u32, u32)>) -> Option<u32> {
    dimensions
        .into_iter()
        .map(|(width, height)| width.max(height))
        .filter(|edge| *edge > 0)
        .max()
}

/// Reserved synthetic viewer-id for the local "startup grace" demand seeded
/// when a share is demoted the instant a newer share starts. It is NOT a real
/// remote viewer -- it is a self-expiring placeholder keyed like any other
/// passive demand, so it flows through the SAME `expire_stale_viewer_demands`
/// path after `VIEWER_DEMAND_STALE_AFTER`.
const STARTUP_GRACE_VIEWER_ID: &str = "__petal_startup_grace__";

/// Seed a self-expiring startup-grace demand for `window_id` so a just-demoted
/// share holds `Full` until an already-watching remote viewer's first
/// Open/Heartbeat arrives. Without it, a viewer who began watching window B
/// milliseconds before the sharer started window C -- but whose first demand
/// packet is still in flight (track-subscribe + first-frame + one RTT can trail
/// the new share by up to ~2s) -- would see B silently demote to `Reduced`
/// (4fps) until their next heartbeat (<=2s later) repromoted it. The grace
/// entry expires through `expire_stale_viewer_demands` like any real demand, so
/// a window nobody actually watches still correctly drops to `Reduced` after
/// the grace window; a real demand arriving meanwhile simply keeps it `Full`.
/// Caller must hold `state.inner`.
fn seed_startup_grace_demand(guard: &mut super::SessionInner, window_id: u32, now: Instant) {
    guard.viewer_demands.insert(
        ViewerDemandKey {
            window_id,
            viewer_id: STARTUP_GRACE_VIEWER_ID.to_string(),
        },
        PassiveViewerDemand {
            seq: 0,
            updated_at: now,
            width: 0,
            height: 0,
            scale: 1.0,
            pixel_width: 0,
            pixel_height: 0,
        },
    );
}

/// Manual resolution is an orthogonal pixel cap: it never pins a share to
/// `Full`. The automatic focus/live-demand policy here continues to own FPS
/// and bitrate, so a background 4K share still drops to `Reduced`.
fn effective_share_quality(requested: ShareQuality, has_live_demand: bool) -> ShareQuality {
    if has_live_demand {
        ShareQuality::Full
    } else {
        requested
    }
}

async fn publish_window_with_timeout(
    room_connection: &RoomConnection,
    window_id: u32,
    width: u32,
    height: u32,
    quality: ShareQuality,
    reason: &str,
) -> Option<Arc<crate::transport::publisher::PublishedTrack>> {
    match tokio::time::timeout(
        REPUBLISH_AWAIT_TIMEOUT,
        room_connection.publish_window_at(width, height, quality, Some(window_id)),
    )
    .await
    {
        Ok(Ok(track)) => Some(Arc::new(track)),
        Ok(Err(e)) => {
            log::warn!(
                "session: failed to republish window {window_id} ({reason}, {width}x{height}, {quality:?}): {e}"
            );
            None
        }
        Err(_) => {
            log::error!(
                "session: timed out after {:.1}s republishing window {window_id} ({reason}, {width}x{height}, {quality:?}); keeping previous track",
                REPUBLISH_AWAIT_TIMEOUT.as_secs_f64()
            );
            None
        }
    }
}

async fn unpublish_with_timeout(
    track: &crate::transport::publisher::PublishedTrack,
    window_id: u32,
    reason: &str,
) -> bool {
    match tokio::time::timeout(REPUBLISH_AWAIT_TIMEOUT, track.unpublish()).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            log::warn!(
                "session: error unpublishing superseded track for window {window_id} ({reason}): {e}"
            );
            false
        }
        Err(_) => {
            log::warn!(
                "session: timed out after {:.1}s unpublishing superseded track for window {window_id} ({reason}); leaving recovery track active",
                REPUBLISH_AWAIT_TIMEOUT.as_secs_f64()
            );
            false
        }
    }
}

/// A reconnect lifecycle can become stale after its replacement has already
/// committed. In that case the slot and capture belong to the replacement and
/// must remain untouched; only this captured obsolete track is reclaimed.
/// Retries are bounded and terminal failure is diagnostic-only, never a
/// teardown of a newer share generation (#298).
fn schedule_deferred_old_unpublish(
    obsolete_track: Arc<crate::transport::publisher::PublishedTrack>,
    window_id: u32,
) {
    tauri::async_runtime::spawn(async move {
        for attempt in 1..=DEFERRED_RECONNECT_OLD_UNPUBLISH_ATTEMPTS {
            if unpublish_with_timeout(
                obsolete_track.as_ref(),
                window_id,
                "deferred superseded old-track cleanup",
            )
            .await
            {
                log::info!(
                    "session: window {window_id} deferred superseded old-track cleanup succeeded on attempt {attempt}"
                );
                return;
            }
            if attempt < DEFERRED_RECONNECT_OLD_UNPUBLISH_ATTEMPTS {
                tokio::time::sleep(DEFERRED_RECONNECT_OLD_UNPUBLISH_RETRY_DELAY).await;
            }
        }
        log::error!(
            "session: window {window_id} deferred superseded old-track cleanup exhausted {DEFERRED_RECONNECT_OLD_UNPUBLISH_ATTEMPTS} attempts; replacement remains authoritative and receiver dedupe hides the obsolete publication"
        );
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RepublishTarget {
    width: u32,
    height: u32,
    quality: ShareQuality,
    resolution: CaptureResolution,
}

impl RepublishTarget {
    fn from_track(
        track: &crate::transport::publisher::PublishedTrack,
        resolution: CaptureResolution,
    ) -> Self {
        Self {
            width: track.width(),
            height: track.height(),
            quality: track.quality(),
            resolution,
        }
    }
}

async fn republish_window_with_target(
    room_connection: Arc<RoomConnection>,
    slot: Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>,
    republish_intent: RepublishIntent,
    intent_generation: u64,
    capture_config: &crate::capture::WindowCaptureConfig,
    window_id: u32,
    mut target_for_attempt: impl FnMut(
        &crate::transport::publisher::PublishedTrack,
        &crate::capture::WindowCaptureConfig,
        CaptureResolution,
    ) -> RepublishTarget,
    reason: &str,
    force_republish: bool,
    max_publish_attempts: usize,
    reconnect_guard: Option<&ReconnectRepublishGuard<'_>>,
) -> RepublishOutcome {
    // Capture the resolution once for this transaction. Reading the config's
    // mutex at each await boundary could mix the old and new preference and
    // report a successful commit as Failed when a concurrent request changes
    // it (#417).
    let transaction_resolution = capture_config.resolution();
    let _apply_guard = republish_intent.apply_lock.lock().await;
    if !republish_intent_is_current(&republish_intent, intent_generation) {
        log::info!(
            "session: window {window_id} skipping superseded republish intent {intent_generation} ({reason})"
        );
        return reconnect_republish_superseded_outcome(reconnect_guard);
    }
    let mut retry_delay = REPUBLISH_RETRY_INITIAL_DELAY;
    for attempt in 0..max_publish_attempts.max(1) {
        if reconnect_guard.is_some_and(|guard| !guard.is_current()) {
            log::info!(
                "session: window {window_id} cancelling stale reconnect republish before transaction ({reason})"
            );
            return RepublishOutcome::Cancelled;
        }
        if !republish_intent_is_current(&republish_intent, intent_generation) {
            return reconnect_republish_superseded_outcome(reconnect_guard);
        }
        let (observed, target) = {
            let current = slot.lock_unpoisoned().clone();
            let observed = RepublishTarget::from_track(current.as_ref(), transaction_resolution);
            let target =
                target_for_attempt(current.as_ref(), capture_config, transaction_resolution);
            if observed == target && !force_republish {
                if reconnect_guard.is_some_and(|guard| !guard.is_current()) {
                    return RepublishOutcome::Cancelled;
                }
                if !update_capture_after_republish(capture_config, window_id, target, reason) {
                    return RepublishOutcome::Failed;
                }
                if reconnect_guard.is_some_and(|guard| !guard.is_current()) {
                    return RepublishOutcome::Cancelled;
                }
                room_connection
                    .set_shared_window_capture_scale(window_id, capture_config.source_scale())
                    .await;
                if reconnect_guard.is_some_and(|guard| !guard.is_current())
                    || !republish_intent_is_current(&republish_intent, intent_generation)
                {
                    return reconnect_republish_superseded_outcome(reconnect_guard);
                }
                return RepublishOutcome::Replaced;
            }
            (observed, target)
        };

        let Some(new_published) = publish_window_with_timeout(
            &room_connection,
            window_id,
            target.width,
            target.height,
            target.quality,
            reason,
        )
        .await
        else {
            if attempt + 1 < max_publish_attempts.max(1) {
                log::warn!(
                    "session: window {window_id} republish attempt {}/{} failed ({reason}); retrying in {:.2}s",
                    attempt + 1,
                    max_publish_attempts.max(1),
                    retry_delay.as_secs_f64()
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(REPUBLISH_RETRY_MAX_DELAY);
                continue;
            }
            return RepublishOutcome::Failed;
        };

        if reconnect_guard.is_some_and(|guard| {
            reconnect_republish_must_cleanup_new_after_publish(guard.is_current())
        }) {
            unpublish_with_timeout(
                new_published.as_ref(),
                window_id,
                "cancelled reconnect republish",
            )
            .await;
            return RepublishOutcome::Cancelled;
        }
        if !republish_intent_is_current(&republish_intent, intent_generation) {
            unpublish_with_timeout(new_published.as_ref(), window_id, "superseded republish").await;
            return reconnect_republish_superseded_outcome(reconnect_guard);
        }

        let mut committed_target = false;
        let mut capture_update_failed = false;
        let track_to_unpublish = {
            let mut guard = slot.lock_unpoisoned();
            let current = RepublishTarget::from_track(guard.as_ref(), transaction_resolution);
            if reconnect_guard.is_some_and(|guard| !guard.is_current()) {
                new_published
            } else if !republish_intent_is_current(&republish_intent, intent_generation) {
                new_published
            } else {
                match republish_swap_decision(current, observed, target, force_republish) {
                    RepublishSwapDecision::AlreadyAtTarget => {
                        committed_target = true;
                        new_published
                    }
                    RepublishSwapDecision::SwapOldAfterNewPublished => {
                        if update_capture_after_republish(capture_config, window_id, target, reason)
                        {
                            committed_target = true;
                            std::mem::replace(&mut *guard, new_published)
                        } else {
                            capture_update_failed = true;
                            new_published
                        }
                    }
                    RepublishSwapDecision::DropNewAndRetry => new_published,
                }
            }
        };

        if !committed_target && reconnect_guard.is_some_and(|guard| !guard.is_current()) {
            // This path only owns `new_published` when cancellation happened
            // before the swap. Do not touch capture, slot, or old track.
            unpublish_with_timeout(
                track_to_unpublish.as_ref(),
                window_id,
                "cancelled reconnect republish",
            )
            .await;
            return RepublishOutcome::Cancelled;
        }

        if committed_target && reconnect_guard.is_some_and(|guard| !guard.is_current()) {
            // Slot/capture ownership is already committed. Do not pretend a
            // rollback occurred, and do not unpublish the old track from a
            // stale lifecycle; receiver dedupe covers the temporary overlap.
            schedule_deferred_old_unpublish(track_to_unpublish.clone(), window_id);
            return reconnect_republish_invalidation_outcome(true);
        }

        if committed_target {
            if reconnect_guard.is_some_and(|guard| !guard.is_current()) {
                schedule_deferred_old_unpublish(track_to_unpublish.clone(), window_id);
                return reconnect_republish_invalidation_outcome(true);
            }
            room_connection
                .set_shared_window_capture_scale(window_id, capture_config.source_scale())
                .await;
            if let Some(outcome) = reconnect_guard.and_then(|guard| {
                reconnect_republish_post_capture_scale_early_outcome(
                    guard.is_current(),
                    republish_intent_is_current(&republish_intent, intent_generation),
                )
            }) {
                schedule_deferred_old_unpublish(track_to_unpublish.clone(), window_id);
                return outcome;
            }
            if !republish_intent_is_current(&republish_intent, intent_generation) {
                schedule_deferred_old_unpublish(track_to_unpublish.clone(), window_id);
                return reconnect_republish_superseded_outcome(reconnect_guard);
            }
        }

        if reconnect_guard.is_some_and(|guard| !guard.is_current()) {
            if committed_target {
                schedule_deferred_old_unpublish(track_to_unpublish.clone(), window_id);
            }
            return reconnect_republish_invalidation_outcome(committed_target);
        }
        let old_cleanup_succeeded =
            unpublish_with_timeout(track_to_unpublish.as_ref(), window_id, reason).await;
        if committed_republish_needs_deferred_old_cleanup(committed_target, old_cleanup_succeeded) {
            schedule_deferred_old_unpublish(track_to_unpublish.clone(), window_id);
        }
        if let Some(outcome) = reconnect_guard.and_then(|guard| {
            reconnect_republish_post_old_cleanup_early_outcome(
                committed_target,
                guard.is_current(),
                republish_intent_is_current(&republish_intent, intent_generation),
            )
        }) {
            return outcome;
        }
        if !republish_intent_is_current(&republish_intent, intent_generation) {
            return reconnect_republish_superseded_outcome(reconnect_guard);
        }
        if capture_update_failed {
            return RepublishOutcome::Failed;
        }

        let current = slot.lock_unpoisoned().clone();
        let current = RepublishTarget::from_track(current.as_ref(), transaction_resolution);
        if committed_target && current == target {
            log::info!(
                "session: window {window_id} republish complete ({width}x{height}, quality {quality:?}, resolution {resolution:?}, {reason})",
                width = target.width,
                height = target.height,
                quality = target.quality,
                resolution = target.resolution
            );
            crate::logging::note_republish_complete(window_id);
            return if old_cleanup_succeeded {
                RepublishOutcome::Replaced
            } else {
                RepublishOutcome::ReplacedWithOldCleanupPending
            };
        }
        if attempt + 1 < max_publish_attempts.max(1) {
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(REPUBLISH_RETRY_MAX_DELAY);
        }
    }

    log::warn!(
        "session: window {window_id} republish lost concurrent target races for {reason}; will retry later"
    );
    RepublishOutcome::Failed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepublishSwapDecision {
    AlreadyAtTarget,
    SwapOldAfterNewPublished,
    DropNewAndRetry,
}

fn republish_swap_decision(
    current: RepublishTarget,
    observed: RepublishTarget,
    target: RepublishTarget,
    force_republish: bool,
) -> RepublishSwapDecision {
    if current == target && (!force_republish || current != observed) {
        RepublishSwapDecision::AlreadyAtTarget
    } else if current == observed {
        RepublishSwapDecision::SwapOldAfterNewPublished
    } else {
        RepublishSwapDecision::DropNewAndRetry
    }
}

fn update_capture_after_republish(
    capture_config: &crate::capture::WindowCaptureConfig,
    window_id: u32,
    target: RepublishTarget,
    reason: &str,
) -> bool {
    let target_fps = target.quality.capture_fps();
    match capture_config.update_stream_configuration(
        target.width,
        target.height,
        target_fps,
        target.resolution,
    ) {
        Ok(()) => true,
        Err(e) => {
            log::warn!(
                "session: failed to update capture configuration for window {window_id} to {}x{} at {target_fps}fps ({:?}) after republish for {reason}; keeping previous published track: {e}",
                target.width,
                target.height,
                target.resolution
            );
            false
        }
    }
}

async fn republish_window_for_quality(
    room_connection: Arc<RoomConnection>,
    slot: Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>,
    republish_intent: RepublishIntent,
    intent_generation: u64,
    capture_config: &crate::capture::WindowCaptureConfig,
    window_id: u32,
    quality: ShareQuality,
    reason: &str,
) -> bool {
    republish_window_with_target(
        room_connection,
        slot,
        republish_intent,
        intent_generation,
        capture_config,
        window_id,
        |_current, capture_config, resolution| {
            let (width, height, _) = capture_config.capture_size_for_resolution(resolution);
            RepublishTarget {
                width,
                height,
                quality,
                resolution,
            }
        },
        reason,
        false,
        REPUBLISH_RETRY_ATTEMPTS,
        None,
    )
    .await
    .replaced()
}

async fn republish_window_for_quality_forced(
    state: &SessionState,
    reconnect_guard: &ReconnectRepairGuard,
    started_seq: u64,
    room_connection: Arc<RoomConnection>,
    slot: Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>,
    republish_intent: RepublishIntent,
    intent_generation: u64,
    capture_config: &crate::capture::WindowCaptureConfig,
    window_id: u32,
    quality: ShareQuality,
    reason: &str,
) -> RepublishOutcome {
    let transaction_guard = ReconnectRepublishGuard {
        state,
        lifecycle: reconnect_guard,
        window_id,
        started_seq,
    };
    republish_window_with_target(
        room_connection,
        slot,
        republish_intent,
        intent_generation,
        capture_config,
        window_id,
        |_current, capture_config, resolution| {
            let (width, height, _) = capture_config.capture_size_for_resolution(resolution);
            RepublishTarget {
                width,
                height,
                quality,
                resolution,
            }
        },
        reason,
        true,
        REPUBLISH_RETRY_ATTEMPTS,
        Some(&transaction_guard),
    )
    .await
}

async fn republish_window_for_resize(
    room_connection: Arc<RoomConnection>,
    slot: Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>,
    republish_intent: RepublishIntent,
    intent_generation: u64,
    capture_config: &crate::capture::WindowCaptureConfig,
    window_id: u32,
) -> bool {
    // #841: this path used to re-cap the CAPTURED FRAME SIZE as if it were
    // the source backing size, bypassing `capture_size_for_resolution` and
    // the ROI memo -- a third writer of the stream size, and the ~3/sec
    // republisher in the incident log. It must ask the same authority the
    // quality path asks, and obey the same per-window rate limit.
    //
    // The rate-limit claim is the CALLER's (the pump's `StableResize` arm,
    // #869) and must not be repeated here. Claiming twice is fatal, not
    // merely redundant: the caller's claim stamps `Instant::now()`, nothing
    // awaits in between, so a second claim reads ~0ms elapsed against the 3s
    // interval, refuses, and returns false WITHOUT republishing -- killing
    // the resize republish entirely on every stable resize. Two lanes each
    // added the guard to their own half of this path and each lane's test
    // pinned its own half, so both passed.
    republish_window_with_target(
        room_connection,
        slot,
        republish_intent,
        intent_generation,
        capture_config,
        window_id,
        |current, capture_config, resolution| {
            let (width, height, _) = capture_config.capture_size_for_resolution(resolution);
            RepublishTarget {
                width,
                height,
                quality: current.quality(),
                resolution,
            }
        },
        "resize",
        false,
        REPUBLISH_RETRY_ATTEMPTS,
        None,
    )
    .await
    .replaced()
}

async fn republish_window_for_resolution(
    room_connection: Arc<RoomConnection>,
    slot: Arc<Mutex<Arc<crate::transport::publisher::PublishedTrack>>>,
    republish_intent: RepublishIntent,
    intent_generation: u64,
    capture_config: &crate::capture::WindowCaptureConfig,
    window_id: u32,
    resolution: CaptureResolution,
) -> bool {
    capture_config.set_resolution_preference(resolution);
    republish_window_with_target(
        room_connection,
        slot,
        republish_intent,
        intent_generation,
        capture_config,
        window_id,
        |current, capture_config, resolution| {
            let (width, height, _) = capture_config.capture_size_for_resolution(resolution);
            RepublishTarget {
                width,
                height,
                quality: current.quality(),
                resolution,
            }
        },
        "resolution",
        false,
        REPUBLISH_RETRY_ATTEMPTS,
        None,
    )
    .await
    .replaced()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_title_prefers_live_and_falls_back_to_start() {
        assert_eq!(
            effective_title(Some("Live Tab — Chrome".to_string()), Some("Start Tab — Chrome")),
            Some("Live Tab — Chrome".to_string()),
            "a live title must win so the freshness-skip check can see a real change"
        );
        assert_eq!(
            effective_title(None, Some("Start Tab — Chrome")),
            Some("Start Tab — Chrome".to_string()),
            "no live title (e.g. a transient CGWindowID miss) must fall back to the start title"
        );
    }

    #[test]
    fn the_per_share_lock_fails_closed_for_a_window_we_do_not_share() {
        // Opposite default from the metadata decoder, on purpose: that one is
        // a display hint and fails OPEN so pre-key sharers keep their button;
        // THIS one is the authorization and must fail CLOSED. No live share
        // means there is nothing to authorize.
        let state = SessionState::default();
        assert!(!state.share_allows_remote_control(4242));
        // And there is nothing to flip, so the setter reports no such share
        // rather than inventing permission for a window we do not own.
        assert_eq!(state.set_share_allows_remote_control(4242, true), None);
        assert!(!state.share_allows_remote_control(4242));
    }

    use std::collections::{BTreeMap, BTreeSet, HashMap};

    #[test]
    fn every_stop_share_exit_reconciles_overlay_from_session_authority() {
        // Source-level by design: the guarantee is that every async exit goes
        // through one wrapper point, a control-flow property headless AppKit
        // unit tests cannot observe (#872).
        // Scope to production code: this test's own body mentions the symbol
        // several times, and counting the whole file counts the assertions
        // themselves -- a source-scanning guard that matches its own text
        // reports a number that has nothing to do with the wiring (#872).
        let full = include_str!("share.rs");
        let source = full
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(prod, _)| prod)
            .expect("share.rs must keep its test module in one #[cfg(test)] block");
        assert_eq!(
            source.matches("stop_share_with_started_seq_inner(").count(),
            2,
            "the inner teardown must have one definition and exactly one caller"
        );
        let wrapper = source
            .split_once("async fn stop_share_with_started_seq(")
            .and_then(|(_, rest)| rest.split_once("async fn stop_share_with_started_seq_inner("))
            .map(|(body, _)| body)
            .expect("single-exit wrapper must directly precede the inner teardown");
        let inner_call = wrapper
            .find("let result = stop_share_with_started_seq_inner(")
            .expect("wrapper must await the inner teardown into result");
        let reconcile = wrapper
            .find("\n    reconcile_share_overlay_from_authority(app, state, window_id);")
            .expect("wrapper must reconcile unconditionally at function scope");
        let final_result = wrapper
            .find("\n    result\n")
            .expect("wrapper must return the saved inner result");
        assert!(inner_call < reconcile && reconcile < final_result);
        assert!(!wrapper[inner_call..reconcile].contains('?'));

        let inner = source
            .split_once("async fn stop_share_with_started_seq_inner(")
            .and_then(|(_, rest)| {
                rest.split_once("pub(crate) async fn restart_active_shares_after_wake(")
            })
            .map(|(body, _)| body)
            .expect("inner teardown must precede wake restart");
        assert!(!inner.contains("reconcile_share_overlay_from_authority"));
    }

    #[test]
    fn every_non_hover_share_end_path_has_registry_backed_retirement() {
        let room = include_str!("room.rs");
        let leave_cleanup = room
            .split_once("async fn cleanup_left_room(")
            .map(|(_, body)| body)
            .expect("leave-room cleanup must exist");
        let hover_clear = leave_cleanup
            .find("crate::hover_tab::clear_share_state_on_leave(app);")
            .expect("leave-room must clear hover-tab state");
        let retire_all = leave_cleanup
            .find("crate::share_overlay::retire_all_overlays(app);")
            .expect("leave-room must retire overlays from their own registry");
        assert!(hover_clear < retire_all);

        let hover = include_str!("../hover_tab.rs");
        let clear_on_leave = hover
            .split_once("pub fn clear_share_state_on_leave(")
            .and_then(|(_, rest)| rest.split_once("pub fn clear_share_state_for_window("))
            .map(|(body, _)| body)
            .expect("hover leave clear must precede per-window clear");
        assert!(clear_on_leave.contains("crate::share_overlay::retire_all_overlays(app);"));
        let clear_for_window = hover
            .split_once("pub fn clear_share_state_for_window(")
            .and_then(|(_, rest)| rest.split_once("pub async fn toggle_window_share("))
            .map(|(body, _)| body)
            .expect("per-window clear must precede share toggle");
        assert!(clear_for_window
            .contains("crate::share_overlay::retire_overlays_for_window(app, window_id);"));
    }

    // #622: the release-smoke liveness marker must be impossible to satisfy
    // with static re-pushes; only affirmatively-changed pushes count, and the
    // marker fires exactly once, at the threshold.
    #[test]
    fn moving_frame_liveness_ignores_static_push_decisions() {
        let mut liveness = MovingFrameLiveness::default();
        for _ in 0..(MOVING_FRAME_LIVENESS_THRESHOLD * 10) {
            for decision in [
                "idle_static_refresh",
                "push_refresh_floor",
                "push_first_frame",
                "bypass_static_pacer_remote_control",
                "push_dirty_rect_skip_disabled",
                "push_non_normal_status",
                "push_size_changed",
                "push_tier_changed",
                "push_dirty_rects_unknown",
                "skip_dirty_rect_clean",
                "none",
            ] {
                assert_eq!(
                    liveness.observe(decision),
                    None,
                    "decision {decision} must never confirm liveness"
                );
            }
        }
        assert_eq!(liveness.moving_pushes, 0);
    }

    #[test]
    fn moving_frame_liveness_fires_once_at_threshold_on_motion_evidence() {
        for decision in [
            "push_dirty_rect",
            "push_dirty_rect_after_skip",
            "pull_snapshot",
        ] {
            let mut liveness = MovingFrameLiveness::default();
            for i in 1..MOVING_FRAME_LIVENESS_THRESHOLD {
                assert_eq!(
                    liveness.observe(decision),
                    None,
                    "below threshold ({i}) must not fire"
                );
            }
            assert_eq!(
                liveness.observe(decision),
                Some(MOVING_FRAME_LIVENESS_THRESHOLD),
                "threshold push must fire the marker"
            );
            assert_eq!(
                liveness.observe(decision),
                None,
                "marker must fire exactly once"
            );
        }
    }

    #[test]
    fn moving_frame_liveness_threshold_survives_interleaved_static_pushes() {
        let mut liveness = MovingFrameLiveness::default();
        let mut fired = None;
        for _ in 0..MOVING_FRAME_LIVENESS_THRESHOLD {
            assert_eq!(liveness.observe("idle_static_refresh"), None);
            if let Some(count) = liveness.observe("push_dirty_rect") {
                fired = Some(count);
            }
        }
        assert_eq!(fired, Some(MOVING_FRAME_LIVENESS_THRESHOLD));
    }

    #[test]
    fn capture_boundary_precedes_a_pending_or_failed_unpublish_tail() {
        tauri::async_runtime::block_on(async {
            let local_indicator_is_unshared = Arc::new(AtomicBool::new(false));
            let indicator_for_boundary = local_indicator_is_unshared.clone();
            let (unpublish_entered_tx, unpublish_entered_rx) = tokio::sync::oneshot::channel();
            let (finish_unpublish_tx, finish_unpublish_rx) =
                tokio::sync::oneshot::channel::<Result<(), &'static str>>();

            let stop = unpublish_after_capture_boundary(
                move || indicator_for_boundary.store(true, Ordering::SeqCst),
                || async move {
                    unpublish_entered_tx
                        .send(())
                        .expect("test observes the pending unpublish tail");
                    finish_unpublish_rx
                        .await
                        .expect("test releases the pending unpublish tail")
                },
            );
            let observe_pending_tail = async {
                // Receiving this signal means the unpublish future is held at
                // its network tail. The local transition must already have
                // happened.
                tokio::time::timeout(Duration::from_secs(1), unpublish_entered_rx)
                    .await
                    .expect("unpublish tail reaches its pending boundary before timeout")
                    .expect("unpublish tail starts after the capture boundary");
                assert!(
                    local_indicator_is_unshared.load(Ordering::SeqCst),
                    "a slow unpublish must not delay the local unshared indicator"
                );
                finish_unpublish_tx
                    .send(Err("injected unpublish failure"))
                    .expect("pending unpublish tail receives injected failure");
            };
            let bounded_stop = tokio::time::timeout(Duration::from_secs(1), stop);
            let (unpublish_result, ()) = tokio::join!(bounded_stop, observe_pending_tail);

            assert_eq!(
                unpublish_result.expect("stop completes after injected unpublish failure"),
                Err("injected unpublish failure")
            );
            assert!(
                local_indicator_is_unshared.load(Ordering::SeqCst),
                "a failed unpublish must not roll the local indicator back to sharing"
            );
        });
    }

    /// #764: drive the real post-wake handler chain and verify that a
    /// successful display-share restart restores its border exactly once with
    /// the per-share color and source kind carried by the restart plan.
    #[tokio::test]
    async fn post_wake_refresh_success_restores_display_border_once() {
        let window_id = crate::window_source::display_source_id(1);
        let frame = crate::hover_tab::WindowFrame {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let border_color = "#0f9d58";
        let share_active = Arc::new(AtomicBool::new(true));
        let border_restores = Arc::new(Mutex::new(Vec::<(
            u32,
            crate::hover_tab::WindowFrame,
            SharedSourceKind,
            String,
        )>::new()));

        let active_for_check = share_active.clone();
        let active_for_stop = share_active.clone();
        let active_for_start = share_active.clone();
        let border_restores_in = border_restores.clone();

        restart_active_shares_after_wake_with(
            vec![(
                window_id,
                frame,
                1,
                CaptureResolution::Auto,
                SharedSourceKind::Display,
                border_color.to_string(),
            )],
            move |_window_id| active_for_check.load(Ordering::SeqCst),
            move |_window_id| {
                active_for_stop.store(false, Ordering::SeqCst);
                async { Ok(()) }
            },
            move |_window_id, _frame, source_kind, _resolution, publish_origin| {
                active_for_start.store(true, Ordering::SeqCst);
                async move {
                    assert_eq!(source_kind, SharedSourceKind::Display);
                    assert_eq!(publish_origin, SharePublishOrigin::PostWakeRestart);
                    Ok(())
                }
            },
            move |restored_window_id, restored_frame, source_kind, color| {
                border_restores_in.lock_unpoisoned().push((
                    restored_window_id,
                    restored_frame,
                    source_kind,
                    color.to_string(),
                ));
            },
            || false,
            |_window_id, _error| panic!("successful restart must not tear the share down"),
        )
        .await;

        assert_eq!(
            border_restores.lock_unpoisoned().as_slice(),
            [(
                window_id,
                frame,
                SharedSourceKind::Display,
                border_color.to_string(),
            )],
            "successful display restart must restore one border with the original plan values"
        );
    }

    /// #749: drive the real post-wake handler chain with a fake capture start.
    /// The first start represents ScreenCaptureKit losing a display because a
    /// second lid-close lands while that call is in flight. The sleep flag is
    /// reasserted before `DisplayNotFound` returns, so the handler must wait for
    /// the next stable wake and retry without firing terminal teardown effects.
    #[tokio::test]
    async fn post_wake_refresh_retries_source_loss_when_sleep_resumes_mid_start() {
        let window_id = crate::window_source::display_source_id(1);
        let frame = crate::hover_tab::WindowFrame {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let share_active = Arc::new(AtomicBool::new(true));
        let sleep_correlated = Arc::new(AtomicBool::new(false));
        let stop_calls = Arc::new(AtomicU64::new(0));
        let start_calls = Arc::new(AtomicU64::new(0));
        let terminal_teardowns = Arc::new(AtomicU64::new(0));

        let active_for_check = share_active.clone();
        let active_for_stop = share_active.clone();
        let stop_calls_in = stop_calls.clone();
        let active_for_start = share_active.clone();
        let sleep_for_start = sleep_correlated.clone();
        let start_calls_in = start_calls.clone();
        let sleep_for_gate = sleep_correlated.clone();
        let terminal_teardowns_in = terminal_teardowns.clone();

        restart_active_shares_after_wake_with(
            vec![(
                window_id,
                frame,
                1,
                CaptureResolution::Auto,
                SharedSourceKind::Display,
                "#0f9d58".to_string(),
            )],
            move |_window_id| active_for_check.load(Ordering::SeqCst),
            move |_window_id| {
                stop_calls_in.fetch_add(1, Ordering::SeqCst);
                active_for_stop.store(false, Ordering::SeqCst);
                async { Ok(()) }
            },
            move |_window_id, _frame, source_kind, _resolution, publish_origin| {
                let attempt = start_calls_in.fetch_add(1, Ordering::SeqCst);
                let active = active_for_start.clone();
                let sleep = sleep_for_start.clone();
                async move {
                    assert_eq!(source_kind, SharedSourceKind::Display);
                    assert_eq!(publish_origin, SharePublishOrigin::PostWakeRestart);
                    if attempt == 0 {
                        // Reasserted *inside* the capture-start effect, matching
                        // a lid close during the real SCK start call.
                        sleep.store(true, Ordering::SeqCst);
                        let sleep_for_wake = sleep.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            sleep_for_wake.store(false, Ordering::SeqCst);
                        });
                        Err(ShareSessionError::DisplayNotFound(1))
                    } else {
                        active.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                }
            },
            |_window_id, _frame, _source_kind, _color| {},
            move || sleep_for_gate.load(Ordering::SeqCst),
            move |_window_id, _error| {
                terminal_teardowns_in.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(start_calls.load(Ordering::SeqCst), 2);
        assert!(
            share_active.load(Ordering::SeqCst),
            "the successful retry must restore the active share"
        );
        assert_eq!(
            terminal_teardowns.load(Ordering::SeqCst),
            0,
            "sleep-correlated source loss must not clear UI state, revoke control, or emit a fatal error"
        );
    }

    /// #749's opposite direction: the same real handler must preserve the
    /// existing fail-closed behavior when a source is genuinely gone and no
    /// sleep-correlated window is active.
    #[tokio::test]
    async fn post_wake_refresh_genuine_source_loss_still_tears_share_down() {
        let window_id = 42;
        let frame = crate::hover_tab::WindowFrame {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        let share_active = Arc::new(AtomicBool::new(true));
        let start_calls = Arc::new(AtomicU64::new(0));
        let border_restores = Arc::new(Mutex::new(Vec::<u32>::new()));
        let terminal_errors = Arc::new(Mutex::new(Vec::<String>::new()));

        let active_for_check = share_active.clone();
        let active_for_stop = share_active.clone();
        let start_calls_in = start_calls.clone();
        let border_restores_in = border_restores.clone();
        let terminal_errors_in = terminal_errors.clone();

        restart_active_shares_after_wake_with(
            vec![(
                window_id,
                frame,
                1,
                CaptureResolution::Auto,
                SharedSourceKind::Window,
                "#0f9d58".to_string(),
            )],
            move |_window_id| active_for_check.load(Ordering::SeqCst),
            move |_window_id| {
                active_for_stop.store(false, Ordering::SeqCst);
                async { Ok(()) }
            },
            move |_window_id, _frame, source_kind, _resolution, publish_origin| {
                start_calls_in.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert_eq!(source_kind, SharedSourceKind::Window);
                    assert_eq!(publish_origin, SharePublishOrigin::PostWakeRestart);
                    Err(ShareSessionError::WindowNotFound(window_id))
                }
            },
            move |restored_window_id, _frame, _source_kind, _color| {
                border_restores_in
                    .lock_unpoisoned()
                    .push(restored_window_id);
            },
            || false,
            move |failed_window_id, error| {
                terminal_errors_in
                    .lock_unpoisoned()
                    .push(format!("{failed_window_id}: {error}"));
            },
        )
        .await;

        assert_eq!(start_calls.load(Ordering::SeqCst), 1);
        assert!(
            border_restores.lock_unpoisoned().is_empty(),
            "a terminal restart failure must not restore the border"
        );
        assert!(
            !share_active.load(Ordering::SeqCst),
            "a genuinely missing source must remain stopped"
        );
        assert_eq!(
            terminal_errors.lock_unpoisoned().as_slice(),
            ["42: window 42 not found (closed, or invalid id)"],
            "genuine loss must run the fatal clear/revoke/error branch exactly once"
        );
    }

    #[test]
    fn quality_flip_is_live_when_the_simulcast_layout_is_unchanged() {
        assert!(!quality_change_requires_republish(1920, 1080, 1920, 1080));
        assert!(quality_change_requires_republish(1920, 1080, 1280, 720));
        assert!(quality_change_requires_republish(1920, 1080, 1080, 1920));
    }

    /// #841 containment: a reconcile republish is rate limited per window, so
    /// the ~3/s storm that killed a live display share cannot recur even while
    /// the underlying capture-size disagreement is still unfixed.
    #[test]
    fn reconcile_republish_is_rate_limited_per_window() {
        let start = Instant::now();
        assert_eq!(
            republish_reconcile_wait(None, start),
            None,
            "the first reconcile republish for a window is always allowed"
        );
        let wait = republish_reconcile_wait(Some(start), start + Duration::from_millis(300))
            .expect("a republish 300ms after the previous one must be suppressed");
        assert_eq!(
            wait,
            REPUBLISH_RECONCILE_MIN_INTERVAL - Duration::from_millis(300)
        );
        assert_eq!(
            republish_reconcile_wait(Some(start), start + REPUBLISH_RECONCILE_MIN_INTERVAL),
            None,
            "once the interval has elapsed the next republish is allowed"
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestShare {
        started_seq: u64,
        restart_generation: u64,
        quality: ShareQuality,
        width: u32,
        height: u32,
        resolution: CaptureResolution,
    }

    #[derive(Debug)]
    struct DeferredTrackCleanupModel {
        replacement_slot_is_active: bool,
        obsolete_publication_is_live: bool,
        cleanup_attempts: usize,
    }

    impl DeferredTrackCleanupModel {
        fn reclaim_obsolete_publication(&mut self) {
            self.cleanup_attempts += 1;
            self.obsolete_publication_is_live = false;
        }
    }

    #[derive(Debug, Default)]
    struct SessionModel {
        joined: bool,
        shares: BTreeMap<u32, TestShare>,
        remote_control_windows: BTreeSet<u32>,
        passive_demand_windows: BTreeSet<u32>,
        next_share_seq: u64,
        last_toggled_window: Option<u32>,
        quality_updates: Vec<(u32, ShareQuality)>,
        republish_updates: Vec<(u32, RepublishTarget)>,
        terminal_reconnect_effect_windows: Vec<u32>,
        last_stopped_share_seq: BTreeMap<u32, u64>,
    }

    impl SessionModel {
        fn join(&mut self) {
            self.joined = true;
        }

        fn leave(&mut self) {
            self.joined = false;
            self.shares.clear();
        }

        fn focused_window(&self) -> Option<u32> {
            focused_window_of(
                self.shares
                    .iter()
                    .map(|(id, share)| (*id, share.started_seq)),
            )
        }

        fn start_share(&mut self, window_id: u32) -> Result<(), ShareSessionError> {
            self.start_share_with_resolution(window_id, CaptureResolution::Auto)
        }

        fn start_share_with_resolution(
            &mut self,
            window_id: u32,
            resolution: CaptureResolution,
        ) -> Result<(), ShareSessionError> {
            if self.shares.contains_key(&window_id) {
                return Ok(());
            }
            if self.shares.len() >= MAX_CONCURRENT_SHARES {
                return Err(ShareSessionError::TooManyShares(MAX_CONCURRENT_SHARES));
            }
            if !self.joined {
                return Err(ShareSessionError::NotInRoom);
            }

            let previously_focused = self.focused_window();
            let seq = self.next_share_seq;
            self.next_share_seq += 1;
            self.shares.insert(
                window_id,
                TestShare {
                    started_seq: seq,
                    restart_generation: 0,
                    quality: ShareQuality::Full,
                    width: 1920,
                    height: 1080,
                    resolution,
                },
            );
            self.last_toggled_window = Some(window_id);

            if let Some(demote_id) = previously_focused.filter(|id| *id != window_id) {
                self.set_quality(demote_id, ShareQuality::Reduced);
            }

            Ok(())
        }

        fn stop_share(&mut self, window_id: u32) {
            let was_focused = self.focused_window() == Some(window_id);
            let Some(share) = self.shares.remove(&window_id) else {
                return;
            };
            self.last_stopped_share_seq
                .insert(window_id, share.started_seq);
            self.last_toggled_window = Some(window_id);
            if was_focused {
                if let Some(promote_id) = self.focused_window() {
                    self.set_quality(promote_id, ShareQuality::Full);
                }
            }
        }

        /// Test-model equivalent of reconnect repair's compare-and-remove
        /// terminal cleanup. A new share of the same window id must win over
        /// a stale repair task that captured the old sequence.
        fn stop_share_if_started_seq(&mut self, window_id: u32, expected_started_seq: u64) -> bool {
            if !self
                .shares
                .get(&window_id)
                .is_some_and(|share| share.started_seq == expected_started_seq)
            {
                return false;
            }
            self.stop_share(window_id);
            true
        }

        fn apply_reconnect_terminal_failure(
            &mut self,
            lifecycle_current: bool,
            window_id: u32,
            expected_started_seq: u64,
        ) -> bool {
            if !lifecycle_current
                || !self.stop_share_if_started_seq(window_id, expected_started_seq)
            {
                return false;
            }
            if !terminal_reconnect_effects_are_current(
                self.shares.contains_key(&window_id),
                self.last_stopped_share_seq.get(&window_id).copied(),
                expected_started_seq,
            ) {
                return false;
            }
            self.terminal_reconnect_effect_windows.push(window_id);
            true
        }

        /// Model the real post-stop terminal-effects boundary separately from
        /// the remove step. A stale repair can arrive after a newer generation
        /// was stopped, so `shares.is_empty()` alone is not authority (#298).
        fn apply_terminal_effects_if_generation_current(
            &mut self,
            window_id: u32,
            expected_started_seq: u64,
        ) -> bool {
            if !terminal_reconnect_effects_are_current(
                self.shares.contains_key(&window_id),
                self.last_stopped_share_seq.get(&window_id).copied(),
                expected_started_seq,
            ) {
                return false;
            }
            self.terminal_reconnect_effect_windows.push(window_id);
            true
        }

        fn apply_reconnect_republish_after_publish(
            &mut self,
            lifecycle_current: bool,
            window_id: u32,
            target: RepublishTarget,
        ) -> bool {
            if reconnect_republish_must_cleanup_new_after_publish(lifecycle_current) {
                return false;
            }
            self.republish_updates.push((window_id, target));
            true
        }

        fn restart_capture_in_place(
            &mut self,
            window_id: u32,
            started_seq: u64,
            restart_generation: u64,
        ) -> bool {
            let Some(share) = self.shares.get_mut(&window_id) else {
                return false;
            };
            if share.started_seq != started_seq || share.restart_generation != restart_generation {
                return false;
            }
            share.restart_generation = share.restart_generation.saturating_add(1);
            true
        }

        fn restart_after_wake(&mut self, window_id: u32) -> Result<(), ShareSessionError> {
            let Some(resolution) = self.shares.get(&window_id).map(|share| share.resolution) else {
                return Ok(());
            };
            self.stop_share(window_id);
            self.start_share_with_resolution(window_id, resolution)
        }

        fn set_quality(&mut self, window_id: u32, quality: ShareQuality) {
            let quality = effective_share_quality(quality, self.has_live_demand(window_id));
            if let Some(share) = self.shares.get_mut(&window_id) {
                if share.quality == quality {
                    return;
                }
                share.quality = quality;
                self.quality_updates.push((window_id, quality));
            }
        }

        fn set_resolution(
            &mut self,
            window_id: u32,
            resolution: CaptureResolution,
            width: u32,
            height: u32,
        ) {
            let Some(share) = self.shares.get_mut(&window_id) else {
                return;
            };
            share.resolution = resolution;
            let target = RepublishTarget {
                width,
                height,
                quality: share.quality,
                resolution,
            };
            let current = RepublishTarget {
                width: share.width,
                height: share.height,
                quality: share.quality,
                resolution: share.resolution,
            };
            if current == target {
                return;
            }
            share.width = width;
            share.height = height;
            self.republish_updates.push((window_id, target));
        }

        fn has_live_demand(&self, window_id: u32) -> bool {
            self.remote_control_windows.contains(&window_id)
                || self.passive_demand_windows.contains(&window_id)
        }

        fn reconcile_quality(&mut self, window_id: u32) {
            if !self.shares.contains_key(&window_id) {
                return;
            }
            let target = if self.focused_window() == Some(window_id) {
                ShareQuality::Full
            } else {
                ShareQuality::Reduced
            };
            self.set_quality(window_id, target);
        }
    }

    /// SPEC.md §4.3: "Concurrent-share cap: 4 windows per user." This
    /// exercises the exact cap-check predicate `start_share` uses
    /// (`guard.shares.len() >= MAX_CONCURRENT_SHARES`) against a simulated
    /// sequence of 5 share attempts, without needing a real
    /// ScreenCaptureKit/LiveKit round trip -- `start_share` itself can't run
    /// in a unit test (it needs real capture + a real room connection), so
    /// this asserts the cap logic in isolation: a caller with 4 active
    /// shares gets refused on the 5th, and the refusal doesn't silently
    /// evict any of the 4.
    #[test]
    fn fifth_concurrent_share_is_refused_not_evicted() {
        let mut active: std::collections::HashSet<u32> = std::collections::HashSet::new();

        fn try_start(
            active: &mut std::collections::HashSet<u32>,
            window_id: u32,
        ) -> Result<(), ShareSessionError> {
            if active.contains(&window_id) {
                return Ok(());
            }
            if active.len() >= MAX_CONCURRENT_SHARES {
                return Err(ShareSessionError::TooManyShares(MAX_CONCURRENT_SHARES));
            }
            active.insert(window_id);
            Ok(())
        }

        for window_id in 1..=4 {
            assert!(
                try_start(&mut active, window_id).is_ok(),
                "share {window_id} should be accepted"
            );
        }
        assert_eq!(active.len(), 4);

        let fifth = try_start(&mut active, 5);
        assert!(matches!(fifth, Err(ShareSessionError::TooManyShares(4))));
        // Refusing the 5th must not have evicted any of the original 4.
        assert_eq!(active, std::collections::HashSet::from([1, 2, 3, 4]));
    }

    #[test]
    fn max_concurrent_shares_is_four_per_spec() {
        assert_eq!(MAX_CONCURRENT_SHARES, 4);
    }

    /// Focus model (module doc comment): "most recently toggled-on share is
    /// focused" == highest `started_seq`.
    #[test]
    fn focus_follows_most_recently_started_share() {
        // Started in order 10 (seq 0), 20 (seq 1), 30 (seq 2) -- 30 is most
        // recent, so it's focused.
        let shares = vec![(10u32, 0u64), (20, 1), (30, 2)];
        assert_eq!(focused_window_of(shares.into_iter()), Some(30));
    }

    #[test]
    fn focus_promotes_next_most_recent_after_focused_share_stops() {
        // 10, 20, 30 started in that order; 30 (most recent) stops --
        // 20 becomes focused (next most recent of what remains).
        let remaining_after_30_stops = vec![(10u32, 0u64), (20, 1)];
        assert_eq!(
            focused_window_of(remaining_after_30_stops.into_iter()),
            Some(20)
        );
    }

    #[test]
    fn focus_is_none_when_nothing_is_shared() {
        assert_eq!(focused_window_of(std::iter::empty()), None);
    }

    #[test]
    fn session_model_requires_join_before_starting_share() {
        let mut model = SessionModel::default();

        let result = model.start_share(10);

        assert!(matches!(result, Err(ShareSessionError::NotInRoom)));
        assert!(model.shares.is_empty());
        assert_eq!(model.last_toggled_window, None);
    }

    #[test]
    fn session_model_starting_new_share_demotes_previous_focus() {
        let mut model = SessionModel::default();
        model.join();

        model.start_share(10).unwrap();
        model.start_share(20).unwrap();

        assert_eq!(model.focused_window(), Some(20));
        assert_eq!(model.shares[&10].quality, ShareQuality::Reduced);
        assert_eq!(model.shares[&20].quality, ShareQuality::Full);
        assert_eq!(model.quality_updates, vec![(10, ShareQuality::Reduced)]);
        assert_eq!(model.last_toggled_window, Some(20));
    }

    #[test]
    fn session_model_passive_demand_keeps_demoted_share_full_quality() {
        let mut model = SessionModel::default();
        model.join();

        model.start_share(10).unwrap();
        model.passive_demand_windows.insert(10);
        model.start_share(20).unwrap();

        assert_eq!(model.focused_window(), Some(20));
        assert_eq!(model.shares[&10].quality, ShareQuality::Full);
        assert_eq!(model.shares[&20].quality, ShareQuality::Full);
        assert!(model.quality_updates.is_empty());
    }

    #[test]
    fn session_model_release_restores_nonfocused_controlled_share_to_reduced() {
        let mut model = SessionModel::default();
        model.join();

        model.start_share(10).unwrap();
        model.start_share(20).unwrap();
        assert_eq!(model.shares[&10].quality, ShareQuality::Reduced);

        model.remote_control_windows.insert(10);
        model.set_quality(10, ShareQuality::Reduced);
        assert_eq!(model.shares[&10].quality, ShareQuality::Full);

        model.remote_control_windows.remove(&10);
        model.set_quality(10, ShareQuality::Reduced);
        assert_eq!(model.shares[&10].quality, ShareQuality::Reduced);
    }

    #[test]
    fn session_model_stale_passive_demand_demotes_nonfocused_share() {
        let mut model = SessionModel::default();
        model.join();

        model.start_share(10).unwrap();
        model.start_share(20).unwrap();
        model.passive_demand_windows.insert(10);
        model.reconcile_quality(10);
        assert_eq!(model.shares[&10].quality, ShareQuality::Full);

        model.passive_demand_windows.remove(&10);
        model.reconcile_quality(10);

        assert_eq!(model.shares[&10].quality, ShareQuality::Reduced);
        assert_eq!(
            model.quality_updates.last(),
            Some(&(10, ShareQuality::Reduced))
        );
    }

    #[test]
    fn session_model_remote_control_and_passive_demand_are_independent() {
        let mut model = SessionModel::default();
        model.join();

        model.start_share(10).unwrap();
        model.start_share(20).unwrap();
        model.remote_control_windows.insert(10);
        model.passive_demand_windows.insert(10);
        model.reconcile_quality(10);
        assert_eq!(model.shares[&10].quality, ShareQuality::Full);

        model.remote_control_windows.remove(&10);
        model.reconcile_quality(10);
        assert_eq!(
            model.shares[&10].quality,
            ShareQuality::Full,
            "passive viewing demand should keep Full after active RC stops"
        );

        model.passive_demand_windows.remove(&10);
        model.reconcile_quality(10);
        assert_eq!(model.shares[&10].quality, ShareQuality::Reduced);
    }

    /// Live investigation (2026-07-08, "Till's remote window often freezes on
    /// Bob's side"): a second, independent contributor to the reported stall
    /// (beyond the snapshot-pull hash blind spot) is the demote-at-second-share
    /// race. When a sharer starts a newer share, `start_share` demotes the
    /// previously-focused window to `Reduced` immediately, keeping `Full` only
    /// if a passive viewer demand is ALREADY present. A viewer who just began
    /// watching the older window -- but whose first Open/Heartbeat is still in
    /// flight -- would see it drop to 4fps until their next heartbeat (<=2s).
    /// `seed_startup_grace_demand` closes that window by seeding a self-expiring
    /// local demand that keeps the just-demoted share `Full` through the SAME
    /// `expire_stale_viewer_demands` machinery, then lets it drop correctly if
    /// no real viewer ever signals. This pins both halves.
    #[test]
    fn startup_grace_demand_holds_full_then_self_expires() {
        let state = SessionState::default();
        let t0 = Instant::now();

        // Seed the grace demand exactly as `start_share` does when it demotes a
        // just-superseded focus while a watcher's first demand is still in flight.
        {
            let mut guard = state.inner.lock_unpoisoned();
            seed_startup_grace_demand(&mut guard, 42, t0);
            assert!(
                has_passive_viewer_demand(&guard, 42),
                "seeded startup grace must register as demand so apply_quality keeps Full"
            );
        }

        // Well within the grace window: not expired, still demanded.
        assert!(
            expire_stale_viewer_demands(&state, t0 + Duration::from_secs(1)).is_empty(),
            "startup grace must not expire before VIEWER_DEMAND_STALE_AFTER"
        );
        {
            let guard = state.inner.lock_unpoisoned();
            assert!(has_passive_viewer_demand(&guard, 42));
        }

        // Past the grace window with no real demand refreshing it: expires
        // through the same path real remote demands use, and the window is
        // returned so the caller reconciles it (dropping it to Reduced).
        let expired = expire_stale_viewer_demands(
            &state,
            t0 + VIEWER_DEMAND_STALE_AFTER + Duration::from_secs(1),
        );
        assert_eq!(expired, vec![42]);
        {
            let guard = state.inner.lock_unpoisoned();
            assert!(
                !has_passive_viewer_demand(&guard, 42),
                "after grace expiry with no real demand, the window is no longer demanded"
            );
        }
    }

    #[test]
    fn effective_share_quality_promotes_remote_control_demand() {
        assert_eq!(
            effective_share_quality(ShareQuality::Reduced, true),
            ShareQuality::Full
        );
        assert_eq!(
            effective_share_quality(ShareQuality::Reduced, false),
            ShareQuality::Reduced
        );
        assert_eq!(
            effective_share_quality(ShareQuality::Full, true),
            ShareQuality::Full
        );
    }

    #[test]
    fn viewer_demand_rung_selects_smallest_covering_resolution() {
        assert_eq!(viewer_demand_resolution_rung(None), None);
        assert_eq!(viewer_demand_resolution_rung(Some(720)), Some(960));
        assert_eq!(viewer_demand_resolution_rung(Some(1920)), Some(1920));
        assert_eq!(viewer_demand_resolution_rung(Some(1921)), Some(2560));
        assert_eq!(viewer_demand_resolution_rung(Some(9000)), Some(4096));
    }

    #[test]
    fn viewer_demand_aggregation_uses_largest_visible_receiver_pixels() {
        assert_eq!(
            max_viewer_demand_long_edge([(800, 600), (2560, 1440), (1280, 720)]),
            Some(2560)
        );
        assert_eq!(max_viewer_demand_long_edge([(0, 0)]), None);
    }

    #[test]
    fn newer_republish_intent_supersedes_older_generation() {
        let coordinator = Arc::new(RepublishCoordinator::default());
        let older = begin_republish_intent(&coordinator);
        assert!(republish_intent_is_current(&coordinator, older));

        let newer = begin_republish_intent(&coordinator);
        assert!(!republish_intent_is_current(&coordinator, older));
        assert!(republish_intent_is_current(&coordinator, newer));
    }

    /// #417's residual: `newer_republish_intent_supersedes_older_generation`
    /// above proves the generation counter supersedes, but it is sequential --
    /// it never shows that a BURST of concurrent republish requests actually
    /// coalesces down to one applied republish. That is the churn #417 is
    /// about. This drives the real `RepublishCoordinator` (both fields: the
    /// `generation` counter AND the `apply_lock`) with 8 racing requesters.
    ///
    /// The second half is the positive control: identical harness, one
    /// coordinator per requester, so nothing can coalesce and all 8 must
    /// apply. Without it, "1 applied" could just mean the counter never ran.
    #[tokio::test]
    async fn concurrent_republish_burst_coalesces_to_a_single_applied_republish() {
        const REQUESTERS: usize = 8;

        async fn run_burst(coordinators: Vec<RepublishIntent>) -> usize {
            let applied = Arc::new(AtomicU64::new(0));
            // Every requester bumps its intent before ANY of them takes the
            // apply lock -- that is the burst shape, not a staggered queue.
            let generations: Vec<(RepublishIntent, u64)> = coordinators
                .into_iter()
                .map(|c| {
                    let g = begin_republish_intent(&c);
                    (c, g)
                })
                .collect();

            let mut tasks = Vec::new();
            for (coordinator, generation) in generations {
                let applied = applied.clone();
                tasks.push(tokio::spawn(async move {
                    let _guard = coordinator.apply_lock.lock().await;
                    if republish_intent_is_current(&coordinator, generation) {
                        applied.fetch_add(1, Ordering::SeqCst);
                    }
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
            applied.load(Ordering::SeqCst) as usize
        }

        let shared = Arc::new(RepublishCoordinator::default());
        let coalesced = run_burst(vec![shared; REQUESTERS]).await;
        assert_eq!(
            coalesced, 1,
            "a burst of {REQUESTERS} concurrent republish requests must coalesce \
             to exactly one applied republish"
        );

        // Positive control: no shared generation, so no coalescing is possible.
        let independent: Vec<RepublishIntent> = (0..REQUESTERS)
            .map(|_| Arc::new(RepublishCoordinator::default()))
            .collect();
        let uncoalesced = run_burst(independent).await;
        assert_eq!(
            uncoalesced, REQUESTERS,
            "positive control failed: with one coordinator per requester every \
             request must apply, proving the counter above is live"
        );
    }

    #[test]
    fn viewer_demand_resolution_raises_after_short_hold_and_lowers_after_long_hold() {
        let t0 = Instant::now();
        let mut state = ViewerDemandResolutionState {
            applied_long_edge: Some(1280),
            ..Default::default()
        };

        // A raise is no longer instant: it must be sustained for the upsize
        // hold. One packet starts the clock, an uncontradicted repeat past the
        // hold commits it.
        assert_eq!(state.reconcile(Some(3000), 1280, t0), Some(1280));
        assert_eq!(
            state.reconcile(
                Some(3000),
                1280,
                t0 + VIEWER_DEMAND_UPSIZE_HOLD - Duration::from_millis(1)
            ),
            Some(1280)
        );
        assert_eq!(
            state.reconcile(Some(3000), 1280, t0 + VIEWER_DEMAND_UPSIZE_HOLD),
            Some(3840)
        );

        // Lowering still takes the longer downsize hold.
        let t1 = t0 + VIEWER_DEMAND_UPSIZE_HOLD;
        assert_eq!(state.reconcile(Some(1000), 3840, t1), Some(3840));
        assert_eq!(
            state.reconcile(
                Some(1000),
                3840,
                t1 + VIEWER_DEMAND_DOWNSIZE_HOLD - Duration::from_millis(1)
            ),
            Some(3840)
        );
        assert_eq!(
            state.reconcile(Some(1000), 3840, t1 + VIEWER_DEMAND_DOWNSIZE_HOLD),
            Some(1280)
        );
    }

    /// The structural kill of the republish loop: one spurious raise packet
    /// between contradicting heartbeats must NEVER commit, no matter when it
    /// arrives.
    #[test]
    fn viewer_demand_resolution_ignores_a_transient_spike_between_heartbeats() {
        let t0 = Instant::now();
        let mut state = ViewerDemandResolutionState {
            applied_long_edge: Some(960),
            ..Default::default()
        };

        // Spike (publication-open / raw-box fallback) ...
        assert_eq!(state.reconcile(Some(1700), 960, t0), Some(960));
        // ... contradicted by the next 2s tile heartbeat: pending cleared.
        assert_eq!(
            state.reconcile(Some(900), 960, t0 + Duration::from_secs(2)),
            Some(960)
        );
        assert!(state.pending_change.is_none());
        // Even a second spike much later starts over from zero.
        assert_eq!(
            state.reconcile(Some(1700), 960, t0 + Duration::from_secs(10)),
            Some(960)
        );
        assert_eq!(
            state.reconcile(Some(900), 960, t0 + Duration::from_secs(12)),
            Some(960)
        );
    }

    /// A raise that reverses a just-applied downsize is held to the FULL
    /// downsize hold for `VIEWER_DEMAND_REVERSAL_DWELL`, so even sustained
    /// flip-flopping demand cannot republish faster than once per 6s-sustained
    /// direction.
    #[test]
    fn viewer_demand_resolution_holds_a_prompt_reversal_to_the_long_hold() {
        let t0 = Instant::now();
        let mut state = ViewerDemandResolutionState {
            applied_long_edge: Some(1920),
            ..Default::default()
        };

        // Commit a downsize after the hold.
        assert_eq!(state.reconcile(Some(900), 1920, t0), Some(1920));
        let t1 = t0 + VIEWER_DEMAND_DOWNSIZE_HOLD;
        assert_eq!(state.reconcile(Some(900), 1920, t1), Some(960));

        // Sustained raise starting right after: the short upsize hold must NOT
        // be enough while inside the reversal dwell ...
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(state.reconcile(Some(1700), 960, t2), Some(960));
        assert_eq!(
            state.reconcile(Some(1700), 960, t2 + VIEWER_DEMAND_UPSIZE_HOLD),
            Some(960),
            "a reversal inside the dwell must meet the full downsize hold"
        );
        // ... but a genuinely sustained raise still gets through at the long
        // hold: the user really did enlarge the tile.
        assert_eq!(
            state.reconcile(Some(1700), 960, t2 + VIEWER_DEMAND_DOWNSIZE_HOLD),
            Some(1920)
        );
    }

    /// The 2026-07-30 field loop, replayed against the sender's decision seam
    /// (session.log 18:33:15-18:33:55 and 18:36:05-18:37:02): a viewer whose
    /// tile heartbeat steadily demands rung 960 every 2s, and which -- like
    /// every LiveKit viewer -- answers each republish's TrackPublished
    /// announcement ~300ms later with one demand packet not derived from a
    /// rendered tile (viewport-sized before the web fix). On shipped 0.8.1
    /// this produced a PAIR of republishes every 8.0s forever, because raises
    /// applied instantly: the spike reversed each downsize the sender had just
    /// held 6s for. Steady state over a full simulated minute must instead be
    /// ONE deliberate downsize, then zero republishes.
    #[test]
    fn viewer_demand_resolution_settles_despite_a_spike_after_every_republish() {
        let t0 = Instant::now();
        let tick = Duration::from_secs(2); // the receiver's heartbeat period
        let steady_demand = 900; // a small tile: rung 960
        let spike_demand = 1700; // viewport / raw element box: rung 1920

        let mut state = ViewerDemandResolutionState {
            applied_long_edge: Some(1920),
            ..Default::default()
        };
        let mut applied_changes: Vec<(Duration, u32)> = Vec::new();
        let mut current = 1920;
        let mut now = t0;
        for _ in 0..30 {
            // one simulated minute of heartbeats
            now += tick;
            let next = state.reconcile(Some(steady_demand), current, now).unwrap();
            if next != current {
                applied_changes.push((now - t0, next));
                current = next;
                // Every applied change republishes the track, LiveKit
                // re-announces it, and the viewer answers with one
                // publication-open packet ~300ms later. Feeding that spike
                // back is the closed loop this state machine must break.
                let after_spike = state
                    .reconcile(
                        Some(spike_demand),
                        current,
                        now + Duration::from_millis(300),
                    )
                    .unwrap();
                if after_spike != current {
                    applied_changes.push((now - t0 + Duration::from_millis(300), after_spike));
                    current = after_spike;
                }
            }
        }
        assert_eq!(
            applied_changes.len(),
            1,
            "steady demand + a post-republish spike must yield exactly one \
             republish over a minute; shipped 0.8.1 produced ~14 (a pair every \
             8s): {applied_changes:?}"
        );
        assert_eq!(
            applied_changes[0].1, 960,
            "the one change is the deliberate downsize to the demanded rung"
        );
        assert_eq!(
            current, 960,
            "and the share must still be at that rung at the end of the minute"
        );
    }

    #[test]
    fn first_viewer_demand_seeds_from_current_and_downsizes_after_hold() {
        let t0 = Instant::now();
        // The first reconcile seeds from the published size; a higher demand
        // becomes pending, not an instant raise.
        let mut state = ViewerDemandResolutionState::default();
        assert_eq!(state.reconcile(Some(3000), 1920, t0), Some(1920));
        assert!(state.pending_change.is_some());

        let mut state = ViewerDemandResolutionState::default();
        assert_eq!(state.reconcile(Some(700), 3840, t0), Some(3840));
        assert_eq!(
            state.reconcile(
                Some(700),
                3840,
                t0 + VIEWER_DEMAND_DOWNSIZE_HOLD - Duration::from_millis(1)
            ),
            Some(3840)
        );
        assert!(state.pending_change.is_some());
        // A corrected demand matching the applied rung cancels the pending
        // downsize outright.
        assert_eq!(state.reconcile(Some(3000), 3840, t0), Some(3840));
        assert!(
            state.pending_change.is_none(),
            "a corrected higher demand must cancel the pending downsize"
        );

        let mut state = ViewerDemandResolutionState::default();
        assert_eq!(state.reconcile(Some(700), 3840, t0), Some(3840));
        assert_eq!(
            state.reconcile(Some(700), 3840, t0 + VIEWER_DEMAND_DOWNSIZE_HOLD),
            Some(960)
        );
    }

    #[test]
    fn viewer_demand_aggregates_largest_live_receiver_and_drops_stale_viewers() {
        let state = SessionState::default();
        let t0 = Instant::now();
        {
            let mut guard = state.inner.lock_unpoisoned();
            for (viewer_id, pixel_width, pixel_height) in
                [("viewer-1x", 1280, 720), ("viewer-2x", 2560, 1440)]
            {
                guard.viewer_demands.insert(
                    ViewerDemandKey {
                        window_id: 42,
                        viewer_id: viewer_id.to_string(),
                    },
                    PassiveViewerDemand {
                        seq: 1,
                        updated_at: t0,
                        width: 1280,
                        height: 720,
                        scale: if viewer_id == "viewer-2x" { 2.0 } else { 1.0 },
                        pixel_width,
                        pixel_height,
                    },
                );
            }
            assert_eq!(viewer_demand_requested_long_edge(&guard, 42), Some(2560));
        }

        assert_eq!(
            expire_stale_viewer_demands(
                &state,
                t0 + VIEWER_DEMAND_STALE_AFTER + Duration::from_millis(1)
            ),
            vec![42]
        );
        let guard = state.inner.lock_unpoisoned();
        assert_eq!(viewer_demand_requested_long_edge(&guard, 42), None);
    }

    #[test]
    fn session_model_stopping_focused_share_promotes_next_most_recent() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        model.start_share(20).unwrap();
        model.start_share(30).unwrap();
        model.quality_updates.clear();

        model.stop_share(30);

        assert_eq!(model.focused_window(), Some(20));
        assert_eq!(model.shares[&20].quality, ShareQuality::Full);
        assert_eq!(model.shares[&10].quality, ShareQuality::Reduced);
        assert_eq!(model.quality_updates, vec![(20, ShareQuality::Full)]);
        assert_eq!(model.last_toggled_window, Some(30));
    }

    #[test]
    fn session_model_stopping_unfocused_share_does_not_republish_focus() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        model.start_share(20).unwrap();
        model.quality_updates.clear();

        model.stop_share(10);

        assert_eq!(model.focused_window(), Some(20));
        assert_eq!(model.shares[&20].quality, ShareQuality::Full);
        assert!(model.quality_updates.is_empty());
        assert_eq!(model.last_toggled_window, Some(10));
    }

    /// Records every existence query so a test can assert the expensive
    /// CoreGraphics path is NOT taken when it should be short-circuited.
    struct FakeExistence {
        alive: Vec<u32>,
        asked: std::sync::Mutex<Vec<u32>>,
    }

    impl FakeExistence {
        fn new(alive: &[u32]) -> Self {
            Self {
                alive: alive.to_vec(),
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> Vec<u32> {
            self.asked.lock_unpoisoned().clone()
        }
    }

    impl WindowExistence for FakeExistence {
        fn window_exists(&self, window_id: u32) -> bool {
            self.asked.lock_unpoisoned().push(window_id);
            self.alive.contains(&window_id)
        }
    }

    /// #742: this replaces a test that asserted on `classify_shared_window_
    /// screen_status`, a `#[cfg(test)]` DUPLICATE of the production rules --
    /// it read green while being unable to fail when production regressed.
    /// These assertions run the real functions the app calls.
    #[test]
    fn shared_window_screen_status_distinguishes_offscreen_from_closed() {
        let frame = crate::hover_tab::WindowFrame {
            x: 1,
            y: 2,
            width: 300,
            height: 200,
        };
        assert_eq!(
            screen_status_from_flags(frame, true, false),
            SharedWindowScreenStatus::OnScreen(frame)
        );
        assert_eq!(
            screen_status_from_flags(frame, false, false),
            SharedWindowScreenStatus::OffScreen
        );
        assert_eq!(
            screen_status_from_flags(frame, false, true),
            SharedWindowScreenStatus::Closed
        );
        // "visible" wins over a stale closed flag -- a window that came back
        // must never be reported Closed.
        assert_eq!(
            screen_status_from_flags(frame, true, true),
            SharedWindowScreenStatus::OnScreen(frame)
        );
    }

    #[test]
    fn visibility_decisions_marks_absent_window_closed_only_when_it_is_gone() {
        use crate::transport::publisher::SharedSourceKind;
        let sources = vec![
            (10, SharedSourceKind::Window),
            (20, SharedSourceKind::Window),
            (30, SharedSourceKind::Window),
        ];
        // 10 is on screen; 20 exists but is off-screen; 30 is gone.
        let existence = FakeExistence::new(&[20]);
        let decisions = visibility_decisions(&sources, &[10], &existence);
        assert_eq!(
            decisions,
            vec![(10, true, false), (20, false, false), (30, false, true)]
        );
        // The expensive CoreGraphics call must be skipped for visible windows.
        assert_eq!(
            existence.asked(),
            vec![20, 30],
            "window_exists must not be consulted for a window already known visible"
        );
    }

    /// Display shares are not in the window list at all, so window-presence is
    /// meaningless for them; without this carve-out every display share would
    /// be reported closed on the first refresh tick.
    #[test]
    fn visibility_decisions_never_marks_a_display_share_closed() {
        use crate::transport::publisher::SharedSourceKind;
        let sources = vec![(7, SharedSourceKind::Display)];
        let existence = FakeExistence::new(&[]);
        let decisions = visibility_decisions(&sources, &[], &existence);
        assert_eq!(decisions, vec![(7, true, false)]);
        assert!(
            existence.asked().is_empty(),
            "a display share must never trigger a window-existence query"
        );
    }

    fn target(
        width: u32,
        height: u32,
        quality: ShareQuality,
        resolution: CaptureResolution,
    ) -> RepublishTarget {
        RepublishTarget {
            width,
            height,
            quality,
            resolution,
        }
    }

    #[test]
    fn republish_swap_waits_until_new_track_exists_before_old_unpublish() {
        let observed = target(800, 600, ShareQuality::Full, CaptureResolution::Auto);
        let next = target(900, 700, ShareQuality::Full, CaptureResolution::Auto);

        assert_eq!(
            republish_swap_decision(observed, observed, next, false),
            RepublishSwapDecision::SwapOldAfterNewPublished
        );
    }

    #[test]
    fn republish_swap_drops_duplicate_new_track_if_target_already_landed() {
        let observed = target(800, 600, ShareQuality::Full, CaptureResolution::Auto);
        let next = target(900, 700, ShareQuality::Full, CaptureResolution::Auto);

        assert_eq!(
            republish_swap_decision(next, observed, next, false),
            RepublishSwapDecision::AlreadyAtTarget
        );
    }

    #[test]
    fn forced_republish_swaps_even_when_target_already_matches() {
        let observed = target(800, 600, ShareQuality::Full, CaptureResolution::Auto);

        assert_eq!(
            republish_swap_decision(observed, observed, observed, true),
            RepublishSwapDecision::SwapOldAfterNewPublished
        );
        assert_eq!(
            republish_swap_decision(observed, observed, observed, false),
            RepublishSwapDecision::AlreadyAtTarget
        );
    }

    #[test]
    fn reconnect_publication_health_requires_a_bound_sid_before_preserving_a_share() {
        assert_eq!(
            reconnect_publication_health(
                "TR_current",
                "petal-window-7",
                [("TR_current", "petal-window-7")],
            ),
            ReconnectPublicationHealth::CurrentSidPresent
        );
        assert_eq!(
            reconnect_publication_health(
                "TR_stale",
                "petal-window-7",
                [("TR_sdk_replaced", "petal-window-7")],
            ),
            ReconnectPublicationHealth::ReplacementAlreadyPresent
        );
        assert!(reconnect_publication_requires_repair(
            ReconnectPublicationHealth::ReplacementAlreadyPresent
        ));
        assert_eq!(
            reconnect_publication_health(
                "TR_missing",
                "petal-window-7",
                [("TR_other", "petal-window-8")],
            ),
            ReconnectPublicationHealth::Missing
        );
        assert!(reconnect_publication_requires_repair(
            ReconnectPublicationHealth::Missing
        ));
        assert!(!reconnect_publication_requires_repair(
            ReconnectPublicationHealth::CurrentSidPresent
        ));
    }

    /// #713: drives the REAL `repair_local_track_publication_after_reconnect`
    /// handler chain (not a reimplemented copy) for the case that matters --
    /// a mic/camera publication that survived the reconnect (its SID is still
    /// present) must NOT trigger a republish attempt or a user notice.
    #[tokio::test]
    async fn local_track_reconnect_repair_healthy_current_sid_skips_republish() {
        let republish_calls = Arc::new(AtomicU64::new(0));
        let notice_calls = Arc::new(AtomicU64::new(0));
        let republish_calls_in = republish_calls.clone();
        let notice_calls_in = notice_calls.clone();

        let outcome = repair_local_track_publication_after_reconnect(
            "mic",
            "TR_current",
            "petal-mic",
            &[("TR_current".to_string(), "petal-mic".to_string())],
            || true,
            move || async move {
                republish_calls_in.fetch_add(1, Ordering::SeqCst);
                Ok("TR_new".to_string())
            },
            move |_message| {
                notice_calls_in.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        assert_eq!(outcome, LocalTrackRepairOutcome::Healthy);
        assert_eq!(
            republish_calls.load(Ordering::SeqCst),
            0,
            "a healthy publication must never trigger a republish attempt"
        );
        assert_eq!(notice_calls.load(Ordering::SeqCst), 0);
    }

    /// #713: the vendored SDK's `handle_restarted` unpublishes before it
    /// republishes (confirmed against `vendor/livekit/src/room/mod.rs`), so a
    /// timed-out republish leaves NO local publication for the tracked SID --
    /// exactly the `Missing` health this drives. The repair attempt must
    /// actually fire (the DoD's core assertion) and, on success, no failure
    /// notice is surfaced.
    #[tokio::test]
    async fn local_track_reconnect_repair_missing_publication_retries_and_succeeds() {
        let republish_calls = Arc::new(AtomicU64::new(0));
        let notice_calls = Arc::new(AtomicU64::new(0));
        let republish_calls_in = republish_calls.clone();
        let notice_calls_in = notice_calls.clone();

        let outcome = repair_local_track_publication_after_reconnect(
            "camera",
            "TR_old",
            "petal-camera-alice",
            &[], // no local publications at all -- the SDK's republish attempt timed out
            || true,
            move || async move {
                republish_calls_in.fetch_add(1, Ordering::SeqCst);
                Ok("TR_new".to_string())
            },
            move |_message| {
                notice_calls_in.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        assert_eq!(outcome, LocalTrackRepairOutcome::Replaced);
        assert_eq!(
            republish_calls.load(Ordering::SeqCst),
            1,
            "a missing publication must trigger exactly one republish attempt"
        );
        assert_eq!(
            notice_calls.load(Ordering::SeqCst),
            0,
            "a successful repair must not surface a failure notice"
        );
    }

    /// #713 DoD: "a still-failing republish after repair surfaces a
    /// user-visible notice rather than silently dropping the track." Drives
    /// the real handler chain through a republish failure and asserts the
    /// notice closure actually fires, with the real error message.
    #[tokio::test]
    async fn local_track_reconnect_repair_missing_publication_retry_fails_surfaces_notice() {
        let republish_calls = Arc::new(AtomicU64::new(0));
        let notice_messages = Arc::new(Mutex::new(Vec::<String>::new()));
        let republish_calls_in = republish_calls.clone();
        let notice_messages_in = notice_messages.clone();

        let outcome = repair_local_track_publication_after_reconnect(
            "mic",
            "TR_old",
            "petal-mic",
            &[],
            || true,
            move || async move {
                republish_calls_in.fetch_add(1, Ordering::SeqCst);
                Err("engine: internal error: track publication timed out".to_string())
            },
            move |message| {
                notice_messages_in.lock_unpoisoned().push(message);
            },
        )
        .await;

        assert_eq!(
            outcome,
            LocalTrackRepairOutcome::Failed(
                "engine: internal error: track publication timed out".to_string()
            )
        );
        assert_eq!(
            republish_calls.load(Ordering::SeqCst),
            1,
            "exactly one bounded retry, not an unbounded loop"
        );
        assert_eq!(
            notice_messages.lock_unpoisoned().as_slice(),
            ["engine: internal error: track publication timed out".to_string()],
            "a still-failing republish must surface exactly one user-visible notice"
        );
    }

    /// #713: a reconnect repair task that outlives its room (left/superseded
    /// between the `Reconnected` event and this repair pass running) must
    /// cancel WITHOUT attempting a republish -- same generation-guard
    /// discipline `repair_active_share_publications_after_reconnect` already
    /// enforces for window shares.
    #[tokio::test]
    async fn local_track_reconnect_repair_skips_when_guard_is_stale_before_attempt() {
        let republish_calls = Arc::new(AtomicU64::new(0));
        let notice_calls = Arc::new(AtomicU64::new(0));
        let republish_calls_in = republish_calls.clone();
        let notice_calls_in = notice_calls.clone();

        let outcome = repair_local_track_publication_after_reconnect(
            "camera",
            "TR_old",
            "petal-camera-alice",
            &[],
            || false, // room left / reconnect superseded before this ran
            move || async move {
                republish_calls_in.fetch_add(1, Ordering::SeqCst);
                Ok("TR_new".to_string())
            },
            move |_message| {
                notice_calls_in.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        assert_eq!(outcome, LocalTrackRepairOutcome::Skipped);
        assert_eq!(
            republish_calls.load(Ordering::SeqCst),
            0,
            "a stale reconnect generation must never attempt a republish"
        );
        assert_eq!(notice_calls.load(Ordering::SeqCst), 0);
    }

    /// #713: if the guard goes stale in the exact window between the failed
    /// republish and the notice check (a newer reconnect superseded this
    /// one), the failure must still be reported as `Failed` (so the caller's
    /// own logging/outcome tracking sees the truth) but the user-facing
    /// notice must be suppressed -- an already-superseded repair's failure
    /// is stale information, not something to alarm the user with.
    #[tokio::test]
    async fn local_track_reconnect_repair_suppresses_notice_when_guard_goes_stale_after_failure() {
        let notice_calls = Arc::new(AtomicU64::new(0));
        let notice_calls_in = notice_calls.clone();
        // First call (pre-attempt check) true, second call (post-failure
        // check) false -- simulates the guard going stale WHILE the
        // republish await was in flight.
        let call_count = Arc::new(AtomicU64::new(0));

        let outcome = repair_local_track_publication_after_reconnect(
            "mic",
            "TR_old",
            "petal-mic",
            &[],
            move || call_count.fetch_add(1, Ordering::SeqCst) == 0,
            || async move { Err("timed out".to_string()) },
            move |_message| {
                notice_calls_in.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        assert_eq!(
            outcome,
            LocalTrackRepairOutcome::Failed("timed out".to_string())
        );
        assert_eq!(
            notice_calls.load(Ordering::SeqCst),
            0,
            "a repair superseded mid-attempt must not surface a notice for stale state"
        );
    }

    #[test]
    fn republish_swap_retries_when_any_axis_changed_mid_publish() {
        let observed = target(800, 600, ShareQuality::Full, CaptureResolution::P1440);
        let next = target(900, 700, ShareQuality::Full, CaptureResolution::P1440);
        let interleaved = target(800, 600, ShareQuality::Reduced, CaptureResolution::P1440);

        assert_eq!(
            republish_swap_decision(interleaved, observed, next, false),
            RepublishSwapDecision::DropNewAndRetry
        );
    }

    /// #841: the resize path must ask the same size authority as every other
    /// republish path, not re-cap the frame it just received.
    ///
    /// `republish_target_for_resize` used to pass the CAPTURED FRAME SIZE in
    /// as if it were the source backing size. For the incident's ROI-narrowed
    /// 2556x1652 frame under Auto that yields `native = 2556`, `manual_cap =
    /// 2556`, `min(2560, 2556) = 2556` -- not greater, so it fell through to
    /// `cap_capture_size_to_long_edge` and returned 2556x1652, a size no
    /// other authority would ever compute. Feeding the SOURCE backing size to
    /// the same function is what every other path effectively does, and it
    /// lands on the authority's answer instead.
    #[test]
    fn resize_republish_target_comes_from_the_source_not_the_delivered_frame_841() {
        // The already-capped frame the pump observed during the incident.
        let from_frame = crate::capture::cap_capture_size_for_limits(
            2556,
            1652,
            2.0,
            CaptureResolution::Auto,
            Some(2560),
        );
        assert_eq!(
            (from_frame.0, from_frame.1),
            (2556, 1652),
            "re-capping a capped frame reproduces the third authority's answer"
        );

        // The same call rooted in the SOURCE's real backing size -- which is
        // what `capture_size_for_resolution` computes from -- is stable and
        // agrees with the quality path.
        let from_source = crate::capture::cap_capture_size_for_limits(
            3456,
            2234,
            2.0,
            CaptureResolution::Auto,
            Some(2560),
        );
        assert_eq!((from_source.0, from_source.1), (2560, 1654));
        assert_ne!((from_frame.0, from_frame.1), (from_source.0, from_source.1));

        // The manual resolution cap still binds through that same authority.
        let capped = crate::capture::cap_capture_size_for_limits(
            3840,
            2160,
            2.0,
            CaptureResolution::P1080,
            None,
        );
        assert_eq!((capped.0, capped.1), (1920, 1080));
    }

    /// #841: the resize path was the unlimited republisher in the incident
    /// log. It now shares the quality path's per-window clock, so the two
    /// together cannot exceed one republish per
    /// `REPUBLISH_RECONCILE_MIN_INTERVAL`.
    #[test]
    fn resize_and_quality_republish_share_one_rate_limit_841() {
        let window_id = 0x841F_0001;
        republish_reconcile_last_by_window()
            .lock_unpoisoned()
            .remove(&window_id);

        assert!(
            claim_republish_reconcile_slot(window_id, "resize"),
            "the first republish must be allowed"
        );
        assert!(
            !claim_republish_reconcile_slot(window_id, "capture-size/layout change"),
            "the quality path must not get a second slot the resize path just took"
        );
        assert!(
            !claim_republish_reconcile_slot(window_id, "resize"),
            "nor may the resize path re-take its own slot"
        );

        // A different window is unaffected -- the clock is per window.
        let other = 0x841F_0002;
        republish_reconcile_last_by_window()
            .lock_unpoisoned()
            .remove(&other);
        assert!(claim_republish_reconcile_slot(other, "resize"));

        republish_reconcile_last_by_window()
            .lock_unpoisoned()
            .remove(&window_id);
        republish_reconcile_last_by_window()
            .lock_unpoisoned()
            .remove(&other);
    }

    #[test]
    fn republish_reconfigure_failure_keeps_previous_track_target() {
        let observed = target(800, 600, ShareQuality::Full, CaptureResolution::P1440);
        let next = target(900, 700, ShareQuality::Full, CaptureResolution::P1440);
        let mut slot_target = observed;
        let mut capture_target = observed;
        let mut events = Vec::new();

        assert_eq!(
            republish_swap_decision(slot_target, observed, next, false),
            RepublishSwapDecision::SwapOldAfterNewPublished
        );
        events.push("update_capture");
        let capture_update_succeeded = false;
        if capture_update_succeeded {
            capture_target = next;
            slot_target = next;
            events.push("swap_track");
        } else {
            events.push("drop_new_track");
        }

        assert_eq!(events, vec!["update_capture", "drop_new_track"]);
        assert_eq!(slot_target, observed);
        assert_eq!(capture_target, observed);
    }

    #[test]
    fn session_model_resolution_change_routes_one_republish() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();

        model.set_resolution(10, CaptureResolution::Uhd4k, 3840, 2160);
        model.set_resolution(10, CaptureResolution::Uhd4k, 3840, 2160);

        assert_eq!(
            model.republish_updates,
            vec![(
                10,
                target(3840, 2160, ShareQuality::Full, CaptureResolution::Uhd4k)
            )]
        );
        assert_eq!(model.shares[&10].width, 3840);
        assert_eq!(model.shares[&10].height, 2160);
        assert_eq!(model.shares[&10].resolution, CaptureResolution::Uhd4k);
    }

    #[test]
    fn session_model_post_wake_restart_preserves_manual_resolution() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        model.set_resolution(10, CaptureResolution::P1440, 2560, 1440);

        model.restart_after_wake(10).unwrap();

        assert_eq!(model.shares[&10].resolution, CaptureResolution::P1440);
        assert_eq!(model.shares[&10].quality, ShareQuality::Full);
    }

    #[test]
    fn interleaved_quality_republish_preserves_new_resolution_dimensions() {
        let stale_quality_observed =
            target(1920, 1080, ShareQuality::Reduced, CaptureResolution::P1440);
        let stale_quality_target = target(1920, 1080, ShareQuality::Full, CaptureResolution::P1440);
        let resolution_landed = target(3840, 2160, ShareQuality::Reduced, CaptureResolution::Uhd4k);

        assert_eq!(
            republish_swap_decision(
                resolution_landed,
                stale_quality_observed,
                stale_quality_target,
                false
            ),
            RepublishSwapDecision::DropNewAndRetry
        );

        let retried_quality_target =
            target(3840, 2160, ShareQuality::Full, CaptureResolution::Uhd4k);
        assert_eq!(
            republish_swap_decision(
                resolution_landed,
                resolution_landed,
                retried_quality_target,
                false
            ),
            RepublishSwapDecision::SwapOldAfterNewPublished
        );
    }

    #[test]
    fn published_metadata_color_profile_matches_current_encoder_output() {
        assert_eq!(
            published_metadata_color_profile(crate::video_color::VideoColorProfile::BT601_VIDEO),
            crate::video_color::VideoColorProfile::BT601_VIDEO
        );
        assert_eq!(
            published_metadata_color_profile(
                crate::video_color::VideoColorProfile::SRGB_BT709_FULL
            ),
            crate::video_color::VideoColorProfile::SRGB_BT709_FULL
        );
        assert_eq!(
            published_metadata_color_profile(
                crate::video_color::VideoColorProfile::DISPLAY_P3_BT709_FULL
            ),
            crate::video_color::VideoColorProfile::DISPLAY_P3_BT709_FULL
        );
    }

    #[test]
    fn failed_attempt_error_does_not_poison_successful_retry() {
        let (failed_tx, mut failed_rx) = capture_attempt_error_channel();
        failed_tx
            .send(crate::capture::CAPTURE_LAYOUT_INVALID.to_string())
            .unwrap();
        let (_winning_tx, mut winning_rx) = capture_attempt_error_channel();

        assert_eq!(
            failed_rx.try_recv().unwrap(),
            crate::capture::CAPTURE_LAYOUT_INVALID
        );
        assert!(matches!(
            winning_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn late_losing_attempt_callback_cannot_mutate_winner_state() {
        let generation = Arc::new(AtomicU64::new(1));
        let loser = CaptureAttemptGuard {
            generation: generation.clone(),
            expected: 1,
        };
        let winner = CaptureAttemptGuard {
            generation: generation.clone(),
            expected: 2,
        };
        let first_frame_sent = AtomicBool::new(false);
        let latest_sequence = AtomicU64::new(0);

        generation.store(2, Ordering::SeqCst);
        if loser.is_current() {
            first_frame_sent.store(true, Ordering::SeqCst);
            latest_sequence.store(10, Ordering::SeqCst);
        }
        assert!(!first_frame_sent.load(Ordering::SeqCst));
        assert_eq!(latest_sequence.load(Ordering::SeqCst), 0);

        if winner.is_current() {
            first_frame_sent.store(true, Ordering::SeqCst);
            latest_sequence.store(20, Ordering::SeqCst);
        }
        assert!(first_frame_sent.load(Ordering::SeqCst));
        assert_eq!(latest_sequence.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn startup_reconfigure_without_matching_frame_fails_with_stable_code() {
        let gate = crate::capture::LayoutIntegrityGate::default();
        gate.seed_pending_reconfiguration(800, 600);
        assert_eq!(
            first_frame_timeout_error(&gate),
            crate::capture::CAPTURE_LAYOUT_INVALID
        );
        assert!(gate.is_failed());
    }

    #[test]
    fn active_reconfigure_ack_timeout_uses_exact_pending_target() {
        assert!(pending_layout_ack_expired(
            Some((800, 600)),
            (800, 600),
            true
        ));
        assert!(!pending_layout_ack_expired(
            Some((1000, 600)),
            (800, 600),
            true
        ));
        assert!(!pending_layout_ack_expired(
            Some((800, 600)),
            (800, 600),
            false
        ));
    }

    #[test]
    fn capture_layout_invalid_on_live_share_restarts_in_place_never_stops() {
        // 2026-07-30 defect A: the live monitor used to answer
        // capture-layout-invalid with a full stop_share + unpublish while
        // capture-diag reported the stream healthy at ~29.5fps.
        assert_eq!(
            capture_failure_action(crate::capture::CAPTURE_LAYOUT_INVALID.to_string(), false),
            CaptureFailureAction::RestartInPlace {
                message: crate::capture::CAPTURE_LAYOUT_INVALID.to_string()
            }
        );
        // A stream SCK itself stopped (e.g. the window truly closed) still
        // tears down.
        assert_eq!(
            capture_failure_action("stream stopped by the system".to_string(), false),
            CaptureFailureAction::StopShare {
                message:
                    "capture stalled -- ScreenCaptureKit stopped the stream: stream stopped by the system"
                        .to_string()
            }
        );
    }

    #[test]
    fn display_sleep_capture_stall_restarts_in_place_never_stops() {
        // #734: lid-close / display-sleep emits this exact SCK string and
        // used to permanently stop_share. Sleep flag OR the string alone
        // must restart in place; genuine non-sleep errors still stop.
        let sck_sleep = "SCStream error (No capture source provided): Failed to find any displays or windows to capture";
        assert_eq!(
            capture_failure_action(sck_sleep.to_string(), false),
            CaptureFailureAction::RestartInPlace {
                message: format!("capture interrupted by display/system sleep: {sck_sleep}")
            }
        );
        assert_eq!(
            capture_failure_action("stream stopped by the system".to_string(), true),
            CaptureFailureAction::RestartInPlace {
                message:
                    "capture interrupted by display/system sleep: stream stopped by the system"
                        .to_string()
            }
        );
        // Unrelated genuine stall, no sleep correlation → still StopShare.
        assert_eq!(
            capture_failure_action("encoder hardware fault".to_string(), false),
            CaptureFailureAction::StopShare {
                message:
                    "capture stalled -- ScreenCaptureKit stopped the stream: encoder hardware fault"
                        .to_string()
            }
        );
        assert!(is_sleep_style_sck_error(sck_sleep));
        assert!(!is_sleep_style_sck_error("stream stopped by the system"));
    }

    #[test]
    fn reconfigure_storm_coalesces_to_newest_target() {
        let mut queued = std::collections::VecDeque::from(vec![
            "capture-layout-reconfigure:2144x1280".to_string(),
            "capture-layout-reconfigure:2144x1272".to_string(),
        ]);
        assert_eq!(
            coalesce_capture_error_events(
                "capture-layout-reconfigure:2144x1328".to_string(),
                || queued.pop_front(),
            ),
            CoalescedCaptureEvent::Reconfigure {
                width: 2144,
                height: 1272
            }
        );

        // A terminal error queued behind reconfigure targets wins -- the
        // targets are moot for a capture instance that is being replaced.
        let mut queued = std::collections::VecDeque::from(vec![
            crate::capture::CAPTURE_LAYOUT_INVALID.to_string(),
            "capture-layout-reconfigure:2144x1264".to_string(),
        ]);
        assert_eq!(
            coalesce_capture_error_events(
                "capture-layout-reconfigure:2144x1328".to_string(),
                || queued.pop_front(),
            ),
            CoalescedCaptureEvent::Failure(crate::capture::CAPTURE_LAYOUT_INVALID.to_string())
        );

        // A non-reconfigure first event passes straight through.
        assert_eq!(
            coalesce_capture_error_events("stream stopped".to_string(), || None),
            CoalescedCaptureEvent::Failure("stream stopped".to_string())
        );
    }

    /// Closed-loop replay of the 2026-07-30 18:34:40.273-.360 live failure
    /// (window 1888): a resize drag produced a new content ROI every frame,
    /// SCStream acknowledged reconfigurations ~2 frames late, and a THIRD
    /// distinct target arrived while one was still pending. This drives the
    /// REAL handler chain -- `layout_decision` -> `LayoutIntegrityGate::
    /// observe` -> `layout_event` -> the monitor's coalescing -- not the
    /// isolated helpers. Under the pre-fix gate the third distinct target
    /// returned `FailFirst` (terminal capture-layout-invalid) and the share
    /// was unpublished; the required behavior is: never terminal, settle on
    /// the newest ROI once the drag stops.
    #[test]
    fn live_resize_reconfigure_storm_settles_on_newest_roi_without_teardown() {
        use crate::capture::{
            layout_decision, layout_event, CaptureSourceOrigin, LayoutGateAction,
            LayoutIntegrityGate, LayoutObservationRoute,
        };

        const SCALE: f64 = 2.0;
        const SCK_APPLY_LATENCY_FRAMES: usize = 2; // ~66ms at 30fps, matches the 86ms log gap

        let gate = LayoutIntegrityGate::default();
        let mut configured = (2144u32, 1360u32); // committed by the monitor
        let mut sck_output = (2144u32, 1360u32); // what SCK actually delivers
        let mut sck_apply_at: Option<(usize, (u32, u32))> = None;
        let mut event_queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();

        // Backing-pixel content heights per frame: the drag from the live
        // log (1360 -> 1328 -> 1280 -> ...), then the window settles.
        let mut heights: Vec<u32> = vec![1360, 1328, 1300, 1280, 1264, 1248];
        heights.extend(std::iter::repeat(1248).take(30));

        let mut accepted_at_final = false;
        for (frame, &height) in heights.iter().enumerate() {
            // SCK applies a previously requested configuration late.
            if let Some((apply_frame, target)) = sck_apply_at {
                if frame >= apply_frame {
                    sck_output = target;
                    sck_apply_at = None;
                }
            }

            let rect = screencapturekit::cg::CGRect::new(
                0.0,
                0.0,
                f64::from(sck_output.0) / SCALE,
                f64::from(height.min(sck_output.1)) / SCALE,
            );
            let decision = layout_decision(
                CaptureSourceOrigin::DirectWindowId,
                sck_output,
                configured,
                Some(rect),
                Some(SCALE),
                Some(1.0),
            );
            let action = gate.observe(decision, LayoutObservationRoute::Stream);

            // The defect: the pre-fix gate returned FailFirst here on the
            // third distinct mid-drag target and tore the share down.
            assert_ne!(
                action,
                LayoutGateAction::FailFirst,
                "frame {frame}: a transient layout/ROI mismatch must never be terminal"
            );
            assert!(!gate.is_failed(), "frame {frame}: gate must never fail");

            if let Some(event) = layout_event(action) {
                assert_ne!(
                    event,
                    crate::capture::CAPTURE_LAYOUT_INVALID,
                    "frame {frame}: no terminal layout event may be emitted"
                );
                event_queue.push_back(event);
            }

            // Monitor step: drain + coalesce the storm exactly as
            // spawn_share_monitor does, apply the NEWEST target. Run it
            // only every second frame so events pile up as they did live
            // (the .273/.359/.360 sequence had a target in flight while
            // two more arrived).
            if frame % 2 == 1 {
                if let Some(first) = event_queue.pop_front() {
                    match coalesce_capture_error_events(first, || event_queue.pop_front()) {
                        CoalescedCaptureEvent::Reconfigure { width, height } => {
                            configured = (width, height);
                            sck_apply_at =
                                Some((frame + SCK_APPLY_LATENCY_FRAMES, (width, height)));
                        }
                        CoalescedCaptureEvent::Failure(error) => {
                            panic!("no terminal capture error may surface, got {error}");
                        }
                    }
                }
            }

            if action == LayoutGateAction::Accept && sck_output == (2144, 1248) {
                accepted_at_final = true;
                break;
            }
        }

        assert!(
            accepted_at_final,
            "the share must settle on the newest requested ROI (2144x1248) after the drag stops"
        );
        assert_eq!(gate.pending_reconfiguration(), None);
        assert!(!gate.is_failed());
    }

    #[test]
    fn final_attempt_classifier_preserves_capture_layout_invalid() {
        let layout_error =
            ShareSessionError::Capture(crate::capture::CAPTURE_LAYOUT_INVALID.to_string());
        assert_eq!(
            final_first_frame_failure_message(true, true, Some(&layout_error)),
            crate::capture::CAPTURE_LAYOUT_INVALID
        );

        let ordinary_timeout =
            ShareSessionError::Capture("timed out waiting for first captured frame".to_string());
        assert!(
            final_first_frame_failure_message(true, true, Some(&ordinary_timeout))
                .starts_with("macOS screen-capture stalled")
        );
    }

    #[test]
    fn capture_layout_terminal_paths_are_typed_and_share_one_event_class() {
        let representative_paths = [
            CaptureLayoutStage::FirstFrame,
            CaptureLayoutStage::Validation,
            CaptureLayoutStage::Publish,
            CaptureLayoutStage::Reconfiguration,
        ];
        for stage in representative_paths {
            assert_eq!(
                capture_layout_diagnostic(SourceSelectionClass::Window, stage),
                SentryDiagnosticEvent::CaptureLayoutInvalid(CaptureLayoutDiagnostic {
                    role: DiagnosticRole::Sharer,
                    source: SourceSelectionClass::Window,
                    capture_geometry: GeometryBucket::Unknown,
                    configured_geometry: GeometryBucket::Unknown,
                    pixel_format: PixelFormatClass::Unknown,
                    scale: ScaleBucket::Unknown,
                    encoder: EncoderImplementationClass::NotApplicable,
                    stage,
                })
            );
        }
        assert_eq!(
            capture_layout_diagnostic(
                SourceSelectionClass::Display,
                CaptureLayoutStage::Validation,
            ),
            SentryDiagnosticEvent::CaptureLayoutInvalid(CaptureLayoutDiagnostic {
                role: DiagnosticRole::Sharer,
                source: SourceSelectionClass::Display,
                capture_geometry: GeometryBucket::Unknown,
                configured_geometry: GeometryBucket::Unknown,
                pixel_format: PixelFormatClass::Unknown,
                scale: ScaleBucket::Unknown,
                encoder: EncoderImplementationClass::NotApplicable,
                stage: CaptureLayoutStage::Validation,
            })
        );
    }

    #[tokio::test]
    async fn failed_start_metadata_cleanup_waits_for_metadata_owner() {
        let metadata_lock = Arc::new(tokio::sync::Mutex::new(()));
        let owner = metadata_lock.lock().await;
        let cleared_generation = Arc::new(AtomicU64::new(0));
        let task = {
            let metadata_lock = metadata_lock.clone();
            let cleared_generation = cleared_generation.clone();
            tokio::spawn(async move {
                clear_failed_start_metadata(metadata_lock.as_ref(), || async move {
                    cleared_generation.store(42, Ordering::SeqCst);
                    true
                })
                .await
            })
        };

        tokio::task::yield_now().await;
        assert_eq!(cleared_generation.load(Ordering::SeqCst), 0);
        drop(owner);
        assert!(task.await.unwrap());
        assert_eq!(cleared_generation.load(Ordering::SeqCst), 42);
    }

    #[tokio::test]
    async fn capture_monitor_waits_until_active_share_is_inserted() {
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let monitor_started = Arc::new(AtomicBool::new(false));
        let task = {
            let monitor_started = monitor_started.clone();
            tokio::spawn(async move {
                if await_monitor_activation(Some(activation_rx)).await {
                    monitor_started.store(true, Ordering::SeqCst);
                }
            })
        };

        tokio::task::yield_now().await;
        assert!(!monitor_started.load(Ordering::SeqCst));
        activation_tx.send(()).unwrap();
        task.await.unwrap();
        assert!(monitor_started.load(Ordering::SeqCst));

        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        drop(cancel_tx);
        assert!(!await_monitor_activation(Some(cancel_rx)).await);
        assert!(await_monitor_activation(None).await);
    }

    fn test_frame(fill: u8, sequence: u64) -> crate::capture::CapturedFrame {
        crate::capture::CapturedFrame {
            width: 4,
            height: 4,
            payload: crate::capture::CapturedFramePayload::Bgra {
                bytes_per_row: 16,
                data: crate::capture::PooledFrameData::from_vec(vec![fill; 64]),
            },
            source_scale: 1.0,
            layout_validated: true,
            color_profile: crate::video_color::VideoColorProfile::BT601_VIDEO,
            sequence,
            frame_status: None,
            dirty_rect_count: 0,
            dirty_area_px: 0,
            dirty_rects_known: true,
            lock_copy_ms: 0.25,
            region_generation: None,
        }
    }

    #[test]
    fn malformed_snapshot_cannot_reach_force_push() {
        let mut malformed = test_frame(7, 1);
        malformed.layout_validated = false;
        match validated_snapshot_for_force_push(malformed) {
            Ok(_) => panic!("unvalidated snapshot reached force-push"),
            Err(error) => assert_eq!(error, crate::capture::CAPTURE_LAYOUT_INVALID),
        }

        let valid = test_frame(7, 2);
        assert!(validated_snapshot_for_force_push(valid).is_ok());
    }

    fn test_strided_frame(
        width: u32,
        height: u32,
        bytes_per_row: usize,
        fill: u8,
        sequence: u64,
        changed_visible_byte: Option<(usize, usize, u8)>,
    ) -> crate::capture::CapturedFrame {
        let mut data = vec![fill; bytes_per_row * height as usize];
        if let Some((row, col_byte, value)) = changed_visible_byte {
            let offset = row * bytes_per_row + col_byte;
            data[offset] = value;
        }
        crate::capture::CapturedFrame {
            width,
            height,
            payload: crate::capture::CapturedFramePayload::Bgra {
                bytes_per_row,
                data: crate::capture::PooledFrameData::from_vec(data),
            },
            source_scale: 1.0,
            layout_validated: true,
            color_profile: crate::video_color::VideoColorProfile::BT601_VIDEO,
            sequence,
            frame_status: None,
            dirty_rect_count: 0,
            dirty_area_px: 0,
            dirty_rects_known: true,
            lock_copy_ms: 0.25,
            region_generation: None,
        }
    }

    fn test_nv12_frame(y_fill: u8, uv_fill: u8, sequence: u64) -> crate::capture::CapturedFrame {
        crate::capture::CapturedFrame {
            width: 4,
            height: 4,
            payload: crate::capture::CapturedFramePayload::Nv12 {
                y: crate::capture::PooledFrameData::from_vec(vec![y_fill; 16]),
                y_stride: 4,
                uv: crate::capture::PooledFrameData::from_vec(vec![uv_fill; 8]),
                uv_stride: 4,
            },
            source_scale: 1.0,
            layout_validated: true,
            color_profile: crate::video_color::VideoColorProfile {
                range: crate::video_color::PixelRange::Video,
                ..crate::video_color::VideoColorProfile::SRGB_BT709_FULL
            },
            sequence,
            frame_status: None,
            dirty_rect_count: 0,
            dirty_area_px: 0,
            dirty_rects_known: true,
            lock_copy_ms: 0.25,
            region_generation: None,
        }
    }

    fn test_native_frame(
        sequence: u64,
        dirty_rect_count: usize,
        dirty_area_px: u64,
        frame_status: Option<screencapturekit::cm::SCFrameStatus>,
    ) -> crate::capture::CapturedFrame {
        let pixel_buffer = screencapturekit::cv::CVPixelBuffer::create(4, 4, 0x3432_3076)
            .expect("headless CoreVideo NV12 pixel buffer");
        crate::capture::CapturedFrame {
            width: 4,
            height: 4,
            payload: crate::capture::CapturedFramePayload::Native {
                pixel_buffer: crate::capture::NativeCapturedPixelBuffer::new(pixel_buffer),
            },
            source_scale: 1.0,
            layout_validated: true,
            color_profile: crate::video_color::VideoColorProfile {
                range: crate::video_color::PixelRange::Video,
                ..crate::video_color::VideoColorProfile::SRGB_BT709_FULL
            },
            sequence,
            frame_status,
            dirty_rect_count,
            dirty_area_px,
            dirty_rects_known: true,
            lock_copy_ms: 0.0,
            region_generation: None,
        }
    }

    // Fable finding 3: a frame whose SCK dirtyRects attachment was missing or
    // unparseable -- dirty_rect_count is 0 but that is NOT an affirmative
    // "nothing changed" signal, unlike every other test_native_frame(...)
    // call in this module.
    fn test_native_frame_with_unknown_dirty_rects(
        sequence: u64,
        frame_status: Option<screencapturekit::cm::SCFrameStatus>,
    ) -> crate::capture::CapturedFrame {
        crate::capture::CapturedFrame {
            dirty_rects_known: false,
            ..test_native_frame(sequence, 0, 0, frame_status)
        }
    }

    #[test]
    fn dirty_rect_skip_skips_clean_frames_and_resumes_on_a_tiny_caret_rect() {
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();

        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000,
                start,
                ShareQuality::Full,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::FirstFrame)
        );
        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame(2, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                2_000,
                start + Duration::from_micros(1_000),
                ShareQuality::Full,
                true,
                false,
            ),
            DirtyRectFrameDecision::Skip { run_length: 1 }
        );
        assert_eq!(
            pump.observe_captured_frame(
                // A blinking caret is only 1x20px, but SCK still reports it
                // authoritatively as one dirty rect. It must resume immediately.
                test_native_frame(
                    3,
                    1,
                    20,
                    Some(screencapturekit::cm::SCFrameStatus::Complete)
                ),
                3_000,
                start + Duration::from_micros(2_000),
                ShareQuality::Full,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::DirtyRectAfterSkip {
                skipped_frames: 1,
            })
        );
        assert_eq!(pump.skip_run_length(), 0);
    }

    #[test]
    fn dirty_rect_skip_never_skips_a_frame_with_unknown_dirty_rects() {
        // Fable finding 3's fail-safe: dirty_rect_count == 0 with a missing
        // or unparseable dirtyRects attachment is NOT the same as SCK
        // affirming "nothing changed" -- it must always push, never skip,
        // even in the middle of an otherwise-legitimate skip run.
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;

        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000,
                start,
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::FirstFrame)
        );
        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame(2, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                2_000,
                start + Duration::from_micros(1_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Skip { run_length: 1 }
        );
        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame_with_unknown_dirty_rects(
                    3,
                    Some(screencapturekit::cm::SCFrameStatus::Complete)
                ),
                3_000,
                start + Duration::from_micros(2_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::DirtyRectsUnknown)
        );
        assert_eq!(pump.skip_run_length(), 0);
    }

    #[test]
    fn dirty_rect_skip_accounts_for_each_frame_in_a_skip_run() {
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;
        let first = test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete));
        assert!(matches!(
            pump.observe_captured_frame(first, 1_000, start, quality, true, false),
            DirtyRectFrameDecision::Push(_)
        ));

        for (sequence, expected_run) in (2..=6).zip(1..=5) {
            assert_eq!(
                pump.observe_captured_frame(
                    test_native_frame(
                        sequence,
                        0,
                        0,
                        Some(screencapturekit::cm::SCFrameStatus::Complete),
                    ),
                    sequence * 1_000,
                    start + Duration::from_micros(sequence * 1_000),
                    quality,
                    true,
                    false,
                ),
                DirtyRectFrameDecision::Skip {
                    run_length: expected_run
                }
            );
        }
        assert_eq!(pump.skip_run_length(), 5);
    }

    #[test]
    fn dirty_rect_skip_never_suppresses_frames_with_unknown_dirty_rects() {
        // Fable finding 3: dirty_rect_count == 0 is only an affirmative
        // "nothing changed" signal when SCK actually delivered the
        // dirtyRects attachment. A frame where the attachment was missing or
        // unparseable must always push, exactly like a real dirty rect
        // would -- collapsing "unknown" into "no change" would reintroduce
        // the #38 blind-spot class this feature exists to avoid.
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000,
                start,
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(_)
        ));
        // A genuinely clean, KNOWN frame right after is correctly skipped --
        // establishes the baseline this test's next assertion contrasts with.
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(2, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                2_000,
                start + Duration::from_micros(1_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Skip { .. }
        ));
        // The same shape of frame, but with an UNKNOWN dirty-rects signal,
        // must push rather than skip.
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame_with_unknown_dirty_rects(
                    3,
                    Some(screencapturekit::cm::SCFrameStatus::Complete)
                ),
                3_000,
                start + Duration::from_micros(2_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::DirtyRectsUnknown)
        ));
    }

    #[test]
    fn dirty_rect_skip_refresh_floor_fires_even_under_sustained_raw_frame_arrival() {
        // Fable finding 4: the ~1fps refresh floor must not depend solely on
        // the idle-tick path. A stream of clean frames arriving faster than
        // any idle tick could (this call sequence never goes through
        // idle_refresh_frame_at at all) must still hit the refresh floor
        // inside observe_captured_frame itself -- otherwise a sustained
        // clean-but-unencoded stream starves the keepalive indefinitely, an
        // unbounded silent freeze the watchdog cannot detect (the pump is
        // alive, not wedged).
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                0,
                start,
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(_)
        ));
        // Clean frames arriving every 100ms (well under the tick interval,
        // and well under the 1s refresh floor) are correctly skipped.
        for (sequence, offset_us) in [(2u64, 100_000u64), (3, 200_000), (4, 300_000)] {
            assert!(
                matches!(
                    pump.observe_captured_frame(
                        test_native_frame(
                            sequence,
                            0,
                            0,
                            Some(screencapturekit::cm::SCFrameStatus::Complete)
                        ),
                        offset_us,
                        start + Duration::from_micros(offset_us),
                        quality,
                        true,
                        false,
                    ),
                    DirtyRectFrameDecision::Skip { .. }
                ),
                "frame at {offset_us}us should have been skipped"
            );
        }
        // A clean frame arriving a full second after the last real push
        // must be pushed as the refresh floor, not skipped.
        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame(5, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000_000,
                start + Duration::from_micros(1_000_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::RefreshFloor)
        );
    }

    #[test]
    fn dirty_rect_skip_never_suppresses_remote_control_frames() {
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000,
                start,
                quality,
                true,
                true,
            ),
            DirtyRectFrameDecision::Push(_)
        ));
        for sequence in 2..=4 {
            assert_eq!(
                pump.observe_captured_frame(
                    test_native_frame(
                        sequence,
                        0,
                        0,
                        Some(screencapturekit::cm::SCFrameStatus::Complete),
                    ),
                    sequence * 1_000,
                    start + Duration::from_micros(sequence * 1_000),
                    quality,
                    true,
                    true,
                ),
                DirtyRectFrameDecision::Push(DirtyRectPushReason::RemoteControl)
            );
        }
        assert_eq!(pump.skip_run_length(), 0);
    }

    #[test]
    fn dirty_rect_skip_pushes_non_normal_status_even_without_dirty_rects() {
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000,
                start,
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(_)
        ));
        assert_eq!(
            pump.observe_captured_frame(
                test_native_frame(2, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Idle)),
                2_000,
                start + Duration::from_micros(1_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(DirtyRectPushReason::NonNormalStatus)
        );
    }

    #[test]
    fn adaptive_idle_tick_backs_off_only_for_empty_unarmed_slots_and_resets_on_wake() {
        let mut tick = AdaptiveIdleTick::default();
        assert_eq!(tick.interval(), Duration::from_millis(100));

        tick.back_off_after_empty_idle_tick(true, false);
        assert_eq!(tick.interval(), Duration::from_millis(200));
        tick.back_off_after_empty_idle_tick(true, false);
        assert_eq!(tick.interval(), Duration::from_millis(400));
        tick.back_off_after_empty_idle_tick(true, false);
        assert_eq!(tick.interval(), Duration::from_millis(500));
        tick.back_off_after_empty_idle_tick(true, false);
        assert_eq!(tick.interval(), Duration::from_millis(500));

        // A non-empty, unarmed tick (real work waiting) is a no-op: neither
        // backs off further nor resets.
        tick.back_off_after_empty_idle_tick(false, false);
        assert_eq!(tick.interval(), Duration::from_millis(500));

        // A completed snapshot pull is itself forward progress and resets
        // the interval back to base, even if the slot also reports empty
        // (see the Fable-finding-1 comment on back_off_after_empty_idle_tick).
        tick.back_off_after_empty_idle_tick(true, true);
        assert_eq!(tick.interval(), Duration::from_millis(100));

        // Re-establish backoff, then confirm the explicit wake-reset path.
        tick.back_off_after_empty_idle_tick(true, false);
        assert_eq!(tick.interval(), Duration::from_millis(200));
        tick.reset_on_wake();
        assert_eq!(tick.interval(), Duration::from_millis(100));
    }

    #[test]
    fn dirty_rect_pump_keeps_the_static_refresh_floor_without_raw_frames() {
        let mut pump = DirtyRectPumpState::default();
        let start = Instant::now();
        let quality = ShareQuality::Full;
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_000_000,
                start,
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Push(_)
        ));
        assert!(matches!(
            pump.observe_captured_frame(
                test_native_frame(2, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete)),
                1_010_000,
                start + Duration::from_micros(10_000),
                quality,
                true,
                false,
            ),
            DirtyRectFrameDecision::Skip { .. }
        ));
        assert_eq!(
            pump.idle_refresh_frame_at(start + Duration::from_micros(999_999), 1_999_999, quality,)
                .map(|frame| frame.sequence),
            None
        );
        assert_eq!(
            pump.idle_refresh_frame_at(
                start + Duration::from_micros(1_000_000),
                2_000_000,
                quality,
            )
            .map(|frame| frame.sequence),
            Some(2)
        );
        assert_eq!(pump.skip_run_length(), 0);

        // Fable finding 2: immediately after that refresh, the NEXT idle tick
        // must NOT refresh again. The regression was that mark_pushed
        // recorded the parked (frozen) frame's original capture timestamp
        // instead of this refresh's actual wall time, making the wall-clock
        // disjunct permanently true after the first refresh and firing a
        // full push on every subsequent idle tick (a 500ms-during-backoff
        // "static" floor instead of the intended ~1s one).
        assert_eq!(
            pump.idle_refresh_frame_at(
                start + Duration::from_micros(1_000_100),
                2_000_100,
                quality,
            )
            .map(|frame| frame.sequence),
            None
        );
        // A full second after THAT refresh, it correctly fires again.
        assert_eq!(
            pump.idle_refresh_frame_at(
                start + Duration::from_micros(2_000_000),
                3_000_000,
                quality,
            )
            .map(|frame| frame.sequence),
            Some(2)
        );
    }

    #[test]
    fn static_frame_pacer_skips_after_identical_warmup() {
        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();
        let first = test_frame(7, 1);

        assert_eq!(
            pacer.observe(&first, 1_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &test_frame(7, 2),
                2_000,
                start + Duration::from_micros(1_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(7, 3),
                3_000,
                start + Duration::from_micros(2_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(7, 4),
                4_000,
                start + Duration::from_micros(3_000)
            ),
            FramePaceDecision::SkipStatic
        );
    }

    #[test]
    fn static_frame_pacer_refreshes_periodically_while_static() {
        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();

        assert_eq!(
            pacer.observe(&test_frame(9, 1), 1_000_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 2),
                1_010_000,
                start + Duration::from_micros(10_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 3),
                1_020_000,
                start + Duration::from_micros(20_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 4),
                1_030_000,
                start + Duration::from_micros(30_000)
            ),
            FramePaceDecision::SkipStatic
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 5),
                2_020_000,
                start + Duration::from_micros(1_020_000)
            ),
            FramePaceDecision::PushRefresh
        );
    }

    #[test]
    fn static_frame_pacer_refreshes_parked_static_frame_after_notify_idle() {
        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();

        assert_eq!(
            pacer.observe(&test_frame(9, 1), 1_000_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 2),
                1_010_000,
                start + Duration::from_micros(10_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 3),
                1_020_000,
                start + Duration::from_micros(20_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 4),
                1_030_000,
                start + Duration::from_micros(30_000)
            ),
            FramePaceDecision::SkipStatic
        );

        assert!(!pacer.should_refresh_static_at(start + Duration::from_micros(900_000), 1_900_000));
        assert!(pacer.should_refresh_static_at(start + Duration::from_micros(1_020_000), 2_020_000));
        assert!(
            !pacer.should_refresh_static_at(start + Duration::from_micros(1_500_000), 2_500_000)
        );
    }

    #[test]
    fn static_frame_pump_repushes_parked_frame_after_sck_goes_silent() {
        let mut pump = StaticFramePumpState::with_dedup();
        let start = Instant::now();
        let first_capture_ts = 1_000_000;
        let mut pushed = Vec::new();

        for sequence in 1..=4 {
            let elapsed_us = (sequence - 1) * 10_000;
            let capture_ts = first_capture_ts + elapsed_us;
            let now = start + Duration::from_micros(elapsed_us);
            match pump.observe_captured_frame(test_frame(9, sequence), capture_ts, now) {
                StaticPumpFrameDecision::Push(_) => {
                    let (frame, pushed_ts) = pump.parked_frame().expect("pushed frame is parked");
                    pushed.push((frame.sequence, pushed_ts));
                }
                StaticPumpFrameDecision::SkipStatic => {}
            }
        }

        assert_eq!(pushed, vec![(1, 1_000_000), (2, 1_010_000), (3, 1_020_000)]);
        assert_eq!(
            pump.parked_frame()
                .map(|(frame, capture_ts)| (frame.sequence, capture_ts)),
            Some((4, 1_030_000))
        );

        let idle_ticks = [
            (250_000, false),
            (500_000, false),
            (750_000, false),
            (1_020_000, true),
            (1_250_000, false),
            (1_500_000, false),
            (1_750_000, false),
            (2_020_000, true),
        ];
        for (elapsed_us, should_push) in idle_ticks {
            let refresh_wall_time_us = first_capture_ts + elapsed_us;
            match pump.idle_refresh_frame_at(
                start + Duration::from_micros(elapsed_us),
                refresh_wall_time_us,
            ) {
                Some(frame) => {
                    assert!(should_push, "unexpected push at {elapsed_us}us");
                    pushed.push((frame.sequence, refresh_wall_time_us));
                }
                None => assert!(!should_push, "missed push at {elapsed_us}us"),
            }
        }

        assert_eq!(
            pushed,
            vec![
                (1, 1_000_000),
                (2, 1_010_000),
                (3, 1_020_000),
                (4, 2_020_000),
                (4, 3_020_000),
            ]
        );
        assert_eq!(
            pump.observe_captured_frame(
                test_frame(9, 5),
                3_030_000,
                start + Duration::from_micros(2_030_000),
            ),
            StaticPumpFrameDecision::SkipStatic
        );
        assert_eq!(
            pump.parked_frame()
                .map(|(frame, capture_ts)| (frame.sequence, capture_ts)),
            Some((5, 3_030_000))
        );
    }

    #[test]
    fn static_frame_pacer_refreshes_after_warmup_silence_before_skip_static() {
        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();

        assert_eq!(
            pacer.observe(&test_frame(9, 1), 1_000_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 2),
                1_010_000,
                start + Duration::from_micros(10_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(9, 3),
                1_020_000,
                start + Duration::from_micros(20_000)
            ),
            FramePaceDecision::PushWarmup
        );

        assert!(
            !pacer.should_refresh_static_at(start + Duration::from_micros(1_019_999), 2_019_999)
        );
        assert!(pacer.should_refresh_static_at(start + Duration::from_micros(1_020_000), 2_020_000));
        assert!(
            !pacer.should_refresh_static_at(start + Duration::from_micros(1_500_000), 2_500_000)
        );
        assert!(pacer.should_refresh_static_at(start + Duration::from_micros(2_020_000), 3_020_000));
    }

    #[test]
    fn static_frame_pacer_resumes_immediately_on_content_change() {
        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();

        assert_eq!(
            pacer.observe(&test_frame(1, 1), 1_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &test_frame(1, 2),
                2_000,
                start + Duration::from_micros(1_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(1, 3),
                3_000,
                start + Duration::from_micros(2_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_frame(1, 4),
                4_000,
                start + Duration::from_micros(3_000)
            ),
            FramePaceDecision::SkipStatic
        );

        assert_eq!(
            pacer.observe(
                &test_frame(2, 5),
                5_000,
                start + Duration::from_micros(4_000)
            ),
            FramePaceDecision::PushChangedAfterStatic
        );
    }

    #[test]
    fn static_frame_pacer_detects_one_row_offset_change() {
        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();

        assert_eq!(
            pacer.observe(&test_strided_frame(4, 32, 24, 1, 1, None), 1_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &test_strided_frame(4, 32, 24, 1, 2, None),
                2_000,
                start + Duration::from_micros(1_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_strided_frame(4, 32, 24, 1, 3, None),
                3_000,
                start + Duration::from_micros(2_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(
                &test_strided_frame(4, 32, 24, 1, 4, None),
                4_000,
                start + Duration::from_micros(3_000)
            ),
            FramePaceDecision::SkipStatic
        );

        assert_eq!(
            pacer.observe(
                &test_strided_frame(4, 32, 24, 1, 5, Some((7, 8, 2))),
                5_000,
                start + Duration::from_micros(4_000)
            ),
            FramePaceDecision::PushChangedAfterStatic
        );
    }

    #[test]
    fn frame_fingerprint_covers_visible_pixels_below_old_row_step() {
        let base = test_strided_frame(4, 32, 24, 3, 1, None);
        let changed = test_strided_frame(4, 32, 24, 3, 2, Some((11, 12, 4)));

        assert_ne!(frame_fingerprint(&base), frame_fingerprint(&changed));
    }

    #[test]
    fn native_frame_fingerprint_tracks_metadata_not_sequence() {
        let first = test_native_frame(
            1,
            1,
            16,
            Some(screencapturekit::cm::SCFrameStatus::Complete),
        );
        let same_metadata_next_sequence = test_native_frame(
            2,
            1,
            16,
            Some(screencapturekit::cm::SCFrameStatus::Complete),
        );
        let changed_dirty = test_native_frame(
            3,
            2,
            32,
            Some(screencapturekit::cm::SCFrameStatus::Complete),
        );
        let changed_status =
            test_native_frame(4, 1, 16, Some(screencapturekit::cm::SCFrameStatus::Idle));

        assert_eq!(
            frame_fingerprint(&first),
            frame_fingerprint(&same_metadata_next_sequence)
        );
        assert_ne!(frame_fingerprint(&first), frame_fingerprint(&changed_dirty));
        assert_ne!(
            frame_fingerprint(&first),
            frame_fingerprint(&changed_status)
        );

        let mut pacer = StaticFramePacer::with_dedup();
        let start = Instant::now();
        assert_eq!(
            pacer.observe(&first, 1_000, start),
            FramePaceDecision::PushChanged
        );
        assert_eq!(
            pacer.observe(
                &same_metadata_next_sequence,
                2_000,
                start + Duration::from_micros(1_000)
            ),
            FramePaceDecision::PushWarmup
        );
        assert_eq!(
            pacer.observe(&changed_dirty, 3_000, start + Duration::from_micros(2_000)),
            FramePaceDecision::PushChanged
        );
    }

    #[test]
    fn capture_state_derives_idle_occluded_and_cpu_timings() {
        let mut idle = test_frame(7, 1);
        idle.frame_status = Some(screencapturekit::cm::SCFrameStatus::Idle);
        idle.lock_copy_ms = 0.42;

        let idle_report = capture_state_report(&idle, false, Some(0.2));
        assert_eq!(
            idle_report.state,
            crate::diagnostics::CaptureStateKind::Idle
        );
        assert_eq!(idle_report.occlusion_pct, Some(20.0));
        assert_eq!(idle_report.cpu.lock_copy_ms, Some(0.42));

        let occluded = capture_state_report(&idle, true, Some(0.98));
        assert_eq!(
            occluded.state,
            crate::diagnostics::CaptureStateKind::Occluded
        );
        assert_eq!(occluded.occlusion_pct, Some(98.0));
    }

    #[test]
    fn capture_freeze_hash_uses_nv12_y_plane_only() {
        let base = test_nv12_frame(42, 90, 1);
        let changed_uv_only = test_nv12_frame(42, 240, 2);
        let changed_y = test_nv12_frame(43, 90, 3);

        assert_eq!(
            capture_freeze_hash(&base),
            capture_freeze_hash(&changed_uv_only)
        );
        assert_ne!(capture_freeze_hash(&base), capture_freeze_hash(&changed_y));
    }

    /// Live investigation (2026-07-08, "window 14 freezes on the viewer"):
    /// the strided `capture_freeze_hash` (~1 sampled byte per 2KB) is cheap
    /// enough for the >=30fps live on_frame diagnostic, but the snapshot-pull
    /// fallback (#183) reuses the SAME sampled byte offsets on every pull of
    /// a given window size -- so a localized content edit that never lands on
    /// a sampled offset is invisible not just for one pull, but forever while
    /// the window stays that size. This is exactly the failure mode a viewer
    /// perceives as "the shared window rarely updates": real edits (a status
    /// line, a cursor, one line of a document) happen, but never register as
    /// "changed" so nothing gets pushed. The fix: gate the pull path's push
    /// decision on `frame_fingerprint`'s dense per-row hash instead (see the
    /// snapshot-pull call site), which is only evaluated at <=10fps and can
    /// afford full sensitivity. This test pins the gap the old hash had and
    /// proves the dense fingerprint closes it.
    #[test]
    fn dense_fingerprint_catches_localized_edit_the_strided_freeze_hash_misses() {
        let width = 4096u32;
        let make_frame = |edit_byte: Option<(usize, u8)>| {
            let mut y = vec![7u8; width as usize];
            if let Some((offset, value)) = edit_byte {
                y[offset] = value;
            }
            crate::capture::CapturedFrame {
                width,
                height: 1,
                payload: crate::capture::CapturedFramePayload::Nv12 {
                    y: crate::capture::PooledFrameData::from_vec(y),
                    y_stride: width,
                    uv: crate::capture::PooledFrameData::from_vec(vec![128; width as usize / 2]),
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
                lock_copy_ms: 0.25,
                region_generation: None,
            }
        };

        let base = make_frame(None);
        // Offset 1000 is not a multiple of the strided hash's 2048-byte step,
        // so it falls entirely in the old gate's blind spot.
        let edited = make_frame(Some((1000, 200)));

        assert_eq!(
            capture_freeze_hash(&base),
            capture_freeze_hash(&edited),
            "sanity: this is the exact blind spot capture_freeze_hash has -- \
             a real edit outside the sampled grid hashes identically"
        );
        assert_ne!(
            frame_fingerprint(&base).hash,
            frame_fingerprint(&edited).hash,
            "the dense per-row fingerprint used to gate snapshot-pull pushes \
             must catch an edit the strided freeze hash misses"
        );
    }

    #[test]
    fn capture_freeze_hash_keeps_native_static_frames_detectable() {
        let base = test_native_frame(1, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Idle));
        let next_same_metadata =
            test_native_frame(2, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Idle));
        let changed_dirty =
            test_native_frame(3, 1, 32, Some(screencapturekit::cm::SCFrameStatus::Idle));
        let changed_status =
            test_native_frame(4, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Complete));

        assert_eq!(
            capture_freeze_hash(&base),
            capture_freeze_hash(&next_same_metadata)
        );
        assert_ne!(
            capture_freeze_hash(&base),
            capture_freeze_hash(&changed_dirty)
        );
        assert_ne!(
            capture_freeze_hash(&base),
            capture_freeze_hash(&changed_status)
        );
    }

    #[test]
    fn issue548_native_complete_dirty_frames_do_not_infer_idle_without_sampled_pixels() {
        let mut diag = CaptureFreezeDiag::default();

        for sequence in 1..=(CAPTURE_FREEZE_RUN_THRESHOLD + 2) {
            let frame = test_native_frame(
                sequence,
                1,
                16,
                Some(screencapturekit::cm::SCFrameStatus::Complete),
            );
            let sample = capture_freeze_sample(&frame);
            assert!(!sample.pixels_sampled);
            let frozen_now = diag.observe_sample(sample);
            assert!(!frozen_now);
            assert!(!capture_source_appears_idle(
                &frame,
                frozen_now,
                sample.pixels_sampled
            ));
            assert!(!capture_source_appears_idle(&frame, true, false));
        }
    }

    #[test]
    fn issue548_sampled_static_pixels_can_infer_idle_and_explicit_native_idle_stays_idle() {
        let mut diag = CaptureFreezeDiag::default();
        let mut sampled_static_is_idle = false;
        for sequence in 1..=(CAPTURE_FREEZE_RUN_THRESHOLD + 1) {
            let frame = test_nv12_frame(42, 90, sequence);
            let sample = capture_freeze_sample(&frame);
            assert!(sample.pixels_sampled);
            let frozen_now = diag.observe_sample(sample);
            sampled_static_is_idle =
                capture_source_appears_idle(&frame, frozen_now, sample.pixels_sampled);
        }
        assert!(sampled_static_is_idle);

        let native_idle =
            test_native_frame(99, 0, 0, Some(screencapturekit::cm::SCFrameStatus::Idle));
        let sample = capture_freeze_sample(&native_idle);
        assert!(!sample.pixels_sampled);
        assert!(capture_source_appears_idle(
            &native_idle,
            false,
            sample.pixels_sampled
        ));
    }

    #[test]
    fn issue548_changing_snapshot_clears_stale_idle_for_the_45_second_watchdog() {
        const WINDOW_ID: u32 = u32::MAX - 548;
        set_source_appears_idle(WINDOW_ID, true);

        assert!(snapshot_hash_changed(WINDOW_ID, None, 10));
        assert!(source_appears_idle(WINDOW_ID));
        assert!(!snapshot_hash_changed(WINDOW_ID, Some(10), 10));
        assert!(source_appears_idle(WINDOW_ID));

        assert!(snapshot_hash_changed(WINDOW_ID, Some(10), 11));
        assert!(!source_appears_idle(WINDOW_ID));
        assert_eq!(
            raw_capture_watchdog_decision(46_000_002, 1_000_000, source_appears_idle(WINDOW_ID)),
            RawCaptureWatchdogDecision::Stalled {
                silent_for_us: 45_000_002
            }
        );
    }

    #[test]
    fn capture_watchdog_only_stalls_when_permission_is_gone() {
        assert_eq!(
            capture_watchdog_decision(3_500_001, 1_000_000, true),
            CaptureWatchdogDecision::Healthy
        );
        assert_eq!(
            capture_watchdog_decision(3_500_001, 1_000_000, false),
            CaptureWatchdogDecision::StalledPermissionDenied
        );
        assert_eq!(
            capture_watchdog_decision(2_000_000, 1_000_000, false),
            CaptureWatchdogDecision::Healthy
        );
    }

    #[test]
    fn pump_watchdog_restarts_after_activity_stall() {
        assert_eq!(
            pump_watchdog_decision(6_999_999, 1_000_000),
            PumpWatchdogDecision::Healthy
        );
        assert_eq!(
            pump_watchdog_decision(7_000_001, 1_000_000),
            PumpWatchdogDecision::Stalled {
                silent_for_us: 6_000_001
            }
        );
    }

    #[test]
    fn snapshot_pull_paces_on_silence_and_min_interval() {
        // Not silent long enough: no pull, regardless of pull spacing.
        assert!(!snapshot_pull_decision(
            2_000_000,
            1_000_000,
            0,
            SNAPSHOT_PULL_MIN_INTERVAL_US
        ));
        // Silent past the engage threshold, previous pull old: pull.
        assert!(snapshot_pull_decision(
            3_000_000,
            1_000_000,
            0,
            SNAPSHOT_PULL_MIN_INTERVAL_US
        ));
        // Silent, but previous pull too recent (rate ceiling): no pull.
        assert!(!snapshot_pull_decision(
            3_000_000,
            1_000_000,
            2_950_000,
            SNAPSHOT_PULL_MIN_INTERVAL_US
        ));
        // Silent, previous pull just old enough: pull.
        assert!(snapshot_pull_decision(
            3_000_000,
            1_000_000,
            2_900_000,
            SNAPSHOT_PULL_MIN_INTERVAL_US
        ));
    }

    #[test]
    fn snapshot_pull_backoff_widens_the_effective_interval() {
        // Same silence/last-pull timing, but a widened (backed-off) interval
        // suppresses a pull that the normal interval would allow.
        assert!(snapshot_pull_decision(2_000_000, 0, 1_000_000, 100_000));
        assert!(!snapshot_pull_decision(2_000_000, 0, 1_000_000, 5_000_000));
    }

    #[test]
    fn interaction_snapshot_only_fires_without_a_post_input_raw_frame() {
        assert!(interaction_snapshot_decision(4, 3, 10_000, 10_000));
        assert!(!interaction_snapshot_decision(4, 3, 10_001, 10_000));
    }

    #[test]
    fn interaction_snapshot_coalesces_an_already_handled_epoch() {
        assert!(!interaction_snapshot_decision(4, 4, 0, 10_000));
        assert!(interaction_snapshot_decision(5, 4, 0, 10_000));
    }

    // -- Interaction-burst policy (#290 step 5) --------------------------

    #[test]
    fn interaction_burst_is_inactive_with_no_recorded_interaction() {
        // `last_applied_at_us == 0` means "never interacted" -- must not be
        // treated as an interaction that happened at the Unix epoch.
        assert!(!interaction_burst_is_active(1_000_000, 0));
    }

    #[test]
    fn interaction_burst_stays_active_through_a_rapid_sequence() {
        // A steady stream of keystrokes/wheel-ticks, each well inside the
        // active window of the last one, must never flap the burst
        // off in between -- every gap here is 150-300ms, comfortably under
        // the 900ms window.
        let mut last_applied_us = 0u64;
        // Start from a nonzero base -- 0 is reserved to mean "never
        // interacted" (see `interaction_burst_is_active`'s `!= 0` guard).
        let mut now_us = 1_000_000u64;
        for gap_us in [0, 180_000, 150_000, 300_000, 220_000, 260_000] {
            now_us += gap_us;
            last_applied_us = now_us; // simulates note_remote_interaction firing
            assert!(
                interaction_burst_is_active(now_us, last_applied_us),
                "expected burst active immediately after an interaction at {now_us}"
            );
        }
        // And it should still read active for a short beat after the last
        // keystroke even with no further input (a brief thinking pause).
        now_us += 500_000;
        assert!(interaction_burst_is_active(now_us, last_applied_us));
    }

    #[test]
    fn interaction_burst_lapses_after_genuine_idle() {
        let last_applied_us = 1_000_000u64;
        // Right at the edge of the window: still active.
        assert!(interaction_burst_is_active(
            last_applied_us + INTERACTION_BURST_ACTIVE_WINDOW_US,
            last_applied_us
        ));
        // One microsecond past it: inactive. A genuinely idle multi-second
        // gap must release the floor, not hold it forever.
        assert!(!interaction_burst_is_active(
            last_applied_us + INTERACTION_BURST_ACTIVE_WINDOW_US + 1,
            last_applied_us
        ));
        assert!(!interaction_burst_is_active(
            last_applied_us + 5_000_000,
            last_applied_us
        ));
    }

    /// #806 REGRESSION GUARD -- the exact live teardown.
    ///
    /// Window 5245, 2026-08-14: 358 consecutive successful snapshot pulls of a
    /// static window, all returning the same hash, and the watchdog called it
    /// wedged -- `no raw ScreenCaptureKit frames for 45.2s`, three restarts,
    /// then `pump recovery circuit open at restart_generation 3; stopping
    /// share`. A slide nobody scrolls is a normal screenshare state, not an
    /// edge case.
    #[test]
    fn a_successful_pull_keeps_a_static_source_published_806() {
        const WINDOW_ID: u32 = 806_001;
        let now = 1_000_000_000u64;

        // First pull: content moved. Not idle, and freshness recorded.
        assert!(observe_snapshot_pull(WINDOW_ID, None, 0xAAAA, now));
        assert!(!source_appears_idle(WINDOW_ID));
        assert!(snapshot_pull_fresh_within(WINDOW_ID, now, 10_000_000));

        // Every later pull succeeds and returns the SAME content. Both signals
        // the watchdog reads must survive that -- before the fix, neither did.
        // Past the watchdog's 10s freshness horizon, so the freshness
        // assertion below is load-bearing rather than coasting on the one
        // changed pull above.
        for tick in 1..=20u64 {
            let at = now + tick * 1_000_000;
            assert!(!observe_snapshot_pull(WINDOW_ID, Some(0xAAAA), 0xAAAA, at));
            assert!(
                snapshot_pull_fresh_within(WINDOW_ID, at, 10_000_000),
                "an unchanged pull still proves the capture path is alive"
            );
            assert!(
                source_appears_idle(WINDOW_ID),
                "an unchanged pull is the only idle signal a silent raw stream can give"
            );
        }

        // THE FIX, at the level that decides the restart: past the 45s
        // watchdog, a static source whose pulls keep succeeding is
        // `IdleHealthy`, not `Stalled` -- no restart, so no march toward the
        // circuit breaker. This is the 2m15s teardown.
        assert_eq!(
            raw_capture_watchdog_decision(
                46_000_000 + 1_000_000,
                1_000_000,
                source_appears_idle(WINDOW_ID)
            ),
            RawCaptureWatchdogDecision::IdleHealthy {
                silent_for_us: 46_000_000
            },
            "an unchanged-but-succeeding pull must make the source read as idle"
        );

        // The 300s absolute backstop still fires, even for an idle source: it
        // exists for the case where the idle signal is WRONG -- a DRM/hardware
        // surface that SCScreenshotManager captures as a static placeholder
        // hashes identical forever and looks idle while genuinely changing.
        assert_eq!(
            raw_capture_watchdog_decision(
                RAW_CAPTURE_HARD_RESTART_THRESHOLD_US + 2_000_000,
                1_000_000,
                source_appears_idle(WINDOW_ID)
            ),
            RawCaptureWatchdogDecision::Stalled {
                silent_for_us: RAW_CAPTURE_HARD_RESTART_THRESHOLD_US + 1_000_000
            }
        );
        assert!(
            !raw_capture_stall_hold(RAW_CAPTURE_HARD_RESTART_THRESHOLD_US + 1, true),
            "nothing may hold past the absolute safety net"
        );

        // The wedged case keeps its bound: content still moving, pull-only lag
        // is real, so the raw stream must be recovered after the grace.
        assert!(raw_capture_stall_hold(60_000_000, true));
        assert!(!raw_capture_stall_hold(200_000_000, true));
        // And a stalled PULL path is a genuine wedge either way.
        assert!(!raw_capture_stall_hold(60_000_000, false));

        set_source_appears_idle(WINDOW_ID, false);
    }

    /// #804: the ROI attempt budget must run out BEFORE the pump-recovery
    /// circuit breaker does. The live suite measured what happens otherwise:
    /// three restarts, `pump recovery circuit open at restart_generation 3;
    /// stopping share`, and every subsequent remote-control request refused
    /// for an inactive window. A budget that outlives the breaker does not
    /// fix the loop, it just renames it "dead share".
    #[test]
    fn the_roi_attempt_budget_runs_out_before_the_recovery_circuit_804() {
        assert!(
            u64::from(LAYOUT_ROI_MAX_ATTEMPTS) <= MAX_PUMP_FAILURE_RECOVERY_RESTARTS,
            "abandoning a ROI must preempt the circuit breaker's stop_share"
        );
        for attempts in 1..LAYOUT_ROI_MAX_ATTEMPTS {
            assert_eq!(
                layout_ack_failure_action(attempts),
                LayoutAckFailureAction::RestartCapture,
                "attempt {attempts} still gets a real recovery"
            );
        }
        assert_eq!(
            layout_ack_failure_action(LAYOUT_ROI_MAX_ATTEMPTS),
            LayoutAckFailureAction::AbandonRoi
        );
        assert_eq!(
            layout_ack_failure_action(LAYOUT_ROI_MAX_ATTEMPTS + 7),
            LayoutAckFailureAction::AbandonRoi
        );
    }

    /// #804: the counter has to be global because the monitor task does not
    /// survive the restart it triggers -- a per-task counter resets every
    /// cycle and can never reach a bound, which IS the livelock. It must also
    /// reset on a new target and on share teardown, so transient failures
    /// spread over a meeting never pre-poison a later share.
    #[test]
    fn layout_roi_ack_failures_count_per_target_and_reset_804() {
        let window_id = 804_001;
        clear_layout_roi_ack_failures(window_id);

        assert_eq!(record_layout_roi_ack_failure(window_id, (1278, 822)), 1);
        assert_eq!(record_layout_roi_ack_failure(window_id, (1278, 822)), 2);

        // A moving target is a live resize converging, not a livelock.
        assert_eq!(record_layout_roi_ack_failure(window_id, (960, 616)), 1);
        assert_eq!(record_layout_roi_ack_failure(window_id, (960, 616)), 2);
        assert_eq!(record_layout_roi_ack_failure(window_id, (960, 616)), 3);

        clear_layout_roi_ack_failures(window_id);
        assert_eq!(record_layout_roi_ack_failure(window_id, (960, 616)), 1);
        clear_layout_roi_ack_failures(window_id);
    }

    #[test]
    fn interaction_burst_active_for_window_tracks_the_per_window_map() {
        let window_id = 900_290;
        clear_interaction_burst_state(window_id);
        assert!(!interaction_burst_active_for_window(window_id, 1_000_000));

        mark_interaction_burst(window_id, 1_000_000);
        assert!(interaction_burst_active_for_window(window_id, 1_500_000));
        assert!(!interaction_burst_active_for_window(
            window_id,
            1_000_000 + INTERACTION_BURST_ACTIVE_WINDOW_US + 1
        ));

        clear_interaction_burst_state(window_id);
        assert!(!interaction_burst_active_for_window(window_id, 1_500_000));
    }

    #[test]
    fn interaction_burst_floor_forces_full_quality_except_data_saver() {
        assert_eq!(
            interaction_burst_floor(SharePriority::Automatic, true).map(|f| f.quality),
            Some(ShareQuality::Full)
        );
        assert_eq!(
            interaction_burst_floor(SharePriority::Responsive, true).map(|f| f.quality),
            Some(ShareQuality::Full)
        );
        assert_eq!(
            interaction_burst_floor(SharePriority::SharpText, true).map(|f| f.quality),
            Some(ShareQuality::Full)
        );
        // DataSaver's resource cap is never raised by the burst policy.
        assert_eq!(
            interaction_burst_floor(SharePriority::DataSaver, true),
            None
        );
    }

    #[test]
    fn interaction_burst_floor_is_none_when_burst_is_inactive() {
        for priority in [
            SharePriority::Automatic,
            SharePriority::Responsive,
            SharePriority::SharpText,
            SharePriority::DataSaver,
        ] {
            assert_eq!(interaction_burst_floor(priority, false), None);
        }
    }

    #[test]
    fn interaction_burst_floor_biases_resolution_by_priority() {
        // SharpText: preserve resolution, no burst-imposed cap.
        assert_eq!(
            interaction_burst_floor(SharePriority::SharpText, true)
                .and_then(|f| f.resolution_ceiling),
            None
        );
        // Automatic/Responsive: prefer trimming resolution over frame rate.
        assert_eq!(
            interaction_burst_floor(SharePriority::Automatic, true)
                .and_then(|f| f.resolution_ceiling),
            Some(CaptureResolution::P1080)
        );
        assert_eq!(
            interaction_burst_floor(SharePriority::Responsive, true)
                .and_then(|f| f.resolution_ceiling),
            Some(CaptureResolution::P1080)
        );
    }

    #[test]
    fn tighter_capture_resolution_prefers_the_smaller_explicit_cap() {
        assert_eq!(
            tighter_capture_resolution(CaptureResolution::Auto, CaptureResolution::P1080),
            CaptureResolution::P1080
        );
        assert_eq!(
            tighter_capture_resolution(CaptureResolution::P1080, CaptureResolution::Auto),
            CaptureResolution::P1080
        );
        assert_eq!(
            tighter_capture_resolution(CaptureResolution::Auto, CaptureResolution::Auto),
            CaptureResolution::Auto
        );
        assert_eq!(
            tighter_capture_resolution(CaptureResolution::P1440, CaptureResolution::P1080),
            CaptureResolution::P1080
        );
        assert_eq!(
            tighter_capture_resolution(CaptureResolution::Uhd4k, CaptureResolution::P1440),
            CaptureResolution::P1440
        );
    }

    #[test]
    fn burst_effective_capture_resolution_degrades_resolution_before_frame_rate() {
        // Resolution-before-frame-rate ordering (#290 step 5c): while a
        // burst is active, Automatic/Responsive give up resolution (down to
        // the burst ceiling) rather than letting cadence take the hit --
        // `apply_quality`'s separate floor is what protects cadence itself.
        assert_eq!(
            burst_effective_capture_resolution(
                SharePriority::Automatic,
                CaptureResolution::Auto,
                true
            ),
            CaptureResolution::P1080
        );
        // Already at/under the ceiling: unaffected.
        assert_eq!(
            burst_effective_capture_resolution(
                SharePriority::Responsive,
                CaptureResolution::P1080,
                true
            ),
            CaptureResolution::P1080
        );
        // SharpText never trims resolution for a burst.
        assert_eq!(
            burst_effective_capture_resolution(
                SharePriority::SharpText,
                CaptureResolution::Auto,
                true
            ),
            CaptureResolution::Auto
        );
        // DataSaver's cap is untouched either way.
        assert_eq!(
            burst_effective_capture_resolution(
                SharePriority::DataSaver,
                CaptureResolution::P1080,
                true
            ),
            CaptureResolution::P1080
        );
        // No burst active: steady state passes through unchanged regardless
        // of priority.
        assert_eq!(
            burst_effective_capture_resolution(
                SharePriority::Automatic,
                CaptureResolution::Auto,
                false
            ),
            CaptureResolution::Auto
        );
    }

    #[test]
    fn share_startup_applies_the_priority_cadence_floor_at_the_capture_call_site() {
        assert_eq!(startup_capture_fps(SharePriority::Automatic), 30);
        assert_eq!(startup_capture_fps(SharePriority::Responsive), 30);
        assert_eq!(startup_capture_fps(SharePriority::SharpText), 30);
        assert_eq!(startup_capture_fps(SharePriority::DataSaver), 15);

        assert_eq!(apply_startup_cadence_floor(10, 30), 30);
        assert_eq!(apply_startup_cadence_floor(30, 30), 30);
        assert_eq!(apply_startup_cadence_floor(60, 30), 60);
    }

    #[test]
    fn raw_capture_watchdog_does_not_restart_healthy_idle_source() {
        // Below threshold: healthy regardless of idle flag.
        assert_eq!(
            raw_capture_watchdog_decision(20_000_000, 1_000_000, false),
            RawCaptureWatchdogDecision::Healthy
        );
        assert_eq!(
            raw_capture_watchdog_decision(45_999_999, 1_000_000, true),
            RawCaptureWatchdogDecision::Healthy
        );
        // Past the 45s threshold with an IDLE source: do NOT restart (this is
        // the fix for the 336×-restart storm on windows that simply stopped
        // drawing) -- report IdleHealthy instead.
        assert_eq!(
            raw_capture_watchdog_decision(46_000_002, 1_000_000, true),
            RawCaptureWatchdogDecision::IdleHealthy {
                silent_for_us: 45_000_002
            }
        );
        // Past the 45s threshold with an ACTIVE source that abruptly stopped:
        // that looks wedged -> restart.
        assert_eq!(
            raw_capture_watchdog_decision(46_000_002, 1_000_000, false),
            RawCaptureWatchdogDecision::Stalled {
                silent_for_us: 45_000_002
            }
        );
        // Absolute hard-restart net: even an idle source restarts once after a
        // very long silence, in case the idle signal was wrong.
        assert_eq!(
            raw_capture_watchdog_decision(301_000_002, 1_000_000, true),
            RawCaptureWatchdogDecision::Stalled {
                silent_for_us: 300_000_002
            }
        );
    }

    #[test]
    fn pump_failure_recovery_circuit_breaker_bounds_restarts() {
        for failures in 1..=MAX_PUMP_FAILURE_RECOVERY_RESTARTS {
            assert_eq!(
                pump_failure_recovery_decision(failures as u32),
                PumpFailureRecoveryDecision::Restart,
                "failure {failures} still gets a real recovery"
            );
        }
        assert_eq!(
            pump_failure_recovery_decision(MAX_PUMP_FAILURE_RECOVERY_RESTARTS as u32 + 1),
            PumpFailureRecoveryDecision::CircuitOpen
        );
        assert_eq!(
            pump_failure_recovery_decision(u32::MAX),
            PumpFailureRecoveryDecision::CircuitOpen
        );
    }

    /// #807 REGRESSION GUARD. The breaker measured "has this share ever had
    /// trouble", so three self-healed restarts spread across a long meeting
    /// stopped the share -- and that is what turned #804's and #806's
    /// transient faults into dead shares rather than hiccups.
    #[test]
    fn the_recovery_breaker_counts_recent_failures_not_lifetime_ones_807() {
        const WINDOW_ID: u32 = 807_001;
        clear_pump_recovery_failures(WINDOW_ID);

        // A tight burst still trips it: three restarts, then open.
        let t0 = 1_000_000_000u64;
        assert_eq!(record_pump_recovery_failure(WINDOW_ID, t0), 1);
        assert_eq!(record_pump_recovery_failure(WINDOW_ID, t0 + 2_000_000), 2);
        assert_eq!(record_pump_recovery_failure(WINDOW_ID, t0 + 4_000_000), 3);
        assert_eq!(
            pump_failure_recovery_decision(record_pump_recovery_failure(WINDOW_ID, t0 + 6_000_000)),
            PumpFailureRecoveryDecision::CircuitOpen,
            "a wedge in a tight loop must still stop"
        );

        // Spread out beyond the window, the count restarts every time, so a
        // long meeting with occasional self-healed restarts keeps its share.
        clear_pump_recovery_failures(WINDOW_ID);
        let mut at = t0;
        for _ in 0..10 {
            assert_eq!(
                record_pump_recovery_failure(WINDOW_ID, at),
                1,
                "a failure a quiet window later is a fresh incident"
            );
            at += PUMP_RECOVERY_FAILURE_WINDOW_US + 1_000_000;
        }

        // And the 300s defensive restart of a static window can never
        // accumulate: its cadence is longer than the window by construction.
        assert!(
            PUMP_RECOVERY_FAILURE_WINDOW_US < RAW_CAPTURE_HARD_RESTART_THRESHOLD_US,
            "the idle backstop's cadence must reset the burst counter (#806)"
        );
        clear_pump_recovery_failures(WINDOW_ID);
    }

    #[test]
    fn metadata_publish_outcome_waits_within_budget_and_proceeds_past_it() {
        // issue #249: the source-metadata publish must not gate the media
        // publish forever. Under budget -> wait (keeps title/color ahead of the
        // track on the receiver); at/over budget -> publish the track NOW rather
        // than keep the viewer dark while a stalled signaling round-trip hangs.
        let budget = SHARE_METADATA_PUBLISH_BUDGET;
        assert_eq!(
            metadata_publish_outcome(Duration::from_millis(0), budget),
            MetadataPublishOutcome::WithinBudget
        );
        assert_eq!(
            metadata_publish_outcome(budget - Duration::from_millis(1), budget),
            MetadataPublishOutcome::WithinBudget
        );
        // Boundary: exactly at budget publishes now (>= comparison).
        assert_eq!(
            metadata_publish_outcome(budget, budget),
            MetadataPublishOutcome::ExceededBudget
        );
        // The ~30s live-session stall this fix targets: publish the track now.
        assert_eq!(
            metadata_publish_outcome(Duration::from_secs(30), budget),
            MetadataPublishOutcome::ExceededBudget
        );
    }

    #[test]
    fn metadata_publish_budget_is_shorter_than_the_first_frame_wait() {
        // The metadata budget only bounds a signaling round-trip that races an
        // already-captured first frame; it must be well under the capture
        // first-frame timeout so the honest "publishing now" path fires quickly,
        // not after the multi-attempt capture budget could itself elapse.
        assert!(SHARE_METADATA_PUBLISH_BUDGET < FIRST_FRAME_TIMEOUT * FIRST_FRAME_ATTEMPTS as u32);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn media_publish_overlaps_metadata_signaling_round_trip() {
        // Regression proof for #299: two representative signaling operations
        // must take roughly max(metadata, media), not their serial sum.
        let metadata_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let started = Instant::now();
        let (media_result, metadata_outcome, _) =
            publish_media_while_metadata_runs(metadata_task, async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok::<_, ()>(())
            })
            .await;

        let elapsed = started.elapsed();
        assert!(media_result.is_ok());
        assert_eq!(metadata_outcome, MetadataPublishOutcome::WithinBudget);
        assert!(
            elapsed < Duration::from_millis(180),
            "metadata and media were serialized: elapsed={:?}",
            elapsed
        );
    }

    #[test]
    fn raw_capture_threshold_is_longer_than_pump_activity_threshold() {
        // The whole point of this second watchdog is to be strictly more
        // patient than the pump-liveness one -- if it were shorter or equal,
        // it would just reintroduce the idle-window false-positive PR #58
        // fixed.
        assert!(RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US > PUMP_STALL_THRESHOLD_US);
        // And the hard-restart net is much longer still than the idle
        // threshold, so idle windows are re-checked (not restarted) many times
        // before any defensive restart.
        assert!(RAW_CAPTURE_HARD_RESTART_THRESHOLD_US > RAW_CAPTURE_SILENCE_RESTART_THRESHOLD_US);
    }

    #[test]
    fn republish_timeout_fires_before_pump_watchdog_restart() {
        assert!((REPUBLISH_AWAIT_TIMEOUT.as_micros() as u64) < PUMP_STALL_THRESHOLD_US);
    }

    #[test]
    fn session_model_duplicate_start_is_idempotent_and_preserves_sequence() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();

        model.start_share(10).unwrap();

        assert_eq!(model.shares.len(), 1);
        assert_eq!(model.shares[&10].started_seq, 0);
        assert_eq!(model.next_share_seq, 1);
        assert!(model.quality_updates.is_empty());
    }

    #[test]
    fn reconnect_terminal_cleanup_cannot_stop_a_stop_reshared_window() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        let stale_repair_seq = model.shares[&10].started_seq;

        model.stop_share(10);
        model.start_share(10).unwrap();
        let replacement_seq = model.shares[&10].started_seq;
        assert_ne!(replacement_seq, stale_repair_seq);

        assert!(!model.stop_share_if_started_seq(10, stale_repair_seq));
        assert_eq!(model.shares[&10].started_seq, replacement_seq);
        assert_eq!(model.focused_window(), Some(10));
    }

    #[test]
    fn post_unpublish_stop_reshare_skips_stale_title_metadata_cleanup() {
        let original_started_seq = 7;
        let reshared_started_seq = 8;

        // This is the exact post-await seam in stop_share_with_started_seq:
        // old track unpublish completed, but a new share of the same window
        // appeared before title metadata cleanup.
        assert!(!stopped_share_metadata_cleanup_is_current(
            true,
            true,
            Some(reshared_started_seq),
        ));
        assert!(!stopped_share_metadata_cleanup_is_current(
            true,
            true,
            Some(original_started_seq),
        ));
        assert!(stopped_share_metadata_cleanup_is_current(true, true, None));
    }

    #[test]
    fn reconnect_terminal_effects_skip_an_interleaved_reshare() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        let stale_repair_seq = model.shares[&10].started_seq;

        // The old repair removed its matching generation, then the user
        // shared the same native window again before terminal UI/control
        // effects could run. Those old effects must be skipped.
        assert!(model.stop_share_if_started_seq(10, stale_repair_seq));
        model.start_share(10).unwrap();
        assert_ne!(model.shares[&10].started_seq, stale_repair_seq);
        assert!(!model.apply_terminal_effects_if_generation_current(10, stale_repair_seq));
        assert_eq!(model.focused_window(), Some(10));
    }

    #[test]
    fn stop_reshare_stop_tombstone_rejects_old_terminal_effects() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        let old_seq = model.shares[&10].started_seq;
        model.stop_share(10);

        model.start_share(10).unwrap();
        let new_seq = model.shares[&10].started_seq;
        model.stop_share(10);

        assert_ne!(old_seq, new_seq);
        // This is the actual old-terminal interleaving: old stop → re-share
        // → new stop, then the old repair finally tries to clear/revoke.
        assert!(!model.apply_terminal_effects_if_generation_current(10, old_seq));
        assert!(model.terminal_reconnect_effect_windows.is_empty());
        assert!(model.apply_terminal_effects_if_generation_current(10, new_seq));
        assert_eq!(model.terminal_reconnect_effect_windows, vec![10]);
    }

    #[test]
    fn stopped_generation_tombstone_is_monotonic_per_window() {
        let mut tombstones = HashMap::new();
        record_last_stopped_share_generation(&mut tombstones, 10, 8);
        record_last_stopped_share_generation(&mut tombstones, 10, 7);

        assert_eq!(tombstones.get(&10), Some(&8));
    }

    #[test]
    fn stale_disconnect_or_new_reconnect_epoch_cannot_mutate_old_share_or_effects() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        let stale_repair_seq = model.shares[&10].started_seq;

        // The old repair's RoomGeneration/epoch was invalidated by a
        // disconnect or a newer Reconnected event before its terminal path.
        let stale_lifecycle_is_current = reconnect_repair_lifecycle_is_current(false, false, true);
        assert!(!model.apply_reconnect_terminal_failure(
            stale_lifecycle_is_current,
            10,
            stale_repair_seq,
        ));
        assert_eq!(model.shares[&10].started_seq, stale_repair_seq);
        assert!(model.terminal_reconnect_effect_windows.is_empty());
    }

    #[test]
    fn in_flight_reconnect_publish_invalidation_cleans_new_without_swap_or_capture_mutation() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        let before = model.shares[&10];
        let replacement = RepublishTarget {
            width: 1280,
            height: 720,
            quality: ShareQuality::Full,
            resolution: CaptureResolution::Auto,
        };

        // A disconnect, newer reconnect epoch, or leave invalidated the
        // repair while publish_window_at was in flight. The just-published
        // track is cleanup-only; old slot/capture ownership is unchanged.
        assert!(!model.apply_reconnect_republish_after_publish(false, 10, replacement));
        assert_eq!(model.shares[&10], before);
        assert!(model.republish_updates.is_empty());
    }

    #[test]
    fn post_swap_reconnect_invalidation_reports_committed_deferred_cleanup() {
        assert_eq!(
            reconnect_republish_invalidation_outcome(true),
            RepublishOutcome::ReplacedWithOldCleanupDeferred
        );
        assert_eq!(
            reconnect_republish_invalidation_outcome(false),
            RepublishOutcome::Cancelled
        );
    }

    #[test]
    fn deferred_post_swap_cleanup_reclaims_old_publication_without_touching_replacement_slot() {
        let mut model = DeferredTrackCleanupModel {
            replacement_slot_is_active: true,
            obsolete_publication_is_live: true,
            cleanup_attempts: 0,
        };

        assert_eq!(
            reconnect_republish_invalidation_outcome(true),
            RepublishOutcome::ReplacedWithOldCleanupDeferred
        );
        model.reclaim_obsolete_publication();

        assert!(model.replacement_slot_is_active);
        assert!(!model.obsolete_publication_is_live);
        assert_eq!(model.cleanup_attempts, 1);
    }

    #[test]
    fn ordinary_committed_swap_schedules_old_track_cleanup_after_timeout_or_error() {
        let mut model = DeferredTrackCleanupModel {
            replacement_slot_is_active: true,
            obsolete_publication_is_live: true,
            cleanup_attempts: 0,
        };

        // The new track/capture swap already committed. The first old-track
        // unpublish attempt timed out or errored, so only that captured old
        // track is retried; the replacement must not be rolled back.
        assert!(committed_republish_needs_deferred_old_cleanup(true, false));
        model.reclaim_obsolete_publication();

        assert!(model.replacement_slot_is_active);
        assert!(!model.obsolete_publication_is_live);
        assert_eq!(model.cleanup_attempts, 1);
        assert!(!committed_republish_needs_deferred_old_cleanup(true, true));
        assert!(!committed_republish_needs_deferred_old_cleanup(
            false, false
        ));
    }

    #[test]
    fn failed_old_cleanup_is_scheduled_before_lifecycle_or_intent_invalidation() {
        let mut lifecycle_model = DeferredTrackCleanupModel {
            replacement_slot_is_active: true,
            obsolete_publication_is_live: true,
            cleanup_attempts: 0,
        };

        // The old unpublish await fails, cleanup is scheduled, then the
        // reconnect lifecycle invalidates before the repair reports success.
        assert!(committed_republish_needs_deferred_old_cleanup(true, false));
        lifecycle_model.reclaim_obsolete_publication();
        assert_eq!(
            reconnect_republish_post_old_cleanup_early_outcome(true, false, true),
            Some(RepublishOutcome::ReplacedWithOldCleanupDeferred)
        );
        assert!(lifecycle_model.replacement_slot_is_active);
        assert!(!lifecycle_model.obsolete_publication_is_live);
        assert_eq!(lifecycle_model.cleanup_attempts, 1);

        // Intent supersession follows the same scheduling boundary but is not
        // reported as this repair's success.
        assert_eq!(
            reconnect_republish_post_old_cleanup_early_outcome(true, true, false),
            Some(RepublishOutcome::Cancelled)
        );
    }

    #[test]
    fn post_capture_scale_supersession_schedules_old_cleanup_without_rolling_back_replacement() {
        let mut model = DeferredTrackCleanupModel {
            replacement_slot_is_active: true,
            obsolete_publication_is_live: true,
            cleanup_attempts: 0,
        };

        // The replacement is committed and the capture-scale await just
        // completed. A newer republish intent wins, so the old captured track
        // is deferred for cleanup while the replacement remains authoritative.
        assert_eq!(
            reconnect_republish_post_capture_scale_early_outcome(true, false),
            Some(RepublishOutcome::Cancelled)
        );
        model.reclaim_obsolete_publication();

        assert!(model.replacement_slot_is_active);
        assert!(!model.obsolete_publication_is_live);
        assert_eq!(model.cleanup_attempts, 1);
    }

    #[test]
    fn session_model_in_place_restart_preserves_focus_sequence_and_remote_control() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        model.start_share(20).unwrap();
        model.remote_control_windows.insert(10);
        model.quality_updates.clear();

        let seq = model.shares[&10].started_seq;
        let generation = model.shares[&10].restart_generation;
        assert!(model.restart_capture_in_place(10, seq, generation));

        assert_eq!(model.shares[&10].started_seq, seq);
        assert_eq!(model.shares[&10].restart_generation, generation + 1);
        assert_eq!(model.next_share_seq, 2);
        assert_eq!(model.focused_window(), Some(20));
        assert_eq!(model.last_toggled_window, Some(20));
        assert!(model.remote_control_windows.contains(&10));
        assert!(model.quality_updates.is_empty());
    }

    #[test]
    fn session_model_stale_in_place_restart_generation_is_rejected() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();

        let seq = model.shares[&10].started_seq;
        assert!(model.restart_capture_in_place(10, seq, 0));
        assert!(!model.restart_capture_in_place(10, seq, 0));
        assert_eq!(model.shares[&10].restart_generation, 1);
    }

    #[test]
    fn session_model_leave_clears_active_shares_but_not_last_toggle_memory() {
        let mut model = SessionModel::default();
        model.join();
        model.start_share(10).unwrap();
        model.start_share(20).unwrap();

        model.leave();

        assert!(!model.joined);
        assert!(model.shares.is_empty());
        assert_eq!(model.focused_window(), None);
        assert_eq!(model.last_toggled_window, Some(20));
    }

    #[test]
    fn resize_debounce_ignores_matching_published_size() {
        let mut debounce = ResizeDebounce::default();
        assert_eq!(
            debounce.observe(800, 600, 900, 700),
            ResizeDecision::WaitingForStableSize {
                width: 900,
                height: 700,
                frames: 1
            }
        );
        assert_eq!(
            debounce.observe(800, 600, 800, 600),
            ResizeDecision::MatchingPublishedSize
        );
        assert_eq!(
            debounce.observe(800, 600, 900, 700),
            ResizeDecision::WaitingForStableSize {
                width: 900,
                height: 700,
                frames: 1
            }
        );
    }

    #[test]
    fn resize_debounce_requires_repeated_same_size() {
        let mut debounce = ResizeDebounce::default();
        assert_eq!(
            debounce.observe(800, 600, 900, 700),
            ResizeDecision::WaitingForStableSize {
                width: 900,
                height: 700,
                frames: 1
            }
        );
        assert_eq!(
            debounce.observe(800, 600, 901, 701),
            ResizeDecision::WaitingForStableSize {
                width: 901,
                height: 701,
                frames: 1
            }
        );
        for frames in 2..RESIZE_REPUBLISH_STABLE_FRAMES {
            assert_eq!(
                debounce.observe(800, 600, 901, 701),
                ResizeDecision::WaitingForStableSize {
                    width: 901,
                    height: 701,
                    frames
                }
            );
        }
        assert_eq!(
            debounce.observe(800, 600, 901, 701),
            ResizeDecision::StableResize {
                width: 901,
                height: 701
            }
        );
    }

    // #714: `resize_pump_action` is not just a pure-function assertion on
    // its own inputs/outputs -- it is the ACTUAL call the real pump loop
    // makes (see the `if resize_pump_action(resize_decision) ==
    // ResizePumpAction::SkipThisFrame { continue; }` line right before the
    // `match resize_decision` block in the pump task) to decide whether to
    // drop a captured frame. A regression that reintroduces a
    // frame-skipping `continue` for `WaitingForStableSize` inside that
    // match arm would NOT be caught by this test (the match arms no longer
    // gate on `continue` at all, by design -- `resize_pump_action` is now
    // the single gate), but reverting `resize_pump_action` itself to map
    // `WaitingForStableSize` back to `SkipThisFrame` -- the actual shape
    // the pre-#714 bug took -- breaks this test immediately, at the exact
    // call site the pump loop uses, not a parallel helper the pump loop
    // could silently drift away from.
    #[test]
    fn resize_pump_action_never_skips_a_frame() {
        for decision in [
            ResizeDecision::MatchingPublishedSize,
            ResizeDecision::WaitingForStableSize {
                width: 900,
                height: 700,
                frames: 1,
            },
            ResizeDecision::StableResize {
                width: 900,
                height: 700,
            },
        ] {
            assert_eq!(
                resize_pump_action(decision),
                ResizePumpAction::Push,
                "{decision:?} must not skip pushing a frame (#714: a resize in progress must never black out the receiver)"
            );
        }
    }

    // #714: drives a REALISTIC continuous drag-resize -- a live window drag
    // rarely lands on the exact same size twice in a row, so the debounce
    // candidate keeps resetting to `frames: 1` and `StableResize` never
    // fires for the whole gesture. Before #714 this was precisely the
    // freeze: `WaitingForStableSize` mapped to a `continue` in the pump
    // loop, so a drag that never held still produced zero pushed frames for
    // its entire duration -- long enough, live, to trip the 6s
    // frame-pump-stall watchdog and force a capture restart (the Sentry
    // event this issue is filed from). Assert every single frame in the
    // sequence is a push, not just the debounce's own state transitions.
    #[test]
    fn continuous_drag_resize_never_produces_a_skipped_frame() {
        let mut debounce = ResizeDebounce::default();
        let published = (1920u32, 1080u32);
        // A slow, continuous drag: the captured size changes on every
        // frame and never repeats, so `frames` never reaches
        // RESIZE_REPUBLISH_STABLE_FRAMES and `StableResize` never fires.
        let mut pushed = 0u32;
        for step in 0..200u32 {
            let width = published.0 - step; // shrinking every frame
            let height = published.1 - (step / 2);
            let decision = debounce.observe(published.0, published.1, width, height);
            assert_ne!(
                decision,
                ResizeDecision::StableResize { width, height },
                "size changes every frame in this drag -- it must never be seen as stable"
            );
            assert_eq!(
                resize_pump_action(decision),
                ResizePumpAction::Push,
                "frame {step} ({width}x{height}) must be pushed, not skipped -- a long \
                 drag with no frames reaching the receiver is exactly the #714 freeze"
            );
            pushed += 1;
        }
        assert_eq!(pushed, 200, "every frame of the drag must have been pushed");
    }

    // Cheap defense-in-depth ONLY -- per CLAUDE.md's native-window-lifecycle
    // rule this pure-function assertion is explicitly NOT sufficient
    // evidence on its own (it proves nothing about whether
    // `spawn_pump_failure_recovery`'s real restart call site actually
    // invokes this function with the right input). The two tests below it
    // are what carry that burden; this just catches the cheapest possible
    // regression (the match arms swapped) for free, everywhere, without
    // needing a live display.
    #[test]
    fn direct_for_kind_matches_window_and_display_variants() {
        assert!(matches!(
            ShareCaptureSource::direct_for_kind(SharedSourceKind::Window),
            ShareCaptureSource::DirectWindowId
        ));
        assert!(matches!(
            ShareCaptureSource::direct_for_kind(SharedSourceKind::Display),
            ShareCaptureSource::DirectDisplayId
        ));
    }

    // #712 Fable follow-up (non-blocking title paper-cut): a display's
    // fallback title must read "Screen <raw id>", never "Window <tagged
    // id>" -- the pure formatting decision `fallback_source_title` makes.
    #[test]
    fn fallback_source_title_labels_display_and_window_ids_distinctly() {
        let raw_display_id = 42u32;
        let tagged_display_id = crate::window_source::display_source_id(raw_display_id);
        assert_eq!(
            fallback_source_title(tagged_display_id),
            format!("Screen {raw_display_id}")
        );
        let window_id = 4242u32;
        assert_eq!(
            fallback_source_title(window_id),
            format!("Window {window_id}")
        );
    }

    // -- #712: display-share in-place-restart source selection ---------------
    //
    // CLAUDE.md's native-window-lifecycle rule: a unit test on
    // `ShareCaptureSource::direct_for_kind` alone would prove that pure
    // branch logic is right without proving anything about the real
    // production restart path that depends on it. These tests instead drive
    // `start_capture_for_share` -- the exact async function
    // `spawn_pump_failure_recovery` calls at its restart boundary (`Ok(..)`
    // means the share survives in place; `Err(..)` there is what triggers
    // full teardown) -- not a reimplemented copy of it, against REAL
    // ScreenCaptureKit (`SCShareableContent`), using whatever real display(s)
    // this machine has. `start_capture_for_share` takes no `AppHandle`/
    // `SessionState`/`RoomConnection`, so it is reachable directly without
    // reimplementing the tauri/LiveKit plumbing `spawn_pump_failure_recovery`
    // wraps around it -- that surrounding plumbing is a straight-line
    // `snapshot.source_kind` passthrough (see `ShareCaptureSource::
    // direct_for_kind` call site above) reviewed by inspection, not
    // reimplemented logic these tests would need to duplicate.
    //
    // Needs live Screen Recording permission for the TEST HARNESS process
    // (not the shipped app's dev binary -- a different code identity, see
    // CLAUDE.md's "keeps its grant" note) and a real attached display, so
    // both tests skip with a clear message rather than false-passing when
    // that's unavailable (e.g. a headless/sandboxed `cargo test` run). They
    // exercise the real mechanism on a machine that has both, which is what
    // `scripts/ci-local.sh` actually runs on.
    fn first_real_display_id() -> Option<u32> {
        if !crate::window_source::has_screen_recording_access() {
            return None;
        }
        let content = screencapturekit::shareable_content::SCShareableContent::create()
            .with_on_screen_windows_only(true)
            .with_exclude_desktop_windows(true)
            .get()
            .ok()?;
        content
            .displays()
            .first()
            .map(|display| display.display_id())
    }

    #[test]
    fn display_share_restart_survives_via_direct_display_id_not_window_lookup() {
        let Some(display_id) = first_real_display_id() else {
            eprintln!(
                "skipping display_share_restart_survives_via_direct_display_id_not_window_lookup: \
                 no Screen Recording permission / no display available to this test process"
            );
            return;
        };
        // #712 Fable follow-up: production NEVER stores a raw `CGDirectDisplayID`
        // in `ActiveShare::window_id` for a display share -- the custom picker's
        // display path (`hover_tab::toggle_display_share_from_picker`) always
        // stores the TAGGED source id (`DISPLAY_SOURCE_MARKER |
        // CGDirectDisplayID`, see `window_source::display_source_id`), and both
        // restart call sites thread that same tagged value through unchanged.
        // The first version of this test fed `start_capture_for_share` a raw id
        // directly, which passed even with the decode missing entirely (its own
        // `first_real_display_id()` never goes through the tagged encoding), so
        // it shipped green while the real production path stayed broken. Feed
        // the TAGGED id here to actually exercise the boundary the fix touches.
        let tagged_display_id = crate::window_source::display_source_id(display_id);

        tauri::async_runtime::block_on(async {
            // The #712 FIX: the restart path now decodes the tagged id back to
            // a raw `CGDirectDisplayID` before resolving via
            // `content.displays()`, so a live, healthy display share survives
            // the exact call `spawn_pump_failure_recovery` makes with the exact
            // value it actually holds (tagged, not raw).
            let fixed = start_capture_for_share(
                tagged_display_id,
                ShareCaptureSource::DirectDisplayId,
                5,
                CaptureResolution::default(),
                "test_capture_restart_display",
                None,
            )
            .await;
            assert!(
                fixed.is_ok(),
                "a live display share must survive an in-place restart via DirectDisplayId \
                 (display {display_id}, tagged {tagged_display_id:#x}): {:?}",
                fixed.err()
            );
            drop(fixed); // WindowCapture::Drop stops the real SCStream (module doc comment).

            // The #712 BUG, reproduced live against real ScreenCaptureKit:
            // the OLD unconditional `DirectWindowId` restart path searches
            // ONLY `content.windows()`, which can never contain a display
            // id (tagged or raw), so it must still fail with WindowNotFound.
            // If `ShareCaptureSource::direct_for_kind` ever regresses back to
            // always returning `DirectWindowId`, this assertion turns that
            // regression into a same-run failure instead of a silent live
            // incident (mutation-checked below).
            let broken = start_capture_for_share(
                tagged_display_id,
                ShareCaptureSource::DirectWindowId,
                5,
                CaptureResolution::default(),
                "test_capture_restart_display_wrong_path",
                None,
            )
            .await;
            let broken_err = broken.as_ref().err();
            assert!(
                matches!(broken, Err(ShareSessionError::WindowNotFound(id)) if id == tagged_display_id),
                "sanity check: a display id must never resolve via the window-only lookup \
                 (got {broken_err:?}) -- if this stops failing, the reproduction no longer \
                 demonstrates the #712 defect"
            );
        });
    }

    #[test]
    fn display_share_restart_still_tears_down_a_genuinely_disconnected_display() {
        if !crate::window_source::has_screen_recording_access() {
            eprintln!(
                "skipping display_share_restart_still_tears_down_a_genuinely_disconnected_display: \
                 no Screen Recording permission available to this test process"
            );
            return;
        }
        // #637's original policy must survive #712's fix unweakened: a
        // display id that is genuinely NOT in `content.displays()` is real
        // loss, not a false negative from having searched the wrong list.
        // u32::MAX is not a valid CGDirectDisplayID.
        let bogus_display_id = u32::MAX;
        // #712 Fable follow-up: production stores the TAGGED source id in
        // `ActiveShare::window_id`, never the raw display id -- feed the
        // tagged form here too (matching the sibling `..._survives_..`
        // test above), and assert against the id the fix actually reports:
        // the raw, decoded id `prepare_direct_display_source` compares
        // against `content.displays()`. Derive both via the same
        // `window_source` helpers production uses, not hand-derived bit
        // math that could silently drift from the real encoding.
        let tagged_bogus_display_id = crate::window_source::display_source_id(bogus_display_id);
        let expected_raw_display_id =
            crate::window_source::display_id_from_source_id(tagged_bogus_display_id);
        tauri::async_runtime::block_on(async {
            let result = start_capture_for_share(
                tagged_bogus_display_id,
                ShareCaptureSource::DirectDisplayId,
                5,
                CaptureResolution::default(),
                "test_capture_restart_display_gone",
                None,
            )
            .await;
            let result_err = result.as_ref().err();
            assert!(
                matches!(result, Err(ShareSessionError::DisplayNotFound(id)) if id == expected_raw_display_id),
                "a genuinely-disconnected display id must still tear down as DisplayNotFound \
                 (#637's policy, extended correctly to displays by #712), got {result_err:?}"
            );
        });
    }
}
