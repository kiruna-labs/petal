//! Remote keyboard/mouse control for native-shared windows.
//!
//! This rides the same LiveKit data-channel connection as telepointers, but
//! stays separate because these messages have side effects on the sharer's
//! machine and need stricter routing, ordering, and permission checks.
//!
//! Trust model (GitHub issue #30): "trust every authenticated meeting peer,
//! gated by host-side checks." Sender identity is authenticated (not
//! spoofable), a control Request is rejected unless the requester is a current
//! room participant, and disabling RC revokes all active controllers
//! immediately. What is NOT enforced client-side: strict viewer-only-of-that-
//! window authorization (needs LiveKit per-track subscription state, not
//! exposed here — see the "strict viewer-only" note in the Request handler) and
//! per-topic publish ACLs. Full rationale + operational consequences (invites
//! are sensitive; RC is on by default): docs/remote-control-trust-model.md.

use crate::sync_ext::MutexExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use livekit::StreamReader;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::platform::cg::WindowFrame;
use crate::remote_clipboard;
use crate::session::{RoomGeneration, SessionState, SharedWindowScreenStatus};
use crate::transport::publisher::RoomConnection;

use crate::remote_control_core::*;
struct MacPlatformControl;

impl PlatformControl for MacPlatformControl {
    fn accessibility_trusted(&self) -> bool {
        input::accessibility_trusted()
    }

    fn prompt_accessibility(&self) -> bool {
        input::prompt_accessibility()
    }

    fn replay(
        &self,
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
    ) -> Result<(), String> {
        input::replay(message, frame, target_pid)
    }

    fn clear_cached_app(&self, pid: i32) {
        input::clear_cached_ax_app_for_pid(pid);
    }

    fn clear_resolution_cache(&self, window_id: u32) {
        input::clear_ax_resolution_cache_for_window(window_id);
    }

    fn clear_window_gestures(&self, window_id: u32) {
        input::clear_ax_gesture_for_window(window_id);
    }

    fn clear_controller_gestures(&self, window_id: u32, controller_id: &str) {
        input::clear_ax_gesture_for_controller(window_id, controller_id);
    }

    fn clear_all_control_state(&self) {
        input::clear_all_ax_control_state_except_sl_drag();
    }

    fn release_window_gestures(&self, window_id: u32) {
        input::release_session_tap_gestures_for_window(window_id);
    }
}

static PLATFORM_CONTROL: MacPlatformControl = MacPlatformControl;

fn platform_control() -> &'static dyn PlatformControl {
    &PLATFORM_CONTROL
}

fn release_platform_window_gestures(window_id: u32) {
    platform_control().release_window_gestures(window_id);
}

#[cfg(target_os = "macos")]
fn remote_window_exists(owner_identity: &str, window_id: u32) -> bool {
    crate::compositor::has_window_for_owner(owner_identity, window_id)
}

#[cfg(target_os = "windows")]
fn remote_window_exists(owner_identity: &str, window_id: u32) -> bool {
    crate::windows_compositor::remote_control_window_exists(window_id, Some(owner_identity))
}

#[cfg(target_os = "macos")]
fn remote_window_owner(window_id: u32, owner_identity: Option<&str>) -> Option<String> {
    crate::compositor::owner_identity_for_window(window_id, owner_identity)
}

#[cfg(target_os = "windows")]
fn remote_window_owner(window_id: u32, owner_identity: Option<&str>) -> Option<String> {
    crate::windows_compositor::remote_control_target_metadata(window_id, owner_identity)
        .map(|target| target.owner_identity)
}

#[cfg(target_os = "macos")]
fn set_remote_window_control_active(
    app: &AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
    active: bool,
) -> Result<(), String> {
    crate::compositor::set_remote_control_active(app, window_id, owner_identity, active)
}

#[cfg(target_os = "windows")]
fn set_remote_window_control_active(
    app: &AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
    active: bool,
) -> Result<(), String> {
    crate::windows_compositor::set_remote_control_active(app, window_id, owner_identity, active)
}

struct TauriControlSurface<'a> {
    app: &'a AppHandle,
}

impl ControlSurface for TauriControlSurface<'_> {
    fn emit_status(&self, status: RemoteControlStatus) {
        let _ = self.app.emit("remote-control-status", status);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GlobalPoint {
    pub x: f64,
    pub y: f64,
}

/// #369: which replay shard a task belongs to. Every real resolved-input path
/// (`resolve_one_task`) sets a real, positive `target_pid` before enqueuing,
/// so `Unknown` should not normally be hit -- it exists only so a task that
/// somehow reaches replay with no resolved pid cannot panic or vanish; it
/// also doubles as the overflow bucket once `MAX_DEDICATED_REPLAY_SHARDS` is
/// reached (see `shard_sender_locked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReplayShardKey {
    Pid(i32),
    Unknown,
}

impl ReplayShardKey {
    fn for_task(task: &ReplayTask) -> Self {
        match task.target_pid {
            Some(pid) if pid > 0 => Self::Pid(pid),
            _ => Self::Unknown,
        }
    }
}

fn active_injection_keys() -> &'static Mutex<HashSet<ReplayShardKey>> {
    static ACTIVE: OnceLock<Mutex<HashSet<ReplayShardKey>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

struct ReplayShard {
    sender: mpsc::SyncSender<ReplayTask>,
}

/// #369: the actual "do the OS-level injection" call, behind an `Arc<dyn Fn>`
/// seam instead of calling `input::replay` directly so tests can substitute a
/// controllable fake (e.g. one that blocks until released) to exercise
/// sharding/deadline behavior without touching real Accessibility/CGEvent
/// APIs. Production always uses `production_replay_injector`.
type ReplayInjector = Arc<
    dyn Fn(&RemoteControlMessage, WindowFrame, Option<i32>) -> Result<(), String> + Send + Sync,
>;

fn production_replay_injector() -> &'static ReplayInjector {
    static INJECTOR: OnceLock<ReplayInjector> = OnceLock::new();
    INJECTOR.get_or_init(|| {
        Arc::new(
            |message: &RemoteControlMessage, frame: WindowFrame, target_pid: Option<i32>| {
                platform_control().replay(message, frame, target_pid)
            },
        )
    })
}

thread_local! {
    /// Fable-review fix (#369): set for the duration of one injection thread's
    /// `inject(...)` call (see `run_replay_with_deadline`) so AX/SL mutation
    /// sites deep in `mod input` can check whether the deadline waiter gave up
    /// on this event, WITHOUT threading a cancellation parameter through the
    /// entire AX call graph. This works because each replay event's injection
    /// runs on its own freshly spawned thread (`run_replay_with_deadline`), so
    /// a thread-local is naturally scoped to exactly one event's injection and
    /// never leaks across events or shards.
    static INJECTION_CANCELLED: std::cell::RefCell<Option<Arc<AtomicBool>>> =
        std::cell::RefCell::new(None);
}

/// Returns true if the currently-running injection was abandoned by its
/// deadline waiter (`run_replay_with_deadline` timed out and stopped
/// waiting). Mutation sites in `mod input` that have an observable side
/// effect on the target app or on shared gesture state (gesture-map
/// insert/remove, AX press/selection actions, SkyLight/CGEvent posts) MUST
/// check this immediately before performing that side effect and bail out
/// (typically returning `PassThrough` without acting) if true. Without this,
/// an abandoned-but-still-running injection thread can complete its side
/// effect long after its event was reported as dropped -- e.g. inserting a
/// gesture-map entry for a Down whose matching Up already ran and found
/// nothing, leaving a stale entry a LATER, unrelated gesture's Up then
/// mistakenly acts on.
pub(crate) fn injection_was_cancelled() -> bool {
    INJECTION_CANCELLED.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|flag| flag.load(Ordering::Acquire))
            .unwrap_or(false)
    })
}

/// Test-only: force `injection_was_cancelled()` to read `true`/`false` on the
/// CURRENT thread without going through `run_replay_with_deadline`'s spawn
/// path, so tests can exercise the sink-dispatch guard in
/// `replay_with_backends` directly. Always clears afterward via the returned
/// guard's `Drop` so it can never leak into an unrelated later test.
#[cfg(test)]
pub(crate) struct InjectionCancelledForTests;

#[cfg(test)]
impl InjectionCancelledForTests {
    pub(crate) fn set() -> Self {
        INJECTION_CANCELLED.with(|cell| *cell.borrow_mut() = Some(Arc::new(AtomicBool::new(true))));
        Self
    }
}

#[cfg(test)]
impl Drop for InjectionCancelledForTests {
    fn drop(&mut self) {
        INJECTION_CANCELLED.with(|cell| *cell.borrow_mut() = None);
    }
}

#[derive(Debug, Clone)]
struct ResolveTask {
    message: RemoteControlMessage,
    local_identity: String,
    admission: Option<DiscreteAdmission>,
    result_sender: Option<TerminalResultSender>,
}

/// Best-effort correlation for an invalid v2 envelope. We only emit a terminal
/// malformed result when all fields needed for the controller to match it are
/// present; otherwise the packet is safely dropped without a side effect.
fn malformed_v2_admission(message: &RemoteControlMessage) -> Option<DiscreteAdmission> {
    Some(DiscreteAdmission {
        controller_id: message.controller_id.clone(),
        window_id: message.window_id,
        target_kind: message.target_kind,
        share_instance_id: message.share_instance_id.clone(),
        control_session_id: message.control_session_id.clone()?,
        input_id: message.input_id.clone()?,
        input_seq: message.input_seq?,
        operation_fingerprint: message.operation_fingerprint.clone()?,
    })
}

/// Decodes a binary hot-path frame AND verifies its carried grant-token
/// fingerprint against the CURRENTLY active session for
/// `(window_id, controller_id)` (#370 corrective pass -- closes the bug
/// where the original 23-byte frame had no grant material at all and every
/// binary packet fell into the former tokenless compatibility path
/// unconditionally, for every hot-path packet, forever). A missing session or
/// a fingerprint mismatch rejects the WHOLE packet (`None`) -- it must never
/// fall through to the legacy JSON path. On success the decoded message carries
/// the REAL active
/// grant token (not the fingerprint) in `grant_token`, so it flows through
/// the normal `is_authorized_input` check exactly like a JSON packet would.
fn message_from_binary(
    payload: &[u8],
    target_user_id: &str,
    controller_id: &str,
) -> Option<RemoteControlMessage> {
    if payload.len() != BINARY_FRAME_LEN || payload[0] != BINARY_MAGIC || payload[1] != VERSION {
        return None;
    }
    let read_u16 = |at: usize| u16::from_le_bytes([payload[at], payload[at + 1]]);
    let read_i16 = |at: usize| i16::from_le_bytes([payload[at], payload[at + 1]]);
    let kind = match payload[2] {
        4 => RemoteControlType::Pointer,
        5 => RemoteControlType::Wheel,
        _ => RemoteControlType::Unknown,
    };
    let action = match payload[3] {
        1 => Some(RemoteControlAction::Move),
        0 => None,
        _ => Some(RemoteControlAction::Unknown),
    };
    let modifiers = payload[17];
    let window_id = u32::from_le_bytes(payload[8..12].try_into().ok()?);
    let carried_fingerprint = u32::from_le_bytes(payload[23..27].try_into().ok()?);
    let active_token = active_grant_token(window_id, controller_id)?;
    if fnv1a32(active_token.as_bytes()) != carried_fingerprint {
        log::debug!(
            "remote-control: dropping binary hot-path frame from '{controller_id}' for window {window_id} -- grant token fingerprint mismatch"
        );
        return None;
    }
    Some(RemoteControlMessage {
        v: payload[1],
        message_type: kind,
        action,
        target_user_id: target_user_id.to_string(),
        controller_id: controller_id.to_string(),
        window_id,
        seq: u32::from_le_bytes(payload[4..8].try_into().ok()?) as u64,
        target_kind: None,
        share_instance_id: None,
        controller_capabilities: Vec::new(),
        host_capabilities: Vec::new(),
        reason: None,
        control_session_id: None,
        input_id: None,
        input_seq: None,
        operation_fingerprint_version: None,
        operation_fingerprint: None,
        outcome: None,
        delivery_route: None,
        failure_code: None,
        result_capability: None,
        x: Some(read_u16(12) as f64 / u16::MAX as f64),
        y: Some(read_u16(14) as f64 / u16::MAX as f64),
        button: None,
        buttons: Some(payload[16] as u16),
        click_count: None,
        delta_x: Some(read_i16(18) as f64),
        delta_y: Some(read_i16(20) as f64),
        delta_mode: Some(payload[22]),
        key: None,
        code: None,
        repeat: false,
        location: None,
        text: None,
        status: None,
        message: None,
        grant_token: Some(active_token),
        supports_binary_hot_path: false,
        modifiers: RemoteControlModifiers {
            alt: modifiers & 1 != 0,
            ctrl: modifiers & 2 != 0,
            meta: modifiers & 4 != 0,
            shift: modifiers & 8 != 0,
        },
    })
}

type ResolveQueuePush = BoundedQueuePush<ResolveTask>;

struct ResolveQueue(BoundedCoalescingQueue<ResolveTask, ReplayCoalesceKey>);

impl ResolveQueue {
    fn new(high_rate_capacity: usize) -> Self {
        Self(BoundedCoalescingQueue::new(high_rate_capacity))
    }

    fn push(&self, task: ResolveTask) -> ResolveQueuePush {
        let key = replay_coalesce_key(&task.message);
        self.0.push(task, key)
    }

    fn pop(&self) -> ResolveTask {
        self.0.pop()
    }

    #[cfg(test)]
    fn try_pop(&self) -> Option<ResolveTask> {
        self.0.try_pop()
    }
}

#[derive(Debug, Clone)]
struct CachedControlFrame {
    frame: WindowFrame,
    cached_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedTargetPid {
    pid: i32,
    cached_at: Instant,
}

// Platform orchestration remains process-global, but all portable session,
// reliability, sequencing, and held-input state is owned by
// `remote_control_core::RemoteControlEngine`.
// #370 corrective pass: controller-side capability memory, keyed by
// (window_id, target_user_id) where target_user_id is the HOST being
// controlled. Populated only when a `status: "active"` packet FROM that host
// carries `supportsBinaryHotPath: true` (see the `RemoteControlType::Status`
// arm of `handle_message`); consulted by `publish_message` before it will
// even attempt `binary_frame_for` on an outbound pointer/wheel packet. An
// entry is insert-only for the lifetime of the process -- a host that has
// ever advertised the capability for a given window/target pair does not
// need to re-advertise it every status packet, and downgrading mid-session
// is not a real scenario (hosts do not un-ship code).
// Refs #288: v2 exactly-once admission/idempotency cache, keyed off the same
// per-(window, controller) grant token #377 already mints/rotates/revokes via
// `CONTROL_SESSIONS` -- there is deliberately no separate grant/session-token
// map here (see removed `ControlGrant`/`CONTROL_GRANTS`); admission entries
// carry the grant token as their `control_session_id` so a re-grant naturally
// starts a fresh admission namespace without any extra state to keep in sync.
static TARGET_PID_CACHE: OnceLock<Mutex<HashMap<u32, CachedTargetPid>>> = OnceLock::new();
static CONTROL_FRAME_CACHE: OnceLock<Mutex<HashMap<u32, CachedControlFrame>>> = OnceLock::new();
// #374: keyed per (window_id, controller_id), not just window_id — a single
// controller revoking/disconnecting must invalidate only ITS own queued
// replay tasks, never a concurrent controller's still-in-flight ones on the
// same window.
//
// Fable nit (deliberate, not a leak to "fix"): entries are never removed for
// a departed controller (unlike e.g. `warned_tokenless_inputs`). Removing one
// would be unsafe: `replay_task` creates a fresh entry via `or_insert(0)` at
// enqueue time, so a task enqueued by a NEW controller immediately after an
// old one's entry was removed would again start at epoch 0 and could match a
// stale/abandoned task's epoch, replaying it after the old controller was
// meant to be fully invalidated. Retention is small (~50 bytes/controller)
// and bounded by realistic controller-identity churn.
static RESOLVE_QUEUE: OnceLock<Arc<ResolveQueue>> = OnceLock::new();
// #369: replay used to be one process-global queue/worker thread (like
// RESOLVE_QUEUE still is). It is now sharded per target pid -- see
// `REPLAY_SHARDS` below -- so a hung/slow app owning one shared window can no
// longer head-of-line-block replay for other shared windows whose owning
// process is healthy. The resolver stays a single global queue/thread: it
// only touches in-memory caches (`fresh_control_frame`, `target_pid_for_window`)
// and never calls into the target app itself, so it cannot block on a hung
// app the way replay (which runs the real AX sequence) can.
static REPLAY_SHARDS: OnceLock<Mutex<HashMap<ReplayShardKey, ReplayShard>>> = OnceLock::new();
// #374 nit: still window-keyed (not per-controller) — with concurrent
// controllers, the last one to inject before the frame callback consumes this
// wins, so the `inject_to_frame_ms` diagnostic can mis-attribute to the wrong
// controller. Log-only; no behavior depends on this value.
static INPUT_LATENCY_MARKERS: OnceLock<Mutex<HashMap<u32, InputLatencyMarker>>> = OnceLock::new();
static PRESSED_TTL_SWEEPER_STARTED: AtomicBool = AtomicBool::new(false);
static RESOLVE_HIGH_RATE_DROPS: AtomicU32 = AtomicU32::new(0);
static REPLAY_HIGH_RATE_DROPS: AtomicU32 = AtomicU32::new(0);
// #372: the replay worker is a bare background thread with no AppHandle of
// its own (see REPLAY_QUEUE above) -- this is the one piece of context it
// needs to nack a failed injection back to the controller. Set once per
// `start_receiver_for_room` call; last-joined-room wins, matching the rest of
// this file's process-global state model.
static REPLAY_STATUS_CONTEXT: OnceLock<Mutex<Option<(AppHandle, String)>>> = OnceLock::new();
static REPLAY_FAILURE_STATUS_THROTTLE: OnceLock<Mutex<HashMap<(u32, String), Instant>>> =
    OnceLock::new();
static LATENCY_SUMMARY_STATE: OnceLock<Mutex<LatencySummaryState>> = OnceLock::new();

const HELD_INPUT_SWEEP_INTERVAL: Duration = Duration::from_millis(250);
// #369: this is a backstop only. The real invalidation path is immediate --
// `share_border.rs`'s 100ms border tracker and `telepointer.rs`'s ~110ms
// (`FRAME_REFRESH_TICKS` @ ~9Hz) sender-loop both call `update_control_frame`/
// `invalidate_control_frame` the moment they observe a shared window's frame
// change, well under one tick. This TTL only matters if a window moves/
// resizes without either poll observing it (e.g. between ticks, or a
// codepath that doesn't go through either poller) -- 5s let a stale frame
// survive far too long in that gap; 1s keeps the backstop tight while still
// well above either poll's cadence (so it won't itself cause premature
// eviction of a frame the pollers haven't had a chance to refresh yet).
const CONTROL_FRAME_CACHE_TTL: Duration = Duration::from_secs(1);
const TARGET_PID_CACHE_TTL: Duration = Duration::from_millis(250);
const RESOLVE_QUEUE_CAPACITY: usize = 256;
const REPLAY_QUEUE_CAPACITY: usize = 256;
const RESOLVE_DROP_WARN_EVERY: u32 = 64;
const REPLAY_DROP_WARN_EVERY: u32 = 64;
// #369: small bounded pool -- Petal targets 3-8 person meetings, so the
// number of distinct simultaneously-controlled target processes is naturally
// small. Once at the cap, additional distinct pids share one overflow shard
// rather than spawning unboundedly many OS threads; this only matters for a
// pathological many-distinct-pid scenario far outside the normal use case.
const MAX_DEDICATED_REPLAY_SHARDS: usize = 16;
// #369: a shard with no work for this long tears down its worker thread
// (reaped) rather than idling forever -- a window that was shared+controlled
// once earlier in a long meeting shouldn't keep a thread alive for the rest
// of it. Well above the ~100-110ms frame-invalidation poll cadences so a
// shard isn't reaped and respawned by ordinary gaps between events.
const REPLAY_SHARD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
// #369: soft ceiling on how long the full AX sequence of one replay event may
// run before its shard abandons waiting on it and moves on to the next
// queued event. Individual AX IPC calls are already capped at
// `AX_APP_MESSAGING_TIMEOUT_SECONDS` (0.35s), but one event can legally chain
// several such calls with no bound on the total -- this bounds the tail.
// "Soft" because a blocking AX/ObjC call cannot be safely cancelled
// mid-flight from another thread; see `run_replay_with_deadline`.
const REPLAY_EVENT_DEADLINE: Duration = Duration::from_millis(500);
pub(crate) const MAX_REPLAY_TEXT_CHARS: usize = 1000;
const MAX_REPLAY_TEXT_SLICE_CHARS: usize = 32;
const ACTIVATION_PUBLISH_ATTEMPTS: usize = 5;
const ACTIVATION_RETRY_DELAY: Duration = Duration::from_millis(120);
const CONTROLLER_REQUEST_TIMEOUT_MS: u64 = 8_000;
const CONTROLLER_REQUEST_TIMEOUT_MESSAGE: &str =
    "Remote control request timed out. Check Accessibility on the shared Mac, then try again.";
// #372: cap injection-failure status packets to ~1/sec per (window,
// controller) so a sustained AX/injection failure (e.g. every pointer-move
// event failing) doesn't spam the controller with a status packet per event.
const REPLAY_FAILURE_STATUS_MIN_INTERVAL: Duration = Duration::from_secs(1);
// #372: piggyback the periodic latency summary on the existing TTL sweeper
// tick (HELD_INPUT_SWEEP_INTERVAL=250ms) instead of a new thread; 120 ticks
// == 30s.
const LATENCY_SUMMARY_TICK_INTERVAL: u32 = 120;
const LATENCY_SUMMARY_RING_CAPACITY: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteClipboardCopyRequest {
    v: u8,
    target_user_id: String,
    controller_id: String,
    window_id: u32,
    seq: u64,
    grant_token: String,
    kind: String,
    operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_kind: Option<RemoteControlTargetKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    share_instance_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardShortcut {
    Copy,
    Paste,
}

#[derive(Debug)]
struct InputLatencyMarker {
    summary: String,
    injected_at_ms: u64,
}

/// #372: fixed-capacity ring of recent successful-injection `elapsed_ms`
/// samples plus event counters, drained into one `remote-control-summary:`
/// log line every `LATENCY_SUMMARY_TICK_INTERVAL` sweeper ticks. Bounded
/// capacity + no per-event allocation (the ring is preallocated and reused;
/// the only allocation is the small sorted copy made once per ~30s tick).
#[derive(Debug, Default)]
struct LatencySummaryState {
    samples: VecDeque<u64>,
    success_count: u64,
    failure_count: u64,
}

impl LatencySummaryState {
    fn record_success(&mut self, elapsed_ms: u64) {
        if self.samples.len() >= LATENCY_SUMMARY_RING_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(elapsed_ms);
        self.success_count += 1;
    }

    fn record_failure(&mut self) {
        self.failure_count += 1;
    }

    /// (p50, p95, max, success_count, failure_count) since the last call;
    /// `None` if nothing was recorded. Resets all counters and the ring.
    fn take_summary(&mut self) -> Option<(u64, u64, u64, u64, u64)> {
        if self.success_count == 0 && self.failure_count == 0 {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let (p50, p95, max) = if sorted.is_empty() {
            (0, 0, 0)
        } else {
            (
                percentile(&sorted, 0.50),
                percentile(&sorted, 0.95),
                *sorted.last().expect("checked non-empty above"),
            )
        };
        let summary = (p50, p95, max, self.success_count, self.failure_count);
        self.samples.clear();
        self.success_count = 0;
        self.failure_count = 0;
        Some(summary)
    }
}

/// Nearest-rank percentile over an already-sorted, non-empty slice.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteControlInputDropReason {
    Permission,
    Auth,
    StaleSeq,
    OffScreen,
    TargetUnavailable,
    /// #369: the full AX sequence for one replay event exceeded
    /// `REPLAY_EVENT_DEADLINE` and was abandoned (the shard stopped waiting
    /// and moved on to its next queued event).
    InjectionTimeout,
}

/// Synchronous facts needed to decide whether an inbound input packet may
/// advance to the resolve worker.  Keeping this snapshot free of AppHandle,
/// LiveKit, and queue state gives the v2 rejection path deterministic tests
/// without changing the real resolve/replay workers.
#[derive(Debug, Clone, Copy)]
struct InputGateSnapshot {
    remote_control_allowed: bool,
    authorized: bool,
    unreliable_seq_accepted: bool,
    accessibility_trusted: bool,
}

#[derive(Debug, Clone)]
enum InputV2DispatchSnapshot {
    /// The first pass must not touch global admission state: disabled,
    /// unauthorized, and accessibility-denied packets are terminal before
    /// admission.  The production adapter resolves this only after the gate
    /// action explicitly asks it to.
    Pending {
        correlatable_admission: Option<DiscreteAdmission>,
    },
    Legacy,
    Malformed {
        correlatable_admission: Option<DiscreteAdmission>,
    },
    Valid {
        admission: DiscreteAdmission,
        grant_current: bool,
        decision: Option<AdmissionDecision>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputDispatchStatus {
    Disabled,
    AccessibilityDenied,
}

#[derive(Debug, Clone)]
enum InputDispatchAction {
    Drop,
    EvaluateV2Admission,
    Reject {
        reason: RemoteControlInputDropReason,
        detail: &'static str,
        revoke_control: bool,
        status: Option<InputDispatchStatus>,
        terminal: Option<(DiscreteAdmission, TerminalDisposition)>,
    },
    EnqueueResolve {
        admission: Option<DiscreteAdmission>,
    },
}

/// The input arm's synchronous plan.  This is deliberately only the
/// admission/gating boundary: the production adapter still enqueues the real
/// resolve task, and the existing replay worker remains the sole owner of
/// replay and applied/replayFailed terminal outcomes.
fn plan_input_dispatch(
    gates: InputGateSnapshot,
    v2: InputV2DispatchSnapshot,
) -> InputDispatchAction {
    let early_admission = match &v2 {
        InputV2DispatchSnapshot::Pending {
            correlatable_admission,
        }
        | InputV2DispatchSnapshot::Malformed {
            correlatable_admission,
        } => correlatable_admission.clone(),
        InputV2DispatchSnapshot::Valid { admission, .. } => Some(admission.clone()),
        InputV2DispatchSnapshot::Legacy => None,
    };

    if !gates.remote_control_allowed {
        return InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "host-disabled-control",
            revoke_control: true,
            status: Some(InputDispatchStatus::Disabled),
            terminal: early_admission.map(|admission| {
                (
                    admission,
                    TerminalDisposition::failure(
                        "unauthorized",
                        RemoteControlDeliveryRoute::Admission,
                        RemoteControlFailureCode::Unauthorized,
                    ),
                )
            }),
        };
    }
    if !gates.authorized {
        return InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "no-active-request",
            revoke_control: false,
            status: None,
            terminal: early_admission.map(|admission| {
                (
                    admission,
                    TerminalDisposition::failure(
                        "unauthorized",
                        RemoteControlDeliveryRoute::Admission,
                        RemoteControlFailureCode::Unauthorized,
                    ),
                )
            }),
        };
    }
    if !gates.unreliable_seq_accepted {
        return InputDispatchAction::Drop;
    }
    if !gates.accessibility_trusted {
        return InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Permission,
            detail: "accessibility-trusted-false",
            revoke_control: false,
            status: Some(InputDispatchStatus::AccessibilityDenied),
            terminal: early_admission.map(|admission| {
                (
                    admission,
                    TerminalDisposition::failure(
                        "accessibilityDenied",
                        RemoteControlDeliveryRoute::Admission,
                        RemoteControlFailureCode::AccessibilityDenied,
                    ),
                )
            }),
        };
    }

    match v2 {
        InputV2DispatchSnapshot::Pending { .. } => InputDispatchAction::EvaluateV2Admission,
        InputV2DispatchSnapshot::Legacy => InputDispatchAction::EnqueueResolve { admission: None },
        InputV2DispatchSnapshot::Malformed {
            correlatable_admission,
        } => InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "incomplete-v2-admission-envelope",
            revoke_control: false,
            status: None,
            terminal: correlatable_admission.map(|admission| {
                (
                    admission,
                    TerminalDisposition::failure(
                        "malformed",
                        RemoteControlDeliveryRoute::Admission,
                        RemoteControlFailureCode::Malformed,
                    ),
                )
            }),
        },
        InputV2DispatchSnapshot::Valid {
            admission,
            grant_current: false,
            ..
        } => InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "v2-grant-expired",
            revoke_control: false,
            status: None,
            terminal: Some((
                admission,
                TerminalDisposition::failure(
                    "grantExpired",
                    RemoteControlDeliveryRoute::Admission,
                    RemoteControlFailureCode::GrantExpired,
                ),
            )),
        },
        InputV2DispatchSnapshot::Valid {
            admission,
            grant_current: true,
            decision: Some(AdmissionDecision::Admitted),
        } => InputDispatchAction::EnqueueResolve {
            admission: Some(admission),
        },
        InputV2DispatchSnapshot::Valid {
            admission: _,
            grant_current: true,
            decision: Some(AdmissionDecision::InFlightDuplicate),
        } => InputDispatchAction::Drop,
        InputV2DispatchSnapshot::Valid {
            admission,
            grant_current: true,
            decision: Some(AdmissionDecision::CompletedDuplicate(outcome)),
        } => InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "completed-v2-duplicate",
            revoke_control: false,
            status: None,
            terminal: Some((admission, outcome)),
        },
        InputV2DispatchSnapshot::Valid {
            admission,
            grant_current: true,
            decision: Some(AdmissionDecision::Malformed),
        } => InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "malformed-v2-fingerprint",
            revoke_control: false,
            status: None,
            terminal: Some((
                admission,
                TerminalDisposition::failure(
                    "malformed",
                    RemoteControlDeliveryRoute::Admission,
                    RemoteControlFailureCode::Malformed,
                ),
            )),
        },
        InputV2DispatchSnapshot::Valid {
            admission,
            grant_current: true,
            decision: Some(AdmissionDecision::Overloaded),
        } => InputDispatchAction::Reject {
            reason: RemoteControlInputDropReason::Auth,
            detail: "v2-admission-overloaded",
            revoke_control: false,
            status: None,
            terminal: Some((
                admission,
                TerminalDisposition::failure(
                    "admissionOverloaded",
                    RemoteControlDeliveryRoute::Admission,
                    RemoteControlFailureCode::AdmissionOverloaded,
                ),
            )),
        },
        InputV2DispatchSnapshot::Valid { .. } => {
            unreachable!("current v2 grant must carry an admission decision")
        }
    }
}

fn input_v2_snapshot_before_admission(message: &RemoteControlMessage) -> InputV2DispatchSnapshot {
    InputV2DispatchSnapshot::Pending {
        correlatable_admission: malformed_v2_admission(message),
    }
}

fn resolve_input_v2_snapshot(message: &RemoteControlMessage) -> InputV2DispatchSnapshot {
    match v2_discrete_admission(message) {
        Some(admission) => {
            let grant_current = grant_is_current(&admission, Instant::now());
            let decision = grant_current
                .then(|| admit_discrete_operation(message, &admission, Instant::now()));
            InputV2DispatchSnapshot::Valid {
                admission,
                grant_current,
                decision,
            }
        }
        None if is_v2_discrete_attempt(message) => InputV2DispatchSnapshot::Malformed {
            correlatable_admission: malformed_v2_admission(message),
        },
        None => InputV2DispatchSnapshot::Legacy,
    }
}

impl RemoteControlInputDropReason {
    fn as_log_label(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Auth => "auth",
            Self::StaleSeq => "stale-seq",
            Self::OffScreen => "off-screen",
            Self::TargetUnavailable => "target-unavailable",
            Self::InjectionTimeout => "injection-timeout",
        }
    }
}

fn sessions() -> &'static Mutex<HashMap<ControlGrantKey, String>> {
    remote_control_engine().sessions()
}

fn warned_tokenless_inputs() -> &'static Mutex<HashSet<(u32, String)>> {
    remote_control_engine().warned_tokenless_inputs()
}

fn hot_path_capable_targets() -> &'static Mutex<HashSet<(u32, String)>> {
    remote_control_engine().hot_path_capable_targets()
}

fn new_grant_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable for remote-control grant token");
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    token
}

fn discrete_admissions() -> &'static Mutex<DiscreteAdmissionState> {
    remote_control_engine().discrete_admissions()
}

/// Refs #288: departure-path cleanup for the v2 admission cache, mirroring
/// the #410 pattern used for the other per-(window, controller) maps in this
/// file (`revoke`/`revoke_controller`/`revoke_all`/`drain_window_control`).
/// Without this, a departed controller's admission/overload entries would
/// only ever be pruned lazily -- `prune_discrete_admissions` runs solely
/// inside `admit_discrete_operation`/`admission_is_still_inflight`, both
/// triggered by a LATER admission attempt. A controller that revokes and
/// never sends another v2 op leaves its entries with no future trigger to
/// prune them at all, the same insert-only leak class #410 fixed elsewhere.
/// `retain` returns true to KEEP an entry (mirrors `HashMap::retain`).
fn clear_discrete_admissions(retain: impl FnMut(u32, &str) -> bool) {
    remote_control_engine().clear_discrete_admissions(retain);
}

fn result_capability() -> RemoteControlResultCapability {
    RemoteControlResultCapability {
        version: 2,
        retry_enabled: RESULT_RETRY_ENABLED,
        retry_deadline_ms: 0,
        dedup_guarantee_window_ms: DISCRETE_OVERLOAD_WINDOW.as_millis() as u64,
    }
}

/// Refs #288: v2 admission is scoped by the SAME per-(window, controller)
/// grant token #377 already mints in `authorize_shared`/`sessions()` and
/// rotates on every re-grant -- there is no separate v2 grant/session-token
/// map (see the removed `ControlGrant`/`CONTROL_GRANTS`, which would have
/// duplicated this exact state with its own, divergent TTL). A `Request`
/// handler wanting a `control_session_id` for the "active" status packet
/// should read `active_grant_token` directly instead of minting a second
/// token.
fn grant_is_current(admission: &DiscreteAdmission, _now: Instant) -> bool {
    remote_control_engine().grant_is_current(admission)
}

fn admit_discrete_operation(
    message: &RemoteControlMessage,
    admission: &DiscreteAdmission,
    now: Instant,
) -> AdmissionDecision {
    remote_control_engine().admit_discrete_operation(message, admission, now)
}

fn complete_discrete_operation(
    admission: &DiscreteAdmission,
    disposition: TerminalDisposition,
) -> bool {
    remote_control_engine().complete_discrete_operation(admission, disposition)
}

fn admission_is_still_inflight(admission: &DiscreteAdmission, now: Instant) -> bool {
    remote_control_engine().admission_is_still_inflight(admission, now)
}

fn controller_pointer_positions() -> &'static Mutex<HashMap<(u32, String), (f64, f64)>> {
    remote_control_engine().controller_pointer_positions()
}

fn last_emitted_statuses() -> &'static Mutex<HashMap<(u32, String), &'static str>> {
    remote_control_engine().last_emitted_statuses()
}

fn warned_controller_id_mismatches() -> &'static Mutex<HashSet<(u32, String)>> {
    remote_control_engine().warned_controller_id_mismatches()
}

fn last_unreliable_seqs() -> &'static Mutex<HashMap<(u32, String, UnreliableSeqStream), u64>> {
    remote_control_engine().last_unreliable_seqs()
}

fn pressed_inputs() -> &'static Mutex<HashMap<(u32, String), PressedInputs>> {
    remote_control_engine().pressed_inputs()
}

fn target_pid_cache() -> &'static Mutex<HashMap<u32, CachedTargetPid>> {
    TARGET_PID_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn control_frame_cache() -> &'static Mutex<HashMap<u32, CachedControlFrame>> {
    CONTROL_FRAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn input_latency_markers() -> &'static Mutex<HashMap<u32, InputLatencyMarker>> {
    INPUT_LATENCY_MARKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Shared by the AX-revoked-mid-hold gate and the window-`Closed` resolve arm
/// (#372) -- drains any inputs the departing controller was holding down for
/// this window and enqueues their synthetic releases, so a stuck
/// button/modifier doesn't outlive the session that can no longer service it.
fn drain_and_release_pressed(window_id: u32, controller_id: &str, reason: &str) {
    let releases = drain_pressed_for_controller(window_id, controller_id);
    enqueue_synthetic_releases(releases, reason);
}

fn replay_status_context_store() -> &'static Mutex<Option<(AppHandle, String)>> {
    REPLAY_STATUS_CONTEXT.get_or_init(|| Mutex::new(None))
}

/// Recorded once per `start_receiver_for_room` so the replay worker thread
/// (which has no `AppHandle` of its own) can nack a failed injection back to
/// the controller (#372).
fn set_replay_status_context(app: AppHandle, local_identity: String) {
    *replay_status_context_store().lock_unpoisoned() = Some((app, local_identity));
}

fn replay_status_context() -> Option<(AppHandle, String)> {
    replay_status_context_store().lock_unpoisoned().clone()
}

fn replay_failure_status_throttle() -> &'static Mutex<HashMap<(u32, String), Instant>> {
    REPLAY_FAILURE_STATUS_THROTTLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fable-review fix (#372): this map is insert-only otherwise -- an entry for
/// a departed controller must be dropped here at every place a controller's
/// (window, controller) state is torn down (revoke, displacement, participant
/// disconnect, disable), matching the same cleanup this file already does for
/// `warned_tokenless_inputs` (see its own "Fable F2" comment). Without this,
/// a long-running host with many distinct controller identities cycling
/// through a window leaks one entry per departed identity for the process
/// lifetime.
fn clear_replay_failure_status_throttle(window_id: u32, controller_id: &str) {
    replay_failure_status_throttle()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()));
}

/// At most one replay-failure status per `REPLAY_FAILURE_STATUS_MIN_INTERVAL`
/// per (window, controller). `now` is a parameter (rather than read inside)
/// so tests can drive it without sleeping, matching `drain_expired_pressed`.
fn should_emit_replay_failure_status(window_id: u32, controller_id: &str, now: Instant) -> bool {
    let key = (window_id, controller_id.to_string());
    let mut last = replay_failure_status_throttle().lock_unpoisoned();
    if let Some(previous) = last.get(&key) {
        if now.saturating_duration_since(*previous) < REPLAY_FAILURE_STATUS_MIN_INTERVAL {
            return false;
        }
    }
    last.insert(key, now);
    true
}

fn latency_summary_state() -> &'static Mutex<LatencySummaryState> {
    LATENCY_SUMMARY_STATE.get_or_init(|| Mutex::new(LatencySummaryState::default()))
}

fn record_latency_summary_success(elapsed_ms: u64) {
    latency_summary_state()
        .lock_unpoisoned()
        .record_success(elapsed_ms);
}

fn record_latency_summary_failure() {
    latency_summary_state().lock_unpoisoned().record_failure();
}

fn log_latency_summary_tick() {
    let summary = latency_summary_state().lock_unpoisoned().take_summary();
    let Some((p50, p95, max, success_count, failure_count)) = summary else {
        return;
    };
    log::info!(
        "remote-control-summary: elapsed_ms_p50={p50} elapsed_ms_p95={p95} elapsed_ms_max={max} success_count={success_count} failure_count={failure_count} replay_high_rate_drops={} resolve_high_rate_drops={}",
        REPLAY_HIGH_RATE_DROPS.load(Ordering::Relaxed),
        RESOLVE_HIGH_RATE_DROPS.load(Ordering::Relaxed),
    );
}

/// Maps a replay failure's error string (see `input::ax_error_outcome` /
/// `accessibility_revoked_error`) to the status class the controller UI
/// already renders (`remoteControlFeedback.ts`) -- no new status kind (#372).
fn replay_failure_status_kind(error: &str) -> (&'static str, String) {
    if error.starts_with("accessibilityDenied") {
        return (
            "accessibilityDenied",
            "Petal needs Accessibility permission to replay remote input.".to_string(),
        );
    }
    #[cfg(target_os = "windows")]
    {
        let (status, message) = controller_result_feedback(Some(replay_failure_code(error)))
            .unwrap_or(("requestFailed", "Remote input was not accepted."));
        (status, message.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        (
            "targetUnavailable",
            "Remote control input could not be delivered to the shared window.".to_string(),
        )
    }
}

/// #372: on a replay failure the controller previously heard nothing and
/// kept typing into a dead session. Throttled (~1/sec/controller) status nack
/// reusing the existing status pipeline; no-ops if no room context is set
/// (e.g. in tests) or no room is currently joined.
fn should_notify_replay_failure_status(message: &RemoteControlMessage) -> bool {
    // Capable discrete operations already return a correlated terminal Result.
    // Sending a session Status first would demote the still-valid grant and
    // make that Result uncorrelatable on the controller.
    !has_complete_v2_operation_envelope(message)
}

fn emit_and_send_operation_feedback(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    _message: &RemoteControlMessage,
    status: RemoteControlStatus,
) {
    #[cfg(target_os = "windows")]
    {
        if active_grant_token(_message.window_id, &_message.controller_id).is_none()
            || !should_notify_replay_failure_status(_message)
            || !should_emit_replay_failure_status(
                _message.window_id,
                &_message.controller_id,
                Instant::now(),
            )
        {
            return;
        }
    }
    emit_and_send_status(app, state, local_identity, status);
}

fn notify_replay_failure(message: &RemoteControlMessage, error: &str) {
    if !should_notify_replay_failure_status(message) {
        return;
    }
    // Fable review fix (#372): a failed injection is frequently a SYNTHETIC
    // release for a controller that just got revoked/displaced/disconnected
    // (synthetic releases bypass the epoch guard so they still fire after
    // revoke -- see is_current_replay_epoch). Without this check, a
    // departed controller could receive a failure nack (flipping its chip to
    // "Needs access"/"Unavailable") AFTER its own terminal "stopped" status,
    // and clobber `last_emitted_statuses` for a controller no longer holding
    // a grant at all. Only nack a controller that currently holds one.
    if active_grant_token(message.window_id, &message.controller_id).is_none() {
        return;
    }
    if !should_emit_replay_failure_status(message.window_id, &message.controller_id, Instant::now())
    {
        return;
    }
    let Some((app, local_identity)) = replay_status_context() else {
        return;
    };
    let Some(state) = app.try_state::<SessionState>() else {
        return;
    };
    let (status, status_message) = replay_failure_status_kind(error);
    emit_and_send_status(
        &app,
        state.inner(),
        &local_identity,
        RemoteControlStatus {
            window_id: message.window_id,
            owner_identity: None,
            controller_id: message.controller_id.clone(),
            status,
            message: status_message,
            grant_token: None,
            reason: None,
        },
    );
}

fn classify_input_drop_reason(
    remote_control_allowed: bool,
    authorized: bool,
    unreliable_seq_accepted: bool,
    accessibility_trusted: bool,
) -> Option<RemoteControlInputDropReason> {
    if !remote_control_allowed || !authorized {
        Some(RemoteControlInputDropReason::Auth)
    } else if !unreliable_seq_accepted {
        Some(RemoteControlInputDropReason::StaleSeq)
    } else if !accessibility_trusted {
        Some(RemoteControlInputDropReason::Permission)
    } else {
        None
    }
}

fn classify_resolve_drop_reason(status: SharedWindowScreenStatus) -> RemoteControlInputDropReason {
    match status {
        SharedWindowScreenStatus::OffScreen => RemoteControlInputDropReason::OffScreen,
        SharedWindowScreenStatus::Closed
        | SharedWindowScreenStatus::NotShared
        | SharedWindowScreenStatus::OnScreen(_) => RemoteControlInputDropReason::TargetUnavailable,
    }
}

fn log_input_drop(
    message: &RemoteControlMessage,
    reason: RemoteControlInputDropReason,
    detail: &str,
) {
    if !should_log_message(message.message_type, message.action, message.seq) {
        return;
    }
    let label = reason.as_log_label();
    match reason {
        RemoteControlInputDropReason::Permission
        | RemoteControlInputDropReason::InjectionTimeout => {
            log::warn!(
                "remote-control: dropping input reason={reason} detail={detail} {}",
                message_summary(message),
                reason = label
            )
        }
        _ => log::info!(
            "remote-control: dropping input reason={reason} detail={detail} {}",
            message_summary(message),
            reason = label
        ),
    }
}

fn replay_epoch(window_id: u32, controller_id: &str) -> u64 {
    remote_control_engine().replay_epoch(window_id, controller_id)
}

fn bump_replay_epoch(window_id: u32, controller_id: &str, reason: &str) {
    remote_control_engine().bump_replay_epoch(window_id, controller_id, reason);
}

/// #374: window-wide invalidation (e.g. sharing ended) legitimately bumps
/// every controller's epoch for this window at once — unlike a single
/// controller's own revoke/disconnect, which must only bump its own.
fn bump_replay_epoch_for_window(window_id: u32, reason: &str) {
    remote_control_engine().bump_replay_epoch_for_window(window_id, reason);
}

fn bump_all_replay_epochs(reason: &str) {
    remote_control_engine().bump_all_replay_epochs(reason);
}

fn is_current_replay_epoch(task: &ReplayTask) -> bool {
    remote_control_engine().is_current_replay_epoch(task)
}

fn clear_control_caches_for_window(window_id: u32) {
    remote_clipboard::clear_pending_copy_for(window_id, None);
    if let Some(pid) = target_pid_cache()
        .lock_unpoisoned()
        .get(&window_id)
        .map(|cached| cached.pid)
    {
        platform_control().clear_cached_app(pid);
    }
    platform_control().clear_window_gestures(window_id);
    target_pid_cache().lock_unpoisoned().remove(&window_id);
    control_frame_cache().lock_unpoisoned().remove(&window_id);
    input_latency_markers().lock_unpoisoned().remove(&window_id);
}

pub(crate) fn invalidate_control_frame(window_id: u32) {
    control_frame_cache().lock_unpoisoned().remove(&window_id);
    platform_control().clear_resolution_cache(window_id);
}

pub(crate) fn update_control_frame(window_id: u32, frame: WindowFrame) {
    control_frame_cache().lock_unpoisoned().insert(
        window_id,
        CachedControlFrame {
            frame,
            cached_at: Instant::now(),
        },
    );
    platform_control().clear_resolution_cache(window_id);
}

fn clear_all_control_caches() {
    remote_clipboard::clear_pending_copy();
    remote_clipboard::clear_copy_operations();
    remote_clipboard::clear_paste_operations();
    platform_control().clear_all_control_state();
    target_pid_cache().lock_unpoisoned().clear();
    control_frame_cache().lock_unpoisoned().clear();
    input_latency_markers().lock_unpoisoned().clear();
}

pub(crate) fn take_input_latency_marker(window_id: u32) -> Option<(String, u64)> {
    input_latency_markers()
        .lock_unpoisoned()
        .remove(&window_id)
        .map(|marker| (marker.summary, marker.injected_at_ms))
}

fn record_input_latency_marker(message: &RemoteControlMessage, injected_at_ms: u64) {
    if !should_log_latency_probe(message) {
        return;
    }
    input_latency_markers().lock_unpoisoned().insert(
        message.window_id,
        InputLatencyMarker {
            summary: message_summary(message),
            injected_at_ms,
        },
    );
}

fn should_emit_status(status: &RemoteControlStatus) -> bool {
    let key = (status.window_id, status.controller_id.clone());
    let mut last_statuses = last_emitted_statuses().lock_unpoisoned();
    if last_statuses.get(&key) == Some(&status.status) {
        return false;
    }
    last_statuses.insert(key, status.status);
    true
}

/// Transient operation feedback is every status EXCEPT the lifecycle
/// transitions (`active`/`stopped`/`disabled`). The controller UI clears a
/// transient warning after ~3 seconds, so an identical later refusal (e.g.
/// `occluded`) must re-emit — the permanent lifecycle latch would otherwise
/// swallow it (the exact `004A.log` defect).
fn is_transient_feedback_status(status: &str) -> bool {
    !matches!(status, "active" | "stopped" | "disabled")
}

fn record_status_emitted(status: &RemoteControlStatus) {
    last_emitted_statuses().lock_unpoisoned().insert(
        (status.window_id, status.controller_id.clone()),
        status.status,
    );
}

fn should_deliver_status(status: &RemoteControlStatus, forced: bool) -> bool {
    if forced {
        record_status_emitted(status);
        return true;
    }
    if is_transient_feedback_status(status.status) {
        // Transient feedback bypasses the permanent lifecycle latch. It is
        // NOT recorded there (recording would poison the next lifecycle
        // transition's dedup). Upstream host-side throttling (the 1-second
        // replay-failure throttle) still applies on the sender side.
        return true;
    }
    should_emit_status(status)
}

fn should_warn_controller_id_mismatch(window_id: u32, controller_id: &str) -> bool {
    remote_control_engine().should_warn_controller_id_mismatch(window_id, controller_id)
}

fn reset_unreliable_seq(window_id: u32, controller_id: &str) {
    remote_control_engine().reset_unreliable_seq(window_id, controller_id);
}

fn should_accept_unreliable_seq(message: &RemoteControlMessage) -> bool {
    match remote_control_engine().accept_unreliable_seq(message) {
        UnreliableSeqDecision::Accepted => true,
        UnreliableSeqDecision::AcceptedRestart { stream, previous } => {
            log::info!(
                "remote-control: accepting controller restart for unreliable {:?} window {} controller='{}' seq={} previous_watermark={}",
                stream,
                message.window_id,
                message.controller_id,
                message.seq,
                previous
            );
            true
        }
        UnreliableSeqDecision::Rejected { stream, last_seen } => {
            if should_log_message(message.message_type, message.action, message.seq) {
                log::info!(
                    "remote-control: dropping input reason=stale-seq detail=unreliable {:?} window {} controller='{}' seq={} last_seen={}",
                    stream,
                    message.window_id,
                    message.controller_id,
                    message.seq,
                    last_seen
                );
            } else {
                log::debug!(
                    "remote-control: dropping input reason=stale-seq detail=unreliable {:?} window {} controller='{}' seq={} last_seen={}",
                    stream,
                    message.window_id,
                    message.controller_id,
                    message.seq,
                    last_seen
                );
            }
            false
        }
    }
}

/// Reconnects preserve grants, but any held input whose release packet was in
/// flight must be released before the transport resumes (issue #371).
pub(crate) fn release_held_inputs_for_reconnect() -> usize {
    let releases = drain_all_pressed();
    let count = releases.len();
    enqueue_synthetic_releases(releases, "transparent reconnect");
    count
}
fn ensure_pressed_ttl_sweeper() {
    if PRESSED_TTL_SWEEPER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        // #372: piggyback the periodic remote-control-summary log on this
        // existing tick instead of spawning a second thread.
        let mut ticks_since_summary: u32 = 0;
        loop {
            std::thread::sleep(HELD_INPUT_SWEEP_INTERVAL);
            let releases = drain_expired_pressed(Instant::now());
            enqueue_synthetic_releases(releases, "held input TTL expired");
            ticks_since_summary += 1;
            if ticks_since_summary >= LATENCY_SUMMARY_TICK_INTERVAL {
                ticks_since_summary = 0;
                log_latency_summary_tick();
            }
        }
    });
}

fn enqueue_synthetic_releases(tasks: Vec<ReplayTask>, reason: &str) {
    if tasks.is_empty() {
        return;
    }
    log::info!(
        "remote-control: synthesizing {} release event(s) ({})",
        tasks.len(),
        reason
    );
    for task in tasks {
        enqueue_replay(task);
    }
}

fn active_status_for_session(
    window_id: u32,
    controller_id: String,
    grant_token: String,
) -> RemoteControlStatus {
    RemoteControlStatus {
        window_id,
        owner_identity: None,
        controller_id,
        status: "active",
        message: "Remote control active for shared window".to_string(),
        grant_token: Some(grant_token),
        reason: None,
    }
}

/// Reconfirm grants after LiveKit reconnects without changing authorization.
/// The forced path is intentional: normal status de-duplication would hide
/// this status from a controller whose data channel was recreated (#371).
pub(crate) fn reemit_active_statuses(app: &AppHandle, state: &SessionState) {
    let Some((_, local_identity)) = state.control_channel_snapshot() else {
        log::warn!("remote-control: cannot re-emit active statuses without a joined room");
        return;
    };
    let active: Vec<(u32, String, String)> = sessions()
        .lock_unpoisoned()
        .iter()
        .map(|(key, grant_token)| {
            (
                key.window_id,
                key.controller_id.clone(),
                grant_token.clone(),
            )
        })
        .collect();
    for (window_id, controller_id, grant_token) in active {
        emit_and_send_status_forced(
            app,
            state,
            &local_identity,
            active_status_for_session(window_id, controller_id, grant_token),
        );
    }
}

/// #374: grants are shared, not exclusive. A Request from a NEW controller
/// on a window that already has a different active controller ADDS a second
/// concurrent grant rather than displacing the existing one — Petal injects
/// via independent per-target-pid AX/SkyLight actions, not a shared system
/// cursor, so two controllers' streams can interleave without contention.
/// Each grant gets its own token (keyed by the complete target identity plus
/// controller), so this naturally supports N simultaneous controllers by
/// simply never removing another controller's entry here.
/// Windows only: mirror the newly minted grant for (window, controller) under
/// the legacy key, so legacy-shaped (lossy) packets — the macOS drag stream —
/// authorize on a v2-granted Windows session. The caller passes the exact new
/// token so a regrant cannot accidentally reselect the stale legacy entry;
/// revocation is key-agnostic
/// (revoke/drain/revoke_all prune every session entry for the window/
/// controller), so no stale key survives teardown.
#[cfg(target_os = "windows")]
pub(crate) fn mirror_grant_to_legacy_key(window_id: u32, controller_id: &str, grant_token: &str) {
    sessions().lock_unpoisoned().insert(
        ControlGrantKey::legacy(window_id, controller_id),
        grant_token.to_string(),
    );
}

fn authorize_shared(window_id: u32, controller_id: &str) -> String {
    authorize_shared_key(ControlGrantKey::legacy(window_id, controller_id))
}

fn authorize_shared_key(key: ControlGrantKey) -> String {
    let window_id = key.window_id;
    let controller_id = key.controller_id.clone();
    let grant_token = new_grant_token();
    remote_control_engine().install_grant(key, grant_token.clone());
    warned_tokenless_inputs()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.clone()));
    reset_unreliable_seq(window_id, &controller_id);
    grant_token
}

fn revoke(window_id: u32, controller_id: &str) {
    remote_clipboard::clear_pending_copy_for(window_id, Some(controller_id));
    #[cfg(target_os = "windows")]
    clear_escalations_where(|pending_window, pending_controller| {
        pending_window == window_id && pending_controller == controller_id
    });
    // #374: clear only THIS controller's parked AX gesture, never a
    // concurrent controller's — gesture state is keyed per (window,
    // controller) precisely so one controller revoking/disconnecting can't
    // clobber another still-active controller's in-progress drag anchor.
    platform_control().clear_controller_gestures(window_id, controller_id);
    // Refs #288: same insert-only leak class #410 fixed for the other
    // per-(window, controller) maps -- see `clear_discrete_admissions`.
    clear_discrete_admissions(|w, c| !(w == window_id && c == controller_id));
    let releases = drain_pressed_for_controller(window_id, controller_id);
    let mut grants = sessions().lock_unpoisoned();
    let before = grants.len();
    grants.retain(|key, _| key.window_id != window_id || key.controller_id != controller_id);
    let removed = grants.len() != before;
    drop(grants);
    warned_tokenless_inputs()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()));
    // Fable review fix (#372): same "F2" leak class as warned_tokenless_inputs
    // above -- this map is otherwise insert-only.
    clear_replay_failure_status_throttle(window_id, controller_id);
    // #410: same insert-only leak class as warned_tokenless_inputs/
    // replay_failure_status_throttle above -- these three maps had no
    // cleanup on any departure path at all until now.
    controller_pointer_positions()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()));
    last_emitted_statuses()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()));
    warned_controller_id_mismatches()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()));
    if removed || !releases.is_empty() {
        // #374: bump only this controller's replay epoch, not the whole
        // window's — a concurrent controller's already-queued replay tasks
        // must survive this controller revoking/disconnecting.
        bump_replay_epoch(window_id, controller_id, "controller revoked");
    }
    reset_unreliable_seq(window_id, controller_id);
    enqueue_synthetic_releases(releases, "controller revoked");
}

fn drain_window_control(window_id: u32) -> (Vec<String>, Vec<ReplayTask>) {
    drain_window_control_releasing(window_id, release_platform_window_gestures)
}

/// `release_session_tap` is injected so a test can observe WHEN the
/// held-button release runs relative to the cache clear -- the ordering IS
/// the defect (#446 A7), not the release itself.
fn drain_window_control_releasing(
    window_id: u32,
    release_session_tap: fn(u32),
) -> (Vec<String>, Vec<ReplayTask>) {
    #[cfg(target_os = "windows")]
    clear_escalations_where(|pending_window, _| pending_window == window_id);
    // #446 A7: this MUST run before `clear_control_caches_for_window`, which
    // reaches `clear_ax_gesture_for_window` and keeps only an opted-in
    // SlDrag -- dropping a session-tap gesture. A dropped record cannot be
    // released, so the release that used to sit at the end of this function
    // found nothing and the target app kept the button held. Live evidence:
    // `mode=<none> outcome=Handled` with zero events at the target.
    release_session_tap(window_id);
    clear_control_caches_for_window(window_id);
    // Window-wide: every controller on this window loses control (sharing
    // ended), so this is the one place that legitimately bumps ALL of a
    // window's controller epochs at once.
    bump_replay_epoch_for_window(window_id, "window control drained");
    last_unreliable_seqs()
        .lock_unpoisoned()
        .retain(|(stored_window_id, _, _), _| *stored_window_id != window_id);
    let releases = drain_pressed_for_window(window_id);
    let mut active = Vec::new();
    sessions().lock_unpoisoned().retain(|key, _| {
        if key.window_id == window_id {
            active.push(key.controller_id.clone());
            false
        } else {
            true
        }
    });
    // Fable review fix (#372), round 2: this is a genuine grant-teardown path
    // (sharing ended) that was missing the same per-departed-controller
    // cleanup `revoke()`/`revoke_controller()` already do -- window IDs are
    // never reused, so without this a controller's warn-once and
    // replay-failure-throttle entries leak for the process lifetime every
    // time a share with an active controller ends (the common
    // share-stopped/window-disappeared path, not just an explicit revoke).
    for controller_id in &active {
        warned_tokenless_inputs()
            .lock_unpoisoned()
            .remove(&(window_id, controller_id.clone()));
        clear_replay_failure_status_throttle(window_id, controller_id);
        // #410: same insert-only leak class as warned_tokenless_inputs/
        // replay_failure_status_throttle above -- these three maps had no
        // cleanup on any departure path at all until now.
        controller_pointer_positions()
            .lock_unpoisoned()
            .remove(&(window_id, controller_id.clone()));
        last_emitted_statuses()
            .lock_unpoisoned()
            .remove(&(window_id, controller_id.clone()));
        warned_controller_id_mismatches()
            .lock_unpoisoned()
            .remove(&(window_id, controller_id.clone()));
    }
    // Refs #288: same insert-only leak class as the maps above -- see
    // `clear_discrete_admissions`.
    clear_discrete_admissions(|w, _| w != window_id);
    // #446: a session-tap gesture holds a REAL mouse button down in the target
    // app. If control is torn down mid-drag (revoke, disconnect, share ended,
    // deadline abandon) nothing else will ever post the matching Up, and the
    // target is left with a phantom held button -- worse than the silent
    // no-op this route exists to fix. The release itself now runs at the TOP
    // of this function -- see the ordering note there.
    (active, releases)
}

pub(crate) fn revoke_window(app: &AppHandle, window_id: u32, reason: &'static str) {
    deny_pending_requests_where(
        app,
        |pending_window, _| pending_window == window_id,
        "Remote control request ended because sharing stopped.",
    );
    #[cfg(target_os = "windows")]
    remote_control_engine().remove_controller_grants_for_window(window_id);
    #[cfg(target_os = "windows")]
    crate::windows_remote_control::clear_pending_controller_operations(window_id, None);
    let (active, releases) = drain_window_control(window_id);
    enqueue_synthetic_releases(releases, reason);
    if active.is_empty() {
        log::debug!("remote-control: revoke_window({window_id}) no active controllers ({reason})");
        return;
    }
    log::info!(
        "remote-control: revoking {} active controller(s) for window {} ({})",
        active.len(),
        window_id,
        reason
    );
    for controller_id in active {
        emit_status(
            app,
            RemoteControlStatus {
                window_id,
                owner_identity: None,
                controller_id,
                status: "stopped",
                message: "Remote control stopped because sharing ended.".to_string(),
                grant_token: None,
                reason: None,
            },
        );
    }
}

pub(crate) fn revoke_all(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    clear_escalations_where(|_, _| true);
    deny_pending_requests_where(
        app,
        |_, _| true,
        "Remote control is disabled for this meeting",
    );
    #[cfg(target_os = "windows")]
    remote_control_engine().clear_controller_grants();
    #[cfg(target_os = "windows")]
    crate::windows_remote_control::clear_all_pending_controller_operations();
    clear_all_control_caches();
    bump_all_replay_epochs("all control revoked");
    let releases = drain_all_pressed();
    enqueue_synthetic_releases(releases, "all control revoked");
    let active: Vec<(u32, String)> = {
        let mut guard = sessions().lock_unpoisoned();
        guard
            .drain()
            .map(|(key, _)| (key.window_id, key.controller_id))
            .collect()
    };
    warned_tokenless_inputs().lock_unpoisoned().clear();
    last_unreliable_seqs().lock_unpoisoned().clear();
    // Fable review fix (#372): same insert-only leak class as
    // warned_tokenless_inputs above.
    replay_failure_status_throttle().lock_unpoisoned().clear();
    // #410: same insert-only leak class as warned_tokenless_inputs/
    // replay_failure_status_throttle above -- these three maps had no
    // cleanup on any departure path at all until now.
    controller_pointer_positions().lock_unpoisoned().clear();
    last_emitted_statuses().lock_unpoisoned().clear();
    warned_controller_id_mismatches().lock_unpoisoned().clear();
    // Refs #288: same insert-only leak class as the maps above.
    *discrete_admissions().lock_unpoisoned() = DiscreteAdmissionState::default();
    for (window_id, controller_id) in active {
        emit_status(
            app,
            RemoteControlStatus {
                window_id,
                owner_identity: None,
                controller_id,
                status: "stopped",
                message: "Remote control disabled for this meeting".to_string(),
                grant_token: None,
                reason: None,
            },
        );
    }
}

pub(crate) fn revoke_controller(app: &AppHandle, controller_id: &str, reason: &'static str) {
    remote_clipboard::clear_pending_copy_for_owner(controller_id);
    remote_clipboard::clear_copy_operations_for_sender(controller_id);
    remote_clipboard::clear_paste_operations_for_sender(controller_id);
    #[cfg(target_os = "windows")]
    clear_escalations_where(|_, pending_controller| pending_controller == controller_id);
    deny_pending_requests_where(
        app,
        |_, pending_controller| pending_controller == controller_id,
        "Remote control request ended because the requester left.",
    );
    #[cfg(target_os = "windows")]
    {
        remote_control_engine().remove_controller_grants_for_owner(controller_id);
        crate::windows_remote_control::clear_pending_controller_operations_for_owner(controller_id);
    }
    let releases = drain_pressed_for_controller_id(controller_id);
    enqueue_synthetic_releases(releases, reason);
    last_unreliable_seqs()
        .lock_unpoisoned()
        .retain(|(_, stored_controller_id, _), _| stored_controller_id != controller_id);
    let mut active = Vec::new();
    sessions().lock_unpoisoned().retain(|key, _| {
        if key.controller_id == controller_id {
            active.push(key.window_id);
            false
        } else {
            true
        }
    });
    // Refs #288: same insert-only leak class as the maps below -- see
    // `clear_discrete_admissions`.
    clear_discrete_admissions(|_, c| c != controller_id);
    for window_id in &active {
        warned_tokenless_inputs()
            .lock_unpoisoned()
            .remove(&(*window_id, controller_id.to_string()));
        // Fable review fix (#372): same insert-only leak class as
        // warned_tokenless_inputs above.
        clear_replay_failure_status_throttle(*window_id, controller_id);
        // #410: same insert-only leak class as warned_tokenless_inputs/
        // replay_failure_status_throttle above -- these three maps had no
        // cleanup on any departure path at all until now.
        controller_pointer_positions()
            .lock_unpoisoned()
            .remove(&(*window_id, controller_id.to_string()));
        last_emitted_statuses()
            .lock_unpoisoned()
            .remove(&(*window_id, controller_id.to_string()));
        warned_controller_id_mismatches()
            .lock_unpoisoned()
            .remove(&(*window_id, controller_id.to_string()));
        // #374: this controller disconnected entirely, so clear/invalidate
        // only ITS state per window — never another concurrent controller's
        // gesture or replay epoch on the same window.
        platform_control().clear_controller_gestures(*window_id, controller_id);
        bump_replay_epoch(*window_id, controller_id, reason);
    }
    if active.is_empty() {
        log::debug!(
            "remote-control: revoke_controller('{controller_id}') no active windows ({reason})"
        );
        return;
    }
    log::info!(
        "remote-control: revoking controller '{}' for {} window(s) ({})",
        controller_id,
        active.len(),
        reason
    );
    for window_id in active {
        emit_status(
            app,
            RemoteControlStatus {
                window_id,
                owner_identity: None,
                controller_id: controller_id.to_string(),
                status: "stopped",
                message: "Remote control stopped because the controller disconnected.".to_string(),
                grant_token: None,
                reason: None,
            },
        );
    }
}

fn is_authorized(window_id: u32, controller_id: &str) -> bool {
    sessions()
        .lock_unpoisoned()
        .contains_key(&ControlGrantKey::legacy(window_id, controller_id))
}

fn is_authorized_input(message: &RemoteControlMessage) -> bool {
    let Some(grant_key) = ControlGrantKey::for_message(message) else {
        return false;
    };
    let Some(active_token) = remote_control_engine().active_grant_token(&grant_key) else {
        return false;
    };
    match message.grant_token.as_deref() {
        Some(token) if token == active_token => true,
        Some(_) => {
            log::debug!(
                "remote-control: dropping input from '{}' for window {} because grant token is stale or invalid",
                message.controller_id,
                message.window_id
            );
            false
        }
        None => {
            let key = (message.window_id, message.controller_id.clone());
            let first_tokenless_input = warned_tokenless_inputs().lock_unpoisoned().insert(key);
            if TOKENLESS_GRANT_COMPATIBILITY_ENABLED {
                if first_tokenless_input {
                    log::warn!(
                        "remote-control: accepting tokenless input from '{}' for window {} during the one-release grant-token compatibility window",
                        message.controller_id,
                        message.window_id
                    );
                }
                true
            } else {
                if first_tokenless_input {
                    log::warn!(
                        "remote-control: dropping tokenless input from '{}' for window {} because the grant-token compatibility window has ended",
                        message.controller_id,
                        message.window_id
                    );
                }
                false
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn input_authority_is_current(message: &RemoteControlMessage) -> bool {
    is_authorized_input(message)
}

fn bind_trusted_sender(
    trusted_sender: Option<String>,
    message: RemoteControlMessage,
) -> Option<RemoteControlMessage> {
    remote_control_engine().bind_trusted_sender(trusted_sender, message)
}

fn active_grant_token(window_id: u32, controller_id: &str) -> Option<String> {
    sessions()
        .lock_unpoisoned()
        .iter()
        .find(|(key, _)| key.window_id == window_id && key.controller_id == controller_id)
        .map(|(_, token)| token.clone())
}

pub(crate) fn window_has_active_controller(window_id: u32) -> bool {
    sessions()
        .lock_unpoisoned()
        .keys()
        .any(|key| key.window_id == window_id)
}

/// Friendly display state for a local Petal View selector. Technical
/// controller identities stay native-only; the title bar needs only a bounded
/// name and treats multiple simultaneous grants conservatively.
pub(crate) fn active_controller_display_name(
    state: &SessionState,
    window_id: u32,
) -> Option<String> {
    let controller_ids = sessions()
        .lock_unpoisoned()
        .keys()
        .filter(|key| key.window_id == window_id)
        .map(|key| key.controller_id.clone())
        .collect::<Vec<_>>();
    match controller_ids.as_slice() {
        [] => None,
        [controller_id] => Some(controller_display_name(state, controller_id)),
        _ => Some("Multiple participants".to_string()),
    }
}

/// Read-only state for the owner-gated autotest socket. Keep replay handles
/// private: the runner only needs to prove ownership and that held input was
/// drained, not inspect platform-specific release tasks.
pub(crate) fn autotest_status_snapshot() -> serde_json::Value {
    let sessions = sessions()
        .lock_unpoisoned()
        .keys()
        .map(|key| {
            serde_json::json!({
                "windowId": key.window_id,
                "controllerId": key.controller_id,
            })
        })
        .collect::<Vec<_>>();
    let pressed_inputs = pressed_inputs()
        .lock_unpoisoned()
        .iter()
        .map(|((window_id, controller_id), pressed)| {
            serde_json::json!({
                "windowId": window_id,
                "controllerId": controller_id,
                "buttons": pressed.buttons.len(),
                "keys": pressed.keys.len(),
            })
        })
        .collect::<Vec<_>>();
    let pending = remote_control_engine()
        .pending_request_keys()
        .into_iter()
        .map(|(window_id, controller_id)| {
            serde_json::json!({ "windowId": window_id, "controllerId": controller_id })
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "sessions": sessions, "pressedInputs": pressed_inputs, "pending": pending })
}

/// Observer-only ledgers for RC-N2N (#819), the cockpit scenario in which a
/// NATIVE instance is the controller. They record what the real control path
/// did; they never gate, delay or alter it. Compiled out entirely without
/// `cockpit-privileged`, so a customer build has no code path here at all.
///
/// Do NOT make anything in here decide anything: the moment a ledger read
/// changes behaviour it stops being evidence about the product and becomes
/// part of it.
#[cfg(feature = "cockpit-privileged")]
pub(crate) mod cockpit_ledger {
    use super::{RemoteControlMessage, RemoteControlStatus, RemoteControlType};
    use crate::sync_ext::MutexExt;
    use crate::time_util::now_ms;
    use std::sync::{Mutex, OnceLock};

    /// Bounded so a soak run cannot grow these without limit. Oldest entries
    /// are dropped first; a scenario that overruns this is not a scenario the
    /// keystone set produces.
    const MAX_ENTRIES: usize = 512;

    #[derive(Clone, Debug, PartialEq, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct ControllerPublish {
        pub t_ms: u64,
        pub kind: RemoteControlType,
        pub action: Option<super::RemoteControlAction>,
        pub window_id: u32,
        pub target_user_id: String,
        pub key: Option<String>,
        pub code: Option<String>,
        pub text: Option<String>,
        pub button: Option<i16>,
        pub meta: bool,
        pub shift: bool,
        pub seq: u64,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct ControllerStatus {
        pub t_ms: u64,
        pub window_id: u32,
        pub controller_id: String,
        pub status: String,
        pub has_grant_token: bool,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct HostReplay {
        pub t_ms: u64,
        pub window_id: u32,
        pub controller_id: String,
        pub kind: RemoteControlType,
        pub action: Option<super::RemoteControlAction>,
        pub key: Option<String>,
        pub text: Option<String>,
        /// The host's own terminal disposition for this input: "applied",
        /// "replayFailed", "injectionTimeout" or "superseded". This is the
        /// host-side effect record -- not a wire echo.
        pub outcome: String,
    }

    #[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct CockpitControlLedger {
        pub published: Vec<ControllerPublish>,
        pub statuses: Vec<ControllerStatus>,
        pub replays: Vec<HostReplay>,
        pub clipboard_published: Vec<ClipboardPublish>,
        pub clipboard_replays: Vec<ClipboardReplay>,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct ClipboardPublish {
        pub t_ms: u64,
        pub window_id: u32,
        pub controller_id: String,
        pub target_user_id: String,
        pub operation: String,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(crate) struct ClipboardReplay {
        pub t_ms: u64,
        pub window_id: u32,
        pub controller_id: String,
        pub operation: String,
        pub outcome: String,
    }

    fn store() -> &'static Mutex<CockpitControlLedger> {
        static STORE: OnceLock<Mutex<CockpitControlLedger>> = OnceLock::new();
        STORE.get_or_init(|| Mutex::new(CockpitControlLedger::default()))
    }

    fn push_bounded<T>(items: &mut Vec<T>, item: T) {
        if items.len() >= MAX_ENTRIES {
            items.remove(0);
        }
        items.push(item);
    }

    pub(crate) fn reset() {
        *store().lock_unpoisoned() = CockpitControlLedger::default();
    }

    pub(crate) fn snapshot() -> CockpitControlLedger {
        store().lock_unpoisoned().clone()
    }

    pub(super) fn record_publish(message: &RemoteControlMessage) {
        let entry = ControllerPublish {
            t_ms: now_ms(),
            kind: message.message_type,
            action: message.action,
            window_id: message.window_id,
            target_user_id: message.target_user_id.clone(),
            key: message.key.clone(),
            code: message.code.clone(),
            text: message.text.clone(),
            button: message.button,
            meta: message.modifiers.meta,
            shift: message.modifiers.shift,
            seq: message.seq,
        };
        push_bounded(&mut store().lock_unpoisoned().published, entry);
    }

    pub(super) fn record_clipboard_publish(
        window_id: u32,
        controller_id: &str,
        target_user_id: &str,
        operation: &str,
    ) {
        push_bounded(
            &mut store().lock_unpoisoned().clipboard_published,
            ClipboardPublish {
                t_ms: now_ms(),
                window_id,
                controller_id: controller_id.to_string(),
                target_user_id: target_user_id.to_string(),
                operation: operation.to_string(),
            },
        );
    }

    pub(super) fn record_clipboard_replay(
        window_id: u32,
        controller_id: &str,
        operation: &str,
        outcome: &str,
    ) {
        push_bounded(
            &mut store().lock_unpoisoned().clipboard_replays,
            ClipboardReplay {
                t_ms: now_ms(),
                window_id,
                controller_id: controller_id.to_string(),
                operation: operation.to_string(),
                outcome: outcome.to_string(),
            },
        );
    }

    pub(super) fn record_status(status: &RemoteControlStatus) {
        let entry = ControllerStatus {
            t_ms: now_ms(),
            window_id: status.window_id,
            controller_id: status.controller_id.clone(),
            status: status.status.to_string(),
            has_grant_token: status.grant_token.is_some(),
        };
        push_bounded(&mut store().lock_unpoisoned().statuses, entry);
    }

    pub(super) fn record_replay(message: &RemoteControlMessage, outcome: &str) {
        let entry = HostReplay {
            t_ms: now_ms(),
            window_id: message.window_id,
            controller_id: message.controller_id.clone(),
            kind: message.message_type,
            action: message.action,
            key: message.key.clone(),
            text: message.text.clone(),
            outcome: outcome.to_string(),
        };
        push_bounded(&mut store().lock_unpoisoned().replays, entry);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ledger_is_bounded_and_drops_the_oldest_entry_first() {
            let mut items: Vec<u32> = Vec::new();
            for value in 0..(MAX_ENTRIES as u32 + 5) {
                push_bounded(&mut items, value);
            }
            assert_eq!(items.len(), MAX_ENTRIES);
            assert_eq!(items[0], 5, "oldest entries must be the ones dropped");
            assert_eq!(items[MAX_ENTRIES - 1], MAX_ENTRIES as u32 + 4);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestGate {
    Allowed,
    Disabled,
    RequesterNotPresent,
    /// `Ask` policy: the request is parked for the sharer's Allow/Deny. No
    /// grant exists until an explicit Allow (`answer_consent`).
    AwaitingConsent,
}

fn requester_is_present_in_room(state: &SessionState, controller_id: &str) -> bool {
    let Some((room_connection, local_identity)) = state.control_channel_snapshot() else {
        return false;
    };
    if controller_id == local_identity {
        return false;
    }
    room_connection
        .room()
        .remote_participants()
        .keys()
        .any(|identity| identity.to_string() == controller_id)
}

fn apply_request_gate(window_id: u32, controller_id: &str, gate: RequestGate) -> Option<String> {
    if gate == RequestGate::AwaitingConsent {
        return None;
    }
    if gate == RequestGate::RequesterNotPresent {
        revoke(window_id, controller_id);
        return None;
    }
    if gate == RequestGate::Disabled {
        revoke(window_id, controller_id);
        return None;
    }
    Some(authorize_shared(window_id, controller_id))
}

fn apply_request_gate_for_message(
    message: &RemoteControlMessage,
    gate: RequestGate,
) -> Option<String> {
    if gate == RequestGate::AwaitingConsent {
        // Parked, not refused: leave any existing state alone and mint nothing.
        return None;
    }
    if gate != RequestGate::Allowed {
        revoke(message.window_id, &message.controller_id);
        return None;
    }
    ControlGrantKey::for_message(message).map(authorize_shared_key)
}

fn apply_request_accessibility_decision(
    window_id: u32,
    controller_id: &str,
    accessibility_granted: bool,
) -> bool {
    if !accessibility_granted {
        revoke(window_id, controller_id);
    }
    accessibility_granted
}

/// How long a parked consent request waits for the sharer before it is
/// denied (`denied` / `consentTimedOut`). A timeout NEVER grants.
pub(crate) const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Kind of prompt rendered by the always-loaded control-consent panel.
/// Ordinary control requests are parked in the remote-control engine; Windows
/// full-control escalation requests use a separate short-lived host record.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ControlConsentPromptKind {
    Control,
    FullControlEscalation,
}

/// `control-consent-requested` event payload (mirrors
/// `ControlConsentRequestedEvent` in `ipc.ts`). Emitted on the global bus
/// (never `emit_to`, see share_notice.rs) for the always-loaded
/// `control-consent` panel route on both supported desktop platforms.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlConsentRequestedPayload {
    pub kind: ControlConsentPromptKind,
    pub window_id: u32,
    pub controller_id: String,
    pub controller_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    pub timeout_ms: u64,
}

#[cfg(target_os = "windows")]
const ESCALATION_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(target_os = "windows")]
fn pending_escalations() -> &'static Mutex<HashMap<(u32, String), Instant>> {
    static PENDING: OnceLock<Mutex<HashMap<(u32, String), Instant>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "windows")]
fn park_escalation(window_id: u32, controller_id: &str) -> bool {
    let now = Instant::now();
    let mut pending = pending_escalations().lock_unpoisoned();
    pending.retain(|_, deadline| *deadline > now);
    if pending.contains_key(&(window_id, controller_id.to_string())) {
        return false;
    }
    pending.insert(
        (window_id, controller_id.to_string()),
        now + ESCALATION_TIMEOUT,
    );
    true
}

#[cfg(target_os = "windows")]
fn take_escalation(window_id: u32, controller_id: &str) -> bool {
    pending_escalations()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()))
        .is_some_and(|deadline| deadline > Instant::now())
}

#[cfg(target_os = "windows")]
fn clear_escalations_where(mut take: impl FnMut(u32, &str) -> bool) {
    pending_escalations()
        .lock_unpoisoned()
        .retain(|(window_id, controller_id), _| !take(*window_id, controller_id));
}

#[cfg(target_os = "windows")]
pub(crate) fn clear_escalations_for_window(window_id: u32) {
    clear_escalations_where(|pending_window, _| pending_window == window_id);
}

#[cfg(all(test, target_os = "windows"))]
mod escalation_prompt_tests {
    use super::*;

    #[test]
    fn prompt_kinds_use_the_discriminated_wire_names() {
        assert_eq!(
            serde_json::to_value(ControlConsentPromptKind::Control).unwrap(),
            "control"
        );
        assert_eq!(
            serde_json::to_value(ControlConsentPromptKind::FullControlEscalation).unwrap(),
            "fullControlEscalation"
        );
    }

    #[test]
    fn escalation_requests_are_deduplicated_and_one_shot() {
        let window_id = u32::MAX - 17;
        let controller_id = "escalation-test-controller";
        clear_escalations_where(|window, controller| {
            window == window_id && controller == controller_id
        });
        assert!(park_escalation(window_id, controller_id));
        assert!(!park_escalation(window_id, controller_id));
        assert!(take_escalation(window_id, controller_id));
        assert!(!take_escalation(window_id, controller_id));
    }
}

fn looks_like_technical_identity(value: &str) -> bool {
    fn is_hex(byte: u8) -> bool {
        byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'A'..=b'F')
    }
    fn is_uuid(value: &[u8]) -> bool {
        value.len() == 36
            && [8, 13, 18, 23]
                .into_iter()
                .all(|index| value[index] == b'-')
            && value
                .iter()
                .enumerate()
                .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || is_hex(*byte))
    }
    let value = value.trim().as_bytes();
    is_uuid(value)
        || value.strip_prefix(b"web-").is_some_and(is_uuid)
        || (value.len() == 32 && value.iter().all(|byte| is_hex(*byte)))
}

pub(crate) fn controller_display_name(state: &SessionState, controller_id: &str) -> String {
    let name = state
        .control_channel_snapshot()
        .and_then(|(room_connection, _)| {
            room_connection
                .room()
                .remote_participants()
                .into_iter()
                .find(|(identity, _)| identity.to_string() == controller_id)
                .map(|(_, participant)| participant.name())
        })
        .unwrap_or_default();
    let name = name.trim();
    if name.is_empty() || looks_like_technical_identity(name) {
        "A participant".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod controller_display_name_tests {
    use super::looks_like_technical_identity;

    #[test]
    fn technical_participant_names_are_not_user_facing() {
        assert!(looks_like_technical_identity(
            "123e4567-e89b-12d3-a456-426614174000"
        ));
        assert!(looks_like_technical_identity(
            "web-123e4567-e89b-12d3-a456-426614174000"
        ));
        assert!(looks_like_technical_identity(
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(!looks_like_technical_identity("Jordan Kim"));
    }
}

/// Park a Request under the `Ask` policy: store it, tell the controller it
/// is waiting (`awaitingConsent`, a non-lifecycle status that never installs
/// a grant), prompt the sharer ONCE per (window, controller), and arm the
/// deny-on-timeout timer. A repeat Request while parked only re-emits the
/// waiting status -- it never re-prompts.
fn park_consent_request(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    message: RemoteControlMessage,
) {
    let window_id = message.window_id;
    let controller_id = message.controller_id.clone();
    let engine = remote_control_engine();
    let already_pending = engine.has_pending_request(window_id, &controller_id);
    let Some(key) = ControlGrantKey::for_message(&message) else {
        log::warn!(
            "remote-control: cannot park consent request from '{controller_id}' for window {window_id}: no grant key"
        );
        return;
    };
    let seq = message.seq;
    // A repeat request while parked must NOT replace the stored message:
    // the deny timer armed below is keyed to the ORIGINAL seq, so replacing
    // it would make that timer see a newer seq and exit without denying --
    // an orphaned entry with no timer, no re-prompt, and no `denied`
    // (adversarial review P1). Keep the original; only re-emit the status.
    if !already_pending {
        engine.store_pending_request(key, message);
    }
    log::info!(
        "remote-control: consent {} for '{controller_id}' on shared window {window_id} (policy=ask, seq={})",
        if already_pending { "re-requested, still pending (original timer kept)" } else { "requested" },
        if already_pending {
            engine.pending_request_seq(window_id, &controller_id).unwrap_or(seq)
        } else {
            seq
        }
    );
    emit_and_send_status(
        app,
        state,
        local_identity,
        RemoteControlStatus {
            window_id,
            owner_identity: None,
            controller_id: controller_id.clone(),
            status: "awaitingConsent",
            message: "Waiting for the sharer to approve remote control.".to_string(),
            grant_token: None,
            reason: None,
        },
    );
    if already_pending {
        return;
    }
    let payload = ControlConsentRequestedPayload {
        kind: ControlConsentPromptKind::Control,
        window_id,
        controller_id: controller_id.clone(),
        controller_name: controller_display_name(state, &controller_id),
        window_title: state.active_share_source_title(window_id),
        timeout_ms: CONSENT_TIMEOUT.as_millis() as u64,
    };
    let _ = app.emit("control-consent-requested", payload);
    let app_for_timer = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(CONSENT_TIMEOUT).await;
        // Only deny the SAME parked request this timer was armed for: a
        // repeat keeps the original sequence, while a resolved request is
        // gone.
        if remote_control_engine().pending_request_seq(window_id, &controller_id) != Some(seq) {
            return;
        }
        log::info!(
            "remote-control: consent for '{controller_id}' on window {window_id} timed out after {:?}; denying",
            CONSENT_TIMEOUT
        );
        answer_consent(
            &app_for_timer,
            window_id,
            &controller_id,
            false,
            RemoteControlReason::ConsentTimedOut,
        );
    });
}

/// Resolve a parked consent request. `approve == true` runs the exact
/// authorize tail an `Auto` request runs (gate re-check, accessibility
/// decision, `active` + grant token); `false` emits `denied` with `reason`.
/// Returns false when nothing was pending for this (window, controller).
pub(crate) fn answer_consent(
    app: &AppHandle,
    window_id: u32,
    controller_id: &str,
    approve: bool,
    reason: RemoteControlReason,
) -> bool {
    let Some(message) = remote_control_engine().take_pending_request(window_id, controller_id)
    else {
        log::info!(
            "remote-control: consent answer ({}) for '{controller_id}' on window {window_id} had nothing pending",
            if approve { "allow" } else { "deny" }
        );
        return false;
    };
    let Some(state) = app.try_state::<SessionState>() else {
        return false;
    };
    let state = state.inner();
    let Some((_, local_identity)) = state.control_channel_snapshot() else {
        log::warn!("remote-control: consent answered outside a joined room; dropping");
        return false;
    };
    if !approve {
        log::info!(
            "remote-control: sharer denied control for '{controller_id}' on window {window_id} ({reason:?})"
        );
        emit_and_send_status(
            app,
            state,
            &local_identity,
            RemoteControlStatus {
                window_id,
                owner_identity: None,
                controller_id: controller_id.to_string(),
                status: "denied",
                message: match reason {
                    RemoteControlReason::ConsentTimedOut => {
                        "The sharer did not respond to the control request.".to_string()
                    }
                    _ => "The sharer declined remote control.".to_string(),
                },
                grant_token: None,
                reason: Some(reason),
            },
        );
        return true;
    }
    // Re-run the gate at answer time: the sharer may have turned control off
    // or the requester may have left while the prompt was up.
    let gate = if !state.remote_control_allowed() {
        RequestGate::Disabled
    } else if !state.share_allows_remote_control(window_id) {
        // The per-share lock can be flipped while the prompt is up, or the
        // share can end entirely; either way the approval must not mint a
        // grant for a window that no longer permits control.
        RequestGate::Disabled
    } else if !requester_is_present_in_room(state, controller_id) {
        RequestGate::RequesterNotPresent
    } else {
        RequestGate::Allowed
    };
    let Some(grant_token) = apply_request_gate_for_message(&message, gate) else {
        let (status, status_message) = if gate == RequestGate::Disabled {
            (
                "disabled",
                "Remote control is disabled for this meeting".to_string(),
            )
        } else {
            (
                "requestUnavailable",
                "Remote control request denied because the requester is not in this meeting."
                    .to_string(),
            )
        };
        emit_and_send_status(
            app,
            state,
            &local_identity,
            RemoteControlStatus {
                window_id,
                owner_identity: None,
                controller_id: controller_id.to_string(),
                status,
                message: status_message,
                grant_token: None,
                reason: None,
            },
        );
        return true;
    };
    log::info!(
        "remote-control: sharer allowed control for '{controller_id}' on window {window_id}"
    );
    complete_granted_request(app, state, &local_identity, message, grant_token);
    true
}

/// Deny every parked consent request matching `take` and tell the
/// controller. Called from the revoke paths so a parked request can never
/// outlive its share, its controller's presence, or the meeting's policy.
fn deny_pending_requests_where(
    app: &AppHandle,
    take: impl FnMut(u32, &str) -> bool,
    message: &str,
) {
    let taken = remote_control_engine().take_pending_requests_where(take);
    if taken.is_empty() {
        return;
    }
    let local_identity = app
        .try_state::<SessionState>()
        .and_then(|state| state.inner().control_channel_snapshot().map(|(_, id)| id));
    for pending in taken {
        let status = RemoteControlStatus {
            window_id: pending.window_id,
            owner_identity: None,
            controller_id: pending.controller_id.clone(),
            status: "denied",
            message: message.to_string(),
            grant_token: None,
            reason: Some(RemoteControlReason::ConsentDenied),
        };
        match (&local_identity, app.try_state::<SessionState>()) {
            (Some(local_identity), Some(state)) => {
                emit_and_send_status(app, state.inner(), local_identity, status)
            }
            _ => emit_status(app, status),
        }
    }
}

/// The authorize tail shared by an `Auto` request and an approved consent:
/// legacy-key mirror (Windows), accessibility decision, quality promotion,
/// and the `active` status that carries the freshly minted grant token.
fn complete_granted_request(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    message: RemoteControlMessage,
    grant_token: String,
) {
    // Windows: the v2-scoped grant (target kind + share instance) is
    // mirrored under the legacy key so the lossy pointer drag stream
    // (macOS parity) authorizes too.
    #[cfg(target_os = "windows")]
    mirror_grant_to_legacy_key(message.window_id, &message.controller_id, &grant_token);
    log::info!(
        "remote-control: local request gate accepted '{}' for shared window {}; LiveKit per-track viewer/subscription ACL is not exposed here, so backend/live validation is still required for strict viewer-only authorization",
        message.controller_id,
        message.window_id
    );
    // #374: concurrent grants — a new controller's Request no longer
    // displaces any existing controller on this window, so there is
    // no "stopped" notification to send to anyone else here.
    // On the first real control attempt, actively request Accessibility
    // (register Petal + show the grant dialog + open the pane) rather
    // than silently dropping every event with no user feedback (#201).
    let accessibility_granted = platform_control().accessibility_trusted();
    log::info!(
        "remote-control: accessibility decision for request from '{}' on window {} granted={}",
        message.controller_id,
        message.window_id,
        accessibility_granted
    );
    let status = if apply_request_accessibility_decision(
        message.window_id,
        &message.controller_id,
        accessibility_granted,
    ) {
        log::info!(
            "remote-control: '{}' started control of local shared window {}",
            message.controller_id,
            message.window_id
        );
        // Refs #288: v2 admission is scoped by this SAME grant token
        // (see `grant_is_current`) -- no separate v2 session mint.
        log::debug!(
            "remote-control: granted v2 control session '{}' for window {} controller='{}'",
            grant_token,
            message.window_id,
            message.controller_id
        );
        let app_for_quality = app.clone();
        let _quality_window_id = message.window_id;
        tauri::async_runtime::spawn(async move {
            if let Some(_state) = app_for_quality.try_state::<SessionState>() {
                #[cfg(target_os = "macos")]
                crate::session::promote_quality_for_remote_control(
                    _state.inner(),
                    _quality_window_id,
                )
                .await;
            }
        });
        RemoteControlStatus {
            window_id: message.window_id,
            owner_identity: None,
            controller_id: message.controller_id,
            status: "active",
            message: "Remote control active for shared window".to_string(),
            grant_token: Some(grant_token),
            reason: None,
        }
    } else {
        log::warn!(
            "remote-control: accessibility denial for request from '{}' on window {} accessibility_trusted=false",
            message.controller_id,
            message.window_id
        );
        platform_control().prompt_accessibility();
        RemoteControlStatus {
            window_id: message.window_id,
            owner_identity: None,
            controller_id: message.controller_id,
            status: "accessibilityDenied",
            message: "Petal needs Accessibility permission to let someone control your shared window. Grant Petal in System Settings > Privacy & Security > Accessibility, then try again.".to_string(),
            grant_token: None,
            reason: None,
        }
    };
    if accessibility_granted {
        emit_and_send_status_forced(app, state, local_identity, status);
    } else {
        emit_and_send_status(app, state, local_identity, status);
    }
}

pub(crate) fn normalized_to_global(frame: WindowFrame, x: f64, y: f64) -> GlobalPoint {
    let width = frame.width.max(1) as f64;
    let height = frame.height.max(1) as f64;
    GlobalPoint {
        x: frame.x as f64 + x.clamp(0.0, 1.0) * width,
        y: frame.y as f64 + y.clamp(0.0, 1.0) * height,
    }
}

#[cfg(target_os = "windows")]
fn active_display_share(state: &SessionState, window_id: u32) -> bool {
    state.active_share_is_display(window_id)
}

#[cfg(not(target_os = "windows"))]
fn active_display_share(state: &SessionState, window_id: u32) -> bool {
    state.is_display_share(window_id)
}

fn resolved_replay_target_pid(is_display_share: bool, window_pid: Option<i32>) -> Option<i32> {
    if is_display_share {
        // A display is a monitor, not an HWND-owning process. The replay
        // pipeline only needs a positive sentinel to pass the shared resolve
        // stage; display replay never uses it as a process identity.
        Some(1)
    } else {
        window_pid.filter(|pid| *pid > 0)
    }
}

#[cfg(test)]
mod target_pid_tests {
    use super::resolved_replay_target_pid;

    #[test]
    fn display_replay_does_not_require_a_window_pid() {
        assert_eq!(resolved_replay_target_pid(true, None), Some(1));
        assert_eq!(resolved_replay_target_pid(true, Some(42)), Some(1));
        assert_eq!(resolved_replay_target_pid(false, Some(42)), Some(42));
        assert_eq!(resolved_replay_target_pid(false, Some(0)), None);
    }
}

fn target_pid_for_window(state: &SessionState, window_id: u32) -> Option<i32> {
    if let Some(pid) = state.active_share_pid(window_id).filter(|pid| *pid > 0) {
        return Some(pid);
    }
    let now = Instant::now();
    if let Some(pid) = cached_target_pid(window_id, now) {
        return Some(pid);
    }
    let pid = crate::window_registry::global()
        .map(|r| r.owner_pid_fresh(window_id))
        .unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(window_id))
        .filter(|pid| *pid > 0)?;
    target_pid_cache().lock_unpoisoned().insert(
        window_id,
        CachedTargetPid {
            pid,
            cached_at: now,
        },
    );
    Some(pid)
}

fn cache_target_pid(window_id: u32, pid: Option<i32>) {
    if let Some(pid) = pid.filter(|pid| *pid > 0) {
        target_pid_cache().lock_unpoisoned().insert(
            window_id,
            CachedTargetPid {
                pid,
                cached_at: Instant::now(),
            },
        );
    }
}

fn cached_target_pid(window_id: u32, now: Instant) -> Option<i32> {
    let mut cache = target_pid_cache().lock_unpoisoned();
    let Some(cached) = cache.get(&window_id) else {
        return None;
    };
    if now.saturating_duration_since(cached.cached_at) <= TARGET_PID_CACHE_TTL {
        return Some(cached.pid);
    }
    cache.remove(&window_id);
    None
}

fn fresh_control_frame(state: &SessionState, window_id: u32) -> SharedWindowScreenStatus {
    match state.shared_window_screen_status(window_id) {
        SharedWindowScreenStatus::OnScreen(frame) => {
            if let Some(cached) = cached_control_frame(window_id, Instant::now()) {
                SharedWindowScreenStatus::OnScreen(cached)
            } else {
                update_control_frame(window_id, frame);
                SharedWindowScreenStatus::OnScreen(frame)
            }
        }
        status => {
            invalidate_control_frame(window_id);
            status
        }
    }
}

fn cached_control_frame(window_id: u32, now: Instant) -> Option<WindowFrame> {
    control_frame_cache()
        .lock_unpoisoned()
        .get(&window_id)
        .filter(|cached| now.saturating_duration_since(cached.cached_at) <= CONTROL_FRAME_CACHE_TTL)
        .map(|cached| cached.frame)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayCoalesceKey {
    window_id: u32,
    controller_id: String,
    stream: UnreliableSeqStream,
}

fn replay_coalesce_key(message: &RemoteControlMessage) -> Option<ReplayCoalesceKey> {
    unreliable_seq_stream(message).map(|stream| ReplayCoalesceKey {
        window_id: message.window_id,
        controller_id: message.controller_id.clone(),
        stream,
    })
}

fn replay_shards() -> &'static Mutex<HashMap<ReplayShardKey, ReplayShard>> {
    REPLAY_SHARDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// #369: get-or-create the shard for `key` and hand back its sender, spawning
/// a fresh shard worker thread if none exists yet. Must be called with
/// `shards` already locked -- the whole point is that finding/creating the
/// shard and (in the caller) sending to it happen under the same lock that
/// `reap_idle_shard_locked` uses to remove an idle shard, so a task can never
/// be sent to a channel whose receiver is about to be (or was just)
/// abandoned: either the shard still exists when we look it up (send
/// succeeds normally), or it was already removed and we spawn a fresh
/// replacement here.
fn shard_sender_locked(
    shards: &mut HashMap<ReplayShardKey, ReplayShard>,
    key: ReplayShardKey,
    inject: &ReplayInjector,
) -> mpsc::SyncSender<ReplayTask> {
    if let Some(shard) = shards.get(&key) {
        return shard.sender.clone();
    }
    let key = if key != ReplayShardKey::Unknown && shards.len() >= MAX_DEDICATED_REPLAY_SHARDS {
        log::warn!(
            "remote-control: replay shard pool at cap ({MAX_DEDICATED_REPLAY_SHARDS}); routing {key:?} through the shared overflow shard"
        );
        ReplayShardKey::Unknown
    } else {
        key
    };
    if let Some(shard) = shards.get(&key) {
        return shard.sender.clone();
    }
    let (tx, rx) = mpsc::sync_channel::<ReplayTask>(REPLAY_QUEUE_CAPACITY);
    let inject = Arc::clone(inject);
    // Fable-review fix: `Builder::spawn` instead of bare `thread::spawn` so a
    // spawn failure (OS thread-table exhaustion) cannot panic -- this
    // function is called from the single global resolver thread, so an
    // unhandled panic here would have killed remote control for the entire
    // process, not just this one shard.
    match std::thread::Builder::new()
        .name(format!("petal-rc-shard-{key:?}"))
        .spawn(move || replay_worker(key, rx, inject))
    {
        Ok(_join_handle) => {
            shards.insert(key, ReplayShard { sender: tx.clone() });
            log::debug!(
                "remote-control: spawned replay shard {key:?} ({} active)",
                shards.len()
            );
            tx
        }
        Err(error) => {
            log::error!(
                "remote-control: failed to spawn replay shard {key:?}: {error} -- events for this shard will fail fast this attempt; a later event may retry"
            );
            // Deliberately do NOT insert a shard entry: leaving no entry
            // means the NEXT event for this key retries spawning fresh
            // rather than being pinned to a permanently-broken shard.
            // `Builder::spawn` consumed and dropped the closure (along with
            // its captured `rx`) as part of failing, so every `try_send` on
            // this returned sender already fails immediately with
            // `Disconnected` (fail fast, observable via the existing
            // drop-reason logging) instead of silently queuing up to
            // `REPLAY_QUEUE_CAPACITY` tasks that can never be delivered.
            tx
        }
    }
}

/// #369: try to reap this shard if it's had no work for `REPLAY_SHARD_IDLE_TIMEOUT`.
/// Takes the same lock `shard_sender_locked` uses so the "is there really
/// nothing queued" check and "remove the shard" step are atomic with respect
/// to any concurrent enqueue -- see `shard_sender_locked`'s doc comment.
/// Returns a straggler task if one was sent in the race window between the
/// worker's `recv_timeout` elapsing and this function acquiring the lock (in
/// which case the shard is NOT reaped and the worker keeps running).
fn reap_idle_shard_locked(
    key: ReplayShardKey,
    rx: &mpsc::Receiver<ReplayTask>,
) -> Option<ReplayTask> {
    let mut shards = replay_shards().lock_unpoisoned();
    match rx.try_recv() {
        Ok(task) => Some(task),
        Err(_) => {
            if shards.remove(&key).is_some() {
                log::debug!(
                    "remote-control: reaped idle replay shard {key:?} after {:?} ({} active)",
                    REPLAY_SHARD_IDLE_TIMEOUT,
                    shards.len()
                );
            }
            None
        }
    }
}

fn resolver_queue(app: AppHandle) -> Arc<ResolveQueue> {
    RESOLVE_QUEUE
        .get_or_init(|| {
            let queue = Arc::new(ResolveQueue::new(RESOLVE_QUEUE_CAPACITY));
            let worker_queue = Arc::clone(&queue);
            std::thread::spawn(move || resolver_worker(app, worker_queue));
            queue
        })
        .clone()
}

fn resolver_worker(app: AppHandle, queue: Arc<ResolveQueue>) {
    loop {
        resolve_one_task(&app, queue.pop());
    }
}

fn replay_worker(key: ReplayShardKey, rx: mpsc::Receiver<ReplayTask>, inject: ReplayInjector) {
    let mut pending = VecDeque::new();
    while let Some(task) = next_replay_task(key, &rx, &mut pending) {
        let task = coalesce_ready_replay_task(task, &rx, &mut pending);
        let (task, continuation) = split_text_replay_task(task);
        replay_one_task(task, &inject);
        if let Some(continuation) = continuation {
            // Give one already-queued input a turn between text slices. This
            // caps head-of-line blocking without allowing a sustained flood to
            // starve the remainder of the paste indefinitely.
            if let Ok(ready) = rx.try_recv() {
                pending.push_back(ready);
            }
            pending.push_back(continuation);
        }
    }
}

fn split_text_replay_task(mut task: ReplayTask) -> (ReplayTask, Option<ReplayTask>) {
    if task.message.message_type != RemoteControlType::Text {
        return (task, None);
    }
    let Some(text) = task.message.text.take() else {
        return (task, None);
    };
    let Some((split_at, _)) = text.char_indices().nth(MAX_REPLAY_TEXT_SLICE_CHARS) else {
        task.message.text = Some(text);
        return (task, None);
    };

    let mut continuation = task.clone();
    task.message.text = Some(text[..split_at].to_string());
    continuation.message.text = Some(text[split_at..].to_string());
    task.terminal_on_success = false;
    (task, Some(continuation))
}

/// #369: like the old unconditional `rx.recv()`, but with an idle-reap
/// timeout so a shard whose target pid is no longer being controlled doesn't
/// keep an OS thread parked forever -- see `reap_idle_shard_locked`.
fn next_replay_task(
    key: ReplayShardKey,
    rx: &mpsc::Receiver<ReplayTask>,
    pending: &mut VecDeque<ReplayTask>,
) -> Option<ReplayTask> {
    if let Some(task) = pending.pop_front() {
        return Some(task);
    }
    match rx.recv_timeout(REPLAY_SHARD_IDLE_TIMEOUT) {
        Ok(task) => Some(task),
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
        Err(mpsc::RecvTimeoutError::Timeout) => reap_idle_shard_locked(key, rx),
    }
}

fn coalesce_ready_replay_task(
    mut task: ReplayTask,
    rx: &mpsc::Receiver<ReplayTask>,
    pending: &mut VecDeque<ReplayTask>,
) -> ReplayTask {
    let Some(key) = replay_coalesce_key(&task.message) else {
        return task;
    };
    loop {
        let Some(next) = pending.pop_front().or_else(|| rx.try_recv().ok()) else {
            return task;
        };
        if replay_coalesce_key(&next.message).as_ref() == Some(&key) {
            log::debug!(
                "remote-control: coalescing {:?} replay for window {} controller='{}' seq {} -> {}",
                key.stream,
                key.window_id,
                key.controller_id,
                task.message.seq,
                next.message.seq
            );
            task = next;
        } else {
            pending.push_back(next);
            return task;
        }
    }
}

/// #369: outcome of racing one event's AX injection sequence against
/// `REPLAY_EVENT_DEADLINE` -- see `run_replay_with_deadline`.
enum ReplayRunOutcome {
    Completed(Result<(), String>),
    TimedOut,
}

/// #369: run `inject` (the full AX sequence for one replay event) with a soft
/// deadline. AX/ObjC calls are blocking FFI with no safe cross-thread
/// cancellation point, so this cannot truly interrupt a hung call -- instead
/// it races the call on its own thread and, if the deadline passes first,
/// stops waiting ("abandons" the event) while letting that thread finish (or
/// keep hanging) in the background; its eventual result is discarded (the
/// `send` on a channel nobody is still receiving from just returns an error,
/// which is ignored). This means a permanently-hung target process can
/// accumulate abandoned threads for as long as it stays hung -- an accepted
/// tradeoff of "abandon and move on" without true preemption, and one that
/// stays isolated to that one pid's shard (see `replay_worker`) rather than
/// affecting other shared windows.
///
/// Fable-review fix: on abandon, `cancelled` is flipped so any side effect
/// the abandoned thread later attempts (gesture-map insert/remove, AX/SL
/// action) can detect it via `injection_was_cancelled()` and skip performing
/// the effect -- see that function's doc comment. This is a routine path,
/// not a hung-app-only one: `inject` can legally chain several AX calls each
/// individually capped at `AX_APP_MESSAGING_TIMEOUT_SECONDS`, so a merely
/// slow (not hung) target app can exceed `REPLAY_EVENT_DEADLINE` too.
///
/// Also: uses `thread::Builder` instead of bare `thread::spawn` so a spawn
/// failure (OS thread-table exhaustion) cannot panic -- a panic here would
/// unwind through this shard's worker thread (killing control of every
/// window sharing this target pid) or, if this ever runs on a shared/global
/// caller, worse. A spawn failure is treated as an ordinary injection
/// failure instead.
fn run_replay_with_deadline(task: &ReplayTask, inject: &ReplayInjector) -> ReplayRunOutcome {
    let shard_key = ReplayShardKey::for_task(task);
    if !active_injection_keys().lock_unpoisoned().insert(shard_key) {
        return ReplayRunOutcome::Completed(Err(
            "previous target injection is still in progress".to_string()
        ));
    }
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let inject = Arc::clone(inject);
    let message = task.message.clone();
    let frame = task.frame;
    let target_pid = task.target_pid;
    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = Arc::clone(&cancelled);
    let thread_shard_key = shard_key;
    let spawn_result = std::thread::Builder::new()
        .name("petal-rc-inject".to_string())
        .spawn(move || {
            INJECTION_CANCELLED.with(|cell| *cell.borrow_mut() = Some(thread_cancelled));
            let result = inject(&message, frame, target_pid);
            active_injection_keys()
                .lock_unpoisoned()
                .remove(&thread_shard_key);
            let _ = tx.send(result);
        });
    let _handle = match spawn_result {
        Ok(handle) => handle,
        Err(error) => {
            active_injection_keys().lock_unpoisoned().remove(&shard_key);
            log::warn!(
                "remote-control: failed to spawn injection thread for window {} controller='{}': {error}",
                task.message.window_id,
                task.message.controller_id
            );
            return ReplayRunOutcome::Completed(Err(format!(
                "failed to spawn injection thread: {error}"
            )));
        }
    };
    match rx.recv_timeout(REPLAY_EVENT_DEADLINE) {
        Ok(result) => ReplayRunOutcome::Completed(result),
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            ReplayRunOutcome::TimedOut
        }
    }
    // `_handle` drops here without joining -- `JoinHandle::drop` detaches
    // rather than blocking, so the spawned thread keeps running independently
    // exactly as intended for the abandon case.
}

fn replay_failure_code(error: &str) -> RemoteControlFailureCode {
    let error = error.to_ascii_lowercase();
    if error.contains("foreground") {
        RemoteControlFailureCode::NotForeground
    } else if error.contains("occluded") || error.contains("point belongs") {
        RemoteControlFailureCode::Occluded
    } else if error.contains("integrity") {
        RemoteControlFailureCode::IntegrityBlocked
    } else if error.contains("password") || error.contains("secure field") {
        RemoteControlFailureCode::SecureField
    } else if error.contains("share instance") || error.contains("capture instance is stale") {
        RemoteControlFailureCode::StaleShareInstance
    } else if error.contains("unsupported")
        || error.contains("not eligible")
        || error.contains("not invokable")
        || error.contains("no scrollable")
    {
        RemoteControlFailureCode::UnsupportedRoute
    } else if error.contains("not-injectible") || error.contains("not injectible") {
        // The op cannot be injected in the current (cursor-preserving) mode;
        // drives the user-initiated escalation affordance.
        RemoteControlFailureCode::NotInjectible
    } else if error.contains("unavailable") {
        RemoteControlFailureCode::TargetUnavailable
    } else {
        RemoteControlFailureCode::ReplayFailed
    }
}

/// The terminal success outcome for a completed replay. A window-share wheel
/// is delivered via `PostMessageW`, which only proves OS submission, not an
/// application effect — so it reports `submitted`, never `applied`. Everything
/// else that reached a successful injection keeps the established `applied`
/// semantics (macOS replays provably deliver events).
fn successful_replay_outcome(_message: &RemoteControlMessage) -> &'static str {
    #[cfg(target_os = "windows")]
    {
        // Window-share wheel is delivered via PostMessageW, which proves OS
        // submission but not application.
        let is_window = _message.effective_target_kind() == RemoteControlTargetKind::Window;
        if _message.message_type == RemoteControlType::Wheel && is_window {
            return "submitted";
        }
        // Cursor-preserving WINDOW ops are best-effort: discrete gestures are
        // cursor-restoring and keyboard/pointer may be message-driven into a
        // non-focused target that we cannot verify consumed them. Petal cannot
        // judge whether the event took effect, so it reports `submitted` (OS
        // submission only) and lets the user judge the result -- never a
        // guessed `applied`. Display shares and full-control (real global
        // input to the verified foreground) can still claim `applied`.
        if is_window
            && crate::windows_remote_control::share_mode(_message.window_id)
                == RemoteControlMode::CursorPreserving
        {
            return "submitted";
        }
    }
    "applied"
}

fn replay_one_task(task: ReplayTask, inject: &ReplayInjector) {
    if let (Some(admission), Some(sender)) = (&task.admission, &task.result_sender) {
        if !admission_is_still_inflight(admission, Instant::now()) {
            send_discrete_result(
                sender.clone(),
                admission.clone(),
                TerminalDisposition::failure(
                    "superseded",
                    RemoteControlDeliveryRoute::Replay,
                    RemoteControlFailureCode::Superseded,
                ),
            );
            return;
        }
    }
    if !is_current_replay_epoch(&task) {
        log::info!(
            "remote-control: dropping stale queued replay for window {} controller='{}' seq={} epoch={} current={}",
            task.message.window_id,
            task.message.controller_id,
            task.message.seq,
            task.replay_epoch,
            replay_epoch(task.message.window_id, &task.message.controller_id)
        );
        complete_replay_task(
            &task,
            TerminalDisposition::failure(
                "superseded",
                RemoteControlDeliveryRoute::Replay,
                RemoteControlFailureCode::Superseded,
            ),
            true,
        );
        return;
    }
    let started = Instant::now();
    let inject_started_ms = now_ms();
    if should_log_latency_probe(&task.message) {
        log::info!(
            "remote-control-latency: host inject_ts_ms={} {} target_pid={:?}",
            inject_started_ms,
            message_summary(&task.message),
            task.target_pid
        );
    }
    match run_replay_with_deadline(&task, inject) {
        ReplayRunOutcome::Completed(Ok(())) => {
            let injected_at_ms = now_ms();
            record_input_latency_marker(&task.message, injected_at_ms);
            #[cfg(target_os = "macos")]
            crate::session::note_remote_interaction(task.message.window_id, task.message.seq);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            record_latency_summary_success(elapsed_ms);
            if should_log_latency_probe(&task.message) {
                log::info!(
                    "remote-control-latency: host replay complete_ts_ms={} {} elapsed_ms={} target_pid={:?}",
                    injected_at_ms,
                    message_summary(&task.message),
                    elapsed_ms,
                    task.target_pid
                );
            }
            // #819: observer only -- see `cockpit_ledger`.
            #[cfg(feature = "cockpit-privileged")]
            cockpit_ledger::record_replay(&task.message, "applied");
            crate::analytics::note_remote_control_applied(&task.message);
            complete_replay_task(
                &task,
                TerminalDisposition::success(
                    successful_replay_outcome(&task.message),
                    RemoteControlDeliveryRoute::Replay,
                ),
                task.terminal_on_success,
            );
        }
        ReplayRunOutcome::Completed(Err(e)) => {
            log::warn!(
                "remote-control: replay failed for window {} from '{}': {e}",
                task.message.window_id,
                task.message.controller_id
            );
            record_latency_summary_failure();
            notify_replay_failure(&task.message, &e);
            // #819: observer only -- see `cockpit_ledger`.
            #[cfg(feature = "cockpit-privileged")]
            cockpit_ledger::record_replay(&task.message, "replayFailed");
            complete_replay_task(
                &task,
                TerminalDisposition::failure(
                    "replayFailed",
                    RemoteControlDeliveryRoute::Replay,
                    replay_failure_code(&e),
                ),
                true,
            );
        }
        ReplayRunOutcome::TimedOut => {
            log_input_drop(
                &task.message,
                RemoteControlInputDropReason::InjectionTimeout,
                &format!(
                    "ax-sequence-exceeded-{}ms-deadline",
                    REPLAY_EVENT_DEADLINE.as_millis()
                ),
            );
            log::warn!(
                "remote-control-latency: host replay timeout_ts_ms={} {} elapsed_ms={} target_pid={:?} deadline_ms={}",
                now_ms(),
                message_summary(&task.message),
                started.elapsed().as_millis(),
                task.target_pid,
                REPLAY_EVENT_DEADLINE.as_millis()
            );
            record_latency_summary_failure();
            notify_replay_failure(
                &task.message,
                &format!(
                    "injection timeout: ax-sequence-exceeded-{}ms-deadline",
                    REPLAY_EVENT_DEADLINE.as_millis()
                ),
            );
            // #819: observer only -- see `cockpit_ledger`.
            #[cfg(feature = "cockpit-privileged")]
            cockpit_ledger::record_replay(&task.message, "injectionTimeout");
            complete_replay_task(
                &task,
                TerminalDisposition::failure(
                    "replayFailed",
                    RemoteControlDeliveryRoute::Replay,
                    RemoteControlFailureCode::InjectionTimeout,
                ),
                true,
            );
        }
    }
}

fn complete_replay_task(task: &ReplayTask, disposition: TerminalDisposition, terminal: bool) {
    let Some(admission) = &task.admission else {
        return;
    };
    if terminal && complete_discrete_operation(admission, disposition) {
        if let Some(sender) = &task.result_sender {
            send_discrete_result(sender.clone(), admission.clone(), disposition);
        }
    }
}

fn enqueue_replay(task: ReplayTask) {
    enqueue_replay_with_injector(task, production_replay_injector());
}

/// #369: shared by production (`enqueue_replay`) and tests -- looks up (or
/// spawns) the shard for this task's target pid and sends non-blockingly.
/// Both the coalescable (pointer move/wheel) and discrete (click/key/text)
/// paths now use `try_send`: the old discrete path used a blocking `send`,
/// which -- now that resolution is fully decoupled from a single global
/// replay thread -- would let a saturated shard for one hung pid block the
/// single global `resolver_worker` thread itself (the exact cross-window
/// head-of-line blocking this issue is about), since `resolve_one_task` calls
/// this synchronously. Dropping under sustained backpressure (logged, same as
/// the existing high-rate path) is the correct tradeoff over ever blocking
/// the resolver.
///
/// Refs #288: both branches below also complete any v2 discrete admission
/// carried by the task. #369 made the discrete (click/key/text) path
/// non-blocking too, so a task can now be silently dropped by shard
/// backpressure (`Full`) exactly like a coalescable one -- without this, a
/// dropped discrete op would leave its admission entry "in flight" until
/// `DISCRETE_IN_FLIGHT_TTL` (5s) silently expires it with no terminal result
/// ever sent, and the controller's pending-op tracking would never resolve.
fn enqueue_replay_with_injector(task: ReplayTask, inject: &ReplayInjector) {
    let key = ReplayShardKey::for_task(&task);
    // Fable review (issue #369): the lookup-or-create AND the send must share
    // one lock acquisition with reap_idle_shard_locked's lock, not two
    // separate ones -- otherwise the guard from shard_sender_locked drops
    // before try_send runs, reopening exactly the reap race the doc comments
    // above claim is closed (a shard reaped between lookup and send silently
    // drops the task instead of routing to a freshly-spawned replacement).
    // try_send never blocks, so holding the lock across it is safe.
    let mut shards = replay_shards().lock_unpoisoned();
    let sender = shard_sender_locked(&mut shards, key, inject);
    let result = sender.try_send(task);
    drop(shards);
    match result {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(task)) => {
            record_replay_drop(&task.message);
            complete_replay_task(
                &task,
                TerminalDisposition::failure(
                    "replayFailed",
                    RemoteControlDeliveryRoute::Replay,
                    RemoteControlFailureCode::ReplayFailed,
                ),
                true,
            );
        }
        Err(mpsc::TrySendError::Disconnected(task)) => {
            log::warn!("remote-control: replay shard queue is closed");
            complete_replay_task(
                &task,
                TerminalDisposition::failure(
                    "superseded",
                    RemoteControlDeliveryRoute::Replay,
                    RemoteControlFailureCode::Superseded,
                ),
                true,
            );
        }
    }
}

fn record_replay_drop(message: &RemoteControlMessage) {
    let drops = REPLAY_HIGH_RATE_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
    if drops == 1 || drops % REPLAY_DROP_WARN_EVERY == 0 {
        log::warn!(
            "remote-control: dropped {} saturated high-rate replay event(s); latest window {} controller='{}' seq={}",
            drops,
            message.window_id,
            message.controller_id,
            message.seq
        );
    } else {
        log::debug!(
            "remote-control: dropping saturated high-rate replay for window {} controller='{}' seq={}",
            message.window_id,
            message.controller_id,
            message.seq
        );
    }
}

fn enqueue_resolve(app: &AppHandle, task: ResolveTask) {
    match resolver_queue(app.clone()).push(task) {
        ResolveQueuePush::Enqueued => {}
        ResolveQueuePush::Coalesced => {
            log::debug!("remote-control: coalesced queued high-rate resolve event");
        }
        ResolveQueuePush::Dropped(task) => record_resolve_drop(&task.message),
    }
}

fn record_resolve_drop(message: &RemoteControlMessage) {
    let drops = RESOLVE_HIGH_RATE_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
    if drops == 1 || drops % RESOLVE_DROP_WARN_EVERY == 0 {
        log::warn!(
            "remote-control: dropped {} saturated high-rate resolve event(s); latest window {} controller='{}' seq={}",
            drops,
            message.window_id,
            message.controller_id,
            message.seq
        );
    } else {
        log::debug!(
            "remote-control: dropping saturated high-rate resolve for window {} controller='{}' seq={}",
            message.window_id,
            message.controller_id,
            message.seq
        );
    }
}

#[cfg(test)]
fn replay_high_rate_drop_count() -> u32 {
    REPLAY_HIGH_RATE_DROPS.load(Ordering::Relaxed)
}

fn emit_status(app: &AppHandle, status: RemoteControlStatus) {
    if !should_deliver_status(&status, false) {
        return;
    }
    emit_status_unchecked(app, status);
}

fn emit_status_unchecked(app: &AppHandle, status: RemoteControlStatus) {
    log_status_emitted("local", &status);
    crate::region_window::emit_region_control_state_for_status(app, &status);
    // #819: observer only -- see `cockpit_ledger`.
    #[cfg(feature = "cockpit-privileged")]
    cockpit_ledger::record_status(&status);
    remote_control_engine().emit_status(&TauriControlSurface { app }, status);
}

fn emit_and_send_status(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    status: RemoteControlStatus,
) {
    if !should_deliver_status(&status, false) {
        return;
    }
    emit_and_send_status_unchecked(app, state, local_identity, status);
}

fn emit_and_send_status_forced(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    status: RemoteControlStatus,
) {
    if !should_deliver_status(&status, true) {
        return;
    }
    emit_and_send_status_unchecked(app, state, local_identity, status);
}

fn emit_and_send_status_unchecked(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    status: RemoteControlStatus,
) {
    log_status_emitted("local+controller", &status);
    crate::region_window::emit_region_control_state_for_status(app, &status);
    send_status_to_controller(state, local_identity, &status);
    let _ = app.emit("remote-control-status", status);
}

fn log_status_emitted(destination: &str, status: &RemoteControlStatus) {
    log::info!(
        "remote-control: status emitted ({destination}) status='{}' window={} controller='{}': {}",
        status.status,
        status.window_id,
        status.controller_id,
        status.message
    );
}

fn should_log_message(
    message_type: RemoteControlType,
    action: Option<RemoteControlAction>,
    seq: u64,
) -> bool {
    !matches!(
        (message_type, action),
        (RemoteControlType::Pointer, Some(RemoteControlAction::Move))
    ) || seq % 120 == 0
}

fn should_log_latency_probe(message: &RemoteControlMessage) -> bool {
    match message.message_type {
        RemoteControlType::Pointer => {
            should_log_message(message.message_type, message.action, message.seq)
        }
        RemoteControlType::Wheel => message.seq % 120 == 0,
        RemoteControlType::Key | RemoteControlType::Text => true,
        RemoteControlType::Request
        | RemoteControlType::Release
        | RemoteControlType::Status
        | RemoteControlType::Result
        | RemoteControlType::Unknown => false,
    }
}

fn message_summary(message: &RemoteControlMessage) -> String {
    let status = message.status.as_deref().unwrap_or("-");
    format!(
        "kind={:?} action={:?} window={} seq={} controller='{}' target='{}' status='{}'",
        message.message_type,
        message.action,
        message.window_id,
        message.seq,
        message.controller_id,
        message.target_user_id,
        status
    )
}

fn text_char_count(text: &str) -> usize {
    text.chars().count()
}

fn truncate_text_to_limit(text: &str) -> String {
    text.chars().take(MAX_REPLAY_TEXT_CHARS).collect()
}

fn remote_text_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for ch in text.chars() {
        if current_chars == MAX_REPLAY_TEXT_CHARS {
            chunks.push(current);
            current = String::new();
            current_chars = 0;
        }
        current.push(ch);
        current_chars += 1;
    }
    if !current.is_empty() || text.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn enforce_replay_text_limit(message: &mut RemoteControlMessage) -> Option<(usize, usize)> {
    if message.message_type != RemoteControlType::Text {
        return None;
    }
    let text = message.text.as_deref().unwrap_or("");
    let original_chars = text_char_count(text);
    if original_chars <= MAX_REPLAY_TEXT_CHARS {
        return None;
    }
    message.text = Some(truncate_text_to_limit(text));
    Some((original_chars, MAX_REPLAY_TEXT_CHARS))
}

fn known_status(status: &str) -> Option<&'static str> {
    match status {
        "active" => Some("active"),
        "stopped" => Some("stopped"),
        "disabled" => Some("disabled"),
        "accessibilityDenied" => Some("accessibilityDenied"),
        "requestFailed" => Some("requestFailed"),
        "targetPaused" => Some("targetPaused"),
        "targetUnavailable" => Some("targetUnavailable"),
        "requestUnavailable" => Some("requestUnavailable"),
        "notForeground" => Some("notForeground"),
        "occluded" => Some("occluded"),
        "integrityBlocked" => Some("integrityBlocked"),
        "secureField" => Some("secureField"),
        "unsupportedRoute" => Some("unsupportedRoute"),
        "staleShareInstance" => Some("staleShareInstance"),
        "injectionTimeout" => Some("injectionTimeout"),
        "awaitingConsent" => Some("awaitingConsent"),
        "denied" => Some("denied"),
        _ => None,
    }
}

fn status_packet_for(status: &RemoteControlStatus, local_identity: &str) -> RemoteControlMessage {
    // Refs #288: the v2 admission namespace is scoped by the SAME grant token
    // #377 already carries on `RemoteControlStatus` -- no separate control
    // session lookup. See `grant_is_current`.
    let has_grant = status.status == "active" && status.grant_token.is_some();
    let grant_keys: Vec<ControlGrantKey> = sessions()
        .lock_unpoisoned()
        .keys()
        .filter(|key| {
            key.window_id == status.window_id && key.controller_id == status.controller_id
        })
        .cloned()
        .collect();
    // Prefer the v2-scoped key (it carries target kind + share instance, so
    // the active status advertises the full capable envelope). The Windows
    // legacy mirror intentionally adds a second key; HashMap iteration order
    // is otherwise arbitrary and could select the markerless legacy key.
    let grant_key = grant_keys
        .iter()
        .find(|key| key.target_kind.is_some())
        .or_else(|| grant_keys.first())
        .cloned();
    let target_kind = grant_key.as_ref().and_then(|key| key.target_kind);
    let share_instance_id = grant_key
        .as_ref()
        .and_then(|key| key.share_instance_id.clone());
    // The v2-scoped grant key carries BOTH a target kind and a share instance
    // (`ControlGrantKey::for_message`); a legacy key carries neither.
    let has_capable_grant = has_grant && target_kind.is_some() && share_instance_id.is_some();
    #[cfg(target_os = "windows")]
    let host_capabilities = target_kind
        .map(|kind| crate::windows_remote_control::host_capabilities(status.window_id, kind))
        .unwrap_or_default();
    // A Mac host is a LEGACY host by contract -- docs/CONTRACTS.md: "the same
    // build continues to advertise legacy host behavior when sharing from Mac."
    // It never publishes a `share_instance_id`, so no controller can send the
    // capable envelope at it, so `target_kind` above is always None here and no
    // v2 grant key ever exists. Empty is the honest answer, NOT the #802 bug.
    #[cfg(not(target_os = "windows"))]
    let host_capabilities = Vec::new();
    RemoteControlMessage {
        v: VERSION,
        message_type: RemoteControlType::Status,
        action: None,
        target_user_id: status.controller_id.clone(),
        controller_id: local_identity.to_string(),
        window_id: status.window_id,
        seq: now_ms(),
        target_kind,
        share_instance_id,
        controller_capabilities: Vec::new(),
        host_capabilities,
        reason: status.reason,
        // #802: a LEGACY grant must not advertise a v2 control session. These
        // two fields are what make the controller's gate demand the rest of the
        // capable envelope (`targetKind`, `shareInstanceId`, a non-empty
        // `hostCapabilities`) -- which a legacy grant has none of, by design.
        // Emitting them anyway made every macOS grant self-contradictory: the
        // controller saw a v2 session with no v2 envelope, refused the whole
        // status, and dropped the grant token on the floor with no error on
        // either side. `has_grant` alone is NOT the condition; the grant key
        // must actually be the v2-scoped one. Measured live: 30/30 cases failed
        // with `grantToken: null` while the token was demonstrably on the wire.
        control_session_id: has_capable_grant
            .then(|| status.grant_token.clone())
            .flatten(),
        input_id: None,
        input_seq: None,
        operation_fingerprint_version: None,
        operation_fingerprint: None,
        outcome: None,
        delivery_route: None,
        failure_code: None,
        result_capability: has_capable_grant.then(result_capability),
        x: None,
        y: None,
        button: None,
        buttons: None,
        click_count: None,
        delta_x: None,
        delta_y: None,
        delta_mode: None,
        key: None,
        code: None,
        repeat: false,
        location: None,
        text: None,
        status: Some(status.status.to_string()),
        message: Some(status.message.clone()),
        grant_token: status.grant_token.clone(),
        // #370 corrective pass: unconditionally true on an "active" status --
        // its presence is the whole capability signal. Only a host running
        // this corrective-pass code path ever reaches this line at all, so
        // there is no version check to make: an old host's status packet
        // just never sets the field, which decodes as `false` by default.
        // Must be true on Windows too -- the host_capabilities/grant above
        // are negotiated regardless of platform, and the controller relies
        // on this flag to switch pointer/wheel sends to the binary encoding.
        supports_binary_hot_path: status.status == "active",
        modifiers: RemoteControlModifiers::default(),
    }
}

fn discrete_result_packet(
    local_identity: &str,
    admission: &DiscreteAdmission,
    disposition: TerminalDisposition,
) -> RemoteControlMessage {
    RemoteControlMessage {
        v: VERSION,
        message_type: RemoteControlType::Result,
        action: None,
        target_user_id: admission.controller_id.clone(),
        controller_id: local_identity.to_string(),
        window_id: admission.window_id,
        seq: now_ms(),
        target_kind: admission.target_kind,
        share_instance_id: admission.share_instance_id.clone(),
        controller_capabilities: Vec::new(),
        host_capabilities: Vec::new(),
        reason: None,
        control_session_id: Some(admission.control_session_id.clone()),
        input_id: Some(admission.input_id.clone()),
        input_seq: Some(admission.input_seq),
        operation_fingerprint_version: Some(1),
        operation_fingerprint: Some(admission.operation_fingerprint.clone()),
        outcome: Some(disposition.outcome.to_string()),
        delivery_route: Some(disposition.delivery_route),
        failure_code: disposition.failure_code,
        result_capability: None,
        x: None,
        y: None,
        button: None,
        buttons: None,
        click_count: None,
        delta_x: None,
        delta_y: None,
        delta_mode: None,
        key: None,
        code: None,
        repeat: false,
        location: None,
        text: None,
        status: None,
        message: None,
        grant_token: None,
        supports_binary_hot_path: false,
        modifiers: RemoteControlModifiers::default(),
    }
}

fn send_discrete_result(
    sender: TerminalResultSender,
    admission: DiscreteAdmission,
    disposition: TerminalDisposition,
) {
    let message = discrete_result_packet(&sender.local_identity, &admission, disposition);
    let summary = message_summary(&message);
    tauri::async_runtime::spawn(async move {
        if let Err(error) = publish_message(sender.publisher.clone(), message.clone()).await {
            // This is host-to-controller terminal delivery recovery, not the
            // controller retry protocol: retry capability remains advertised
            // as false. Re-emitting the same terminal packet is idempotent.
            log::warn!(
                "remote-control: result publish failed for {summary}; retrying once: {error}"
            );
            tokio::time::sleep(Duration::from_millis(75)).await;
            if let Err(retry_error) = publish_message(sender.publisher, message).await {
                log::warn!(
                    "remote-control: result publish recovery failed for {summary}: {retry_error}"
                );
            }
        } else {
            log::debug!("remote-control: published terminal result {summary}");
        }
    });
}

fn send_status_to_controller(
    state: &SessionState,
    local_identity: &str,
    status: &RemoteControlStatus,
) {
    let Some((publisher, _identity)) = state.control_channel_snapshot() else {
        log::warn!(
            "remote-control: cannot publish status '{}' for window {} to '{}' because no room is joined",
            status.status,
            status.window_id,
            status.controller_id
        );
        return;
    };
    let message = status_packet_for(status, local_identity);
    let summary = message_summary(&message);
    tauri::async_runtime::spawn(async move {
        if let Err(e) = publish_message(publisher, message).await {
            log::warn!("remote-control: status publish failed for {summary}: {e}");
        } else {
            log::info!("remote-control: published host status {summary}");
        }
    });
}

fn resolve_task_still_authorized(message: &RemoteControlMessage) -> bool {
    let authorized = is_authorized_input(message);
    if !authorized {
        log::debug!(
            "remote-control: dropping resolved input from '{}' for window {}, no longer authorized",
            message.controller_id,
            message.window_id
        );
    }
    authorized
}

fn resolve_one_task(app: &AppHandle, task: ResolveTask) {
    if !resolve_task_still_authorized(&task.message) {
        complete_resolve_task(
            &task,
            TerminalDisposition::failure(
                "unauthorized",
                RemoteControlDeliveryRoute::Resolve,
                RemoteControlFailureCode::Unauthorized,
            ),
        );
        return;
    }
    if task
        .admission
        .as_ref()
        .is_some_and(|admission| !grant_is_current(admission, Instant::now()))
    {
        complete_resolve_task(
            &task,
            TerminalDisposition::failure(
                "grantExpired",
                RemoteControlDeliveryRoute::Resolve,
                RemoteControlFailureCode::GrantExpired,
            ),
        );
        return;
    }
    if task
        .admission
        .as_ref()
        .is_some_and(|admission| !admission_is_still_inflight(admission, Instant::now()))
    {
        if let (Some(admission), Some(sender)) = (&task.admission, &task.result_sender) {
            send_discrete_result(
                sender.clone(),
                admission.clone(),
                TerminalDisposition::failure(
                    "superseded",
                    RemoteControlDeliveryRoute::Resolve,
                    RemoteControlFailureCode::Superseded,
                ),
            );
        }
        return;
    }
    let Some(state) = app.try_state::<SessionState>() else {
        log::warn!("remote-control: session state unavailable while resolving input");
        complete_resolve_task(
            &task,
            TerminalDisposition::failure(
                "resolveFailed",
                RemoteControlDeliveryRoute::Resolve,
                RemoteControlFailureCode::ResolveFailed,
            ),
        );
        return;
    };
    let state = state.inner();
    let admission = task.admission.clone();
    let result_sender = task.result_sender.clone();
    let mut message = task.message;
    let local_identity = task.local_identity.clone();
    let frame = match fresh_control_frame(state, message.window_id) {
        SharedWindowScreenStatus::OnScreen(frame) => frame,
        status @ SharedWindowScreenStatus::OffScreen => {
            drain_and_release_pressed(
                message.window_id,
                &message.controller_id,
                "target window left on-screen list",
            );
            emit_and_send_operation_feedback(
                app,
                state,
                &local_identity,
                &message,
                RemoteControlStatus {
                    window_id: message.window_id,
                    owner_identity: None,
                    controller_id: message.controller_id.clone(),
                    status: "targetPaused",
                    message: "Remote input was ignored because the shared window is minimized or off screen.".to_string(),
                    grant_token: None,
                    reason: None,
                },
            );
            log_input_drop(
                &message,
                classify_resolve_drop_reason(status),
                "window-off-screen",
            );
            complete_terminal_admission(
                &admission,
                &result_sender,
                TerminalDisposition::failure(
                    "targetOffScreen",
                    RemoteControlDeliveryRoute::Resolve,
                    RemoteControlFailureCode::TargetOffScreen,
                ),
            );
            return;
        }
        status @ SharedWindowScreenStatus::Closed => {
            // #372: symmetric with OffScreen above -- the window is gone, so
            // any input this controller was still holding down can never be
            // released by a normal Up/keyup packet. Drain it now instead of
            // leaving it for the (slower) TTL sweeper.
            drain_and_release_pressed(
                message.window_id,
                &message.controller_id,
                "target window closed",
            );
            emit_and_send_operation_feedback(
                app,
                state,
                &local_identity,
                &message,
                RemoteControlStatus {
                    window_id: message.window_id,
                    owner_identity: None,
                    controller_id: message.controller_id.clone(),
                    status: "targetUnavailable",
                    message:
                        "Remote input was ignored because the shared window is no longer available."
                            .to_string(),
                    grant_token: None,
                    reason: None,
                },
            );
            log_input_drop(
                &message,
                classify_resolve_drop_reason(status),
                "window-closed",
            );
            complete_terminal_admission(
                &admission,
                &result_sender,
                TerminalDisposition::failure(
                    "targetUnavailable",
                    RemoteControlDeliveryRoute::Resolve,
                    RemoteControlFailureCode::TargetUnavailable,
                ),
            );
            return;
        }
        status @ SharedWindowScreenStatus::NotShared => {
            emit_and_send_status(
                app,
                state,
                &local_identity,
                RemoteControlStatus {
                    window_id: message.window_id,
                    owner_identity: None,
                    controller_id: message.controller_id.clone(),
                    status: "stopped",
                    message: "Remote control stopped because sharing ended.".to_string(),
                    grant_token: None,
                    reason: None,
                },
            );
            log_input_drop(
                &message,
                classify_resolve_drop_reason(status),
                "window-not-shared",
            );
            complete_terminal_admission(
                &admission,
                &result_sender,
                TerminalDisposition::failure(
                    "targetUnavailable",
                    RemoteControlDeliveryRoute::Resolve,
                    RemoteControlFailureCode::TargetUnavailable,
                ),
            );
            return;
        }
    };
    // Displays have no owning process. Resolve their kind from the active
    // sharer-side share registry, not from the input envelope: legacy-shaped
    // Move/Down/Up pointer messages intentionally omit v2 target metadata.
    let resolved_target_pid = resolved_replay_target_pid(
        active_display_share(state, message.window_id),
        target_pid_for_window(state, message.window_id),
    );
    let Some(target_pid) = resolved_target_pid else {
        emit_and_send_operation_feedback(
            app,
            state,
            &local_identity,
            &message,
            RemoteControlStatus {
                window_id: message.window_id,
                owner_identity: None,
                controller_id: message.controller_id.clone(),
                status: "targetUnavailable",
                message: "Remote input was ignored because the target app could not be resolved."
                    .to_string(),
                grant_token: None,
                reason: None,
            },
        );
        log_input_drop(
            &message,
            RemoteControlInputDropReason::TargetUnavailable,
            "target-pid-unavailable",
        );
        complete_terminal_admission(
            &admission,
            &result_sender,
            TerminalDisposition::failure(
                "resolveFailed",
                RemoteControlDeliveryRoute::Resolve,
                RemoteControlFailureCode::ResolveFailed,
            ),
        );
        return;
    };
    cache_target_pid(message.window_id, Some(target_pid));
    if let Some((original_chars, capped_chars)) = enforce_replay_text_limit(&mut message) {
        log::warn!(
            "remote-control: truncating oversized text replay for window {} from '{}' ({} -> {} chars)",
            message.window_id,
            message.controller_id,
            original_chars,
            capped_chars
        );
        emit_and_send_status(
            app,
            state,
            &local_identity,
            RemoteControlStatus {
                window_id: message.window_id,
                owner_identity: None,
                controller_id: message.controller_id.clone(),
                status: "textTruncated",
                message: format!(
                    "Remote paste was limited to {capped_chars} characters; {dropped} characters were not inserted.",
                    dropped = original_chars.saturating_sub(capped_chars)
                ),
                grant_token: None,
                reason: None,
            },
        );
    }
    emit_status(
        app,
        RemoteControlStatus {
            window_id: message.window_id,
            owner_identity: None,
            controller_id: message.controller_id.clone(),
            status: "active",
            message: "Remote control active for shared window".to_string(),
            grant_token: active_grant_token(message.window_id, &message.controller_id),
            reason: None,
        },
    );
    // Option A trusts meeting peers and keeps replay non-focus-stealing:
    // AX-first replay plus pid-targeted fallbacks never raise the shared window.
    // The #209 cache here has pid/frame freshness, not z-order/occlusion;
    // confirming frontmost-at-point would require a per-event CG scan.
    publish_control_activity(state, &message);
    if should_log_message(message.message_type, message.action, message.seq) {
        log::info!(
            "remote-control: enqueue replay {} frame=({},{} {}x{}) target_pid={}",
            message_summary(&message),
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            target_pid
        );
    }
    if should_log_latency_probe(&message) {
        log::info!(
            "remote-control-latency: host enqueue_ts_ms={} {} target_pid={}",
            now_ms(),
            message_summary(&message),
            target_pid
        );
    }
    let synthetic_releases = track_pressed_input(&message, frame, Some(target_pid));
    enqueue_synthetic_releases(synthetic_releases, "held input reconciled");
    let mut replay = replay_task(message, frame, Some(target_pid), false);
    replay.admission = admission;
    replay.result_sender = result_sender;
    enqueue_replay(replay);
}

fn complete_resolve_task(task: &ResolveTask, disposition: TerminalDisposition) {
    complete_terminal_admission(&task.admission, &task.result_sender, disposition);
}

fn complete_terminal_admission(
    admission: &Option<DiscreteAdmission>,
    sender: &Option<TerminalResultSender>,
    disposition: TerminalDisposition,
) {
    let Some(admission) = admission else {
        return;
    };
    if complete_discrete_operation(admission, disposition) {
        if let Some(sender) = sender {
            send_discrete_result(sender.clone(), admission.clone(), disposition);
        }
    }
}
fn controller_result_feedback(
    failure_code: Option<RemoteControlFailureCode>,
) -> Option<(&'static str, &'static str)> {
    match failure_code? {
        RemoteControlFailureCode::NotForeground => Some((
            "notForeground",
            "Bring the shared target to the foreground, then try again.",
        )),
        RemoteControlFailureCode::Occluded => {
            Some(("occluded", "The shared target is covered at that point."))
        }
        RemoteControlFailureCode::IntegrityBlocked => Some((
            "integrityBlocked",
            "Windows blocked control because the target has higher privileges.",
        )),
        RemoteControlFailureCode::SecureField => {
            Some(("secureField", "Remote input is blocked for secure fields."))
        }
        RemoteControlFailureCode::UnsupportedRoute => Some((
            "unsupportedRoute",
            "That control is not supported for this shared app.",
        )),
        RemoteControlFailureCode::StaleShareInstance => Some((
            "staleShareInstance",
            "The shared target changed. Start remote control again.",
        )),
        RemoteControlFailureCode::InjectionTimeout => Some((
            "injectionTimeout",
            "Windows did not accept the remote input in time.",
        )),
        RemoteControlFailureCode::TargetOffScreen | RemoteControlFailureCode::TargetUnavailable => {
            Some(("targetUnavailable", "The shared target is unavailable."))
        }
        RemoteControlFailureCode::Unknown => None,
        _ => Some(("requestFailed", "Remote input was not accepted.")),
    }
}

fn send_discrete_result_from_state(
    state: &SessionState,
    local_identity: &str,
    admission: DiscreteAdmission,
    disposition: TerminalDisposition,
) {
    let Some((publisher, _)) = state.control_channel_snapshot() else {
        return;
    };
    send_discrete_result(
        TerminalResultSender {
            publisher,
            local_identity: local_identity.to_string(),
        },
        admission,
        disposition,
    );
}

fn handle_message(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    trusted_sender: Option<String>,
    message: RemoteControlMessage,
) {
    if message.v != VERSION {
        log::debug!("remote-control: dropping unsupported version {}", message.v);
        return;
    }
    if message.target_user_id != local_identity {
        if should_log_message(message.message_type, message.action, message.seq) {
            log::debug!(
                "remote-control: ignoring packet for another target: {} local='{}'",
                message_summary(&message),
                local_identity
            );
        }
        return;
    }
    // #493: Bind LiveKit identity before authorization; reordering this call
    // lets input auth observe the packet's spoofable controllerId.
    let Some(message) = bind_trusted_sender(trusted_sender, message) else {
        return;
    };
    if message.message_type == RemoteControlType::Unknown
        || message.action == Some(RemoteControlAction::Unknown)
    {
        log::debug!("remote-control: ignoring unknown kind/action packet");
        return;
    }
    if should_log_message(message.message_type, message.action, message.seq) {
        log::info!(
            "remote-control: received {} from trusted sender '{}'",
            message_summary(&message),
            message.controller_id
        );
    }
    if should_log_latency_probe(&message) {
        log::info!(
            "remote-control-latency: host receive_ts_ms={} {}",
            now_ms(),
            message_summary(&message)
        );
    }

    match message.message_type {
        RemoteControlType::Status => {
            let Some(status) = message.status.as_deref().and_then(known_status) else {
                log::warn!(
                    "remote-control: ignoring unknown status packet for window {} from '{}': {:?}",
                    message.window_id,
                    message.controller_id,
                    message.status
                );
                return;
            };
            if matches!(status, "stopped" | "disabled") {
                remote_clipboard::clear_pending_copy_for(
                    message.window_id,
                    Some(&message.controller_id),
                );
            }
            if !remote_window_exists(&message.controller_id, message.window_id) {
                log::warn!(
                    "remote-control: dropping status '{}' for window {} from '{}' because it is not the window owner",
                    status,
                    message.window_id,
                    message.controller_id
                );
                return;
            }
            #[cfg(target_os = "windows")]
            if !crate::windows_remote_control::record_controller_status(&message, status) {
                log::warn!(
                    "remote-control: dropping status '{}' for window {} from '{}' because its negotiated grant does not match the live share",
                    status,
                    message.window_id,
                    message.controller_id
                );
                return;
            }
            let status_message = message
                .message
                .clone()
                .unwrap_or_else(|| "Remote control status changed".to_string());
            log::info!(
                "remote-control: host '{}' reported status '{}' for window {}: {}",
                message.controller_id,
                status,
                message.window_id,
                status_message
            );
            #[cfg(target_os = "windows")]
            let overlay_active =
                match crate::windows_remote_control::controller_status_effect(status) {
                    crate::windows_remote_control::ControllerStatusEffect::Activate => Some(true),
                    crate::windows_remote_control::ControllerStatusEffect::Terminate => Some(false),
                    crate::windows_remote_control::ControllerStatusEffect::Feedback => None,
                };
            #[cfg(not(target_os = "windows"))]
            let overlay_active = Some(status == "active");
            if let Some(active) = overlay_active {
                if let Err(e) = set_remote_window_control_active(
                    app,
                    message.window_id,
                    Some(&message.controller_id),
                    active,
                ) {
                    log::warn!(
                        "remote-control: failed to apply controller overlay status '{}' for window {}: {}",
                        status,
                        message.window_id,
                        e
                    );
                }
            }
            // #370 corrective pass: this is the controller's OWN receipt of the
            // host's status packet (`message.controllerId` on the wire holds the
            // HOST's identity for status/result kinds -- see the comment on
            // `RemoteControlMessage`). Record the host's advertised hot-path
            // capability here, keyed by (window, host), so our own subsequent
            // `publish_message` calls for this session know it is safe to switch
            // to the binary encoding. An old (or downgraded -- this project has
            // shipped revert releases) host's status packet never sets
            // `supports_binary_hot_path`, so this stays latest-wins, mirroring
            // the web side (`remoteControlUi.ts`'s `active.supportsBinaryHotPath
            // = message.supportsBinaryHotPath === true`): an insert-only cache
            // here would let a stale capable-entry survive a host version
            // rollback and force binary frames at an old host that can no
            // longer parse them, silently killing remote control.
            let key = (message.window_id, message.controller_id.clone());
            if status == "active" && message.supports_binary_hot_path {
                hot_path_capable_targets().lock_unpoisoned().insert(key);
            } else if matches!(status, "active" | "stopped" | "disabled") {
                hot_path_capable_targets().lock_unpoisoned().remove(&key);
            }
            emit_status(
                app,
                RemoteControlStatus {
                    window_id: message.window_id,
                    owner_identity: Some(message.controller_id.clone()),
                    controller_id: message.controller_id,
                    status,
                    message: status_message,
                    grant_token: message.grant_token,
                    reason: None,
                },
            );
        }
        RemoteControlType::Request => {
            log::info!(
                "remote-control: request received from '{}' for shared window {}",
                message.controller_id,
                message.window_id
            );
            if state.active_share_frame(message.window_id).is_none() {
                log::info!(
                    "remote-control: ignoring request from '{}' for inactive window {}",
                    message.controller_id,
                    message.window_id
                );
                emit_and_send_status(
                    app,
                    state,
                    local_identity,
                    RemoteControlStatus {
                        window_id: message.window_id,
                        owner_identity: None,
                        controller_id: message.controller_id,
                        status: "requestUnavailable",
                        message: "Remote control is not available because this window is not being shared.".to_string(),
                        grant_token: None,
                        reason: None,
                    },
                );
                return;
            }
            #[cfg(target_os = "windows")]
            if let Err(error) =
                crate::windows_remote_control::validate_host_request(state, &message)
            {
                log::warn!(
                    "remote-control: Windows request rejected for window {}: {error}",
                    message.window_id
                );
                emit_and_send_status(
                    app,
                    state,
                    local_identity,
                    RemoteControlStatus {
                        window_id: message.window_id,
                        owner_identity: None,
                        controller_id: message.controller_id.clone(),
                        status: "requestUnavailable",
                        message: "Remote control is unavailable for this share.".to_string(),
                        grant_token: None,
                        reason: None,
                    },
                );
                return;
            }
            // User-initiated escalation (Step 3D.2): the controller asks the
            // sharer for FullControl of a cursor-preserving share. It uses the
            // same non-activating consent panel as ordinary control requests;
            // the mode flips ONLY after an explicit, revalidated approval.
            if message.reason == Some(RemoteControlReason::RequestEscalation) {
                log::info!(
                    "remote-control: escalation requested by '{}' for shared window {}",
                    message.controller_id,
                    message.window_id
                );
                #[cfg(target_os = "windows")]
                {
                    let eligible = state.remote_control_allowed()
                        && requester_is_present_in_room(state, &message.controller_id)
                        && is_authorized(message.window_id, &message.controller_id)
                        && crate::windows_remote_control::share_mode(message.window_id)
                            != RemoteControlMode::FullControl;
                    if eligible && park_escalation(message.window_id, &message.controller_id) {
                        let payload = ControlConsentRequestedPayload {
                            kind: ControlConsentPromptKind::FullControlEscalation,
                            window_id: message.window_id,
                            controller_id: message.controller_id.clone(),
                            controller_name: controller_display_name(state, &message.controller_id),
                            window_title: state.active_share_source_title(message.window_id),
                            timeout_ms: ESCALATION_TIMEOUT.as_millis() as u64,
                        };
                        let _ = app.emit("control-consent-requested", payload);
                        let controller_id = message.controller_id.clone();
                        let window_id = message.window_id;
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(ESCALATION_TIMEOUT).await;
                            if take_escalation(window_id, &controller_id) {
                                log::info!(
                                    "remote-control: full-control escalation for '{controller_id}' on window {window_id} expired; leaving cursor-preserving mode"
                                );
                            }
                        });
                    }
                }
                return;
            }
            let policy = state.remote_control_policy();
            let request_gate = if !policy.allows_requests() {
                RequestGate::Disabled
            } else if !state.share_allows_remote_control(message.window_id) {
                // Per-share lock (the sharer denied THIS window, or the window
                // is not actually shared by us). Refused here so no grant is
                // ever minted, rather than relying on the input gate below.
                RequestGate::Disabled
            } else if !requester_is_present_in_room(state, &message.controller_id) {
                RequestGate::RequesterNotPresent
            } else if policy == RemoteControlPolicy::Ask
                && !is_authorized(message.window_id, &message.controller_id)
            {
                // Consent flow: park the request and ask the sharer. A
                // re-request from a controller that ALREADY holds a grant is
                // idempotent (#374) and is answered with `active` below
                // instead of prompting again.
                RequestGate::AwaitingConsent
            } else {
                RequestGate::Allowed
            };
            if request_gate == RequestGate::AwaitingConsent {
                park_consent_request(app, state, local_identity, message);
                return;
            }
            let Some(grant_token) = apply_request_gate_for_message(&message, request_gate) else {
                let (status, status_message) = match request_gate {
                    RequestGate::Disabled => {
                        log::info!(
                            "remote-control: dropping request from '{}' for shared window {} because host disabled control",
                            message.controller_id,
                            message.window_id
                        );
                        (
                            "disabled",
                            "Remote control is disabled for this meeting".to_string(),
                        )
                    }
                    RequestGate::RequesterNotPresent => {
                        log::warn!(
                            "remote-control: dropping request from '{}' for shared window {} because requester is not a current room participant",
                            message.controller_id,
                            message.window_id
                        );
                        (
                            "requestUnavailable",
                            "Remote control request denied because the requester is not in this meeting.".to_string(),
                        )
                    }
                    RequestGate::Allowed | RequestGate::AwaitingConsent => {
                        unreachable!("allowed request gate must authorize")
                    }
                };
                emit_and_send_status(
                    app,
                    state,
                    local_identity,
                    RemoteControlStatus {
                        window_id: message.window_id,
                        owner_identity: None,
                        controller_id: message.controller_id,
                        status,
                        message: status_message,
                        grant_token: None,
                        reason: None,
                    },
                );
                return;
            };
            complete_granted_request(app, state, local_identity, message, grant_token);
        }
        RemoteControlType::Release => {
            revoke(message.window_id, &message.controller_id);
            log::info!(
                "remote-control: '{}' released control of local shared window {}",
                message.controller_id,
                message.window_id
            );
            let app_for_quality = app.clone();
            let _quality_window_id = message.window_id;
            tauri::async_runtime::spawn(async move {
                if let Some(_state) = app_for_quality.try_state::<SessionState>() {
                    #[cfg(target_os = "macos")]
                    crate::session::reconcile_quality_after_remote_control_release(
                        _state.inner(),
                        _quality_window_id,
                    )
                    .await;
                }
            });
            emit_and_send_status(
                app,
                state,
                local_identity,
                RemoteControlStatus {
                    window_id: message.window_id,
                    owner_identity: None,
                    controller_id: message.controller_id,
                    status: "stopped",
                    message: "Remote control stopped".to_string(),
                    grant_token: None,
                    reason: None,
                },
            );
        }
        // Result packets are controller-directed. Capable native controllers
        // surface privacy-safe replay failures without ending the grant.
        RemoteControlType::Result => {
            if !remote_window_exists(&message.controller_id, message.window_id) {
                log::warn!(
                    "remote-control: dropping result for window {} from '{}' because it is not the window owner",
                    message.window_id,
                    message.controller_id
                );
                return;
            }
            #[cfg(target_os = "windows")]
            if !crate::windows_remote_control::accept_controller_result(&message) {
                log::warn!(
                    "remote-control: dropping uncorrelated result for window {} from '{}'",
                    message.window_id,
                    message.controller_id
                );
                return;
            }
            let Some((status, feedback)) = controller_result_feedback(message.failure_code) else {
                return;
            };
            emit_status(
                app,
                RemoteControlStatus {
                    window_id: message.window_id,
                    owner_identity: Some(message.controller_id.clone()),
                    controller_id: message.controller_id,
                    status,
                    message: feedback.to_string(),
                    grant_token: None,
                    reason: None,
                },
            );
        }
        RemoteControlType::Pointer
        | RemoteControlType::Wheel
        | RemoteControlType::Key
        | RemoteControlType::Text => {
            // Preserve the original short-circuit order. In particular,
            // `should_accept_unreliable_seq` mutates the high-rate watermark,
            // so a disabled or unauthorized packet must never advance it.
            // AND of the meeting-wide policy and this window's own lock. The
            // per-share half is re-read per packet, so revoking mid-session
            // stops input already in flight -- `plan_input_dispatch` turns a
            // false here into a reject WITH revoke_control.
            let remote_control_allowed = state.remote_control_allowed()
                && state.share_allows_remote_control(message.window_id);
            let authorized = remote_control_allowed
                .then(|| is_authorized_input(&message))
                .unwrap_or(true);
            let unreliable_seq_accepted = (remote_control_allowed && authorized)
                .then(|| should_accept_unreliable_seq(&message))
                .unwrap_or(true);
            let accessibility_trusted =
                (remote_control_allowed && authorized && unreliable_seq_accepted)
                    .then(|| platform_control().accessibility_trusted())
                    .unwrap_or(true);
            let gates = InputGateSnapshot {
                remote_control_allowed,
                authorized,
                unreliable_seq_accepted,
                accessibility_trusted,
            };
            let mut action =
                plan_input_dispatch(gates, input_v2_snapshot_before_admission(&message));
            if matches!(action, InputDispatchAction::EvaluateV2Admission) {
                action = plan_input_dispatch(gates, resolve_input_v2_snapshot(&message));
            }
            match action {
                InputDispatchAction::Drop => {}
                InputDispatchAction::EvaluateV2Admission => {
                    unreachable!("input dispatch must resolve v2 admission before execution")
                }
                InputDispatchAction::Reject {
                    reason,
                    detail,
                    revoke_control,
                    status,
                    terminal,
                } => {
                    if revoke_control {
                        revoke(message.window_id, &message.controller_id);
                    }
                    if status == Some(InputDispatchStatus::AccessibilityDenied) {
                        // #372: Accessibility can be revoked mid-hold (a button/key
                        // already Down when the user flips the System Settings
                        // toggle). Without this, the held input orphans in
                        // pressed_inputs -- the TTL sweeper's synthetic release would
                        // also fail to inject while AX stays revoked, so drain it
                        // here immediately instead of waiting on that sweep.
                        drain_and_release_pressed(
                            message.window_id,
                            &message.controller_id,
                            "held-input-orphaned-ax-revoked",
                        );
                    }
                    // Preserve the existing rate limit for accessibility loss:
                    // a dropped high-rate stream must not flood status events.
                    let emit_drop = status != Some(InputDispatchStatus::AccessibilityDenied)
                        || should_log_message(message.message_type, message.action, message.seq);
                    if emit_drop {
                        log_input_drop(&message, reason, detail);
                    }
                    match status {
                        Some(InputDispatchStatus::Disabled) => emit_and_send_status(
                            app,
                            state,
                            local_identity,
                            RemoteControlStatus {
                                window_id: message.window_id,
                                owner_identity: None,
                                controller_id: message.controller_id.clone(),
                                status: "disabled",
                                message: "Remote control is disabled for this meeting".to_string(),
                                grant_token: None,
                                reason: None,
                            },
                        ),
                        Some(InputDispatchStatus::AccessibilityDenied) if emit_drop => {
                            emit_and_send_operation_feedback(
                                app,
                                state,
                                local_identity,
                                &message,
                                RemoteControlStatus {
                                    window_id: message.window_id,
                                    owner_identity: None,
                                    controller_id: message.controller_id.clone(),
                                    status: "accessibilityDenied",
                                    message: "Remote input was ignored because Petal needs Accessibility permission."
                                        .to_string(),
                                    grant_token: None,
                                    reason: None,
                                },
                            );
                        }
                        Some(InputDispatchStatus::AccessibilityDenied) | None => {}
                    }
                    if let Some((admission, outcome)) = terminal {
                        send_discrete_result_from_state(state, local_identity, admission, outcome);
                    }
                }
                InputDispatchAction::EnqueueResolve { admission } => {
                    // Windows host: an authorized remote input just landed for
                    // a local share — briefly raise that share's feed cadence
                    // so the receiver stays responsive (session boost).
                    #[cfg(target_os = "windows")]
                    crate::windows_remote_control::note_remote_input(message.window_id);
                    enqueue_resolve(
                        app,
                        ResolveTask {
                            message,
                            local_identity: local_identity.to_string(),
                            admission,
                            result_sender: state.control_channel_snapshot().map(
                                |(publisher, _)| TerminalResultSender {
                                    publisher,
                                    local_identity: local_identity.to_string(),
                                },
                            ),
                        },
                    )
                }
            }
        }
        RemoteControlType::Unknown => {
            log::debug!("remote-control: ignoring unknown message kind");
        }
    }
}

/// #820: how long a disconnect observation gets to prove itself against the
/// roster before it may revoke. Long enough to span a resume's teardown+re-add
/// (measured ~6.4s controller-absent window during a host resume under load),
/// short enough that a genuinely departed controller's grants do not linger.
const RECONNECT_REVOKE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

fn parse_clipboard_copy_request(payload: &[u8]) -> Option<RemoteClipboardCopyRequest> {
    let request = serde_json::from_slice::<RemoteClipboardCopyRequest>(payload).ok()?;
    (request.v == VERSION && request.kind == "copy").then_some(request)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardStreamMetadata {
    operation_id: String,
    direction: ClipboardStreamDirection,
    window_id: u32,
    grant_token: String,
    declared_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardStreamDirection {
    CopyResponse,
    Paste,
}

fn clipboard_stream_metadata(info: &livekit::ByteStreamInfo) -> Option<ClipboardStreamMetadata> {
    if info.topic != remote_clipboard::REMOTE_CLIPBOARD_TEXT_TOPIC
        || info.mime_type != remote_clipboard::REMOTE_CLIPBOARD_TEXT_MIME
        || info.attributes.len() != 4
    {
        return None;
    }
    let operation_id = info.attributes.get("operationId")?.clone();
    if !remote_clipboard::operation_id_is_valid(&operation_id) {
        return None;
    }
    let direction = match info.attributes.get("direction")?.as_str() {
        "copyResponse" => ClipboardStreamDirection::CopyResponse,
        "paste" => ClipboardStreamDirection::Paste,
        _ => return None,
    };
    let window_id = info.attributes.get("windowId")?.parse::<u32>().ok()?;
    let grant_token = info.attributes.get("grantToken")?.clone();
    if grant_token.is_empty() {
        return None;
    }
    let declared_length = usize::try_from(info.total_length?).ok()?;
    if !(1..=remote_clipboard::MAX_REMOTE_CLIPBOARD_TEXT_BYTES).contains(&declared_length) {
        return None;
    }
    Some(ClipboardStreamMetadata {
        operation_id,
        direction,
        window_id,
        grant_token,
        declared_length,
    })
}

async fn read_clipboard_stream(
    mut reader: livekit::ByteStreamReader,
    declared_length: usize,
) -> Option<Vec<u8>> {
    let deadline = Instant::now() + remote_clipboard::REMOTE_CLIPBOARD_OPERATION_TTL;
    let mut body = Vec::with_capacity(declared_length);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let next = tokio::time::timeout(remaining, reader.next()).await.ok()?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.ok()?;
        let new_length = body.len().checked_add(chunk.len())?;
        if new_length > declared_length
            || new_length > remote_clipboard::MAX_REMOTE_CLIPBOARD_TEXT_BYTES
        {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    if body.len() != declared_length || remote_clipboard::validate_remote_text(&body).is_err() {
        return None;
    }
    Some(body)
}

fn clipboard_copy_message_if_authorized(
    state: &SessionState,
    local_identity: &str,
    trusted_sender: &str,
    request: &RemoteClipboardCopyRequest,
) -> Option<RemoteControlMessage> {
    if request.target_user_id != local_identity
        || request.controller_id != trusted_sender
        || !remote_clipboard::operation_id_is_valid(&request.operation_id)
        || request.grant_token.is_empty()
        || request.target_kind.is_some() != request.share_instance_id.is_some()
        || request.target_kind == Some(RemoteControlTargetKind::Unknown)
        || state.active_share_frame(request.window_id).is_none()
        || active_display_share(state, request.window_id)
    {
        return None;
    }
    #[cfg(target_os = "windows")]
    if let (Some(target_kind), Some(share_instance_id)) =
        (request.target_kind, request.share_instance_id.as_deref())
    {
        use crate::windows_capture_target::TargetKind;
        let target = state.control_target_snapshot(request.window_id, share_instance_id)?;
        if target_kind != RemoteControlTargetKind::Window || target.kind != TargetKind::Window {
            return None;
        }
    }
    let message = clipboard_key_message(
        local_identity,
        trusted_sender,
        request.window_id,
        request.seq,
        &request.grant_token,
        request.target_kind,
        request.share_instance_id.clone(),
        ClipboardShortcut::Copy,
        RemoteControlAction::Down,
    );
    is_authorized_input(&message).then_some(message)
}

#[derive(Debug)]
struct CompletedClipboardCopy {
    operation_id: String,
    target_user_id: String,
    window_id: u32,
    grant_token: String,
    bytes: Vec<u8>,
}

fn execute_host_clipboard_copy(
    app: &AppHandle,
    message: RemoteControlMessage,
    operation_id: String,
    generation: &RoomGeneration,
) -> Option<CompletedClipboardCopy> {
    if !generation.is_current() {
        return None;
    }
    let _clipboard_lock = remote_clipboard::try_clipboard_operation_lock()?;
    if !remote_clipboard::reserve_copy_operation(
        &message.controller_id,
        &operation_id,
        Instant::now(),
    ) {
        return None;
    }
    let clipboard = remote_clipboard::system_clipboard();
    let before = clipboard.sequence().ok()?;
    let deadline = Instant::now() + remote_clipboard::REMOTE_COPY_OBSERVATION_DEADLINE;
    run_clipboard_shortcut(app, message.clone(), ClipboardShortcut::Copy).ok()?;
    loop {
        if !generation.is_current() {
            return None;
        }
        let sequence = clipboard.sequence().ok()?;
        if sequence != before {
            if !is_authorized_input(&message) {
                return None;
            }
            let bytes = clipboard.read_transfer_text().ok()?;
            #[cfg(feature = "cockpit-privileged")]
            cockpit_ledger::record_clipboard_replay(
                message.window_id,
                &message.controller_id,
                "copy",
                "applied",
            );
            return Some(CompletedClipboardCopy {
                operation_id,
                target_user_id: message.controller_id.clone(),
                window_id: message.window_id,
                grant_token: message.grant_token.clone()?,
                bytes,
            });
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn apply_clipboard_copy_response(
    sender_identity: &str,
    metadata: ClipboardStreamMetadata,
    body: Vec<u8>,
) {
    let now = Instant::now();
    let Some(pending) = remote_clipboard::pending_copy(now) else {
        return;
    };
    if pending.operation_id != metadata.operation_id
        || pending.owner_identity != sender_identity
        || pending.window_id != metadata.window_id
        || pending.grant_token != metadata.grant_token
    {
        return;
    }
    if !remote_window_exists(sender_identity, metadata.window_id) {
        remote_clipboard::clear_pending_copy_if_operation(&metadata.operation_id);
        return;
    }
    let Some(_clipboard_lock) = remote_clipboard::try_clipboard_operation_lock() else {
        return;
    };
    let Some(pending) = remote_clipboard::take_pending_copy_if(now, |current| {
        current.operation_id == metadata.operation_id
            && current.owner_identity == sender_identity
            && current.window_id == metadata.window_id
            && current.grant_token == metadata.grant_token
    }) else {
        return;
    };
    let clipboard = remote_clipboard::system_clipboard();
    if clipboard.sequence().ok() != Some(pending.local_clipboard_sequence) {
        return;
    }
    let Ok(text) = String::from_utf8(body) else {
        return;
    };
    if clipboard.write_text(&text).is_err() {
        return;
    }
}

fn host_clipboard_target_is_current(state: &SessionState, window_id: u32) -> bool {
    state.active_share_frame(window_id).is_some() && !active_display_share(state, window_id)
}

fn apply_clipboard_paste(
    app: &AppHandle,
    local_identity: &str,
    sender_identity: &str,
    metadata: ClipboardStreamMetadata,
    body: Vec<u8>,
    generation: &RoomGeneration,
) {
    if !generation.is_current() {
        return;
    }
    let Some(state) = app.try_state::<SessionState>() else {
        return;
    };
    if !host_clipboard_target_is_current(state.inner(), metadata.window_id) {
        return;
    }
    #[cfg(target_os = "windows")]
    let target_envelope = state
        .inner()
        .active_window_control_envelope(metadata.window_id)
        .map(|(target_kind, share_instance_id)| (Some(target_kind), Some(share_instance_id)))
        .unwrap_or((None, None));
    #[cfg(not(target_os = "windows"))]
    let target_envelope = (None, None);
    let message = clipboard_key_message(
        local_identity,
        sender_identity,
        metadata.window_id,
        now_ms(),
        &metadata.grant_token,
        target_envelope.0,
        target_envelope.1,
        ClipboardShortcut::Paste,
        RemoteControlAction::Down,
    );
    if !is_authorized_input(&message) {
        return;
    }
    let Some(_clipboard_lock) = remote_clipboard::try_clipboard_operation_lock() else {
        return;
    };
    if !generation.is_current() || !host_clipboard_target_is_current(state.inner(), metadata.window_id)
    {
        return;
    }
    if !is_authorized_input(&message)
        || !remote_clipboard::reserve_paste_operation(
            sender_identity,
            &metadata.operation_id,
            Instant::now(),
        )
    {
        return;
    }
    let Ok(text) = String::from_utf8(body) else {
        return;
    };
    if remote_clipboard::system_clipboard().write_text(&text).is_err() {
        return;
    }
    // The clipboard write is deliberately before target invocation. If the
    // target rejects the native operation, the host still retains the text.
    let replay_succeeded =
        run_clipboard_shortcut(app, message.clone(), ClipboardShortcut::Paste).is_ok();
    #[cfg(feature = "cockpit-privileged")]
    cockpit_ledger::record_clipboard_replay(
        metadata.window_id,
        &message.controller_id,
        "paste",
        if replay_succeeded { "applied" } else { "replayFailed" },
    );
    #[cfg(not(feature = "cockpit-privileged"))]
    let _ = replay_succeeded;
}

async fn handle_clipboard_stream(
    app: AppHandle,
    local_identity: String,
    sender_identity: String,
    metadata: ClipboardStreamMetadata,
    reader: livekit::ByteStreamReader,
    generation: RoomGeneration,
) {
    let Some(body) = read_clipboard_stream(reader, metadata.declared_length).await else {
        if metadata.direction == ClipboardStreamDirection::CopyResponse {
            remote_clipboard::clear_pending_copy_if_operation(&metadata.operation_id);
        }
        return;
    };
    if !generation.is_current() {
        return;
    }
    match metadata.direction {
        ClipboardStreamDirection::CopyResponse => {
            apply_clipboard_copy_response(&sender_identity, metadata, body);
        }
        ClipboardStreamDirection::Paste => {
            apply_clipboard_paste(
                &app,
                &local_identity,
                &sender_identity,
                metadata,
                body,
                &generation,
            );
        }
    }
}

fn dispatch_clipboard_copy_request(
    app: &AppHandle,
    state: &SessionState,
    local_identity: &str,
    trusted_sender: &str,
    request: RemoteClipboardCopyRequest,
    generation: &RoomGeneration,
) {
    let Some(message) = clipboard_copy_message_if_authorized(
        state,
        local_identity,
        trusted_sender,
        &request,
    ) else {
        return;
    };
    let app = app.clone();
    let generation_for_copy = generation.clone();
    let generation_for_send = generation_for_copy.clone();
    tauri::async_runtime::spawn(async move {
        let operation_id = request.operation_id.clone();
        let app_for_copy = app.clone();
        let generation_for_worker = generation_for_copy;
        let completed = tokio::task::spawn_blocking(move || {
            execute_host_clipboard_copy(
                &app_for_copy,
                message,
                operation_id,
                &generation_for_worker,
            )
        })
        .await
        .ok()
        .flatten();
        let Some(completed) = completed else {
            return;
        };
        if !generation_for_send.is_current() {
            return;
        }
        let Some(state) = app.try_state::<SessionState>() else {
            return;
        };
        let Some((publisher, _)) = state.inner().control_channel_snapshot() else {
            return;
        };
        let _ = send_clipboard_text_stream(
            publisher,
            completed.target_user_id,
            completed.operation_id,
            "copyResponse",
            completed.window_id,
            completed.grant_token,
            completed.bytes,
        )
        .await;
    });
}

fn roster_contains(room: &livekit::Room, identity: &str) -> bool {
    room.remote_participants()
        .keys()
        .any(|key| key.as_str() == identity)
}

pub fn start_receiver_for_room(
    app: &AppHandle,
    room: Arc<livekit::Room>,
    local_identity: String,
    generation: RoomGeneration,
) {
    ensure_pressed_ttl_sweeper();
    set_replay_status_context(app.clone(), local_identity.clone());
    log::info!(
        "remote-control: receiver starting for identity '{}' -- Accessibility {}",
        crate::logging::log_safe_quoted(&local_identity),
        if platform_control().accessibility_trusted() {
            "GRANTED"
        } else {
            "DENIED"
        }
    );
    let mut events = room.subscribe();
    let app = app.clone();
    // #820: kept alive so the disconnect handler can verify an event against
    // the CURRENT roster instead of trusting event order.
    let roster_room = Arc::clone(&room);
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("remote-control: receiver exiting for stale room generation");
                break;
            }
            match event {
                livekit::RoomEvent::DataReceived {
                    payload,
                    topic,
                    participant,
                    ..
                } => {
                    if topic.as_deref() != Some(TOPIC) {
                        continue;
                    }
                    let trusted_sender = participant.as_ref().map(|p| p.identity().to_string());
                    if payload.first().copied() != Some(BINARY_MAGIC) {
                        if let (Some(request), Some(sender), Some(state)) = (
                            parse_clipboard_copy_request(payload.as_slice()),
                            trusted_sender.as_deref(),
                            app.try_state::<SessionState>(),
                        ) {
                            dispatch_clipboard_copy_request(
                                &app,
                                state.inner(),
                                &local_identity,
                                sender,
                                request,
                                &generation,
                            );
                            continue;
                        }
                    }
                    let message = if payload.first().copied() == Some(BINARY_MAGIC) {
                        trusted_sender.as_deref().and_then(|sender| {
                            message_from_binary(&payload, &local_identity, sender)
                        })
                    } else {
                        serde_json::from_slice::<RemoteControlMessage>(&payload).ok()
                    };
                    let Some(message) = message else {
                        log::info!(
                            "remote-control: dropping malformed packet on topic {TOPIC} ({} bytes)",
                            payload.len()
                        );
                        continue;
                    };
                    let trusted_sender = trusted_sender;
                    if let Some(state) = app.try_state::<SessionState>() {
                        handle_message(
                            &app,
                            state.inner(),
                            &local_identity,
                            trusted_sender,
                            message,
                        );
                    }
                }
                livekit::RoomEvent::ByteStreamOpened {
                    reader,
                    topic,
                    participant_identity,
                } => {
                    if topic != remote_clipboard::REMOTE_CLIPBOARD_TEXT_TOPIC {
                        continue;
                    }
                    let Some(reader) = reader.take_if(|info| clipboard_stream_metadata(info).is_some())
                    else {
                        continue;
                    };
                    let Some(metadata) = clipboard_stream_metadata(reader.info()) else {
                        continue;
                    };
                    remote_clipboard::prune_paste_operations(Instant::now());
                    let app = app.clone();
                    let local_identity = local_identity.clone();
                    let sender_identity = participant_identity.to_string();
                    let generation = generation.clone();
                    tauri::async_runtime::spawn(handle_clipboard_stream(
                        app,
                        local_identity,
                        sender_identity,
                        metadata,
                        reader,
                        generation,
                    ));
                }
                livekit::RoomEvent::ParticipantDisconnected(participant) => {
                    let identity = participant.identity().to_string();
                    // #820: a full/resume reconnect replays a DELAYED
                    // ParticipantDisconnected for peers that never left, and
                    // this handler used to trust event order -- measured live:
                    // a request GRANTED at t was revoked at t+1.3s by exactly
                    // such an aftershock. The roster is the authority, not the
                    // event (the events-lie-after-reconnect lesson of #631's
                    // publication reconcile) -- BUT a point-in-time roster
                    // read is not enough either: during the host's own resume
                    // the controller is briefly absent from the map while its
                    // stale disconnect is delivered (measured: the aftershock
                    // fired 1.0s into a resume and the immediate check read
                    // "absent", so the revoke still went through). So: skip
                    // immediately when the participant is demonstrably
                    // present, otherwise CONFIRM the departure against the
                    // roster after a grace window before revoking. A genuine
                    // departure still revokes -- just up to
                    // RECONNECT_REVOKE_GRACE later; a disconnected controller
                    // can send no input in the meantime, and held-input
                    // safety is already covered by the pressed-TTL sweeper.
                    // The #759 revoke-on-departure posture is unchanged.
                    // Held-input safety is IMMEDIATE regardless of whether
                    // the event is stale: a stuck press is worse than a lost
                    // press, the harness's disconnect case asserts the
                    // synthetic release within 5s, and a controller whose
                    // pointer is genuinely still down will simply re-send.
                    // Only the GRANT revocation waits for confirmation.
                    let releases = drain_pressed_for_controller_id(&identity);
                    if !releases.is_empty() {
                        enqueue_synthetic_releases(releases, "controller disconnected");
                    }
                    if roster_contains(&roster_room, &identity) {
                        log::warn!(
                            "remote-control: ignoring stale ParticipantDisconnected for '{}' ({identity}) -- still in the room roster (reconnect aftershock, #820); grants kept",
                            participant.name()
                        );
                        continue;
                    }
                    log::warn!(
                        "remote-control: '{}' ({identity}) disconnected -- confirming departure against the roster for {}s before revoking (#820)",
                        participant.name(),
                        RECONNECT_REVOKE_GRACE.as_secs()
                    );
                    let app = app.clone();
                    let roster_room = Arc::clone(&roster_room);
                    let generation = generation.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(RECONNECT_REVOKE_GRACE).await;
                        if !generation.is_current() {
                            return;
                        }
                        if roster_contains(&roster_room, &identity) {
                            log::warn!(
                                "remote-control: '{identity}' is back in the roster after the grace window -- stale disconnect during a reconnect (#820); grants kept"
                            );
                            return;
                        }
                        log::warn!(
                            "remote-control: '{identity}' confirmed absent after the grace window -- revoking any active remote-control grants they held"
                        );
                        revoke_controller(&app, &identity, "controller disconnected");
                    });
                }
                _ => {}
            }
        }
        if generation.is_current() {
            remote_clipboard::clear_pending_copy();
            remote_clipboard::clear_copy_operations();
            remote_clipboard::clear_paste_operations();
        }
    });
}

fn publish_control_activity(state: &SessionState, message: &RemoteControlMessage) {
    use crate::telepointer::PointerActivity;

    let key = (message.window_id, message.controller_id.clone());
    let mut last_positions = controller_pointer_positions().lock_unpoisoned();
    if let (Some(x), Some(y)) = (message.x, message.y) {
        last_positions.insert(key.clone(), (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
    }

    let activity = match message.message_type {
        RemoteControlType::Pointer if message.action == Some(RemoteControlAction::Down) => {
            Some(PointerActivity::Click)
        }
        RemoteControlType::Key | RemoteControlType::Text => Some(PointerActivity::Type),
        _ => None,
    };
    let Some(activity) = activity else {
        return;
    };

    let (x, y) = message
        .x
        .zip(message.y)
        .map(|(x, y)| (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)))
        .or_else(|| last_positions.get(&key).copied())
        .unwrap_or((0.5, 0.5));
    drop(last_positions);

    let Some((publisher, _identity)) = state.control_channel_snapshot() else {
        return;
    };
    crate::telepointer::publish_activity(
        &publisher,
        message.window_id,
        message.controller_id.clone(),
        x,
        y,
        activity,
    );
}

async fn publish_message(
    room_connection: Arc<RoomConnection>,
    message: RemoteControlMessage,
) -> Result<(), String> {
    remote_control_engine()
        .publish_message(room_connection, message)
        .await
}

async fn publish_message_with_retry(
    room_connection: Arc<RoomConnection>,
    message: RemoteControlMessage,
    attempts: usize,
    retry_delay: Duration,
) -> Result<usize, String> {
    let attempts = attempts.max(1);
    let summary = message_summary(&message);
    let mut last_error = None;
    for attempt in 1..=attempts {
        match publish_message(room_connection.clone(), message.clone()).await {
            Ok(()) => {
                if attempt > 1 {
                    log::info!(
                        "remote-control: publish succeeded on retry attempt {attempt}/{attempts}: {summary}"
                    );
                }
                return Ok(attempt);
            }
            Err(e) => {
                log::warn!(
                    "remote-control: publish attempt {attempt}/{attempts} failed for {summary}: {e}"
                );
                last_error = Some(e);
                if attempt < attempts {
                    tokio::time::sleep(retry_delay).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "publish remote-control data failed".to_string()))
}

fn viewer_channel(
    app: &AppHandle,
    window_id: u32,
    owner_identity: Option<&str>,
) -> Result<(Arc<RoomConnection>, String, String), String> {
    let state = app
        .try_state::<SessionState>()
        .ok_or_else(|| "session state is not available".to_string())?;
    let (room_connection, controller_id) = state
        .inner()
        .control_channel_snapshot()
        .ok_or_else(|| "join a room before using remote control".to_string())?;
    let target_user_id = remote_window_owner(window_id, owner_identity)
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
    Ok((room_connection, controller_id, target_user_id))
}

async fn publish_clipboard_copy_request(
    publisher: Arc<RoomConnection>,
    target_user_id: String,
    request: RemoteClipboardCopyRequest,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&request)
        .map_err(|error| format!("serialize remote clipboard Copy request: {error}"))?;
    let packet = livekit::DataPacket {
        payload,
        topic: Some(TOPIC.to_string()),
        reliable: true,
        destination_identities: vec![livekit::prelude::ParticipantIdentity(target_user_id)],
    };
    publisher
        .room()
        .local_participant()
        .publish_data(packet)
        .await
        .map_err(|error| format!("publish remote clipboard Copy request: {error}"))
}

async fn send_clipboard_text_stream(
    publisher: Arc<RoomConnection>,
    target_user_id: String,
    operation_id: String,
    direction: &str,
    window_id: u32,
    grant_token: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    remote_clipboard::validate_remote_text(&bytes).map_err(|error| error.to_string())?;
    let mut attributes = HashMap::new();
    attributes.insert("operationId".to_string(), operation_id);
    attributes.insert("direction".to_string(), direction.to_string());
    attributes.insert("windowId".to_string(), window_id.to_string());
    attributes.insert("grantToken".to_string(), grant_token);
    let options = livekit::StreamByteOptions {
        topic: remote_clipboard::REMOTE_CLIPBOARD_TEXT_TOPIC.to_string(),
        attributes,
        destination_identities: vec![livekit::prelude::ParticipantIdentity(target_user_id)],
        id: None,
        mime_type: Some(remote_clipboard::REMOTE_CLIPBOARD_TEXT_MIME.to_string()),
        name: Some(String::new()),
        total_length: Some(bytes.len() as u64),
    };
    publisher
        .room()
        .local_participant()
        .send_bytes(bytes, options)
        .await
        .map(|_| ())
        .map_err(|error| format!("publish remote clipboard text stream: {error}"))
}

async fn controller_clipboard_target(
    window_id: u32,
    owner_identity: &str,
) -> Result<(RemoteControlTargetKind, Option<String>), String> {
    #[cfg(target_os = "macos")]
    {
        let target = crate::compositor::remote_control_target_metadata(
            window_id,
            Some(owner_identity),
        )
        .ok_or_else(|| format!("remote window {window_id} is not open"))?;
        if target.target_kind != RemoteControlTargetKind::Window {
            return Err("remote clipboard requires an application-window share".to_string());
        }
        return Ok((target.target_kind, target.share_instance_id));
    }
    #[cfg(target_os = "windows")]
    {
        let target = crate::windows_compositor::compositor_list_windows()
            .await
            .into_iter()
            .find(|window| {
                window.window_id == window_id && window.owner_identity == owner_identity && !window.hidden
            })
            .ok_or_else(|| format!("remote window {window_id} is not open"))?;
        if target.source_kind != crate::transport::publisher::SharedSourceKind::Window {
            return Err("remote clipboard requires an application-window share".to_string());
        }
        return Ok((
            RemoteControlTargetKind::Window,
            target.share_instance_id,
        ));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (window_id, owner_identity);
        Err("remote clipboard is unavailable on this platform".to_string())
    }
}

fn clipboard_key_message(
    target_user_id: &str,
    controller_id: &str,
    window_id: u32,
    seq: u64,
    grant_token: &str,
    target_kind: Option<RemoteControlTargetKind>,
    share_instance_id: Option<String>,
    shortcut: ClipboardShortcut,
    action: RemoteControlAction,
) -> RemoteControlMessage {
    let (key, code) = match shortcut {
        ClipboardShortcut::Copy => ("c", "KeyC"),
        ClipboardShortcut::Paste => ("v", "KeyV"),
    };
    #[cfg(target_os = "macos")]
    let modifiers = RemoteControlModifiers {
        meta: true,
        ..RemoteControlModifiers::default()
    };
    #[cfg(target_os = "windows")]
    let modifiers = RemoteControlModifiers {
        ctrl: true,
        ..RemoteControlModifiers::default()
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let modifiers = RemoteControlModifiers::default();
    RemoteControlMessage {
        v: VERSION,
        message_type: RemoteControlType::Key,
        action: Some(action),
        target_user_id: target_user_id.to_string(),
        controller_id: controller_id.to_string(),
        window_id,
        seq,
        target_kind,
        share_instance_id,
        controller_capabilities: Vec::new(),
        host_capabilities: Vec::new(),
        reason: None,
        control_session_id: None,
        input_id: None,
        input_seq: None,
        operation_fingerprint_version: None,
        operation_fingerprint: None,
        outcome: None,
        delivery_route: None,
        failure_code: None,
        result_capability: None,
        x: None,
        y: None,
        button: None,
        buttons: None,
        click_count: None,
        delta_x: None,
        delta_y: None,
        delta_mode: None,
        key: Some(key.to_string()),
        code: Some(code.to_string()),
        repeat: false,
        location: Some(0),
        text: None,
        status: None,
        message: None,
        grant_token: Some(grant_token.to_string()),
        supports_binary_hot_path: false,
        modifiers,
    }
}

fn clipboard_replay_message(
    state: &SessionState,
    message: RemoteControlMessage,
) -> Result<(), String> {
    if !is_authorized_input(&message) {
        return Err("remote-control grant is no longer active".to_string());
    }
    let task_frame = match fresh_control_frame(state, message.window_id) {
        SharedWindowScreenStatus::OnScreen(frame) => frame,
        SharedWindowScreenStatus::OffScreen => return Err("target window is off screen".to_string()),
        SharedWindowScreenStatus::Closed => return Err("target window is unavailable".to_string()),
        SharedWindowScreenStatus::NotShared => return Err("target window is no longer shared".to_string()),
    };
    let target_pid = resolved_replay_target_pid(
        active_display_share(state, message.window_id),
        target_pid_for_window(state, message.window_id),
    )
    .ok_or_else(|| "target application could not be resolved".to_string())?;
    cache_target_pid(message.window_id, Some(target_pid));
    let task = replay_task(message, task_frame, Some(target_pid), false);
    if !is_current_replay_epoch(&task) {
        return Err("remote-control operation was superseded".to_string());
    }
    match run_replay_with_deadline(&task, production_replay_injector()) {
        ReplayRunOutcome::Completed(Ok(())) => Ok(()),
        ReplayRunOutcome::Completed(Err(error)) => Err(error),
        ReplayRunOutcome::TimedOut => Err(format!(
            "clipboard shortcut exceeded {}ms deadline",
            REPLAY_EVENT_DEADLINE.as_millis()
        )),
    }
}

fn run_clipboard_shortcut(
    app: &AppHandle,
    message: RemoteControlMessage,
    shortcut: ClipboardShortcut,
) -> Result<(), String> {
    let state = app
        .try_state::<SessionState>()
        .ok_or_else(|| "session state is not available".to_string())?;
    let sequence = message.seq;
    let down = clipboard_key_message(
        &message.target_user_id,
        &message.controller_id,
        message.window_id,
        sequence,
        message.grant_token.as_deref().unwrap_or_default(),
        message.target_kind,
        message.share_instance_id.clone(),
        shortcut,
        RemoteControlAction::Down,
    );
    clipboard_replay_message(state.inner(), down)?;
    let up = clipboard_key_message(
        &message.target_user_id,
        &message.controller_id,
        message.window_id,
        sequence.saturating_add(1),
        message.grant_token.as_deref().unwrap_or_default(),
        message.target_kind,
        message.share_instance_id,
        shortcut,
        RemoteControlAction::Up,
    );
    clipboard_replay_message(state.inner(), up)
}

fn controller_timeout_status(window_id: u32, target_user_id: String) -> RemoteControlStatus {
    RemoteControlStatus {
        window_id,
        owner_identity: Some(target_user_id.clone()),
        controller_id: target_user_id,
        status: "requestFailed",
        message: CONTROLLER_REQUEST_TIMEOUT_MESSAGE.to_string(),
        grant_token: None,
        reason: None,
    }
}

#[cfg(target_os = "macos")]
fn prepare_macos_controller_request_for_capable_host(
    message: &mut RemoteControlMessage,
    window_id: u32,
    owner_identity: &str,
) {
    // Windows publishes an opaque share instance in participant metadata; Mac
    // shares intentionally do not, so this preserves the legacy Mac-host
    // request shape while allowing Mac -> Windows activation to use the
    // envelope Windows requires.
    let Some(target) = crate::compositor::remote_control_target_metadata(
        window_id,
        Some(owner_identity),
    ) else {
        return;
    };
    let Some(share_instance_id) = target.share_instance_id else {
        return;
    };
    message.target_kind = Some(target.target_kind);
    message.share_instance_id = Some(share_instance_id);
    message.controller_capabilities = match target.target_kind {
        RemoteControlTargetKind::Window => vec![
            RemoteControlCapability::LegacyControl,
            RemoteControlCapability::DiscretePointerV1,
            RemoteControlCapability::DiscreteScrollV1,
            RemoteControlCapability::WindowLocalPointer,
            RemoteControlCapability::GlobalKeyboard,
            RemoteControlCapability::UiaInvoke,
            RemoteControlCapability::UnicodeText,
        ],
        RemoteControlTargetKind::Display => vec![
            RemoteControlCapability::LegacyControl,
            RemoteControlCapability::DiscretePointerV1,
            RemoteControlCapability::DiscreteScrollV1,
            RemoteControlCapability::GlobalKeyboard,
            RemoteControlCapability::UnicodeText,
        ],
        RemoteControlTargetKind::Unknown => Vec::new(),
    };
}

#[tauri::command]
pub async fn remote_clipboard_copy(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    grant_token: Option<String>,
) -> Result<(), String> {
    let (publisher, controller_id, target_user_id) =
        viewer_channel(&app, window_id, owner_identity.as_deref())?;
    let grant_token = grant_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "remote-control grant is not active".to_string())?;
    let (target_kind, share_instance_id) =
        controller_clipboard_target(window_id, &target_user_id).await?;
    let local_clipboard_sequence = remote_clipboard::system_clipboard()
        .sequence()
        .map_err(|error| error.to_string())?;
    let operation_id = remote_clipboard::new_operation_id().map_err(|error| error.to_string())?;
    let now = Instant::now();
    remote_clipboard::replace_pending_copy(remote_clipboard::PendingCopy {
        operation_id: operation_id.clone(),
        owner_identity: target_user_id.clone(),
        window_id,
        grant_token: grant_token.clone(),
        local_clipboard_sequence,
        expires_at: now + remote_clipboard::REMOTE_CLIPBOARD_OPERATION_TTL,
    });
    let capable_target = share_instance_id.is_some();
    let request = RemoteClipboardCopyRequest {
        v: VERSION,
        target_user_id: target_user_id.clone(),
        controller_id,
        window_id,
        seq: now_ms(),
        grant_token,
        kind: "copy".to_string(),
        operation_id: operation_id.clone(),
        target_kind: capable_target.then_some(target_kind),
        share_instance_id,
    };
    #[cfg(feature = "cockpit-privileged")]
    cockpit_ledger::record_clipboard_publish(
        request.window_id,
        &request.controller_id,
        &request.target_user_id,
        "copy",
    );
    if let Err(error) = publish_clipboard_copy_request(publisher, target_user_id, request).await {
        remote_clipboard::clear_pending_copy_if_operation(&operation_id);
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
pub async fn remote_clipboard_paste(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    grant_token: Option<String>,
) -> Result<(), String> {
    let (publisher, controller_id, target_user_id) =
        viewer_channel(&app, window_id, owner_identity.as_deref())?;
    #[cfg(not(feature = "cockpit-privileged"))]
    let _ = &controller_id;
    let grant_token = grant_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "remote-control grant is not active".to_string())?;
    let _ = controller_clipboard_target(window_id, &target_user_id).await?;
    let bytes = remote_clipboard::system_clipboard()
        .read_transfer_text()
        .map_err(|error| error.to_string())?;
    // The stream protocol is intentionally independent of Copy. A fresh ID
    // is generated for every user Paste, regardless of any prior Copy.
    let operation_id = remote_clipboard::new_operation_id().map_err(|error| error.to_string())?;
    #[cfg(feature = "cockpit-privileged")]
    cockpit_ledger::record_clipboard_publish(window_id, &controller_id, &target_user_id, "paste");
    send_clipboard_text_stream(
        publisher,
        target_user_id,
        operation_id,
        "paste",
        window_id,
        grant_token,
        bytes,
    )
    .await
}

#[tauri::command]
#[allow(unused_mut)]
pub async fn remote_control_send(app: AppHandle, draft: RemoteControlDraft) -> Result<(), String> {
    let (publisher, controller_id, target_user_id) =
        viewer_channel(&app, draft.window_id, draft.target_owner_id.as_deref())?;
    let mut message = draft.into_message(target_user_id, controller_id);
    // #819: observer only -- see `cockpit_ledger`. Recorded once per draft the
    // controller route produced, before any chunking, so the ledger mirrors the
    // gestures a user made rather than the wire packets they became.
    #[cfg(feature = "cockpit-privileged")]
    cockpit_ledger::record_publish(&message);
    if should_log_message(message.message_type, message.action, message.seq) {
        log::info!("remote-control: publishing {}", message_summary(&message));
    }
    if should_log_latency_probe(&message) {
        log::info!(
            "remote-control-latency: controller send_ts_ms={} {}",
            now_ms(),
            message_summary(&message)
        );
    }
    if message.message_type == RemoteControlType::Text {
        let text = message.text.as_deref().unwrap_or("");
        let chunks = remote_text_chunks(text);
        if chunks.len() > 1 {
            log::info!(
                "remote-control: chunking oversized text replay for window {} into {} chunks ({} chars, cap {} chars)",
                message.window_id,
                chunks.len(),
                text_char_count(text),
                MAX_REPLAY_TEXT_CHARS
            );
            for (index, chunk) in chunks.into_iter().enumerate() {
                let mut chunk_message = message.clone();
                chunk_message.seq = message.seq.saturating_add(index as u64);
                chunk_message.text = Some(chunk);
                #[cfg(target_os = "windows")]
                if !crate::windows_remote_control::prepare_outbound_input(&mut chunk_message)? {
                    return Err(
                        "operation is unsupported by the active Windows control grant".to_string(),
                    );
                }
                let summary = message_summary(&chunk_message);
                if let Err(e) = publish_message(publisher.clone(), chunk_message).await {
                    log::warn!("remote-control: chunked text publish failed for {summary}: {e}");
                    return Err(e);
                }
            }
            return Ok(());
        }
    }
    #[cfg(target_os = "windows")]
    if !crate::windows_remote_control::prepare_outbound_input(&mut message)? {
        // Silent-swallow guard: the controller page catches this error, so
        // without this line an unusable grant looks like "input did
        // nothing" with zero diagnostics (018B: terminals never published).
        log::warn!(
            "remote-control: controller input dropped -- no usable Windows control grant for window {} ({:?})",
            message.window_id,
            message.message_type
        );
        return Err("operation is unsupported by the active Windows control grant".to_string());
    }
    let summary = message_summary(&message);
    match publish_message(publisher, message).await {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("remote-control: publish failed for {summary}: {e}");
            Err(e)
        }
    }
}

#[tauri::command]
#[allow(unused_mut)]
pub async fn remote_control_set_active(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
    active: bool,
) -> Result<bool, String> {
    let (publisher, controller_id, target_user_id) =
        viewer_channel(&app, window_id, owner_identity.as_deref())?;
    let mut message = RemoteControlMessage {
        v: VERSION,
        message_type: if active {
            RemoteControlType::Request
        } else {
            RemoteControlType::Release
        },
        action: None,
        target_user_id,
        controller_id: controller_id.clone(),
        window_id,
        seq: now_ms(),
        target_kind: None,
        share_instance_id: None,
        controller_capabilities: Vec::new(),
        host_capabilities: Vec::new(),
        reason: None,
        control_session_id: None,
        input_id: None,
        input_seq: None,
        operation_fingerprint_version: None,
        operation_fingerprint: None,
        outcome: None,
        delivery_route: None,
        failure_code: None,
        result_capability: None,
        x: None,
        y: None,
        button: None,
        buttons: None,
        click_count: None,
        delta_x: None,
        delta_y: None,
        delta_mode: None,
        key: None,
        code: None,
        repeat: false,
        location: None,
        text: None,
        status: None,
        message: None,
        grant_token: None,
        supports_binary_hot_path: false,
        modifiers: RemoteControlModifiers::default(),
    };
    #[cfg(target_os = "windows")]
    {
        let request_owner = owner_identity
            .clone()
            .unwrap_or_else(|| message.target_user_id.clone());
        if active {
            crate::windows_remote_control::prepare_control_request(
                &mut message,
                window_id,
                request_owner.as_str(),
            )
            .await?;
        }
    }
    #[cfg(target_os = "macos")]
    if active {
        let request_owner = owner_identity
            .as_deref()
            .unwrap_or(message.target_user_id.as_str())
            .to_owned();
        prepare_macos_controller_request_for_capable_host(
            &mut message,
            window_id,
            &request_owner,
        );
    }
    log::info!(
        "remote-control: {} requested for window {window_id}: {}",
        if active { "activation" } else { "deactivation" },
        message_summary(&message)
    );
    // #819: observer only -- see `cockpit_ledger`. The request/release pair is
    // part of the keystone set, so it belongs in the same ledger as the inputs.
    #[cfg(feature = "cockpit-privileged")]
    cockpit_ledger::record_publish(&message);
    if active {
        let summary = message_summary(&message);
        if let Err(e) = publish_message_with_retry(
            publisher,
            message,
            ACTIVATION_PUBLISH_ATTEMPTS,
            ACTIVATION_RETRY_DELAY,
        )
        .await
        {
            log::warn!("remote-control: activation publish failed for {summary}: {e}");
            emit_status(
                &app,
                RemoteControlStatus {
                    window_id,
                    owner_identity: owner_identity.clone().filter(|owner| !owner.is_empty()),
                    controller_id,
                    status: "requestFailed",
                    message: format!("Remote control request could not be sent: {e}"),
                    grant_token: None,
                    reason: None,
                },
            );
            return Err(e);
        }
        log::info!(
            "remote-control: activation request published for window {window_id}; waiting for host status"
        );
    } else {
        remote_clipboard::clear_pending_copy_for(window_id, Some(&message.target_user_id));
        set_remote_window_control_active(&app, window_id, owner_identity.as_deref(), false)?;
        let summary = message_summary(&message);
        if let Err(e) = publish_message_with_retry(
            publisher,
            message,
            ACTIVATION_PUBLISH_ATTEMPTS,
            ACTIVATION_RETRY_DELAY,
        )
        .await
        {
            log::warn!("remote-control: deactivation publish failed for {summary}: {e}");
            return Err(e);
        }
    }
    Ok(false)
}

/// Controller requests the sharer to escalate this share to FullControl
/// (Step 3D.2). Host-side authority: the sharer approves/denies; the mode
/// flips ONLY on approval (via `set_share_control_mode`). Petal never auto-
/// escalates -- this command only sends the request intent for the sharer to
/// review.
#[tauri::command]
#[allow(unused_mut)]
pub async fn remote_control_request_escalation(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) -> Result<(), String> {
    let (publisher, controller_id, target_user_id) =
        viewer_channel(&app, window_id, owner_identity.as_deref())?;
    let mut message = RemoteControlMessage {
        v: VERSION,
        message_type: RemoteControlType::Request,
        action: None,
        target_user_id,
        controller_id: controller_id.clone(),
        window_id,
        seq: now_ms(),
        target_kind: None,
        share_instance_id: None,
        controller_capabilities: Vec::new(),
        host_capabilities: Vec::new(),
        reason: Some(RemoteControlReason::RequestEscalation),
        control_session_id: None,
        input_id: None,
        input_seq: None,
        operation_fingerprint_version: None,
        operation_fingerprint: None,
        outcome: None,
        delivery_route: None,
        failure_code: None,
        result_capability: None,
        x: None,
        y: None,
        button: None,
        buttons: None,
        click_count: None,
        delta_x: None,
        delta_y: None,
        delta_mode: None,
        key: None,
        code: None,
        repeat: false,
        location: None,
        text: None,
        status: None,
        message: None,
        grant_token: None,
        supports_binary_hot_path: false,
        modifiers: RemoteControlModifiers::default(),
    };
    #[cfg(target_os = "windows")]
    {
        let request_owner = owner_identity
            .clone()
            .unwrap_or_else(|| message.target_user_id.clone());
        crate::windows_remote_control::prepare_control_request(
            &mut message,
            window_id,
            request_owner.as_str(),
        )
        .await?;
    }
    log::info!(
        "remote-control: escalation requested by '{}' for window {window_id}",
        controller_id
    );
    publish_message_with_retry(
        publisher,
        message,
        ACTIVATION_PUBLISH_ATTEMPTS,
        ACTIVATION_RETRY_DELAY,
    )
    .await
    .map_err(|e| format!("escalation request could not be sent: {e}"))?;
    Ok(())
}

#[tauri::command]
pub fn remote_control_request_timed_out(
    app: AppHandle,
    window_id: u32,
    owner_identity: Option<String>,
) -> Result<(), String> {
    let (_publisher, controller_id, target_user_id) =
        viewer_channel(&app, window_id, owner_identity.as_deref())?;
    log::warn!(
        "remote-control: controller timeout waiting for active status for window {window_id} controller='{controller_id}' target='{target_user_id}' (controller-side request budget elapsed; {CONTROLLER_REQUEST_TIMEOUT_MS}ms without consent, longer while awaitingConsent)"
    );
    set_remote_window_control_active(&app, window_id, owner_identity.as_deref(), false)?;
    emit_status(&app, controller_timeout_status(window_id, target_user_id));
    Ok(())
}

/// Sharer answers the consent prompt for a parked request. Returns whether
/// a request was actually pending (false = already resolved, e.g. by the
/// 30 s timeout or a share stop).
#[tauri::command]
pub fn remote_control_answer_consent(
    app: AppHandle,
    window_id: u32,
    controller_id: String,
    approve: bool,
) -> bool {
    answer_consent(
        &app,
        window_id,
        &controller_id,
        approve,
        RemoteControlReason::ConsentDenied,
    )
}

/// Resolve a Windows full-control escalation shown in the shared consent
/// panel. The request is one-shot and expires after 30 seconds; approval
/// revalidates the live share, controller presence, and active grant before
/// changing the host-authoritative mode. Denial and every stale path leave the
/// share cursor-preserving.
#[cfg(target_os = "windows")]
#[tauri::command]
pub async fn remote_control_answer_escalation(
    app: AppHandle,
    window_id: u32,
    controller_id: String,
    approve: bool,
) -> bool {
    if !take_escalation(window_id, &controller_id) {
        log::info!(
            "remote-control: escalation answer for '{controller_id}' on window {window_id} was stale"
        );
        return false;
    }
    if !approve {
        log::info!(
            "remote-control: sharer denied full-control escalation for '{controller_id}' on window {window_id}"
        );
        return true;
    }
    let Some(state) = app.try_state::<SessionState>() else {
        return false;
    };
    let state = state.inner();
    let valid = state.active_share_frame(window_id).is_some()
        && state.remote_control_allowed()
        && requester_is_present_in_room(state, &controller_id)
        && is_authorized(window_id, &controller_id)
        && crate::windows_remote_control::share_mode(window_id)
            == RemoteControlMode::CursorPreserving;
    if !valid {
        log::warn!(
            "remote-control: rejecting stale full-control escalation approval for '{controller_id}' on window {window_id}"
        );
        return false;
    }
    match crate::session::set_share_control_mode_for_window(
        &app,
        state,
        window_id,
        RemoteControlMode::FullControl,
    )
    .await
    {
        Ok(()) => {
            log::info!(
                "remote-control: full-control escalation approved for '{controller_id}' on window {window_id}"
            );
            true
        }
        Err(error) => {
            log::warn!(
                "remote-control: full-control escalation approval failed for '{controller_id}' on window {window_id}: {error}"
            );
            false
        }
    }
}

#[tauri::command]
pub fn remote_control_revoke(app: AppHandle, window_id: u32, controller_id: String) {
    revoke(window_id, &controller_id);
    log::info!("remote-control: host stopped '{controller_id}' for shared window {window_id}");
    emit_status(
        &app,
        RemoteControlStatus {
            window_id,
            owner_identity: None,
            controller_id,
            status: "stopped",
            message: "Remote control stopped".to_string(),
            grant_token: None,
            reason: None,
        },
    );
}

use crate::time_util::now_ms;

// Visibility widened to `pub(crate)` for #658: AI window control reuses this
// exact replay path instead of standing up a second AX/CGEvent stack.
#[cfg(target_os = "macos")]
pub(crate) mod input {
    use super::{
        normalized_to_global, truncate_text_to_limit, RemoteControlAction, RemoteControlButton,
        RemoteControlMessage, RemoteControlModifiers, RemoteControlType,
    };
    use crate::platform::cg::WindowFrame;
    use crate::sync_ext::MutexExt;
    use std::collections::{HashMap, HashSet};
    use std::ffi::{c_char, c_void};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct CFRange {
        location: isize,
        length: isize,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        // Prompt variant: registers Petal in the Accessibility list and shows
        // the system grant dialog when the options dict sets the prompt key.
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        static kAXTrustedCheckOptionPrompt: *const c_void;
        fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
        fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
        fn AXUIElementCopyElementAtPosition(
            application: *const c_void,
            x: f32,
            y: f32,
            element: *mut *const c_void,
        ) -> i32;
        fn AXUIElementCopyActionNames(element: *const c_void, names: *mut *const c_void) -> i32;
        fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> i32;
        fn AXUIElementIsAttributeSettable(
            element: *const c_void,
            attribute: *const c_void,
            settable: *mut u8,
        ) -> i32;
        fn AXUIElementCopyParameterizedAttributeValue(
            element: *const c_void,
            parameterized_attribute: *const c_void,
            parameter: *const c_void,
            value: *mut *const c_void,
        ) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: *const c_void,
            attribute: *const c_void,
            value: *mut *const c_void,
        ) -> i32;
        fn AXUIElementSetAttributeValue(
            element: *const c_void,
            attribute: *const c_void,
            value: *const c_void,
        ) -> i32;
        fn AXValueCreate(the_type: i32, value_ptr: *const c_void) -> *const c_void;
        fn AXValueGetValue(value: *const c_void, the_type: i32, value_ptr: *mut c_void) -> bool;
        fn AXUIElementGetTypeID() -> usize;
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            mouse_type: u32,
            mouse_cursor_position: CGPoint,
            mouse_button: u32,
        ) -> *mut c_void;
        fn CGEventCreateScrollWheelEvent(
            source: *const c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
        ) -> *mut c_void;
        fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            virtual_key: u16,
            key_down: bool,
        ) -> *mut c_void;
        fn CGEventKeyboardSetUnicodeString(
            event: *mut c_void,
            string_length: usize,
            unicode_string: *const u16,
        );
        fn CGEventSetFlags(event: *mut c_void, flags: u64);
        fn CGEventSetLocation(event: *mut c_void, new_location: CGPoint);
        fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
        fn CGEventPostToPid(pid: i32, event: *mut c_void);
        // #446: the session-tap route. Unlike CGEventPostToPid/SLEventPostToPid
        // -- which post into a target process's own queue and were both
        // measured delivering ZERO pointer NSEvents -- CGEventPost hands the
        // event to WindowServer, which hit-tests it against the real window
        // stack. That hit-test is why the target must be frontmost.
        fn CGEventPost(tap: u32, event: *mut c_void);
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
        fn CFRetain(cf: *const c_void) -> *const c_void;
        fn CFRelease(cf: *const c_void);
        fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: *const c_void;
        static kCFBooleanFalse: *const c_void;
        fn CFArrayGetCount(the_array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: isize) -> *const c_void;
        fn CFArrayGetTypeID() -> usize;
        fn CFEqual(cf1: *const c_void, cf2: *const c_void) -> bool;
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFNumberCreate(
            allocator: *const c_void,
            the_type: i64,
            value_ptr: *const c_void,
        ) -> *const c_void;
        fn CFNumberGetValue(number: *const c_void, the_type: i64, value_ptr: *mut c_void) -> bool;
        fn CFNumberGetTypeID() -> usize;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFStringGetCString(
            the_string: *const c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> bool;
        // #170: reading full AXSelectedText / AXValue strings that can exceed the
        // fixed 128-byte scratch buffer used by `cf_string_to_string`.
        fn CFStringGetLength(the_string: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetTypeID() -> usize;
        // Null key/value callbacks are fine here: the only entries are
        // immortal CF constants (never released) and the dict is short-lived.
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
    }

    const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
    const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
    const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
    const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
    const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
    const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
    const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
    const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
    const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
    const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

    /// #446: `kCGSessionEventTap`. Posting here routes through WindowServer's
    /// own hit-test rather than into one process's queue.
    const K_CG_SESSION_EVENT_TAP: u32 = 1;

    const K_CG_SCROLL_EVENT_UNIT_PIXEL: u32 = 0;
    const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2: u32 = 12;
    const K_CG_MOUSE_EVENT_CLICK_STATE: u32 = 1;
    const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
    const K_CG_MOUSE_EVENT_WINDOW_UNDER_POINTER: u32 = 91;
    const K_CG_MOUSE_EVENT_WINDOW_UNDER_POINTER_CAN_HANDLE: u32 = 92;

    const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x20000;
    const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x40000;
    const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x80000;
    const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x100000;
    const LINE_SCROLL_PIXELS: f64 = 40.0;
    const AX_TRUST_CACHE_TTL: Duration = Duration::from_secs(1);
    const AX_APP_MESSAGING_TIMEOUT_SECONDS: f32 = 0.35;
    pub(super) const AX_RESOLUTION_CACHE_TTL: Duration = Duration::from_millis(350);
    // #368 F3: keep the cache-key bucket at the click-precision threshold so a
    // single key cannot span two distinct small controls (an 8px bucket could
    // serve a click the neighbouring control's cached element).
    pub(super) const AX_POINT_BUCKET: f64 = AX_CLICK_DRAG_THRESHOLD_POINTS;
    pub(super) const AX_CLICK_DRAG_THRESHOLD_POINTS: f64 = 4.0;
    const AX_SCROLL_PARENT_HOPS: usize = 10;
    const AX_FALLBACK_SCROLL_FRACTION: f64 = 0.02;
    const K_AX_VALUE_TYPE_CG_POINT: i32 = 1;
    const K_AX_VALUE_TYPE_CG_SIZE: i32 = 2;
    const K_AX_VALUE_TYPE_CF_RANGE: i32 = 4;
    const K_CF_NUMBER_FLOAT64: i64 = 6;
    // #170: AXNumberOfCharacters comes back as a CFNumber we read as an i64.
    const K_CF_NUMBER_SINT64: i64 = 4;
    // #170: bound the window-scoped AX tree walk that hunts for the focused text
    // element (a backgrounded app's AXFocusedUIElement resolves to AXApplication,
    // so we descend AXWindows/AXChildren by role instead). Depth+node caps keep a
    // pathological UI tree from stalling the replay worker.
    const AX_TEXT_SEARCH_MAX_DEPTH: usize = 8;
    const AX_TEXT_SEARCH_MAX_NODES: usize = 256;
    const K_AX_ERROR_SUCCESS: i32 = 0;
    const K_AX_ERROR_INVALID_UI_ELEMENT: i32 = -25202;
    const K_AX_ERROR_CANNOT_COMPLETE: i32 = -25204;
    const K_AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
    const K_AX_ERROR_ACTION_UNSUPPORTED: i32 = -25206;
    const K_AX_ERROR_API_DISABLED: i32 = -25211;
    const K_AX_ERROR_NO_VALUE: i32 = -25212;
    // Local sentinel: never returned by Accessibility itself. A known sibling
    // window is an authorization failure, not a capability miss/fallback (#759).
    const K_AX_ERROR_WINDOW_ID_MISMATCH: i32 = -75_900;
    // Local sentinel: the AX-element -> CGWindowID primitive was unavailable,
    // failed, or could not correlate uniquely. This is deliberately distinct
    // from a successfully resolved unauthorized sibling window.
    const K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE: i32 = -75_901;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const RTLD_LAZY: i32 = 0x1;
    const RTLD_LOCAL: i32 = 0x4;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MouseKind {
        LeftDown,
        LeftUp,
        RightDown,
        RightUp,
        Moved,
        LeftDragged,
        RightDragged,
        OtherDown,
        OtherUp,
        OtherDragged,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ScrollUnit {
        Pixel,
    }

    trait InputSink {
        fn verify_key_window(&self, window_id: u32) -> Result<(), String>;
        /// Same check, plus the one-shot AXRaise recovery (#777). Called ONCE
        /// per wire event at the replay entry points -- never per character.
        /// Defaults to the raw check so non-CGEvent sinks are unaffected.
        fn verify_key_window_with_recovery(&self, window_id: u32) -> Result<(), String> {
            self.verify_key_window(window_id)
        }
        /// The cheap, purely LOCAL half of the key-window gate: does this
        /// message even name the window this sink was authorized for, and does
        /// the sink have a usable pid? No AX round-trip, no live focus check.
        /// Used for key RELEASES, which must not be refused on focus drift --
        /// see the comment in `replay_key`. Defaults to permissive so sinks
        /// with no notion of an authorized window are unaffected.
        fn verify_key_window_sink_identity(&self, _window_id: u32) -> Result<(), String> {
            Ok(())
        }
        fn mouse(
            &self,
            kind: MouseKind,
            at: super::GlobalPoint,
            button: RemoteControlButton,
            click_state: u32,
            flags: u64,
        ) -> Result<(), String>;
        fn scroll(
            &self,
            axis1: i32,
            axis2: i32,
            at: super::GlobalPoint,
            unit: ScrollUnit,
            flags: u64,
        ) -> Result<(), String>;
        fn key(
            &self,
            keycode: u16,
            down: bool,
            flags: u64,
            unicode: Option<&str>,
        ) -> Result<(), String>;
        fn text(&self, s: &str) -> Result<(), String>;
    }

    #[derive(Debug)]
    struct CfObject {
        ptr: *const c_void,
    }

    // CF/AX references are immutable handles to OS-managed objects. Petal uses
    // them only on the replay worker after retaining them, so moving the handle
    // between the resolver/control path and that worker does not share Rust
    // memory without synchronization.
    unsafe impl Send for CfObject {}

    impl CfObject {
        fn from_create(ptr: *const c_void) -> Option<Self> {
            if ptr.is_null() {
                None
            } else {
                Some(Self { ptr })
            }
        }

        unsafe fn retain(ptr: *const c_void) -> Option<Self> {
            if ptr.is_null() {
                None
            } else {
                Some(Self {
                    ptr: unsafe { CFRetain(ptr) },
                })
            }
        }

        fn as_ptr(&self) -> *const c_void {
            self.ptr
        }
    }

    impl Clone for CfObject {
        fn clone(&self) -> Self {
            unsafe { Self::retain(self.ptr).expect("retaining non-null CF object") }
        }
    }

    impl Drop for CfObject {
        fn drop(&mut self) {
            unsafe {
                if !self.ptr.is_null() {
                    CFRelease(self.ptr);
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    pub(super) enum AxElementHandle {
        Real(CfObject),
        #[cfg(test)]
        Test(u64),
    }

    impl AxElementHandle {
        fn as_real_ptr(&self) -> Option<*const c_void> {
            match self {
                AxElementHandle::Real(object) => Some(object.as_ptr()),
                #[cfg(test)]
                AxElementHandle::Test(_) => None,
            }
        }

        #[cfg(test)]
        pub(super) fn test_id(&self) -> Option<u64> {
            match self {
                AxElementHandle::Test(id) => Some(*id),
                AxElementHandle::Real(_) => None,
            }
        }
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(super) struct AxCapabilities {
        pub(super) pressable: bool,
        pub(super) show_menu: bool,
        pub(super) text_selectable: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct AxError {
        code: i32,
    }

    impl AxError {
        fn new(code: i32) -> Self {
            Self { code }
        }

        fn is_invalid_ui_element(self) -> bool {
            self.code == K_AX_ERROR_INVALID_UI_ELEMENT
        }

        fn is_api_disabled(self) -> bool {
            self.code == K_AX_ERROR_API_DISABLED
        }

        fn is_capability_miss(self) -> bool {
            matches!(
                self.code,
                K_AX_ERROR_ATTRIBUTE_UNSUPPORTED
                    | K_AX_ERROR_CANNOT_COMPLETE
                    | K_AX_ERROR_ACTION_UNSUPPORTED
                    | K_AX_ERROR_NO_VALUE
            )
        }

        fn is_window_id_mismatch(self) -> bool {
            self.code == K_AX_ERROR_WINDOW_ID_MISMATCH
        }

        fn is_window_identity_unavailable(self) -> bool {
            self.code == K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE
        }
    }

    trait AxInputBackend {
        fn resolve_at(
            &self,
            pid: i32,
            window_id: u32,
            point: super::GlobalPoint,
        ) -> Result<Option<AxElementHandle>, AxError>;
        /// #170: resolve the app's currently editable text element WITHOUT relying
        /// on the app being frontmost/key. App-scoped `AXFocusedUIElement` resolves
        /// to `AXApplication` while the controller (not the target) stays frontmost
        /// (Petal never steals focus — case 27), so we descend `AXWindows` and look
        /// for a window-scoped focused text element / role-based text descendant.
        /// Returns `Ok(None)` when no editable text element is reachable, in which
        /// case the caller falls back to the (already-broken-when-backgrounded)
        /// CGEvent key-equivalent path. The `TextElementSource` reports whether the
        /// element came from genuine window-scoped focus or the BFS-shallowest
        /// fallback (F5: destructive shortcuts trust only the former).
        fn resolve_text_element(
            &self,
            pid: i32,
            window_id: u32,
        ) -> Result<Option<(AxElementHandle, TextElementSource)>, AxError>;
        fn capabilities(&self, element: &AxElementHandle) -> AxCapabilities;
        fn press(&self, element: &AxElementHandle) -> Result<(), AxError>;
        fn show_menu(&self, element: &AxElementHandle) -> Result<(), AxError>;
        fn offset_at_point(
            &self,
            element: &AxElementHandle,
            point: super::GlobalPoint,
        ) -> Result<i64, AxError>;
        /// #170: number of characters in the element's text (AXNumberOfCharacters,
        /// falling back to the length of the AXValue string). Used to compute the
        /// full-document range for Cmd+A select-all.
        fn text_length(&self, element: &AxElementHandle) -> Result<i64, AxError>;
        /// #170: current AXSelectedText, used to service Cmd+C by writing the real
        /// selection to the pasteboard ourselves (the CGEvent Cmd+C never reaches
        /// the menu key-equivalent when the app is backgrounded).
        fn selected_text(&self, element: &AxElementHandle) -> Result<Option<String>, AxError>;
        /// #170: replace the current selection (or insert at the caret) with the
        /// given string by setting AXSelectedText — services Cmd+V paste.
        fn set_selected_text(&self, element: &AxElementHandle, text: &str) -> Result<(), AxError>;
        fn set_selected_range(
            &self,
            element: &AxElementHandle,
            start: i64,
            len: i64,
        ) -> Result<(), AxError>;
        fn scroll_by(
            &self,
            window_id: u32,
            point: super::GlobalPoint,
            element: &AxElementHandle,
            delta_px_y: f64,
            delta_px_x: f64,
        ) -> Result<bool, AxError>;
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum SlClickError {
        Unavailable,
        Failed(String),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SlClickOutcome {
        Posted,
        PassThrough,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SlMouseEvent {
        Down,
        Up,
        Dragged,
    }

    trait SlClickBackend {
        /// #373: `click_state` carries the multi-click count (1 = single,
        /// 2 = double, ...) into the posted CGEvent's
        /// `kCGMouseEventClickState` field, which is how a synthesized event
        /// tells the target app's own mouseDown handler "this is the Nth
        /// click of a sequence" -- e.g. NSTextView selects a word on
        /// click_state=2. Priming pings always pass 1.
        fn post_click(
            &self,
            pid: i32,
            point: super::GlobalPoint,
            button: RemoteControlButton,
            click_state: u32,
        ) -> Result<(), SlClickError>;

        fn post_mouse_event(
            &self,
            pid: i32,
            point: super::GlobalPoint,
            button: RemoteControlButton,
            event: SlMouseEvent,
        ) -> Result<(), SlClickError>;

        fn post_scroll(
            &self,
            pid: i32,
            point: super::GlobalPoint,
            delta_y: i32,
            delta_x: i32,
            flags: u64,
        ) -> Result<(), SlClickError>;
    }

    /// #170: the system clipboard, abstracted so the Cmd+C / Cmd+V AX round-trip
    /// is unit-testable without touching the OS clipboard. Production delegates
    /// to `remote_clipboard`; tests continue to provide a deterministic mock.
    trait PasteboardBackend {
        fn read_text(&self) -> Option<String>;
        fn write_text(&self, text: &str);
    }

    struct SystemAxBackend;
    struct SystemSlClickBackend;
    struct SystemPasteboardBackend;

    impl PasteboardBackend for SystemPasteboardBackend {
        fn read_text(&self) -> Option<String> {
            crate::remote_clipboard::system_clipboard()
                .read_text()
                .ok()
                .flatten()
        }

        fn write_text(&self, text: &str) {
            if let Err(error) = crate::remote_clipboard::system_clipboard().write_text(text) {
                log::warn!("remote-control: native clipboard write failed during AX Cmd+C: {error}");
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AxClickAction {
        Press,
        ShowMenu,
    }

    #[derive(Debug)]
    enum GestureMode {
        PassThrough,
        SlDrag,
        /// #446: the whole gesture (Down / drag Moves / Up) is owned by the
        /// session-tap route. Chosen at Down time and never mixed with AX
        /// mid-gesture -- an AX-handled Down followed by a session-tap Up is
        /// incoherent, and a session-tap Down leaves a physically held button
        /// that only a session-tap Up can release.
        SessionTap,
        AxPressable {
            element: AxElementHandle,
            action: AxClickAction,
        },
        AxText {
            element: AxElementHandle,
            anchor_offset: i64,
        },
    }

    #[derive(Debug)]
    struct PointerGestureState {
        mode: GestureMode,
        /// Where the gesture STARTED. Never updated after Down -- the
        /// click-vs-drag displacement at Up genuinely wants the origin.
        /// Do not repurpose this as "where the pointer is now" (#611).
        down_point: super::GlobalPoint,
        /// #611: where the pointer actually IS, updated on every delivered
        /// drag Move. A cancellation (revoke / disconnect / share ended /
        /// deadline abandon) must post its synthetic release HERE, not at
        /// `down_point`: post_mouse warps the cursor to the release point, so
        /// releasing at the origin both moves the pointer back and drops
        /// drag-and-drop content the whole drag distance away from where the
        /// user left it (measured live: released at (140,70) for a drag that
        /// ended at (420,70)).
        last_point: super::GlobalPoint,
        button: RemoteControlButton,
        /// #373: multi-click count captured at Down time, carried through to
        /// Up so the SL/CGEvent fallback path can post the correct
        /// click_state (a separate Up wire message may not repeat it).
        click_count: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AxReplayOutcome {
        Handled,
        PassThrough,
    }

    static AX_APP_ELEMENT_CACHE: OnceLock<Mutex<HashMap<i32, AxElementHandle>>> = OnceLock::new();
    static AX_RESOLUTION_CACHE: OnceLock<Mutex<AxResolutionCache>> = OnceLock::new();
    static AX_SCROLL_TARGET_CACHE: OnceLock<Mutex<HashMap<AxPointKey, CachedAxScrollTarget>>> =
        OnceLock::new();
    static AX_PROBE_COUNTERS: OnceLock<AxProbeCounters> = OnceLock::new();
    // TODO(#368 Phase 2): replace these short-lived caches with the persistent
    // per-target AXObserver session described by the issue.
    // #374: keyed per (window_id, controller_id), not just window_id — two
    // concurrent controllers dragging in the same window each need their own
    // parked gesture/anchor so one doesn't clobber the other's in-progress
    // drag state.
    static AX_POINTER_GESTURES: OnceLock<Mutex<HashMap<(u32, String), PointerGestureState>>> =
        OnceLock::new();
    static SL_PRIMED_PIDS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    static SL_EVENT_POST_TO_PID: OnceLock<Option<SlEventPostToPidFn>> = OnceLock::new();
    static SL_FAILURE_LOGGED: OnceLock<()> = OnceLock::new();

    type SlEventPostToPidFn = unsafe extern "C" fn(i32, *mut c_void);

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct AxPointKey {
        pub(super) window_id: u32,
        pub(super) x: i32,
        pub(super) y: i32,
    }

    #[derive(Debug, Clone)]
    pub(super) struct CachedAxResolution {
        pub(super) element: AxElementHandle,
        pub(super) capabilities: AxCapabilities,
        pub(super) cached_at: Instant,
    }

    #[derive(Debug, Default)]
    pub(super) struct AxResolutionCache {
        entries: HashMap<AxPointKey, CachedAxResolution>,
    }

    impl AxResolutionCache {
        pub(super) fn insert_at(
            &mut self,
            key: AxPointKey,
            element: AxElementHandle,
            capabilities: AxCapabilities,
            cached_at: Instant,
        ) {
            self.entries.insert(
                key,
                CachedAxResolution {
                    element,
                    capabilities,
                    cached_at,
                },
            );
        }

        pub(super) fn get_at(
            &mut self,
            key: AxPointKey,
            now: Instant,
        ) -> Option<CachedAxResolution> {
            let cached = self.entries.get(&key)?.clone();
            if now.saturating_duration_since(cached.cached_at) < AX_RESOLUTION_CACHE_TTL {
                Some(cached)
            } else {
                self.entries.remove(&key);
                None
            }
        }

        pub(super) fn invalidate_window(&mut self, window_id: u32) {
            self.entries.retain(|key, _| key.window_id != window_id);
        }

        pub(super) fn invalidate_key(&mut self, key: AxPointKey) {
            self.entries.remove(&key);
        }

        pub(super) fn clear(&mut self) {
            self.entries.clear();
        }
    }

    #[derive(Debug, Clone)]
    struct CachedAxScrollTarget {
        scroll_area: Option<AxElementHandle>,
        vertical: Option<AxElementHandle>,
        horizontal: Option<AxElementHandle>,
        cached_at: Instant,
    }

    #[derive(Debug, Default)]
    struct AxProbeCounters {
        ax_ipc: AtomicU32,
        cache_hits: AtomicU32,
        cache_misses: AtomicU32,
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub(super) struct AxProbeSnapshot {
        pub ax_ipc: u32,
        pub cache_hits: u32,
        pub cache_misses: u32,
    }

    fn ax_resolution_cache() -> &'static Mutex<AxResolutionCache> {
        AX_RESOLUTION_CACHE.get_or_init(|| Mutex::new(AxResolutionCache::default()))
    }

    fn ax_probe_counters() -> &'static AxProbeCounters {
        AX_PROBE_COUNTERS.get_or_init(AxProbeCounters::default)
    }

    pub(super) fn ax_probe_snapshot() -> AxProbeSnapshot {
        let counters = ax_probe_counters();
        AxProbeSnapshot {
            ax_ipc: counters.ax_ipc.load(Ordering::Relaxed),
            cache_hits: counters.cache_hits.load(Ordering::Relaxed),
            cache_misses: counters.cache_misses.load(Ordering::Relaxed),
        }
    }

    fn ax_point_key(window_id: u32, point: super::GlobalPoint) -> AxPointKey {
        AxPointKey {
            window_id,
            x: (point.x / AX_POINT_BUCKET).floor() as i32,
            y: (point.y / AX_POINT_BUCKET).floor() as i32,
        }
    }

    pub(super) fn clear_ax_resolution_cache_for_window(window_id: u32) {
        ax_resolution_cache()
            .lock_unpoisoned()
            .invalidate_window(window_id);
        ax_scroll_target_cache()
            .lock_unpoisoned()
            .retain(|key, _| key.window_id != window_id);
    }

    // #368 F1: after a successful mutating AX action (press / show-menu /
    // scroll / caret set) the content under the cursor has changed, so drop
    // the window's cached element resolutions to prevent a follow-up event
    // within the TTL from acting on an element that scrolled or was replaced.
    // The scroll-target cache (scroll area + scrollbars) is intentionally
    // preserved: those elements do not move when their content scrolls.
    pub(super) fn invalidate_ax_resolution_after_mutation(window_id: u32) {
        ax_resolution_cache()
            .lock_unpoisoned()
            .invalidate_window(window_id);
    }

    fn clear_ax_resolution_cache_key(key: AxPointKey) {
        ax_resolution_cache().lock_unpoisoned().invalidate_key(key);
        ax_scroll_target_cache().lock_unpoisoned().remove(&key);
    }

    fn clear_ax_resolution_cache() {
        ax_resolution_cache().lock_unpoisoned().clear();
        ax_scroll_target_cache().lock_unpoisoned().clear();
    }

    fn ax_scroll_target_cache() -> &'static Mutex<HashMap<AxPointKey, CachedAxScrollTarget>> {
        AX_SCROLL_TARGET_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn resolve_cached(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        ax: &dyn AxInputBackend,
    ) -> Result<Option<(AxElementHandle, AxCapabilities)>, AxError> {
        resolve_at_point(window_id, pid, point, ax, true)
    }

    // #368 F2: when a click acted on a cache-served element that turned out to
    // be stale (kAXErrorInvalidUIElement), the caller re-resolves with
    // `use_cache = false` so a fresh hit-test replaces the dead entry instead
    // of the click being silently swallowed.
    fn resolve_at_point(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        ax: &dyn AxInputBackend,
        use_cache: bool,
    ) -> Result<Option<(AxElementHandle, AxCapabilities)>, AxError> {
        let key = ax_point_key(window_id, point);
        let now = Instant::now();
        if use_cache {
            if let Some(cached) = ax_resolution_cache().lock_unpoisoned().get_at(key, now) {
                ax_probe_counters()
                    .cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(Some((cached.element, cached.capabilities)));
            }
        } else {
            ax_resolution_cache().lock_unpoisoned().invalidate_key(key);
        }
        ax_probe_counters()
            .cache_misses
            .fetch_add(1, Ordering::Relaxed);
        let element = match ax.resolve_at(pid, window_id, point) {
            Ok(element) => element,
            Err(error) => {
                clear_ax_resolution_cache_key(key);
                return Err(error);
            }
        };
        let Some(element) = element else {
            return Ok(None);
        };
        let capabilities = ax.capabilities(&element);
        ax_resolution_cache()
            .lock_unpoisoned()
            .insert_at(key, element.clone(), capabilities, now);
        Ok(Some((element, capabilities)))
    }

    fn ax_app_element_cache() -> &'static Mutex<HashMap<i32, AxElementHandle>> {
        AX_APP_ELEMENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn ax_pointer_gestures() -> &'static Mutex<HashMap<(u32, String), PointerGestureState>> {
        AX_POINTER_GESTURES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn sl_primed_pids() -> &'static Mutex<HashSet<i32>> {
        SL_PRIMED_PIDS.get_or_init(|| Mutex::new(HashSet::new()))
    }

    pub fn clear_cached_ax_app_for_pid(pid: i32) {
        ax_app_element_cache().lock_unpoisoned().remove(&pid);
    }

    /// Window-wide: clears EVERY controller's parked gesture for this window
    /// (sharing ended / window drained). For a single controller's own
    /// revoke/disconnect, use [`clear_ax_gesture_for_controller`] instead so
    /// a concurrent controller's in-progress drag survives.
    pub fn clear_ax_gesture_for_window(window_id: u32) {
        ax_pointer_gestures()
            .lock_unpoisoned()
            // Keep an opted-in SkyLight drag alive until the synthetic Up
            // queued by revoke/drain can deliver its cancellation release.
            .retain(|(stored_window_id, _), state| {
                *stored_window_id != window_id || matches!(&state.mode, GestureMode::SlDrag)
            });
    }

    /// #374: clears only this controller's parked gesture, leaving any other
    /// concurrent controller's in-progress drag/anchor on the same window
    /// untouched.
    pub fn clear_ax_gesture_for_controller(window_id: u32, controller_id: &str) {
        // #446: a session-tap gesture holds a REAL mouse button down, so it
        // must be RELEASED here, not merely forgotten -- forgetting it left a
        // phantom held button on every revoke/disconnect mid-drag. (SlDrag is
        // retained instead: its release rides the synthetic Up queued next.)
        let tap = SystemSessionTapBackend;
        clear_ax_gesture_for_controller_with_backend(window_id, controller_id, &tap);
    }

    fn clear_ax_gesture_for_controller_with_backend(
        window_id: u32,
        controller_id: &str,
        tap: &dyn SessionTapBackend,
    ) {
        release_session_tap_gestures_with_backend(window_id, Some(controller_id), tap);
        ax_pointer_gestures()
            .lock_unpoisoned()
            .retain(|key, state| {
                *key != (window_id, controller_id.to_string())
                    || matches!(&state.mode, GestureMode::SlDrag)
            });
    }

    /// Serializes every test touching the process-wide AX-control state
    /// (gesture map, cursor takeovers, resolution/pid/frame caches). BOTH
    /// this module's tests and remote_control's outer tests wipe or count
    /// these statics; an unlocked parallel run corrupts an in-flight
    /// gesture (observed: a wiped mid-gesture cursor takeover restoring a
    /// foreign cursor position).
    #[cfg(test)]
    pub(crate) fn ax_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock_unpoisoned()
    }

    pub fn clear_all_ax_control_state() {
        ax_app_element_cache().lock_unpoisoned().clear();
        ax_resolution_cache().lock_unpoisoned().clear();
        ax_scroll_target_cache().lock_unpoisoned().clear();
        ax_pointer_gestures().lock_unpoisoned().clear();
        sl_primed_pids().lock_unpoisoned().clear();
        // This function has no production callers -- it is the reset every
        // test in this module runs. The cursor-takeover map is keyed by
        // WINDOW and every test uses the same window id, so a leftover entry
        // makes the next test's `prepare_session_tap_target` take its
        // idempotent early return and skip the raise/warp: an order-dependent
        // failure unrelated to whatever that test asserts.
        cursor_takeovers().lock_unpoisoned().clear();
    }

    /// Same as `clear_all_ax_control_state`, except an in-progress SkyLight
    /// drag's gesture state is retained. This is what the real revoke_all/
    /// room-disconnect path (`clear_all_control_caches`) must call instead:
    /// an unconditional clear there -- the canonical "controller vanished
    /// mid-drag" scenario -- would wipe the only record of a physically-held
    /// SL mouse button before enqueue_synthetic_releases's Up replay can find
    /// it and release it, a permanent phantom held button in the target app
    /// (#446 review finding, worse than the bug being fixed). Kept as a
    /// separate function rather than changing `clear_all_ax_control_state`
    /// itself, since dozens of tests use that one as an unconditional
    /// full-reset helper between cases and would otherwise leak SlDrag state
    /// across test boundaries.
    pub fn clear_all_ax_control_state_except_sl_drag() {
        ax_app_element_cache().lock_unpoisoned().clear();
        ax_resolution_cache().lock_unpoisoned().clear();
        ax_scroll_target_cache().lock_unpoisoned().clear();
        ax_pointer_gestures()
            .lock_unpoisoned()
            .retain(|_, state| matches!(&state.mode, GestureMode::SlDrag));
        sl_primed_pids().lock_unpoisoned().clear();
    }

    #[cfg(test)]
    pub(super) fn ax_gesture_count_for_tests() -> usize {
        ax_pointer_gestures().lock_unpoisoned().len()
    }

    #[cfg(test)]
    pub(super) fn insert_pass_through_ax_gesture_for_tests(window_id: u32, controller_id: &str) {
        ax_pointer_gestures().lock_unpoisoned().insert(
            (window_id, controller_id.to_string()),
            PointerGestureState {
                mode: GestureMode::PassThrough,
                down_point: super::GlobalPoint { x: 0.0, y: 0.0 },
                last_point: super::GlobalPoint { x: 0.0, y: 0.0 },
                button: RemoteControlButton::Left,
                click_count: 1,
            },
        );
    }

    #[cfg(test)]
    pub(super) fn insert_sl_drag_gesture_for_tests(window_id: u32, controller_id: &str) {
        ax_pointer_gestures().lock_unpoisoned().insert(
            (window_id, controller_id.to_string()),
            PointerGestureState {
                mode: GestureMode::SlDrag,
                down_point: super::GlobalPoint { x: 0.0, y: 0.0 },
                last_point: super::GlobalPoint { x: 0.0, y: 0.0 },
                button: RemoteControlButton::Left,
                click_count: 1,
            },
        );
    }

    struct CGEventSink {
        target_pid: Option<i32>,
        window_id: u32,
    }

    #[derive(Debug, Default)]
    struct AxTrustCache {
        cached: Option<CachedAxTrust>,
    }

    #[derive(Debug, Clone, Copy)]
    struct CachedAxTrust {
        trusted: bool,
        checked_at: Instant,
    }

    impl AxTrustCache {
        fn get_or_refresh<F>(&mut self, now: Instant, mut check: F) -> bool
        where
            F: FnMut() -> bool,
        {
            if let Some(cached) = self.cached {
                if now.saturating_duration_since(cached.checked_at) < AX_TRUST_CACHE_TTL {
                    return cached.trusted;
                }
            }
            let trusted = check();
            self.store(now, trusted);
            trusted
        }

        fn store(&mut self, checked_at: Instant, trusted: bool) {
            self.cached = Some(CachedAxTrust {
                trusted,
                checked_at,
            });
        }
    }

    static AX_TRUST_CACHE: OnceLock<Mutex<AxTrustCache>> = OnceLock::new();

    fn ax_trust_cache() -> &'static Mutex<AxTrustCache> {
        AX_TRUST_CACHE.get_or_init(|| Mutex::new(AxTrustCache::default()))
    }

    pub fn accessibility_trusted() -> bool {
        ax_trust_cache()
            .lock_unpoisoned()
            .get_or_refresh(Instant::now(), || unsafe { AXIsProcessTrusted() })
    }

    /// Register Petal in the Accessibility list and show the macOS grant
    /// dialog, and open the Accessibility settings pane so the user can toggle
    /// it on. Without this permission every replayed mouse/key event is dropped
    /// (remote control silently does nothing). Returns whether access is
    /// already granted. Safe to call repeatedly; call sites rate-limit it.
    pub fn prompt_accessibility() -> bool {
        let trusted = unsafe {
            let keys: [*const c_void; 1] = [kAXTrustedCheckOptionPrompt];
            let values: [*const c_void; 1] = [kCFBooleanTrue];
            let options = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                std::ptr::null(),
                std::ptr::null(),
            );
            let trusted = AXIsProcessTrustedWithOptions(options);
            if !options.is_null() {
                CFRelease(options);
            }
            trusted
        };
        ax_trust_cache()
            .lock_unpoisoned()
            .store(Instant::now(), trusted);
        if !trusted {
            // Also deep-link straight to the pane — the AX prompt alone is easy
            // to miss, and this lands the user exactly where the toggle lives.
            let _ = std::process::Command::new("open")
                .arg(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
                )
                .spawn();
        }
        trusted
    }

    impl AxInputBackend for SystemAxBackend {
        fn resolve_at(
            &self,
            pid: i32,
            window_id: u32,
            point: super::GlobalPoint,
        ) -> Result<Option<AxElementHandle>, AxError> {
            let Some(app) = ax_app_element_for_pid(pid) else {
                return Ok(None);
            };
            let Some(app_ptr) = app.as_real_ptr() else {
                return Ok(None);
            };
            let mut out = std::ptr::null();
            ax_probe_counters().ax_ipc.fetch_add(1, Ordering::Relaxed);
            let error = unsafe {
                AXUIElementCopyElementAtPosition(app_ptr, point.x as f32, point.y as f32, &mut out)
            };
            if error != K_AX_ERROR_SUCCESS {
                if error == K_AX_ERROR_API_DISABLED {
                    log::warn!(
                        "remote-control: Accessibility permission disabled during AX hit-test"
                    );
                }
                return Err(AxError::new(error));
            }
            let Some(element) = CfObject::from_create(out).map(AxElementHandle::Real) else {
                return Ok(None);
            };
            if let Some(element_ptr) = element.as_real_ptr() {
                let error = unsafe {
                    AXUIElementSetMessagingTimeout(element_ptr, AX_APP_MESSAGING_TIMEOUT_SECONDS)
                };
                if error != K_AX_ERROR_SUCCESS {
                    log::warn!(
                        "remote-control: failed to set AX messaging timeout for hit-tested element in pid {pid}: {error}"
                    );
                }
            }
            if !element_belongs_to_window(&element, pid, window_id)? {
                return Err(AxError::new(K_AX_ERROR_WINDOW_ID_MISMATCH));
            }
            Ok(Some(element))
        }

        fn resolve_text_element(
            &self,
            pid: i32,
            window_id: u32,
        ) -> Result<Option<(AxElementHandle, TextElementSource)>, AxError> {
            let Some(app) = ax_app_element_for_pid(pid) else {
                return Ok(None);
            };
            // Candidate windows, most-likely-active first: the app's focused/main
            // window, then every AXWindows entry. We deliberately AVOID the
            // app-scoped AXFocusedUIElement here — for a backgrounded app it
            // resolves up to AXApplication (the #170 root cause). Window objects,
            // by contrast, are reachable regardless of key/frontmost state.
            let mut windows: Vec<AxElementHandle> = Vec::new();
            for attr in [ax_focused_window_attribute(), ax_main_window_attribute()] {
                if let Ok(window) = copy_attribute(&app, attr.as_ptr()) {
                    if is_ax_ui_element(window.as_ptr()) {
                        windows.push(AxElementHandle::Real(window));
                    }
                }
            }
            if let Ok(list) = copy_attribute(&app, ax_windows_attribute().as_ptr()) {
                push_ax_element_array(&list, &mut windows);
            }
            let resolved = find_text_element_in_window_candidates(
                &windows,
                window_id,
                |window| ax_element_window_id(window, pid),
                window_text_element,
            )?;
            if let Some((element, source)) = resolved {
                if let Some(ptr) = element.as_real_ptr() {
                    let error = unsafe {
                        AXUIElementSetMessagingTimeout(ptr, AX_APP_MESSAGING_TIMEOUT_SECONDS)
                    };
                    if error != K_AX_ERROR_SUCCESS {
                        log::warn!(
                            "remote-control: failed to set AX messaging timeout for resolved text element in pid {pid}: {error}"
                        );
                    }
                }
                return Ok(Some((element, source)));
            }
            Ok(None)
        }

        fn capabilities(&self, element: &AxElementHandle) -> AxCapabilities {
            let mut caps = AxCapabilities::default();
            let press = ax_press_action();
            let show_menu = ax_show_menu_action();
            // AXUIElementCopyActionNames is one IPC call; inspect the returned
            // array locally for both actions (#368 Phase 1).
            if let Some(names) = copy_action_names(element) {
                caps.pressable = action_names_contain(&names, press.as_ptr());
                caps.show_menu = action_names_contain(&names, show_menu.as_ptr());
            }
            let selected_text_range = ax_selected_text_range_attribute();
            if attribute_settable(element, selected_text_range.as_ptr()) {
                caps.text_selectable = true;
            }
            caps
        }

        fn press(&self, element: &AxElementHandle) -> Result<(), AxError> {
            let action = ax_press_action();
            perform_action(element, action.as_ptr())
        }

        fn show_menu(&self, element: &AxElementHandle) -> Result<(), AxError> {
            let action = ax_show_menu_action();
            perform_action(element, action.as_ptr())
        }

        fn text_length(&self, element: &AxElementHandle) -> Result<i64, AxError> {
            let count_attribute = ax_number_of_characters_attribute();
            match copy_attribute(element, count_attribute.as_ptr())
                .and_then(|number| cf_number_to_i64(number.as_ptr()))
            {
                Ok(len) if len >= 0 => Ok(len),
                Ok(_) => Err(AxError::new(K_AX_ERROR_NO_VALUE)),
                // AXNumberOfCharacters is not universally supported; fall back to
                // the length of the AXValue string.
                Err(error) if error.is_capability_miss() => {
                    let value_attribute = ax_value_attribute();
                    let value = copy_attribute(element, value_attribute.as_ptr())?;
                    cf_string_length(value.as_ptr())
                        .ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
                }
                Err(error) => Err(error),
            }
        }

        fn selected_text(&self, element: &AxElementHandle) -> Result<Option<String>, AxError> {
            let attribute = ax_selected_text_attribute();
            let value = copy_attribute(element, attribute.as_ptr())?;
            Ok(cf_string_to_owned(value.as_ptr()))
        }

        fn set_selected_text(&self, element: &AxElementHandle, text: &str) -> Result<(), AxError> {
            let value = cf_string_from_str(text)?;
            let attribute = ax_selected_text_attribute();
            set_attribute(element, attribute.as_ptr(), value.as_ptr())
        }

        fn offset_at_point(
            &self,
            element: &AxElementHandle,
            point: super::GlobalPoint,
        ) -> Result<i64, AxError> {
            let point_value = create_ax_value(
                K_AX_VALUE_TYPE_CG_POINT,
                &CGPoint {
                    x: point.x,
                    y: point.y,
                },
            )?;
            let range_value = copy_parameterized_attribute(
                element,
                ax_range_for_position_parameterized_attribute().as_ptr(),
                point_value.as_ptr(),
            )?;
            let mut range = CFRange::default();
            let ok = unsafe {
                AXValueGetValue(
                    range_value.as_ptr(),
                    K_AX_VALUE_TYPE_CF_RANGE,
                    &mut range as *mut CFRange as *mut c_void,
                )
            };
            if ok {
                Ok(range.location as i64)
            } else {
                Err(AxError::new(K_AX_ERROR_NO_VALUE))
            }
        }

        fn set_selected_range(
            &self,
            element: &AxElementHandle,
            start: i64,
            len: i64,
        ) -> Result<(), AxError> {
            let range = CFRange {
                location: start as isize,
                length: len as isize,
            };
            let value = create_ax_value(K_AX_VALUE_TYPE_CF_RANGE, &range)?;
            let attribute = ax_selected_text_range_attribute();
            set_attribute(element, attribute.as_ptr(), value.as_ptr())
        }

        fn scroll_by(
            &self,
            window_id: u32,
            point: super::GlobalPoint,
            element: &AxElementHandle,
            delta_px_y: f64,
            delta_px_x: f64,
        ) -> Result<bool, AxError> {
            let Some(target) = cached_scroll_target(window_id, point, element)? else {
                return Ok(false);
            };
            let Some(scroll_area) = target.scroll_area.as_ref() else {
                return Ok(false);
            };
            let mut changed = false;
            if delta_px_y != 0.0 {
                changed |= scroll_axis_with_target(
                    scroll_area,
                    target.vertical.as_ref(),
                    delta_px_y,
                    Axis::Vertical,
                )?;
            }
            if delta_px_x != 0.0 {
                changed |= scroll_axis_with_target(
                    scroll_area,
                    target.horizontal.as_ref(),
                    delta_px_x,
                    Axis::Horizontal,
                )?;
            }
            Ok(changed)
        }
    }

    fn ax_app_element_for_pid(pid: i32) -> Option<AxElementHandle> {
        if let Some(cached) = ax_app_element_cache().lock_unpoisoned().get(&pid).cloned() {
            return Some(cached);
        }
        let ptr = unsafe { AXUIElementCreateApplication(pid) };
        let app = CfObject::from_create(ptr).map(AxElementHandle::Real)?;
        if let Some(app_ptr) = app.as_real_ptr() {
            let error = unsafe {
                AXUIElementSetMessagingTimeout(app_ptr, AX_APP_MESSAGING_TIMEOUT_SECONDS)
            };
            if error != K_AX_ERROR_SUCCESS {
                log::warn!(
                    "remote-control: failed to set AX messaging timeout for pid {pid}: {error}"
                );
            }
        }
        ax_app_element_cache()
            .lock_unpoisoned()
            .insert(pid, app.clone());
        Some(app)
    }

    fn copy_action_names(element: &AxElementHandle) -> Option<CfObject> {
        let Some(ptr) = element.as_real_ptr() else {
            return None;
        };
        let mut out = std::ptr::null();
        ax_probe_counters().ax_ipc.fetch_add(1, Ordering::Relaxed);
        let error = unsafe { AXUIElementCopyActionNames(ptr, &mut out) };
        if error != K_AX_ERROR_SUCCESS {
            return None;
        }
        CfObject::from_create(out)
    }

    fn action_names_contain(names: &CfObject, action: *const c_void) -> bool {
        let count = unsafe { CFArrayGetCount(names.as_ptr()) };
        (0..count).any(|index| {
            let value = unsafe { CFArrayGetValueAtIndex(names.as_ptr(), index) };
            !value.is_null() && unsafe { CFEqual(value, action) }
        })
    }

    fn attribute_settable(element: &AxElementHandle, attribute: *const c_void) -> bool {
        let Some(ptr) = element.as_real_ptr() else {
            return false;
        };
        let mut settable = 0u8;
        let error = unsafe { AXUIElementIsAttributeSettable(ptr, attribute, &mut settable) };
        if error != K_AX_ERROR_SUCCESS {
            log::debug!(
                "remote-control: AXUIElementIsAttributeSettable failed for attribute {:?}: {}",
                attribute,
                error
            );
            return false;
        }
        settable != 0
    }

    fn is_ax_ui_element(ptr: *const c_void) -> bool {
        !ptr.is_null() && unsafe { CFGetTypeID(ptr) } == unsafe { AXUIElementGetTypeID() }
    }

    /// Append the AXUIElement entries of a CFArray attribute value to `out`.
    fn push_ax_element_array(array: &CfObject, out: &mut Vec<AxElementHandle>) {
        let ptr = array.as_ptr();
        if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFArrayGetTypeID() } {
            return;
        }
        let count = unsafe { CFArrayGetCount(ptr) };
        for index in 0..count {
            let value = unsafe { CFArrayGetValueAtIndex(ptr, index) };
            if is_ax_ui_element(value) {
                if let Some(object) = unsafe { CfObject::retain(value) } {
                    out.push(AxElementHandle::Real(object));
                }
            }
        }
    }

    fn find_text_element_in_window_candidates<T, R>(
        windows: &[T],
        window_id: u32,
        mut resolve_window_id: impl FnMut(&T) -> Result<u32, AxError>,
        mut resolve_text_element: impl FnMut(&T) -> Result<Option<R>, AxError>,
    ) -> Result<Option<R>, AxError> {
        let mut matched_window = false;
        let mut identity_unavailable = false;
        for window in windows {
            match resolve_window_id(window) {
                Ok(candidate_window_id) if candidate_window_id == window_id => {
                    matched_window = true;
                }
                Ok(_) => continue,
                Err(error) if error.is_window_identity_unavailable() => {
                    identity_unavailable = true;
                    continue;
                }
                Err(error) => return Err(error),
            }
            if let Some(element) = resolve_text_element(window)? {
                return Ok(Some(element));
            }
        }
        if matched_window {
            Ok(None)
        } else if identity_unavailable {
            Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE))
        } else {
            Err(AxError::new(K_AX_ERROR_WINDOW_ID_MISMATCH))
        }
    }

    fn all_window_frames_for_pid(pid: i32) -> Option<HashMap<u32, (f64, f64, f64, f64)>> {
        let frames: HashMap<u32, (f64, f64, f64, f64)> = crate::platform::cg::all_windows_lean()?
            .into_iter()
            .filter(|window| window.owner_pid == i64::from(pid))
            .filter_map(|window| {
                let wid = u32::try_from(window.number).ok()?;
                Some((wid, (window.x, window.y, window.w, window.h)))
            })
            .collect();
        (!frames.is_empty()).then_some(frames)
    }

    /// The ONE production AX-element -> CGWindowID path for remote control.
    /// `_AXUIElementGetWindow` is primary; when that symbol is unavailable the
    /// platform helper correlates AX frame against the fresh registry candidates
    /// and an OptionAll same-pid universe. Any miss/ambiguity/error is identity
    /// unavailable. Only a successfully resolved different id is a genuine
    /// authorization mismatch.
    fn ax_element_window_id(element: &AxElementHandle, pid: i32) -> Result<u32, AxError> {
        let Some(ptr) = element.as_real_ptr() else {
            return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
        };
        if !crate::platform::ax::ax_mechanism_available() {
            return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
        }
        // The private-symbol fast path needs no CG enumeration. Correlation
        // mode must refresh immediately before building candidates so a newly
        // created same-pid sibling participates in uniqueness and cannot evade
        // the authorization boundary via a stale ~10Hz snapshot.
        let (same_pid_frames, all_same_pid_frames) =
            if crate::platform::ax::get_window_symbol_available() {
                (
                    crate::platform::ax::CandidateFrames(HashMap::new()),
                    crate::platform::ax::UniverseFrames(HashMap::new()),
                )
            } else {
                let Some(registry) = crate::window_registry::global() else {
                    return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
                };
                let snapshot = registry.refresh_now();
                let frames: HashMap<u32, (f64, f64, f64, f64)> = snapshot
                    .by_id
                    .values()
                    .filter(|window| window.owner_pid == pid)
                    .map(|window| (window.wid, (window.rx, window.ry, window.rw, window.rh)))
                    .collect();
                if frames.is_empty() {
                    return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
                }
                let Some(all_frames) = all_window_frames_for_pid(pid) else {
                    return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
                };
                (
                    crate::platform::ax::CandidateFrames(frames),
                    crate::platform::ax::UniverseFrames(all_frames),
                )
            };
        // SAFETY: real AxElementHandle owns a retained AXUIElementRef for the
        // duration of this call.
        unsafe {
            crate::platform::ax::resolve_element_window_id(
                ptr,
                &same_pid_frames,
                &all_same_pid_frames,
            )
        }
        .map_err(|error| {
            log::debug!("remote-control: AX window identity unavailable for pid {pid}: {error:?}");
            AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE)
        })
    }

    /// A point hit-test starts at an arbitrary descendant. Resolve its
    /// containing window and require exact CGWindowID identity.
    fn element_belongs_to_window(
        element: &AxElementHandle,
        pid: i32,
        window_id: u32,
    ) -> Result<bool, AxError> {
        Ok(ax_element_window_id(element, pid)? == window_id)
    }

    /// AXFocusedWindow ONLY -- deliberately. AXFocusedWindow IS the key window,
    /// the one the responder chain hands keys to.
    ///
    /// Do NOT re-add an AXMainWindow fallback. Direct AX measurement (#777)
    /// showed AXFocusedWindow DOES resolve for a BACKGROUNDED app and returns
    /// the same window AXMainWindow does, so the fallback was inert -- and it is
    /// unsafe in principle, because the two diverge whenever an accessory panel
    /// (find bar, inspector, font panel) is key while a document window stays
    /// main. Accepting on main while focused named a SIBLING re-opens #759, and
    /// a controller can induce that divergence deliberately by clicking a
    /// "Find..." affordance through the authorized pointer path.
    ///
    /// A genuine mismatch is recovered in `verify_key_window` instead, by a bare
    /// `AXRaise` of the AUTHORIZED window while the app is not frontmost.
    fn focused_window_matches(pid: i32, window_id: u32) -> Result<bool, AxError> {
        let Some(app) = ax_app_element_for_pid(pid) else {
            return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
        };
        let focused = match copy_attribute(&app, ax_focused_window_attribute().as_ptr()) {
            Ok(window) if is_ax_ui_element(window.as_ptr()) => AxElementHandle::Real(window),
            Ok(_) => return Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE)),
            Err(error) => return Err(error),
        };
        Ok(ax_element_window_id(&focused, pid)? == window_id)
    }

    /// Is `pid`'s app the frontmost (active) application?
    ///
    /// FAIL CLOSED: every error path -- no app element, AX timeout, a value that
    /// is not `kCFBooleanTrue`/`False` -- answers `true`. The only caller uses
    /// this to decide whether raising the shared window is safe, and "cannot
    /// tell" must never authorize a raise: raising inside an app the sharer is
    /// actively using moves THEIR focus, and their next keystrokes land in the
    /// broadcast window (#777).
    fn app_is_frontmost(pid: i32) -> bool {
        let Some(app) = ax_app_element_for_pid(pid) else {
            return true;
        };
        match copy_attribute(&app, ax_frontmost_attribute().as_ptr()) {
            // Only an exact `kCFBooleanFalse` answers "not frontmost". A plain
            // `!ptr::eq(.., kCFBooleanTrue)` would answer `false` for ANY
            // non-boolean payload -- i.e. an unreadable value would AUTHORIZE
            // the raise, the exact opposite of this function's contract
            // (Fable review, #777).
            Ok(value) => !std::ptr::eq(value.as_ptr(), unsafe { kCFBooleanFalse }),
            Err(_) => true,
        }
    }

    /// Make the AUTHORIZED window its app's focused window WITHOUT activating
    /// the app: `AXRaise` on that one window and nothing else.
    ///
    /// Deliberately not `SystemSessionTapBackend::raise`, which also sets
    /// `AXFrontmost` -- that combination activates the app and steals the
    /// sharer's focus. Measured (#777): a bare `AXRaise` changes the app's
    /// `AXFocusedWindow` while the frontmost application stays untouched.
    /// NEVER set `AXFrontmost` here.
    ///
    /// Only the window whose resolved CGWindowID equals `window_id` can ever be
    /// raised; an identity capability failure is skipped, never raised.
    fn raise_authorized_window(pid: i32, window_id: u32) -> bool {
        let Some(app) = ax_app_element_for_pid(pid) else {
            return false;
        };
        let Ok(list) = copy_attribute(&app, ax_windows_attribute().as_ptr()) else {
            return false;
        };
        let mut windows = Vec::new();
        push_ax_element_array(&list, &mut windows);
        let Some(window) = windows
            .into_iter()
            .find(|window| ax_element_window_id(window, pid) == Ok(window_id))
        else {
            return false;
        };
        perform_action(&window, ax_raise_action().as_ptr()).is_ok()
    }

    /// Pure decision behind `verify_key_window`'s one-shot raise recovery.
    /// `focus_verdict` is `Some(matched)`, or `None` when the focus check itself
    /// errored. Raise only when the authorized window is NOT confirmed focused
    /// AND the target app is not frontmost -- a frontmost app is one the sharer
    /// is actively using, and raising there would redirect their own keystrokes
    /// into the broadcast window (#777).
    fn should_attempt_key_window_raise(focus_verdict: Option<bool>, app_frontmost: bool) -> bool {
        focus_verdict != Some(true) && !app_frontmost
    }

    /// #170: is this element an editable text element we can drive via
    /// AXSelectedText / AXSelectedTextRange? We treat "AXSelectedTextRange is a
    /// settable attribute" as the authoritative signal (matches the pointer
    /// `text_selectable` capability), which covers AXTextArea, AXTextField and
    /// AXComboBox editors without hard-coding roles.
    fn element_is_text_selectable(element: &AxElementHandle) -> bool {
        let attribute = ax_selected_text_range_attribute();
        attribute_settable(element, attribute.as_ptr())
    }

    /// #170: resolve a window's editable text element without app-scoped focus.
    /// Order: the window's own AXFocusedUIElement (window-scoped focus DOES
    /// resolve below the app level for a backgrounded app, unlike app-scoped),
    /// then a bounded role-agnostic descendant search for the first
    /// text-selectable element.
    fn window_text_element(
        window: &AxElementHandle,
    ) -> Result<Option<(AxElementHandle, TextElementSource)>, AxError> {
        let focused_attribute = ax_focused_ui_element_attribute();
        if let Ok(focused) = copy_attribute(window, focused_attribute.as_ptr()) {
            if is_ax_ui_element(focused.as_ptr()) {
                let focused = AxElementHandle::Real(focused);
                // Genuine window-scoped focus: the trustworthy target (F5).
                if element_is_text_selectable(&focused) {
                    return Ok(Some((focused, TextElementSource::FocusedElement)));
                }
                // BFS from the focused element down: shallowest text field, not
                // guaranteed to be the intended one -> fallback provenance.
                if let Some(found) = find_text_descendant(&focused)? {
                    return Ok(Some((found, TextElementSource::BfsFallback)));
                }
            }
        }
        Ok(find_text_descendant(window)?.map(|element| (element, TextElementSource::BfsFallback)))
    }

    /// Breadth-first search of an element's AXChildren subtree for the first
    /// text-selectable element. Bounded by `AX_TEXT_SEARCH_MAX_DEPTH` and
    /// `AX_TEXT_SEARCH_MAX_NODES` so a deep/wide tree can't stall replay.
    fn find_text_descendant(root: &AxElementHandle) -> Result<Option<AxElementHandle>, AxError> {
        let mut frontier = vec![root.clone()];
        let mut visited = 0usize;
        for _ in 0..AX_TEXT_SEARCH_MAX_DEPTH {
            let mut next: Vec<AxElementHandle> = Vec::new();
            for element in &frontier {
                if visited >= AX_TEXT_SEARCH_MAX_NODES {
                    return Ok(None);
                }
                visited += 1;
                if element_is_text_selectable(element) {
                    return Ok(Some(element.clone()));
                }
                let children_attribute = ax_children_attribute();
                match copy_attribute(element, children_attribute.as_ptr()) {
                    Ok(children) => push_ax_element_array(&children, &mut next),
                    Err(error) if error.is_capability_miss() => {}
                    Err(error) if error.is_api_disabled() => return Err(error),
                    Err(_) => {}
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(None)
    }

    fn cf_number_to_i64(number: *const c_void) -> Result<i64, AxError> {
        // F1: a third-party AX app (Electron/Java/etc.) can return a non-CFNumber
        // for AXNumberOfCharacters; calling CFNumberGetValue on it risks an
        // unrecognized-selector NSException -> abort. Guard on the concrete CF
        // type first, mirroring the `is_cf_string` pattern used for string attrs.
        if !is_cf_number(number) {
            return Err(AxError::new(K_AX_ERROR_NO_VALUE));
        }
        let mut value = 0i64;
        let ok = unsafe {
            CFNumberGetValue(
                number,
                K_CF_NUMBER_SINT64,
                &mut value as *mut i64 as *mut c_void,
            )
        };
        if ok {
            Ok(value)
        } else {
            Err(AxError::new(K_AX_ERROR_NO_VALUE))
        }
    }

    /// UTF-16 code-unit length of a CFString attribute value (matches how AX
    /// selection ranges are indexed), or None if the value isn't a CFString.
    fn cf_string_length(value: *const c_void) -> Option<i64> {
        if !is_cf_string(value) {
            return None;
        }
        let len = unsafe { CFStringGetLength(value) };
        (len >= 0).then_some(len as i64)
    }

    fn is_cf_string(value: *const c_void) -> bool {
        !value.is_null() && unsafe { CFGetTypeID(value) } == unsafe { CFStringGetTypeID() }
    }

    /// F1: is this CFTypeRef actually a CFNumber? Guards `cf_number_to_i64`
    /// against a non-NSNumber AX attribute value (see there).
    fn is_cf_number(value: *const c_void) -> bool {
        !value.is_null() && unsafe { CFGetTypeID(value) } == unsafe { CFNumberGetTypeID() }
    }

    /// Read a CFString of arbitrary length into an owned String. Unlike
    /// `cf_string_to_string` (fixed 128-byte scratch buffer, used for short role
    /// names), this sizes the buffer to the string so a full copied document
    /// survives Cmd+C. Returns None for non-strings or decode failure.
    fn cf_string_to_owned(value: *const c_void) -> Option<String> {
        if !is_cf_string(value) {
            return None;
        }
        let len = unsafe { CFStringGetLength(value) };
        if len < 0 {
            return None;
        }
        // Empty string is a legitimate value (empty selection); return it as-is.
        if len == 0 {
            return Some(String::new());
        }
        let max_bytes =
            unsafe { CFStringGetMaximumSizeForEncoding(len, K_CF_STRING_ENCODING_UTF8) };
        if max_bytes <= 0 {
            return None;
        }
        // +1 for the NUL terminator CFStringGetCString writes.
        let mut buffer = vec![0 as c_char; (max_bytes as usize) + 1];
        let ok = unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if !ok {
            return None;
        }
        Some(
            unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Build a CFString from a runtime &str (for AXSelectedText paste). Interior
    /// NULs can't survive a C string, so bail to a capability miss in that case.
    fn cf_string_from_str(text: &str) -> Result<CfObject, AxError> {
        let c_string =
            std::ffi::CString::new(text).map_err(|_| AxError::new(K_AX_ERROR_NO_VALUE))?;
        CfObject::from_create(unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                c_string.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        })
        .ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
    }

    fn perform_action(element: &AxElementHandle, action: *const c_void) -> Result<(), AxError> {
        let Some(ptr) = element.as_real_ptr() else {
            return Err(AxError::new(K_AX_ERROR_INVALID_UI_ELEMENT));
        };
        ax_result(unsafe { AXUIElementPerformAction(ptr, action) })
    }

    fn create_ax_value<T>(the_type: i32, value: &T) -> Result<CfObject, AxError> {
        CfObject::from_create(unsafe {
            AXValueCreate(the_type, value as *const T as *const c_void)
        })
        .ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
    }

    fn copy_parameterized_attribute(
        element: &AxElementHandle,
        attribute: *const c_void,
        parameter: *const c_void,
    ) -> Result<CfObject, AxError> {
        let Some(ptr) = element.as_real_ptr() else {
            return Err(AxError::new(K_AX_ERROR_INVALID_UI_ELEMENT));
        };
        let mut out = std::ptr::null();
        ax_result(unsafe {
            AXUIElementCopyParameterizedAttributeValue(ptr, attribute, parameter, &mut out)
        })?;
        CfObject::from_create(out).ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
    }

    fn copy_attribute(
        element: &AxElementHandle,
        attribute: *const c_void,
    ) -> Result<CfObject, AxError> {
        let Some(ptr) = element.as_real_ptr() else {
            return Err(AxError::new(K_AX_ERROR_INVALID_UI_ELEMENT));
        };
        let mut out = std::ptr::null();
        ax_result(unsafe { AXUIElementCopyAttributeValue(ptr, attribute, &mut out) })?;
        CfObject::from_create(out).ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
    }

    fn set_attribute(
        element: &AxElementHandle,
        attribute: *const c_void,
        value: *const c_void,
    ) -> Result<(), AxError> {
        let Some(ptr) = element.as_real_ptr() else {
            return Err(AxError::new(K_AX_ERROR_INVALID_UI_ELEMENT));
        };
        ax_result(unsafe { AXUIElementSetAttributeValue(ptr, attribute, value) })
    }

    fn ax_result(error: i32) -> Result<(), AxError> {
        if error == K_AX_ERROR_SUCCESS {
            Ok(())
        } else {
            Err(AxError::new(error))
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Axis {
        Vertical,
        Horizontal,
    }

    fn scroll_axis_with_target(
        scroll_area: &AxElementHandle,
        scrollbar: Option<&AxElementHandle>,
        delta_px: f64,
        axis: Axis,
    ) -> Result<bool, AxError> {
        let Some(scrollbar) = scrollbar else {
            return Ok(false);
        };
        let value_attribute = ax_value_attribute();
        let old_value = match copy_attribute(scrollbar, value_attribute.as_ptr())
            .and_then(|value| cf_number_to_f64(value.as_ptr()))
        {
            Ok(value) => value,
            Err(error) if error.is_capability_miss() => return Ok(false),
            Err(error) => return Err(error),
        };
        let scrollable_extent = scrollable_extent_for(scroll_area, axis).ok();
        let new_value = scrollbar_value_after_delta(old_value, delta_px, scrollable_extent);
        if (new_value - old_value).abs() <= f64::EPSILON {
            return Ok(false);
        }
        let number = cf_number_from_f64(new_value)?;
        set_attribute(scrollbar, value_attribute.as_ptr(), number.as_ptr())?;
        Ok(true)
    }

    fn cached_scroll_target(
        window_id: u32,
        point: super::GlobalPoint,
        element: &AxElementHandle,
    ) -> Result<Option<CachedAxScrollTarget>, AxError> {
        let key = ax_point_key(window_id, point);
        let now = Instant::now();
        if let Some(cached) = ax_scroll_target_cache()
            .lock_unpoisoned()
            .get(&key)
            .cloned()
        {
            if now.saturating_duration_since(cached.cached_at) < AX_RESOLUTION_CACHE_TTL {
                return Ok(Some(cached));
            }
            ax_scroll_target_cache().lock_unpoisoned().remove(&key);
        }
        let Some(scroll_area) = find_scroll_area(element)? else {
            let target = CachedAxScrollTarget {
                scroll_area: None,
                vertical: None,
                horizontal: None,
                cached_at: now,
            };
            ax_scroll_target_cache()
                .lock_unpoisoned()
                .insert(key, target);
            return Ok(None);
        };
        // #368 F4: probe each axis independently and non-fatally. A missing or
        // broken scrollbar on the axis we are NOT scrolling must not fail the
        // whole scroll — only a revoked-permission error is fatal; every other
        // failure just means "no cached scrollbar for this axis".
        let scrollbar = |attribute: CfObject| -> Result<Option<AxElementHandle>, AxError> {
            match copy_attribute(&scroll_area, attribute.as_ptr()) {
                Ok(value) => Ok(Some(AxElementHandle::Real(value))),
                Err(error) if error.is_api_disabled() => Err(error),
                Err(_) => Ok(None),
            }
        };
        let vertical = scrollbar(ax_vertical_scrollbar_attribute())?;
        let horizontal = scrollbar(ax_horizontal_scrollbar_attribute())?;
        let target = CachedAxScrollTarget {
            scroll_area: Some(scroll_area),
            vertical,
            horizontal,
            cached_at: now,
        };
        ax_scroll_target_cache()
            .lock_unpoisoned()
            .insert(key, target.clone());
        Ok(Some(target))
    }

    fn find_scroll_area(element: &AxElementHandle) -> Result<Option<AxElementHandle>, AxError> {
        let mut current = element.clone();
        for _ in 0..AX_SCROLL_PARENT_HOPS {
            let role_attribute = ax_role_attribute();
            let scroll_area_role = ax_scroll_area_role();
            match copy_attribute(&current, role_attribute.as_ptr()) {
                Ok(role) if unsafe { CFEqual(role.as_ptr(), scroll_area_role.as_ptr()) } => {
                    return Ok(Some(current));
                }
                Ok(_) => {}
                Err(error) if error.is_capability_miss() => {}
                Err(error) => return Err(error),
            }
            let parent_attribute = ax_parent_attribute();
            current = match copy_attribute(&current, parent_attribute.as_ptr()) {
                Ok(parent) => AxElementHandle::Real(parent),
                Err(error) if error.is_capability_miss() => return Ok(None),
                Err(error) => return Err(error),
            };
        }
        Ok(None)
    }

    fn scrollable_extent_for(scroll_area: &AxElementHandle, axis: Axis) -> Result<f64, AxError> {
        let viewport = ax_size(scroll_area)?;
        let content = scroll_area_content(scroll_area).and_then(|content| ax_size(&content))?;
        let extent = match axis {
            Axis::Vertical => content.height - viewport.height,
            Axis::Horizontal => content.width - viewport.width,
        };
        if extent.is_finite() && extent > 0.0 {
            Ok(extent)
        } else {
            Err(AxError::new(K_AX_ERROR_NO_VALUE))
        }
    }

    fn scroll_area_content(scroll_area: &AxElementHandle) -> Result<AxElementHandle, AxError> {
        let contents_attribute = ax_contents_attribute();
        let contents = copy_attribute(scroll_area, contents_attribute.as_ptr())?;
        let type_id = unsafe { CFGetTypeID(contents.as_ptr()) };
        if type_id == unsafe { CFArrayGetTypeID() } {
            let count = unsafe { CFArrayGetCount(contents.as_ptr()) };
            if count <= 0 {
                return Err(AxError::new(K_AX_ERROR_NO_VALUE));
            }
            let first = unsafe { CFArrayGetValueAtIndex(contents.as_ptr(), 0) };
            if first.is_null() {
                return Err(AxError::new(K_AX_ERROR_NO_VALUE));
            }
            return unsafe { CfObject::retain(first) }
                .map(AxElementHandle::Real)
                .ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE));
        }
        if type_id == unsafe { AXUIElementGetTypeID() } {
            return Ok(AxElementHandle::Real(contents));
        }
        Err(AxError::new(K_AX_ERROR_NO_VALUE))
    }

    fn ax_size(element: &AxElementHandle) -> Result<CGSize, AxError> {
        let size_attribute = ax_size_attribute();
        let size_value = copy_attribute(element, size_attribute.as_ptr())?;
        let mut size = CGSize::default();
        let ok = unsafe {
            AXValueGetValue(
                size_value.as_ptr(),
                K_AX_VALUE_TYPE_CG_SIZE,
                &mut size as *mut CGSize as *mut c_void,
            )
        };
        if ok {
            Ok(size)
        } else {
            Err(AxError::new(K_AX_ERROR_NO_VALUE))
        }
    }

    fn scrollbar_value_after_delta(
        old_value: f64,
        delta_px: f64,
        scrollable_extent: Option<f64>,
    ) -> f64 {
        let delta = if let Some(extent) = scrollable_extent.filter(|extent| *extent > 0.0) {
            delta_px / extent
        } else if delta_px > 0.0 {
            AX_FALLBACK_SCROLL_FRACTION
        } else if delta_px < 0.0 {
            -AX_FALLBACK_SCROLL_FRACTION
        } else {
            0.0
        };
        (old_value + delta).clamp(0.0, 1.0)
    }

    fn cf_number_to_f64(number: *const c_void) -> Result<f64, AxError> {
        let mut value = 0.0f64;
        let ok = unsafe {
            CFNumberGetValue(
                number,
                K_CF_NUMBER_FLOAT64,
                &mut value as *mut f64 as *mut c_void,
            )
        };
        if ok {
            Ok(value)
        } else {
            Err(AxError::new(K_AX_ERROR_NO_VALUE))
        }
    }

    fn cf_number_from_f64(value: f64) -> Result<CfObject, AxError> {
        CfObject::from_create(unsafe {
            CFNumberCreate(
                std::ptr::null(),
                K_CF_NUMBER_FLOAT64,
                &value as *const f64 as *const c_void,
            )
        })
        .ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
    }

    fn cf_string(bytes_with_nul: &'static [u8]) -> CfObject {
        CfObject::from_create(unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                bytes_with_nul.as_ptr().cast::<c_char>(),
                K_CF_STRING_ENCODING_UTF8,
            )
        })
        .expect("AX CFString creation should not fail for static UTF-8")
    }

    fn cf_string_to_string(value: *const c_void) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let mut buffer = [0 as c_char; 128];
        let ok = unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if !ok {
            return None;
        }
        Some(
            unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn ax_role_description(element: &AxElementHandle) -> String {
        #[cfg(test)]
        if let Some(id) = element.test_id() {
            return format!("Test({id})");
        }

        let role_attribute = ax_role_attribute();
        match copy_attribute(element, role_attribute.as_ptr()) {
            Ok(role) => {
                cf_string_to_string(role.as_ptr()).unwrap_or_else(|| "<unreadable>".to_string())
            }
            Err(error) => format!("<error {:?}>", error),
        }
    }

    fn ax_press_action() -> CfObject {
        cf_string(b"AXPress\0")
    }

    fn ax_show_menu_action() -> CfObject {
        cf_string(b"AXShowMenu\0")
    }

    fn ax_selected_text_range_attribute() -> CfObject {
        cf_string(b"AXSelectedTextRange\0")
    }

    // #170 attribute names for the window-scoped text-element resolution and the
    // Cmd+A / Cmd+C / Cmd+V AX round-trip.
    fn ax_selected_text_attribute() -> CfObject {
        cf_string(b"AXSelectedText\0")
    }

    fn ax_number_of_characters_attribute() -> CfObject {
        cf_string(b"AXNumberOfCharacters\0")
    }

    fn ax_focused_ui_element_attribute() -> CfObject {
        cf_string(b"AXFocusedUIElement\0")
    }

    fn ax_focused_window_attribute() -> CfObject {
        cf_string(b"AXFocusedWindow\0")
    }

    fn ax_main_window_attribute() -> CfObject {
        cf_string(b"AXMainWindow\0")
    }

    fn ax_windows_attribute() -> CfObject {
        cf_string(b"AXWindows\0")
    }

    fn ax_frontmost_attribute() -> CfObject {
        cf_string(b"AXFrontmost\0")
    }

    /// #777: raise ONE window without touching `AXFrontmost`. Pairing the two
    /// activates the app and steals the sharer's focus -- see
    /// `raise_authorized_window`.
    fn ax_raise_action() -> CfObject {
        cf_string(b"AXRaise\0")
    }

    fn ax_children_attribute() -> CfObject {
        cf_string(b"AXChildren\0")
    }

    fn ax_range_for_position_parameterized_attribute() -> CfObject {
        cf_string(b"AXRangeForPosition\0")
    }

    fn ax_parent_attribute() -> CfObject {
        cf_string(b"AXParent\0")
    }

    fn ax_role_attribute() -> CfObject {
        cf_string(b"AXRole\0")
    }

    fn ax_scroll_area_role() -> CfObject {
        cf_string(b"AXScrollArea\0")
    }

    fn ax_vertical_scrollbar_attribute() -> CfObject {
        cf_string(b"AXVerticalScrollBar\0")
    }

    fn ax_horizontal_scrollbar_attribute() -> CfObject {
        cf_string(b"AXHorizontalScrollBar\0")
    }

    fn ax_value_attribute() -> CfObject {
        cf_string(b"AXValue\0")
    }

    fn ax_size_attribute() -> CfObject {
        cf_string(b"AXSize\0")
    }

    fn ax_contents_attribute() -> CfObject {
        cf_string(b"AXContents\0")
    }

    impl SlClickBackend for SystemSlClickBackend {
        fn post_click(
            &self,
            pid: i32,
            point: super::GlobalPoint,
            button: RemoteControlButton,
            click_state: u32,
        ) -> Result<(), SlClickError> {
            let Some(post_to_pid) = sl_event_post_to_pid() else {
                return Err(SlClickError::Unavailable);
            };
            post_sl_mouse_click(post_to_pid, pid, point, button, click_state)
        }

        fn post_mouse_event(
            &self,
            pid: i32,
            point: super::GlobalPoint,
            button: RemoteControlButton,
            event: SlMouseEvent,
        ) -> Result<(), SlClickError> {
            let Some(post_to_pid) = sl_event_post_to_pid() else {
                return Err(SlClickError::Unavailable);
            };
            post_sl_mouse_event(
                post_to_pid,
                pid,
                point,
                button,
                match (event, button) {
                    (SlMouseEvent::Down, RemoteControlButton::Left) => MouseKind::LeftDown,
                    (SlMouseEvent::Down, RemoteControlButton::Right) => MouseKind::RightDown,
                    (SlMouseEvent::Down, RemoteControlButton::Middle) => MouseKind::OtherDown,
                    (SlMouseEvent::Up, RemoteControlButton::Left) => MouseKind::LeftUp,
                    (SlMouseEvent::Up, RemoteControlButton::Right) => MouseKind::RightUp,
                    (SlMouseEvent::Up, RemoteControlButton::Middle) => MouseKind::OtherUp,
                    (SlMouseEvent::Dragged, RemoteControlButton::Left) => MouseKind::LeftDragged,
                    (SlMouseEvent::Dragged, RemoteControlButton::Right) => MouseKind::RightDragged,
                    (SlMouseEvent::Dragged, RemoteControlButton::Middle) => MouseKind::OtherDragged,
                },
                1,
            )
        }

        fn post_scroll(
            &self,
            pid: i32,
            point: super::GlobalPoint,
            delta_y: i32,
            delta_x: i32,
            flags: u64,
        ) -> Result<(), SlClickError> {
            let Some(post_to_pid) = sl_event_post_to_pid() else {
                return Err(SlClickError::Unavailable);
            };
            post_sl_scroll(post_to_pid, pid, point, delta_y, delta_x, flags)
        }
    }

    // ---------------------------------------------------------------------
    // #446: session-tap pointer route -- tier 3 of the injection ladder.
    //
    // The ladder is ordered most-semantic-first, because every tier satisfied
    // above tier 3 avoids moving the host's cursor at all:
    //
    //   1. Semantic accessibility -- set a field's value, select text, invoke
    //      an element's action, scroll an element by identity. Addressed by
    //      element identity rather than coordinates, so it costs no cursor
    //      movement and works on a background window. This is
    //      `replay_pointer_via_ax` / `replay_wheel_via_ax`, and it stays FIRST.
    //   2. (Seam -- deliberately not implemented.) Any route that delivers
    //      WITHOUT moving the cursor and WITHOUT changing z-order. Defined by
    //      that property rather than by one technology, because more than one
    //      candidate fits and none is demonstrated yet:
    //        - scripting/automation events for apps exposing a scripting
    //          interface (needs a per-app story plus an Automation TCC grant
    //          Petal does not currently request);
    //        - establishing synthetic app-active/key-window state for the
    //          target FIRST and only then posting per-PID -- an activation
    //          step the existing per-PID routes never performed. Untested
    //          against an arbitrary custom-drawn target.
    //      Any such tier is a new `GestureMode` variant plus its own backend
    //      trait, mirroring `SessionTapBackend`. Each of the three tier-3
    //      entry points below (`session_tap_pointer_down`, `session_tap_wheel`,
    //      `session_tap_semantic_click`) is reached only after tier 1 has
    //      already returned PassThrough, so a tier 2 slots in as one branch in
    //      front of each rather than a restructure.
    //      Do NOT default a tier 2 on without a delivered-NSEvent count from a
    //      real target: every route here is fire-and-forget, so "posted" reads
    //      identical to "delivered" -- the exact trap that kept #446 open.
    //   3. Coordinate-based synthetic pointer input -- below. The only
    //      universal route, and the only one that costs a cursor takeover.
    //
    // Measured 2026-07-27, macOS 26.5.2 arm64, against an AppKit target that
    // logs every `NSWindow.sendEvent:`, with the target verified
    // `active=true key=true` at post time. Same target, same coordinates,
    // same three gestures:
    //
    //   route                        click        drag                  scroll
    //   CGEventPostToPid             0 down/0 up  0                     0 wheel
    //   SLEventPostToPid             0 down/0 up  0                     0 wheel
    //   CGEventPost(session tap)     1 down/1 up  1 down/10 drag/1 up   10 wheel
    //
    // Two constraints came out of the same runs and are load-bearing:
    //   1. The cursor ALWAYS moves -- WindowServer snaps the pointer to the
    //      posted coordinate. There is no cursor-preserving variant, hence
    //      the save/restore below.
    //   2. Delivery is geometry-hit-tested: zero events landed until the
    //      target window was frontmost, unobscured, and on the ACTIVE Space.
    //      (A target parked on another Space reads as "posted, not delivered"
    //      exactly like the two dead routes -- the trap that cost this issue
    //      three weeks.)
    // ---------------------------------------------------------------------

    /// How close the cursor must still be to where we last posted it for a
    /// restore to be considered safe. Beyond this, the host physically moved
    /// the mouse during our gesture and we must not yank it back.
    const SESSION_TAP_CURSOR_TOLERANCE_POINTS: f64 = 6.0;

    /// A wheel stream is a high-rate burst (30+ events in under half a
    /// second). Restoring per event would fight the stream, so the restore is
    /// debounced until the stream has been quiet this long.
    const SESSION_TAP_WHEEL_SETTLE: Duration = Duration::from_millis(300);

    #[derive(Debug, Clone, Copy)]
    struct CursorTakeover {
        /// Where the host's cursor was before we took it over.
        saved: super::GlobalPoint,
        /// The last point WE posted. The restore is skipped unless the cursor
        /// is still here -- see `restore_is_safe`.
        last_posted: super::GlobalPoint,
        /// Restore no earlier than this (wheel-stream debounce).
        restore_after: Option<Instant>,
    }

    static CURSOR_TAKEOVERS: OnceLock<Mutex<HashMap<u32, CursorTakeover>>> = OnceLock::new();
    static CURSOR_RESTORE_WATCHDOG: OnceLock<()> = OnceLock::new();
    static SESSION_TAP_UNTRUSTED_LOGGED: OnceLock<()> = OnceLock::new();

    fn cursor_takeovers() -> &'static Mutex<HashMap<u32, CursorTakeover>> {
        CURSOR_TAKEOVERS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Give allowed when asking whether a point is inside the TARGET's own
    /// bounds. Absorbs the `i32` rounding in `WindowFrame` against this
    /// snapshot's `f64` bounds; see [`StackWindow::covers_target_point`].
    const TARGET_POINT_EDGE_SLOP: f64 = 2.0;

    /// One on-screen window as the pre-post hit test needs to see it, in the
    /// global top-left-origin space `GlobalPoint` uses.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct StackWindow {
        window_id: i64,
        owner_pid: i32,
        /// `kCGWindowLayer`. Load-bearing: see `BLOCKING_LAYER_MAX`.
        layer: i64,
        alpha: f64,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    impl StackWindow {
        fn contains(&self, point: super::GlobalPoint) -> bool {
            point.x >= self.x
                && point.x < self.x + self.w
                && point.y >= self.y
                && point.y < self.y + self.h
        }

        /// Same rectangle as [`Self::contains`], but inclusive and with
        /// [`TARGET_POINT_EDGE_SLOP`] of give -- only for asking "is this the
        /// target's own area?", never for occlusion.
        ///
        /// Two separate reasons the strict half-open form would nack input
        /// that is entirely legitimate, which is the failure mode that
        /// matters here (#777 shipped a guard verified only against the
        /// blocked case and refused 284 real key events live):
        ///
        /// 1. `normalized_to_global` maps a normalized 1.0 to exactly
        ///    `frame.x + width`, so a controller clicking the extreme right
        ///    or bottom edge produces a point `contains` rejects outright.
        /// 2. The point is mapped through a `WindowFrame`, whose fields are
        ///    `i32` rounded from the CG bounds (`platform/cg.rs`), while this
        ///    entry keeps the raw `f64`. A window at a fractional origin
        ///    therefore yields a cached rectangle offset from the live one by
        ///    up to a point in each direction, entirely in normal operation.
        ///
        /// The slop costs nothing against what this guard exists to stop: a
        /// point in a DIFFERENT window, which is a whole window away, not two
        /// points away.
        ///
        /// The half-open `contains` stays correct for OCCLUDERS, where a
        /// shared edge belongs to the window below and slop would manufacture
        /// nacks.
        fn covers_target_point(&self, point: super::GlobalPoint) -> bool {
            point.x >= self.x - TARGET_POINT_EDGE_SLOP
                && point.x <= self.x + self.w + TARGET_POINT_EDGE_SLOP
                && point.y >= self.y - TARGET_POINT_EDGE_SLOP
                && point.y <= self.y + self.h + TARGET_POINT_EDGE_SLOP
        }
    }

    /// What a snapshot of the window stack says about whether a coordinate
    /// post can reach the target *right now*.
    ///
    /// This is a PRECONDITION, never a delivery receipt. See
    /// [`hit_test_target`].
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum HitTestVerdict {
        /// Nothing another process owns sits in front of the target at this
        /// point in the snapshot.
        NothingInFront,
        /// A window belonging to a different process is in front of the
        /// target at this point, so WindowServer will route the click there.
        CoveredBy { owner_pid: i32, window_id: i64 },
        /// The target window is not in the on-screen list at all (minimized,
        /// another Space, or gone). Nothing posted at a screen coordinate can
        /// reach it.
        TargetNotOnScreen,
        /// The target window IS on screen, but the point does not lie within
        /// its live bounds -- so the post lands on some other window by
        /// definition, and this route must not take it. See `hit_test_target`.
        TargetNotAtPoint,
        /// The stack could not be read. NOT a failure -- a CoreGraphics
        /// hiccup must never manufacture a nack for input that would have
        /// landed.
        Unknown,
    }

    /// Highest `kCGWindowLayer` that can actually block a coordinate post.
    ///
    /// Measured live, not guessed. The AppKit window levels that matter:
    /// normal windows are 0, `.floating` is 3, `.modalPanel` is 8 -- all of
    /// which a shared window can genuinely be buried under. Above that sits
    /// system chrome that does NOT consume clicks at arbitrary points: the
    /// Dock owns a **full-screen** window at layer 20 (observed
    /// `0,0 1512x982`, alpha 1.0, in front of everything), the menu bar is
    /// 24, Control Center 25, pop-up menus 101.
    ///
    /// Without this bound the Dock's full-screen window matches every point on
    /// the display and the tier nacks ALL input. That is not hypothetical --
    /// it regressed six live acceptance cases (A1/A2/A3/A5/A6/A7) that pass
    /// without it, while `PC-DIRECT` kept landing a real click at the exact
    /// same coordinate, proving the "occluder" blocked nothing.
    ///
    /// Trade stated honestly: a genuine occluder above this band is not
    /// detected. That is the conservative direction, and consistent with the
    /// rest of this check -- never manufacture a nack for input that lands.
    const BLOCKING_LAYER_MAX: i64 = 8;

    /// #599: decide, from a snapshot of the front-to-back on-screen window
    /// stack, whether a coordinate post at `point` can reach the target.
    ///
    /// **What this does and does not prove.** `CGEventPost` is fire-and-
    /// forget: there is no post-hoc signal that an event was delivered. What
    /// IS knowable is that delivery is geometry-hit-tested by WindowServer --
    /// the event goes to whatever window is topmost at that screen point --
    /// so a target buried under another process's window provably cannot
    /// receive it. Checking that *before* posting converts a silent false
    /// success into a narrow race: the stack can change between this snapshot
    /// and the post, and this function cannot see that. It is a precondition
    /// check, not delivery confirmation, and must never be named or logged as
    /// one.
    ///
    /// Two deliberate exclusions keep this from manufacturing false nacks:
    ///
    /// **Our own windows** (`self_pid`) never block, and that is now the ONLY
    /// pid-based exclusion. Petal's overlays -- share border, hover tab,
    /// telepointer -- sit over the shared window by construction and are
    /// click-through; treating them as occluders would nack every healthy
    /// gesture.
    ///
    /// A second exclusion for the TARGET's own pid used to sit here (#599). It
    /// was a stated conservative preference, not a mechanism requirement, and
    /// it left the target's unshared sibling windows able to swallow a
    /// controller's click silently (#759). Do NOT restore it for symmetry. The
    /// loop iterates only windows strictly in front of the target, so the
    /// authorized window is excluded by construction, and the `alpha <= 0.0`
    /// and layer-band skips below still shield transparent scaffolding and
    /// system chrome.
    ///
    /// The deliberate consequence: an attached modal SHEET is a separate
    /// same-pid window and now blocks. That is intended. Window capture serves
    /// the shared window alone, so a sheet is invisible to the controller --
    /// delivering their click into UI they cannot see is the #759 accident
    /// itself, not a workaround for it. Refusing is the honest answer.
    fn hit_test_target(
        stack: Option<&[StackWindow]>,
        target_window_id: u32,
        _target_pid: i32,
        point: super::GlobalPoint,
        self_pid: i32,
    ) -> HitTestVerdict {
        let Some(stack) = stack else {
            return HitTestVerdict::Unknown;
        };
        let Some(target_idx) = stack
            .iter()
            .position(|entry| entry.window_id >= 0 && entry.window_id as u32 == target_window_id)
        else {
            return HitTestVerdict::TargetNotOnScreen;
        };
        // #759: the point must lie inside the TARGET's own live bounds before
        // anything else is asked. Without this the loop below is close to
        // vacuous for a point outside the shared window: the caller has just
        // AXRaised the target, so `stack[..target_idx]` is nearly empty and
        // almost any such point answers NothingInFront -- and the coordinate
        // post then lands on whichever window really is there, which under the
        // scenario this issue is about is an unshared sibling of the same app.
        //
        // The point is derived by clamping the controller's normalized
        // coordinates into the CACHED frame for `target_window_id`
        // (`normalized_to_global` over `cached_control_frame`, ~1s TTL, backed
        // by ~100ms pollers). This entry is the LIVE CGWindow bounds from the
        // same snapshot the occlusion loop reads, so comparing them is exactly
        // the staleness check the cached frame cannot make about itself: while
        // the two agree the point is inside by construction and nothing is
        // refused, and when they have diverged far enough for the point to
        // leave the real window, refusing is the only honest answer.
        if !stack[target_idx].covers_target_point(point) {
            return HitTestVerdict::TargetNotAtPoint;
        }
        for front in &stack[..target_idx] {
            if front.owner_pid == self_pid {
                continue;
            }
            // A fully transparent window paints nothing; treat it as not
            // covering rather than risk nacking on invisible scaffolding.
            if front.alpha <= 0.0 {
                continue;
            }
            // System chrome above the ordinary application band does not
            // consume clicks at arbitrary points -- see BLOCKING_LAYER_MAX.
            if front.layer < 0 || front.layer > BLOCKING_LAYER_MAX {
                continue;
            }
            if front.contains(point) {
                return HitTestVerdict::CoveredBy {
                    owner_pid: front.owner_pid,
                    window_id: front.window_id,
                };
            }
        }
        HitTestVerdict::NothingInFront
    }

    trait SessionTapBackend: Sync {
        fn post_mouse(
            &self,
            point: super::GlobalPoint,
            button: RemoteControlButton,
            kind: MouseKind,
            click_state: u32,
        ) -> Result<(), String>;

        fn post_scroll(
            &self,
            point: super::GlobalPoint,
            delta_y: i32,
            delta_x: i32,
            flags: u64,
        ) -> Result<(), String>;

        /// Current host cursor position, or `None` if it cannot be read.
        fn cursor_position(&self) -> Option<super::GlobalPoint>;

        /// Move the cursor without generating a click. Used both to warp INTO
        /// the shared window before a gesture and to put the host's cursor
        /// back afterwards.
        fn move_cursor(&self, point: super::GlobalPoint) -> Result<(), String>;

        /// Raise the target application's window so WindowServer's hit-test
        /// resolves to it. Returns false if it could not be raised.
        fn raise(&self, pid: i32, window_id: u32) -> bool;

        /// Snapshot of the on-screen windows in front-to-back order, for the
        /// pre-post hit test. `None` means the stack could not be read, which
        /// callers must treat as "unknown", never as "blocked".
        fn onscreen_stack(&self) -> Option<Vec<StackWindow>>;

        /// The same stack, but the CHEAP read: whatever the ~10Hz registry
        /// snapshot already holds, never a forced CG sweep. For per-event
        /// gates on high-rate streams (hover moves) that must honour
        /// `session/share.rs`'s "never enumerate per event" invariant.
        /// Defaults to `onscreen_stack` so recording backends are unaffected.
        fn cached_onscreen_stack(&self) -> Option<Vec<StackWindow>> {
            self.onscreen_stack()
        }

        fn is_trusted(&self) -> bool;
    }

    struct SystemSessionTapBackend;

    impl SystemSessionTapBackend {
        fn post(event: *mut c_void) {
            unsafe {
                CGEventPost(K_CG_SESSION_EVENT_TAP, event);
                CFRelease(event.cast_const());
            }
        }
    }

    impl SessionTapBackend for SystemSessionTapBackend {
        fn post_mouse(
            &self,
            point: super::GlobalPoint,
            button: RemoteControlButton,
            kind: MouseKind,
            click_state: u32,
        ) -> Result<(), String> {
            unsafe {
                let event = CGEventCreateMouseEvent(
                    std::ptr::null(),
                    mouse_kind_code(kind),
                    CGPoint {
                        x: point.x,
                        y: point.y,
                    },
                    button_number(button),
                );
                if event.is_null() {
                    return Err("CGEventCreateMouseEvent returned null".to_string());
                }
                CGEventSetIntegerValueField(
                    event,
                    K_CG_MOUSE_EVENT_BUTTON_NUMBER,
                    button_number(button) as i64,
                );
                CGEventSetIntegerValueField(
                    event,
                    K_CG_MOUSE_EVENT_CLICK_STATE,
                    i64::from(click_state.max(1)),
                );
                Self::post(event);
            }
            Ok(())
        }

        fn post_scroll(
            &self,
            point: super::GlobalPoint,
            delta_y: i32,
            delta_x: i32,
            flags: u64,
        ) -> Result<(), String> {
            unsafe {
                let event = CGEventCreateScrollWheelEvent(
                    std::ptr::null(),
                    K_CG_SCROLL_EVENT_UNIT_PIXEL,
                    1,
                    -delta_y,
                );
                if event.is_null() {
                    return Err("CGEventCreateScrollWheelEvent returned null".to_string());
                }
                CGEventSetIntegerValueField(
                    event,
                    K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                    i64::from(-delta_x),
                );
                CGEventSetFlags(event, flags);
                CGEventSetLocation(
                    event,
                    CGPoint {
                        x: point.x,
                        y: point.y,
                    },
                );
                Self::post(event);
            }
            Ok(())
        }

        fn cursor_position(&self) -> Option<super::GlobalPoint> {
            unsafe {
                let event = CGEventCreate(std::ptr::null());
                if event.is_null() {
                    return None;
                }
                let location = CGEventGetLocation(event);
                CFRelease(event.cast_const());
                Some(super::GlobalPoint {
                    x: location.x,
                    y: location.y,
                })
            }
        }

        fn move_cursor(&self, point: super::GlobalPoint) -> Result<(), String> {
            self.post_mouse(point, RemoteControlButton::Left, MouseKind::Moved, 1)
        }

        fn raise(&self, pid: i32, window_id: u32) -> bool {
            // Delivery is hit-tested against the real window stack, so the
            // target must be frontmost. AX raise + AXFrontmost is what the
            // live pass found actually works cross-process (a plain
            // NSRunningApplication.activate from a background app does not).
            let (Ok(raise_action), Ok(frontmost_attr)) = (
                cf_string_from_str("AXRaise"),
                cf_string_from_str("AXFrontmost"),
            ) else {
                return false;
            };
            let Some(app) = CfObject::from_create(unsafe { AXUIElementCreateApplication(pid) })
                .map(AxElementHandle::Real)
            else {
                return false;
            };
            let Ok(list) = copy_attribute(&app, ax_windows_attribute().as_ptr()) else {
                return false;
            };
            let mut windows = Vec::new();
            push_ax_element_array(&list, &mut windows);
            let Some(window) = windows
                .into_iter()
                .find(|window| ax_element_window_id(window, pid) == Ok(window_id))
            else {
                return false;
            };
            unsafe {
                let Some(app_ptr) = app.as_real_ptr() else {
                    return false;
                };
                let Some(window_ptr) = window.as_real_ptr() else {
                    return false;
                };
                let raised = AXUIElementPerformAction(window_ptr, raise_action.as_ptr()) == 0;
                let fronted =
                    AXUIElementSetAttributeValue(app_ptr, frontmost_attr.as_ptr(), kCFBooleanTrue)
                        == 0;
                // Fable review (#759): `raised || fronted` let a
                // window-specific AXRaise FAILURE (minimized, no AXRaise
                // support, AX timeout) report success as long as the
                // app-level AXFrontmost happened to succeed. AXFrontmost only
                // brings the app forward -- it says nothing about which of
                // the app's windows ends up on top, so this could report
                // "raised" while leaving an unauthorized sibling window
                // frontmost. `raised` is the property callers actually rely
                // on (delivery is hit-tested against the real window stack,
                // so only the specific authorized window being on top makes
                // the security guarantee hold) -- require it. `fronted` is
                // still worth attempting (it can help AXRaise itself succeed
                // for a backgrounded app) and worth logging when it's the
                // only thing that worked, since that's a real signal the
                // window-specific raise is failing for some reason.
                if !raised && fronted {
                    log::debug!(
                        "remote-control: raise(pid={pid}, window_id={window_id}) brought the app forward but AXRaise on the specific window failed -- not reporting success"
                    );
                }
                raised
            }
        }

        fn onscreen_stack(&self) -> Option<Vec<StackWindow>> {
            // Same read-only CGWindowList idiom window_diag/share_border/
            // telepointer already use, via the shared leaf wrapper. This is
            // not AppKit, so it is safe off the main thread (this runs on the
            // `petal-rc-inject` thread).
            // #744: route through the registry. The blocking hit-test's
            // 3x20ms retry wants FRESH truth each attempt, so this uses
            // refresh_now() (a forced CG sweep), not the ~10Hz snapshot --
            // preserving the retry semantics while removing the last direct
            // enumeration here. Falls back to a direct lean enumeration only
            // before the registry global is set.
            let to_stack = |snap: std::sync::Arc<crate::window_registry::Snapshot>| {
                snap.records_front_to_back()
                    .map(|r| StackWindow {
                        window_id: r.wid as i64,
                        owner_pid: r.owner_pid,
                        layer: r.layer,
                        alpha: r.alpha,
                        x: r.rx,
                        y: r.ry,
                        w: r.rw,
                        h: r.rh,
                    })
                    .collect::<Vec<_>>()
            };
            match crate::window_registry::global() {
                Some(reg) => Some(to_stack(reg.refresh_now())),
                None => Some(Self::lean_stack()?),
            }
        }

        fn cached_onscreen_stack(&self) -> Option<Vec<StackWindow>> {
            // Hover-move gate: read the registry's
            // existing ~10Hz snapshot, never `refresh_now()` -- this runs per
            // event on a 30Hz+ stream. Before the registry exists, one lean
            // enumeration is the only truth available.
            match crate::window_registry::global() {
                Some(reg) => Some(
                    reg.snapshot()
                        .records_front_to_back()
                        .map(|r| StackWindow {
                            window_id: r.wid as i64,
                            owner_pid: r.owner_pid,
                            layer: r.layer,
                            alpha: r.alpha,
                            x: r.rx,
                            y: r.ry,
                            w: r.rw,
                            h: r.rh,
                        })
                        .collect(),
                ),
                None => Some(Self::lean_stack()?),
            }
        }

        fn is_trusted(&self) -> bool {
            unsafe { AXIsProcessTrusted() }
        }
    }

    impl SystemSessionTapBackend {
        /// Direct lean CG enumeration, used only before the window registry
        /// global is set.
        fn lean_stack() -> Option<Vec<StackWindow>> {
            Some(
                crate::platform::cg::onscreen_windows_lean()?
                    .into_iter()
                    .map(|entry| StackWindow {
                        window_id: entry.number,
                        owner_pid: i32::try_from(entry.owner_pid).unwrap_or(-1),
                        layer: entry.layer,
                        alpha: entry.alpha,
                        x: entry.x,
                        y: entry.y,
                        w: entry.w,
                        h: entry.h,
                    })
                    .collect(),
            )
        }
    }

    fn point_within(a: super::GlobalPoint, b: super::GlobalPoint, tolerance: f64) -> bool {
        point_distance(a, b) <= tolerance
    }

    /// Take the host's cursor over for `window_id`, remembering where it was
    /// so the takeover can be undone. Idempotent within a gesture: a second
    /// call while a takeover is live does NOT overwrite the saved position
    /// (that would save our own injected coordinate and make the restore a
    /// no-op).
    fn begin_cursor_takeover(
        window_id: u32,
        point: super::GlobalPoint,
        tap: &dyn SessionTapBackend,
    ) {
        let mut takeovers = cursor_takeovers().lock_unpoisoned();
        let entry = takeovers
            .entry(window_id)
            .or_insert_with(|| CursorTakeover {
                saved: tap.cursor_position().unwrap_or(super::GlobalPoint {
                    x: point.x,
                    y: point.y,
                }),
                last_posted: point,
                restore_after: None,
            });
        entry.last_posted = point;
    }

    fn note_cursor_posted(window_id: u32, point: super::GlobalPoint, settle: Option<Duration>) {
        let mut takeovers = cursor_takeovers().lock_unpoisoned();
        if let Some(entry) = takeovers.get_mut(&window_id) {
            entry.last_posted = point;
            entry.restore_after = settle.map(|delay| Instant::now() + delay);
        }
    }

    /// The host-presence policy, stated once so it is reviewable:
    ///
    /// We restore the cursor ONLY if it is still within
    /// `SESSION_TAP_CURSOR_TOLERANCE_POINTS` of the last coordinate we
    /// ourselves posted. If it has moved further, a human physically moved
    /// the mouse during our gesture and warping it back would yank the
    /// pointer out from under them mid-motion -- strictly worse than leaving
    /// it where they put it. In that case we abandon the restore and drop the
    /// takeover. This is preferred over a timer on
    /// `CGEventSourceSecondsSinceLastEventType` because our own injected
    /// events refresh that counter, so it cannot distinguish us from the host.
    fn restore_is_safe(entry: &CursorTakeover, tap: &dyn SessionTapBackend) -> bool {
        match tap.cursor_position() {
            Some(current) => point_within(
                current,
                entry.last_posted,
                SESSION_TAP_CURSOR_TOLERANCE_POINTS,
            ),
            // Cannot read the cursor: do not guess, do not warp.
            None => false,
        }
    }

    /// End the takeover for `window_id` and put the host's cursor back.
    /// Called once per GESTURE, never per event -- restoring between a
    /// mouse-down and its drag moves would break the drag.
    fn end_cursor_takeover(window_id: u32, tap: &dyn SessionTapBackend) {
        let entry = { cursor_takeovers().lock_unpoisoned().remove(&window_id) };
        let Some(entry) = entry else { return };
        if !restore_is_safe(&entry, tap) {
            log::info!(
                "remote-control: session-tap cursor restore skipped window_id={window_id} reason=host-moved-cursor"
            );
            return;
        }
        if let Err(error) = tap.move_cursor(entry.saved) {
            log::warn!(
                "remote-control: session-tap cursor restore failed window_id={window_id}: {error}"
            );
        }
    }

    /// Wheel has no Up event to hang the restore off, so the restore is
    /// deferred and swept by this watchdog once the stream settles.
    fn ensure_cursor_restore_watchdog() {
        CURSOR_RESTORE_WATCHDOG.get_or_init(|| {
            thread::Builder::new()
                .name("petal-rc-cursor-restore".to_string())
                .spawn(|| {
                    let tap = SystemSessionTapBackend;
                    loop {
                        thread::sleep(Duration::from_millis(100));
                        let now = Instant::now();
                        let due: Vec<u32> = {
                            let takeovers = cursor_takeovers().lock_unpoisoned();
                            takeovers
                                .iter()
                                .filter(|(_, entry)| {
                                    entry.restore_after.is_some_and(|at| at <= now)
                                })
                                .map(|(window_id, _)| *window_id)
                                .collect()
                        };
                        for window_id in due {
                            end_cursor_takeover(window_id, &tap);
                        }
                    }
                })
                .ok();
        });
    }

    /// How many times the pre-post hit test re-reads the window stack before
    /// concluding the target is unreachable, and how long it waits between
    /// reads. A raise settles asynchronously in WindowServer, so the first
    /// snapshot after `raise` can still show the old order. The whole budget
    /// (2 sleeps) stays far inside `REPLAY_EVENT_DEADLINE`.
    const HIT_TEST_ATTEMPTS: usize = 3;
    const HIT_TEST_SETTLE: Duration = Duration::from_millis(20);

    /// Returns `Some(verdict)` only when the stack says, consistently across
    /// [`HIT_TEST_ATTEMPTS`] reads, that the target cannot be reached at
    /// `point`. `None` means reachable OR unknown -- both of which must be
    /// allowed to proceed, since "unknown" is not evidence of failure.
    fn blocking_hit_test_verdict(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        tap: &dyn SessionTapBackend,
    ) -> Option<HitTestVerdict> {
        let self_pid = std::process::id() as i32;
        let mut last = HitTestVerdict::Unknown;
        for attempt in 0..HIT_TEST_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(HIT_TEST_SETTLE);
            }
            let stack = tap.onscreen_stack();
            last = hit_test_target(stack.as_deref(), window_id, pid, point, self_pid);
            match last {
                HitTestVerdict::NothingInFront | HitTestVerdict::Unknown => return None,
                HitTestVerdict::CoveredBy { .. }
                | HitTestVerdict::TargetNotOnScreen
                | HitTestVerdict::TargetNotAtPoint => {}
            }
        }
        Some(last)
    }

    /// Prepare the target for a session-tap gesture: make it the frontmost,
    /// hit-testable window and warp the cursor into it. Both are required --
    /// the measured runs delivered zero events without them.
    fn prepare_session_tap_target(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        tap: &dyn SessionTapBackend,
    ) -> Result<(), String> {
        if !tap.is_trusted() {
            SESSION_TAP_UNTRUSTED_LOGGED.get_or_init(|| {
                log::warn!(
                    "remote-control: session-tap route unavailable -- Accessibility not granted"
                )
            });
            return Err("session-tap route requires Accessibility".to_string());
        }
        // Raise and warp only when OPENING a takeover. A wheel stream calls
        // this once per event (30+ in under half a second); re-raising the
        // target on every one would burn AX round-trips and fight the window
        // stack for the whole stream.
        if cursor_takeovers()
            .lock_unpoisoned()
            .contains_key(&window_id)
        {
            return Ok(());
        }
        // #599: the raise is a PRECONDITION, not a courtesy. Delivery on this
        // route is geometry-hit-tested, so if the target could not be brought
        // to the front nothing posted here can reach it. Discarding this
        // boolean made the tier log `outcome=Handled` -- and the controller
        // record `outcome=applied` -- for input the target provably never
        // received. Fail instead, so the message takes the normal failure
        // path and the controller gets a real nack.
        if !tap.raise(pid, window_id) {
            log::warn!(
                "remote-control: session-tap route could not raise target window_id={window_id} pid={pid} -- refusing to report delivery (#599)"
            );
            return Err("session-tap route could not raise the target window".to_string());
        }
        // #599 part 2: a raise that RETURNS TRUE still does not mean the
        // target is reachable. Under a `.floating` occluder AXRaise genuinely
        // succeeds -- the window really was raised, it just is not the topmost
        // thing at this coordinate, because a floating panel sits above
        // anything a normal window can be lifted to. That is the case the
        // raise boolean cannot see by construction, and it is why the tier
        // still reported `outcome=applied` for zero delivered events.
        //
        // So check the precondition the raise was standing in for: is anything
        // another process owns in front of the target at this point? Delivery
        // is geometry-hit-tested, so if something is, the post cannot land.
        //
        // Cost: this runs ONCE PER GESTURE, not per event -- the takeover
        // early-return above means a 30-event wheel stream takes one snapshot
        // and none after. That is deliberate: `session/share.rs`'s
        // `visible_on_screen` / `known_closed` exist precisely so remote
        // control never calls `CGWindowListCopyWindowInfo` per event, and this
        // check honours that invariant rather than re-deriving it.
        //
        // This is NOT delivery confirmation and must never be logged as such
        // -- `CGEventPost` gives no post-hoc signal at all. It converts a
        // silent false success into a narrow race: the stack can change
        // between this snapshot and the post below. A raise also settles
        // asynchronously in WindowServer, so re-read the stack a few times
        // before concluding the target is buried, rather than nacking input
        // that would have landed a frame later.
        if let Some(verdict) = blocking_hit_test_verdict(window_id, pid, point, tap) {
            let detail = match verdict {
                HitTestVerdict::CoveredBy {
                    owner_pid,
                    window_id: front_id,
                } => format!("covered_by_pid={owner_pid} covered_by_window={front_id}"),
                // #759: distinct from "not on screen" -- the window is there,
                // the point is not in it. Same refusal, different diagnosis,
                // and the controller-facing text has to say which.
                HitTestVerdict::TargetNotAtPoint => "target_not_at_point".to_string(),
                _ => "target_not_on_screen".to_string(),
            };
            log::warn!(
                "remote-control: session-tap pre-post hit test found the target unreachable \
                 at ({:.0},{:.0}) window_id={window_id} pid={pid} {detail} -- refusing to \
                 report delivery (#599)",
                point.x,
                point.y
            );
            return Err(format!(
                "session-tap route: target is not the frontmost window at the target point ({detail})"
            ));
        }
        begin_cursor_takeover(window_id, point, tap);
        // Warp INTO the shared window first so the visible jump is contained
        // to that window instead of flying across the desktop.
        tap.move_cursor(point)
    }

    /// #446: the three direct (SkyLight) pointer routes stay OPT-IN.
    ///
    /// Measured live 2026-07-27, macOS 26.5.2 (25F84) arm64, web->native, real
    /// AppKit target that logs every `NSWindow.sendEvent:` it receives: with
    /// `PETAL_REMOTE_CONTROL_DIRECT_{CLICK,DRAG,SCROLL}=1` the host logged
    /// `route=direct` / `mode=SlDrag outcome=Handled` and the target app
    /// received ZERO mouse NSEvents -- for the v2 semantic click, the legacy
    /// raw Down/Up click, the legacy drag, and the wheel alike. Keyboard and
    /// Cmd+V in the same run DID land, so the loop itself was healthy.
    /// `SLEventPostToPid` is declared here as `-> ()` and its result is never
    /// inspected, so `route=direct` records "posted", never "delivered".
    ///
    /// Turning these on therefore does not fix #446; it only suppresses the
    /// honest `pointer or wheel injection exhausted AX/SkyLight routes`
    /// failure the default path still reports. Do not default them on without
    /// NEW evidence of an actual delivered NSEvent in a target app.
    fn direct_route_enabled(var: &str) -> bool {
        std::env::var(var).as_deref() == Ok("1")
    }

    fn direct_scroll_enabled() -> bool {
        direct_route_enabled("PETAL_REMOTE_CONTROL_DIRECT_SCROLL")
    }

    fn direct_drag_enabled() -> bool {
        // This route owns the whole gesture (Down/Move/Up), including the
        // synthetic Up retained through revoke/drain; it is not a delivery ACK.
        direct_route_enabled("PETAL_REMOTE_CONTROL_DIRECT_DRAG")
    }

    fn direct_click_enabled() -> bool {
        direct_route_enabled("PETAL_REMOTE_CONTROL_DIRECT_CLICK")
    }

    fn sl_event_post_to_pid() -> Option<SlEventPostToPidFn> {
        *SL_EVENT_POST_TO_PID.get_or_init(|| unsafe {
            let path = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
            let handle = dlopen(path.as_ptr().cast::<c_char>(), RTLD_LAZY | RTLD_LOCAL);
            if handle.is_null() {
                SL_FAILURE_LOGGED.get_or_init(|| {
                    log::warn!("remote-control: SkyLight unavailable for SLEventPostToPid fallback")
                });
                return None;
            }
            let symbol = dlsym(handle, b"SLEventPostToPid\0".as_ptr().cast::<c_char>());
            if symbol.is_null() {
                SL_FAILURE_LOGGED.get_or_init(|| {
                    log::warn!("remote-control: SLEventPostToPid symbol unavailable")
                });
                return None;
            }
            Some(std::mem::transmute::<*mut c_void, SlEventPostToPidFn>(
                symbol,
            ))
        })
    }

    fn post_sl_mouse_click(
        post_to_pid: SlEventPostToPidFn,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_state: u32,
    ) -> Result<(), SlClickError> {
        // #373: Middle now synthesizes a real other-mouse down/up pair
        // instead of erroring Unavailable -- SLEventPostToPid delivers these
        // the same way it does Left/Right (unlike raw CGEventPostToPid, which
        // has no real pointer effect; see the crash-class note in CLAUDE.md).
        let (down, up) = match button {
            RemoteControlButton::Left => (MouseKind::LeftDown, MouseKind::LeftUp),
            RemoteControlButton::Right => (MouseKind::RightDown, MouseKind::RightUp),
            // #369: middle click previously had no SkyLight route and fell
            // through to CGEventPostToPid, which has no real effect for
            // pointer buttons (silent no-op) -- route it the same as
            // Left/Right via SLEventPostToPid's "other mouse" event kind.
            RemoteControlButton::Middle => (MouseKind::OtherDown, MouseKind::OtherUp),
        };
        post_sl_mouse_event(post_to_pid, pid, point, button, down, click_state)?;
        post_sl_mouse_event(post_to_pid, pid, point, button, up, click_state)
    }

    fn post_sl_mouse_event(
        post_to_pid: SlEventPostToPidFn,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        kind: MouseKind,
        click_state: u32,
    ) -> Result<(), SlClickError> {
        unsafe {
            let event = CGEventCreateMouseEvent(
                std::ptr::null(),
                mouse_kind_code(kind),
                CGPoint {
                    x: point.x,
                    y: point.y,
                },
                button_number(button),
            );
            if event.is_null() {
                return Err(SlClickError::Failed(
                    "CGEventCreateMouseEvent returned null for SLEventPostToPid".to_string(),
                ));
            }
            CGEventSetIntegerValueField(
                event,
                K_CG_MOUSE_EVENT_BUTTON_NUMBER,
                button_number(button) as i64,
            );
            CGEventSetIntegerValueField(
                event,
                K_CG_MOUSE_EVENT_CLICK_STATE,
                i64::from(click_state.max(1)),
            );
            post_to_pid(pid, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn post_sl_scroll(
        post_to_pid: SlEventPostToPidFn,
        pid: i32,
        point: super::GlobalPoint,
        delta_y: i32,
        delta_x: i32,
        flags: u64,
    ) -> Result<(), SlClickError> {
        unsafe {
            let event = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                K_CG_SCROLL_EVENT_UNIT_PIXEL,
                1,
                -delta_y,
            );
            if event.is_null() {
                return Err(SlClickError::Failed(
                    "CGEventCreateScrollWheelEvent returned null for SLEventPostToPid".to_string(),
                ));
            }
            CGEventSetIntegerValueField(
                event,
                K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                i64::from(-delta_x),
            );
            CGEventSetFlags(event, flags);
            CGEventSetLocation(
                event,
                CGPoint {
                    x: point.x,
                    y: point.y,
                },
            );
            post_to_pid(pid, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn post_sl_click_with_priming(
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_state: u32,
        backend: &dyn SlClickBackend,
    ) -> SlClickOutcome {
        {
            let primed = sl_primed_pids().lock_unpoisoned();
            if !primed.contains(&pid) {
                drop(primed);
                // The primer is a health probe, not a real click -- always a
                // plain single click_state regardless of the real click's
                // multi-click count.
                match backend.post_click(pid, super::GlobalPoint { x: -1.0, y: -1.0 }, button, 1) {
                    Ok(()) => {
                        sl_primed_pids().lock_unpoisoned().insert(pid);
                    }
                    Err(SlClickError::Unavailable) => return SlClickOutcome::PassThrough,
                    Err(SlClickError::Failed(error)) => {
                        SL_FAILURE_LOGGED.get_or_init(|| {
                            log::warn!(
                                "remote-control: SLEventPostToPid primer click failed: {error}"
                            )
                        });
                        return SlClickOutcome::PassThrough;
                    }
                }
            }
        }
        match backend.post_click(pid, point, button, click_state) {
            Ok(()) => SlClickOutcome::Posted,
            Err(SlClickError::Unavailable) => SlClickOutcome::PassThrough,
            Err(SlClickError::Failed(error)) => {
                SL_FAILURE_LOGGED.get_or_init(|| {
                    log::warn!("remote-control: SLEventPostToPid click failed: {error}")
                });
                SlClickOutcome::PassThrough
            }
        }
    }

    pub fn replay(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
    ) -> Result<(), String> {
        let sink = CGEventSink {
            target_pid,
            window_id: message.window_id,
        };
        let ax = SystemAxBackend;
        let sl = SystemSlClickBackend;
        let pb = SystemPasteboardBackend;
        let tap = SystemSessionTapBackend;
        ensure_cursor_restore_watchdog();
        replay_with_backends(message, frame, target_pid, &sink, &ax, &sl, &pb, &tap)
    }

    #[allow(clippy::too_many_arguments)]
    fn replay_with_backends(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
        sink: &dyn InputSink,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        pb: &dyn PasteboardBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<(), String> {
        let probe_before = ax_probe_snapshot();
        let ax_result = match message.message_type {
            RemoteControlType::Pointer => {
                replay_pointer_via_ax(message, frame, target_pid, ax, sl, tap)
            }
            RemoteControlType::Wheel => {
                replay_wheel_via_ax(message, frame, target_pid, ax, sl, tap)
            }
            // #170: intercept clipboard/select-all key equivalents and drive them
            // through AX directly. The CGEvent Cmd+A/C/V never matches the app's
            // menu key-equivalent when the app is backgrounded (case 27 keeps the
            // controller frontmost), so they were silent no-ops.
            RemoteControlType::Key => replay_key_via_ax(message, target_pid, ax, pb),
            _ => Ok(AxReplayOutcome::PassThrough),
        };
        let probe_after = ax_probe_snapshot();
        if super::should_log_latency_probe(message) {
            log::info!(
                "remote-control-latency: ax probes {} ax_ipc={} cache_hit={} cache_miss={}",
                super::message_summary(message),
                probe_after.ax_ipc.saturating_sub(probe_before.ax_ipc),
                probe_after
                    .cache_hits
                    .saturating_sub(probe_before.cache_hits),
                probe_after
                    .cache_misses
                    .saturating_sub(probe_before.cache_misses),
            );
        }
        let ax_outcome = ax_result?;
        if message.message_type == RemoteControlType::Wheel
            && ax_outcome == AxReplayOutcome::PassThrough
            // Wheel is not classified as a move by should_log_message, so
            // throttle this high-rate stream explicitly.
            && message.seq % 120 == 0
        {
            log::info!(
                "remote-control: wheel route=PassThrough reason=ax-and-skylight-unavailable-or-disabled window_id={} controller='{}'",
                message.window_id,
                message.controller_id
            );
        }
        // Fable-review fix (#369), second pass: `injection_was_cancelled()`
        // is only ever true when THIS thread's own `run_replay_with_deadline`
        // wrapper already gave up on it -- any code still running past that
        // point is, by construction, part of an abandoned injection. The
        // gesture-map checks earlier in this call chain stop an orphan MAP
        // entry, but a cancelled AX outcome of `PassThrough` would otherwise
        // still fall through to the CGEvent/SkyLight sink fallback below and
        // post a real, seconds-late side effect to the target app -- e.g. an
        // abandoned pressable Down posting a stale mouse-down to the pid
        // after its matching Up (finding no gesture) already posted the up,
        // leaving a phantom held button. Bail out here, before ANY sink
        // dispatch, for every message type uniformly.
        if super::injection_was_cancelled() {
            return Ok(());
        }
        let result = match ax_outcome {
            AxReplayOutcome::Handled => Ok(()),
            AxReplayOutcome::PassThrough
                if message.message_type == RemoteControlType::Pointer
                    && message.action == Some(RemoteControlAction::Click) =>
            {
                replay_semantic_click_to_sink(message, frame, sink)
            }
            AxReplayOutcome::PassThrough
                if message.message_type == RemoteControlType::Pointer
                    && message.action == Some(RemoteControlAction::Down) =>
            {
                // #446 follow-up: a PassThrough Down always records a gesture
                // (see pass_through_pointer_down's unconditional insert) that
                // the matching Up will actually attempt to deliver via
                // sl_click_or_passthrough. Hard-failing as soon as the Down
                // resolves to PassThrough was premature -- it nacked every
                // click attempt with "Unavailable" even when the Up went on
                // to succeed. Defer the real success/failure signal to Up.
                Ok(())
            }
            AxReplayOutcome::PassThrough
                if message.message_type == RemoteControlType::Wheel
                    || (message.message_type == RemoteControlType::Pointer
                        && (message.action != Some(RemoteControlAction::Move)
                            || message.buttons.unwrap_or(0) != 0)) =>
            {
                Err("pointer or wheel injection exhausted AX/SkyLight routes".to_string())
            }
            AxReplayOutcome::PassThrough
                if message.message_type == RemoteControlType::Pointer
                    && hover_blocked_by_window_stack(message, frame, target_pid, tap) =>
            {
                // Dropped, not nacked: a hover is best-effort and a nack per
                // refused move on a 30Hz stream would be pure noise.
                Ok(())
            }
            AxReplayOutcome::PassThrough => replay_to_sink(message, frame, sink),
        };
        // #368 F1: any message that can mutate the UI under the cursor must drop
        // this window's cached element resolutions — whether it was serviced via
        // AX or the synthetic CGEvent/SkyLight fallback (a passthrough wheel
        // still scrolls; a passthrough click still clicks). This is the single
        // authoritative invalidation point so a follow-up event within the TTL
        // re-resolves against the changed UI instead of pressing an element that
        // moved or was replaced. The scroll-target cache is preserved here (it is
        // invalidated only on frame change), keeping the wheel-stream latency win.
        if message_mutates_ui(message) {
            invalidate_ax_resolution_after_mutation(message.window_id);
        }
        result
    }

    /// Same-PID window scoping: the buttonless hover Move is the only
    /// pointer message that still reaches the legacy `CGEventPostToPid` sink
    /// (`replay_pointer_via_ax` keeps it PassThrough so it never hijacks the
    /// host cursor, #446). That sink is PID-scoped: the pid's AppKit resolves
    /// a posted mouseMoved to whichever of ITS OWN windows is at the point, so
    /// an unshared same-app sibling overlapping the shared window receives
    /// the hover -- in UI the controller cannot see. Apply the same stack
    /// verdict the session-tap route already applies to clicks (#759), using
    /// the CHEAP cached stack (this runs per event on a 30Hz+ stream).
    ///
    /// `true` = refuse. `Unknown` (no stack) and `NothingInFront` deliver;
    /// only a message with a real pid and coordinates is ever gated.
    fn hover_blocked_by_window_stack(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
        tap: &dyn SessionTapBackend,
    ) -> bool {
        if message.action != Some(RemoteControlAction::Move) || message.buttons.unwrap_or(0) != 0 {
            return false;
        }
        let Some(pid) = target_pid.filter(|pid| *pid > 0) else {
            return false;
        };
        let (Some(x), Some(y)) = (message.x, message.y) else {
            return false;
        };
        let point = normalized_to_global(frame, x, y);
        let stack = tap.cached_onscreen_stack();
        let self_pid = std::process::id() as i32;
        match hit_test_target(stack.as_deref(), message.window_id, pid, point, self_pid) {
            HitTestVerdict::NothingInFront | HitTestVerdict::Unknown => false,
            verdict => {
                if message.seq % 120 == 0 {
                    log::debug!(
                        "remote-control: hover dropped window_id={} pid={pid} verdict={verdict:?} -- sink route is pid-scoped and the point is not on the authorized window",
                        message.window_id
                    );
                }
                true
            }
        }
    }

    /// #368 F1: does replaying this message potentially change the UI under the
    /// cursor (so cached element resolutions must be dropped)? Buttonless hover
    /// moves and non-input control/status messages do not.
    fn message_mutates_ui(message: &RemoteControlMessage) -> bool {
        match message.message_type {
            RemoteControlType::Pointer => match message.action {
                Some(RemoteControlAction::Down)
                | Some(RemoteControlAction::Up)
                | Some(RemoteControlAction::Click) => true,
                // A held-button move is a drag (selection/drag mutation); a
                // buttonless move is a hover and changes nothing.
                _ => message.buttons.unwrap_or(0) != 0,
            },
            RemoteControlType::Wheel => true,
            RemoteControlType::Key => message.action == Some(RemoteControlAction::Down),
            RemoteControlType::Text => true,
            RemoteControlType::Request
            | RemoteControlType::Release
            | RemoteControlType::Status
            | RemoteControlType::Result
            | RemoteControlType::Unknown => false,
        }
    }

    fn replay_semantic_click_to_sink(
        _message: &RemoteControlMessage,
        _frame: WindowFrame,
        _sink: &dyn InputSink,
    ) -> Result<(), String> {
        // A v2 semantic click must not degrade into the known-ineffective
        // CGEventPostToPid pointer path. AX/SL exhaustion is a real failure.
        Err("semantic pointer click exhausted AX/SkyLight routes".to_string())
    }

    fn replay_to_sink(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        sink: &dyn InputSink,
    ) -> Result<(), String> {
        match message.message_type {
            RemoteControlType::Pointer => replay_pointer(message, frame, sink),
            RemoteControlType::Wheel => replay_wheel(message, frame, sink),
            RemoteControlType::Key => replay_key(message, sink),
            RemoteControlType::Text => replay_text(message, sink),
            RemoteControlType::Request
            | RemoteControlType::Release
            | RemoteControlType::Status
            | RemoteControlType::Result
            | RemoteControlType::Unknown => Ok(()),
        }
    }

    fn replay_pointer_via_ax(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        let Some(pid) = target_pid.filter(|pid| *pid > 0) else {
            return Ok(AxReplayOutcome::PassThrough);
        };
        let x = message
            .x
            .ok_or_else(|| "pointer message missing x".to_string())?;
        let y = message
            .y
            .ok_or_else(|| "pointer message missing y".to_string())?;
        let point = normalized_to_global(frame, x, y);
        let action = message
            .action
            .ok_or_else(|| "pointer message missing action".to_string())?;
        let button = button_from_wire(message.button);
        let click_count = click_state_from_count(message.click_count);

        match action {
            RemoteControlAction::Down => ax_pointer_down(
                message.window_id,
                &message.controller_id,
                pid,
                point,
                button,
                click_count,
                ax,
                sl,
                tap,
            ),
            RemoteControlAction::Move if message.buttons.unwrap_or(0) != 0 => ax_pointer_drag_move(
                message.window_id,
                &message.controller_id,
                pid,
                point,
                sl,
                tap,
            ),
            RemoteControlAction::Up => ax_pointer_up(
                message.window_id,
                &message.controller_id,
                pid,
                point,
                button,
                click_count,
                ax,
                sl,
                tap,
            ),
            // Hover moves stay PassThrough by design: a buttonless move must
            // never hijack the host's cursor, which is exactly what a
            // session-tap post would do (#446 -- the cursor always moves).
            RemoteControlAction::Move => Ok(AxReplayOutcome::PassThrough),
            RemoteControlAction::Click => replay_semantic_click(
                message.window_id,
                pid,
                point,
                button,
                click_count,
                ax,
                sl,
                tap,
            ),
            RemoteControlAction::Unknown => Ok(AxReplayOutcome::PassThrough),
        }
    }

    /// Route a complete click without changing the semantics of a drag or text
    /// selection. The direct SkyLight path is opt-in until it has passed a live
    /// exactly-once matrix: SLEventPostToPid has no delivery acknowledgement,
    /// so an automatic AX retry after a posted event could double-activate a
    /// destructive control. Unknown/unsupported targets remain AX-authoritative.
    ///
    /// #369 (sub-item 3, "decide and default the SkyLight direct path") left
    /// this opt-in pending a live pass. #446 ran that pass on 2026-07-27
    /// (macOS 26.5.2 arm64, web->native, instrumented AppKit target): with
    /// `PETAL_REMOTE_CONTROL_DIRECT_CLICK=1` this branch logged `route=direct`
    /// and the target received no `leftMouseDown`/`leftMouseUp` at all. The
    /// default therefore stays OFF -- see `direct_route_enabled` for the full
    /// measurement. What #369 *did* land: middle click now has a real
    /// SkyLight route (see `post_sl_mouse_click`) instead of silently
    /// no-opping through the ineffective `CGEventPostToPid` pointer path.
    /// A v2 semantic click is a WHOLE gesture in one message, so unlike the
    /// legacy Down/Up pair it can safely fall back to the session tap here
    /// without ever mixing routes mid-gesture.
    #[allow(clippy::too_many_arguments)]
    fn replay_semantic_click(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        let outcome =
            replay_semantic_click_ax(window_id, pid, point, button, click_count, ax, sl, tap)?;
        if outcome != AxReplayOutcome::PassThrough {
            return Ok(outcome);
        }
        session_tap_semantic_click(window_id, pid, point, button, click_count, tap)
    }

    /// #446: SkyLight must not claim a semantic click ahead of the session
    /// tap. `SLEventPostToPid` is fire-and-forget with no delivery ack and was
    /// measured to deliver ZERO mouse NSEvents to a real target -- yet
    /// `sl_click_or_passthrough` reports `Handled` whenever the post merely
    /// succeeded. On AX-hostile (custom-drawn) content that verdict swallowed
    /// every click before `replay_semantic_click` could fall through to the
    /// only route measured to actually deliver. That is why `action=Click` --
    /// what real browser controllers send -- landed nothing while the legacy
    /// Down/Up pair at the same coordinate worked: Down/Up never consults
    /// SkyLight, it goes straight to the session tap.
    ///
    /// The route stays reachable behind its existing opt-in, so a run with
    /// `PETAL_REMOTE_CONTROL_DIRECT_CLICK=1` is unchanged.
    /// `direct_enabled` is passed in rather than read from the environment
    /// here so both branches are testable without mutating process-global
    /// state under a parallel test harness.
    fn semantic_click_sl_or_passthrough(
        direct_enabled: bool,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        displacement: f64,
        click_count: u32,
        sl: &dyn SlClickBackend,
    ) -> AxReplayOutcome {
        if !direct_enabled {
            return AxReplayOutcome::PassThrough;
        }
        sl_click_or_passthrough(pid, point, button, displacement, click_count, sl)
    }

    /// Post a complete click through the session tap and hand the cursor
    /// back, all within this one call -- a semantic click has no separate Up
    /// message to close the takeover.
    fn session_tap_semantic_click(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        if let Err(error) = prepare_session_tap_target(window_id, pid, point, tap) {
            log::info!(
                "remote-control: session-tap semantic click unavailable window_id={window_id}: {error}"
            );
            return Ok(AxReplayOutcome::PassThrough);
        }
        let (down, up) = match button {
            RemoteControlButton::Left => (MouseKind::LeftDown, MouseKind::LeftUp),
            RemoteControlButton::Right => (MouseKind::RightDown, MouseKind::RightUp),
            RemoteControlButton::Middle => (MouseKind::OtherDown, MouseKind::OtherUp),
        };
        let result = tap
            .post_mouse(point, button, down, click_count)
            .and_then(|()| tap.post_mouse(point, button, up, click_count));
        note_cursor_posted(window_id, point, None);
        end_cursor_takeover(window_id, tap);
        // #446: this route had NO decision line at all, so a semantic click
        // was invisible in petal.log while Down/Up logged theirs -- the
        // absence was the first clue that the click never got here.
        log::info!(
            "remote-control: semantic click window_id={window_id} button={button:?} click_count={click_count} mode=SessionTap outcome={}",
            if result.is_ok() { "Handled" } else { "Error" }
        );
        result.map(|()| AxReplayOutcome::Handled)
    }

    #[allow(clippy::too_many_arguments)]
    fn replay_semantic_click_ax(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        replay_semantic_click_ax_with_direct(
            window_id,
            pid,
            point,
            button,
            click_count,
            direct_click_enabled(),
            ax,
            sl,
            tap,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn replay_semantic_click_ax_with_direct(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        direct: bool,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        _tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        let mut first_resolution = Some(resolve_at_point(window_id, pid, point, ax, true));
        if button == RemoteControlButton::Left && direct {
            match first_resolution
                .as_ref()
                .expect("initial resolution is present")
            {
                Err(error) if error.is_window_identity_unavailable() => {
                    return Err(window_identity_unavailable_error(window_id))
                }
                Err(error) if error.is_window_id_mismatch() => {
                    return Err(window_identity_error(window_id))
                }
                _ => {}
            }
            // Deliberately do not use the legacy (-1,-1) primer here: it is a
            // real click with side effects, not a health probe.
            match sl.post_click(pid, point, button, click_count) {
                Ok(()) => {
                    log::info!(
                        "remote-control: semantic click route=direct pid={pid} point=({:.1},{:.1})",
                        point.x,
                        point.y
                    );
                    return Ok(AxReplayOutcome::Handled);
                }
                // #446: the other SkyLight-unavailability sites already log at
                // `warn` behind `SL_FAILURE_LOGGED`; this one was still at
                // `debug`, i.e. below the file sink's default `info` level, so
                // an opted-in run whose SL route silently fell back to AX left
                // no trace in a normal petal.log. Once-only, not per-event.
                Err(error) => {
                    SL_FAILURE_LOGGED.get_or_init(|| {
                        log::warn!(
                            "remote-control: semantic direct click unavailable; keeping AX authority: {error:?}"
                        )
                    });
                }
            }
        }

        // #368 F2: attempt 0 may serve a cached element; if that element is
        // stale at action time we re-resolve fresh once (attempt 1) rather than
        // swallow the click. A press/show-menu/caret that did nothing cannot
        // have double-activated, so the retry is safe.
        for attempt in 0..2 {
            let use_cache = attempt == 0;
            let resolution = if attempt == 0 {
                first_resolution
                    .take()
                    .expect("initial resolution is consumed once")
            } else {
                resolve_at_point(window_id, pid, point, ax, use_cache)
            };
            let (element, caps) = match resolution {
                Ok(Some(resolved)) => resolved,
                Ok(None) => {
                    return Ok(semantic_click_sl_or_passthrough(
                        direct,
                        pid,
                        point,
                        button,
                        0.0,
                        click_count,
                        sl,
                    ))
                }
                Err(error) if error.is_window_id_mismatch() => {
                    return Err(window_identity_error(window_id))
                }
                Err(error) if error.is_window_identity_unavailable() => {
                    return Err(window_identity_unavailable_error(window_id))
                }
                Err(error) if error.is_api_disabled() => return Err(accessibility_revoked_error()),
                Err(error) if error.is_invalid_ui_element() || error.is_capability_miss() => {
                    return Ok(semantic_click_sl_or_passthrough(
                        direct,
                        pid,
                        point,
                        button,
                        0.0,
                        click_count,
                        sl,
                    ));
                }
                Err(error) => {
                    log::warn!("remote-control: semantic click hit-test failed: {error:?}");
                    return Ok(semantic_click_sl_or_passthrough(
                        direct,
                        pid,
                        point,
                        button,
                        0.0,
                        click_count,
                        sl,
                    ));
                }
            };
            // #373: a double/triple click on a text view means "select the
            // word/paragraph", which our own offset+set_selected_range only
            // approximates as a caret placement. Let the target app's own
            // mouseDown handler do the real multi-click selection by falling
            // through to a click_state-tagged SL/CGEvent click instead.
            if caps.text_selectable && !caps.pressable && click_count >= 2 {
                return Ok(semantic_click_sl_or_passthrough(
                    direct,
                    pid,
                    point,
                    button,
                    0.0,
                    click_count,
                    sl,
                ));
            }
            let action = if button == RemoteControlButton::Right {
                if caps.show_menu {
                    ax.show_menu(&element)
                } else {
                    return Ok(semantic_click_sl_or_passthrough(
                        direct,
                        pid,
                        point,
                        button,
                        0.0,
                        click_count,
                        sl,
                    ));
                }
            } else if caps.pressable {
                ax.press(&element)
            } else if caps.text_selectable {
                match ax.offset_at_point(&element, point) {
                    Ok(offset) => ax.set_selected_range(&element, offset, 0),
                    Err(error) => {
                        if attempt == 0 && error.is_invalid_ui_element() {
                            clear_ax_resolution_cache_key(ax_point_key(window_id, point));
                            continue;
                        }
                        return Ok(ax_error_outcome(error, "semantic text click offset")?);
                    }
                }
            } else {
                return Ok(semantic_click_sl_or_passthrough(
                    direct,
                    pid,
                    point,
                    button,
                    0.0,
                    click_count,
                    sl,
                ));
            };
            match action {
                // #368 F1 invalidation is applied centrally in
                // replay_with_backends after the whole message is replayed.
                //
                // #820 (case 29 investigation): every OTHER outcome of a
                // semantic click logs something (session_tap_semantic_click
                // always does; the direct-SkyLight branch above does; every
                // AX error path does via ax_action_outcome/ax_error_outcome).
                // This was the one silent success path in the whole
                // function -- a click serviced by AX (press / show-menu /
                // text-selection) left NO trace in petal.log, which is what
                // made a genuinely-delivered AX click indistinguishable from
                // "the session tap didn't fire" during that investigation.
                // Log on success too, matching the sibling AX-pointer-down
                // path's unconditional log, so the route actually taken is
                // never in question again.
                Ok(()) => {
                    let route = if button == RemoteControlButton::Right {
                        "show_menu"
                    } else if caps.pressable {
                        "press"
                    } else {
                        "text_selectable"
                    };
                    log::info!(
                        "remote-control: semantic click window_id={window_id} button={button:?} click_count={click_count} mode=Ax route={route} outcome=Handled"
                    );
                    return Ok(AxReplayOutcome::Handled);
                }
                Err(error) if attempt == 0 && error.is_invalid_ui_element() => {
                    clear_ax_resolution_cache_key(ax_point_key(window_id, point));
                    continue;
                }
                Err(error) => return ax_action_outcome(Err(error), "semantic click"),
            }
        }
        Ok(semantic_click_sl_or_passthrough(
            direct,
            pid,
            point,
            button,
            0.0,
            click_count,
            sl,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn ax_pointer_down(
        window_id: u32,
        controller_id: &str,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        if button == RemoteControlButton::Middle {
            match resolve_cached(window_id, pid, point, ax) {
                Err(error) if error.is_window_id_mismatch() => {
                    return Err(window_identity_error(window_id))
                }
                Err(error) if error.is_window_identity_unavailable() => {
                    return Err(window_identity_unavailable_error(window_id))
                }
                _ => {}
            }
            // #369: no AX press action applies to a middle click, so skip
            // AX action selection -- but DO record gesture state (matching the
            // PassThrough case below) so `ax_pointer_up` runs the button
            // release through `sl_click_or_passthrough` instead of finding no
            // stored gesture and passing straight to the ineffective
            // CGEventPostToPid pointer path.
            log::info!(
                "remote-control: AX pointer down window_id={window_id} controller='{controller_id}' button={button:?} mode=PassThrough reason=middle-button"
            );
            // Deadlock fix (#446 self-review): this branch used to lock
            // `ax_pointer_gestures()` itself and insert a PassThrough entry
            // before falling through to `pass_through_pointer_down` below --
            // but that function locks the SAME static mutex again to do its
            // own insert under the SAME (window_id, controller_id) key, which
            // always runs right after and unconditionally overwrites this
            // one. `return pass_through_pointer_down(...)` evaluates that
            // call before dropping this scope's locals, so the still-held
            // guard here caused a real self-deadlock on every middle-click
            // Down (std::sync::Mutex is not reentrant). The insert was fully
            // redundant with pass_through_pointer_down's own -- dropping it
            // removes the deadlock with no behavior change.
            return pass_through_pointer_down(
                window_id,
                controller_id,
                pid,
                point,
                button,
                click_count,
                sl,
                tap,
            );
        }
        let mode = match resolve_cached(window_id, pid, point, ax) {
            Err(error) if error.is_window_id_mismatch() => {
                return Err(window_identity_error(window_id))
            }
            Err(error) if error.is_window_identity_unavailable() => {
                return Err(window_identity_unavailable_error(window_id))
            }
            Err(error) if error.is_api_disabled() => return Err(accessibility_revoked_error()),
            Err(error) if error.is_invalid_ui_element() => {
                log::warn!("remote-control: invalid AX app/element during pointer hit-test");
                GestureMode::PassThrough
            }
            Err(error) if error.is_capability_miss() => {
                log::info!(
                    "remote-control: AX pointer down window_id={window_id} hit-test capability miss: {error:?}"
                );
                GestureMode::PassThrough
            }
            Err(error) => {
                log::warn!("remote-control: AX pointer hit-test failed: {error:?}");
                GestureMode::PassThrough
            }
            Ok(Some((element, caps))) => {
                gesture_mode_for_element(window_id, element, caps, point, button, click_count, ax)?
            }
            Ok(None) => {
                log::info!(
                    "remote-control: AX pointer down window_id={window_id} hit-test resolved no element"
                );
                GestureMode::PassThrough
            }
        };
        log::info!(
            "remote-control: AX pointer down window_id={window_id} controller='{controller_id}' button={button:?} mode={}",
            gesture_mode_summary(&mode)
        );
        // #446 review finding: check cancellation BEFORE posting a real SL
        // Down, not after. Posting first and only skipping the state-insert
        // (the pre-existing check below) would still physically press a
        // button seconds late for an event this waiter already gave up on
        // -- exactly the "don't act on state the target app no longer
        // reflects" hazard the #369 fix (see ax_pointer_up) exists to
        // avoid -- and leaves it un-releasable, since no gesture state gets
        // recorded for the matching Up to find.
        let mode = if matches!(&mode, GestureMode::PassThrough)
            && direct_drag_enabled()
            && !super::injection_was_cancelled()
        {
            match sl.post_mouse_event(pid, point, button, SlMouseEvent::Down) {
                Ok(()) => GestureMode::SlDrag,
                Err(SlClickError::Unavailable) => GestureMode::PassThrough,
                Err(SlClickError::Failed(error)) => {
                    SL_FAILURE_LOGGED.get_or_init(|| {
                        log::warn!("remote-control: SLEventPostToPid drag down failed: {error}")
                    });
                    GestureMode::PassThrough
                }
            }
        } else {
            mode
        };
        // #446: AX found nothing actionable (custom-drawn content), so take
        // the session tap -- the only route measured to actually deliver.
        let mode = if matches!(&mode, GestureMode::PassThrough) && !super::injection_was_cancelled()
        {
            session_tap_pointer_down(window_id, pid, point, button, click_count, tap)
        } else {
            mode
        };
        let outcome = match &mode {
            GestureMode::PassThrough => AxReplayOutcome::PassThrough,
            GestureMode::SlDrag | GestureMode::SessionTap => AxReplayOutcome::Handled,
            _ => AxReplayOutcome::Handled,
        };
        // #374: keyed per (window, controller) so a second concurrent
        // controller starting its own gesture on the same window cannot
        // clobber this one's parked anchor/mode.
        //
        // Fable-review fix (#369): same reasoning as the middle-click branch
        // above -- check-and-insert while holding the lock so a concurrent
        // Up's `remove()` can't interleave between the check and the insert.
        {
            let mut gestures = ax_pointer_gestures().lock_unpoisoned();
            if !super::injection_was_cancelled() {
                gestures.insert(
                    (window_id, controller_id.to_string()),
                    PointerGestureState {
                        mode,
                        down_point: point,
                        last_point: point,
                        button,
                        click_count,
                    },
                );
            }
        }
        Ok(outcome)
    }

    /// Open a session-tap gesture: raise the target, warp into it, post the
    /// Down. Returns `SessionTap` on success so Up/drag stay on this route,
    /// or `PassThrough` if the route is unavailable (no Accessibility).
    fn session_tap_pointer_down(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        tap: &dyn SessionTapBackend,
    ) -> GestureMode {
        if let Err(error) = prepare_session_tap_target(window_id, pid, point, tap) {
            log::info!(
                "remote-control: session-tap pointer down unavailable window_id={window_id}: {error}"
            );
            return GestureMode::PassThrough;
        }
        let kind = match button {
            RemoteControlButton::Left => MouseKind::LeftDown,
            RemoteControlButton::Right => MouseKind::RightDown,
            RemoteControlButton::Middle => MouseKind::OtherDown,
        };
        match tap.post_mouse(point, button, kind, click_count) {
            Ok(()) => {
                note_cursor_posted(window_id, point, None);
                GestureMode::SessionTap
            }
            Err(error) => {
                log::warn!("remote-control: session-tap pointer down failed: {error}");
                // Nothing is held, so drop the takeover rather than leaving a
                // restore pending for a gesture that never started.
                end_cursor_takeover(window_id, tap);
                GestureMode::PassThrough
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pass_through_pointer_down(
        window_id: u32,
        controller_id: &str,
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        // #446 review finding: same reordering as ax_pointer_down -- check
        // cancellation before posting a real SL Down, not after.
        let mode = if direct_drag_enabled() && !super::injection_was_cancelled() {
            match sl.post_mouse_event(pid, point, button, SlMouseEvent::Down) {
                Ok(()) => GestureMode::SlDrag,
                Err(SlClickError::Unavailable) => GestureMode::PassThrough,
                Err(SlClickError::Failed(error)) => {
                    SL_FAILURE_LOGGED.get_or_init(|| {
                        log::warn!("remote-control: SLEventPostToPid drag down failed: {error}")
                    });
                    GestureMode::PassThrough
                }
            }
        } else {
            GestureMode::PassThrough
        };
        let mode = if matches!(&mode, GestureMode::PassThrough) && !super::injection_was_cancelled()
        {
            session_tap_pointer_down(window_id, pid, point, button, click_count, tap)
        } else {
            mode
        };
        let is_sl_drag = matches!(&mode, GestureMode::SlDrag | GestureMode::SessionTap);
        if !super::injection_was_cancelled() {
            ax_pointer_gestures().lock_unpoisoned().insert(
                (window_id, controller_id.to_string()),
                PointerGestureState {
                    mode,
                    down_point: point,
                    last_point: point,
                    button,
                    click_count,
                },
            );
        }
        Ok(if is_sl_drag {
            AxReplayOutcome::Handled
        } else {
            AxReplayOutcome::PassThrough
        })
    }

    fn gesture_mode_for_element(
        window_id: u32,
        element: AxElementHandle,
        caps: AxCapabilities,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        ax: &dyn AxInputBackend,
    ) -> Result<GestureMode, String> {
        if button == RemoteControlButton::Right {
            return Ok(if caps.show_menu {
                GestureMode::AxPressable {
                    element,
                    action: AxClickAction::ShowMenu,
                }
            } else {
                log::info!(
                    "remote-control: AX pointer down window_id={window_id} mode=PassThrough role={} attempted=show_menu failed caps={caps:?}",
                    ax_role_description(&element)
                );
                GestureMode::PassThrough
            });
        }
        if caps.pressable {
            return Ok(GestureMode::AxPressable {
                element,
                action: AxClickAction::Press,
            });
        }
        // #373: a double/triple-click down on a text view is routed
        // PassThrough (not AxText) so the eventual up posts a real
        // click_state-tagged SL/CGEvent click and lets the target app's own
        // mouseDown handler perform word/paragraph selection -- our own
        // offset+set_selected_range can only place a caret, never select a
        // word, so trying to do it ourselves would silently degrade a
        // double-click into a single click.
        if caps.text_selectable && click_count >= 2 {
            log::info!(
                "remote-control: AX pointer down window_id={window_id} mode=PassThrough role={} reason=multi-click-text click_count={click_count}",
                ax_role_description(&element)
            );
            return Ok(GestureMode::PassThrough);
        }
        if caps.text_selectable {
            return match ax.offset_at_point(&element, point) {
                Ok(anchor_offset) => Ok(GestureMode::AxText {
                    element,
                    anchor_offset,
                }),
                Err(error) if error.is_api_disabled() => Err(accessibility_revoked_error()),
                Err(error) if error.is_invalid_ui_element() => {
                    log::warn!(
                        "remote-control: AX text element became invalid while starting pointer gesture"
                    );
                    Ok(GestureMode::PassThrough)
                }
                Err(error) if error.is_capability_miss() => {
                    log::info!(
                        "remote-control: AX pointer down window_id={window_id} mode=PassThrough role={} attempted=text_anchor_offset failed={error:?} caps={caps:?}",
                        ax_role_description(&element)
                    );
                    Ok(GestureMode::PassThrough)
                }
                Err(error) => {
                    log::warn!(
                        "remote-control: AX offset lookup failed while starting pointer gesture: {:?}",
                        error
                    );
                    Ok(GestureMode::PassThrough)
                }
            };
        }
        log::info!(
            "remote-control: AX pointer down window_id={window_id} mode=PassThrough role={} attempted=pressable,text_selectable failed caps={caps:?}",
            ax_role_description(&element)
        );
        Ok(GestureMode::PassThrough)
    }

    fn ax_click_action_name(action: AxClickAction) -> &'static str {
        match action {
            AxClickAction::Press => "Press",
            AxClickAction::ShowMenu => "ShowMenu",
        }
    }

    fn ax_replay_outcome_name(outcome: AxReplayOutcome) -> &'static str {
        match outcome {
            AxReplayOutcome::Handled => "Handled",
            AxReplayOutcome::PassThrough => "PassThrough",
        }
    }

    fn gesture_mode_summary(mode: &GestureMode) -> String {
        match mode {
            GestureMode::PassThrough => "PassThrough".to_string(),
            GestureMode::SlDrag => "SlDrag".to_string(),
            GestureMode::SessionTap => "SessionTap".to_string(),
            GestureMode::AxPressable { action, .. } => {
                format!("AxPressable action={}", ax_click_action_name(*action))
            }
            GestureMode::AxText { anchor_offset, .. } => {
                format!("AxText anchor_offset={anchor_offset}")
            }
        }
    }

    fn ax_pointer_drag_move(
        window_id: u32,
        controller_id: &str,
        pid: i32,
        point: super::GlobalPoint,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        // #446: resolve the route while holding the lock, then release it
        // BEFORE posting -- the session-tap post reaches into CoreGraphics and
        // must not run under the gesture mutex (std::sync::Mutex is not
        // reentrant and the cursor-takeover map is a second lock).
        let mode = {
            let gestures = ax_pointer_gestures().lock_unpoisoned();
            match gestures.get(&(window_id, controller_id.to_string())) {
                Some(state) => match state.mode {
                    GestureMode::SessionTap => Some((GestureMode::SessionTap, state.button)),
                    _ => None,
                },
                None => return Ok(AxReplayOutcome::PassThrough),
            }
        };
        if let Some((GestureMode::SessionTap, button)) = mode {
            let kind = match button {
                RemoteControlButton::Left => MouseKind::LeftDragged,
                RemoteControlButton::Right => MouseKind::RightDragged,
                RemoteControlButton::Middle => MouseKind::OtherDragged,
            };
            return match tap.post_mouse(point, button, kind, 1) {
                Ok(()) => {
                    // No restore here: a drag must keep the cursor where the
                    // drag is. The restore happens once, at Up.
                    note_cursor_posted(window_id, point, None);
                    // #611: record where the pointer now is, so a cancellation
                    // mid-drag releases HERE instead of back at down_point.
                    // Written after the post succeeds and outside the earlier
                    // lock scope -- never hold the gesture mutex across a
                    // CoreGraphics call.
                    if let Some(state) = ax_pointer_gestures()
                        .lock_unpoisoned()
                        .get_mut(&(window_id, controller_id.to_string()))
                    {
                        state.last_point = point;
                    }
                    Ok(AxReplayOutcome::Handled)
                }
                Err(error) => Err(error),
            };
        }
        let mut gestures = ax_pointer_gestures().lock_unpoisoned();
        let Some(state) = gestures.get_mut(&(window_id, controller_id.to_string())) else {
            return Ok(AxReplayOutcome::PassThrough);
        };
        let button = state.button;
        match &state.mode {
            GestureMode::PassThrough => Ok(AxReplayOutcome::PassThrough),
            GestureMode::SessionTap => Ok(AxReplayOutcome::Handled),
            GestureMode::SlDrag => {
                match sl.post_mouse_event(pid, point, button, SlMouseEvent::Dragged) {
                    Ok(()) => {
                        // #611: keep the live position accurate on this route too,
                        // so any cancellation path reading it gets the truth rather
                        // than the drag origin.
                        state.last_point = point;
                        Ok(AxReplayOutcome::Handled)
                    }
                    Err(SlClickError::Unavailable) => {
                        Err("SkyLight drag route unavailable".to_string())
                    }
                    Err(SlClickError::Failed(error)) => Err(error),
                }
            }
            _ => Ok(AxReplayOutcome::Handled),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ax_pointer_up(
        window_id: u32,
        controller_id: &str,
        pid: i32,
        up_point: super::GlobalPoint,
        button: RemoteControlButton,
        click_count: u32,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        let Some(state) = ax_pointer_gestures()
            .lock_unpoisoned()
            .remove(&(window_id, controller_id.to_string()))
        else {
            // #446 follow-up: an orphaned Up (no gesture recorded -- the Down
            // was superseded/cancelled, e.g. by an epoch bump from a rapid
            // re-request) used to bail out here without ever attempting the
            // SkyLight fallback, silently dropping the click that a stored
            // GestureMode::PassThrough (right below) would have delivered.
            // Attempt the same fallback here instead of guaranteeing loss.
            let outcome = sl_click_or_passthrough(pid, up_point, button, 0.0, click_count, sl);
            log::info!(
                "remote-control: AX pointer up window_id={window_id} controller='{controller_id}' mode=<none> outcome={}",
                ax_replay_outcome_name(outcome)
            );
            return Ok(outcome);
        };
        // Fable-review fix (#369): the map entry is always removed above (so
        // no orphan lingers), but if THIS event's own deadline waiter already
        // gave up on it, don't perform the actual AX/SL/CGEvent action below
        // -- it could land long after the event was reported dropped, acting
        // on state the target app no longer reflects (e.g. pressing a button
        // that has since disappeared, or selecting into a changed document).
        if super::injection_was_cancelled() {
            // #446 review finding: SlDrag is the ONE mode where something is
            // physically held (a real posted mouse-down) before Up-time --
            // unlike every AX mode, where skipping a late action is safe
            // because nothing OS-level is held yet. Abandoning a SlDrag Up
            // here would leave a permanent phantom held mouse button in the
            // target app, worse than the bug this whole fallback exists to
            // fix. Still post the release; only skip the AX-side/CGEvent
            // side effects the comment above is actually protecting against.
            //
            // Fable review (this fix): this thread's own deadline waiter has
            // already given up by the time we get here, so the per-pid replay
            // shard may already be injecting the NEXT gesture's Down
            // concurrently. Spreading the retried releases over a few ms
            // widens (doesn't newly introduce) the pre-existing window where
            // a late release could land after a fresh Down and immediately
            // end the new drag. Accepted: a rare early-release beats the
            // near-certain permanently-held button this path exists to avoid.
            // #446: same reasoning as SlDrag below -- a session-tap Down is a
            // real, physically held button. Abandoning its Up would leave a
            // phantom held button in the target app, which is strictly worse
            // than the silent no-op this whole route exists to fix. Always
            // post the release, then hand the cursor back.
            if let GestureMode::SessionTap = state.mode {
                let outcome = session_tap_pointer_up(window_id, up_point, state.button, tap);
                log::warn!(
                    "remote-control: AX pointer up window_id={window_id} mode=SessionTap abandoned before completing -- posted release anyway (ok={})",
                    outcome.is_ok()
                );
                return Ok(AxReplayOutcome::Handled);
            }
            if let GestureMode::SlDrag = state.mode {
                match post_sl_release_with_retry(pid, up_point, state.button, sl) {
                    Ok(()) => log::warn!(
                        "remote-control: AX pointer up window_id={window_id} mode=SlDrag abandoned before completing -- posted release anyway"
                    ),
                    Err(SlClickError::Unavailable) => log::warn!(
                        "remote-control: AX pointer up window_id={window_id} mode=SlDrag abandoned before completing -- release route unavailable, button may remain held"
                    ),
                    Err(SlClickError::Failed(error)) => log::warn!(
                        "remote-control: AX pointer up window_id={window_id} mode=SlDrag abandoned before completing -- release failed ({error}), button may remain held"
                    ),
                }
                return Ok(AxReplayOutcome::Handled);
            }
            log::info!(
                "remote-control: AX pointer up window_id={window_id} abandoned before completing -- skipping action"
            );
            return Ok(AxReplayOutcome::PassThrough);
        }
        let displacement = point_distance(state.down_point, up_point);
        match state.mode {
            GestureMode::SessionTap => {
                let result = session_tap_pointer_up(window_id, up_point, state.button, tap);
                log::info!(
                    "remote-control: AX pointer up window_id={window_id} mode=SessionTap displacement={displacement:.2} outcome={}",
                    if result.is_ok() { "Handled" } else { "Error" }
                );
                result.map(|()| AxReplayOutcome::Handled)
            }
            GestureMode::PassThrough => {
                let outcome = sl_click_or_passthrough(
                    pid,
                    up_point,
                    state.button,
                    displacement,
                    state.click_count,
                    sl,
                );
                log::info!(
                    "remote-control: AX pointer up window_id={window_id} mode=PassThrough displacement={displacement:.2} outcome={}",
                    ax_replay_outcome_name(outcome)
                );
                Ok(outcome)
            }
            GestureMode::SlDrag => {
                let outcome = match post_sl_release_with_retry(pid, up_point, state.button, sl) {
                    Ok(()) => AxReplayOutcome::Handled,
                    Err(SlClickError::Unavailable) => {
                        return Err("SkyLight drag release route unavailable".to_string())
                    }
                    Err(SlClickError::Failed(error)) => return Err(error),
                };
                log::info!(
                    "remote-control: AX pointer up window_id={window_id} mode=SlDrag displacement={displacement:.2} outcome=Handled"
                );
                Ok(outcome)
            }
            GestureMode::AxPressable { element, action } => {
                if action == AxClickAction::ShowMenu
                    && displacement >= AX_CLICK_DRAG_THRESHOLD_POINTS
                {
                    let outcome = sl_click_or_passthrough(
                        pid,
                        up_point,
                        state.button,
                        displacement,
                        state.click_count,
                        sl,
                    );
                    log::info!(
                        "remote-control: AX pointer up window_id={window_id} mode=AxPressable action=ShowMenu displacement={displacement:.2} skipped_ax_action=drag outcome={}",
                        ax_replay_outcome_name(outcome)
                    );
                    return Ok(outcome);
                }
                let result = match action {
                    AxClickAction::Press => ax.press(&element),
                    AxClickAction::ShowMenu => ax.show_menu(&element),
                };
                let outcome = ax_action_outcome(result, "pressable pointer gesture")?;
                let final_outcome = if outcome == AxReplayOutcome::PassThrough {
                    sl_click_or_passthrough(
                        pid,
                        up_point,
                        state.button,
                        displacement,
                        state.click_count,
                        sl,
                    )
                } else {
                    outcome
                };
                log::info!(
                    "remote-control: AX pointer up window_id={window_id} mode=AxPressable action={} displacement={displacement:.2} ax_outcome={} outcome={}",
                    ax_click_action_name(action),
                    ax_replay_outcome_name(outcome),
                    ax_replay_outcome_name(final_outcome)
                );
                Ok(final_outcome)
            }
            GestureMode::AxText {
                element,
                anchor_offset,
            } => {
                let up_offset = match ax.offset_at_point(&element, up_point) {
                    Ok(offset) => offset,
                    Err(error) => {
                        let outcome = ax_error_outcome(error, "AX text offset lookup")?;
                        let final_outcome = if outcome == AxReplayOutcome::PassThrough {
                            sl_click_or_passthrough(
                                pid,
                                up_point,
                                state.button,
                                displacement,
                                state.click_count,
                                sl,
                            )
                        } else {
                            outcome
                        };
                        log::info!(
                            "remote-control: AX pointer up window_id={window_id} mode=AxText anchor_offset={anchor_offset} displacement={displacement:.2} up_offset_error={error:?} ax_outcome={} outcome={}",
                            ax_replay_outcome_name(outcome),
                            ax_replay_outcome_name(final_outcome)
                        );
                        return Ok(final_outcome);
                    }
                };
                let (start, len) = if displacement < AX_CLICK_DRAG_THRESHOLD_POINTS {
                    (up_offset, 0)
                } else {
                    let start = anchor_offset.min(up_offset);
                    (start, (up_offset - anchor_offset).abs())
                };
                let outcome = ax_action_outcome(
                    ax.set_selected_range(&element, start, len),
                    "AX selected text range update",
                )?;
                let final_outcome = if outcome == AxReplayOutcome::PassThrough {
                    sl_click_or_passthrough(
                        pid,
                        up_point,
                        state.button,
                        displacement,
                        state.click_count,
                        sl,
                    )
                } else {
                    outcome
                };
                log::info!(
                    "remote-control: AX pointer up window_id={window_id} mode=AxText anchor_offset={anchor_offset} displacement={displacement:.2} up_offset={up_offset} selection_start={start} selection_len={len} ax_outcome={} outcome={}",
                    ax_replay_outcome_name(outcome),
                    ax_replay_outcome_name(final_outcome)
                );
                Ok(final_outcome)
            }
        }
    }

    /// Close a session-tap gesture: post the release, then restore the host
    /// cursor. The restore happens HERE (once per gesture) and never between
    /// a Down and its drag Moves.
    ///
    /// The release is posted even if the restore is later skipped, and the
    /// takeover is always dropped -- a failure to hand the cursor back must
    /// not also leave the button held.
    fn session_tap_pointer_up(
        window_id: u32,
        up_point: super::GlobalPoint,
        button: RemoteControlButton,
        tap: &dyn SessionTapBackend,
    ) -> Result<(), String> {
        let kind = match button {
            RemoteControlButton::Left => MouseKind::LeftUp,
            RemoteControlButton::Right => MouseKind::RightUp,
            RemoteControlButton::Middle => MouseKind::OtherUp,
        };
        let posted = tap.post_mouse(up_point, button, kind, 1);
        if posted.is_ok() {
            note_cursor_posted(window_id, up_point, None);
        }
        end_cursor_takeover(window_id, tap);
        posted
    }

    /// Release every session-tap gesture still open on `window_id` and undo
    /// its cursor takeover. Safe to call when there is nothing open.
    ///
    /// This is the single cancellation path: `drain_window_control` funnels
    /// revoke, disconnect, share-ended and deadline-abandon into it, so a
    /// held button cannot outlive the grant that created it.
    pub fn release_session_tap_gestures_for_window(window_id: u32) {
        let tap = SystemSessionTapBackend;
        release_session_tap_gestures_with_backend(window_id, None, &tap);
    }

    /// `only_controller` scopes the cancellation to one controller: #374
    /// keys gesture state per (window, controller), so a concurrent
    /// controller's in-progress drag on the same window must survive a
    /// single controller's revoke.
    fn release_session_tap_gestures_with_backend(
        window_id: u32,
        only_controller: Option<&str>,
        tap: &dyn SessionTapBackend,
    ) -> usize {
        // Collect first, then post: never hold the gesture lock across a
        // CoreGraphics call.
        let (open, window_still_has_gestures) = {
            let mut gestures = ax_pointer_gestures().lock_unpoisoned();
            let keys: Vec<(u32, String)> = gestures
                .iter()
                .filter(|((stored_window_id, stored_controller_id), state)| {
                    *stored_window_id == window_id
                        && matches!(state.mode, GestureMode::SessionTap)
                        && match only_controller {
                            Some(controller_id) => stored_controller_id == controller_id,
                            None => true,
                        }
                })
                .map(|(key, _)| key.clone())
                .collect();
            let open: Vec<(super::GlobalPoint, RemoteControlButton)> = keys
                .iter()
                .filter_map(|key| gestures.remove(key))
                // #611: release where the pointer actually is, not where the
                // drag began.
                .map(|state| (state.last_point, state.button))
                .collect();
            // A second controller may still be mid-gesture on this window; its
            // cursor takeover must not be torn down under it.
            let remaining = gestures.iter().any(|((stored_window_id, _), state)| {
                *stored_window_id == window_id && matches!(state.mode, GestureMode::SessionTap)
            });
            (open, remaining)
        };
        for (point, button) in &open {
            let kind = match button {
                RemoteControlButton::Left => MouseKind::LeftUp,
                RemoteControlButton::Right => MouseKind::RightUp,
                RemoteControlButton::Middle => MouseKind::OtherUp,
            };
            if let Err(error) = tap.post_mouse(*point, *button, kind, 1) {
                log::warn!(
                    "remote-control: session-tap cancellation release failed window_id={window_id}: {error} -- button may remain held"
                );
            } else {
                log::warn!(
                    "remote-control: session-tap gesture cancelled window_id={window_id} -- posted synthetic release"
                );
            }
        }
        // Always drop the takeover, even with nothing open: a wheel stream
        // leaves a pending restore with no gesture entry behind it. The one
        // exception is a per-controller release that left another
        // controller's gesture open on the same window -- the takeover map is
        // keyed by window, so ending it there would hand the cursor back
        // mid-drag for someone else.
        if !window_still_has_gestures {
            end_cursor_takeover(window_id, tap);
        }
        open.len()
    }

    fn sl_click_or_passthrough(
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        displacement: f64,
        click_count: u32,
        sl: &dyn SlClickBackend,
    ) -> AxReplayOutcome {
        if displacement >= AX_CLICK_DRAG_THRESHOLD_POINTS {
            return AxReplayOutcome::PassThrough;
        }
        match post_sl_click_with_priming(pid, point, button, click_count.max(1), sl) {
            SlClickOutcome::Posted => AxReplayOutcome::Handled,
            SlClickOutcome::PassThrough => AxReplayOutcome::PassThrough,
        }
    }

    // SLEventPostToPid is fire-and-forget: a successful return from our
    // wrapper only means that the CGEvent was constructed and handed to
    // SkyLight. A duplicate mouse-Up cannot activate a control, while a
    // duplicate Down/Up click can, so retry releases only.
    const SL_RELEASE_ATTEMPTS: usize = 3;

    // Fable review (this fix): live evidence was a 2-of-3 loss in one
    // sample -- that supports "losses can be correlated," not "3 attempts is
    // enough." A uniform 1ms gap barely spans one scheduler quantum, so a
    // loss burst wider than ~2ms would have killed all 3 attempts anyway.
    // Growing gaps (5ms, then 25ms) instead of a fixed 1ms spread the
    // attempts across enough real wall-clock time to decorrelate from a
    // single momentary stall, while staying well under user-perceptible
    // latency for a mouse release.
    const SL_RELEASE_RETRY_DELAYS_MS: [u64; SL_RELEASE_ATTEMPTS - 1] = [5, 25];

    fn post_sl_release_with_retry(
        pid: i32,
        point: super::GlobalPoint,
        button: RemoteControlButton,
        sl: &dyn SlClickBackend,
    ) -> Result<(), SlClickError> {
        let mut last_error = None;
        let mut posted = false;
        for attempt in 0..SL_RELEASE_ATTEMPTS {
            match sl.post_mouse_event(pid, point, button, SlMouseEvent::Up) {
                // There is no delivery acknowledgement, so keep posting
                // even after a locally successful handoff. The target sees
                // at most duplicate releases, never duplicate activation.
                Ok(()) => posted = true,
                Err(error) => last_error = Some(error),
            }
            if let Some(delay_ms) = SL_RELEASE_RETRY_DELAYS_MS.get(attempt) {
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            }
        }
        if posted {
            Ok(())
        } else {
            Err(last_error.expect("release attempts must include one result"))
        }
    }

    fn ax_action_outcome(
        result: Result<(), AxError>,
        context: &str,
    ) -> Result<AxReplayOutcome, String> {
        match result {
            Ok(()) => Ok(AxReplayOutcome::Handled),
            Err(error) => {
                clear_ax_resolution_cache();
                ax_error_outcome(error, context)
            }
        }
    }

    fn ax_error_outcome(error: AxError, context: &str) -> Result<AxReplayOutcome, String> {
        clear_ax_resolution_cache();
        if error.is_api_disabled() {
            log::warn!("remote-control: Accessibility permission was revoked during {context}");
            return Err(accessibility_revoked_error());
        }
        if error.is_invalid_ui_element() {
            log::warn!("remote-control: stale AX element during {context}; abandoning gesture");
            return Ok(AxReplayOutcome::Handled);
        }
        if error.is_capability_miss() {
            log::debug!("remote-control: AX capability disappeared during {context}: {error:?}");
            return Ok(AxReplayOutcome::PassThrough);
        }
        log::warn!("remote-control: {context} failed: {error:?}");
        Ok(AxReplayOutcome::PassThrough)
    }

    /// F4: error->outcome mapping for the Key AX shortcut path. Unlike the
    /// pointer-gesture path (`ax_error_outcome`, which returns `Handled` on a
    /// stale `invalid_ui_element` to ABANDON a partially-posted multi-event
    /// gesture), a Cmd+A/C/V is a single atomic act with no gesture to abandon:
    /// if it didn't actually happen we want the CGEvent key-equivalent to still
    /// fire (never-worse-than-before when the target app is frontmost on the
    /// host). So map every non-fatal failure — including the stale-element race
    /// between resolve and act — to `PassThrough`. Only a revoked-permission
    /// error is fatal.
    fn ax_key_error_outcome(error: AxError, context: &str) -> Result<AxReplayOutcome, String> {
        clear_ax_resolution_cache();
        if error.is_api_disabled() {
            log::warn!("remote-control: Accessibility permission was revoked during {context}");
            return Err(accessibility_revoked_error());
        }
        log::debug!(
            "remote-control: {context} failed: {error:?}; falling back to CGEvent key-equivalent"
        );
        Ok(AxReplayOutcome::PassThrough)
    }

    fn point_distance(a: super::GlobalPoint, b: super::GlobalPoint) -> f64 {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        (dx * dx + dy * dy).sqrt()
    }

    fn replay_wheel_via_ax(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
        ax: &dyn AxInputBackend,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        let Some(pid) = target_pid.filter(|pid| *pid > 0) else {
            return Ok(AxReplayOutcome::PassThrough);
        };
        let x = message
            .x
            .ok_or_else(|| "wheel message missing x".to_string())?;
        let y = message
            .y
            .ok_or_else(|| "wheel message missing y".to_string())?;
        let point = normalized_to_global(frame, x, y);
        let element = match resolve_cached(message.window_id, pid, point, ax) {
            Ok(Some((element, _caps))) => element,
            Ok(None) => return wheel_sl_fallback(message, frame, pid, point, sl, tap),
            Err(error) if error.is_window_id_mismatch() => {
                return Err(window_identity_error(message.window_id))
            }
            Err(error) if error.is_window_identity_unavailable() => {
                return Err(window_identity_unavailable_error(message.window_id))
            }
            Err(error) if error.is_api_disabled() => return Err(accessibility_revoked_error()),
            Err(error) if error.is_invalid_ui_element() => {
                log::warn!("remote-control: invalid AX app/element during wheel hit-test");
                return wheel_sl_fallback(message, frame, pid, point, sl, tap);
            }
            Err(error) if error.is_capability_miss() => {
                return wheel_sl_fallback(message, frame, pid, point, sl, tap)
            }
            Err(error) => {
                log::warn!("remote-control: AX wheel hit-test failed: {error:?}");
                return wheel_sl_fallback(message, frame, pid, point, sl, tap);
            }
        };
        let (dx, dy) = wheel_delta_pixels(message, frame);
        if dx == 0 && dy == 0 {
            return Ok(AxReplayOutcome::Handled);
        }
        match ax.scroll_by(
            message.window_id,
            point,
            &element,
            f64::from(dy),
            f64::from(dx),
        ) {
            // #368 F1 invalidation is applied centrally in replay_with_backends
            // (which also covers the Ok(false) synthetic-scroll passthrough that
            // still scrolls the content — see message_mutates_ui for Wheel).
            Ok(true) => Ok(AxReplayOutcome::Handled),
            Ok(false) => wheel_sl_fallback(message, frame, pid, point, sl, tap),
            Err(error) => ax_action_outcome(Err(error), "AX scroll replay"),
        }
    }

    fn wheel_sl_fallback(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        pid: i32,
        point: super::GlobalPoint,
        sl: &dyn SlClickBackend,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        let (dx, dy) = wheel_delta_pixels(message, frame);
        let flags = cg_flags_for_modifiers(&message.modifiers);
        if direct_scroll_enabled() {
            match sl.post_scroll(pid, point, dy, dx, flags) {
                Ok(()) => return Ok(AxReplayOutcome::Handled),
                Err(SlClickError::Unavailable) => {}
                Err(SlClickError::Failed(error)) => {
                    SL_FAILURE_LOGGED.get_or_init(|| {
                        log::warn!("remote-control: SLEventPostToPid scroll failed: {error}")
                    });
                }
            }
        }
        // #446: AX exposed no scrollable element (custom-drawn content) --
        // take the session tap. A wheel stream has no Up to close it, so the
        // cursor restore is DEBOUNCED: each event pushes the deadline out, and
        // the watchdog restores once the stream has been quiet.
        session_tap_wheel(message.window_id, pid, point, dy, dx, flags, tap)
    }

    fn session_tap_wheel(
        window_id: u32,
        pid: i32,
        point: super::GlobalPoint,
        delta_y: i32,
        delta_x: i32,
        flags: u64,
        tap: &dyn SessionTapBackend,
    ) -> Result<AxReplayOutcome, String> {
        if let Err(error) = prepare_session_tap_target(window_id, pid, point, tap) {
            log::info!(
                "remote-control: session-tap wheel unavailable window_id={window_id}: {error}"
            );
            return Ok(AxReplayOutcome::PassThrough);
        }
        match tap.post_scroll(point, delta_y, delta_x, flags) {
            Ok(()) => {
                note_cursor_posted(window_id, point, Some(SESSION_TAP_WHEEL_SETTLE));
                Ok(AxReplayOutcome::Handled)
            }
            Err(error) => {
                end_cursor_takeover(window_id, tap);
                Err(error)
            }
        }
    }

    /// #170: clipboard/select-all key equivalents we service via AX instead of
    /// letting them fall through to the (backgrounded-app-broken) CGEvent path.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TextShortcut {
        SelectAll,
        Copy,
        Paste,
    }

    impl TextShortcut {
        /// F5: a shortcut whose effect DESTROYS or overwrites field contents if
        /// aimed at the wrong element. Cmd+V overwrites the selection; Cmd+A is
        /// destructive because the next keystroke replaces the whole selection.
        /// Cmd+C (copy) is read-only, so it is safe on a best-effort target.
        fn is_destructive(self) -> bool {
            matches!(self, TextShortcut::SelectAll | TextShortcut::Paste)
        }
    }

    /// F5: provenance of a resolved text element. `FocusedElement` means the
    /// window's own AXFocusedUIElement WAS the editable text element — a target
    /// we trust. `BfsFallback` means we fell back to the shallowest
    /// text-selectable descendant, which for a browser can be a URL/search bar
    /// sitting ABOVE the document. Destructive shortcuts (Cmd+V/Cmd+A) must not
    /// act on a BFS-fallback element and instead pass through to CGEvent.
    /// Broadening BFS coverage to safely pick the intended field is deferred to
    /// live validation (follow-up issue).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    enum TextElementSource {
        #[default]
        FocusedElement,
        BfsFallback,
    }

    impl TextElementSource {
        fn is_trusted_focus(self) -> bool {
            matches!(self, TextElementSource::FocusedElement)
        }
    }

    /// Classify a key-down message as one of the AX-serviced text shortcuts.
    /// Requires exactly Cmd (+A/+C/+V) with no Shift/Ctrl/Alt, so Cmd+Shift+A,
    /// Ctrl+A, plain A, etc. are left to the normal CGEvent path.
    ///
    /// F8: macOS menu key-equivalents are matched by the LOGICAL character, so we
    /// match the logical `key` (a/c/v) FIRST and only fall back to the physical
    /// `code` (KeyA/KeyC/KeyV) when the logical key is empty/unavailable.
    /// Matching the physical code first mis-fired on non-US layouts — on AZERTY
    /// physical `KeyA` is logical `q` (Cmd+Q would look like select-all); on
    /// Dvorak physical `KeyC` is logical `j` (Cmd+J would look like copy).
    fn classify_text_shortcut(message: &RemoteControlMessage) -> Option<TextShortcut> {
        let m = &message.modifiers;
        if !m.meta || m.shift || m.ctrl || m.alt {
            return None;
        }
        let code = message.code.as_deref().unwrap_or("");
        let key = message.key.as_deref().unwrap_or("");
        let matches = |code_name: &str, key_ch: char| {
            if !key.is_empty() {
                key.eq_ignore_ascii_case(&key_ch.to_string())
            } else {
                code == code_name
            }
        };
        if matches("KeyA", 'a') {
            Some(TextShortcut::SelectAll)
        } else if matches("KeyC", 'c') {
            Some(TextShortcut::Copy)
        } else if matches("KeyV", 'v') {
            Some(TextShortcut::Paste)
        } else {
            None
        }
    }

    /// #170: the full-document selection range for Cmd+A: the whole string,
    /// anchored at 0. Split out so the trivial-but-load-bearing arithmetic is
    /// unit-tested independently of the AX plumbing.
    fn select_all_range(text_length: i64) -> (i64, i64) {
        (0, text_length.max(0))
    }

    fn replay_key_via_ax(
        message: &RemoteControlMessage,
        target_pid: Option<i32>,
        ax: &dyn AxInputBackend,
        pb: &dyn PasteboardBackend,
    ) -> Result<AxReplayOutcome, String> {
        // Only act on the key-DOWN of a recognized shortcut; key-up and auto-
        // repeat re-run would double-apply (e.g. paste twice).
        let Some(shortcut) = classify_text_shortcut(message) else {
            return Ok(AxReplayOutcome::PassThrough);
        };
        if message.action != Some(RemoteControlAction::Down) || message.repeat {
            return Ok(AxReplayOutcome::PassThrough);
        }
        let Some(pid) = target_pid.filter(|pid| *pid > 0) else {
            return Ok(AxReplayOutcome::PassThrough);
        };
        let (element, source) = match ax.resolve_text_element(pid, message.window_id) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                log::info!(
                    "remote-control: AX text shortcut {shortcut:?} found no text element in pid {pid}; falling back to CGEvent"
                );
                return Ok(AxReplayOutcome::PassThrough);
            }
            Err(error) if error.is_window_id_mismatch() => {
                return Err(window_identity_error(message.window_id))
            }
            Err(error) if error.is_window_identity_unavailable() => {
                return Err(window_identity_unavailable_error(message.window_id))
            }
            Err(error) if error.is_api_disabled() => return Err(accessibility_revoked_error()),
            Err(error) if error.is_invalid_ui_element() => {
                log::warn!(
                    "remote-control: invalid AX element while resolving text target for {shortcut:?}"
                );
                return Ok(AxReplayOutcome::PassThrough);
            }
            Err(error) if error.is_capability_miss() => return Ok(AxReplayOutcome::PassThrough),
            Err(error) => {
                log::warn!(
                    "remote-control: AX text-target lookup failed for {shortcut:?}: {error:?}"
                );
                return Ok(AxReplayOutcome::PassThrough);
            }
        };
        // Defensive: only drive elements we can actually select/edit. Guards
        // against a resolver that returned a non-text element.
        let caps = ax.capabilities(&element);
        if !caps.text_selectable {
            log::info!(
                "remote-control: AX text shortcut {shortcut:?} skipped; resolved role={} not text-selectable caps={caps:?}",
                ax_role_description(&element)
            );
            return Ok(AxReplayOutcome::PassThrough);
        }
        // F5: a destructive shortcut (paste / select-all) may only drive an
        // element resolved from genuine window focus. A BFS-fallback element can
        // be the wrong field (e.g. a browser URL bar above the document), so
        // pass such cases through to CGEvent rather than risk a wrong-field
        // paste/overwrite. Copy (read-only) may still use the BFS result.
        if shortcut.is_destructive() && !source.is_trusted_focus() {
            log::info!(
                "remote-control: AX text shortcut {shortcut:?} resolved via BFS fallback (untrusted for a destructive op); falling back to CGEvent"
            );
            return Ok(AxReplayOutcome::PassThrough);
        }
        match shortcut {
            TextShortcut::SelectAll => ax_select_all(&element, ax),
            TextShortcut::Copy => ax_copy_selection(&element, ax, pb),
            TextShortcut::Paste => ax_paste_selection(&element, ax, pb),
        }
    }

    fn ax_select_all(
        element: &AxElementHandle,
        ax: &dyn AxInputBackend,
    ) -> Result<AxReplayOutcome, String> {
        let len = match ax.text_length(element) {
            Ok(len) => len,
            Err(error) => return ax_key_error_outcome(error, "AX select-all text length"),
        };
        let (start, span) = select_all_range(len);
        match ax.set_selected_range(element, start, span) {
            Ok(()) => {
                log::info!(
                    "remote-control: AX select-all selected full text role={} len={span}",
                    ax_role_description(element)
                );
                Ok(AxReplayOutcome::Handled)
            }
            Err(error) => ax_key_error_outcome(error, "AX select-all range update"),
        }
    }

    fn ax_copy_selection(
        element: &AxElementHandle,
        ax: &dyn AxInputBackend,
        pb: &dyn PasteboardBackend,
    ) -> Result<AxReplayOutcome, String> {
        match ax.selected_text(element) {
            Ok(Some(text)) if !text.is_empty() => {
                pb.write_text(&text);
                log::info!(
                    "remote-control: AX Cmd+C copied {} chars to pasteboard",
                    text.chars().count()
                );
                Ok(AxReplayOutcome::Handled)
            }
            // Empty / absent selection: nothing to copy. Don't clobber the
            // clipboard; let CGEvent try (harmless if it also does nothing).
            Ok(_) => {
                log::info!("remote-control: AX Cmd+C found no selection; falling back to CGEvent");
                Ok(AxReplayOutcome::PassThrough)
            }
            Err(error) => ax_key_error_outcome(error, "AX Cmd+C selected-text read"),
        }
    }

    fn ax_paste_selection(
        element: &AxElementHandle,
        ax: &dyn AxInputBackend,
        pb: &dyn PasteboardBackend,
    ) -> Result<AxReplayOutcome, String> {
        let Some(text) = pb.read_text().filter(|text| !text.is_empty()) else {
            log::info!("remote-control: AX Cmd+V found empty pasteboard; falling back to CGEvent");
            return Ok(AxReplayOutcome::PassThrough);
        };
        match ax.set_selected_text(element, &text) {
            Ok(()) => {
                log::info!(
                    "remote-control: AX Cmd+V inserted {} chars via AXSelectedText",
                    text.chars().count()
                );
                Ok(AxReplayOutcome::Handled)
            }
            Err(error) => ax_key_error_outcome(error, "AX Cmd+V selected-text replace"),
        }
    }

    fn accessibility_revoked_error() -> String {
        "accessibilityDenied: Accessibility permission was revoked during remote-control replay"
            .to_string()
    }

    fn window_identity_error(window_id: u32) -> String {
        format!(
            "windowMismatch: resolved input target does not belong to authorized window {window_id}"
        )
    }

    fn window_identity_unavailable_error(window_id: u32) -> String {
        log::warn!(
            "remote-control: input refused window_id={window_id} reason=window-identity-unavailable"
        );
        format!(
            "targetUnavailable: input target window identity could not be established for authorized window {window_id}"
        )
    }

    fn pointer_event_kind(
        action: RemoteControlAction,
        button: RemoteControlButton,
    ) -> (MouseKind, RemoteControlButton) {
        let kind = match (action, button) {
            (RemoteControlAction::Move, _) => MouseKind::Moved,
            (RemoteControlAction::Down, RemoteControlButton::Left) => MouseKind::LeftDown,
            (RemoteControlAction::Down, RemoteControlButton::Right) => MouseKind::RightDown,
            (RemoteControlAction::Down, RemoteControlButton::Middle) => MouseKind::OtherDown,
            (RemoteControlAction::Up, RemoteControlButton::Left) => MouseKind::LeftUp,
            (RemoteControlAction::Up, RemoteControlButton::Right) => MouseKind::RightUp,
            (RemoteControlAction::Up, RemoteControlButton::Middle) => MouseKind::OtherUp,
            (RemoteControlAction::Click, _) => {
                // Semantic clicks are consumed before reaching the synthetic
                // CGEvent sink. Keep this arm explicit so a future caller
                // cannot silently turn a click into a partial mouse event.
                return (MouseKind::LeftDown, button);
            }
            (RemoteControlAction::Unknown, _) => {
                unreachable!("handle_message drops Unknown actions before replay dispatch")
            }
        };
        (kind, button)
    }

    fn drag_kind(button: RemoteControlButton) -> MouseKind {
        match button {
            RemoteControlButton::Left => MouseKind::LeftDragged,
            RemoteControlButton::Right => MouseKind::RightDragged,
            RemoteControlButton::Middle => MouseKind::OtherDragged,
        }
    }

    fn button_number(button: RemoteControlButton) -> u32 {
        match button {
            RemoteControlButton::Left => 0,
            RemoteControlButton::Right => 1,
            RemoteControlButton::Middle => 2,
        }
    }

    fn replay_pointer(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        sink: &dyn InputSink,
    ) -> Result<(), String> {
        let x = message
            .x
            .ok_or_else(|| "pointer message missing x".to_string())?;
        let y = message
            .y
            .ok_or_else(|| "pointer message missing y".to_string())?;
        let point = normalized_to_global(frame, x, y);
        let action = message
            .action
            .ok_or_else(|| "pointer message missing action".to_string())?;
        if action == RemoteControlAction::Click {
            return Err("semantic click reached legacy pointer sink unexpectedly".to_string());
        }
        let (kind, button, click_state) =
            pointer_event_for(action, message.button, message.buttons, message.click_count);
        sink.mouse(
            kind,
            point,
            button,
            click_state,
            cg_flags_for_modifiers(&message.modifiers),
        )
    }

    fn replay_wheel(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        sink: &dyn InputSink,
    ) -> Result<(), String> {
        let x = message
            .x
            .ok_or_else(|| "wheel message missing x".to_string())?;
        let y = message
            .y
            .ok_or_else(|| "wheel message missing y".to_string())?;
        let point = normalized_to_global(frame, x, y);
        let flags = cg_flags_for_modifiers(&message.modifiers);
        sink.mouse(MouseKind::Moved, point, RemoteControlButton::Left, 0, flags)?;
        let (dx, dy) = wheel_delta_pixels(message, frame);
        sink.scroll(-dy, -dx, point, ScrollUnit::Pixel, flags)
    }

    fn button_from_wire(button: Option<i16>) -> RemoteControlButton {
        match button {
            Some(1) => RemoteControlButton::Middle,
            Some(2) => RemoteControlButton::Right,
            _ => RemoteControlButton::Left,
        }
    }

    fn button_from_buttons(buttons: Option<u16>) -> RemoteControlButton {
        let buttons = buttons.unwrap_or(0);
        if buttons & 2 != 0 {
            RemoteControlButton::Right
        } else if buttons & 4 != 0 {
            RemoteControlButton::Middle
        } else {
            RemoteControlButton::Left
        }
    }

    /// #373: `click_count` is the authoritative multi-click count carried by
    /// the controller's wire message (mirrors DOM `detail`); it becomes the
    /// CGEvent/SL click_state field so a real double-click (click_state=2)
    /// reaches the target instead of two independent single presses. Absent
    /// for old peers/synthetic move events, in which case a non-move action
    /// falls back to click_state=1 (unchanged prior behavior).
    fn click_state_from_count(click_count: Option<u32>) -> u32 {
        click_count.filter(|count| *count > 0).unwrap_or(1)
    }

    fn pointer_event_for(
        action: RemoteControlAction,
        button: Option<i16>,
        buttons: Option<u16>,
        click_count: Option<u32>,
    ) -> (MouseKind, RemoteControlButton, u32) {
        if action == RemoteControlAction::Move && buttons.unwrap_or(0) != 0 {
            let button = button_from_buttons(buttons);
            (drag_kind(button), button, 1)
        } else {
            let (kind, button) = pointer_event_kind(action, button_from_wire(button));
            let click_state = if action == RemoteControlAction::Move {
                0
            } else {
                click_state_from_count(click_count)
            };
            (kind, button, click_state)
        }
    }

    fn replay_key(message: &RemoteControlMessage, sink: &dyn InputSink) -> Result<(), String> {
        let action = message
            .action
            .ok_or_else(|| "key message missing action".to_string())?;
        let key_down = action == RemoteControlAction::Down;
        let Some(plan) = key_replay_plan(message, key_down) else {
            log::debug!(
                "remote-control: dropping unmapped key event window={} seq={} controller='{}' code={:?} key={:?} action={:?}",
                message.window_id,
                message.seq,
                message.controller_id,
                message.code,
                message.key,
                action
            );
            return Ok(());
        };
        match plan {
            KeyReplayPlan::VirtualKey {
                virtual_key,
                unicode,
            } => {
                // Stuck-modifier fix (post-0.8.5 plan item 4; gate lineage
                // #759 -> #777 -> #779): gate the DOWN direction only. A key-UP cannot inject
                // meaning -- it can only STOP a key this session already put
                // down. Refusing it is strictly worse than delivering it to a
                // possibly-wrong window of the same app: every release source
                // (revoke/revoke_window/revoke_all/revoke_controller and the
                // TTL sweeper) drains the pressed entry BEFORE the replay, so
                // a refused Up is dropped permanently and the key stays held
                // in the target app until the sharer physically presses it.
                // This mirrors the AX pointer-up path, which has always posted
                // the release unconditionally for the same reason (a phantom
                // held button is worse than the silent no-op that route
                // exists to fix). DO NOT "restore symmetry" here -- the
                // asymmetry is the fix. The cheap local identity check still
                // runs for Up; only the live focus round-trip is skipped.
                if key_down {
                    sink.verify_key_window_with_recovery(message.window_id)?;
                } else {
                    sink.verify_key_window_sink_identity(message.window_id)?;
                }
                sink.key(
                    virtual_key,
                    key_down,
                    cg_flags_for_modifiers(&message.modifiers),
                    unicode.as_deref(),
                )
            }
            // Text is never a release -- `plain_text_for_key` returns None
            // unless `key_down` -- so this arm stays fully gated.
            KeyReplayPlan::Text(text) => {
                sink.verify_key_window_with_recovery(message.window_id)?;
                sink.text(&text)
            }
        }
    }

    fn replay_text(message: &RemoteControlMessage, sink: &dyn InputSink) -> Result<(), String> {
        let text = capped_replay_text(message.text.as_deref().unwrap_or(""));
        sink.verify_key_window_with_recovery(message.window_id)?;
        sink.text(&text)
    }

    fn post_unicode_char(ch: char, key_down: bool, target_pid: Option<i32>) -> Result<(), String> {
        let text = ch.to_string();
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), 0, key_down);
            if event.is_null() {
                return Err("CGEventCreateKeyboardEvent returned null".to_string());
            }
            CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
            post_event(event, target_pid)?;
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    impl CGEventSink {
        /// Preconditions shared by both key-window checks: a usable pid, and the
        /// requested window pinned to this sink's own authorized window.
        fn key_window_pid(&self, window_id: u32) -> Result<i32, String> {
            let Some(pid) = self.target_pid.filter(|pid| *pid > 0) else {
                return Err("target pid is required for remote-control replay".to_string());
            };
            if window_id != self.window_id {
                return Err(window_identity_error(window_id));
            }
            Ok(pid)
        }
    }

    impl InputSink for CGEventSink {
        /// RAW check -- never raises. `text()` calls this between every character
        /// (2ms cadence), so putting recovery here would fire a raise per
        /// character: hundreds of AX round-trips and hundreds of frontmost-check
        /// races for one paste, and a tug-of-war with any panel the app pops
        /// mid-replay. Mid-text focus drift must abort, as it did before #777.
        /// The recovery lives in `verify_key_window_with_recovery`, called once
        /// per wire event (Fable review, #777).
        fn verify_key_window(&self, window_id: u32) -> Result<(), String> {
            let pid = self.key_window_pid(window_id)?;
            match focused_window_matches(pid, window_id) {
                Ok(true) => Ok(()),
                Err(error) if error.is_window_identity_unavailable() => {
                    Err(window_identity_unavailable_error(window_id))
                }
                _ => Err(format!(
                    "windowMismatch: authorized window {window_id} is not the focused window for pid {pid}"
                )),
            }
        }

        /// Stuck-modifier fix: the local half only -- a usable pid plus the message naming
        /// THIS sink's authorized window. Never raises, never asks AX which
        /// window has focus.
        fn verify_key_window_sink_identity(&self, window_id: u32) -> Result<(), String> {
            self.key_window_pid(window_id).map(|_| ())
        }

        fn verify_key_window_with_recovery(&self, window_id: u32) -> Result<(), String> {
            let pid = self.key_window_pid(window_id)?;
            // Reached only for an already-authorized window: `is_authorized_input`
            // gates enqueue and `resolve_task_still_authorized` re-checks the live
            // grant token at resolve time, and the `window_id != self.window_id`
            // guard above pins this to the sink's own authorized window. A revoked
            // or expired grant therefore never reaches the raise below.
            match focused_window_matches(pid, window_id) {
                Ok(true) => Ok(()),
                Err(error) if error.is_window_identity_unavailable() => {
                    Err(window_identity_unavailable_error(window_id))
                }
                verdict => {
                    let focus_verdict = verdict.as_ref().ok().copied();
                    let frontmost = app_is_frontmost(pid);
                    // Short-circuit is load-bearing: no raise is even attempted
                    // unless the predicate says so (#777).
                    let attempted = should_attempt_key_window_raise(focus_verdict, frontmost);
                    let raised = attempted && raise_authorized_window(pid, window_id);
                    if raised {
                        match focused_window_matches(pid, window_id) {
                            Ok(true) => {
                                log::debug!(
                                    "remote-control: key window recovered by AXRaise window={window_id} pid={pid}"
                                );
                                return Ok(());
                            }
                            Err(error) if error.is_window_identity_unavailable() => {
                                return Err(window_identity_unavailable_error(window_id));
                            }
                            _ => {}
                        }
                    }
                    log::info!(
                        "remote-control: key refused window={window_id} pid={pid} focus={} frontmost={frontmost} raise={}",
                        match &verdict {
                            Ok(false) => "mismatch".to_string(),
                            Ok(true) => "matched-but-recheck-failed".to_string(),
                            Err(error) => format!("error({error:?})"),
                        },
                        if !attempted {
                            "skipped"
                        } else if raised {
                            "no-effect"
                        } else {
                            "failed"
                        }
                    );
                    Err(format!(
                        "windowMismatch: authorized window {window_id} is not the focused window for pid {pid}"
                    ))
                }
            }
        }

        fn mouse(
            &self,
            kind: MouseKind,
            at: super::GlobalPoint,
            button: RemoteControlButton,
            click_state: u32,
            flags: u64,
        ) -> Result<(), String> {
            post_mouse(
                kind,
                at.x,
                at.y,
                button_number(button),
                click_state,
                self.target_pid,
                self.window_id,
                flags,
            )
        }

        fn scroll(
            &self,
            axis1: i32,
            axis2: i32,
            at: super::GlobalPoint,
            unit: ScrollUnit,
            flags: u64,
        ) -> Result<(), String> {
            let units = match unit {
                ScrollUnit::Pixel => K_CG_SCROLL_EVENT_UNIT_PIXEL,
            };
            unsafe {
                let event = CGEventCreateScrollWheelEvent(std::ptr::null(), units, 1, axis1);
                if event.is_null() {
                    return Err("CGEventCreateScrollWheelEvent returned null".to_string());
                }
                CGEventSetFlags(event, flags);
                CGEventSetIntegerValueField(
                    event,
                    K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                    i64::from(axis2),
                );
                CGEventSetLocation(event, CGPoint { x: at.x, y: at.y });
                tag_window(event, self.window_id);
                post_event(event, self.target_pid)?;
                CFRelease(event.cast_const());
            }
            Ok(())
        }

        fn key(
            &self,
            keycode: u16,
            down: bool,
            flags: u64,
            unicode: Option<&str>,
        ) -> Result<(), String> {
            unsafe {
                let event = CGEventCreateKeyboardEvent(std::ptr::null(), keycode, down);
                if event.is_null() {
                    return Err("CGEventCreateKeyboardEvent returned null".to_string());
                }
                CGEventSetFlags(event, flags);
                if let Some(text) = unicode {
                    let utf16: Vec<u16> = text.encode_utf16().collect();
                    CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
                }
                CGEventSetIntegerValueField(event, K_CG_MOUSE_EVENT_BUTTON_NUMBER, 0);
                post_event(event, self.target_pid)?;
                CFRelease(event.cast_const());
            }
            Ok(())
        }

        fn text(&self, s: &str) -> Result<(), String> {
            for (index, ch) in s.chars().enumerate() {
                // replay_key/replay_text checked immediately before the first
                // character; refresh between characters so focus cannot drift.
                if index > 0 {
                    self.verify_key_window(self.window_id)?;
                }
                post_unicode_char(ch, true, self.target_pid)?;
                post_unicode_char(ch, false, self.target_pid)?;
                thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        }
    }

    fn post_mouse(
        kind: MouseKind,
        x: f64,
        y: f64,
        button: u32,
        click_state: u32,
        target_pid: Option<i32>,
        window_id: u32,
        flags: u64,
    ) -> Result<(), String> {
        if click_state > 0 {
            log::debug!(
                "remote-control: posting mouse event kind={kind:?} point=({x:.1},{y:.1}) button={button} click_state={click_state}"
            );
        }
        let kind = mouse_kind_code(kind);
        unsafe {
            let event = CGEventCreateMouseEvent(std::ptr::null(), kind, CGPoint { x, y }, button);
            if event.is_null() {
                return Err("CGEventCreateMouseEvent returned null".to_string());
            }
            CGEventSetFlags(event, flags);
            CGEventSetIntegerValueField(event, K_CG_MOUSE_EVENT_BUTTON_NUMBER, button as i64);
            if click_state > 0 {
                CGEventSetIntegerValueField(
                    event,
                    K_CG_MOUSE_EVENT_CLICK_STATE,
                    i64::from(click_state),
                );
            }
            tag_window(event, window_id);
            post_event(event, target_pid)?;
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn mouse_kind_code(kind: MouseKind) -> u32 {
        match kind {
            MouseKind::LeftDown => K_CG_EVENT_LEFT_MOUSE_DOWN,
            MouseKind::LeftUp => K_CG_EVENT_LEFT_MOUSE_UP,
            MouseKind::RightDown => K_CG_EVENT_RIGHT_MOUSE_DOWN,
            MouseKind::RightUp => K_CG_EVENT_RIGHT_MOUSE_UP,
            MouseKind::Moved => K_CG_EVENT_MOUSE_MOVED,
            MouseKind::LeftDragged => K_CG_EVENT_LEFT_MOUSE_DRAGGED,
            MouseKind::RightDragged => K_CG_EVENT_RIGHT_MOUSE_DRAGGED,
            MouseKind::OtherDown => K_CG_EVENT_OTHER_MOUSE_DOWN,
            MouseKind::OtherUp => K_CG_EVENT_OTHER_MOUSE_UP,
            MouseKind::OtherDragged => K_CG_EVENT_OTHER_MOUSE_DRAGGED,
        }
    }

    unsafe fn tag_window(event: *mut c_void, window_id: u32) {
        CGEventSetIntegerValueField(
            event,
            K_CG_MOUSE_EVENT_WINDOW_UNDER_POINTER,
            window_id as i64,
        );
        CGEventSetIntegerValueField(
            event,
            K_CG_MOUSE_EVENT_WINDOW_UNDER_POINTER_CAN_HANDLE,
            window_id as i64,
        );
    }

    unsafe fn post_event(event: *mut c_void, target_pid: Option<i32>) -> Result<(), String> {
        if let Some(pid) = target_pid.filter(|pid| *pid > 0) {
            CGEventPostToPid(pid, event);
            Ok(())
        } else {
            Err("target pid is required for remote-control replay".to_string())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum KeyReplayPlan {
        VirtualKey {
            virtual_key: u16,
            unicode: Option<String>,
        },
        Text(String),
    }

    fn key_replay_plan(message: &RemoteControlMessage, key_down: bool) -> Option<KeyReplayPlan> {
        let key = message.key.as_deref().unwrap_or("");
        let code = message.code.as_deref().unwrap_or("");
        if let Some(text) = plain_text_for_key(message, key, key_down) {
            return Some(KeyReplayPlan::Text(text));
        }
        let keycode = keycode_for(code, key);
        let unicode = plain_unicode_for_key(message, key, key_down);
        match (keycode, unicode) {
            (Some(virtual_key), unicode) => Some(KeyReplayPlan::VirtualKey {
                virtual_key,
                unicode,
            }),
            (None, Some(unicode)) => Some(KeyReplayPlan::VirtualKey {
                virtual_key: 0,
                unicode: Some(unicode),
            }),
            (None, None) => None,
        }
    }

    fn plain_text_for_key(
        message: &RemoteControlMessage,
        key: &str,
        key_down: bool,
    ) -> Option<String> {
        if !key_down
            || message.repeat
            || message.modifiers.ctrl
            || message.modifiers.meta
            || message.modifiers.alt
        {
            return None;
        }
        single_printable_key(key)
    }

    fn plain_unicode_for_key(
        message: &RemoteControlMessage,
        key: &str,
        key_down: bool,
    ) -> Option<String> {
        if !key_down || message.modifiers.ctrl || message.modifiers.meta || message.modifiers.alt {
            return None;
        }
        single_printable_key(key)
    }

    fn single_printable_key(key: &str) -> Option<String> {
        let mut chars = key.chars();
        let ch = chars.next()?;
        if chars.next().is_some() || ch.is_control() {
            return None;
        }
        Some(key.to_string())
    }

    fn capped_replay_text(text: &str) -> String {
        truncate_text_to_limit(text)
    }

    fn cg_flags_for_modifiers(modifiers: &RemoteControlModifiers) -> u64 {
        let mut flags = 0;
        if modifiers.shift {
            flags |= K_CG_EVENT_FLAG_MASK_SHIFT;
        }
        if modifiers.ctrl {
            flags |= K_CG_EVENT_FLAG_MASK_CONTROL;
        }
        if modifiers.alt {
            flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
        }
        if modifiers.meta {
            flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
        }
        flags
    }

    fn wheel_delta_pixels(message: &RemoteControlMessage, frame: WindowFrame) -> (i32, i32) {
        let scale_x = wheel_delta_scale(message.delta_mode, frame.width);
        let scale_y = wheel_delta_scale(message.delta_mode, frame.height);
        (
            round_scroll_delta(message.delta_x.unwrap_or(0.0) * scale_x),
            round_scroll_delta(message.delta_y.unwrap_or(0.0) * scale_y),
        )
    }

    fn wheel_delta_scale(delta_mode: Option<u8>, page_size: i32) -> f64 {
        match delta_mode {
            Some(1) => LINE_SCROLL_PIXELS,
            Some(2) => page_size.max(1) as f64,
            _ => 1.0,
        }
    }

    fn round_scroll_delta(value: f64) -> i32 {
        if !value.is_finite() {
            return 0;
        }
        value.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }

    fn keycode_for(code: &str, key: &str) -> Option<u16> {
        keycode_from_code(code).or_else(|| keycode_from_key(key))
    }

    fn keycode_from_code(code: &str) -> Option<u16> {
        Some(match code {
            "KeyA" => 0,
            "KeyS" => 1,
            "KeyD" => 2,
            "KeyF" => 3,
            "KeyH" => 4,
            "KeyG" => 5,
            "KeyZ" => 6,
            "KeyX" => 7,
            "KeyC" => 8,
            "KeyV" => 9,
            "KeyB" => 11,
            "KeyQ" => 12,
            "KeyW" => 13,
            "KeyE" => 14,
            "KeyR" => 15,
            "KeyY" => 16,
            "KeyT" => 17,
            "Digit1" => 18,
            "Digit2" => 19,
            "Digit3" => 20,
            "Digit4" => 21,
            "Digit6" => 22,
            "Digit5" => 23,
            "Equal" => 24,
            "Digit9" => 25,
            "Digit7" => 26,
            "Minus" => 27,
            "Digit8" => 28,
            "Digit0" => 29,
            "BracketRight" => 30,
            "KeyO" => 31,
            "KeyU" => 32,
            "BracketLeft" => 33,
            "KeyI" => 34,
            "KeyP" => 35,
            "Enter" => 36,
            "KeyL" => 37,
            "KeyJ" => 38,
            "Quote" => 39,
            "KeyK" => 40,
            "Semicolon" => 41,
            "Backslash" | "IntlBackslash" => 42,
            "Comma" => 43,
            "Slash" => 44,
            "KeyN" => 45,
            "KeyM" => 46,
            "Period" => 47,
            "Tab" => 48,
            "Space" => 49,
            "Backquote" => 50,
            "Backspace" => 51,
            "Escape" => 53,
            "MetaRight" => 54,
            "MetaLeft" => 55,
            "ShiftLeft" => 56,
            "CapsLock" => 57,
            "AltLeft" => 58,
            "ControlLeft" => 59,
            "ShiftRight" => 60,
            "AltRight" => 61,
            "ControlRight" => 62,
            "Fn" => 63,
            "F17" => 64,
            "NumpadDecimal" => 65,
            "NumpadMultiply" => 67,
            "NumpadAdd" => 69,
            "NumpadClear" => 71,
            "NumpadDivide" => 75,
            "NumpadEnter" => 36,
            "NumpadSubtract" => 78,
            "F18" => 79,
            "F19" => 80,
            "NumpadEqual" => 81,
            "Numpad0" => 82,
            "Numpad1" => 83,
            "Numpad2" => 84,
            "Numpad3" => 85,
            "Numpad4" => 86,
            "Numpad5" => 87,
            "Numpad6" => 88,
            "Numpad7" => 89,
            "F20" => 90,
            "Numpad8" => 91,
            "Numpad9" => 92,
            "F5" => 96,
            "F6" => 97,
            "F7" => 98,
            "F3" => 99,
            "F8" => 100,
            "F9" => 101,
            "F11" => 103,
            "F13" => 105,
            "F16" => 106,
            "F14" => 107,
            "F10" => 109,
            "F12" => 111,
            "F15" => 113,
            "Help" | "Insert" => 114,
            "Home" => 115,
            "PageUp" => 116,
            "Delete" => 117,
            "F4" => 118,
            "End" => 119,
            "F2" => 120,
            "PageDown" => 121,
            "F1" => 122,
            "ArrowLeft" => 123,
            "ArrowRight" => 124,
            "ArrowDown" => 125,
            "ArrowUp" => 126,
            _ => return None,
        })
    }

    fn keycode_from_key(key: &str) -> Option<u16> {
        Some(match key {
            "\n" | "Enter" => 36,
            "\t" | "Tab" => 48,
            " " | "Space" | "Spacebar" => 49,
            "Backspace" => 51,
            "Escape" | "Esc" => 53,
            "Shift" => 56,
            "CapsLock" => 57,
            "Alt" | "Option" => 58,
            "Control" | "Ctrl" => 59,
            "Meta" | "OS" | "Command" | "Super" => 55,
            "ArrowLeft" | "Left" => 123,
            "ArrowRight" | "Right" => 124,
            "ArrowDown" | "Down" => 125,
            "ArrowUp" | "Up" => 126,
            "Delete" | "Del" => 117,
            "Home" => 115,
            "End" => 119,
            "PageUp" => 116,
            "PageDown" => 121,
            "Insert" | "Help" => 114,
            _ => {
                return key
                    .strip_prefix('F')?
                    .parse::<u8>()
                    .ok()
                    .and_then(f_keycode);
            }
        })
    }

    fn f_keycode(number: u8) -> Option<u16> {
        Some(match number {
            1 => 122,
            2 => 120,
            3 => 99,
            4 => 118,
            5 => 96,
            6 => 97,
            7 => 98,
            8 => 100,
            9 => 101,
            10 => 109,
            11 => 103,
            12 => 111,
            13 => 105,
            14 => 107,
            15 => 113,
            16 => 106,
            17 => 64,
            18 => 79,
            19 => 80,
            20 => 90,
            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::remote_control::MAX_REPLAY_TEXT_CHARS;
        use crate::remote_control::VERSION;
        use std::collections::HashMap;
        use std::sync::Mutex;

        /// #779 live identity gate. Unlike the RecordingAxBackend tests below,
        /// this reads real AXWindow elements and the real CG-backed registry.
        /// It requires two visible sibling windows from one application so the
        /// same production predicate proves both the positive identity and the
        /// same-pid unauthorized-sibling refusal.
        #[test]
        fn real_ax_window_identity_accepts_exact_window_and_refuses_same_app_sibling() {
            const TEST_NAME: &str = "rc-real-ax-window-identity";
            if std::env::var("PETAL_RUN_REAL_AX_WINDOW_IDENTITY_TEST").as_deref() != Ok("1") {
                eprintln!(
                    "SKIP[{TEST_NAME}]: set PETAL_RUN_REAL_AX_WINDOW_IDENTITY_TEST=1 to exercise real macOS AX windows"
                );
                return;
            }
            // Opting OUT (the env check above) is a choice. Opting IN and
            // then finding the run unusable is a FAILURE, never a skip: every
            // precondition from here down panics. Four early returns used to
            // live here, and `ci-local.sh` runs `cargo test --lib` without
            // `--nocapture`, so the harness swallowed their SKIP lines
            // entirely -- the guard on a shipped P0 could not have gone red
            // for any reason. `scripts/verify-rc-window-identity.sh` supplies
            // the fixture and translates the two environment panics below back
            // into HARNESS INVALID (exit 3), so a missing grant is never read
            // as "the #779 fix regressed".
            assert!(
                accessibility_trusted(),
                "[{TEST_NAME}] this test process has no macOS Accessibility grant, so the guard cannot run at all"
            );
            assert!(
                crate::platform::ax::get_window_symbol_available(),
                "[{TEST_NAME}] _AXUIElementGetWindow is unavailable, so the production primary-path assertion cannot run"
            );

            let registry = match crate::window_registry::global() {
                Some(registry) => registry,
                None => {
                    crate::window_registry::set_global(
                        crate::window_registry::WindowRegistry::new(),
                    );
                    crate::window_registry::global()
                        .expect("the live identity test must install the global window registry")
                }
            };
            let snapshot = registry.refresh_now();
            assert!(
                !snapshot.by_id.is_empty(),
                "[{TEST_NAME}] CoreGraphics returned no on-screen windows (no display, or no Screen Recording grant)"
            );

            let mut ids_by_pid: HashMap<i32, HashSet<u32>> = HashMap::new();
            for record in snapshot.records_front_to_back() {
                if record.owner_pid > 0 && record.layer == 0 && record.is_real {
                    ids_by_pid
                        .entry(record.owner_pid)
                        .or_default()
                        .insert(record.wid);
                }
            }

            // Every qualifying application is exercised, not just the
            // first: returning on the first success meant a pass could be
            // entirely attributable to some unrelated app that happened to be
            // open, so the guard would still pass with its own fixture absent.
            // `scripts/verify-rc-window-identity.sh` greps the PASS lines for
            // the fixture's pid and calls the run INVALID if it is missing.
            // The `continue`s below are fixture SELECTION (an app that serves
            // no real AXWindow pair, or none with a descendant, is not a
            // usable fixture); every path past selection asserts.
            let mut exercised = 0usize;
            for (pid, sibling_ids) in ids_by_pid
                .into_iter()
                .filter(|(_, sibling_ids)| sibling_ids.len() >= 2)
            {
                let Some(app) = ax_app_element_for_pid(pid) else {
                    continue;
                };
                let Ok(list) = copy_attribute(&app, ax_windows_attribute().as_ptr()) else {
                    continue;
                };
                let mut windows = Vec::new();
                push_ax_element_array(&list, &mut windows);
                // Establish the precondition WITHOUT the function under test.
                // An earlier revision selected fixtures by calling
                // `ax_element_window_id` and keeping only ids found in
                // `sibling_ids`; a broken resolver then produced no qualifying
                // fixtures and the test SKIPPED instead of failing -- verified by
                // mutation (`wid.wrapping_add(1)` turned PASS into SKIP, which
                // counts as ok). That is the same "green regardless" pathology
                // that let #779 ship. Role is read directly from AX here, so the
                // candidate set never depends on the identity resolver.
                let ax_window_elements: Vec<_> = windows
                    .into_iter()
                    .filter(|window| {
                        copy_attribute(window, ax_role_attribute().as_ptr())
                            .ok()
                            .and_then(|role| cf_string_to_string(role.as_ptr()))
                            .is_some_and(|role| role == "AXWindow")
                    })
                    .collect();
                if ax_window_elements.len() < 2 {
                    // Genuinely not a usable fixture app (e.g. Finder serves its
                    // desktop as AXScrollArea, not AXWindow).
                    continue;
                }

                // Precondition met independently: this pid has >= 2 on-screen CG
                // windows AND >= 2 real AX window elements. From here the
                // resolver MUST work -- anything less is a failure, never a skip.
                // An AXWindow may legitimately be off-screen/minimised/on another
                // Space, and `sibling_ids` holds only ON-SCREEN CG ids, so an id
                // outside that set is not per se an error. What IS an error is
                // failing to land at least two elements INSIDE it: the resolver
                // is being asked about an app that demonstrably has >= 2 on-screen
                // windows. A broken resolver scores zero matches here and FAILS,
                // instead of quietly finding no fixtures and skipping.
                let ax_window_count = ax_window_elements.len();
                let mut resolved = Vec::new();
                for window in ax_window_elements {
                    let Ok(wid) = ax_element_window_id(&window, pid) else {
                        continue;
                    };
                    if sibling_ids.contains(&wid)
                        && !resolved
                            .iter()
                            .any(|(_, resolved_wid)| *resolved_wid == wid)
                    {
                        resolved.push((window, wid));
                    }
                }
                assert!(
                    resolved.len() >= 2,
                    "[{TEST_NAME}] pid {pid} has {} on-screen CG windows {sibling_ids:?} and served \
                     {ax_window_count} real AXWindow elements, but the identity resolver mapped only \
                     {} of them into that id space. It cannot tell sibling windows apart -- exactly \
                     the #779 defect. (This assertion must FAIL, never skip, when the resolver breaks.)",
                    sibling_ids.len(),
                    resolved.len()
                );

                // Prefer a real descendant so the forced fallback proves it
                // ascends to AXWindow before reading the correlation frame.
                let mut descendant_case = None;
                for (index, (window, _)) in resolved.iter().enumerate() {
                    let Ok(children) = copy_attribute(window, ax_children_attribute().as_ptr())
                    else {
                        continue;
                    };
                    let mut descendants = Vec::new();
                    push_ax_element_array(&children, &mut descendants);
                    if let Some(descendant) = descendants.into_iter().next() {
                        descendant_case = Some((index, descendant));
                        break;
                    }
                }
                let Some((authorized_index, descendant)) = descendant_case else {
                    continue;
                };
                let sibling_index = if authorized_index == 0 { 1 } else { 0 };
                let (authorized_element, authorized_wid) = &resolved[authorized_index];
                let unauthorized_sibling_wid = resolved[sibling_index].1;
                assert!(
                    element_belongs_to_window(authorized_element, pid, *authorized_wid)
                        .expect("real AX identity helper must resolve the authorized window"),
                    "the real AX window must accept its exact CGWindowID"
                );
                assert!(
                    !element_belongs_to_window(
                        authorized_element,
                        pid,
                        unauthorized_sibling_wid,
                    )
                    .expect("real AX identity helper must resolve the sibling comparison"),
                    "input for one real window must be refused for unauthorized same-app sibling {unauthorized_sibling_wid}"
                );

                let fresh = registry.refresh_now();
                let same_pid_frames: HashMap<u32, (f64, f64, f64, f64)> = fresh
                    .by_id
                    .values()
                    .filter(|window| window.owner_pid == pid)
                    .map(|window| (window.wid, (window.rx, window.ry, window.rw, window.rh)))
                    .collect();
                let all_same_pid_frames = all_window_frames_for_pid(pid)
                    .expect("OptionAll must include the real AX fixture windows");
                let descendant_ptr = descendant
                    .as_real_ptr()
                    .expect("the real AX descendant must carry a real element pointer");
                // SAFETY: `descendant` retains this real AXUIElement for the
                // duration of the forced production-fallback call.
                let fallback_wid = unsafe {
                    crate::platform::ax::resolve_element_window_id_via_frame_fallback(
                        descendant_ptr,
                        &crate::platform::ax::CandidateFrames(same_pid_frames),
                        &crate::platform::ax::UniverseFrames(all_same_pid_frames),
                    )
                }
                .expect("frame fallback must ascend the descendant and resolve its real window");
                assert_eq!(
                    fallback_wid, *authorized_wid,
                    "descendant frame fallback must resolve the exact authorized window"
                );
                assert_ne!(
                    fallback_wid, unauthorized_sibling_wid,
                    "descendant frame fallback must refuse the unauthorized same-app sibling"
                );
                eprintln!(
                    "PASS[{TEST_NAME}]: pid={pid} authorized={authorized_wid} unauthorized_sibling={unauthorized_sibling_wid} fallback=descendant"
                );
                exercised += 1;
            }

            assert!(
                exercised > 0,
                "[{TEST_NAME}] no Accessibility-readable application with two visible sibling windows and a \
                 real AX descendant was available. Opted in with nothing to exercise is a FAILURE, not a skip -- \
                 run scripts/verify-rc-window-identity.sh, which launches scripts/probes/twowin.m as the fixture."
            );
        }

        // #777: `verify_key_window` recovers a focus mismatch with ONE bare
        // AXRaise of the authorized window. These pin the decision itself; the
        // AX behaviour the decision relies on is only checkable live.
        #[test]
        fn raise_is_attempted_when_the_authorized_window_is_not_focused_and_the_app_is_background()
        {
            // Confirmed mismatch, and the sharer is not in the app -- recover.
            assert!(should_attempt_key_window_raise(Some(false), false));
            // The focus check itself errored: unknown is still not "confirmed
            // focused", so a background app is still eligible for recovery.
            assert!(should_attempt_key_window_raise(None, false));
        }

        #[test]
        fn raise_is_refused_while_the_target_app_is_frontmost() {
            // The sharer is actively in this app; raising would move THEIR focus
            // and send their next keystrokes into the broadcast window.
            assert!(!should_attempt_key_window_raise(Some(false), true));
            assert!(!should_attempt_key_window_raise(None, true));
        }

        #[test]
        fn no_raise_when_the_authorized_window_is_already_focused() {
            assert!(!should_attempt_key_window_raise(Some(true), false));
            assert!(!should_attempt_key_window_raise(Some(true), true));
        }

        #[test]
        fn text_window_search_skips_unidentifiable_candidate_before_legitimate_window() {
            let candidates = [1_u64, 2_u64];
            let mut identity_visits = Vec::new();
            let mut text_visits = Vec::new();

            let resolved = find_text_element_in_window_candidates(
                &candidates,
                42,
                |candidate| {
                    identity_visits.push(*candidate);
                    if *candidate == 1 {
                        Err(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE))
                    } else {
                        Ok(42)
                    }
                },
                |candidate| {
                    text_visits.push(*candidate);
                    Ok((*candidate == 2).then_some(99_u64))
                },
            )
            .expect("a non-window candidate must not abort the remaining search");

            assert_eq!(resolved, Some(99));
            assert_eq!(identity_visits, vec![1, 2]);
            assert_eq!(text_visits, vec![2]);
        }

        #[derive(Debug, Clone, PartialEq)]
        enum SynthEvent {
            Mouse {
                kind: MouseKind,
                x: f64,
                y: f64,
                button: RemoteControlButton,
                click_state: u32,
                flags: u64,
            },
            Scroll {
                axis1: i32,
                axis2: i32,
                x: f64,
                y: f64,
                unit: ScrollUnit,
                flags: u64,
            },
            Key {
                keycode: u16,
                down: bool,
                flags: u64,
                unicode: Option<String>,
            },
            Text {
                s: String,
            },
        }

        #[derive(Debug, Default)]
        struct RecordingSink {
            events: Mutex<Vec<SynthEvent>>,
            focused_window_id: Mutex<Option<u32>>,
            /// Stuck-modifier fix: the window this sink was authorized for, modelling
            /// `CGEventSink::window_id`. `None` (the default) means the sink
            /// has no notion of one, so the local identity check is permissive
            /// and every pre-existing test is unaffected.
            authorized_window_id: Mutex<Option<u32>>,
        }

        impl RecordingSink {
            fn events(&self) -> Vec<SynthEvent> {
                self.events.lock_unpoisoned().clone()
            }

            fn focus_window(&self, window_id: u32) {
                *self.focused_window_id.lock_unpoisoned() = Some(window_id);
            }

            fn authorize_window(&self, window_id: u32) {
                *self.authorized_window_id.lock_unpoisoned() = Some(window_id);
            }
        }

        impl InputSink for RecordingSink {
            fn verify_key_window(&self, window_id: u32) -> Result<(), String> {
                match *self.focused_window_id.lock_unpoisoned() {
                    Some(focused) if focused != window_id => Err(window_identity_error(window_id)),
                    _ => Ok(()),
                }
            }

            fn verify_key_window_sink_identity(&self, window_id: u32) -> Result<(), String> {
                match *self.authorized_window_id.lock_unpoisoned() {
                    Some(authorized) if authorized != window_id => {
                        Err(window_identity_error(window_id))
                    }
                    _ => Ok(()),
                }
            }

            fn mouse(
                &self,
                kind: MouseKind,
                at: super::super::GlobalPoint,
                button: RemoteControlButton,
                click_state: u32,
                flags: u64,
            ) -> Result<(), String> {
                self.events.lock_unpoisoned().push(SynthEvent::Mouse {
                    kind,
                    x: at.x,
                    y: at.y,
                    button,
                    click_state,
                    flags,
                });
                Ok(())
            }

            fn scroll(
                &self,
                axis1: i32,
                axis2: i32,
                at: super::super::GlobalPoint,
                unit: ScrollUnit,
                flags: u64,
            ) -> Result<(), String> {
                self.events.lock_unpoisoned().push(SynthEvent::Scroll {
                    axis1,
                    axis2,
                    x: at.x,
                    y: at.y,
                    unit,
                    flags,
                });
                Ok(())
            }

            fn key(
                &self,
                keycode: u16,
                down: bool,
                flags: u64,
                unicode: Option<&str>,
            ) -> Result<(), String> {
                self.events.lock_unpoisoned().push(SynthEvent::Key {
                    keycode,
                    down,
                    flags,
                    unicode: unicode.map(ToString::to_string),
                });
                Ok(())
            }

            fn text(&self, s: &str) -> Result<(), String> {
                self.events
                    .lock_unpoisoned()
                    .push(SynthEvent::Text { s: s.to_string() });
                Ok(())
            }
        }

        #[derive(Debug, Clone, PartialEq)]
        enum AxOp {
            Press(u64),
            ShowMenu(u64),
            SetSelectedRange {
                id: u64,
                start: i64,
                len: i64,
            },
            SetSelectedText {
                id: u64,
                text: String,
            },
            Scroll {
                id: u64,
                delta_y: f64,
                delta_x: f64,
                value_y: f64,
                value_x: f64,
            },
        }

        #[derive(Debug, Clone, Copy)]
        struct RecordingScrollState {
            value_y: f64,
            extent_y: Option<f64>,
            value_x: f64,
            extent_x: Option<f64>,
        }

        #[derive(Debug, Default)]
        struct RecordingAxBackend {
            resolved: Mutex<Option<AxElementHandle>>,
            resolve_error: Mutex<Option<AxError>>,
            // #170: the element returned by resolve_text_element (window-scoped
            // text resolution), configured independently of the pointer hit-test
            // `resolved` so tests can model "no editable text element found".
            text_resolved: Mutex<Option<AxElementHandle>>,
            text_resolve_error: Mutex<Option<AxError>>,
            element_window_ids: Mutex<HashMap<u64, u32>>,
            // F5: provenance reported alongside the resolved text element.
            // Defaults to FocusedElement (trusted) so existing happy-path tests
            // still exercise the destructive-op AX path.
            text_source: Mutex<TextElementSource>,
            text_lengths: Mutex<HashMap<u64, i64>>,
            selected_texts: Mutex<HashMap<u64, String>>,
            capabilities: Mutex<HashMap<u64, AxCapabilities>>,
            offsets: Mutex<HashMap<(u64, i64, i64), Result<i64, AxError>>>,
            scroll: Mutex<Option<RecordingScrollState>>,
            // F4: inject a failure for the AX act calls (set_selected_range /
            // set_selected_text) to model a stale-element race between resolve
            // and act.
            act_error: Mutex<Option<AxError>>,
            // #368 F2: successive hit-test results, popped one per resolve_at
            // call, so a test can model the element under the cursor changing
            // between the cached attempt and the fresh retry. Empty => fall
            // back to `resolved`.
            resolve_sequence: Mutex<Vec<AxElementHandle>>,
            // #368 F2: element ids whose `press` returns kAXErrorInvalidUIElement,
            // modelling a cached element that was destroyed before the press.
            press_error_ids: Mutex<std::collections::HashSet<u64>>,
            ops: Mutex<Vec<AxOp>>,
        }

        #[derive(Debug, Clone, PartialEq)]
        struct SlClick {
            pid: i32,
            x: f64,
            y: f64,
            button: RemoteControlButton,
            click_state: u32,
        }

        /// #446 test double for the session-tap route. Records every posted
        /// event so a test can assert on the REAL gesture the handler chain
        /// produced -- including the cursor warp in and the restore out --
        /// rather than on a pure helper in isolation.
        #[derive(Debug, PartialEq, Clone, Copy)]
        enum TapEvent {
            Raise(i32, u32),
            Mouse(MouseKind, i64, i64),
            Scroll(i64, i64, i32, i32),
            MoveCursor(i64, i64),
        }

        /// A pid for the fake occluder that is guaranteed to be neither the
        /// test process (which the hit test skips as "our own overlay") nor
        /// the fake target's 1234. Hardcoding 5678 would silently disarm the
        /// occlusion tests on the one run where the harness happens to own
        /// that pid.
        fn foreign_test_pid() -> i32 {
            let self_pid = std::process::id() as i32;
            let mut pid = 5678;
            while pid == self_pid || pid == 1234 {
                pid += 1;
            }
            pid
        }

        struct RecordingSessionTap {
            trusted: bool,
            /// Where `cursor_position` reports the host's pointer to be. Tests
            /// mutate this to simulate the host grabbing the mouse mid-gesture.
            cursor: Mutex<super::super::GlobalPoint>,
            /// When true, `cursor_position` follows our own posts, i.e. nobody
            /// else touched the mouse.
            cursor_follows_posts: bool,
            events: Mutex<Vec<TapEvent>>,
            fail_posts: bool,
            /// #599: what the real `raise` returns when AXRaise/AXFrontmost
            /// both fail on the target -- i.e. the tier cannot make the
            /// window hit-testable, so nothing it posts can be delivered.
            raise_ok: bool,
            /// AXWindows order for the target pid. The first entry may be a
            /// sibling; raise must select the requested identity (#759).
            windows: Vec<u32>,
            /// #599: the on-screen window stack the pre-post hit test reads,
            /// front-to-back. `None` = "could not be read", which must never
            /// be treated as a failure.
            stack: Option<Vec<StackWindow>>,
        }

        impl RecordingSessionTap {
            fn trusted() -> Self {
                Self {
                    trusted: true,
                    cursor: Mutex::new(super::super::GlobalPoint { x: 11.0, y: 22.0 }),
                    cursor_follows_posts: true,
                    events: Mutex::new(Vec::new()),
                    fail_posts: false,
                    raise_ok: true,
                    windows: vec![42],
                    stack: None,
                }
            }

            fn with_windows(windows: Vec<u32>) -> Self {
                Self {
                    windows,
                    ..Self::trusted()
                }
            }

            /// #599: Accessibility is granted, the raise SUCCEEDS, and the
            /// target really is on screen -- but another process's window
            /// (a `.floating` panel, exactly like the acceptance suite's
            /// occluder) sits in front of it at the target coordinate. This is
            /// the case the raise boolean cannot see.
            fn occluded_by_foreign_window() -> Self {
                Self {
                    stack: Some(vec![
                        StackWindow {
                            window_id: 9001,
                            owner_pid: foreign_test_pid(),
                            // `.floating` -- what the acceptance occluder uses.
                            layer: 3,
                            alpha: 1.0,
                            x: 0.0,
                            y: 0.0,
                            w: 200.0,
                            h: 200.0,
                        },
                        StackWindow {
                            window_id: 42,
                            owner_pid: 1234,
                            layer: 0,
                            alpha: 1.0,
                            x: 0.0,
                            y: 0.0,
                            w: 100.0,
                            h: 100.0,
                        },
                    ]),
                    ..Self::trusted()
                }
            }

            /// The same stack with the occluder BEHIND the target: the raise
            /// worked and nothing foreign is in front, so the gesture must
            /// proceed normally. Without this control, a passing occluded test
            /// proves nothing -- an always-nack would satisfy it too.
            fn frontmost_over_foreign_window() -> Self {
                Self {
                    stack: Some(vec![
                        StackWindow {
                            window_id: 42,
                            owner_pid: 1234,
                            layer: 0,
                            alpha: 1.0,
                            x: 0.0,
                            y: 0.0,
                            w: 100.0,
                            h: 100.0,
                        },
                        StackWindow {
                            window_id: 9001,
                            owner_pid: foreign_test_pid(),
                            // `.floating` -- what the acceptance occluder uses.
                            layer: 3,
                            alpha: 1.0,
                            x: 0.0,
                            y: 0.0,
                            w: 200.0,
                            h: 200.0,
                        },
                    ]),
                    ..Self::trusted()
                }
            }

            /// #759: the target is frontmost and the raise succeeds, but it has
            /// MOVED since the control-frame cache was last refreshed -- its
            /// live bounds are at 500,500 while the caller is still mapping the
            /// controller's normalized coordinates into the cached 0,0 frame.
            /// An unshared sibling of the same app now occupies 0,0. The
            /// sibling is BEHIND the target, so the occlusion loop has nothing
            /// to report and the coordinate post would land in the sibling.
            fn target_moved_off_the_cached_frame() -> Self {
                Self {
                    stack: Some(vec![
                        StackWindow {
                            window_id: 42,
                            owner_pid: 1234,
                            layer: 0,
                            alpha: 1.0,
                            x: 500.0,
                            y: 500.0,
                            w: 100.0,
                            h: 100.0,
                        },
                        StackWindow {
                            window_id: 43,
                            owner_pid: 1234,
                            layer: 0,
                            alpha: 1.0,
                            x: 0.0,
                            y: 0.0,
                            w: 100.0,
                            h: 100.0,
                        },
                    ]),
                    ..Self::trusted()
                }
            }

            /// The control for [`Self::target_moved_off_the_cached_frame`]:
            /// same two windows, same z-order, but the target's live bounds
            /// still agree with the cached frame. The gesture must proceed --
            /// an always-nack would satisfy the moved-target test on its own.
            fn sibling_behind_an_unmoved_target() -> Self {
                Self {
                    stack: Some(vec![
                        StackWindow {
                            window_id: 42,
                            owner_pid: 1234,
                            layer: 0,
                            alpha: 1.0,
                            x: 0.0,
                            y: 0.0,
                            w: 100.0,
                            h: 100.0,
                        },
                        StackWindow {
                            window_id: 43,
                            owner_pid: 1234,
                            layer: 0,
                            alpha: 1.0,
                            x: 500.0,
                            y: 500.0,
                            w: 100.0,
                            h: 100.0,
                        },
                    ]),
                    ..Self::trusted()
                }
            }

            fn untrusted() -> Self {
                Self {
                    trusted: false,
                    ..Self::trusted()
                }
            }

            /// #599: Accessibility IS granted and the route is otherwise
            /// healthy, but the target cannot be raised to the front.
            fn unraisable() -> Self {
                Self {
                    raise_ok: false,
                    ..Self::trusted()
                }
            }

            /// Simulate a present host user: the cursor does NOT stay where we
            /// put it, so the restore must be skipped.
            fn with_host_moving_cursor() -> Self {
                Self {
                    cursor_follows_posts: false,
                    ..Self::trusted()
                }
            }

            fn events(&self) -> Vec<TapEvent> {
                self.events.lock_unpoisoned().clone()
            }

            fn mouse_kinds(&self) -> Vec<MouseKind> {
                self.events()
                    .into_iter()
                    .filter_map(|event| match event {
                        TapEvent::Mouse(kind, _, _) => Some(kind),
                        _ => None,
                    })
                    .collect()
            }

            fn record(&self, event: TapEvent) {
                self.events.lock_unpoisoned().push(event);
            }

            fn track_cursor(&self, point: super::super::GlobalPoint) {
                if self.cursor_follows_posts {
                    *self.cursor.lock_unpoisoned() = point;
                }
            }
        }

        fn q(value: f64) -> i64 {
            (value * 100.0).round() as i64
        }

        impl SessionTapBackend for RecordingSessionTap {
            fn post_mouse(
                &self,
                point: super::super::GlobalPoint,
                _button: RemoteControlButton,
                kind: MouseKind,
                _click_state: u32,
            ) -> Result<(), String> {
                if self.fail_posts {
                    return Err("post failed".to_string());
                }
                self.record(TapEvent::Mouse(kind, q(point.x), q(point.y)));
                self.track_cursor(point);
                Ok(())
            }

            fn post_scroll(
                &self,
                point: super::super::GlobalPoint,
                delta_y: i32,
                delta_x: i32,
                _flags: u64,
            ) -> Result<(), String> {
                if self.fail_posts {
                    return Err("post failed".to_string());
                }
                self.record(TapEvent::Scroll(q(point.x), q(point.y), delta_y, delta_x));
                self.track_cursor(point);
                Ok(())
            }

            fn cursor_position(&self) -> Option<super::super::GlobalPoint> {
                Some(*self.cursor.lock_unpoisoned())
            }

            fn move_cursor(&self, point: super::super::GlobalPoint) -> Result<(), String> {
                if self.fail_posts {
                    return Err("post failed".to_string());
                }
                self.record(TapEvent::MoveCursor(q(point.x), q(point.y)));
                self.track_cursor(point);
                Ok(())
            }

            fn raise(&self, pid: i32, window_id: u32) -> bool {
                self.record(TapEvent::Raise(pid, window_id));
                self.raise_ok && self.windows.contains(&window_id)
            }

            fn onscreen_stack(&self) -> Option<Vec<StackWindow>> {
                self.stack.clone()
            }

            fn is_trusted(&self) -> bool {
                self.trusted
            }
        }

        #[derive(Debug, Default)]
        struct RecordingSlClickBackend {
            available: bool,
            clicks: Mutex<Vec<SlClick>>,
            events: Mutex<Vec<SlMouseEvent>>,
            scrolls: Mutex<usize>,
            fail_up_attempts: Mutex<usize>,
        }

        impl RecordingSlClickBackend {
            fn unavailable() -> Self {
                Self {
                    available: false,
                    clicks: Mutex::new(Vec::new()),
                    events: Mutex::new(Vec::new()),
                    scrolls: Mutex::new(0),
                    fail_up_attempts: Mutex::new(0),
                }
            }

            fn available() -> Self {
                Self {
                    available: true,
                    clicks: Mutex::new(Vec::new()),
                    events: Mutex::new(Vec::new()),
                    scrolls: Mutex::new(0),
                    fail_up_attempts: Mutex::new(0),
                }
            }

            fn clicks(&self) -> Vec<SlClick> {
                self.clicks.lock_unpoisoned().clone()
            }

            fn events(&self) -> Vec<SlMouseEvent> {
                self.events.lock_unpoisoned().clone()
            }

            fn scroll_count(&self) -> usize {
                *self.scrolls.lock_unpoisoned()
            }

            fn fail_next_up_attempts(&self, count: usize) {
                *self.fail_up_attempts.lock_unpoisoned() = count;
            }
        }

        impl RecordingAxBackend {
            fn element(id: u64) -> AxElementHandle {
                AxElementHandle::Test(id)
            }

            fn resolve_to(&self, id: u64) {
                *self.resolved.lock_unpoisoned() = Some(Self::element(id));
            }

            fn fail_resolution_with(&self, error: AxError) {
                *self.resolve_error.lock_unpoisoned() = Some(error);
            }

            fn resolve_text_to(&self, id: u64) {
                *self.text_resolved.lock_unpoisoned() = Some(Self::element(id));
                *self.text_source.lock_unpoisoned() = TextElementSource::FocusedElement;
            }

            fn fail_text_resolution_with(&self, error: AxError) {
                *self.text_resolve_error.lock_unpoisoned() = Some(error);
            }

            fn place_element_in_window(&self, id: u64, window_id: u32) {
                self.element_window_ids
                    .lock_unpoisoned()
                    .insert(id, window_id);
            }

            // F5: model a text element found only via the BFS-shallowest fallback
            // (untrusted for destructive shortcuts).
            fn resolve_text_via_bfs_to(&self, id: u64) {
                *self.text_resolved.lock_unpoisoned() = Some(Self::element(id));
                *self.text_source.lock_unpoisoned() = TextElementSource::BfsFallback;
            }

            // F4: make subsequent AX act calls fail with the given error.
            fn fail_acts_with(&self, error: AxError) {
                *self.act_error.lock_unpoisoned() = Some(error);
            }

            // #368 F2: return these element ids from successive resolve_at calls
            // so a test can model the element under the cursor changing between
            // the cached attempt and the fresh retry.
            fn resolve_sequence(&self, ids: &[u64]) {
                *self.resolve_sequence.lock_unpoisoned() =
                    ids.iter().map(|id| Self::element(*id)).collect();
            }

            // #368 F2: make press on this element id fail as a stale element.
            fn fail_press_for(&self, id: u64) {
                self.press_error_ids.lock_unpoisoned().insert(id);
            }

            fn set_text_length(&self, id: u64, len: i64) {
                self.text_lengths.lock_unpoisoned().insert(id, len);
            }

            fn set_selected_text_value(&self, id: u64, text: &str) {
                self.selected_texts
                    .lock_unpoisoned()
                    .insert(id, text.to_string());
            }

            fn set_capabilities(&self, id: u64, capabilities: AxCapabilities) {
                self.capabilities.lock_unpoisoned().insert(id, capabilities);
            }

            fn set_offset(&self, id: u64, x: f64, y: f64, offset: i64) {
                self.offsets
                    .lock_unpoisoned()
                    .insert((id, x.round() as i64, y.round() as i64), Ok(offset));
            }

            fn set_scroll(&self, state: RecordingScrollState) {
                *self.scroll.lock_unpoisoned() = Some(state);
            }

            fn ops(&self) -> Vec<AxOp> {
                self.ops.lock_unpoisoned().clone()
            }
        }

        impl AxInputBackend for RecordingAxBackend {
            fn resolve_at(
                &self,
                _pid: i32,
                window_id: u32,
                _point: super::super::GlobalPoint,
            ) -> Result<Option<AxElementHandle>, AxError> {
                if let Some(error) = *self.resolve_error.lock_unpoisoned() {
                    return Err(error);
                }
                let mut sequence = self.resolve_sequence.lock_unpoisoned();
                let resolved = if !sequence.is_empty() {
                    Some(sequence.remove(0))
                } else {
                    self.resolved.lock_unpoisoned().clone()
                };
                if let Some(id) = resolved.as_ref().and_then(AxElementHandle::test_id) {
                    if self
                        .element_window_ids
                        .lock_unpoisoned()
                        .get(&id)
                        .is_some_and(|candidate| *candidate != window_id)
                    {
                        return Err(AxError::new(K_AX_ERROR_WINDOW_ID_MISMATCH));
                    }
                }
                Ok(resolved)
            }

            fn resolve_text_element(
                &self,
                _pid: i32,
                window_id: u32,
            ) -> Result<Option<(AxElementHandle, TextElementSource)>, AxError> {
                if let Some(error) = *self.text_resolve_error.lock_unpoisoned() {
                    return Err(error);
                }
                let source = *self.text_source.lock_unpoisoned();
                let resolved = self.text_resolved.lock_unpoisoned().clone();
                if let Some(id) = resolved.as_ref().and_then(AxElementHandle::test_id) {
                    if self
                        .element_window_ids
                        .lock_unpoisoned()
                        .get(&id)
                        .is_some_and(|candidate| *candidate != window_id)
                    {
                        return Err(AxError::new(K_AX_ERROR_WINDOW_ID_MISMATCH));
                    }
                }
                Ok(resolved.map(|element| (element, source)))
            }

            fn text_length(&self, element: &AxElementHandle) -> Result<i64, AxError> {
                let id = element.test_id().unwrap();
                self.text_lengths
                    .lock_unpoisoned()
                    .get(&id)
                    .copied()
                    .ok_or_else(|| AxError::new(K_AX_ERROR_NO_VALUE))
            }

            fn selected_text(&self, element: &AxElementHandle) -> Result<Option<String>, AxError> {
                let id = element.test_id().unwrap();
                Ok(self.selected_texts.lock_unpoisoned().get(&id).cloned())
            }

            fn set_selected_text(
                &self,
                element: &AxElementHandle,
                text: &str,
            ) -> Result<(), AxError> {
                if let Some(error) = *self.act_error.lock_unpoisoned() {
                    return Err(error);
                }
                let id = element.test_id().unwrap();
                self.ops.lock_unpoisoned().push(AxOp::SetSelectedText {
                    id,
                    text: text.to_string(),
                });
                // Model the app: the pasted text becomes the current selection.
                self.selected_texts
                    .lock_unpoisoned()
                    .insert(id, text.to_string());
                Ok(())
            }

            fn capabilities(&self, element: &AxElementHandle) -> AxCapabilities {
                element
                    .test_id()
                    .and_then(|id| self.capabilities.lock_unpoisoned().get(&id).copied())
                    .unwrap_or_default()
            }

            fn press(&self, element: &AxElementHandle) -> Result<(), AxError> {
                let id = element.test_id().unwrap();
                if self.press_error_ids.lock_unpoisoned().contains(&id) {
                    // Modelled stale element: no op recorded, press did nothing.
                    return Err(AxError::new(K_AX_ERROR_INVALID_UI_ELEMENT));
                }
                self.ops.lock_unpoisoned().push(AxOp::Press(id));
                Ok(())
            }

            fn show_menu(&self, element: &AxElementHandle) -> Result<(), AxError> {
                let id = element.test_id().unwrap();
                self.ops.lock_unpoisoned().push(AxOp::ShowMenu(id));
                Ok(())
            }

            fn offset_at_point(
                &self,
                element: &AxElementHandle,
                point: super::super::GlobalPoint,
            ) -> Result<i64, AxError> {
                let id = element.test_id().unwrap();
                self.offsets
                    .lock_unpoisoned()
                    .get(&(id, point.x.round() as i64, point.y.round() as i64))
                    .copied()
                    .unwrap_or(Ok(0))
            }

            fn set_selected_range(
                &self,
                element: &AxElementHandle,
                start: i64,
                len: i64,
            ) -> Result<(), AxError> {
                if let Some(error) = *self.act_error.lock_unpoisoned() {
                    return Err(error);
                }
                let id = element.test_id().unwrap();
                self.ops
                    .lock_unpoisoned()
                    .push(AxOp::SetSelectedRange { id, start, len });
                Ok(())
            }

            fn scroll_by(
                &self,
                _window_id: u32,
                _point: super::super::GlobalPoint,
                element: &AxElementHandle,
                delta_px_y: f64,
                delta_px_x: f64,
            ) -> Result<bool, AxError> {
                let id = element.test_id().unwrap();
                let mut guard = self.scroll.lock_unpoisoned();
                let Some(mut state) = *guard else {
                    return Ok(false);
                };
                let old_y = state.value_y;
                let old_x = state.value_x;
                state.value_y =
                    scrollbar_value_after_delta(state.value_y, delta_px_y, state.extent_y);
                state.value_x =
                    scrollbar_value_after_delta(state.value_x, delta_px_x, state.extent_x);
                let changed = state.value_y != old_y || state.value_x != old_x;
                *guard = Some(state);
                if !changed {
                    return Ok(false);
                }
                self.ops.lock_unpoisoned().push(AxOp::Scroll {
                    id,
                    delta_y: delta_px_y,
                    delta_x: delta_px_x,
                    value_y: state.value_y,
                    value_x: state.value_x,
                });
                Ok(true)
            }
        }

        impl SlClickBackend for RecordingSlClickBackend {
            fn post_click(
                &self,
                pid: i32,
                point: super::super::GlobalPoint,
                button: RemoteControlButton,
                click_state: u32,
            ) -> Result<(), SlClickError> {
                if !self.available {
                    return Err(SlClickError::Unavailable);
                }
                self.clicks.lock_unpoisoned().push(SlClick {
                    pid,
                    x: point.x,
                    y: point.y,
                    button,
                    click_state,
                });
                Ok(())
            }

            fn post_mouse_event(
                &self,
                _pid: i32,
                _point: super::super::GlobalPoint,
                _button: RemoteControlButton,
                event: SlMouseEvent,
            ) -> Result<(), SlClickError> {
                if !self.available {
                    return Err(SlClickError::Unavailable);
                }
                self.events.lock_unpoisoned().push(event);
                if event == SlMouseEvent::Up {
                    let mut failures = self.fail_up_attempts.lock_unpoisoned();
                    if *failures > 0 {
                        *failures -= 1;
                        return Err(SlClickError::Failed(
                            "test SkyLight release drop".to_string(),
                        ));
                    }
                }
                Ok(())
            }

            fn post_scroll(
                &self,
                _pid: i32,
                _point: super::super::GlobalPoint,
                _delta_y: i32,
                _delta_x: i32,
                _flags: u64,
            ) -> Result<(), SlClickError> {
                if self.available {
                    *self.scrolls.lock_unpoisoned() += 1;
                    Ok(())
                } else {
                    Err(SlClickError::Unavailable)
                }
            }
        }

        #[derive(Debug, Default)]
        struct RecordingPasteboard {
            contents: Mutex<Option<String>>,
        }

        impl RecordingPasteboard {
            fn with_text(text: &str) -> Self {
                Self {
                    contents: Mutex::new(Some(text.to_string())),
                }
            }

            fn contents(&self) -> Option<String> {
                self.contents.lock_unpoisoned().clone()
            }
        }

        impl PasteboardBackend for RecordingPasteboard {
            fn read_text(&self) -> Option<String> {
                self.contents.lock_unpoisoned().clone()
            }

            fn write_text(&self, text: &str) {
                *self.contents.lock_unpoisoned() = Some(text.to_string());
            }
        }

        fn base_message(message_type: RemoteControlType) -> RemoteControlMessage {
            RemoteControlMessage {
                v: VERSION,
                message_type,
                action: None,
                target_user_id: "host".to_string(),
                controller_id: "viewer".to_string(),
                window_id: 42,
                seq: 1,
                target_kind: None,
                share_instance_id: None,
                controller_capabilities: Vec::new(),
                host_capabilities: Vec::new(),
                reason: None,
                control_session_id: None,
                input_id: None,
                input_seq: None,
                operation_fingerprint_version: None,
                operation_fingerprint: None,
                outcome: None,
                delivery_route: None,
                failure_code: None,
                result_capability: None,
                x: None,
                y: None,
                button: None,
                buttons: None,
                click_count: None,
                delta_x: None,
                delta_y: None,
                delta_mode: None,
                key: None,
                code: None,
                repeat: false,
                location: None,
                text: None,
                status: None,
                message: None,
                grant_token: None,
                supports_binary_hot_path: false,
                modifiers: RemoteControlModifiers::default(),
            }
        }

        fn key_message(
            code: &str,
            key: &str,
            modifiers: RemoteControlModifiers,
        ) -> RemoteControlMessage {
            let mut message = base_message(RemoteControlType::Key);
            message.action = Some(RemoteControlAction::Down);
            message.code = Some(code.to_string());
            message.key = Some(key.to_string());
            message.modifiers = modifiers;
            message
        }

        fn wheel_message(
            delta_x: f64,
            delta_y: f64,
            delta_mode: Option<u8>,
        ) -> RemoteControlMessage {
            let mut message = base_message(RemoteControlType::Wheel);
            message.x = Some(0.5);
            message.y = Some(0.5);
            message.delta_x = Some(delta_x);
            message.delta_y = Some(delta_y);
            message.delta_mode = delta_mode;
            message
        }

        fn pointer_message(
            action: RemoteControlAction,
            x: f64,
            y: f64,
            button: RemoteControlButton,
            buttons: u16,
        ) -> RemoteControlMessage {
            let mut message = base_message(RemoteControlType::Pointer);
            message.action = Some(action);
            message.x = Some(x);
            message.y = Some(y);
            message.button = Some(match button {
                RemoteControlButton::Left => 0,
                RemoteControlButton::Middle => 1,
                RemoteControlButton::Right => 2,
            });
            message.buttons = Some(buttons);
            message
        }

        fn with_click_count(
            mut message: RemoteControlMessage,
            click_count: u32,
        ) -> RemoteControlMessage {
            message.click_count = Some(click_count);
            message
        }

        fn replay_events(message: &RemoteControlMessage, frame: WindowFrame) -> Vec<SynthEvent> {
            let sink = RecordingSink::default();
            replay_to_sink(message, frame, &sink).unwrap();
            sink.events()
        }

        fn replay_events_with_ax(
            message: &RemoteControlMessage,
            frame: WindowFrame,
            ax: &RecordingAxBackend,
        ) -> Vec<SynthEvent> {
            let sl = RecordingSlClickBackend::unavailable();
            replay_events_with_backends(message, frame, ax, &sl)
        }

        fn replay_events_with_backends(
            message: &RemoteControlMessage,
            frame: WindowFrame,
            ax: &RecordingAxBackend,
            sl: &RecordingSlClickBackend,
        ) -> Vec<SynthEvent> {
            let pb = RecordingPasteboard::default();
            let sink = RecordingSink::default();
            // #446: these helpers exist to observe the CGEvent SINK, so the
            // session-tap route is deliberately unavailable here -- otherwise
            // it would service the message and the sink would stay empty.
            let tap = RecordingSessionTap::untrusted();
            let _ = replay_with_backends(message, frame, Some(1234), &sink, ax, sl, &pb, &tap);
            sink.events()
        }

        // #170: key-shortcut tests need to observe/seed the pasteboard, so they
        // drive replay with an explicit pasteboard mock (SL clicks unavailable).
        fn replay_events_with_pasteboard(
            message: &RemoteControlMessage,
            frame: WindowFrame,
            ax: &RecordingAxBackend,
            pb: &RecordingPasteboard,
        ) -> Vec<SynthEvent> {
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let tap = RecordingSessionTap::untrusted();
            replay_with_backends(message, frame, Some(1234), &sink, ax, &sl, pb, &tap).unwrap();
            sink.events()
        }

        fn unit_frame() -> WindowFrame {
            WindowFrame {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }
        }

        #[test]
        fn ax_click_on_pressable_element_performs_press_and_clears_gesture() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(1);
            ax.set_capabilities(
                1,
                AxCapabilities {
                    pressable: true,
                    ..AxCapabilities::default()
                },
            );

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            assert!(replay_events_with_ax(&down, frame, &ax).is_empty());
            assert!(ax_pointer_gestures()
                .lock_unpoisoned()
                .contains_key(&(down.window_id, down.controller_id.clone())));

            let up = pointer_message(
                RemoteControlAction::Up,
                0.11,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            assert!(replay_events_with_ax(&up, frame, &ax).is_empty());

            assert_eq!(ax.ops(), vec![AxOp::Press(1)]);
            assert!(!ax_pointer_gestures()
                .lock_unpoisoned()
                .contains_key(&(down.window_id, down.controller_id.clone())));
        }

        #[test]
        fn semantic_click_uses_ax_authority_without_creating_held_gesture_state() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(11);
            ax.set_capabilities(
                11,
                AxCapabilities {
                    pressable: true,
                    ..AxCapabilities::default()
                },
            );
            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.20,
                RemoteControlButton::Left,
                0,
            );

            assert!(replay_events_with_ax(&click, frame, &ax).is_empty());
            assert_eq!(ax.ops(), vec![AxOp::Press(11)]);
            assert_eq!(ax_gesture_count_for_tests(), 0);
        }

        #[test]
        fn semantic_click_at_case29_button_coordinates_uses_ax_not_session_tap() {
            // #820 (case 29 "reconnect during control preserves grant",
            // remote-control-scenario.mjs): the harness used to re-send
            // `api.click({x:0.6,y:0.5})` against the sentinel's real 960x600
            // content frame (offset 120,262 -- exactly as logged live,
            // /tmp/petal-dev-rc.log run 11) expecting the session-tap/CGEvent
            // route (a real `leftMouseDown` NSEvent). At that literal point
            // the sentinel's real "REMOTE CLICK" AppKit button IS pressable
            // (remote-control-photon-sentinel.swift's `clickButton` content
            // rect is 560,270,300,150; remote-control-gestures.mjs's
            // `axButtonCenter` documents the same region). A pressable
            // target is AX-authoritative by design (see
            // semantic_click_uses_ax_authority_without_creating_held_gesture_state
            // above) and never reaches the session tap, so it can never
            // produce a leftMouseDown -- the click IS delivered (AxOp::Press),
            // just not via the route the test happened to assert on. This
            // was misdiagnosed twice as "the session-tap pointer injector
            // does not survive the SDK resume" before this coordinate
            // collision was found; this test pins the real mechanism with
            // the exact real-world geometry so it cannot recur silently.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = WindowFrame {
                x: 120,
                y: 262,
                width: 960,
                height: 600,
            };
            let ax = RecordingAxBackend::default();
            ax.resolve_to(1);
            ax.set_capabilities(
                1,
                AxCapabilities {
                    pressable: true,
                    ..AxCapabilities::default()
                },
            );
            let click = pointer_message(
                RemoteControlAction::Click,
                0.6,
                0.5,
                RemoteControlButton::Left,
                0,
            );

            assert!(
                replay_events_with_ax(&click, frame, &ax).is_empty(),
                "AX route must not touch the CGEvent/session-tap sink"
            );
            assert_eq!(ax.ops(), vec![AxOp::Press(1)]);
            assert_eq!(ax_gesture_count_for_tests(), 0);
        }

        #[test]
        fn stale_cached_press_reresolves_fresh_instead_of_swallowing() {
            // #368 F2: when the element served for a click turns out to be stale
            // at press time, the click must re-resolve and press the live
            // element, not be silently dropped. The cursor points at element 1
            // (stale) and then element 2 (live) on the fresh retry.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_sequence(&[1, 2]);
            ax.fail_press_for(1);
            for id in [1, 2] {
                ax.set_capabilities(
                    id,
                    AxCapabilities {
                        pressable: true,
                        ..AxCapabilities::default()
                    },
                );
            }
            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            assert!(replay_events_with_ax(&click, frame, &ax).is_empty());
            // Only the fresh element (2) was pressed; the stale press on 1
            // recorded no op and the click was NOT swallowed.
            assert_eq!(ax.ops(), vec![AxOp::Press(2)]);
        }

        #[test]
        fn message_mutates_ui_flags_every_ui_changing_replay() {
            // #368 F1 (P1/P2): the central invalidation must fire for every
            // message that can change the UI under the cursor — including
            // held-button drags and synthetic (passthrough) scrolls — but not
            // for buttonless hover moves or a key release.
            assert!(!message_mutates_ui(&pointer_message(
                RemoteControlAction::Move,
                0.1,
                0.2,
                RemoteControlButton::Left,
                0,
            )));
            assert!(message_mutates_ui(&pointer_message(
                RemoteControlAction::Move,
                0.1,
                0.2,
                RemoteControlButton::Left,
                1,
            )));
            for action in [
                RemoteControlAction::Down,
                RemoteControlAction::Up,
                RemoteControlAction::Click,
            ] {
                assert!(message_mutates_ui(&pointer_message(
                    action,
                    0.1,
                    0.2,
                    RemoteControlButton::Left,
                    0,
                )));
            }
            assert!(message_mutates_ui(&wheel_message(0.0, 4.0, None)));
            let mut key = key_message("KeyA", "a", RemoteControlModifiers::default());
            assert!(message_mutates_ui(&key)); // Down mutates
            key.action = Some(RemoteControlAction::Up);
            assert!(!message_mutates_ui(&key)); // Up does not
        }

        #[test]
        fn ax_click_on_text_element_sets_zero_length_range_at_up_offset() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(2);
            ax.set_capabilities(
                2,
                AxCapabilities {
                    text_selectable: true,
                    ..AxCapabilities::default()
                },
            );
            ax.set_offset(2, 10.0, 20.0, 4);
            ax.set_offset(2, 12.0, 20.0, 5);

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.12,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            assert!(replay_events_with_ax(&down, frame, &ax).is_empty());
            assert!(replay_events_with_ax(&up, frame, &ax).is_empty());

            assert_eq!(
                ax.ops(),
                vec![AxOp::SetSelectedRange {
                    id: 2,
                    start: 5,
                    len: 0,
                }]
            );
        }

        // ---- #170: AX-direct clipboard / select-all key shortcuts ----

        fn meta_only() -> RemoteControlModifiers {
            RemoteControlModifiers {
                meta: true,
                ..RemoteControlModifiers::default()
            }
        }

        fn text_selectable_caps() -> AxCapabilities {
            AxCapabilities {
                text_selectable: true,
                ..AxCapabilities::default()
            }
        }

        #[test]
        fn classify_text_shortcut_matches_only_bare_cmd_letters() {
            assert_eq!(
                classify_text_shortcut(&key_message("KeyA", "a", meta_only())),
                Some(TextShortcut::SelectAll)
            );
            assert_eq!(
                classify_text_shortcut(&key_message("KeyC", "c", meta_only())),
                Some(TextShortcut::Copy)
            );
            assert_eq!(
                classify_text_shortcut(&key_message("KeyV", "v", meta_only())),
                Some(TextShortcut::Paste)
            );
            // Cmd+Shift+A / Ctrl+A / Alt+A are NOT select-all here.
            for extra in [
                RemoteControlModifiers {
                    meta: true,
                    shift: true,
                    ..RemoteControlModifiers::default()
                },
                RemoteControlModifiers {
                    meta: true,
                    ctrl: true,
                    ..RemoteControlModifiers::default()
                },
                RemoteControlModifiers {
                    meta: true,
                    alt: true,
                    ..RemoteControlModifiers::default()
                },
            ] {
                assert_eq!(
                    classify_text_shortcut(&key_message("KeyA", "a", extra)),
                    None
                );
            }
            // Plain A (no Cmd), Cmd+B, and other letters are ignored.
            assert_eq!(
                classify_text_shortcut(&key_message(
                    "KeyA",
                    "a",
                    RemoteControlModifiers::default()
                )),
                None
            );
            assert_eq!(
                classify_text_shortcut(&key_message("KeyB", "b", meta_only())),
                None
            );
        }

        #[test]
        fn classify_text_shortcut_falls_back_to_logical_key_without_code() {
            // A layout that reports no physical `code` but a logical key still maps.
            let mut message = key_message("", "a", meta_only());
            message.code = None;
            assert_eq!(
                classify_text_shortcut(&message),
                Some(TextShortcut::SelectAll)
            );
        }

        #[test]
        fn classify_text_shortcut_prefers_logical_key_over_physical_code() {
            // F8: macOS key-equivalents are LOGICAL-character based. On non-US
            // layouts the physical `code` no longer implies the logical letter, so
            // matching the code first hijacked unrelated shortcuts.
            //
            // AZERTY: physical KeyA is logical 'q'. Remote Cmd+Q must NOT be
            // misread as select-all.
            assert_eq!(
                classify_text_shortcut(&key_message("KeyA", "q", meta_only())),
                None
            );
            // Dvorak: physical KeyC is logical 'j'. Remote Cmd+J must NOT be
            // misread as copy.
            assert_eq!(
                classify_text_shortcut(&key_message("KeyC", "j", meta_only())),
                None
            );
            // And the ACTUAL logical letters still classify, even when the
            // physical code belongs to a different key (AZERTY logical 'a' sits on
            // physical KeyQ; logical 'c'/'v' likewise).
            assert_eq!(
                classify_text_shortcut(&key_message("KeyQ", "a", meta_only())),
                Some(TextShortcut::SelectAll)
            );
            assert_eq!(
                classify_text_shortcut(&key_message("KeyJ", "c", meta_only())),
                Some(TextShortcut::Copy)
            );
            assert_eq!(
                classify_text_shortcut(&key_message("KeyDot", "v", meta_only())),
                Some(TextShortcut::Paste)
            );
        }

        #[test]
        fn cf_number_to_i64_rejects_non_cfnumber_types() {
            // F1: a real CFNumber round-trips; a non-CFNumber CFTypeRef (here a
            // CFString) is rejected up front instead of being handed to
            // CFNumberGetValue (which on a hostile third-party AX value risks an
            // unrecognized-selector NSException -> abort). Mirrors the is_cf_string
            // guard already used for string attributes.
            let n: i64 = 4242;
            let number = CfObject::from_create(unsafe {
                CFNumberCreate(
                    std::ptr::null(),
                    K_CF_NUMBER_SINT64,
                    &n as *const i64 as *const c_void,
                )
            })
            .expect("CFNumberCreate returned null");
            assert_eq!(cf_number_to_i64(number.as_ptr()), Ok(4242));

            let string = cf_string_from_str("not-a-number").expect("CFString create");
            assert!(
                cf_number_to_i64(string.as_ptr()).is_err(),
                "a CFString must not be read as a CFNumber"
            );

            // A null CFTypeRef is likewise rejected by the guard.
            assert!(cf_number_to_i64(std::ptr::null()).is_err());
        }

        #[test]
        fn select_all_range_spans_whole_document_and_clamps_negative() {
            assert_eq!(select_all_range(8), (0, 8));
            assert_eq!(select_all_range(0), (0, 0));
            // A negative length (bogus AXNumberOfCharacters) must never produce a
            // negative span passed to AXSelectedTextRange.
            assert_eq!(select_all_range(-3), (0, 0));
        }

        #[test]
        fn cmd_a_selects_full_text_via_ax_and_suppresses_cgevent() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(30);
            ax.set_capabilities(30, text_selectable_caps());
            ax.set_text_length(30, 17);
            let pb = RecordingPasteboard::default();

            let message = key_message("KeyA", "a", meta_only());
            // AX handled it, so NO fallback CGEvent keystroke is posted.
            assert!(replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert_eq!(
                ax.ops(),
                vec![AxOp::SetSelectedRange {
                    id: 30,
                    start: 0,
                    len: 17,
                }]
            );
        }

        #[test]
        fn cmd_a_falls_back_to_cgevent_when_no_text_element_resolves() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            // No resolve_text_to(): window-scoped resolution finds nothing.
            let ax = RecordingAxBackend::default();
            let pb = RecordingPasteboard::default();

            let message = key_message("KeyA", "a", meta_only());
            let events = replay_events_with_pasteboard(&message, frame, &ax, &pb);
            // Falls through to the CGEvent Cmd+A keystroke.
            assert!(!events.is_empty());
            assert!(ax.ops().is_empty());
        }

        #[test]
        fn cmd_a_skips_ax_when_resolved_element_not_text_selectable() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(31);
            // Default capabilities: text_selectable == false.
            let pb = RecordingPasteboard::default();

            let message = key_message("KeyA", "a", meta_only());
            assert!(!replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert!(ax.ops().is_empty());
        }

        #[test]
        fn cmd_c_copies_ax_selection_to_pasteboard() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(32);
            ax.set_capabilities(32, text_selectable_caps());
            ax.set_selected_text_value(32, "copy-me-42");
            let pb = RecordingPasteboard::with_text("stale-clipboard");

            let message = key_message("KeyC", "c", meta_only());
            // Handled via AX: no CGEvent Cmd+C, and the pasteboard now holds the
            // real AXSelectedText (the case 10/13 assertion).
            assert!(replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert_eq!(pb.contents(), Some("copy-me-42".to_string()));
        }

        #[test]
        fn cmd_c_with_empty_selection_leaves_clipboard_and_falls_back() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(33);
            ax.set_capabilities(33, text_selectable_caps());
            // No selected text configured -> selected_text() returns Ok(None).
            let pb = RecordingPasteboard::with_text("keep-me");

            let message = key_message("KeyC", "c", meta_only());
            let events = replay_events_with_pasteboard(&message, frame, &ax, &pb);
            assert!(!events.is_empty());
            // Clipboard untouched; we never clobber it with an empty selection.
            assert_eq!(pb.contents(), Some("keep-me".to_string()));
        }

        #[test]
        fn cmd_v_inserts_clipboard_via_ax_selected_text() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(34);
            ax.set_capabilities(34, text_selectable_caps());
            let pb = RecordingPasteboard::with_text("pasted-99");

            let message = key_message("KeyV", "v", meta_only());
            assert!(replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert_eq!(
                ax.ops(),
                vec![AxOp::SetSelectedText {
                    id: 34,
                    text: "pasted-99".to_string(),
                }]
            );
        }

        #[test]
        fn cmd_v_with_empty_clipboard_falls_back_to_cgevent() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(35);
            ax.set_capabilities(35, text_selectable_caps());
            let pb = RecordingPasteboard::default(); // empty clipboard

            let message = key_message("KeyV", "v", meta_only());
            assert!(!replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert!(ax.ops().is_empty());
        }

        #[test]
        fn cmd_a_act_stale_element_passes_through_to_cgevent() {
            // F4: the resolved element can go stale between resolve and act
            // (invalid_ui_element). The Key path must return PassThrough — NOT
            // Handled — so the CGEvent Cmd+A key-equivalent still fires (it works
            // when the target app is frontmost on the host; suppressing it was a
            // regression).
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(38);
            ax.set_capabilities(38, text_selectable_caps());
            ax.set_text_length(38, 12);
            ax.fail_acts_with(AxError::new(K_AX_ERROR_INVALID_UI_ELEMENT));
            let pb = RecordingPasteboard::default();

            let message = key_message("KeyA", "a", meta_only());
            let outcome = replay_key_via_ax(&message, Some(1234), &ax, &pb).unwrap();
            assert_eq!(outcome, AxReplayOutcome::PassThrough);

            // End-to-end: the CGEvent fallback keystroke IS posted, and no AX op
            // landed (the set_selected_range errored out before recording).
            assert!(!replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert!(ax.ops().is_empty());
        }

        #[test]
        fn cmd_v_via_bfs_fallback_passes_through_to_cgevent() {
            // F5: a destructive paste must NOT act on a BFS-fallback element (it
            // can be the wrong field, e.g. a browser URL bar above the document).
            // It must pass through to CGEvent instead of setting AXSelectedText.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_via_bfs_to(40);
            ax.set_capabilities(40, text_selectable_caps());
            let pb = RecordingPasteboard::with_text("dangerous-paste");

            let message = key_message("KeyV", "v", meta_only());
            let outcome = replay_key_via_ax(&message, Some(1234), &ax, &pb).unwrap();
            assert_eq!(outcome, AxReplayOutcome::PassThrough);

            assert!(!replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert!(
                ax.ops().is_empty(),
                "must not set AXSelectedText on a BFS-fallback element"
            );
        }

        #[test]
        fn cmd_a_via_bfs_fallback_passes_through_to_cgevent() {
            // F5: select-all is destructive (the next keystroke replaces the
            // selection), so it also refuses a BFS-fallback element.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_via_bfs_to(41);
            ax.set_capabilities(41, text_selectable_caps());
            ax.set_text_length(41, 9);
            let pb = RecordingPasteboard::default();

            let message = key_message("KeyA", "a", meta_only());
            let outcome = replay_key_via_ax(&message, Some(1234), &ax, &pb).unwrap();
            assert_eq!(outcome, AxReplayOutcome::PassThrough);
            assert!(ax.ops().is_empty());
        }

        #[test]
        fn cmd_c_via_bfs_fallback_still_copies() {
            // F5: copy is read-only, so it MAY use a BFS-fallback element — the
            // guard only gates the destructive shortcuts.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_via_bfs_to(42);
            ax.set_capabilities(42, text_selectable_caps());
            ax.set_selected_text_value(42, "copied-via-bfs");
            let pb = RecordingPasteboard::with_text("stale");

            let message = key_message("KeyC", "c", meta_only());
            assert!(replay_events_with_pasteboard(&message, frame, &ax, &pb).is_empty());
            assert_eq!(pb.contents(), Some("copied-via-bfs".to_string()));
        }

        #[test]
        fn key_up_and_repeat_do_not_apply_ax_shortcut() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(36);
            ax.set_capabilities(36, text_selectable_caps());
            ax.set_text_length(36, 5);
            let pb = RecordingPasteboard::default();

            // Key-up of Cmd+A must not re-select.
            let mut up = key_message("KeyA", "a", meta_only());
            up.action = Some(RemoteControlAction::Up);
            let _ = replay_events_with_pasteboard(&up, frame, &ax, &pb);
            assert!(ax.ops().is_empty(), "key-up must not drive AX select-all");

            // Auto-repeat of Cmd+A must not re-apply either.
            let mut repeat = key_message("KeyA", "a", meta_only());
            repeat.repeat = true;
            let _ = replay_events_with_pasteboard(&repeat, frame, &ax, &pb);
            assert!(
                ax.ops().is_empty(),
                "auto-repeat must not drive AX select-all"
            );
        }

        #[test]
        fn cmd_a_then_cmd_c_round_trips_selection_to_clipboard() {
            // Mirrors live cases 10/13: select-all then copy leaves the clipboard
            // equal to the full document. (The mock pre-seeds AXSelectedText as the
            // full doc; on a real NSTextView the select-all sets that selection.)
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_text_to(37);
            ax.set_capabilities(37, text_selectable_caps());
            ax.set_text_length(37, 8);
            ax.set_selected_text_value(37, "full-doc");
            let pb = RecordingPasteboard::with_text("not-the-selection");

            let select_all = key_message("KeyA", "a", meta_only());
            assert!(replay_events_with_pasteboard(&select_all, frame, &ax, &pb).is_empty());

            let copy = key_message("KeyC", "c", meta_only());
            assert!(replay_events_with_pasteboard(&copy, frame, &ax, &pb).is_empty());

            assert_eq!(pb.contents(), Some("full-doc".to_string()));
            assert_eq!(
                ax.ops(),
                vec![AxOp::SetSelectedRange {
                    id: 37,
                    start: 0,
                    len: 8,
                }]
            );
        }

        #[test]
        fn concurrent_authorize_does_not_clear_other_controllers_ax_gesture() {
            // #374: a second controller's Request on the SAME window used to
            // displace and clear the first controller's parked AX gesture
            // (`authorize_exclusive`). Under concurrent/shared authorization,
            // granting a NEW controller must leave an already-in-progress
            // controller's drag anchor completely untouched.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let window_id = 424_242;
            let old_controller = "ax-concurrent-old";
            let new_controller = "ax-concurrent-new";
            super::super::revoke(window_id, old_controller);
            super::super::revoke(window_id, new_controller);
            super::super::authorize_shared(window_id, old_controller);

            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(20);
            ax.set_capabilities(
                20,
                AxCapabilities {
                    text_selectable: true,
                    ..AxCapabilities::default()
                },
            );
            ax.set_offset(20, 10.0, 20.0, 4);
            ax.set_offset(20, 12.0, 20.0, 5);

            let mut down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            down.window_id = window_id;
            down.controller_id = old_controller.to_string();
            assert!(replay_events_with_ax(&down, frame, &ax).is_empty());
            assert!(ax_pointer_gestures()
                .lock_unpoisoned()
                .contains_key(&(window_id, old_controller.to_string())));

            // A second, different controller requesting control of the same
            // window ADDS a concurrent grant instead of displacing the first.
            super::super::authorize_shared(window_id, new_controller);
            assert!(ax_pointer_gestures()
                .lock_unpoisoned()
                .contains_key(&(window_id, old_controller.to_string())));

            let mut old_up = pointer_message(
                RemoteControlAction::Up,
                0.12,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            old_up.window_id = window_id;
            old_up.controller_id = old_controller.to_string();
            let _ = replay_events_with_ax(&old_up, frame, &ax);
            // The old controller's gesture survived concurrent authorization
            // of the new controller, so its own Up resolves against the real
            // parked anchor/offset (a genuine AX action), not a no-op the way
            // a displaced controller's synthetic release used to be. The
            // down->up displacement here (2.0pt) is below the click/drag
            // threshold, so this is a click-style caret placement at the up
            // offset rather than a range selection.
            assert_eq!(
                ax.ops(),
                vec![AxOp::SetSelectedRange {
                    id: 20,
                    start: 5,
                    len: 0,
                }]
            );

            super::super::revoke(window_id, old_controller);
            super::super::revoke(window_id, new_controller);
        }

        #[test]
        fn ax_text_drag_sets_selection_range_in_both_directions() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(3);
            ax.set_capabilities(
                3,
                AxCapabilities {
                    text_selectable: true,
                    ..AxCapabilities::default()
                },
            );
            ax.set_offset(3, 20.0, 20.0, 2);
            ax.set_offset(3, 80.0, 20.0, 11);

            let down = pointer_message(
                RemoteControlAction::Down,
                0.20,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.80,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            replay_events_with_ax(&down, frame, &ax);
            replay_events_with_ax(&up, frame, &ax);

            let down = pointer_message(
                RemoteControlAction::Down,
                0.80,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            replay_events_with_ax(&down, frame, &ax);
            replay_events_with_ax(&up, frame, &ax);

            assert_eq!(
                ax.ops(),
                vec![
                    AxOp::SetSelectedRange {
                        id: 3,
                        start: 2,
                        len: 9,
                    },
                    AxOp::SetSelectedRange {
                        id: 3,
                        start: 2,
                        len: 9,
                    },
                ]
            );
        }

        #[test]
        fn concurrent_drags_keyed_per_controller_do_not_clobber() {
            // #374: two different controllers dragging inside the SAME
            // window at the same time must each keep their own parked
            // gesture/anchor. Controller B starting and finishing its own
            // drag while controller A's drag is still parked must not read
            // or clear A's anchor, and vice versa.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(9);
            ax.set_capabilities(
                9,
                AxCapabilities {
                    text_selectable: true,
                    ..AxCapabilities::default()
                },
            );
            ax.set_offset(9, 10.0, 20.0, 1);
            ax.set_offset(9, 90.0, 20.0, 9);
            ax.set_offset(9, 30.0, 20.0, 3);
            ax.set_offset(9, 70.0, 20.0, 7);

            let controller_a = "concurrent-drag-a";
            let controller_b = "concurrent-drag-b";

            let mut down_a = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            down_a.controller_id = controller_a.to_string();
            assert!(replay_events_with_ax(&down_a, frame, &ax).is_empty());

            // B starts its OWN drag on the same window while A's drag is
            // still parked -- this must not clobber or read A's anchor.
            let mut down_b = pointer_message(
                RemoteControlAction::Down,
                0.30,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            down_b.controller_id = controller_b.to_string();
            assert!(replay_events_with_ax(&down_b, frame, &ax).is_empty());

            assert_eq!(ax_gesture_count_for_tests(), 2);

            // B finishes its own drag; A's parked gesture must survive.
            let mut up_b = pointer_message(
                RemoteControlAction::Up,
                0.70,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            up_b.controller_id = controller_b.to_string();
            let _ = replay_events_with_ax(&up_b, frame, &ax);

            assert_eq!(ax_gesture_count_for_tests(), 1);
            assert!(ax_pointer_gestures()
                .lock_unpoisoned()
                .contains_key(&(down_a.window_id, controller_a.to_string())));

            // A finishes its own drag against its OWN anchor, unaffected by
            // B's already-completed drag.
            let mut up_a = pointer_message(
                RemoteControlAction::Up,
                0.90,
                0.20,
                RemoteControlButton::Left,
                0,
            );
            up_a.controller_id = controller_a.to_string();
            let _ = replay_events_with_ax(&up_a, frame, &ax);

            assert_eq!(
                ax.ops(),
                vec![
                    AxOp::SetSelectedRange {
                        id: 9,
                        start: 3,
                        len: 4,
                    },
                    AxOp::SetSelectedRange {
                        id: 9,
                        start: 1,
                        len: 8,
                    },
                ]
            );
            assert_eq!(ax_gesture_count_for_tests(), 0);
        }

        #[test]
        fn ax_drag_on_pressable_element_still_performs_press_best_effort() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(4);
            ax.set_capabilities(
                4,
                AxCapabilities {
                    pressable: true,
                    ..AxCapabilities::default()
                },
            );

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.60,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.60,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            assert!(replay_events_with_ax(&down, frame, &ax).is_empty());
            assert!(replay_events_with_ax(&drag, frame, &ax).is_empty());
            assert!(replay_events_with_ax(&up, frame, &ax).is_empty());

            assert_eq!(ax.ops(), vec![AxOp::Press(4)]);
        }

        #[test]
        fn ax_right_click_on_show_menu_element_only_shows_menu_below_drag_threshold() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(9);
            ax.set_capabilities(
                9,
                AxCapabilities {
                    show_menu: true,
                    ..AxCapabilities::default()
                },
            );
            let sl = RecordingSlClickBackend::unavailable();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Right,
                2,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.13,
                0.10,
                RemoteControlButton::Right,
                0,
            );
            assert!(replay_events_with_backends(&down, frame, &ax, &sl).is_empty());
            assert!(replay_events_with_backends(&up, frame, &ax, &sl).is_empty());

            assert_eq!(ax.ops(), vec![AxOp::ShowMenu(9)]);

            clear_all_ax_control_state();
            ax.ops.lock_unpoisoned().clear();
            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Right,
                2,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.14,
                0.10,
                RemoteControlButton::Right,
                2,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.14,
                0.10,
                RemoteControlButton::Right,
                0,
            );

            let mut events = Vec::new();
            events.extend(replay_events_with_backends(&down, frame, &ax, &sl));
            events.extend(replay_events_with_backends(&drag, frame, &ax, &sl));
            events.extend(replay_events_with_backends(&up, frame, &ax, &sl));

            assert_eq!(ax.ops(), Vec::new());
            assert!(events.is_empty());
        }

        #[test]
        fn ax_pointer_without_capability_reports_injection_failure() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(5);
            ax.set_capabilities(5, AxCapabilities::default());

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.20,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            let mut events = Vec::new();
            events.extend(replay_events_with_ax(&down, frame, &ax));
            events.extend(replay_events_with_ax(&drag, frame, &ax));
            events.extend(replay_events_with_ax(&up, frame, &ax));

            assert!(events.is_empty());
        }

        #[test]
        fn ax_scroll_consumes_pre_negation_wire_delta_for_scrollbar_value() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(6);
            ax.set_scroll(RecordingScrollState {
                value_y: 0.40,
                extent_y: Some(100.0),
                value_x: 0.0,
                extent_x: Some(100.0),
            });

            let message = wheel_message(0.0, 50.0, None);
            assert!(replay_events_with_ax(&message, frame, &ax).is_empty());

            assert_eq!(
                ax.ops(),
                vec![AxOp::Scroll {
                    id: 6,
                    delta_y: 50.0,
                    delta_x: 0.0,
                    value_y: 0.90,
                    value_x: 0.0,
                }]
            );
        }

        #[test]
        fn ax_scroll_without_a_scrollbar_reports_injection_failure() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(6);

            let events = replay_events_with_ax(&wheel_message(0.0, 50.0, None), frame, &ax);

            assert!(ax.ops().is_empty());
            assert!(events.is_empty());
        }

        /// #446: the three direct (SkyLight) pointer routes must stay opt-in.
        ///
        /// This is the durable record of a live validation that FAILED, so the
        /// next session does not re-run it blind. On 2026-07-27 (macOS 26.5.2
        /// arm64, web-harness controller -> native host -> an AppKit target
        /// that logs every `NSWindow.sendEvent:`), running with all three
        /// `PETAL_REMOTE_CONTROL_DIRECT_*` vars set to `1` delivered ZERO mouse
        /// NSEvents to the target for the v2 semantic click, the legacy raw
        /// Down/Up click, the legacy Down/Move/Up drag, and the wheel -- while
        /// keyboard and Cmd+V in the same run landed normally. The only
        /// behavioral difference from the default was that the honest
        /// `pointer or wheel injection exhausted AX/SkyLight routes` failure
        /// stopped being reported. Defaulting these on would trade a correct
        /// error for a silent no-op.
        ///
        /// If you are here to flip a default: get a NEW measurement showing a
        /// real delivered NSEvent in a real target app first, and put it in
        /// this comment.
        #[test]
        fn direct_pointer_routes_stay_opt_in_after_the_446_live_pass() {
            // An unset variable must never enable a route.
            assert!(!direct_route_enabled(
                "PETAL_REMOTE_CONTROL_DIRECT_ROUTE_THAT_IS_NEVER_SET"
            ));
            // Only the literal "1" opts in; the three public switches delegate
            // to that same helper, so this pins the wiring as well as the
            // default.
            for var in [
                "PETAL_REMOTE_CONTROL_DIRECT_CLICK",
                "PETAL_REMOTE_CONTROL_DIRECT_DRAG",
                "PETAL_REMOTE_CONTROL_DIRECT_SCROLL",
            ] {
                let opted_in = std::env::var(var).as_deref() == Ok("1");
                assert_eq!(
                    direct_route_enabled(var),
                    opted_in,
                    "{var} must be enabled only by an explicit `1` opt-in"
                );
            }
            assert_eq!(
                direct_click_enabled(),
                direct_route_enabled("PETAL_REMOTE_CONTROL_DIRECT_CLICK")
            );
            assert_eq!(
                direct_drag_enabled(),
                direct_route_enabled("PETAL_REMOTE_CONTROL_DIRECT_DRAG")
            );
            assert_eq!(
                direct_scroll_enabled(),
                direct_route_enabled("PETAL_REMOTE_CONTROL_DIRECT_SCROLL")
            );
        }

        #[test]
        fn sl_click_fallback_primes_once_per_pid() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(7);
            ax.set_capabilities(7, AxCapabilities::default());
            let sl = RecordingSlClickBackend::available();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.11,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            // #446: a PassThrough Down (no direct-drag opt-in) now reports a
            // real injection failure -- see replay_with_backends's `action !=
            // Move` guard -- instead of posting the known-ineffective CGEvent
            // fallback, so the sink stays empty here too.
            assert!(replay_events_with_backends(&down, frame, &ax, &sl).is_empty());
            assert!(replay_events_with_backends(&up, frame, &ax, &sl).is_empty());

            let down = pointer_message(
                RemoteControlAction::Down,
                0.20,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.21,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            replay_events_with_backends(&down, frame, &ax, &sl);
            replay_events_with_backends(&up, frame, &ax, &sl);

            assert_eq!(
                sl.clicks(),
                vec![
                    SlClick {
                        pid: 1234,
                        x: -1.0,
                        y: -1.0,
                        button: RemoteControlButton::Left,
                        click_state: 1,
                    },
                    SlClick {
                        pid: 1234,
                        x: 11.0,
                        y: 10.0,
                        button: RemoteControlButton::Left,
                        click_state: 1,
                    },
                    SlClick {
                        pid: 1234,
                        x: 21.0,
                        y: 10.0,
                        button: RemoteControlButton::Left,
                        click_state: 1,
                    },
                ]
            );
        }

        #[test]
        fn sl_click_unavailable_reports_injection_failure() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(8);
            ax.set_capabilities(8, AxCapabilities::default());
            let sl = RecordingSlClickBackend::unavailable();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.11,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            let mut events = Vec::new();
            events.extend(replay_events_with_backends(&down, frame, &ax, &sl));
            events.extend(replay_events_with_backends(&up, frame, &ax, &sl));

            assert_eq!(sl.clicks(), Vec::new());
            assert!(events.is_empty());
        }

        #[test]
        fn legacy_pointer_sequence_returns_real_failure_without_ax_or_skylight() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(80);
            ax.set_capabilities(80, AxCapabilities::default());
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.20,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            // #446 follow-up: a PassThrough Down that recorded a gesture no
            // longer hard-fails immediately -- the matching Up is what
            // actually attempts delivery and determines success/failure, so
            // nacking the Down was premature (see the dispatcher's Down-
            // specific arm). Only drag/Up still surface a real failure here.
            // #446: with NO session-tap route available (Accessibility not
            // granted) the honest failure is preserved -- drag/Up still nack
            // rather than silently no-opping through the ineffective
            // CGEventPostToPid pointer path.
            let tap = RecordingSessionTap::untrusted();
            assert!(replay_with_backends(
                &down,
                frame,
                Some(1234),
                &sink,
                &ax,
                &sl,
                &RecordingPasteboard::default(),
                &tap
            )
            .is_ok());
            for message in [&drag, &up] {
                assert!(replay_with_backends(
                    message,
                    frame,
                    Some(1234),
                    &sink,
                    &ax,
                    &sl,
                    &RecordingPasteboard::default(),
                    &tap
                )
                .is_err());
            }
            assert!(sink.events().is_empty());
        }

        #[test]
        fn v2_semantic_click_returns_real_failure_without_ax_or_skylight() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(81);
            ax.set_capabilities(81, AxCapabilities::default());
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            // #446: unchanged when the session tap is unavailable -- a v2
            // semantic click must never degrade into the ineffective
            // CGEventPostToPid pointer path.
            let tap = RecordingSessionTap::untrusted();
            assert!(replay_with_backends(
                &click,
                frame,
                Some(1234),
                &sink,
                &ax,
                &sl,
                &RecordingPasteboard::default(),
                &tap
            )
            .is_err());
            assert!(sink.events().is_empty());
        }

        // -----------------------------------------------------------------
        // #446 session-tap route.
        //
        // These drive the REAL dispatcher (`replay_with_backends`) with the
        // real message types a controller sends, not the pure helpers it
        // delegates to. CLAUDE.md's hard rule exists because 810 green tests
        // on isolated helpers previously missed a showstopper in the wiring:
        // a helper being correct proves nothing about whether the live event
        // chain ever calls it with the right inputs, in the right order.
        // -----------------------------------------------------------------

        /// Build the AX-hostile target these tests need: an element resolves,
        /// but it exposes NO actionable affordance (not pressable, not text
        /// selectable, no menu) -- i.e. custom-drawn content, which is exactly
        /// the case the whole issue is about.
        fn ax_hostile_backend() -> RecordingAxBackend {
            let ax = RecordingAxBackend::default();
            ax.resolve_to(81);
            ax.set_capabilities(81, AxCapabilities::default());
            ax
        }

        /// #446 acceptance A1: `action=Click` -- what real browser
        /// controllers send -- must actuate AX-hostile content.
        ///
        /// It did not, while the legacy Down/Up pair at the same coordinate
        /// on the same target did. The asymmetry is SkyLight: only the
        /// semantic-click path consulted it, and `sl_click_or_passthrough`
        /// reports `Handled` whenever the fire-and-forget post succeeded, so
        /// the click was marked delivered and never fell through to the
        /// session tap. This test therefore uses an AVAILABLE SL backend --
        /// with `unavailable()` the bug does not reproduce at all.
        #[test]
        fn v2_semantic_click_reaches_the_session_tap_even_when_skylight_is_available() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::available();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&click, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("a semantic click on custom-drawn content must be serviced");

            assert_eq!(
                tap.mouse_kinds(),
                vec![MouseKind::LeftDown, MouseKind::LeftUp],
                "the click must be posted through the session tap, the only route \
                 measured to deliver: {:?}",
                tap.events()
            );
            assert!(
                tap.events().contains(&TapEvent::Raise(1234, 42)),
                "delivery is geometry-hit-tested, so the target must be raised first"
            );
            assert!(
                sl.clicks().is_empty(),
                "the known-ineffective SkyLight click must not claim the gesture: {:?}",
                sl.clicks()
            );
            assert!(sink.events().is_empty());
        }

        /// The opt-in escape hatch is preserved: `PETAL_REMOTE_CONTROL_
        /// DIRECT_CLICK=1` still hands the click to SkyLight. The flag is
        /// threaded in as a parameter rather than read from the environment,
        /// so the FLAG cannot race a parallel test.
        ///
        /// #773: that reasoning covered the flag and stopped there, which is
        /// why this was the one SL-path test without `ax_test_lock()`. The
        /// opted-in call still drives the real click route, which writes
        /// process-wide AX/SL state (the primed-pid set, and the gesture and
        /// cursor-takeover maps keyed by a window id every sibling shares).
        /// Running unlocked let it interleave with a sibling that had cleared
        /// that state under the lock and was mid-assertion, which is the
        /// ~6%-under-load flake in `middle_click_reaches_sl_backend_...`.
        #[test]
        fn semantic_click_only_consults_skylight_when_the_direct_route_is_opted_in() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let point = super::super::GlobalPoint { x: 12.0, y: 34.0 };

            let off = RecordingSlClickBackend::available();
            assert_eq!(
                semantic_click_sl_or_passthrough(
                    false,
                    1234,
                    point,
                    RemoteControlButton::Left,
                    0.0,
                    1,
                    &off
                ),
                AxReplayOutcome::PassThrough,
                "by default the click must fall through to the session tap"
            );
            assert!(off.clicks().is_empty());

            let on = RecordingSlClickBackend::available();
            assert_eq!(
                semantic_click_sl_or_passthrough(
                    true,
                    1234,
                    point,
                    RemoteControlButton::Left,
                    0.0,
                    1,
                    &on
                ),
                AxReplayOutcome::Handled,
                "the opt-in must still reach SkyLight"
            );
            assert!(!on.clicks().is_empty());
        }

        #[test]
        fn legacy_down_move_up_drag_is_delivered_end_to_end_by_the_session_tap() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.20,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.20,
                RemoteControlButton::Left,
                0,
            );

            for message in [&down, &drag, &up] {
                replay_with_backends(message, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .expect("session tap should service the whole gesture");
            }

            // The full gesture, in order, on ONE route -- never mixed.
            assert_eq!(
                tap.mouse_kinds(),
                vec![
                    MouseKind::LeftDown,
                    MouseKind::LeftDragged,
                    MouseKind::LeftUp,
                ]
            );
            // The cursor was warped INTO the shared window before the Down, so
            // the visible jump is contained to that window.
            assert!(matches!(
                tap.events().first(),
                Some(TapEvent::Raise(_, _)) | Some(TapEvent::MoveCursor(..))
            ));
            assert!(tap
                .events()
                .iter()
                .any(|event| matches!(event, TapEvent::MoveCursor(..))));
            // The known-ineffective CGEvent pointer sink was never used.
            assert!(sink.events().is_empty());
            // The target was raised: delivery is geometry-hit-tested.
            assert!(tap.events().contains(&TapEvent::Raise(1234, 42)));
        }

        // #759: every process-scoped injection route must retain the grant's
        // exact window identity through the real replay dispatcher.

        #[test]
        fn identity_unavailable_refuses_pointer_before_any_posting_route() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            ax.fail_resolution_with(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::available();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();
            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );

            let error =
                replay_with_backends(&down, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .expect_err("unresolved window identity must refuse pointer input");

            assert!(error.starts_with("targetUnavailable:"), "{error}");
            assert!(sl.events().is_empty());
            assert!(sl.clicks().is_empty());
            assert!(tap.events().is_empty());
            assert!(sink.events().is_empty());
        }

        #[test]
        fn identity_unavailable_refuses_wheel_before_pid_only_scroll() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            ax.fail_resolution_with(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::available();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();
            let wheel = wheel_message(0.0, 20.0, Some(0));

            let error =
                replay_with_backends(&wheel, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .expect_err("unresolved window identity must refuse wheel input");

            assert!(error.starts_with("targetUnavailable:"), "{error}");
            assert_eq!(sl.scroll_count(), 0);
            assert!(tap.events().is_empty());
            assert!(sink.events().is_empty());
        }

        #[test]
        fn identity_unavailable_refuses_text_shortcut_before_key_fallback() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            ax.fail_text_resolution_with(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::untrusted();
            let key = key_message("KeyA", "a", meta_only());

            let error =
                replay_with_backends(&key, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .expect_err("unresolved window identity must refuse keyboard input");

            assert!(error.starts_with("targetUnavailable:"), "{error}");
            assert!(ax.ops().is_empty());
            assert!(sink.events().is_empty());
        }

        #[test]
        fn identity_unavailable_gates_opted_in_direct_semantic_click() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            ax.fail_resolution_with(AxError::new(K_AX_ERROR_WINDOW_IDENTITY_UNAVAILABLE));
            let sl = RecordingSlClickBackend::available();
            let tap = RecordingSessionTap::trusted();

            let error = replay_semantic_click_ax_with_direct(
                42,
                1234,
                super::super::GlobalPoint { x: 10.0, y: 10.0 },
                RemoteControlButton::Left,
                1,
                true,
                &ax,
                &sl,
                &tap,
            )
            .expect_err("direct semantic click must wait for an identity verdict");

            assert!(error.starts_with("targetUnavailable:"), "{error}");
            assert!(sl.clicks().is_empty());
            assert!(tap.events().is_empty());
        }

        #[test]
        fn sibling_window_ax_hit_is_nacked_before_any_click_route_runs() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            ax.place_element_in_window(81, 42);
            ax.place_element_in_window(82, 43);
            ax.resolve_to(82);
            ax.set_capabilities(
                82,
                AxCapabilities {
                    pressable: true,
                    ..AxCapabilities::default()
                },
            );
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();
            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            assert!(replay_with_backends(
                &click,
                unit_frame(),
                Some(1234),
                &sink,
                &ax,
                &sl,
                &pb,
                &tap,
            )
            .is_err());
            assert!(ax.ops().is_empty(), "sibling element must not be pressed");
            assert!(tap.events().is_empty(), "mismatch must not fall through");
            assert!(sink.events().is_empty());
        }

        #[test]
        fn destructive_text_shortcuts_never_act_on_a_sibling_window() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();

            for (message, pasteboard) in [
                (
                    key_message("KeyA", "a", meta_only()),
                    RecordingPasteboard::default(),
                ),
                (
                    key_message("KeyV", "v", meta_only()),
                    RecordingPasteboard::with_text("must-stay-out-of-window-43"),
                ),
            ] {
                let ax = RecordingAxBackend::default();
                ax.place_element_in_window(91, 42);
                ax.place_element_in_window(92, 43);
                ax.resolve_text_to(92);
                ax.set_capabilities(92, text_selectable_caps());
                ax.set_text_length(92, 12);
                let sink = RecordingSink::default();
                // Keep CGEvent focused on A so only the AX text gate can pass.
                sink.focus_window(42);
                let sl = RecordingSlClickBackend::unavailable();
                let tap = RecordingSessionTap::untrusted();

                assert!(replay_with_backends(
                    &message,
                    unit_frame(),
                    Some(1234),
                    &sink,
                    &ax,
                    &sl,
                    &pasteboard,
                    &tap,
                )
                .is_err());
                assert!(ax.ops().is_empty(), "shortcut acted on sibling window");
                assert!(sink.events().is_empty());
            }
        }

        #[test]
        fn cgevent_typing_requires_the_authorized_window_to_have_focus() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::untrusted();
            let mut text = base_message(RemoteControlType::Text);
            text.text = Some("unsafe".to_string());

            let messages = [
                key_message("KeyB", "b", RemoteControlModifiers::default()),
                text,
            ];
            for message in &messages {
                let control = RecordingSink::default();
                control.focus_window(42);
                replay_with_backends(
                    message,
                    unit_frame(),
                    Some(1234),
                    &control,
                    &ax,
                    &sl,
                    &pb,
                    &tap,
                )
                .expect("typing into the authorized focused window must still work");
                assert!(!control.events().is_empty());

                let sink = RecordingSink::default();
                sink.focus_window(43);
                assert!(replay_with_backends(
                    message,
                    unit_frame(),
                    Some(1234),
                    &sink,
                    &ax,
                    &sl,
                    &pb,
                    &tap,
                )
                .is_err());
                assert!(sink.events().is_empty());
            }
        }

        /// Stuck-modifier fix: a shift press/release pair driven through the REAL replay
        /// chain (`replay_with_backends` -> `replay_to_sink` -> `replay_key`),
        /// not the gate helper in isolation.
        fn shift_down_and_up() -> (RemoteControlMessage, RemoteControlMessage) {
            let down = key_message("ShiftLeft", "Shift", RemoteControlModifiers::default());
            let mut up = down.clone();
            up.action = Some(RemoteControlAction::Up);
            up.seq = down.seq + 1;
            (down, up)
        }

        #[test]
        fn a_key_up_is_delivered_even_when_the_authorized_window_is_no_longer_focused() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::untrusted();
            let (down, up) = shift_down_and_up();

            // Focus has drifted to a sibling window (43) while the authorized
            // window is 42. The DOWN must still be refused -- that is what
            // proves the gate is live in this fixture rather than absent.
            let down_sink = RecordingSink::default();
            down_sink.focus_window(43);
            assert!(
                replay_with_backends(
                    &down,
                    unit_frame(),
                    Some(1234),
                    &down_sink,
                    &ax,
                    &sl,
                    &pb,
                    &tap,
                )
                .is_err(),
                "key down under mismatched focus must still be refused"
            );
            assert!(down_sink.events().is_empty());

            // Same mismatch, release direction. Every revoke path drains the
            // pressed entry BEFORE this replay, so a refused Up is dropped
            // forever and the modifier stays held in the target app.
            let up_sink = RecordingSink::default();
            up_sink.focus_window(43);
            replay_with_backends(&up, unit_frame(), Some(1234), &up_sink, &ax, &sl, &pb, &tap)
                .expect("a key release must never be refused for focus drift");
            let events = up_sink.events();
            assert!(
                matches!(events.as_slice(), [SynthEvent::Key { down: false, .. }]),
                "the release must reach the sink, got {events:?}"
            );
        }

        #[test]
        fn a_key_down_is_still_refused_when_focus_does_not_match() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::untrusted();

            // Both key-replay arms: a virtual key (Shift) and the plain-text
            // arm ("b"), which is down-only by construction and stays gated.
            let (shift_down, _) = shift_down_and_up();
            let messages = [
                shift_down,
                key_message("KeyB", "b", RemoteControlModifiers::default()),
            ];
            for message in &messages {
                let sink = RecordingSink::default();
                sink.focus_window(43);
                assert!(
                    replay_with_backends(
                        message,
                        unit_frame(),
                        Some(1234),
                        &sink,
                        &ax,
                        &sl,
                        &pb,
                        &tap,
                    )
                    .is_err(),
                    "the key-up relaxation must never widen to key down: {message:?}"
                );
                assert!(sink.events().is_empty());
            }
        }

        #[test]
        fn a_key_up_naming_an_unauthorized_window_is_still_refused() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::untrusted();
            let (_, mut up) = shift_down_and_up();
            up.window_id = 43;

            // The sink is authorized for window 42 and focus is not consulted
            // at all: skipping the live focus round-trip for a release must
            // NOT skip the cheap local "is this even my window" check.
            let sink = RecordingSink::default();
            sink.authorize_window(42);
            assert!(
                replay_with_backends(&up, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .is_err(),
                "a release naming an unauthorized window must still be refused"
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn session_tap_raise_selects_authorized_window_not_axwindows_zero() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = ax_hostile_backend();
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            // Sibling 43 is AXWindows[0]; authorized window 42 comes second.
            let tap = RecordingSessionTap::with_windows(vec![43, 42]);
            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&click, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("authorized window remains deliverable");
            assert_eq!(tap.events().first(), Some(&TapEvent::Raise(1234, 42)));
            assert!(!tap.mouse_kinds().is_empty());
        }

        // -----------------------------------------------------------------
        // Same-PID window scoping: a buttonless hover Move is the one
        // pointer message that still reaches the legacy CGEventPostToPid sink
        // (AX: PassThrough by design, #446). That sink scopes delivery by PID
        // only -- AppKit resolves a pid-posted mouseMoved to whichever of the
        // app's OWN windows is at the point, so an unshared sibling of the
        // same app overlapping the shared window receives the hover, with
        // the controller unable to see it. The session-tap route already
        // refuses this exact stack (#759); the sink route must too.
        // -----------------------------------------------------------------

        fn tap_with_same_pid_sibling(sibling_in_front: bool) -> RecordingSessionTap {
            let sibling = StackWindow {
                window_id: 43,
                owner_pid: 1234,
                layer: 0,
                alpha: 1.0,
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            };
            let target = StackWindow {
                window_id: 42,
                owner_pid: 1234,
                layer: 0,
                alpha: 1.0,
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            };
            let stack = if sibling_in_front {
                vec![sibling, target]
            } else {
                vec![target, sibling]
            };
            RecordingSessionTap {
                stack: Some(stack),
                ..RecordingSessionTap::trusted()
            }
        }

        #[test]
        fn hover_move_is_not_posted_when_a_same_pid_sibling_covers_the_target() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = ax_hostile_backend();
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = tap_with_same_pid_sibling(true);
            let hover = pointer_message(
                RemoteControlAction::Move,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&hover, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("a refused hover is dropped quietly, never nacked");
            assert!(
                sink.events().is_empty(),
                "hover must not be posted to pid 1234 while sibling 43 covers window 42: {:?}",
                sink.events()
            );
            assert!(
                tap.mouse_kinds().is_empty(),
                "hover never takes the session-tap route"
            );
        }

        #[test]
        fn hover_move_is_posted_when_the_same_pid_sibling_is_behind_the_target() {
            // Control for the test above: an always-drop would pass it too.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let ax = ax_hostile_backend();
            let sink = RecordingSink::default();
            let sl = RecordingSlClickBackend::unavailable();
            let pb = RecordingPasteboard::default();
            let tap = tap_with_same_pid_sibling(false);
            let hover = pointer_message(
                RemoteControlAction::Move,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&hover, unit_frame(), Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("hover over the unobstructed target is delivered");
            assert!(
                matches!(
                    sink.events().as_slice(),
                    [SynthEvent::Mouse {
                        kind: MouseKind::Moved,
                        ..
                    }]
                ),
                "expected exactly one Moved through the sink: {:?}",
                sink.events()
            );
        }

        // -----------------------------------------------------------------
        // #599: the tier must not report success for input it could not
        // deliver.
        //
        // `prepare_session_tap_target` used to call `tap.raise(pid)` and
        // discard the boolean the trait documents as "false if it could not
        // be raised". Delivery on this route is geometry-hit-tested against
        // the real window stack, so a failed raise means nothing posted
        // afterwards can reach the target -- yet the host logged
        // `mode=SessionTap outcome=Handled` and the controller recorded
        // `outcome=applied` while the target received zero events.
        //
        // These drive the REAL dispatcher with the real message types a
        // controller sends, not `prepare_session_tap_target` in isolation:
        // the defect was in what the caller chain did with a return value,
        // which a pure-helper test cannot observe.
        // -----------------------------------------------------------------

        #[test]
        fn v2_semantic_click_nacks_when_the_target_cannot_be_raised() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::unraisable();

            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            assert!(
                replay_with_backends(&click, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .is_err(),
                "a click whose target could not be raised must surface a real failure, \
                 not report Handled"
            );
            // The raise was attempted, and nothing was posted after it failed.
            assert!(tap.events().contains(&TapEvent::Raise(1234, 42)));
            assert!(
                tap.mouse_kinds().is_empty(),
                "nothing may be posted through a route that cannot reach the target: {:?}",
                tap.events()
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn legacy_drag_nacks_when_the_target_cannot_be_raised() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::unraisable();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.20,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.20,
                RemoteControlButton::Left,
                0,
            );

            // A Down defers its verdict to the matching Up by design, but the
            // drag and the Up must both nack rather than claim delivery.
            assert!(
                replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).is_ok()
            );
            for message in [&drag, &up] {
                assert!(
                    replay_with_backends(message, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                        .is_err(),
                    "an undeliverable gesture must nack"
                );
            }
            assert!(
                tap.mouse_kinds().is_empty(),
                "nothing may be posted through a route that cannot reach the target: {:?}",
                tap.events()
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn wheel_nacks_when_the_target_cannot_be_raised() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::unraisable();

            let wheel = wheel_message(0.0, -40.0, None);

            assert!(
                replay_with_backends(&wheel, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .is_err(),
                "an undeliverable wheel must nack"
            );
            assert!(
                tap.events()
                    .iter()
                    .all(|event| !matches!(event, TapEvent::Scroll(..))),
                "nothing may be posted through a route that cannot reach the target: {:?}",
                tap.events()
            );
            assert!(sink.events().is_empty());
        }

        // -----------------------------------------------------------------
        // #599 part 2: the raise SUCCEEDING is still not reachability.
        //
        // Under a `.floating` occluder AXRaise genuinely returns true -- the
        // raise really happened, the target simply is not the topmost window
        // at that coordinate, because a floating panel sits above anything a
        // normal window can be lifted to. The acceptance suite's `Q-OCCLUDED`
        // scenario measured exactly this: 0 events delivered,
        // `outcome=applied`. The raise-boolean fix cannot see it by
        // construction, so the tier hit-tests the window stack before posting.
        //
        // The verdict tests below pin the decision itself; the dispatcher
        // tests drive `replay_with_backends` with the real message a
        // controller sends, because the defect class here is a caller chain
        // ignoring a signal -- which a pure-helper test cannot observe.
        // -----------------------------------------------------------------

        fn stack_window(window_id: i64, owner_pid: i32, x: f64, y: f64) -> StackWindow {
            StackWindow {
                window_id,
                owner_pid,
                layer: 0,
                alpha: 1.0,
                x,
                y,
                w: 100.0,
                h: 100.0,
            }
        }

        fn at(x: f64, y: f64) -> super::super::GlobalPoint {
            super::super::GlobalPoint { x, y }
        }

        #[test]
        fn hit_test_reports_a_foreign_window_in_front_of_the_target() {
            let stack = vec![
                stack_window(9001, 5678, 0.0, 0.0),
                stack_window(42, 1234, 0.0, 0.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::CoveredBy {
                    owner_pid: 5678,
                    window_id: 9001
                }
            );
        }

        #[test]
        fn hit_test_ignores_a_foreign_window_that_does_not_cover_the_point() {
            let stack = vec![
                stack_window(9001, 5678, 500.0, 500.0),
                stack_window(42, 1234, 0.0, 0.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_never_blocks_on_our_own_overlay_windows() {
            // LOAD-BEARING, not belt-and-braces: Petal's own panels are
            // created at AppKit level 0 (`setLevel: 0` in platform/appkit.rs,
            // hover_tab.rs, share_border.rs), i.e. INSIDE the 0..=
            // BLOCKING_LAYER_MAX band, so the layer bound does not exclude
            // them. And the share border genuinely sits in front of the shared
            // window -- the acceptance harness prints exactly that:
            // `share-border-stack window=11059 border=17 source=19 (border in
            // front)`. Without this own-pid skip, Petal's own border would be
            // read as an occluder and every healthy gesture on every shared
            // window would nack. The overlays are click-through, so they block
            // nothing in reality.
            // (Note share_border.rs also has a level-25 mode, above the band;
            // this exclusion is what covers the level-0 panels.)
            // #759: `self_pid` is now the SOLE pid-based exclusion. The
            // target-pid twin was deleted deliberately -- do not add it back
            // to make this look symmetric.
            let stack = vec![
                stack_window(7, 999, 0.0, 0.0),
                stack_window(42, 1234, 0.0, 0.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_blocks_on_the_targets_own_unshared_sibling_window() {
            // #759. This test asserted the OPPOSITE until 2026-08-10, and that
            // is why the hole stayed open: the skip was a conservative
            // preference from #599, the test was written to match it, and the
            // test then made removing it look like a regression. An unshared
            // sibling of the target app is not visible in the shared stream, so
            // a click that lands there lands somewhere the controller cannot
            // see.
            let stack = vec![
                stack_window(43, 1234, 0.0, 0.0),
                stack_window(42, 1234, 0.0, 0.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::CoveredBy {
                    owner_pid: 1234,
                    window_id: 43
                }
            );
        }

        #[test]
        fn hit_test_ignores_a_same_pid_sibling_that_does_not_cover_the_point() {
            // The discrimination is geometric, not pid-based: a sibling window
            // elsewhere on screen must not nack a healthy gesture.
            let stack = vec![
                stack_window(43, 1234, 500.0, 500.0),
                stack_window(42, 1234, 0.0, 0.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_ignores_a_fully_transparent_same_pid_sibling() {
            // Apps keep invisible same-pid scaffolding over their own windows;
            // the alpha skip must still apply now that same-pid windows are
            // candidates at all.
            let mut invisible = stack_window(43, 1234, 0.0, 0.0);
            invisible.alpha = 0.0;
            let stack = vec![invisible, stack_window(42, 1234, 0.0, 0.0)];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_ignores_fully_transparent_windows_in_front() {
            let mut invisible = stack_window(9001, 5678, 0.0, 0.0);
            invisible.alpha = 0.0;
            let stack = vec![invisible, stack_window(42, 1234, 0.0, 0.0)];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_never_blocks_on_the_docks_full_screen_window() {
            // REGRESSION, measured live: the Dock owns a full-screen window
            // (layer 20, alpha 1.0, bounds 0,0 1512x982) that sits in front of
            // everything and therefore contains EVERY point on the display.
            // Counting it as an occluder made the tier nack all input and
            // regressed six acceptance cases, while PC-DIRECT kept landing a
            // real click at the same coordinate -- proving it blocks nothing.
            let mut dock = stack_window(11, 651, 0.0, 0.0);
            dock.layer = 20;
            dock.w = 1512.0;
            dock.h = 982.0;
            // The target has to actually contain the sampled point now that
            // #759's target-bounds precondition runs first; the 100x100
            // default never did, which made this fixture depend on a check
            // that no longer exists. Widening it keeps what this test is
            // about -- the Dock's layer-20 full-screen window is not an
            // occluder -- and nothing else.
            let mut target = stack_window(42, 1234, 0.0, 0.0);
            target.w = 1512.0;
            target.h = 982.0;
            let stack = vec![dock, target];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(600.0, 772.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_still_blocks_on_a_floating_window_in_the_application_band() {
            // The other half of the same boundary: `.floating` is layer 3,
            // which is exactly what the acceptance suite's occluder uses and
            // what AXRaise cannot lift a normal window above. Raising
            // BLOCKING_LAYER_MAX past the Dock's 20 would silently disable
            // the whole check, so pin both sides.
            let mut occluder = stack_window(9001, 5678, 0.0, 0.0);
            occluder.layer = 3;
            let stack = vec![occluder, stack_window(42, 1234, 0.0, 0.0)];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::CoveredBy {
                    owner_pid: 5678,
                    window_id: 9001
                }
            );
        }

        #[test]
        fn hit_test_refuses_a_point_outside_the_targets_own_live_bounds() {
            // #759, the REFUSE direction. The target is frontmost -- which is
            // the normal state here, because `prepare_session_tap_target`
            // AXRaises it immediately before this check -- so nothing is in
            // front of it and the occlusion loop has nothing to look at. The
            // point is nonetheless 500pt away, inside an unshared sibling of
            // the same app that sits BEHIND the target in z-order. Before this
            // check the verdict was NothingInFront and the session tap posted
            // a real click into the sibling.
            let stack = vec![
                stack_window(42, 1234, 0.0, 0.0),
                stack_window(43, 1234, 500.0, 500.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(550.0, 550.0), 999),
                HitTestVerdict::TargetNotAtPoint
            );
        }

        #[test]
        fn hit_test_allows_a_point_inside_the_targets_own_live_bounds() {
            // #759, the ALLOW direction, and the half that matters most: a
            // guard that refuses everything is a regression, not a fix (#777
            // refused 284 real key events live). Same stack as above, same
            // frontmost target, but the point is where the controller
            // actually clicked -- inside the shared window.
            let stack = vec![
                stack_window(42, 1234, 0.0, 0.0),
                stack_window(43, 1234, 500.0, 500.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(50.0, 50.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_allows_the_targets_far_edge() {
            // The other half of the ALLOW direction, and the reason
            // `covers_target_point` is inclusive where `contains` is not:
            // `normalized_to_global` maps a normalized 1.0 to exactly
            // `frame.x + width`, so a controller clicking the extreme
            // right/bottom edge produces this exact point. Half-open bounds
            // would nack it.
            let stack = vec![stack_window(42, 1234, 0.0, 0.0)];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(100.0, 100.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_allows_a_target_whose_live_origin_is_fractional() {
            // The third ALLOW case, and the one that would have shipped a
            // live regression. `WindowFrame`'s fields are `i32` rounded from
            // the CG bounds, so a window whose real origin is 100.5 is cached
            // as 100 -- and a normalized 0.0 click then maps to 100.0, just
            // OUTSIDE the live rectangle. Without the edge slop this refuses
            // the top-left corner of every fractionally-positioned window.
            let mut target = stack_window(42, 1234, 100.5, 200.5);
            target.w = 640.0;
            target.h = 480.0;
            let stack = vec![target];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(100.0, 200.0), 999),
                HitTestVerdict::NothingInFront
            );
        }

        #[test]
        fn hit_test_slop_does_not_reach_into_a_neighbouring_window() {
            // The slop must stay a rounding allowance, not a hole: a point
            // well clear of the target still refuses. 500pt away is the
            // scenario #759 is actually about.
            let stack = vec![
                stack_window(42, 1234, 0.0, 0.0),
                stack_window(43, 1234, 500.0, 500.0),
            ];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(103.0, 50.0), 999),
                HitTestVerdict::TargetNotAtPoint
            );
        }

        #[test]
        fn hit_test_reports_a_target_that_is_not_on_screen_at_all() {
            let stack = vec![stack_window(9001, 5678, 0.0, 0.0)];
            assert_eq!(
                hit_test_target(Some(&stack), 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::TargetNotOnScreen
            );
        }

        #[test]
        fn an_unreadable_window_stack_is_unknown_not_a_failure() {
            // The whole point of this issue is not reporting things we did not
            // verify. That cuts both ways: a CoreGraphics hiccup must never
            // manufacture a nack for input that would have landed.
            assert_eq!(
                hit_test_target(None, 42, 1234, at(10.0, 10.0), 999),
                HitTestVerdict::Unknown
            );
        }

        #[test]
        fn v2_semantic_click_nacks_when_a_foreign_window_covers_the_target_point() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::occluded_by_foreign_window();

            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            assert!(
                replay_with_backends(&click, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .is_err(),
                "a click whose target is buried under another process's window must surface a \
                 real failure, not report Handled"
            );
            // The raise SUCCEEDED here -- that is the whole point. What must
            // not happen is posting afterwards and calling it delivered.
            assert!(tap.events().contains(&TapEvent::Raise(1234, 42)));
            assert!(
                tap.mouse_kinds().is_empty(),
                "nothing may be posted at a coordinate the target cannot receive: {:?}",
                tap.events()
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn v2_semantic_click_nacks_when_the_target_has_moved_off_the_cached_frame() {
            // #759, driven through the REAL replay path rather than the pure
            // hit-test helper -- the defect class this issue is about is a
            // caller chain that never asks the question, which a helper test
            // cannot observe.
            //
            // Cached frame says 0,0 100x100; the target really is at 500,500;
            // window 43, an unshared sibling of the same app, is at 0,0 behind
            // it. The controller's 0.10,0.10 maps to (10,10) -- inside the
            // sibling, not inside the shared window. Nothing may be posted.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::target_moved_off_the_cached_frame();

            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&click, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).expect_err(
                "a click whose mapped point is outside the shared window's live bounds must \
                     nack, not land in the unshared sibling that is actually there",
            );
            assert!(
                tap.mouse_kinds().is_empty(),
                "nothing may be posted into a window the controller is not authorized for: {:?}",
                tap.events()
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn session_tap_names_a_moved_target_distinctly_from_an_absent_one() {
            // #759's definition of done asks for a clear reason, not a silent
            // refusal, and "the window is gone" and "the window is elsewhere"
            // are different problems for whoever reads the log. Asserted here,
            // at the boundary that produces the text: the semantic-click
            // fallback chain above converts any prepare failure into its own
            // generic "exhausted AX/SkyLight routes" string, which loses this
            // detail for the pre-existing CoveredBy case too.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let tap = RecordingSessionTap::target_moved_off_the_cached_frame();

            let error = prepare_session_tap_target(
                42,
                1234,
                super::super::GlobalPoint { x: 10.0, y: 10.0 },
                &tap,
            )
            .expect_err("a point outside the target's live bounds must not be prepared");
            assert!(
                error.contains("target_not_at_point"),
                "the reason must say the point is not in the target, not that the target is off \
                 screen: {error}"
            );
        }

        #[test]
        fn v2_semantic_click_still_lands_when_the_target_has_not_moved() {
            // The ALLOW direction for the same guard, and the half that was
            // skipped when this area was last touched: #777 shipped a check
            // that refused 284 real key events live because only the blocked
            // case was ever verified. Identical stack and z-order to the test
            // above -- same unshared sibling, same target frontmost -- with
            // the one difference that the cached frame is current. The click
            // must be posted.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::sibling_behind_an_unmoved_target();

            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&click, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("an authorized click inside the shared window must still be delivered");
            assert!(
                !tap.mouse_kinds().is_empty(),
                "the authorized click must still be posted: {:?}",
                tap.events()
            );
        }

        #[test]
        fn legacy_drag_nacks_when_a_foreign_window_covers_the_target_point() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::occluded_by_foreign_window();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.20,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.20,
                RemoteControlButton::Left,
                0,
            );

            // Same shape as the raise-variant test above: a Down defers its
            // verdict to the matching Up by design, so the nack lands on the
            // drag and the Up. What must hold either way is that nothing was
            // posted at a coordinate the target cannot receive.
            assert!(
                replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).is_ok()
            );
            for message in [&drag, &up] {
                assert!(
                    replay_with_backends(message, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                        .is_err(),
                    "an occluded gesture must nack"
                );
            }
            assert!(
                tap.mouse_kinds().is_empty(),
                "nothing may be posted at a coordinate the target cannot receive: {:?}",
                tap.events()
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn wheel_nacks_when_a_foreign_window_covers_the_target_point() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::occluded_by_foreign_window();

            let wheel = wheel_message(0.0, -40.0, None);

            assert!(
                replay_with_backends(&wheel, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .is_err(),
                "an occluded wheel must nack"
            );
            assert!(
                tap.events()
                    .iter()
                    .all(|event| !matches!(event, TapEvent::Scroll(..))),
                "nothing may be posted at a coordinate the target cannot receive: {:?}",
                tap.events()
            );
        }

        #[test]
        fn a_frontmost_target_still_gets_its_click_posted() {
            // The positive control for the three tests above: same stack, with
            // the target in FRONT. Without this, an always-nack regression
            // would pass the occluded tests.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::frontmost_over_foreign_window();

            let click = pointer_message(
                RemoteControlAction::Click,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&click, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("a reachable target must not be nacked by the hit test");
            assert!(
                !tap.mouse_kinds().is_empty(),
                "the click must still be posted when nothing is in front: {:?}",
                tap.events()
            );
        }

        #[test]
        fn session_tap_restores_the_host_cursor_once_the_gesture_completes() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();
            let host_cursor = *tap.cursor.lock_unpoisoned();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.20,
                0.20,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.20,
                0.20,
                RemoteControlButton::Left,
                0,
            );

            replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            replay_with_backends(&drag, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            // Mid-gesture the cursor must still be ON the drag, not restored --
            // restoring per event would break the drag.
            assert_ne!(*tap.cursor.lock_unpoisoned(), host_cursor);

            replay_with_backends(&up, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            assert_eq!(
                *tap.cursor.lock_unpoisoned(),
                host_cursor,
                "cursor must be handed back once the gesture completes"
            );
            // Exactly one restore, after the Up.
            assert_eq!(
                tap.events().last(),
                Some(&TapEvent::MoveCursor(q(host_cursor.x), q(host_cursor.y)))
            );
        }

        #[test]
        fn session_tap_does_not_yank_the_cursor_back_from_a_present_host_user() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            // The host is physically moving the mouse: it does not stay where
            // we post it.
            let tap = RecordingSessionTap::with_host_moving_cursor();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            replay_with_backends(&up, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();

            // The release still went out (never leave a button held) ...
            assert!(tap.mouse_kinds().contains(&MouseKind::LeftUp));
            // ... but no restore warp was posted after it: the LAST event is
            // the release, not a MoveCursor.
            assert!(
                matches!(
                    tap.events().last(),
                    Some(TapEvent::Mouse(MouseKind::LeftUp, _, _))
                ),
                "must not warp the pointer out from under a present host user, got {:?}",
                tap.events().last()
            );
        }

        #[test]
        fn cancelling_a_session_tap_drag_posts_the_missing_mouse_up() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            assert_eq!(tap.mouse_kinds(), vec![MouseKind::LeftDown]);

            // Control is torn down mid-drag (revoke / disconnect / share ended).
            let released = release_session_tap_gestures_with_backend(down.window_id, None, &tap);

            assert_eq!(released, 1, "the open gesture must be found and released");
            assert!(
                tap.mouse_kinds().contains(&MouseKind::LeftUp),
                "a cancelled drag must post its mouse-up or the target is left with a phantom held button"
            );
        }

        /// #611: the cancellation release must land where the pointer ACTUALLY
        /// IS, not at the drag origin.
        ///
        /// `PointerGestureState` carried only `down_point`, written at Down and
        /// never updated on Move, and the cancellation path fed that straight
        /// into the synthetic release. Because `post_mouse` for an Up also
        /// warps the cursor to the release point, a revoke mid-drag both moved
        /// the pointer back to the origin and released there -- a
        /// drag-and-drop cancelled in flight dropped its content the whole
        /// drag distance away. Measured live 2026-07-28: released at (140,70)
        /// for a drag that ended at (420,70).
        ///
        /// The sibling test above never sends a Move before cancelling, so
        /// `down_point` and "current position" were trivially identical there
        /// and the bug was invisible; it also asserts only that *a* LeftUp
        /// appears, never WHERE. This drives the real dispatcher
        /// (`replay_with_backends`) for Down + drag Move and then the real
        /// cancellation entry point, and asserts the coordinate.
        #[test]
        fn cancelling_a_moved_session_tap_drag_releases_where_the_pointer_is() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            fn first_point(tap: &RecordingSessionTap, want: MouseKind) -> Option<(i64, i64)> {
                tap.events().into_iter().find_map(|event| match event {
                    TapEvent::Mouse(kind, x, y) if kind == want => Some((x, y)),
                    _ => None,
                })
            }

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let drag = pointer_message(
                RemoteControlAction::Move,
                0.80,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            for message in [&down, &drag] {
                replay_with_backends(message, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .expect("session tap should service the drag");
            }

            let pressed = first_point(&tap, MouseKind::LeftDown).expect("the Down was posted");
            let dragged = first_point(&tap, MouseKind::LeftDragged).expect("the Move was posted");
            // Positive control: without a real displacement here, "released at
            // the drag end" and "released at the origin" are the same
            // assertion and this test proves nothing.
            assert_ne!(
                pressed, dragged,
                "the drag Move must actually move the pointer, or this test cannot distinguish the two coordinates"
            );

            // Control is torn down mid-drag (revoke / disconnect / share ended).
            let released = release_session_tap_gestures_with_backend(down.window_id, None, &tap);
            assert_eq!(released, 1, "the open gesture must be found and released");

            let up = first_point(&tap, MouseKind::LeftUp)
                .expect("a cancelled drag must post its mouse-up");
            assert_eq!(
                up, dragged,
                "#611: the cancellation release must land where the drag ended"
            );
            assert_ne!(
                up, pressed,
                "#611: releasing at the drag origin warps the cursor back and drops content in the wrong place"
            );
        }

        /// #446 acceptance A7, as the live harness actually exercises it:
        /// `remote-control-disable` -> `revoke_window` ->
        /// `drain_window_control`.
        ///
        /// `drain_window_control` released the held session-tap button on its
        /// LAST line, but cleared the caches on its FIRST -- and
        /// `clear_control_caches_for_window` reaches
        /// `clear_ax_gesture_for_window`, which keeps only an opted-in SlDrag
        /// and drops a session-tap gesture. So the release always found an
        /// empty map. Live evidence (2026-07-28, host log):
        /// `AX pointer up window_id=10768 mode=<none> outcome=Handled` and
        /// the target received no mouse-up at all.
        ///
        /// This drives the REAL `drain_window_control` on a gesture opened by
        /// the REAL dispatcher, and observes the map at the moment the
        /// release runs -- an ordering defect is invisible to any test that
        /// calls the two halves itself.
        #[test]
        fn draining_a_window_releases_the_held_button_before_clearing_its_gesture() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static GESTURES_VISIBLE_AT_RELEASE: AtomicUsize = AtomicUsize::new(usize::MAX);

            fn observe_release(_window_id: u32) {
                GESTURES_VISIBLE_AT_RELEASE.store(ax_gesture_count_for_tests(), Ordering::SeqCst);
            }

            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            GESTURES_VISIBLE_AT_RELEASE.store(usize::MAX, Ordering::SeqCst);
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            assert_eq!(tap.mouse_kinds(), vec![MouseKind::LeftDown]);

            super::super::drain_window_control_releasing(down.window_id, observe_release);

            assert_eq!(
                GESTURES_VISIBLE_AT_RELEASE.load(Ordering::SeqCst),
                1,
                "the held-button release must run while the gesture record still exists; \
                 a record cleared first can never be released"
            );

            // `observe_release` stands in for the real release, so close the
            // cursor takeover this gesture opened -- it is keyed by window id
            // and would otherwise leak into the next test.
            release_session_tap_gestures_with_backend(down.window_id, None, &tap);
        }

        /// #446 acceptance A7: revoking a SINGLE controller mid-drag.
        ///
        /// `revoke` / `revoke_controller` never reach
        /// `drain_window_control` -- they tear down through
        /// `clear_ax_gesture_for_controller`, which simply DELETED a
        /// session-tap gesture (only an opted-in SlDrag was retained). Live
        /// evidence: the revoke logged `mode=<none> outcome=Handled` and
        /// posted nothing, leaving the target app with the button held.
        ///
        /// This drives the real chain -- the dispatcher records the gesture,
        /// the real teardown function releases it -- not the release helper
        /// on a hand-built map entry.
        #[test]
        fn revoking_one_controller_mid_drag_posts_the_missing_mouse_up() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            assert_eq!(tap.mouse_kinds(), vec![MouseKind::LeftDown]);

            // Exactly what `revoke()` and `revoke_controller()` call.
            clear_ax_gesture_for_controller_with_backend(down.window_id, &down.controller_id, &tap);

            assert!(
                tap.mouse_kinds().contains(&MouseKind::LeftUp),
                "revoking a controller mid-drag must post the synthetic release, or the \
                 target app is left with a phantom held button: {:?}",
                tap.events()
            );
            assert_eq!(
                ax_gesture_count_for_tests(),
                0,
                "the released gesture must not stay in the map"
            );
        }

        /// The release must be scoped to the revoking controller: #374 allows
        /// two controllers to drag the same window concurrently, and one
        /// revoking must not release the other's held button or hand back the
        /// cursor mid-gesture.
        #[test]
        fn revoking_one_controller_leaves_a_concurrent_controllers_drag_held() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let mut first = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            first.controller_id = "viewer-a".to_string();
            let mut second = first.clone();
            second.controller_id = "viewer-b".to_string();

            for message in [&first, &second] {
                replay_with_backends(message, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                    .unwrap();
            }

            clear_ax_gesture_for_controller_with_backend(
                first.window_id,
                &first.controller_id,
                &tap,
            );

            assert_eq!(
                tap.mouse_kinds()
                    .iter()
                    .filter(|kind| **kind == MouseKind::LeftUp)
                    .count(),
                1,
                "only the revoking controller's gesture may be released: {:?}",
                tap.events()
            );
            assert_eq!(
                ax_gesture_count_for_tests(),
                1,
                "the other controller's gesture must survive"
            );
            assert!(
                !matches!(tap.events().last(), Some(TapEvent::MoveCursor(..))),
                "the cursor must not be handed back while another controller is still \
                 mid-gesture on this window: {:?}",
                tap.events()
            );

            // This test deliberately leaves viewer-b mid-gesture; drain it so
            // the window-keyed cursor takeover does not leak into the next
            // test and suppress its raise/warp.
            release_session_tap_gestures_with_backend(first.window_id, None, &tap);
        }

        #[test]
        fn wheel_on_custom_drawn_content_is_delivered_by_the_session_tap() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            // No scroll state configured -> AX reports it cannot scroll this
            // element, which is the custom-drawn-content case.
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let wheel = wheel_message(0.0, -120.0, None);
            replay_with_backends(&wheel, frame, Some(1234), &sink, &ax, &sl, &pb, &tap)
                .expect("session tap should service the wheel");

            assert!(
                tap.events()
                    .iter()
                    .any(|event| matches!(event, TapEvent::Scroll(..))),
                "wheel must reach the session tap, not vanish into PassThrough"
            );
            assert!(sink.events().is_empty());
        }

        #[test]
        fn hover_moves_never_take_the_cursor_over() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = ax_hostile_backend();
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            // buttons = 0 -> a hover, not a drag.
            let hover = pointer_message(
                RemoteControlAction::Move,
                0.30,
                0.30,
                RemoteControlButton::Left,
                0,
            );
            replay_with_backends(&hover, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();

            assert!(
                tap.events().is_empty(),
                "a buttonless hover must never hijack the host cursor"
            );
        }

        #[test]
        fn ax_keeps_priority_over_the_session_tap_when_the_element_is_actionable() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            // A pressable element: the semantic tier must win and the cursor
            // must never move.
            let ax = RecordingAxBackend::default();
            ax.resolve_to(81);
            ax.set_capabilities(
                81,
                AxCapabilities {
                    pressable: true,
                    ..AxCapabilities::default()
                },
            );
            let sl = RecordingSlClickBackend::unavailable();
            let sink = RecordingSink::default();
            let pb = RecordingPasteboard::default();
            let tap = RecordingSessionTap::trusted();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );
            replay_with_backends(&down, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();
            replay_with_backends(&up, frame, Some(1234), &sink, &ax, &sl, &pb, &tap).unwrap();

            assert!(
                tap.events().is_empty(),
                "AX must stay authoritative -- no cursor takeover when a semantic action exists"
            );
        }

        // #373: Middle click used to be a hardcoded no-op two places deep --
        // `post_sl_click_with_priming` returned PassThrough for Middle before
        // ever asking the backend, and `post_sl_mouse_click` (the real
        // SLEventPostToPid-backed implementation) errored Unavailable for
        // Middle even if asked. Both are fixed; this covers the dispatch
        // level (the FFI-backed `post_sl_mouse_click` itself is covered by
        // `middle_click_synthesizes_other_mouse_events_via_sl_backend` below).
        #[test]
        fn middle_click_reaches_sl_backend_instead_of_short_circuiting_passthrough() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(50);
            ax.set_capabilities(50, AxCapabilities::default());
            let sl = RecordingSlClickBackend::available();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Middle,
                4,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Middle,
                0,
            );

            // Middle never goes through AX (no AXPress semantics for a
            // middle-click), so down is PassThrough. With no opt-in SL drag
            // route, replay reports this as a real injection failure rather
            // than posting the known-ineffective CGEvent fallback.
            replay_events_with_backends(&down, frame, &ax, &sl);
            // ...but up now actually reaches the SL backend and succeeds
            // (Handled), rather than being swallowed as PassThrough before
            // ever asking the backend -- so no CGEvent fallback is recorded.
            assert!(replay_events_with_backends(&up, frame, &ax, &sl).is_empty());
            assert_eq!(ax.ops(), Vec::new());

            let clicks = sl.clicks();
            assert_eq!(
                clicks.len(),
                2,
                "expected a primer click plus the real click"
            );
            assert!(clicks
                .iter()
                .all(|click| click.button == RemoteControlButton::Middle));
        }

        // #369: middle click now has a SkyLight route, but if SkyLight is
        // unavailable the pointer sequence must report a real failure rather
        // than fall back to the ineffective CGEventPostToPid path.
        #[test]
        fn cancelled_injection_never_reaches_the_sink_dispatch() {
            // Fable-review fix (#369), second pass: an abandoned injection's
            // `AxReplayOutcome::PassThrough` must not fall through to the
            // CGEvent/SkyLight sink -- otherwise an abandoned pressable Down
            // (which used to return Handled, no sink post at all) could post
            // a stale mouse-down seconds after its matching Up already
            // posted the up, leaving a phantom held button on the target
            // app. Uses the same Down message as
            // `middle_click_routes_through_skylight_when_available` (which
            // asserts the OPPOSITE: `!down_events.is_empty()` when not
            // cancelled) to prove the guard, not just a different scenario,
            // is what suppresses the dispatch.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::available();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Middle,
                4,
            );

            let _cancelled = super::super::InjectionCancelledForTests::set();
            let down_events = replay_events_with_backends(&down, frame, &ax, &sl);

            assert!(
                down_events.is_empty(),
                "a cancelled injection must not post anything to the CGEvent sink"
            );
            assert!(
                sl.clicks().is_empty(),
                "a cancelled injection must not post anything via SkyLight either"
            );
        }

        #[test]
        fn cancelled_sl_drag_up_still_posts_the_release() {
            // #446 review finding: a SlDrag gesture is the ONE case where
            // something is physically held (a real posted mouse-down)
            // before Up-time -- unlike every AX mode, where an abandoned
            // late action is safely skippable because nothing OS-level is
            // held yet. If a cancelled Up for a SlDrag gesture were dropped
            // the same way (per cancelled_injection_never_reaches_the_sink_dispatch,
            // above), the target app would be left with a permanently
            // stuck mouse-down -- worse than the bug this whole fallback
            // exists to fix. The release must still be posted.
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            insert_sl_drag_gesture_for_tests(42, "viewer");
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::available();

            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            let _cancelled = super::super::InjectionCancelledForTests::set();
            let _ = replay_events_with_backends(&up, frame, &ax, &sl);

            assert_eq!(
                sl.events(),
                vec![SlMouseEvent::Up; SL_RELEASE_ATTEMPTS],
                "a cancelled Up for a SlDrag gesture must still post the SkyLight release \
                 (retried SL_RELEASE_ATTEMPTS times), or the target app is left with a \
                 permanently held mouse button"
            );
        }

        #[test]
        fn skylight_release_retries_after_transient_post_failure() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            insert_sl_drag_gesture_for_tests(42, "viewer");
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::available();
            sl.fail_next_up_attempts(2);

            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            assert!(replay_events_with_backends(&up, frame, &ax, &sl).is_empty());
            assert_eq!(sl.events(), vec![SlMouseEvent::Up; SL_RELEASE_ATTEMPTS]);
        }

        #[test]
        fn middle_click_reports_injection_failure_when_skylight_unavailable() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            let sl = RecordingSlClickBackend::unavailable();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Middle,
                4,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Middle,
                0,
            );

            let mut events = Vec::new();
            events.extend(replay_events_with_backends(&down, frame, &ax, &sl));
            events.extend(replay_events_with_backends(&up, frame, &ax, &sl));

            assert_eq!(sl.clicks(), Vec::new());
            assert!(events.is_empty());
        }

        // Covers the real FFI-backed implementation directly: previously
        // `post_sl_mouse_click` had an explicit `RemoteControlButton::Middle
        // => return Err(SlClickError::Unavailable)` arm. A mock
        // `SlEventPostToPidFn` (matching the real `SLEventPostToPid` C ABI)
        // stands in for the dlopen'd symbol so this exercises the real
        // event-construction code without touching the live event stream.
        #[test]
        fn middle_click_synthesizes_other_mouse_events_via_sl_backend() {
            unsafe extern "C" fn noop_post_to_pid(_pid: i32, _event: *mut c_void) {}

            let point = super::super::GlobalPoint { x: 12.0, y: 34.0 };
            let result = post_sl_mouse_click(
                noop_post_to_pid,
                4321,
                point,
                RemoteControlButton::Middle,
                1,
            );
            assert_eq!(result, Ok(()));
        }

        // #373: a double-click down on a text-selectable (non-pressable)
        // element must NOT enter AxText caret-placement mode -- it should
        // fall through to PassThrough so the eventual up posts a real
        // click_state=2 SL/CGEvent click, letting the target app's own
        // mouseDown handler select the word (our own offset+set_selected_range
        // can only place a caret, never select a word).
        #[test]
        fn double_click_on_text_view_bypasses_ax_caret_and_uses_click_state_two() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(60);
            ax.set_capabilities(
                60,
                AxCapabilities {
                    text_selectable: true,
                    ..AxCapabilities::default()
                },
            );
            ax.set_offset(60, 10.0, 10.0, 3);
            let sl = RecordingSlClickBackend::available();

            let down = with_click_count(
                pointer_message(
                    RemoteControlAction::Down,
                    0.10,
                    0.10,
                    RemoteControlButton::Left,
                    1,
                ),
                2,
            );
            let up = with_click_count(
                pointer_message(
                    RemoteControlAction::Up,
                    0.10,
                    0.10,
                    RemoteControlButton::Left,
                    0,
                ),
                2,
            );

            // Down resolves to PassThrough (bypassing AxText caret mode), so
            // it reports failure when the opt-in SL route is unavailable.
            replay_events_with_backends(&down, frame, &ax, &sl);
            // Up reaches the SL backend and succeeds, so no failure is
            // recorded for it.
            assert!(replay_events_with_backends(&up, frame, &ax, &sl).is_empty());

            // No AX caret/selection op was performed -- the click was routed
            // to the SL backend instead.
            assert_eq!(ax.ops(), Vec::new());
            let clicks = sl.clicks();
            assert!(
                clicks.iter().any(|click| click.click_state == 2),
                "expected a click_state=2 SL click, got {clicks:?}"
            );
        }

        // Baseline contrast for the test above: a plain single click (no
        // clickCount, or clickCount=1) on the same text-selectable element
        // still goes through the existing AX caret-placement path unchanged.
        #[test]
        fn single_click_on_text_view_still_uses_ax_caret_placement() {
            let _guard = ax_test_lock();
            clear_all_ax_control_state();
            let frame = unit_frame();
            let ax = RecordingAxBackend::default();
            ax.resolve_to(61);
            ax.set_capabilities(
                61,
                AxCapabilities {
                    text_selectable: true,
                    ..AxCapabilities::default()
                },
            );
            ax.set_offset(61, 10.0, 10.0, 3);
            let sl = RecordingSlClickBackend::available();

            let down = pointer_message(
                RemoteControlAction::Down,
                0.10,
                0.10,
                RemoteControlButton::Left,
                1,
            );
            let up = pointer_message(
                RemoteControlAction::Up,
                0.10,
                0.10,
                RemoteControlButton::Left,
                0,
            );

            assert!(replay_events_with_backends(&down, frame, &ax, &sl).is_empty());
            assert!(replay_events_with_backends(&up, frame, &ax, &sl).is_empty());

            assert_eq!(
                ax.ops(),
                vec![AxOp::SetSelectedRange {
                    id: 61,
                    start: 3,
                    len: 0,
                }]
            );
            assert_eq!(sl.clicks(), Vec::new());
        }

        #[test]
        fn recording_sink_replays_command_key_without_unicode_fallback() {
            let message = key_message(
                "KeyC",
                "c",
                RemoteControlModifiers {
                    meta: true,
                    ..RemoteControlModifiers::default()
                },
            );

            assert_eq!(
                replay_events(
                    &message,
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    }
                ),
                vec![SynthEvent::Key {
                    keycode: 8,
                    down: true,
                    flags: K_CG_EVENT_FLAG_MASK_COMMAND,
                    unicode: None,
                }]
            );
        }

        #[test]
        fn recording_sink_replays_modifier_masks_on_keys() {
            let message = key_message(
                "KeyA",
                "a",
                RemoteControlModifiers {
                    shift: true,
                    ctrl: true,
                    alt: true,
                    meta: true,
                },
            );

            assert_eq!(
                replay_events(
                    &message,
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    }
                ),
                vec![SynthEvent::Key {
                    keycode: 0,
                    down: true,
                    flags: K_CG_EVENT_FLAG_MASK_SHIFT
                        | K_CG_EVENT_FLAG_MASK_CONTROL
                        | K_CG_EVENT_FLAG_MASK_ALTERNATE
                        | K_CG_EVENT_FLAG_MASK_COMMAND,
                    unicode: None,
                }]
            );
        }

        #[test]
        fn recording_sink_routes_plain_printable_keys_through_text_path() {
            for (code, key, expected_text) in [
                ("KeyZ", "z", "z"),
                ("KeyY", "z", "z"),
                ("Digit1", "1", "1"),
                ("Minus", "-", "-"),
                ("Numpad0", "0", "0"),
                ("KeyA", "A", "A"),
            ] {
                let message = key_message(code, key, RemoteControlModifiers::default());
                assert_eq!(
                    replay_events(
                        &message,
                        WindowFrame {
                            x: 0,
                            y: 0,
                            width: 100,
                            height: 100,
                        }
                    ),
                    vec![SynthEvent::Text {
                        s: expected_text.to_string(),
                    }],
                    "{code}"
                );
            }
        }

        #[test]
        fn recording_sink_keeps_navigation_function_and_unknown_keys_off_text_path() {
            for (code, key, expected_keycode) in [
                ("F5", "F5", 96),
                ("ArrowLeft", "ArrowLeft", 123),
                ("Enter", "Enter", 36),
            ] {
                let message = key_message(code, key, RemoteControlModifiers::default());
                assert_eq!(
                    replay_events(
                        &message,
                        WindowFrame {
                            x: 0,
                            y: 0,
                            width: 100,
                            height: 100,
                        }
                    ),
                    vec![SynthEvent::Key {
                        keycode: expected_keycode,
                        down: true,
                        flags: 0,
                        unicode: None,
                    }],
                    "{code}"
                );
            }

            let unmapped = key_message(
                "AudioVolumeUp",
                "AudioVolumeUp",
                RemoteControlModifiers::default(),
            );
            assert_eq!(
                replay_events(
                    &unmapped,
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    }
                ),
                Vec::<SynthEvent>::new()
            );
        }

        #[test]
        fn recording_sink_replays_scroll_modes_with_horizontal_axis_and_vertical_sign() {
            let frame = WindowFrame {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            };
            for (mode, axis1, axis2) in [(None, 3, -2), (Some(1), 120, -80), (Some(2), 1800, -1600)]
            {
                assert_eq!(
                    replay_events(&wheel_message(2.0, -3.0, mode), frame),
                    vec![
                        SynthEvent::Mouse {
                            kind: MouseKind::Moved,
                            x: 410.0,
                            y: 320.0,
                            button: RemoteControlButton::Left,
                            click_state: 0,
                            flags: 0,
                        },
                        SynthEvent::Scroll {
                            axis1,
                            axis2,
                            x: 410.0,
                            y: 320.0,
                            unit: ScrollUnit::Pixel,
                            flags: 0,
                        },
                    ]
                );
            }
        }

        #[test]
        fn recording_sink_replays_scroll_with_target_point() {
            let frame = WindowFrame {
                x: -100,
                y: 50,
                width: 640,
                height: 480,
            };
            let mut message = wheel_message(0.0, 1.0, Some(1));
            message.x = Some(0.25);
            message.y = Some(0.75);

            let events = replay_events(&message, frame);

            assert_eq!(
                events[1],
                SynthEvent::Scroll {
                    axis1: -40,
                    axis2: 0,
                    x: 60.0,
                    y: 410.0,
                    unit: ScrollUnit::Pixel,
                    flags: 0,
                }
            );
        }

        #[test]
        fn recording_sink_replays_right_drag_from_buttons_bitmask() {
            let mut message = base_message(RemoteControlType::Pointer);
            message.action = Some(RemoteControlAction::Move);
            message.x = Some(0.5);
            message.y = Some(0.25);
            message.button = Some(0);
            message.buttons = Some(2);

            assert_eq!(
                replay_events(
                    &message,
                    WindowFrame {
                        x: 10,
                        y: 20,
                        width: 200,
                        height: 100,
                    }
                ),
                vec![SynthEvent::Mouse {
                    kind: MouseKind::RightDragged,
                    x: 110.0,
                    y: 45.0,
                    button: RemoteControlButton::Right,
                    click_state: 1,
                    flags: 0,
                }]
            );
        }

        #[test]
        fn recording_sink_marks_drag_sequence_with_click_state() {
            let frame = WindowFrame {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            };
            let mut events = Vec::new();

            let mut down = base_message(RemoteControlType::Pointer);
            down.action = Some(RemoteControlAction::Down);
            down.x = Some(0.10);
            down.y = Some(0.20);
            down.button = Some(0);
            down.buttons = Some(1);
            events.extend(replay_events(&down, frame));

            for step in 0..10 {
                let mut drag = base_message(RemoteControlType::Pointer);
                drag.action = Some(RemoteControlAction::Move);
                drag.x = Some(0.10 + f64::from(step) * 0.01);
                drag.y = Some(0.20);
                drag.button = Some(0);
                drag.buttons = Some(1);
                events.extend(replay_events(&drag, frame));
            }

            let mut up = base_message(RemoteControlType::Pointer);
            up.action = Some(RemoteControlAction::Up);
            up.x = Some(0.20);
            up.y = Some(0.20);
            up.button = Some(0);
            up.buttons = Some(0);
            events.extend(replay_events(&up, frame));

            assert_eq!(events.len(), 12);
            assert!(events
                .iter()
                .all(|event| matches!(event, SynthEvent::Mouse { click_state: 1, .. })));
        }

        #[test]
        fn recording_sink_leaves_hover_move_click_state_unset() {
            let mut message = base_message(RemoteControlType::Pointer);
            message.action = Some(RemoteControlAction::Move);
            message.x = Some(0.5);
            message.y = Some(0.25);
            message.buttons = Some(0);

            assert_eq!(
                replay_events(
                    &message,
                    WindowFrame {
                        x: 10,
                        y: 20,
                        width: 200,
                        height: 100,
                    }
                ),
                vec![SynthEvent::Mouse {
                    kind: MouseKind::Moved,
                    x: 110.0,
                    y: 45.0,
                    button: RemoteControlButton::Left,
                    click_state: 0,
                    flags: 0,
                }]
            );
        }

        #[test]
        fn recording_sink_replays_clamped_pointer_coordinates() {
            let mut message = base_message(RemoteControlType::Pointer);
            message.action = Some(RemoteControlAction::Move);
            message.x = Some(2.0);
            message.y = Some(-1.0);
            message.buttons = Some(0);

            assert_eq!(
                replay_events(
                    &message,
                    WindowFrame {
                        x: -200,
                        y: 10,
                        width: 100,
                        height: 50,
                    }
                ),
                vec![SynthEvent::Mouse {
                    kind: MouseKind::Moved,
                    x: -100.0,
                    y: 10.0,
                    button: RemoteControlButton::Left,
                    click_state: 0,
                    flags: 0,
                }]
            );
        }

        #[test]
        fn keycode_for_maps_named_navigation_and_editing_keys() {
            assert_eq!(keycode_for("Enter", ""), Some(36));
            assert_eq!(keycode_for("NumpadEnter", ""), Some(36));
            assert_eq!(keycode_for("Tab", ""), Some(48));
            assert_eq!(keycode_for("Space", ""), Some(49));
            assert_eq!(keycode_for("Backspace", ""), Some(51));
            assert_eq!(keycode_for("Escape", ""), Some(53));
            assert_eq!(keycode_for("Delete", ""), Some(117));
            assert_eq!(keycode_for("Home", ""), Some(115));
            assert_eq!(keycode_for("End", ""), Some(119));
            assert_eq!(keycode_for("PageUp", ""), Some(116));
            assert_eq!(keycode_for("PageDown", ""), Some(121));
        }

        #[test]
        fn keycode_for_maps_common_code_table() {
            assert_eq!(keycode_for("KeyA", "a"), Some(0));
            assert_eq!(keycode_for("KeyC", "c"), Some(8));
            assert_eq!(keycode_for("Digit1", "1"), Some(18));
            assert_eq!(keycode_for("Digit0", "0"), Some(29));
            assert_eq!(keycode_for("Minus", "-"), Some(27));
            assert_eq!(keycode_for("Equal", "="), Some(24));
            assert_eq!(keycode_for("BracketLeft", "["), Some(33));
            assert_eq!(keycode_for("Backslash", "\\"), Some(42));
            assert_eq!(keycode_for("Semicolon", ";"), Some(41));
            assert_eq!(keycode_for("Quote", "'"), Some(39));
            assert_eq!(keycode_for("Comma", ","), Some(43));
            assert_eq!(keycode_for("Period", "."), Some(47));
            assert_eq!(keycode_for("Slash", "/"), Some(44));
            assert_eq!(keycode_for("Backquote", "`"), Some(50));
            assert_eq!(keycode_for("F13", "F13"), Some(105));
            assert_eq!(keycode_for("F20", "F20"), Some(90));
            assert_eq!(keycode_for("ShiftRight", "Shift"), Some(60));
            assert_eq!(keycode_for("MetaRight", "Meta"), Some(54));
            assert_eq!(keycode_for("Numpad0", "0"), Some(82));
            assert_eq!(keycode_for("NumpadEnter", "Enter"), Some(36));
        }

        #[test]
        fn keycode_for_maps_arrow_codes() {
            assert_eq!(keycode_for("ArrowLeft", ""), Some(123));
            assert_eq!(keycode_for("ArrowRight", ""), Some(124));
            assert_eq!(keycode_for("ArrowDown", ""), Some(125));
            assert_eq!(keycode_for("ArrowUp", ""), Some(126));
        }

        #[test]
        fn keycode_for_falls_back_to_key_value_for_common_controls() {
            assert_eq!(keycode_for("", "\n"), Some(36));
            assert_eq!(keycode_for("", "Enter"), Some(36));
            assert_eq!(keycode_for("", "\t"), Some(48));
            assert_eq!(keycode_for("", "Tab"), Some(48));
            assert_eq!(keycode_for("", " "), Some(49));
            assert_eq!(keycode_for("", "Space"), Some(49));
        }

        #[test]
        fn keycode_for_returns_none_for_unknown_codes() {
            assert_eq!(keycode_for("AudioVolumeUp", "AudioVolumeUp"), None);
            assert_eq!(keycode_for("", ""), None);
        }

        #[test]
        fn key_replay_plan_drops_unknown_non_printable_keys() {
            let message = key_message(
                "AudioVolumeUp",
                "AudioVolumeUp",
                RemoteControlModifiers::default(),
            );
            assert_eq!(key_replay_plan(&message, true), None);
            assert_eq!(key_replay_plan(&message, false), None);
        }

        #[test]
        fn key_replay_plan_uses_text_for_plain_printable_key_down_only() {
            let printable = key_message("", "é", RemoteControlModifiers::default());
            assert_eq!(
                key_replay_plan(&printable, true),
                Some(KeyReplayPlan::Text("é".to_string()))
            );
            assert_eq!(key_replay_plan(&printable, false), None);

            let qwertz_intent = key_message("KeyY", "z", RemoteControlModifiers::default());
            assert_eq!(
                key_replay_plan(&qwertz_intent, true),
                Some(KeyReplayPlan::Text("z".to_string()))
            );
        }

        #[test]
        fn key_replay_plan_keeps_shortcuts_and_repeats_on_virtual_key_path() {
            let shortcut = key_message(
                "KeyC",
                "c",
                RemoteControlModifiers {
                    meta: true,
                    ..RemoteControlModifiers::default()
                },
            );
            assert_eq!(
                key_replay_plan(&shortcut, true),
                Some(KeyReplayPlan::VirtualKey {
                    virtual_key: 8,
                    unicode: None,
                })
            );

            let mut repeated = key_message("KeyA", "a", RemoteControlModifiers::default());
            repeated.repeat = true;
            assert_eq!(
                key_replay_plan(&repeated, true),
                Some(KeyReplayPlan::VirtualKey {
                    virtual_key: 0,
                    unicode: Some("a".to_string()),
                })
            );
        }

        #[test]
        fn replay_text_caps_large_pastes_before_injection() {
            let mut message = base_message(RemoteControlType::Text);
            message.text = Some(format!("{}tail", "a".repeat(MAX_REPLAY_TEXT_CHARS)));

            assert_eq!(
                replay_events(
                    &message,
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    }
                ),
                vec![SynthEvent::Text {
                    s: "a".repeat(MAX_REPLAY_TEXT_CHARS),
                }]
            );
        }

        #[test]
        fn modifier_flags_map_to_core_graphics_flags() {
            assert_eq!(
                cg_flags_for_modifiers(&RemoteControlModifiers::default()),
                0
            );
            assert_eq!(
                cg_flags_for_modifiers(&RemoteControlModifiers {
                    shift: true,
                    ctrl: true,
                    alt: true,
                    meta: true,
                }),
                K_CG_EVENT_FLAG_MASK_SHIFT
                    | K_CG_EVENT_FLAG_MASK_CONTROL
                    | K_CG_EVENT_FLAG_MASK_ALTERNATE
                    | K_CG_EVENT_FLAG_MASK_COMMAND
            );
        }

        #[test]
        fn wheel_delta_pixels_converts_pixel_line_and_page_modes() {
            assert_eq!(
                wheel_delta_pixels(
                    &wheel_message(1.4, -2.6, None),
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 800,
                        height: 600,
                    }
                ),
                (1, -3)
            );
            assert_eq!(
                wheel_delta_pixels(
                    &wheel_message(1.5, -2.0, Some(1)),
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 800,
                        height: 600,
                    }
                ),
                (60, -80)
            );
            assert_eq!(
                wheel_delta_pixels(
                    &wheel_message(1.0, -0.5, Some(2)),
                    WindowFrame {
                        x: 0,
                        y: 0,
                        width: 800,
                        height: 600,
                    }
                ),
                (800, -300)
            );
        }

        #[test]
        fn pointer_event_kind_maps_actions_and_buttons_to_cg_event_codes() {
            assert_eq!(
                pointer_event_kind(RemoteControlAction::Move, RemoteControlButton::Right),
                (MouseKind::Moved, RemoteControlButton::Right)
            );
            assert_eq!(
                pointer_event_kind(RemoteControlAction::Down, RemoteControlButton::Left),
                (MouseKind::LeftDown, RemoteControlButton::Left)
            );
            assert_eq!(
                pointer_event_kind(RemoteControlAction::Up, RemoteControlButton::Right),
                (MouseKind::RightUp, RemoteControlButton::Right)
            );
            assert_eq!(
                pointer_event_kind(RemoteControlAction::Down, RemoteControlButton::Middle),
                (MouseKind::OtherDown, RemoteControlButton::Middle)
            );
            assert_eq!(
                pointer_event_kind(RemoteControlAction::Up, RemoteControlButton::Middle),
                (MouseKind::OtherUp, RemoteControlButton::Middle)
            );
        }

        #[test]
        fn pointer_drag_uses_buttons_bitmask_for_held_button() {
            assert_eq!(
                pointer_event_for(RemoteControlAction::Move, Some(0), Some(2), None),
                (MouseKind::RightDragged, RemoteControlButton::Right, 1)
            );
            assert_eq!(
                pointer_event_for(RemoteControlAction::Move, Some(0), Some(4), None),
                (MouseKind::OtherDragged, RemoteControlButton::Middle, 1)
            );
            assert_eq!(
                pointer_event_for(RemoteControlAction::Move, Some(2), Some(1), None),
                (MouseKind::LeftDragged, RemoteControlButton::Left, 1)
            );
        }

        #[test]
        fn pointer_down_click_state_defaults_to_one_without_click_count() {
            // Back-compat: an old peer that never sends `clickCount` must still
            // produce a plain single-click click_state, matching pre-#373
            // behavior exactly.
            assert_eq!(
                pointer_event_for(RemoteControlAction::Down, Some(0), Some(1), None),
                (MouseKind::LeftDown, RemoteControlButton::Left, 1)
            );
        }

        #[test]
        fn pointer_down_click_state_reflects_click_count() {
            assert_eq!(
                pointer_event_for(RemoteControlAction::Down, Some(0), Some(1), Some(2)),
                (MouseKind::LeftDown, RemoteControlButton::Left, 2)
            );
            assert_eq!(
                pointer_event_for(RemoteControlAction::Up, Some(0), Some(0), Some(3)),
                (MouseKind::LeftUp, RemoteControlButton::Left, 3)
            );
            // A zero click_count is nonsensical wire data; treat it the same as
            // absent rather than posting click_state=0 on a down/up.
            assert_eq!(
                pointer_event_for(RemoteControlAction::Down, Some(0), Some(1), Some(0)),
                (MouseKind::LeftDown, RemoteControlButton::Left, 1)
            );
        }

        #[test]
        fn drag_kind_maps_buttons_to_matching_cg_drag_events() {
            assert_eq!(drag_kind(RemoteControlButton::Left), MouseKind::LeftDragged);
            assert_eq!(
                drag_kind(RemoteControlButton::Right),
                MouseKind::RightDragged
            );
            assert_eq!(
                drag_kind(RemoteControlButton::Middle),
                MouseKind::OtherDragged
            );
        }

        #[test]
        fn ax_trust_cache_reuses_value_until_ttl_expires() {
            let mut cache = AxTrustCache::default();
            let start = Instant::now();
            let mut checks = 0;

            assert!(!cache.get_or_refresh(start, || {
                checks += 1;
                false
            }));
            assert_eq!(checks, 1);

            assert!(!cache.get_or_refresh(
                start + AX_TRUST_CACHE_TTL - Duration::from_millis(1),
                || {
                    checks += 1;
                    true
                }
            ));
            assert_eq!(checks, 1);

            assert!(cache.get_or_refresh(start + AX_TRUST_CACHE_TTL, || {
                checks += 1;
                true
            }));
            assert_eq!(checks, 2);
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod ax_resolution_cache_tests {
    use super::input::{AxCapabilities, AxElementHandle, AxPointKey, AxResolutionCache};
    use super::input::{AX_CLICK_DRAG_THRESHOLD_POINTS, AX_POINT_BUCKET, AX_RESOLUTION_CACHE_TTL};
    use std::time::{Duration, Instant};

    fn key(window_id: u32) -> AxPointKey {
        AxPointKey {
            window_id,
            x: 1,
            y: 2,
        }
    }

    fn cache_with_entry(window_id: u32, at: Instant) -> AxResolutionCache {
        let mut cache = AxResolutionCache::default();
        cache.insert_at(
            key(window_id),
            AxElementHandle::Test(7),
            AxCapabilities {
                pressable: true,
                ..AxCapabilities::default()
            },
            at,
        );
        cache
    }

    #[test]
    fn ax_resolution_cache_hit_reuses_element_and_capabilities() {
        let start = Instant::now();
        let mut cache = cache_with_entry(368, start);
        let cached = cache.get_at(key(368), start + Duration::from_millis(1));
        assert_eq!(cached.unwrap().element.test_id(), Some(7));
    }

    #[test]
    fn ax_resolution_cache_expires_at_ttl() {
        let start = Instant::now();
        let mut cache = cache_with_entry(368, start);
        assert!(cache
            .get_at(key(368), start + AX_RESOLUTION_CACHE_TTL)
            .is_none());
    }

    #[test]
    fn ax_resolution_cache_invalidates_on_control_frame_change() {
        let start = Instant::now();
        let mut cache = cache_with_entry(368, start);
        cache.invalidate_window(368);
        assert!(cache.get_at(key(368), start).is_none());
    }

    #[test]
    fn ax_resolution_cache_invalidates_on_ax_error() {
        let start = Instant::now();
        let mut cache = cache_with_entry(368, start);
        cache.invalidate_key(key(368));
        assert!(cache.get_at(key(368), start).is_none());
    }

    #[test]
    fn ax_point_bucket_does_not_exceed_click_precision() {
        // #368 F3: a cache-key bucket wider than the click-precision threshold
        // could serve a click the neighbouring control's cached element, so the
        // bucket must never exceed it.
        assert!(AX_POINT_BUCKET <= AX_CLICK_DRAG_THRESHOLD_POINTS);
    }
}

#[cfg(target_os = "windows")]
pub(crate) mod input {
    use super::RemoteControlMessage;

    pub fn release_session_tap_gestures_for_window(window_id: u32) {
        crate::windows_remote_control::clear_window(window_id);
    }
    use crate::platform::cg::WindowFrame;

    pub fn accessibility_trusted() -> bool {
        crate::windows_remote_control::available()
    }

    pub fn prompt_accessibility() -> bool {
        crate::windows_remote_control::prompt_accessibility()
    }

    pub fn clear_cached_ax_app_for_pid(pid: i32) {
        crate::windows_remote_control::clear_pid(pid);
    }

    pub fn clear_ax_resolution_cache_for_window(window_id: u32) {
        crate::windows_remote_control::clear_window(window_id);
    }

    pub fn clear_ax_gesture_for_window(window_id: u32) {
        crate::windows_remote_control::clear_window(window_id);
    }

    pub fn clear_ax_gesture_for_controller(window_id: u32, controller_id: &str) {
        crate::windows_remote_control::clear_controller(window_id, controller_id);
    }

    pub fn clear_all_ax_control_state() {
        crate::windows_remote_control::clear_all();
    }

    pub fn clear_all_ax_control_state_except_sl_drag() {
        crate::windows_remote_control::clear_all();
    }

    pub fn replay(
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
    ) -> Result<(), String> {
        crate::windows_remote_control::replay(message, frame, target_pid)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) mod input {
    use super::RemoteControlMessage;
    use crate::platform::cg::WindowFrame;

    pub fn accessibility_trusted() -> bool {
        false
    }

    pub fn prompt_accessibility() -> bool {
        false
    }

    pub fn clear_cached_ax_app_for_pid(_pid: i32) {}

    pub fn clear_ax_resolution_cache_for_window(_window_id: u32) {}

    pub fn clear_ax_gesture_for_window(_window_id: u32) {}

    pub fn clear_ax_gesture_for_controller(_window_id: u32, _controller_id: &str) {}

    pub fn clear_all_ax_control_state() {}

    pub fn clear_all_ax_control_state_except_sl_drag() {}

    /// Mirror of the macOS module's test serializer so cross-module tests
    /// can lock unconditionally.
    #[cfg(test)]
    pub(crate) fn ax_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use crate::sync_ext::MutexExt;
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock_unpoisoned()
    }

    #[cfg(test)]
    pub(super) fn ax_gesture_count_for_tests() -> usize {
        0
    }

    #[cfg(test)]
    pub(super) fn insert_pass_through_ax_gesture_for_tests(_window_id: u32, _controller_id: &str) {}

    #[cfg(test)]
    pub(super) fn insert_sl_drag_gesture_for_tests(_window_id: u32, _controller_id: &str) {}

    pub fn replay(
        _message: &RemoteControlMessage,
        _frame: WindowFrame,
        _target_pid: Option<i32>,
    ) -> Result<(), String> {
        Err("remote control replay is macOS-only".to_string())
    }
}

#[cfg(test)]
mod tests {

    /// Source-level, and deliberately so: the guarantee is that all THREE
    /// authorization points consult the per-share lock, which is a
    /// control-flow property no pure-function test can observe (CLAUDE.md's
    /// native-lifecycle rule -- a helper being correct proves nothing about
    /// whether the real path calls it). Dropping any one of these silently
    /// reopens remote control on a window the sharer locked.
    #[test]
    fn every_authorization_point_consults_the_per_share_lock() {
        let source = include_str!("remote_control.rs");
        // Count only production code: this test's own body names the symbol,
        // and counting the whole file would count the assertions themselves.
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(before, _)| before)
            .unwrap_or(source);
        let uses = production.matches("share_allows_remote_control").count();
        assert!(
            uses >= 3,
            "expected the per-share lock at the request gate, the input gate and \
             the consent re-check; found {uses} use(s)"
        );
        // The input gate must AND it with the meeting-wide policy, not replace
        // it: either one denying has to deny.
        assert!(
            production.contains("state.remote_control_allowed()\n                && state.share_allows_remote_control(message.window_id)"),
            "the input gate must AND the meeting policy with the per-share lock"
        );
    }

    use super::*;

    #[derive(Debug)]
    struct RemoteControlTestIds {
        window_id: u32,
        other_window_id: u32,
        controller_id: String,
        other_controller_id: String,
    }

    fn remote_control_test_ids(test_name: &str) -> RemoteControlTestIds {
        // Was a `DefaultHasher` over (test name, thread id, a stack address,
        // `SystemTime::now()` nanos) truncated to 27 bits (see #666). That
        // was collision-*possible*, not merely theoretically imperfect: (1)
        // the truncation to 27 bits puts the birthday bound at ~sqrt(2^27) ≈
        // 11.6k calls, far below what a long-lived suite accumulates over
        // many runs; (2) `SystemTime::now()` resolution is coarser than a
        // nanosecond on common platforms, so concurrent test threads calling
        // this within the same tick feed the hasher an identical timestamp;
        // and (3) a stack-local's address is a function of thread + call
        // depth, so two threads at the same call depth can supply the same
        // address too -- collapsing the hash's remaining "unique" inputs
        // down to just `test_name`, which is fixed per call site. An
        // `AtomicU32` counter can't do any of that: each `fetch_add` hands
        // out a value no other call in this process has ever received or
        // will ever receive again until the counter itself wraps (2^32
        // calls) -- uniqueness by construction, not by low collision
        // probability. Follows the same process-wide-counter convention as
        // `rooms::new_room_id` (rooms.rs) and the `NEXT_*_ID` statics in
        // `share_overlay.rs` / `share_border.rs`.
        static NEXT_TEST_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        // Reserve two consecutive values per call so `window_id` and
        // `other_window_id` (below) are always an adjacent, never-reused
        // pair -- no other concurrent call can ever be handed either one.
        let base = NEXT_TEST_ID.fetch_add(2, std::sync::atomic::Ordering::Relaxed);

        // Keep these in a high, non-camera-tagged range and reserve the next
        // id for same-test negative assertions. The controller ids include
        // the same suffix so pending/authorized entries from parallel tests
        // cannot overlap even though production state is still module-global.
        let window_id = 0x4000_0000 | (base & 0x0fff_fffe);
        RemoteControlTestIds {
            window_id,
            other_window_id: window_id + 1,
            controller_id: format!("{test_name}-controller-{base:x}"),
            other_controller_id: format!("{test_name}-other-{base:x}"),
        }
    }

    #[derive(Deserialize)]
    struct ContractFixture {
        topics: ContractTopics,
        #[serde(default, rename = "remoteControlMessages")]
        remote_control_messages: Vec<RemoteControlMessageFixture>,
        #[serde(rename = "remoteControlPointerFields")]
        #[serde(default)]
        remote_control_pointer_fields: Vec<String>,
        #[serde(default, rename = "remoteControlPacketPolicy")]
        remote_control_packet_policy: Vec<RemoteControlPacketPolicyFixture>,
        #[serde(default, rename = "remoteClipboardMessages")]
        remote_clipboard_messages: Vec<RemoteControlMessageFixture>,
        #[serde(rename = "remoteClipboardStreams")]
        remote_clipboard_streams: RemoteClipboardStreamFixture,
        rules: ContractRules,
        #[serde(rename = "remoteControlBinaryFrames")]
        remote_control_binary_frames: Vec<BinaryFixture>,
        #[serde(rename = "fnv1a32TestVectors", default)]
        fnv1a32_test_vectors: Vec<Fnv1a32Vector>,
    }

    #[derive(Deserialize)]
    struct Fnv1a32Vector {
        input: String,
        #[serde(rename = "hashHex")]
        hash_hex: String,
    }

    #[derive(Deserialize)]
    struct ContractRules {
        reliability: ReliabilityRule,
    }

    #[derive(Deserialize)]
    struct ReliabilityRule {
        lossy: Vec<String>,
        reliable: Vec<String>,
    }

    #[derive(Deserialize)]
    struct BinaryFixture {
        name: String,
        hex: String,
        length: usize,
    }

    #[derive(Deserialize)]
    struct RemoteControlMessageFixture {
        name: String,
        fields: Vec<String>,
        #[serde(default)]
        message: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct RemoteClipboardStreamFixture {
        topic: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        directions: Vec<String>,
        attributes: Vec<String>,
        #[serde(rename = "operationIdHexLength")]
        operation_id_hex_length: usize,
        #[serde(rename = "maxBytes")]
        max_bytes: usize,
        reliability: String,
        destination: String,
        #[serde(rename = "successSignals")]
        success_signals: HashMap<String, String>,
        #[serde(rename = "textRules")]
        text_rules: Vec<String>,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    struct RemoteControlPacketPolicyFixture {
        packet: String,
        reliability: String,
        destination: String,
        authority: String,
    }

    #[derive(Deserialize)]
    struct ContractTopics {
        #[serde(rename = "remoteControl")]
        remote_control: String,
        #[serde(rename = "remoteClipboardText")]
        remote_clipboard_text: String,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    fn frame(x: i32, y: i32, width: i32, height: i32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width,
            height,
        }
    }

    fn pointer_contract_fields(fixture: &ContractFixture) -> Vec<String> {
        if !fixture.remote_control_pointer_fields.is_empty() {
            return fixture.remote_control_pointer_fields.clone();
        }
        fixture
            .remote_control_messages
            .iter()
            .find(|message| message.name == "pointer-down")
            .map(|message| message.fields.clone())
            .expect("remote-control contract must include pointer fields")
    }

    fn pressed_input_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK.get_or_init(|| Mutex::new(())).lock_unpoisoned();
        pressed_inputs().lock_unpoisoned().clear();
        guard
    }
    #[cfg(target_os = "macos")]
    fn platform_input_test_lock() -> std::sync::MutexGuard<'static, ()> {
        input::ax_test_lock()
    }

    #[cfg(not(target_os = "macos"))]
    fn platform_input_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock_unpoisoned()
    }

    fn test_message(
        message_type: RemoteControlType,
        action: Option<RemoteControlAction>,
        seq: u64,
        window_id: u32,
        controller_id: String,
    ) -> RemoteControlMessage {
        RemoteControlMessage {
            v: VERSION,
            message_type,
            action,
            target_user_id: "native-a".to_string(),
            controller_id,
            window_id,
            seq,
            target_kind: None,
            share_instance_id: None,
            controller_capabilities: Vec::new(),
            host_capabilities: Vec::new(),
            reason: None,
            control_session_id: None,
            input_id: None,
            input_seq: None,
            operation_fingerprint_version: None,
            operation_fingerprint: None,
            outcome: None,
            delivery_route: None,
            failure_code: None,
            result_capability: None,
            x: None,
            y: None,
            button: None,
            buttons: None,
            click_count: None,
            delta_x: None,
            delta_y: None,
            delta_mode: None,
            key: None,
            code: None,
            repeat: false,
            location: None,
            text: None,
            status: None,
            message: None,
            grant_token: None,
            supports_binary_hot_path: false,
            modifiers: RemoteControlModifiers::default(),
        }
    }

    fn discrete_admission_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK.get_or_init(|| Mutex::new(())).lock_unpoisoned();
        *discrete_admissions().lock_unpoisoned() = DiscreteAdmissionState::default();
        guard
    }

    fn v2_discrete_message(ids: &RemoteControlTestIds, input_id: &str) -> RemoteControlMessage {
        let mut message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Click),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let control_session_id = authorize_shared(ids.window_id, &ids.controller_id);
        message.control_session_id = Some(control_session_id);
        message.input_id = Some(input_id.to_string());
        message.input_seq = Some(1);
        message.operation_fingerprint_version = Some(1);
        let admission = DiscreteAdmission {
            controller_id: message.controller_id.clone(),
            window_id: message.window_id,
            target_kind: message.target_kind,
            share_instance_id: message.share_instance_id.clone(),
            control_session_id: message.control_session_id.clone().unwrap(),
            input_id: message.input_id.clone().unwrap(),
            input_seq: message.input_seq.unwrap(),
            operation_fingerprint: String::new(),
        };
        message.operation_fingerprint = Some(canonical_operation_fingerprint(&message, &admission));
        message
    }

    fn v2_discrete_message_for_grant(
        ids: &RemoteControlTestIds,
        input_id: &str,
        control_session_id: &str,
    ) -> RemoteControlMessage {
        let mut message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Click),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        message.control_session_id = Some(control_session_id.to_string());
        message.input_id = Some(input_id.to_string());
        message.input_seq = Some(1);
        message.operation_fingerprint_version = Some(1);
        let admission = DiscreteAdmission {
            controller_id: message.controller_id.clone(),
            window_id: message.window_id,
            target_kind: message.target_kind,
            share_instance_id: message.share_instance_id.clone(),
            control_session_id: control_session_id.to_string(),
            input_id: input_id.to_string(),
            input_seq: 1,
            operation_fingerprint: String::new(),
        };
        message.operation_fingerprint = Some(canonical_operation_fingerprint(&message, &admission));
        message
    }

    #[test]
    fn v2_discrete_admission_requires_canonical_fingerprint_and_current_grant() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids(
            "v2_discrete_admission_requires_canonical_fingerprint_and_current_grant",
        );
        let message = v2_discrete_message(&ids, "input-a");
        let admission = v2_discrete_admission(&message).expect("v2 discrete admission");
        assert!(grant_is_current(&admission, Instant::now()));
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Admitted
        );

        let mut malformed = message;
        malformed.operation_fingerprint = Some("ABC".repeat(21) + "A");
        assert_eq!(
            admit_discrete_operation(
                &malformed,
                &v2_discrete_admission(&malformed).unwrap(),
                Instant::now()
            ),
            AdmissionDecision::Malformed
        );

        revoke(ids.window_id, &ids.controller_id);
        assert!(!grant_is_current(&admission, Instant::now()));
    }

    #[test]
    fn partial_v2_envelope_cannot_downgrade_to_legacy_replay() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids("partial_v2_envelope_cannot_downgrade_to_legacy_replay");
        let mut message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Click),
            1,
            ids.window_id,
            ids.controller_id,
        );
        message.control_session_id = Some("partial".to_string());
        assert!(is_v2_discrete_attempt(&message));
        assert!(v2_discrete_admission(&message).is_none());
    }

    #[test]
    fn early_v2_rejections_preserve_terminal_result_correlation() {
        let _guard = discrete_admission_test_lock();
        let ids =
            remote_control_test_ids("early_v2_rejections_preserve_terminal_result_correlation");
        let message = v2_discrete_message(&ids, "early-reject");
        let admission = malformed_v2_admission(&message).expect("correlatable v2 input");
        for disposition in [
            TerminalDisposition::failure(
                "unauthorized",
                RemoteControlDeliveryRoute::Admission,
                RemoteControlFailureCode::Unauthorized,
            ),
            TerminalDisposition::failure(
                "accessibilityDenied",
                RemoteControlDeliveryRoute::Admission,
                RemoteControlFailureCode::AccessibilityDenied,
            ),
            TerminalDisposition::failure(
                "grantExpired",
                RemoteControlDeliveryRoute::Admission,
                RemoteControlFailureCode::GrantExpired,
            ),
        ] {
            let packet = discrete_result_packet("native", &admission, disposition);
            assert_eq!(packet.input_id.as_deref(), Some("early-reject"));
            assert_eq!(
                packet.control_session_id.as_deref(),
                Some(admission.control_session_id.as_str())
            );
            assert_eq!(packet.outcome.as_deref(), Some(disposition.outcome));
            assert_eq!(packet.delivery_route, Some(disposition.delivery_route));
            assert_eq!(packet.failure_code, disposition.failure_code);
        }
    }

    fn assert_planned_terminal(
        action: InputDispatchAction,
        expected_outcome: &'static str,
        expected_input_id: &str,
    ) {
        let InputDispatchAction::Reject {
            terminal: Some((admission, outcome)),
            ..
        } = action
        else {
            panic!("expected a correlated terminal input action");
        };
        assert_eq!(outcome.outcome, expected_outcome);
        let packet = discrete_result_packet("native", &admission, outcome);
        assert_eq!(packet.message_type, RemoteControlType::Result);
        assert_eq!(packet.input_id.as_deref(), Some(expected_input_id));
        assert_eq!(packet.input_seq, Some(admission.input_seq));
        assert_eq!(
            packet.control_session_id.as_deref(),
            Some(admission.control_session_id.as_str())
        );
        assert_eq!(packet.outcome.as_deref(), Some(expected_outcome));
    }

    #[test]
    fn handler_input_plan_rejects_disabled_unauthorized_accessibility_and_expired_grants() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids(
            "handler_input_plan_rejects_disabled_unauthorized_accessibility_and_expired_grants",
        );
        let message = v2_discrete_message(&ids, "handler-reject");
        let pending = input_v2_snapshot_before_admission(&message);

        assert_planned_terminal(
            plan_input_dispatch(
                InputGateSnapshot {
                    remote_control_allowed: false,
                    authorized: true,
                    unreliable_seq_accepted: true,
                    accessibility_trusted: true,
                },
                pending.clone(),
            ),
            "unauthorized",
            "handler-reject",
        );
        assert_planned_terminal(
            plan_input_dispatch(
                InputGateSnapshot {
                    remote_control_allowed: true,
                    authorized: false,
                    unreliable_seq_accepted: true,
                    accessibility_trusted: true,
                },
                pending.clone(),
            ),
            "unauthorized",
            "handler-reject",
        );
        assert_planned_terminal(
            plan_input_dispatch(
                InputGateSnapshot {
                    remote_control_allowed: true,
                    authorized: true,
                    unreliable_seq_accepted: true,
                    accessibility_trusted: false,
                },
                pending,
            ),
            "accessibilityDenied",
            "handler-reject",
        );

        let admission = v2_discrete_admission(&message).expect("v2 admission");
        assert_planned_terminal(
            plan_input_dispatch(
                InputGateSnapshot {
                    remote_control_allowed: true,
                    authorized: true,
                    unreliable_seq_accepted: true,
                    accessibility_trusted: true,
                },
                InputV2DispatchSnapshot::Valid {
                    admission,
                    grant_current: false,
                    decision: None,
                },
            ),
            "grantExpired",
            "handler-reject",
        );
    }

    #[test]
    fn handler_input_plan_keeps_resolve_replay_and_closed_queue_terminals_correlated() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids(
            "handler_input_plan_keeps_resolve_replay_and_closed_queue_terminals_correlated",
        );
        let message = v2_discrete_message(&ids, "handler-terminal");
        let admission = v2_discrete_admission(&message).expect("v2 admission");
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Admitted
        );
        let action = plan_input_dispatch(
            InputGateSnapshot {
                remote_control_allowed: true,
                authorized: true,
                unreliable_seq_accepted: true,
                accessibility_trusted: true,
            },
            InputV2DispatchSnapshot::Valid {
                admission: admission.clone(),
                grant_current: true,
                decision: Some(AdmissionDecision::Admitted),
            },
        );
        let InputDispatchAction::EnqueueResolve {
            admission: Some(enqueued_admission),
        } = action
        else {
            panic!("accepted v2 input must enter the real resolve queue");
        };
        assert_eq!(enqueued_admission, admission);

        // These are the same terminal packets used by the real resolve and
        // replay paths. The plan never replays an early rejection.
        for disposition in [
            TerminalDisposition::failure(
                "resolveFailed",
                RemoteControlDeliveryRoute::Resolve,
                RemoteControlFailureCode::ResolveFailed,
            ),
            TerminalDisposition::failure(
                "replayFailed",
                RemoteControlDeliveryRoute::Replay,
                RemoteControlFailureCode::ReplayFailed,
            ),
        ] {
            let packet = discrete_result_packet("native", &enqueued_admission, disposition);
            assert_eq!(packet.input_id.as_deref(), Some("handler-terminal"));
            assert_eq!(
                packet.control_session_id.as_deref(),
                Some(enqueued_admission.control_session_id.as_str())
            );
            assert_eq!(packet.outcome.as_deref(), Some(disposition.outcome));
            assert_eq!(packet.delivery_route, Some(disposition.delivery_route));
            assert_eq!(packet.failure_code, disposition.failure_code);
        }

        let task = ReplayTask {
            message: message.clone(),
            frame: frame(0, 0, 100, 100),
            target_pid: Some(42),
            replay_epoch: 0,
            synthetic_release: false,
            admission: Some(enqueued_admission.clone()),
            terminal_on_success: true,
            result_sender: None,
        };
        // Refs #288: `push_replay`/`ReplayQueuePush` no longer exist -- #369
        // replaced the single global replay queue with per-target-pid shards
        // (`enqueue_replay_with_injector`), which handles a disconnected
        // shard sender inline. Exercise the same disconnected-channel shape
        // directly instead.
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let Err(mpsc::TrySendError::Disconnected(returned_task)) = sender.try_send(task) else {
            panic!("closed replay queue must return the discrete task without replaying it");
        };
        // This is the same terminal completion `enqueue_replay_with_injector`
        // executes for a disconnected shard; no replay worker is started or
        // invoked.
        let superseded = TerminalDisposition::failure(
            "superseded",
            RemoteControlDeliveryRoute::Replay,
            RemoteControlFailureCode::Superseded,
        );
        complete_replay_task(&returned_task, superseded, true);
        let closed_admission = returned_task.admission.expect("closed task admission");
        assert_eq!(
            admit_discrete_operation(&message, &closed_admission, Instant::now()),
            AdmissionDecision::CompletedDuplicate(superseded)
        );
        let packet = discrete_result_packet("native", &closed_admission, superseded);
        assert_eq!(packet.input_id.as_deref(), Some("handler-terminal"));
        assert_eq!(
            packet.control_session_id.as_deref(),
            Some(closed_admission.control_session_id.as_str())
        );
        assert_eq!(packet.outcome.as_deref(), Some("superseded"));
        assert_eq!(
            packet.delivery_route,
            Some(RemoteControlDeliveryRoute::Replay)
        );
        assert_eq!(
            packet.failure_code,
            Some(RemoteControlFailureCode::Superseded)
        );

        // Exercise the duplicate ingress adapter, not just cache equality:
        // its Reject terminal travels through the same result-packet handoff
        // and must retain the original disposition exactly.
        let duplicate_action = plan_input_dispatch(
            InputGateSnapshot {
                remote_control_allowed: true,
                authorized: true,
                unreliable_seq_accepted: true,
                accessibility_trusted: true,
            },
            InputV2DispatchSnapshot::Valid {
                admission: closed_admission.clone(),
                grant_current: true,
                decision: Some(admit_discrete_operation(
                    &message,
                    &closed_admission,
                    Instant::now(),
                )),
            },
        );
        let InputDispatchAction::Reject {
            terminal: Some((duplicate_admission, duplicate_disposition)),
            ..
        } = duplicate_action
        else {
            panic!("completed v2 duplicate must produce a terminal result handoff");
        };
        let duplicate_packet =
            discrete_result_packet("native", &duplicate_admission, duplicate_disposition);
        assert_eq!(duplicate_packet.outcome, packet.outcome);
        assert_eq!(duplicate_packet.delivery_route, packet.delivery_route);
        assert_eq!(duplicate_packet.failure_code, packet.failure_code);
    }

    #[test]
    fn v2_canonical_fingerprint_matches_shared_binary_vector() {
        let _guard = discrete_admission_test_lock();
        let mut message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Click),
            15,
            42,
            "web-1".to_string(),
        );
        message.target_user_id = "native-1".to_string();
        message.x = Some(0.5);
        message.y = Some(0.25);
        message.button = Some(0);
        message.buttons = Some(0);
        message.modifiers.shift = true;
        let admission = DiscreteAdmission {
            controller_id: "web-1".to_string(),
            window_id: 42,
            target_kind: message.target_kind,
            share_instance_id: message.share_instance_id.clone(),
            control_session_id: "grant_opaque_example".to_string(),
            input_id: "input-example-000000000".to_string(),
            input_seq: 15,
            operation_fingerprint: String::new(),
        };
        assert_eq!(
            canonical_operation_fingerprint(&message, &admission),
            "b3b509e59b423c9c15ee5bde3bf661aa52ff0cd6a65c9cffd81f419ff09ccaa1"
        );
    }

    #[test]
    fn v2_discrete_admission_deduplicates_then_replays_terminal_outcome() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids(
            "v2_discrete_admission_deduplicates_then_replays_terminal_outcome",
        );
        let message = v2_discrete_message(&ids, "input-a");
        let admission = v2_discrete_admission(&message).unwrap();
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Admitted
        );
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::InFlightDuplicate
        );
        let applied = TerminalDisposition::success("applied", RemoteControlDeliveryRoute::Replay);
        assert!(complete_discrete_operation(&admission, applied));
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::CompletedDuplicate(applied)
        );
        assert!(!complete_discrete_operation(
            &admission,
            TerminalDisposition::failure(
                "replayFailed",
                RemoteControlDeliveryRoute::Replay,
                RemoteControlFailureCode::ReplayFailed,
            ),
        ));
    }

    #[test]
    fn v2_discrete_admission_enters_a_bounded_overload_epoch() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids("v2_discrete_admission_enters_a_bounded_overload_epoch");
        let control_session_id = authorize_shared(ids.window_id, &ids.controller_id);
        for index in 0..DISCRETE_ADMISSION_CAPACITY {
            let message =
                v2_discrete_message_for_grant(&ids, &format!("input-{index}"), &control_session_id);
            let admission = v2_discrete_admission(&message).unwrap();
            assert_eq!(
                admit_discrete_operation(&message, &admission, Instant::now()),
                AdmissionDecision::Admitted
            );
        }
        let message = v2_discrete_message_for_grant(&ids, "overflow", &control_session_id);
        let admission = v2_discrete_admission(&message).unwrap();
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Overloaded
        );
        let state = discrete_admissions().lock_unpoisoned();
        let grant = AdmissionGrantKey::from(&admission);
        assert!(state
            .overload_epoch
            .get(&grant)
            .is_some_and(|epoch| *epoch > 0));
        assert!(state.overload_until.contains_key(&grant));
    }

    #[test]
    fn v2_regrant_invalidates_old_generation_and_old_tombstones_cannot_authorize() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids(
            "v2_regrant_invalidates_old_generation_and_old_tombstones_cannot_authorize",
        );
        let old = v2_discrete_message(&ids, "input-a");
        let old_admission = v2_discrete_admission(&old).unwrap();
        assert_eq!(
            admit_discrete_operation(&old, &old_admission, Instant::now()),
            AdmissionDecision::Admitted
        );
        assert!(complete_discrete_operation(
            &old_admission,
            TerminalDisposition::success("applied", RemoteControlDeliveryRoute::Replay),
        ));
        let fresh_grant = authorize_shared(ids.window_id, &ids.controller_id);
        assert_ne!(old_admission.control_session_id, fresh_grant);
        assert!(!grant_is_current(&old_admission, Instant::now()));
        let mut current = old.clone();
        current.control_session_id = Some(fresh_grant);
        current.input_id = Some("input-a".to_string());
        let current_admission = DiscreteAdmission {
            controller_id: current.controller_id.clone(),
            window_id: current.window_id,
            target_kind: current.target_kind,
            share_instance_id: current.share_instance_id.clone(),
            control_session_id: current.control_session_id.clone().unwrap(),
            input_id: "input-a".to_string(),
            input_seq: 1,
            operation_fingerprint: String::new(),
        };
        current.operation_fingerprint = Some(canonical_operation_fingerprint(
            &current,
            &current_admission,
        ));
        let current_admission = v2_discrete_admission(&current).unwrap();
        assert!(grant_is_current(&current_admission, Instant::now()));
        assert_eq!(
            admit_discrete_operation(&current, &current_admission, Instant::now()),
            AdmissionDecision::Admitted
        );
    }

    #[test]
    fn v2_overload_epoch_expires_without_leaking_admissions() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids("v2_overload_epoch_expires_without_leaking_admissions");
        let message = v2_discrete_message(&ids, "input-a");
        let admission = v2_discrete_admission(&message).unwrap();
        {
            let mut state = discrete_admissions().lock_unpoisoned();
            state.overload_until.insert(
                AdmissionGrantKey::from(&admission),
                Instant::now() - Duration::from_millis(1),
            );
            state.entries.insert(
                DiscreteAdmissionKey::from(&admission),
                AdmissionEntry {
                    operation_fingerprint: admission.operation_fingerprint.clone(),
                    terminal_disposition: Some(TerminalDisposition::success(
                        "applied",
                        RemoteControlDeliveryRoute::Replay,
                    )),
                    admitted_at: Instant::now()
                        - DISCRETE_OVERLOAD_WINDOW
                        - Duration::from_millis(1),
                },
            );
        }
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Admitted
        );
        let state = discrete_admissions().lock_unpoisoned();
        assert!(state.overload_until.is_empty());
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn stalled_v2_admission_expires_before_replay_can_execute() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids("stalled_v2_admission_expires_before_replay_can_execute");
        let message = v2_discrete_message(&ids, "stalled");
        let admission = v2_discrete_admission(&message).unwrap();
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Admitted
        );
        discrete_admissions()
            .lock_unpoisoned()
            .entries
            .get_mut(&DiscreteAdmissionKey::from(&admission))
            .unwrap()
            .admitted_at = Instant::now() - DISCRETE_IN_FLIGHT_TTL - Duration::from_millis(1);
        assert!(!admission_is_still_inflight(&admission, Instant::now()));
        assert_eq!(
            admit_discrete_operation(&message, &admission, Instant::now()),
            AdmissionDecision::Admitted
        );
    }

    /// #802 regression, measured live before it was written: a LEGACY grant
    /// advertised a v2 `controlSessionId` + `resultCapability` while carrying
    /// none of the capable envelope those fields imply (`targetKind`,
    /// `shareInstanceId`, non-empty `hostCapabilities`). The controller's gate
    /// short-circuits to "accept" only when `controlSessionId` is ABSENT, so a
    /// legacy grant that claims a v2 session fails a check it can never pass,
    /// and the grant token is discarded silently on both sides.
    ///
    /// A Mac host is legacy by contract (docs/CONTRACTS.md), so this is the
    /// shape EVERY macOS grant has. Asserted on the serialized JSON, because
    /// key presence -- not value -- is the whole mechanism.
    #[test]
    fn a_legacy_grant_carries_its_token_but_claims_no_v2_session() {
        let _guard = discrete_admission_test_lock();
        let ids =
            remote_control_test_ids("a_legacy_grant_carries_its_token_but_claims_no_v2_session");
        let grant = authorize_shared(ids.window_id, &ids.controller_id);
        let packet = status_packet_for(
            &RemoteControlStatus {
                window_id: ids.window_id,
                owner_identity: None,
                controller_id: ids.controller_id.clone(),
                status: "active",
                message: "active".to_string(),
                grant_token: Some(grant.clone()),
                reason: None,
            },
            "host",
        );

        // The token itself MUST still ship -- the legacy flow authorizes every
        // input packet with it, and withholding it would break control outright
        // rather than fix it.
        assert_eq!(packet.grant_token.as_deref(), Some(grant.as_str()));
        assert_eq!(packet.control_session_id, None);
        assert!(packet.result_capability.is_none());
        assert!(packet.host_capabilities.is_empty());
        assert_eq!(packet.target_kind, None);

        let wire: serde_json::Value =
            serde_json::to_value(&packet).expect("status packet serializes");
        assert!(
            wire.get("grantToken").is_some(),
            "the legacy flow authorizes on this token; it must be on the wire"
        );
        assert!(
            wire.get("controlSessionId").is_none(),
            "a legacy grant must not claim a v2 control session -- this is #802"
        );
        assert!(wire.get("resultCapability").is_none());

        revoke(ids.window_id, &ids.controller_id);
    }

    /// The other direction: a genuinely capable (v2-scoped) grant still
    /// advertises the full session, so the gating above cannot be satisfied by
    /// simply never sending v2 fields at all.
    #[test]
    fn a_capable_grant_still_advertises_its_v2_session() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids("a_capable_grant_still_advertises_its_v2_session");
        let grant = authorize_shared_key(ControlGrantKey {
            window_id: ids.window_id,
            controller_id: ids.controller_id.clone(),
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-instance-802".to_string()),
        });
        let packet = status_packet_for(
            &RemoteControlStatus {
                window_id: ids.window_id,
                owner_identity: None,
                controller_id: ids.controller_id.clone(),
                status: "active",
                message: "active".to_string(),
                grant_token: Some(grant.clone()),
                reason: None,
            },
            "host",
        );

        assert_eq!(packet.control_session_id.as_deref(), Some(grant.as_str()));
        assert_eq!(packet.target_kind, Some(RemoteControlTargetKind::Window));
        assert_eq!(
            packet.share_instance_id.as_deref(),
            Some("share-instance-802")
        );
        assert_eq!(packet.result_capability.map(|c| c.version), Some(2));

        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn active_status_advertises_v2_capability_without_enabling_retry() {
        let _guard = discrete_admission_test_lock();
        let ids = remote_control_test_ids(
            "active_status_advertises_v2_capability_without_enabling_retry",
        );
        // #802: this used a LEGACY grant and asserted a v2 controlSessionId --
        // i.e. it pinned the defect in place, and stayed green through it. The
        // v2 capability belongs to a v2-scoped grant, so mint one.
        let grant = authorize_shared_key(ControlGrantKey {
            window_id: ids.window_id,
            controller_id: ids.controller_id.clone(),
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-instance-v2-capability".to_string()),
        });
        let packet = status_packet_for(
            &RemoteControlStatus {
                window_id: ids.window_id,
                owner_identity: None,
                controller_id: ids.controller_id,
                status: "active",
                message: "active".to_string(),
                grant_token: Some(grant.clone()),
                reason: None,
            },
            "host",
        );
        assert_eq!(packet.control_session_id.as_deref(), Some(grant.as_str()));
        let capability = packet.result_capability.expect("v2 capability");
        assert_eq!(capability.version, 2);
        assert!(!capability.retry_enabled);
        assert_eq!(capability.retry_deadline_ms, 0);
        assert_eq!(
            capability.dedup_guarantee_window_ms,
            DISCRETE_OVERLOAD_WINDOW.as_millis() as u64
        );
    }

    #[test]
    fn topic_matches_web_harness_contract() {
        assert_eq!(TOPIC, contract_fixture().topics.remote_control);
    }

    #[test]
    fn native_clipboard_contract_and_unknown_copy_compatibility_are_pinned() {
        let fixture = contract_fixture();
        assert_eq!(
            remote_clipboard::REMOTE_CLIPBOARD_TEXT_TOPIC,
            fixture.topics.remote_clipboard_text
        );
        assert_eq!(fixture.remote_clipboard_messages.len(), 2);
        for entry in &fixture.remote_clipboard_messages {
            let object = entry
                .message
                .as_object()
                .expect("clipboard fixture message must be an object");
            let mut fields = object.keys().cloned().collect::<Vec<_>>();
            fields.sort();
            assert_eq!(fields, entry.fields, "{} fields", entry.name);
            assert_eq!(object.get("kind").and_then(|value| value.as_str()), Some("copy"));
            assert!(remote_clipboard::operation_id_is_valid(
                object
                    .get("operationId")
                    .and_then(|value| value.as_str())
                    .expect("operation id")
            ));
            let encoded = serde_json::to_vec(&entry.message).unwrap();
            assert_eq!(parse_clipboard_copy_request(&encoded).is_some(), true);
            let ordinary: RemoteControlMessage = serde_json::from_value(entry.message.clone())
                .expect("copy remains deserializable as an unknown legacy kind");
            assert_eq!(ordinary.message_type, RemoteControlType::Unknown);
        }
        let streams = fixture.remote_clipboard_streams;
        assert_eq!(streams.topic, remote_clipboard::REMOTE_CLIPBOARD_TEXT_TOPIC);
        assert_eq!(streams.mime_type, remote_clipboard::REMOTE_CLIPBOARD_TEXT_MIME);
        assert_eq!(streams.directions, ["copyResponse", "paste"]);
        assert_eq!(
            streams.attributes,
            ["direction", "grantToken", "operationId", "windowId"]
        );
        assert_eq!(
            streams.operation_id_hex_length,
            remote_clipboard::REMOTE_CLIPBOARD_OPERATION_ID_HEX_LENGTH
        );
        assert_eq!(
            streams.max_bytes,
            remote_clipboard::MAX_REMOTE_CLIPBOARD_TEXT_BYTES
        );
        assert_eq!(streams.reliability, "reliable");
        assert_eq!(streams.destination, "oneAuthenticatedParticipant");
        assert_eq!(
            streams.success_signals.get("copyResponse").map(String::as_str),
            Some("targetedTextStreamOnly")
        );
        assert_eq!(
            streams.success_signals.get("paste").map(String::as_str),
            Some("none")
        );
        assert!(streams.text_rules.iter().any(|rule| rule == "rejectOversize"));
    }

    fn clipboard_stream_info(
        direction: &str,
        operation_id: &str,
        length: u64,
    ) -> livekit::ByteStreamInfo {
        let mut attributes = HashMap::new();
        attributes.insert("operationId".to_string(), operation_id.to_string());
        attributes.insert("direction".to_string(), direction.to_string());
        attributes.insert("windowId".to_string(), "42".to_string());
        attributes.insert(
            "grantToken".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
        );
        livekit::ByteStreamInfo {
            id: "stream-id".to_string(),
            topic: remote_clipboard::REMOTE_CLIPBOARD_TEXT_TOPIC.to_string(),
            timestamp: chrono::Utc::now(),
            total_length: Some(length),
            attributes,
            mime_type: remote_clipboard::REMOTE_CLIPBOARD_TEXT_MIME.to_string(),
            name: String::new(),
            encryption_type: Default::default(),
        }
    }

    #[test]
    fn normalized_clipboard_shortcuts_have_no_terminal_result_admission() {
        let message = clipboard_key_message(
            "host",
            "controller",
            42,
            1,
            "0123456789abcdef0123456789abcdef",
            None,
            None,
            ClipboardShortcut::Copy,
            RemoteControlAction::Down,
        );
        assert_eq!(message.control_session_id, None);
        assert_eq!(message.input_id, None);
        assert_eq!(message.operation_fingerprint, None);
        assert!(v2_discrete_admission(&message).is_none());
        assert_eq!(message.key.as_deref(), Some("c"));
        assert_eq!(message.code.as_deref(), Some("KeyC"));
    }

    #[test]
    fn clipboard_stream_header_validation_rejects_misdirected_or_unbounded_streams() {
        let operation_id = "0123456789abcdef0123456789abcdef";
        let info = clipboard_stream_info("paste", operation_id, 5);
        let metadata = clipboard_stream_metadata(&info).expect("valid stream header");
        assert_eq!(metadata.direction, ClipboardStreamDirection::Paste);
        assert_eq!(metadata.declared_length, 5);

        let mut wrong_topic = info.clone();
        wrong_topic.topic = TOPIC.to_string();
        assert!(clipboard_stream_metadata(&wrong_topic).is_none());

        let mut wrong_direction = info.clone();
        wrong_direction
            .attributes
            .insert("direction".to_string(), "copy".to_string());
        assert!(clipboard_stream_metadata(&wrong_direction).is_none());

        let mut extra_attribute = info.clone();
        extra_attribute
            .attributes
            .insert("extra".to_string(), "rejected".to_string());
        assert!(clipboard_stream_metadata(&extra_attribute).is_none());

        let zero = clipboard_stream_info("paste", operation_id, 0);
        assert!(clipboard_stream_metadata(&zero).is_none());
        let too_large = clipboard_stream_info(
            "paste",
            operation_id,
            (remote_clipboard::MAX_REMOTE_CLIPBOARD_TEXT_BYTES + 1) as u64,
        );
        assert!(clipboard_stream_metadata(&too_large).is_none());
    }

    #[test]
    fn normalized_points_map_to_global_logical_points() {
        let p = normalized_to_global(frame(100, -50, 800, 600), 0.25, 0.5);
        assert_eq!(p, GlobalPoint { x: 300.0, y: 250.0 });
    }

    #[test]
    fn normalized_points_map_to_downscaled_capture_source_frame() {
        // #209: when the receiver is viewing a downscaled share, e.g. P1080
        // from a 3000pt-wide source, the remote-control host must use the real
        // source logical frame. A 0.64 capture scale means 1920px represents
        // 3000pt, so the centered click must land at x=1500pt, not x=960pt.
        let p = normalized_to_global(frame(0, 0, 3000, 1688), 0.5, 0.5);
        assert_eq!(
            p,
            GlobalPoint {
                x: 1500.0,
                y: 844.0
            }
        );
    }

    #[test]
    fn normalized_points_are_clamped_before_mapping() {
        let p = normalized_to_global(frame(-200, 10, 100, 50), 2.0, -1.0);
        assert_eq!(p, GlobalPoint { x: -100.0, y: 10.0 });
    }

    #[test]
    fn request_message_uses_stable_camel_case_shape() {
        let msg = RemoteControlMessage {
            v: VERSION,
            message_type: RemoteControlType::Request,
            action: None,
            target_user_id: "native-a".to_string(),
            controller_id: "web-b".to_string(),
            window_id: 42,
            seq: 7,
            target_kind: None,
            share_instance_id: None,
            controller_capabilities: Vec::new(),
            host_capabilities: Vec::new(),
            reason: None,
            control_session_id: None,
            input_id: None,
            input_seq: None,
            operation_fingerprint_version: None,
            operation_fingerprint: None,
            outcome: None,
            delivery_route: None,
            failure_code: None,
            result_capability: None,
            x: None,
            y: None,
            button: None,
            buttons: None,
            click_count: None,
            delta_x: None,
            delta_y: None,
            delta_mode: None,
            key: None,
            code: None,
            repeat: false,
            location: None,
            text: None,
            status: None,
            message: None,
            grant_token: None,
            supports_binary_hot_path: false,
            modifiers: RemoteControlModifiers::default(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"v\":1"));
        assert!(json.contains("\"kind\":\"request\""));
        assert!(json.contains("\"targetUserId\":\"native-a\""));
        assert!(json.contains("\"controllerId\":\"web-b\""));
        assert!(json.contains("\"windowId\":42"));
        let parsed: RemoteControlMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.message_type, RemoteControlType::Request);
        assert_eq!(parsed.target_user_id, "native-a");
    }

    #[test]
    fn v2_result_message_round_trips_with_terminal_outcome() {
        let message: RemoteControlMessage = serde_json::from_str(
            r#"{
                "v":1,
                "kind":"result",
                "targetUserId":"web-1",
                "controllerId":"native-1",
                "windowId":42,
                "seq":16,
                "controlSessionId":"grant_opaque_example",
                "inputId":"AAAAAAAAAAAAAAAAAAAAAA",
                "inputSeq":15,
                "operationFingerprintVersion":1,
                "operationFingerprint":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "outcome":"applied",
                "deliveryRoute":"replay"
            }"#,
        )
        .unwrap();
        assert_eq!(message.message_type, RemoteControlType::Result);
        assert_eq!(message.outcome.as_deref(), Some("applied"));
        assert_eq!(
            message.delivery_route,
            Some(RemoteControlDeliveryRoute::Replay)
        );
        assert_eq!(message.failure_code, None);
        assert_eq!(
            message.control_session_id.as_deref(),
            Some("grant_opaque_example")
        );
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(encoded.contains("\"kind\":\"result\""));
        assert!(encoded.contains("\"outcome\":\"applied\""));
        assert!(encoded.contains("\"deliveryRoute\":\"replay\""));
    }

    // The helper this drives is #[cfg(target_os = "windows")], so the test
    // must carry the same gate -- without it `cargo test --lib` fails to
    // COMPILE on macOS, which blocks the pre-push Rust gate for every Mac
    // developer, not just this test.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_regrant_mirrors_the_new_token_to_the_binary_hot_path() {
        let ids =
            remote_control_test_ids("windows_regrant_mirrors_the_new_token_to_the_binary_hot_path");
        revoke(ids.window_id, &ids.controller_id);
        let key = ControlGrantKey {
            window_id: ids.window_id,
            controller_id: ids.controller_id.clone(),
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-current".to_string()),
        };
        let first = authorize_shared_key(key.clone());
        mirror_grant_to_legacy_key(ids.window_id, &ids.controller_id, &first);
        let second = authorize_shared_key(key);
        mirror_grant_to_legacy_key(ids.window_id, &ids.controller_id, &second);

        assert_ne!(first, second);
        assert_eq!(
            sessions()
                .lock_unpoisoned()
                .get(&ControlGrantKey::legacy(ids.window_id, &ids.controller_id))
                .cloned(),
            Some(second.clone())
        );
        let mut pointer = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        pointer.x = Some(0.5);
        pointer.y = Some(0.5);
        pointer.grant_token = Some(second);
        let bytes = binary_frame_for(&pointer).expect("binary pointer frame");
        assert!(message_from_binary(&bytes, "host", &ids.controller_id).is_some());
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn capable_operation_failure_uses_its_result_without_demoting_the_session() {
        let ids = remote_control_test_ids(
            "capable_operation_failure_uses_its_result_without_demoting_the_session",
        );
        let mut capable = v2_discrete_message(&ids, "key-failure");
        capable.target_kind = Some(RemoteControlTargetKind::Window);
        capable.share_instance_id = Some("share-current".to_string());
        let legacy = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );

        assert!(!should_notify_replay_failure_status(&capable));
        assert!(should_notify_replay_failure_status(&legacy));
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn windows_unsupported_key_errors_map_to_stable_terminal_feedback() {
        assert_eq!(
            replay_failure_code("unsupported Windows key code 'Unidentified'"),
            RemoteControlFailureCode::UnsupportedRoute
        );
        assert_eq!(
            replay_failure_code(
                "not-injectible: continuous pointer move is full-control semantics"
            ),
            RemoteControlFailureCode::NotInjectible
        );
        assert_eq!(
            replay_failure_code("not-injectible: key not mappable in cursor-preserving mode"),
            RemoteControlFailureCode::NotInjectible
        );
    }

    #[test]
    fn terminal_disposition_never_pairs_applied_with_a_failure_code() {
        let applied = TerminalDisposition::failure(
            "applied",
            RemoteControlDeliveryRoute::Replay,
            RemoteControlFailureCode::InjectionTimeout,
        );
        assert_eq!(applied.failure_code, None);

        let failed = TerminalDisposition::failure(
            "replayFailed",
            RemoteControlDeliveryRoute::Replay,
            RemoteControlFailureCode::InjectionTimeout,
        );
        assert_eq!(
            failed.failure_code,
            Some(RemoteControlFailureCode::InjectionTimeout)
        );
    }

    #[test]
    fn v2_result_keeps_known_outcome_when_future_metadata_is_unknown() {
        let message: RemoteControlMessage = serde_json::from_str(
            r#"{
                "v":1,
                "kind":"result",
                "targetUserId":"web-1",
                "controllerId":"native-1",
                "windowId":42,
                "seq":16,
                "controlSessionId":"grant_opaque_example",
                "inputId":"AAAAAAAAAAAAAAAAAAAAAA",
                "inputSeq":15,
                "operationFingerprintVersion":1,
                "operationFingerprint":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "outcome":"applied",
                "deliveryRoute":"future-stage",
                "failureCode":"future-code"
            }"#,
        )
        .unwrap();
        assert_eq!(message.outcome.as_deref(), Some("applied"));
        assert_eq!(
            message.delivery_route,
            Some(RemoteControlDeliveryRoute::Unknown)
        );
        assert_eq!(
            message.failure_code,
            Some(RemoteControlFailureCode::Unknown)
        );
    }

    #[test]
    fn pointer_message_fields_match_shared_contract_fixture() {
        let fixture = contract_fixture();
        let msg = RemoteControlMessage {
            v: VERSION,
            message_type: RemoteControlType::Pointer,
            action: Some(RemoteControlAction::Down),
            target_user_id: "native-a".to_string(),
            controller_id: "web-b".to_string(),
            window_id: 42,
            seq: 7,
            target_kind: None,
            share_instance_id: None,
            controller_capabilities: Vec::new(),
            host_capabilities: Vec::new(),
            reason: None,
            control_session_id: None,
            input_id: None,
            input_seq: None,
            operation_fingerprint_version: None,
            operation_fingerprint: None,
            outcome: None,
            delivery_route: None,
            failure_code: None,
            result_capability: None,
            x: Some(0.5),
            y: Some(0.25),
            button: Some(0),
            buttons: Some(1),
            click_count: None,
            delta_x: None,
            delta_y: None,
            delta_mode: None,
            key: None,
            code: None,
            repeat: false,
            location: None,
            text: None,
            status: None,
            message: None,
            grant_token: Some("0123456789abcdef0123456789abcdef".to_string()),
            supports_binary_hot_path: false,
            modifiers: RemoteControlModifiers {
                alt: false,
                ctrl: false,
                meta: false,
                shift: true,
            },
        };
        let value = serde_json::to_value(msg).unwrap();
        let mut fields = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        fields.sort();
        assert_eq!(fields, pointer_contract_fields(&fixture));
        let parsed: RemoteControlMessage = serde_json::from_value(value).unwrap();
        assert_eq!(
            parsed.grant_token.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn pointer_double_click_message_matches_shared_contract_fixture() {
        // #373: clickCount is additive/optional -- when present, the field set
        // must match the dedicated "pointer-double-click" fixture (which
        // includes clickCount), not the base pointer-down fixture.
        let fixture = contract_fixture();
        let double_click_fields = fixture
            .remote_control_messages
            .iter()
            .find(|message| message.name == "pointer-double-click")
            .map(|message| message.fields.clone())
            .expect("remote-control contract must include a pointer-double-click fixture");
        let msg = RemoteControlMessage {
            v: VERSION,
            message_type: RemoteControlType::Pointer,
            action: Some(RemoteControlAction::Down),
            target_user_id: "native-a".to_string(),
            controller_id: "web-b".to_string(),
            window_id: 42,
            seq: 16,
            target_kind: None,
            share_instance_id: None,
            controller_capabilities: Vec::new(),
            host_capabilities: Vec::new(),
            reason: None,
            control_session_id: None,
            input_id: None,
            input_seq: None,
            operation_fingerprint_version: None,
            operation_fingerprint: None,
            outcome: None,
            delivery_route: None,
            failure_code: None,
            result_capability: None,
            x: Some(0.5),
            y: Some(0.25),
            button: Some(0),
            buttons: Some(1),
            click_count: Some(2),
            delta_x: None,
            delta_y: None,
            delta_mode: None,
            key: None,
            code: None,
            repeat: false,
            location: None,
            text: None,
            status: None,
            message: None,
            grant_token: Some("0123456789abcdef0123456789abcdef".to_string()),
            supports_binary_hot_path: false,
            modifiers: RemoteControlModifiers::default(),
        };
        let value = serde_json::to_value(msg).unwrap();
        let mut fields = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        fields.sort();
        assert_eq!(fields, double_click_fields);
        let parsed: RemoteControlMessage = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.click_count, Some(2));
    }

    #[test]
    fn draft_command_payload_is_promoted_to_wire_message() {
        let draft = RemoteControlDraft {
            message_type: RemoteControlType::Pointer,
            action: Some(RemoteControlAction::Down),
            window_id: 9,
            target_owner_id: Some("owner".to_string()),
            seq: 12,
            target_kind: None,
            share_instance_id: None,
            controller_capabilities: Vec::new(),
            grant_token: Some("0123456789abcdef0123456789abcdef".to_string()),
            x: Some(0.1),
            y: Some(0.9),
            button: Some(0),
            buttons: Some(1),
            click_count: Some(2),
            delta_x: None,
            delta_y: None,
            delta_mode: None,
            key: None,
            code: None,
            repeat: false,
            location: None,
            text: None,
            modifiers: RemoteControlModifiers::default(),
        };
        let msg = draft.into_message("owner".to_string(), "controller".to_string());
        assert_eq!(msg.v, VERSION);
        assert_eq!(msg.target_user_id, "owner");
        assert_eq!(msg.controller_id, "controller");
        assert_eq!(msg.action, Some(RemoteControlAction::Down));
        assert_eq!(msg.click_count, Some(2));
    }

    #[test]
    fn input_drop_reason_classifier_names_operational_causes() {
        assert_eq!(
            classify_input_drop_reason(false, true, true, true),
            Some(RemoteControlInputDropReason::Auth)
        );
        assert_eq!(
            classify_input_drop_reason(true, false, true, true),
            Some(RemoteControlInputDropReason::Auth)
        );
        assert_eq!(
            classify_input_drop_reason(true, true, false, true),
            Some(RemoteControlInputDropReason::StaleSeq)
        );
        assert_eq!(
            classify_input_drop_reason(true, true, true, false),
            Some(RemoteControlInputDropReason::Permission)
        );
        assert_eq!(classify_input_drop_reason(true, true, true, true), None);
        assert_eq!(
            classify_resolve_drop_reason(SharedWindowScreenStatus::OffScreen),
            RemoteControlInputDropReason::OffScreen
        );
    }

    #[test]
    fn latency_probe_covers_all_input_types_without_spamming_high_rate_streams() {
        let ids = remote_control_test_ids("latency_probe_covers_all_input_types_without_spamming");
        let pointer_down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let pointer_move_119 = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            119,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let pointer_move_120 = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            120,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let wheel_119 = test_message(
            RemoteControlType::Wheel,
            None,
            119,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let wheel_120 = test_message(
            RemoteControlType::Wheel,
            None,
            120,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let key = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let text = test_message(
            RemoteControlType::Text,
            None,
            3,
            ids.window_id,
            ids.controller_id,
        );

        assert!(should_log_latency_probe(&pointer_down));
        assert!(!should_log_latency_probe(&pointer_move_119));
        assert!(should_log_latency_probe(&pointer_move_120));
        assert!(!should_log_latency_probe(&wheel_119));
        assert!(should_log_latency_probe(&wheel_120));
        assert!(should_log_latency_probe(&key));
        assert!(should_log_latency_probe(&text));
    }

    #[test]
    fn remote_control_reliability_keeps_high_rate_streams_unreliable() {
        let ids = remote_control_test_ids(
            "remote_control_reliability_keeps_high_rate_streams_unreliable",
        );
        let pointer_move = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let pointer_down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let mut pointer_drag_move = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            5,
            ids.window_id,
            ids.controller_id.clone(),
        );
        pointer_drag_move.buttons = Some(1);
        let wheel = test_message(
            RemoteControlType::Wheel,
            None,
            3,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let key = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            4,
            ids.window_id,
            ids.controller_id,
        );

        assert_eq!(
            unreliable_seq_stream(&pointer_move),
            Some(UnreliableSeqStream::PointerMove)
        );
        assert_eq!(
            unreliable_seq_stream(&wheel),
            Some(UnreliableSeqStream::Wheel)
        );
        assert_eq!(unreliable_seq_stream(&pointer_drag_move), None);
        assert_eq!(unreliable_seq_stream(&pointer_down), None);
        assert_eq!(unreliable_seq_stream(&key), None);
    }

    #[test]
    fn reliability_rule_is_pinned_in_shared_contract() {
        let fixture = contract_fixture();
        assert_eq!(
            fixture.rules.reliability.lossy,
            ["pointer.move.buttons==0", "wheel"]
        );
        assert!(fixture
            .rules
            .reliability
            .reliable
            .iter()
            .any(|rule| rule == "pointer.move.buttons!=0"));
    }

    #[test]
    fn packet_policy_is_pinned_in_shared_contract() {
        let fixture = contract_fixture();
        let expected = [
            ("request", "reliable", "host", "authenticatedController"),
            ("release", "reliable", "host", "authenticatedController"),
            ("status", "reliable", "controller", "authenticatedHost"),
            ("result", "reliable", "controller", "authenticatedHost"),
            (
                "pointerMoveNoButtons",
                "lossy",
                "host",
                "authenticatedController",
            ),
            (
                "pointerHeldOrDiscrete",
                "reliable",
                "host",
                "authenticatedController",
            ),
            ("wheelLegacy", "lossy", "host", "authenticatedController"),
            (
                "scrollDiscrete",
                "reliable",
                "host",
                "authenticatedController",
            ),
            ("key", "reliable", "host", "authenticatedController"),
            ("text", "reliable", "host", "authenticatedController"),
            (
                "copyRequest",
                "reliable",
                "host",
                "authenticatedController",
            ),
            (
                "clipboardTextStream",
                "reliable",
                "targetParticipant",
                "authenticatedRemoteControlGrant",
            ),
        ];
        assert_eq!(fixture.remote_control_packet_policy.len(), expected.len());
        for (actual, (packet, reliability, destination, authority)) in
            fixture.remote_control_packet_policy.iter().zip(expected)
        {
            assert_eq!(
                (
                    actual.packet.as_str(),
                    actual.reliability.as_str(),
                    actual.destination.as_str(),
                    actual.authority.as_str(),
                ),
                (packet, reliability, destination, authority)
            );
        }
    }

    #[test]
    fn capable_window_fixture_round_trips_and_binds_fingerprint() {
        let fixture = contract_fixture();
        let vector = fixture
            .remote_control_messages
            .iter()
            .find(|entry| entry.name == "pointer-click-capable-window")
            .expect("capable window contract fixture");
        let message: RemoteControlMessage = serde_json::from_value(vector.message.clone()).unwrap();
        let admission = v2_discrete_admission(&message).unwrap();
        assert_eq!(
            canonical_operation_fingerprint(&message, &admission),
            // Clone rather than `unwrap()` the field out: moving it partially
            // moves `message`, which the ControlGrantKey assertion below still
            // needs to borrow.
            message.operation_fingerprint.clone().unwrap()
        );
        assert_eq!(
            ControlGrantKey::for_message(&message).unwrap().target_kind,
            Some(RemoteControlTargetKind::Window)
        );
    }

    #[test]
    fn binary_hot_path_frames_match_shared_byte_fixtures() {
        let fixture = contract_fixture();
        // #370 corrective pass: the 27-byte frame carries a token fingerprint
        // that `message_from_binary` verifies against a REAL active session --
        // seed one here (window 42 / controller "web-1", matching the pinned
        // fixture's encoded window id) and tear it down after, since these are
        // fixed literal ids rather than `remote_control_test_ids`'s
        // per-test-unique ones (the fixture bytes must match the pinned hex
        // exactly, so the window id can't be randomized).
        let token = "0123456789abcdef0123456789abcdef".to_string();
        sessions()
            .lock_unpoisoned()
            .insert(ControlGrantKey::legacy(42, "web-1"), token.clone());
        let mut pointer = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            42,
            42,
            "web-1".to_string(),
        );
        pointer.x = Some(0.5);
        pointer.y = Some(0.25);
        pointer.buttons = Some(0);
        pointer.grant_token = Some(token.clone());
        let mut wheel = pointer.clone();
        wheel.message_type = RemoteControlType::Wheel;
        wheel.action = None;
        wheel.seq = 43;
        wheel.modifiers.ctrl = true;
        wheel.delta_x = Some(-12.0);
        wheel.delta_y = Some(120.0);
        wheel.delta_mode = Some(0);
        for (name, message) in [("pointer-move", pointer), ("wheel", wheel)] {
            let expected = fixture
                .remote_control_binary_frames
                .iter()
                .find(|f| f.name == name)
                .unwrap();
            let bytes = binary_frame_for(&message).unwrap();
            assert_eq!(bytes.len(), expected.length);
            let actual = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(actual, expected.hex);
            let decoded = message_from_binary(&bytes, "native-1", "web-1").unwrap();
            assert_eq!(decoded.window_id, 42);
            assert_eq!(decoded.seq, message.seq as u32 as u64);
            assert_eq!(decoded.grant_token.as_deref(), Some(token.as_str()));
        }
        sessions()
            .lock_unpoisoned()
            .remove(&ControlGrantKey::legacy(42, "web-1"));
    }

    #[test]
    fn binary_hot_path_frame_rejects_stale_or_missing_grant_token() {
        // #370 corrective pass (Bug A): a binary hot-path frame whose carried
        // fingerprint doesn't match the CURRENTLY active grant for its
        // (window, controller) must be dropped outright -- never silently
        // accepted via the legacy tokenless JSON path, even if a future release
        // temporarily re-enables that path.
        let ids =
            remote_control_test_ids("binary_hot_path_frame_rejects_stale_or_missing_grant_token");
        let mut message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        message.x = Some(0.5);
        message.y = Some(0.5);
        message.buttons = Some(0);

        // No active session at all yet: `binary_frame_for` refuses to encode
        // (no token to fingerprint).
        message.grant_token = None;
        assert!(binary_frame_for(&message).is_none());

        // A session exists, but the message carries a STALE token that
        // doesn't match it: encode succeeds (the sender had SOME token), but
        // decode on the receiving end must reject the frame.
        let real_token = authorize_shared(ids.window_id, &ids.controller_id);
        message.grant_token = Some("stale-token-does-not-match".to_string());
        let stale_bytes = binary_frame_for(&message).unwrap();
        assert!(message_from_binary(&stale_bytes, "native-1", &ids.controller_id).is_none());

        // The real, current token round-trips successfully.
        message.grant_token = Some(real_token.clone());
        let fresh_bytes = binary_frame_for(&message).unwrap();
        let decoded = message_from_binary(&fresh_bytes, "native-1", &ids.controller_id).unwrap();
        assert_eq!(decoded.grant_token.as_deref(), Some(real_token.as_str()));

        revoke(ids.window_id, &ids.controller_id);

        // With the session revoked, even the previously-valid frame is
        // rejected -- there is no active session to check it against anymore.
        assert!(message_from_binary(&fresh_bytes, "native-1", &ids.controller_id).is_none());
    }

    #[test]
    fn fnv1a32_matches_pinned_test_vectors() {
        // Pinned vectors shared with `web-harness/src/remoteControl.ts`'s
        // `fnv1a32` and asserted from `web-harness/tests/contracts.test.ts` --
        // keep both implementations in lockstep with these exact values.
        let fixture = contract_fixture();
        for vector in &fixture.fnv1a32_test_vectors {
            assert_eq!(
                format!("{:08x}", fnv1a32(vector.input.as_bytes())),
                vector.hash_hex,
                "input={:?}",
                vector.input
            );
        }
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
    }

    #[test]
    fn unknown_kind_is_ignored_by_deserialization() {
        let first: RemoteControlMessage = serde_json::from_str(
            r#"{"v":1,"kind":"future-kind","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":1}"#,
        ).unwrap();
        let second: RemoteControlMessage = serde_json::from_str(
            r#"{"v":1,"kind":"pointer","action":"move","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":2,"x":0.5,"y":0.5,"buttons":0}"#,
        ).unwrap();
        assert_eq!(first.message_type, RemoteControlType::Unknown);
        assert_eq!(second.message_type, RemoteControlType::Pointer);
    }

    #[test]
    fn replay_coalesce_key_only_exists_for_high_rate_streams() {
        let ids = remote_control_test_ids("replay_coalesce_key_only_exists_for_high_rate_streams");
        let pointer_move = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let wheel = test_message(
            RemoteControlType::Wheel,
            None,
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let pointer_down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            3,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let mut pointer_drag_move = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            5,
            ids.window_id,
            ids.controller_id.clone(),
        );
        pointer_drag_move.buttons = Some(1);
        let key_down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            4,
            ids.window_id,
            ids.controller_id,
        );

        assert_eq!(
            replay_coalesce_key(&pointer_move).map(|key| key.stream),
            Some(UnreliableSeqStream::PointerMove)
        );
        assert_eq!(
            replay_coalesce_key(&wheel).map(|key| key.stream),
            Some(UnreliableSeqStream::Wheel)
        );
        assert_eq!(replay_coalesce_key(&pointer_drag_move), None);
        assert_eq!(replay_coalesce_key(&pointer_down), None);
        assert_eq!(replay_coalesce_key(&key_down), None);
    }

    #[test]
    fn replay_coalescing_keeps_latest_adjacent_high_rate_task() {
        let ids = remote_control_test_ids("replay_coalescing_keeps_latest_adjacent_high_rate_task");
        let first = replay_task(
            test_message(
                RemoteControlType::Pointer,
                Some(RemoteControlAction::Move),
                1,
                ids.window_id,
                ids.controller_id.clone(),
            ),
            frame(0, 0, 100, 100),
            Some(10),
            false,
        );
        let second = replay_task(
            test_message(
                RemoteControlType::Pointer,
                Some(RemoteControlAction::Move),
                2,
                ids.window_id,
                ids.controller_id.clone(),
            ),
            frame(0, 0, 100, 100),
            Some(10),
            false,
        );
        let key_down = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                3,
                ids.window_id,
                ids.controller_id,
            ),
            frame(0, 0, 100, 100),
            Some(10),
            false,
        );
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(second).unwrap();
        tx.send(key_down).unwrap();
        drop(tx);
        let mut pending = VecDeque::new();

        let task = coalesce_ready_replay_task(first, &rx, &mut pending);

        assert_eq!(task.message.seq, 2);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.front().unwrap().message.seq, 3);
    }

    #[test]
    fn resolve_queue_prioritizes_discrete_input_and_coalesces_motion() {
        let ids = remote_control_test_ids(
            "resolve_queue_prioritizes_discrete_input_and_coalesces_motion",
        );
        let queue = ResolveQueue::new(1);
        let first_move = ResolveTask {
            message: test_message(
                RemoteControlType::Pointer,
                Some(RemoteControlAction::Move),
                1,
                ids.window_id,
                ids.controller_id.clone(),
            ),
            local_identity: "host".to_string(),
            admission: None,
            result_sender: None,
        };
        let second_move = ResolveTask {
            message: test_message(
                RemoteControlType::Pointer,
                Some(RemoteControlAction::Move),
                2,
                ids.window_id,
                ids.controller_id.clone(),
            ),
            local_identity: "host".to_string(),
            admission: None,
            result_sender: None,
        };
        let key_down = ResolveTask {
            message: test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                3,
                ids.window_id,
                ids.controller_id,
            ),
            local_identity: "host".to_string(),
            admission: None,
            result_sender: None,
        };

        assert!(matches!(queue.push(first_move), ResolveQueuePush::Enqueued));
        assert!(matches!(
            queue.push(second_move),
            ResolveQueuePush::Coalesced
        ));
        assert!(matches!(queue.push(key_down), ResolveQueuePush::Enqueued));

        // Discrete tasks drain ahead of coalescing high-rate ones, so the key
        // (seq 3) precedes the surviving move (seq 2). The final `is_none` is
        // the point of the test: it proves the second move REPLACED the first
        // rather than being enqueued alongside it.
        assert_eq!(queue.try_pop().unwrap().message.seq, 3);
        assert_eq!(queue.try_pop().unwrap().message.seq, 2);
        assert!(queue.try_pop().is_none());
    }

    #[test]
    fn text_replay_is_split_on_character_boundaries() {
        let ids = remote_control_test_ids("text_replay_is_split_on_character_boundaries");
        let original = format!("{}🪷{}", "a".repeat(31), "b".repeat(40));
        let mut message = test_message(
            RemoteControlType::Text,
            None,
            1,
            ids.window_id,
            ids.controller_id,
        );
        message.text = Some(original.clone());

        let (first, continuation) =
            split_text_replay_task(replay_task(message, frame(0, 0, 100, 100), Some(10), false));
        let continuation = continuation.expect("text longer than one slice");
        let first_text = first.message.text.unwrap();
        let continuation_text = continuation.message.text.unwrap();

        assert_eq!(first_text.chars().count(), MAX_REPLAY_TEXT_SLICE_CHARS);
        assert!(first_text.ends_with('🪷'));
        assert_eq!(format!("{first_text}{continuation_text}"), original);
    }

    #[test]
    fn replay_drop_counter_records_saturated_high_rate_events() {
        let ids = remote_control_test_ids("replay_drop_counter_records_saturated_high_rate_events");
        let before = replay_high_rate_drop_count();
        let message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id,
        );

        record_replay_drop(&message);

        assert_eq!(replay_high_rate_drop_count(), before + 1);
    }

    // #369: per-pid replay sharding + per-event deadline.

    fn test_injector(
        f: impl Fn(&RemoteControlMessage, WindowFrame, Option<i32>) -> Result<(), String>
            + Send
            + Sync
            + 'static,
    ) -> ReplayInjector {
        Arc::new(f)
    }

    #[test]
    fn injection_timeout_is_labeled_for_logs() {
        assert_eq!(
            RemoteControlInputDropReason::InjectionTimeout.as_log_label(),
            "injection-timeout"
        );
    }

    #[test]
    fn run_replay_with_deadline_completes_normally_within_budget() {
        let _guard = platform_input_test_lock();
        let ids =
            remote_control_test_ids("run_replay_with_deadline_completes_normally_within_budget");
        let task = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                1,
                ids.window_id,
                ids.controller_id,
            ),
            frame(0, 0, 100, 100),
            // Unique pid: `run_replay_with_deadline` refuses to start a
            // second injection against a shard key that is already in flight
            // (the production anti-stacking gate), and these deadline tests
            // run in parallel threads, so each must use its own target pid.
            Some(910_001),
            false,
        );
        let inject = test_injector(|_message, _frame, _target_pid| Ok(()));

        assert!(matches!(
            run_replay_with_deadline(&task, &inject),
            ReplayRunOutcome::Completed(Ok(()))
        ));
    }

    #[test]
    fn run_replay_with_deadline_abandons_a_hung_injection_at_the_deadline() {
        let _guard = platform_input_test_lock();
        let ids = remote_control_test_ids(
            "run_replay_with_deadline_abandons_a_hung_injection_at_the_deadline",
        );
        let task = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                1,
                ids.window_id,
                ids.controller_id,
            ),
            frame(0, 0, 100, 100),
            Some(91_234),
            false,
        );
        // Far longer than REPLAY_EVENT_DEADLINE -- the abandoned thread keeps
        // sleeping in the background after this test moves on, then exits
        // harmlessly on its own.
        let inject = test_injector(|_message, _frame, _target_pid| {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        });

        let started = Instant::now();
        let outcome = run_replay_with_deadline(&task, &inject);
        let elapsed = started.elapsed();

        assert!(matches!(outcome, ReplayRunOutcome::TimedOut));
        assert!(matches!(
            run_replay_with_deadline(&task, &test_injector(|_, _, _| Ok(()))),
            ReplayRunOutcome::Completed(Err(error))
                if error == "previous target injection is still in progress"
        ));
        assert!(
            elapsed < Duration::from_millis(REPLAY_EVENT_DEADLINE.as_millis() as u64 + 400),
            "abandoned wait took {elapsed:?}, expected to bail out around {REPLAY_EVENT_DEADLINE:?}"
        );
    }

    #[test]
    fn abandoned_injection_thread_observes_the_cancellation_flag() {
        // Fable-review fix (#369): the whole mitigation for orphaned gesture
        // state / stale late side effects depends on this specific property
        // -- once the calling thread times out and flips the flag, the
        // ALREADY-RUNNING abandoned thread must see it via the thread-local
        // when it resumes past its blocking call, so mutation sites (e.g.
        // ax_pointer_down/up) can bail out instead of performing a stale
        // side effect. This proves the propagation mechanism directly,
        // independent of any specific AX call site.
        let ids =
            remote_control_test_ids("abandoned_injection_thread_observes_the_cancellation_flag");
        let task = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                1,
                ids.window_id,
                ids.controller_id,
            ),
            frame(0, 0, 100, 100),
            Some(1234),
            false,
        );
        let observed_cancelled = Arc::new(AtomicBool::new(false));
        let observed_cancelled_for_injector = Arc::clone(&observed_cancelled);
        let inject = test_injector(move |_message, _frame, _target_pid| {
            std::thread::sleep(Duration::from_millis(
                REPLAY_EVENT_DEADLINE.as_millis() as u64 + 200,
            ));
            observed_cancelled_for_injector.store(injection_was_cancelled(), Ordering::SeqCst);
            Ok(())
        });

        let outcome = run_replay_with_deadline(&task, &inject);
        assert!(matches!(outcome, ReplayRunOutcome::TimedOut));

        // Give the abandoned thread time to wake from its sleep and record.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            observed_cancelled.load(Ordering::SeqCst),
            "injection_was_cancelled() must read true inside the abandoned thread once its \
             deadline waiter has given up"
        );
    }

    #[test]
    fn replay_one_task_logs_injection_timeout_drop_reason_and_returns() {
        let ids = remote_control_test_ids(
            "replay_one_task_logs_injection_timeout_drop_reason_and_returns",
        );
        let task = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                1,
                ids.window_id,
                ids.controller_id,
            ),
            frame(0, 0, 100, 100),
            Some(1234),
            false,
        );
        let inject = test_injector(|_message, _frame, _target_pid| {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        });

        let started = Instant::now();
        // Must return promptly (abandoning the hung call) rather than
        // blocking for the injector's full 5s sleep.
        replay_one_task(task, &inject);
        assert!(
            started.elapsed()
                < Duration::from_millis(REPLAY_EVENT_DEADLINE.as_millis() as u64 + 400)
        );
    }

    #[test]
    fn replay_shard_isolates_a_hung_pid_from_other_shared_windows() {
        let ids =
            remote_control_test_ids("replay_shard_isolates_a_hung_pid_from_other_shared_windows");
        // Derived from this test's unique hashed window ids so concurrently
        // running tests can never collide on the same pid (REPLAY_SHARDS is
        // process-global, like the other caches in this file).
        let hung_pid = ids.window_id as i32;
        let healthy_pid = ids.other_window_id as i32;
        let completed: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
        let inject = {
            let completed = Arc::clone(&completed);
            test_injector(move |_message, _frame, target_pid| {
                if target_pid == Some(hung_pid) {
                    // Longer than the test's own assertion budget below --
                    // this pid's shard must never be on the critical path
                    // for the healthy pid's shard to make progress.
                    std::thread::sleep(Duration::from_secs(5));
                }
                if let Some(pid) = target_pid {
                    completed.lock_unpoisoned().push(pid);
                }
                Ok(())
            })
        };

        let hung_task = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                1,
                ids.window_id,
                ids.controller_id.clone(),
            ),
            frame(0, 0, 100, 100),
            Some(hung_pid),
            false,
        );
        let healthy_task = replay_task(
            test_message(
                RemoteControlType::Key,
                Some(RemoteControlAction::Down),
                1,
                ids.other_window_id,
                ids.other_controller_id.clone(),
            ),
            frame(0, 0, 100, 100),
            Some(healthy_pid),
            false,
        );

        // Enqueue the hung pid's task first so, without per-pid sharding,
        // it would head-of-line-block the healthy pid's task behind it on a
        // single shared queue/thread.
        enqueue_replay_with_injector(hung_task, &inject);
        enqueue_replay_with_injector(healthy_task, &inject);

        let deadline =
            Instant::now() + Duration::from_millis(REPLAY_EVENT_DEADLINE.as_millis() as u64 * 2);
        loop {
            if completed.lock_unpoisoned().contains(&healthy_pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "healthy pid's replay was blocked by the hung pid's shard"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn replay_shard_pool_routes_overflow_pids_through_a_shared_bucket() {
        let mut shards = HashMap::new();
        let inject = test_injector(|_message, _frame, _target_pid| Ok(()));
        for pid in 1..=(MAX_DEDICATED_REPLAY_SHARDS as i32) {
            shard_sender_locked(&mut shards, ReplayShardKey::Pid(pid), &inject);
        }
        assert_eq!(shards.len(), MAX_DEDICATED_REPLAY_SHARDS);
        assert!(!shards.contains_key(&ReplayShardKey::Unknown));

        // One more distinct pid beyond the cap must not grow the pool --
        // it should be routed into the shared overflow shard instead.
        shard_sender_locked(
            &mut shards,
            ReplayShardKey::Pid(MAX_DEDICATED_REPLAY_SHARDS as i32 + 1),
            &inject,
        );
        assert_eq!(shards.len(), MAX_DEDICATED_REPLAY_SHARDS + 1);
        assert!(shards.contains_key(&ReplayShardKey::Unknown));
        assert!(!shards.contains_key(&ReplayShardKey::Pid(MAX_DEDICATED_REPLAY_SHARDS as i32 + 1)));
    }

    #[test]
    fn control_caches_are_window_scoped_and_clearable() {
        let _guard = platform_input_test_lock();
        let ids = remote_control_test_ids("control_caches_are_window_scoped_and_clearable");
        clear_control_caches_for_window(ids.window_id);
        clear_control_caches_for_window(ids.other_window_id);

        target_pid_cache().lock_unpoisoned().insert(
            ids.window_id,
            CachedTargetPid {
                pid: 1234,
                cached_at: Instant::now(),
            },
        );
        target_pid_cache().lock_unpoisoned().insert(
            ids.other_window_id,
            CachedTargetPid {
                pid: 5678,
                cached_at: Instant::now(),
            },
        );
        let now = Instant::now();
        control_frame_cache().lock_unpoisoned().insert(
            ids.window_id,
            CachedControlFrame {
                frame: frame(1, 2, 300, 400),
                cached_at: now,
            },
        );
        control_frame_cache().lock_unpoisoned().insert(
            ids.other_window_id,
            CachedControlFrame {
                frame: frame(5, 6, 700, 800),
                cached_at: now,
            },
        );

        assert_eq!(
            cached_control_frame(ids.window_id, now),
            Some(frame(1, 2, 300, 400))
        );
        assert_eq!(
            cached_control_frame(
                ids.window_id,
                now + CONTROL_FRAME_CACHE_TTL + Duration::from_millis(1)
            ),
            None
        );

        clear_control_caches_for_window(ids.window_id);

        assert!(!target_pid_cache()
            .lock_unpoisoned()
            .contains_key(&ids.window_id));
        assert_eq!(
            target_pid_cache()
                .lock_unpoisoned()
                .get(&ids.other_window_id)
                .map(|cached| cached.pid),
            Some(5678)
        );
        assert_eq!(cached_control_frame(ids.window_id, now), None);
        assert_eq!(
            cached_control_frame(ids.other_window_id, now),
            Some(frame(5, 6, 700, 800))
        );
        clear_control_caches_for_window(ids.other_window_id);
    }

    #[test]
    fn target_pid_cache_expires_after_ttl() {
        let _guard = platform_input_test_lock();
        let ids = remote_control_test_ids("target_pid_cache_expires_after_ttl");
        clear_control_caches_for_window(ids.window_id);
        let now = Instant::now();
        target_pid_cache().lock_unpoisoned().insert(
            ids.window_id,
            CachedTargetPid {
                pid: 1234,
                cached_at: now,
            },
        );

        assert_eq!(cached_target_pid(ids.window_id, now), Some(1234));
        assert_eq!(
            cached_target_pid(
                ids.window_id,
                now + TARGET_PID_CACHE_TTL + Duration::from_millis(1)
            ),
            None
        );
        assert!(!target_pid_cache()
            .lock_unpoisoned()
            .contains_key(&ids.window_id));
    }

    #[test]
    fn replay_epoch_cancels_stale_non_release_tasks_only() {
        let ids = remote_control_test_ids("replay_epoch_cancels_stale_non_release_tasks_only");
        let down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let task = replay_task(down.clone(), frame(0, 0, 100, 100), Some(1234), false);
        let release = release_pointer_task(&down, frame(0, 0, 100, 100), Some(1234));

        assert!(is_current_replay_epoch(&task));
        bump_replay_epoch(ids.window_id, &ids.controller_id, "test");

        assert!(!is_current_replay_epoch(&task));
        assert!(is_current_replay_epoch(&release));
    }

    #[test]
    fn replay_epoch_bump_is_scoped_to_its_own_controller() {
        // #374: a controller's own revoke/disconnect bumps only ITS replay
        // epoch — a concurrent controller's already-queued replay tasks on
        // the SAME window must survive.
        let ids = remote_control_test_ids("replay_epoch_bump_is_scoped_to_its_own_controller");
        let other = remote_control_test_ids("replay_epoch_bump_is_scoped_to_its_own_controller_2");
        let down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let mut other_down = down.clone();
        other_down.controller_id = other.controller_id.clone();

        let task = replay_task(down, frame(0, 0, 100, 100), Some(1234), false);
        let other_task = replay_task(other_down, frame(0, 0, 100, 100), Some(1234), false);

        assert!(is_current_replay_epoch(&task));
        assert!(is_current_replay_epoch(&other_task));

        bump_replay_epoch(ids.window_id, &ids.controller_id, "test");

        assert!(!is_current_replay_epoch(&task));
        assert!(is_current_replay_epoch(&other_task));
    }

    #[test]
    fn invalidate_control_frame_clears_only_that_window() {
        let _guard = platform_input_test_lock();
        let ids = remote_control_test_ids("invalidate_control_frame_clears_only_that_window");
        let now = Instant::now();
        control_frame_cache().lock_unpoisoned().insert(
            ids.window_id,
            CachedControlFrame {
                frame: frame(1, 2, 300, 400),
                cached_at: now,
            },
        );
        control_frame_cache().lock_unpoisoned().insert(
            ids.other_window_id,
            CachedControlFrame {
                frame: frame(5, 6, 700, 800),
                cached_at: now,
            },
        );

        invalidate_control_frame(ids.window_id);

        assert_eq!(cached_control_frame(ids.window_id, now), None);
        assert_eq!(
            cached_control_frame(ids.other_window_id, now),
            Some(frame(5, 6, 700, 800))
        );
        clear_control_caches_for_window(ids.other_window_id);
    }

    // #369: the TTL is a backstop, not the primary invalidation path -- that's
    // immediate via share_border.rs's 100ms border tracker and
    // telepointer.rs's ~110ms (FRAME_REFRESH_TICKS @ ~9Hz) sender-loop, both
    // of which call update_control_frame/invalidate_control_frame the moment
    // they observe a shared window's frame change. Lock in that it stays a
    // tight backstop (comfortably above both poll cadences, so it can't itself
    // evict a frame before either poller gets a chance to refresh it) rather
    // than silently drifting back toward the old 5s value.
    #[test]
    fn control_frame_cache_ttl_is_a_tight_backstop_above_poll_cadence() {
        assert!(CONTROL_FRAME_CACHE_TTL >= Duration::from_millis(200));
        assert!(CONTROL_FRAME_CACHE_TTL <= Duration::from_secs(2));
    }

    #[test]
    fn pointer_revoke_synthesizes_matching_button_release() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("pointer_revoke_synthesizes_matching_button_release");
        let window_frame = frame(10, 20, 300, 200);
        let mut down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        down.x = Some(0.25);
        down.y = Some(0.75);
        down.button = Some(2);
        down.buttons = Some(2);

        track_pressed_input(&down, window_frame, Some(1234));
        let releases = drain_pressed_for_controller(ids.window_id, &ids.controller_id);

        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.message.message_type, RemoteControlType::Pointer);
        assert_eq!(release.message.action, Some(RemoteControlAction::Up));
        assert_eq!(release.message.button, Some(2));
        assert_eq!(release.message.buttons, Some(0));
        assert_eq!(release.message.x, Some(0.25));
        assert_eq!(release.message.y, Some(0.75));
        assert_eq!(release.frame, window_frame);
        assert_eq!(release.target_pid, Some(1234));
        assert!(drain_pressed_for_controller(ids.window_id, &ids.controller_id).is_empty());
    }

    #[test]
    fn key_revoke_synthesizes_matching_key_release() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("key_revoke_synthesizes_matching_key_release");
        let window_frame = frame(10, 20, 300, 200);
        let mut down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        down.key = Some("c".to_string());
        down.code = Some("KeyC".to_string());
        down.location = Some(0);
        down.repeat = false;
        down.modifiers.meta = true;

        track_pressed_input(&down, window_frame, Some(1234));
        let releases = drain_pressed_for_controller(ids.window_id, &ids.controller_id);

        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.message.message_type, RemoteControlType::Key);
        assert_eq!(release.message.action, Some(RemoteControlAction::Up));
        assert_eq!(release.message.key.as_deref(), Some("c"));
        assert_eq!(release.message.code.as_deref(), Some("KeyC"));
        assert_eq!(release.message.location, Some(0));
        assert!(!release.message.repeat);
        assert!(release.message.modifiers.meta);
        assert_eq!(release.frame, window_frame);
        assert_eq!(release.target_pid, Some(1234));
    }

    #[test]
    fn ax_revoked_midhold_drains_pressed() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("ax_revoked_midhold_drains_pressed");
        let mut key_down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        key_down.key = Some("Shift".to_string());
        key_down.code = Some("ShiftLeft".to_string());

        assert!(track_pressed_input(&key_down, frame(0, 0, 100, 100), Some(1234)).is_empty());
        assert!(pressed_inputs()
            .lock_unpoisoned()
            .contains_key(&(ids.window_id, ids.controller_id.clone())));

        // This is exactly what the accessibility-denied input gate in
        // `handle_message` calls before returning, so Accessibility being
        // revoked mid-hold no longer orphans the held key.
        drain_and_release_pressed(
            ids.window_id,
            &ids.controller_id,
            "held-input-orphaned-ax-revoked",
        );

        assert!(!pressed_inputs()
            .lock_unpoisoned()
            .contains_key(&(ids.window_id, ids.controller_id)));
    }

    #[test]
    fn closed_window_drains_held_inputs() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("closed_window_drains_held_inputs");
        let mut pointer_down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        pointer_down.button = Some(0);

        assert!(track_pressed_input(&pointer_down, frame(0, 0, 100, 100), Some(1234)).is_empty());
        assert!(pressed_inputs()
            .lock_unpoisoned()
            .contains_key(&(ids.window_id, ids.controller_id.clone())));

        // Symmetric with the OffScreen arm: this is exactly what the
        // window-Closed arm in `resolve_one_task` now calls before emitting
        // its `targetUnavailable` status and returning.
        drain_and_release_pressed(ids.window_id, &ids.controller_id, "target window closed");

        assert!(!pressed_inputs()
            .lock_unpoisoned()
            .contains_key(&(ids.window_id, ids.controller_id)));
    }

    #[test]
    fn mismatched_modifier_key_up_synthesizes_original_release() {
        let _guard = pressed_input_test_lock();
        let ids =
            remote_control_test_ids("mismatched_modifier_key_up_synthesizes_original_release");
        let window_frame = frame(10, 20, 300, 200);
        let mut down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        down.key = Some("Shift".to_string());
        down.code = Some("ShiftLeft".to_string());
        down.location = Some(1);
        let mut up = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Up),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        up.key = Some("Shift".to_string());

        assert!(track_pressed_input(&down, window_frame, Some(1234)).is_empty());
        let releases = track_pressed_input(&up, window_frame, Some(1234));

        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].message.action, Some(RemoteControlAction::Up));
        assert_eq!(releases[0].message.code.as_deref(), Some("ShiftLeft"));
        assert_eq!(releases[0].message.location, Some(1));
        assert!(drain_pressed_for_controller(ids.window_id, &ids.controller_id).is_empty());
    }

    #[test]
    fn pointer_move_with_empty_buttons_releases_lost_drag() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("pointer_move_with_empty_buttons_releases_lost_drag");
        let window_frame = frame(10, 20, 300, 200);
        let mut down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        down.button = Some(0);
        down.buttons = Some(1);
        let mut move_after_lost_up = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        move_after_lost_up.buttons = Some(0);

        assert!(track_pressed_input(&down, window_frame, Some(1234)).is_empty());
        let releases = track_pressed_input(&move_after_lost_up, window_frame, Some(1234));

        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].message.action, Some(RemoteControlAction::Up));
        assert_eq!(releases[0].message.button, Some(0));
        assert_eq!(releases[0].message.buttons, Some(0));
        assert!(drain_pressed_for_controller(ids.window_id, &ids.controller_id).is_empty());
    }

    #[test]
    fn held_input_ttl_synthesizes_release_when_controller_goes_silent() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids(
            "held_input_ttl_synthesizes_release_when_controller_goes_silent",
        );
        let mut down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        down.key = Some("Meta".to_string());
        down.code = Some("MetaLeft".to_string());
        track_pressed_input(&down, frame(0, 0, 100, 100), Some(1234));
        pressed_inputs()
            .lock_unpoisoned()
            .get_mut(&(ids.window_id, ids.controller_id.clone()))
            .unwrap()
            .last_activity_at = Instant::now() - HELD_INPUT_TTL - Duration::from_millis(1);

        let releases = drain_expired_pressed(Instant::now());

        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].message.action, Some(RemoteControlAction::Up));
        assert_eq!(releases[0].message.code.as_deref(), Some("MetaLeft"));
        assert!(drain_pressed_for_controller(ids.window_id, &ids.controller_id).is_empty());
    }

    #[test]
    fn held_input_ttl_is_refreshed_by_controller_traffic() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("held_input_ttl_is_refreshed_by_controller_traffic");
        let mut down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        down.button = Some(0);
        track_pressed_input(&down, frame(0, 0, 100, 100), Some(1234));
        let key = (ids.window_id, ids.controller_id.clone());
        pressed_inputs()
            .lock_unpoisoned()
            .get_mut(&key)
            .unwrap()
            .last_activity_at = Instant::now() - HELD_INPUT_TTL - Duration::from_millis(1);
        let move_message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );

        track_pressed_input(&move_message, frame(0, 0, 100, 100), Some(1234));
        let releases = drain_expired_pressed(Instant::now());

        assert!(releases.is_empty());
        assert_eq!(
            drain_pressed_for_controller(ids.window_id, &ids.controller_id).len(),
            1
        );
    }

    #[test]
    fn transparent_reconnect_drains_held_inputs_but_keeps_grant() {
        let _guard = pressed_input_test_lock();
        let ids =
            remote_control_test_ids("transparent_reconnect_drains_held_inputs_but_keeps_grant");
        authorize_shared(ids.window_id, &ids.controller_id);

        let mut pointer_down = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        pointer_down.button = Some(0);
        let mut key_down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        key_down.key = Some("Shift".to_string());
        key_down.code = Some("ShiftLeft".to_string());

        track_pressed_input(&pointer_down, frame(0, 0, 100, 100), Some(1234));
        track_pressed_input(&key_down, frame(0, 0, 100, 100), Some(1234));
        assert_eq!(release_held_inputs_for_reconnect(), 2);

        assert!(is_authorized(ids.window_id, &ids.controller_id));
        assert!(pressed_inputs().lock_unpoisoned().is_empty());
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn controller_revoke_drains_pressed_inputs_across_windows_only_for_that_controller() {
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids(
            "controller_revoke_drains_pressed_inputs_across_windows_only_for_that_controller",
        );
        let mut first = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        first.button = Some(0);
        let mut second = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            2,
            ids.other_window_id,
            ids.controller_id.clone(),
        );
        second.key = Some("x".to_string());
        second.code = Some("KeyX".to_string());
        let mut other = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Down),
            3,
            ids.window_id,
            ids.other_controller_id.clone(),
        );
        other.button = Some(2);

        track_pressed_input(&first, frame(0, 0, 100, 100), Some(10));
        track_pressed_input(&second, frame(20, 20, 200, 200), Some(20));
        track_pressed_input(&other, frame(40, 40, 300, 300), Some(30));

        let releases = drain_pressed_for_controller_id(&ids.controller_id);

        assert_eq!(releases.len(), 2);
        assert!(releases
            .iter()
            .all(|task| task.message.controller_id == ids.controller_id));
        assert_eq!(
            drain_pressed_for_controller(ids.window_id, &ids.other_controller_id).len(),
            1
        );
    }

    #[test]
    fn stale_unreliable_sequences_are_dropped_per_stream() {
        let ids = remote_control_test_ids("stale_unreliable_sequences_are_dropped_per_stream");
        reset_unreliable_seq(ids.window_id, &ids.controller_id);

        let pointer_10 = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            10,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let pointer_9 = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            9,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let pointer_11 = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            11,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let wheel_1 = test_message(
            RemoteControlType::Wheel,
            None,
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );

        assert!(should_accept_unreliable_seq(&pointer_10));
        assert!(!should_accept_unreliable_seq(&pointer_9));
        assert!(should_accept_unreliable_seq(&pointer_11));
        assert!(should_accept_unreliable_seq(&wheel_1));
    }

    #[test]
    fn seq_watermark_resets_on_controller_restart() {
        let ids = remote_control_test_ids("seq_watermark_resets_on_controller_restart");
        reset_unreliable_seq(ids.window_id, &ids.controller_id);

        let high = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            CONTROLLER_RESTART_WATERMARK_MIN + 10,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let restart = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            0,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let next = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );

        assert!(should_accept_unreliable_seq(&high));
        assert!(should_accept_unreliable_seq(&restart));
        assert!(should_accept_unreliable_seq(&next));
        reset_unreliable_seq(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn request_lifecycle_resets_unreliable_sequence_state() {
        let ids = remote_control_test_ids("request_lifecycle_resets_unreliable_sequence_state");
        reset_unreliable_seq(ids.window_id, &ids.controller_id);

        let high = test_message(
            RemoteControlType::Wheel,
            None,
            50,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let low = test_message(
            RemoteControlType::Wheel,
            None,
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );

        assert!(should_accept_unreliable_seq(&high));
        assert!(!should_accept_unreliable_seq(&low));

        authorize_shared(ids.window_id, &ids.controller_id);
        assert!(should_accept_unreliable_seq(&low));

        let lower = test_message(
            RemoteControlType::Wheel,
            None,
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        assert!(!should_accept_unreliable_seq(&lower));
        revoke(ids.window_id, &ids.controller_id);
        assert!(should_accept_unreliable_seq(&lower));
    }

    #[test]
    fn window_revoke_clears_only_that_window_control_state() {
        let ids = remote_control_test_ids("window_revoke_clears_only_that_window_control_state");
        let other = remote_control_test_ids("window_revoke_clears_only_that_window_other");

        revoke(ids.window_id, &ids.controller_id);
        revoke(ids.window_id, &ids.other_controller_id);
        revoke(other.window_id, &other.controller_id);
        authorize_shared(ids.window_id, &ids.controller_id);
        authorize_shared(other.window_id, &other.controller_id);

        let high = test_message(
            RemoteControlType::Wheel,
            None,
            40,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let low = test_message(
            RemoteControlType::Wheel,
            None,
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        assert!(should_accept_unreliable_seq(&high));
        assert!(!should_accept_unreliable_seq(&low));

        let (revoked, releases) = drain_window_control(ids.window_id);
        assert_eq!(revoked, vec![ids.controller_id.clone()]);
        assert!(releases.is_empty());
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        assert!(is_authorized(other.window_id, &other.controller_id));
        assert!(should_accept_unreliable_seq(&low));

        revoke(other.window_id, &other.controller_id);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn revoke_paths_clear_in_progress_ax_gestures() {
        let _guard = input::ax_test_lock();
        let ids = remote_control_test_ids("revoke_paths_clear_in_progress_ax_gestures");
        let other = remote_control_test_ids("revoke_paths_clear_in_progress_ax_gestures_other");

        input::clear_all_ax_control_state();
        input::insert_pass_through_ax_gesture_for_tests(ids.window_id, &ids.controller_id);
        assert_eq!(input::ax_gesture_count_for_tests(), 1);
        revoke(ids.window_id, &ids.controller_id);
        assert_eq!(input::ax_gesture_count_for_tests(), 0);

        input::insert_pass_through_ax_gesture_for_tests(ids.window_id, &ids.controller_id);
        input::insert_pass_through_ax_gesture_for_tests(other.window_id, &other.controller_id);
        clear_control_caches_for_window(ids.window_id);
        assert_eq!(input::ax_gesture_count_for_tests(), 1);
        clear_all_control_caches();
        assert_eq!(input::ax_gesture_count_for_tests(), 0);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn clear_all_control_caches_retains_sl_drag_gestures_for_the_synthetic_release() {
        let _guard = input::ax_test_lock();
        // #446 review finding: clear_all_control_caches (called by
        // revoke_all -- the room-disconnect / "controller vanished mid-drag"
        // path) must NOT wipe a physically-held SkyLight drag's gesture
        // state, or the synthetic Up enqueue_synthetic_releases replays
        // afterward finds no gesture and can never post the release,
        // leaving a permanent phantom held mouse button in the target app.
        let ids = remote_control_test_ids(
            "clear_all_control_caches_retains_sl_drag_gestures_for_the_synthetic_release",
        );
        let other = remote_control_test_ids(
            "clear_all_control_caches_retains_sl_drag_gestures_for_the_synthetic_release_other",
        );

        input::clear_all_ax_control_state();
        input::insert_sl_drag_gesture_for_tests(ids.window_id, &ids.controller_id);
        input::insert_pass_through_ax_gesture_for_tests(other.window_id, &other.controller_id);
        assert_eq!(input::ax_gesture_count_for_tests(), 2);

        clear_all_control_caches();

        // The ordinary PassThrough gesture is gone (matches the existing
        // revoke_paths_clear_in_progress_ax_gestures test), but the SlDrag
        // one survives so a later synthetic Up can still find it and post
        // the real release.
        assert_eq!(input::ax_gesture_count_for_tests(), 1);

        input::clear_all_ax_control_state();
    }

    #[test]
    fn authorization_is_window_and_controller_scoped() {
        let ids = remote_control_test_ids("authorization_is_window_and_controller_scoped");

        revoke(ids.window_id, &ids.controller_id);
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        authorize_shared(ids.window_id, &ids.controller_id);
        assert!(is_authorized(ids.window_id, &ids.controller_id));
        assert!(!is_authorized(ids.window_id, &ids.other_controller_id));
        assert!(!is_authorized(ids.other_window_id, &ids.controller_id));
        revoke(ids.window_id, &ids.controller_id);
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
    }

    #[test]
    fn grant_token_is_128_bit_hex_and_rotates_on_regrant() {
        let ids = remote_control_test_ids("grant_token_is_128_bit_hex_and_rotates_on_regrant");
        revoke(ids.window_id, &ids.controller_id);

        let first_token = authorize_shared(ids.window_id, &ids.controller_id);
        assert_eq!(first_token.len(), 32);
        assert!(first_token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let mut first_packet = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        first_packet.grant_token = Some(first_token.clone());
        assert!(is_authorized_input(&first_packet));

        let second_token = authorize_shared(ids.window_id, &ids.controller_id);
        assert_ne!(first_token, second_token);
        assert!(!is_authorized_input(&first_packet));

        first_packet.grant_token = Some(second_token);
        assert!(is_authorized_input(&first_packet));
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn two_controllers_hold_concurrent_grants_on_one_window() {
        // #374: a Request from a different controller on the SAME window
        // must ADD a concurrent grant instead of displacing the existing
        // one — unlike the old exclusive behavior (where this second
        // authorize call would have removed the first controller's session
        // and made its token immediately stale), both controllers' grants
        // and tokens must remain independently valid.
        let ids = remote_control_test_ids("two_controllers_hold_concurrent_grants_on_one_window");
        revoke(ids.window_id, &ids.controller_id);
        revoke(ids.window_id, &ids.other_controller_id);
        let first_token = authorize_shared(ids.window_id, &ids.controller_id);
        let mut first_packet = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        first_packet.grant_token = Some(first_token.clone());

        let second_token = authorize_shared(ids.window_id, &ids.other_controller_id);
        assert_ne!(first_token, second_token);

        // Neither controller displaced the other.
        assert!(is_authorized(ids.window_id, &ids.controller_id));
        assert!(is_authorized(ids.window_id, &ids.other_controller_id));
        assert!(is_authorized_input(&first_packet));

        let mut second_packet = first_packet.clone();
        second_packet.controller_id = ids.other_controller_id.clone();
        second_packet.grant_token = Some(second_token);
        assert!(is_authorized_input(&second_packet));

        revoke(ids.window_id, &ids.controller_id);
        revoke(ids.window_id, &ids.other_controller_id);
    }

    #[test]
    fn revoke_one_controller_leaves_the_other_active() {
        // #374 DoD: per-controller revoke/disconnect drains only that
        // controller — a concurrent controller's grant, held inputs, and
        // replay stream must survive untouched.
        let _guard = pressed_input_test_lock();
        let ids = remote_control_test_ids("revoke_one_controller_leaves_the_other_active");
        revoke(ids.window_id, &ids.controller_id);
        revoke(ids.window_id, &ids.other_controller_id);
        authorize_shared(ids.window_id, &ids.controller_id);
        let other_token = authorize_shared(ids.window_id, &ids.other_controller_id);

        let mut first_down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        first_down.key = Some("a".to_string());
        first_down.code = Some("KeyA".to_string());
        let mut other_down = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.other_controller_id.clone(),
        );
        other_down.key = Some("b".to_string());
        other_down.code = Some("KeyB".to_string());
        track_pressed_input(&first_down, frame(0, 0, 100, 100), Some(1234));
        track_pressed_input(&other_down, frame(0, 0, 100, 100), Some(1234));

        revoke(ids.window_id, &ids.controller_id);

        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        assert!(is_authorized(ids.window_id, &ids.other_controller_id));
        assert!(drain_pressed_for_controller(ids.window_id, &ids.controller_id).is_empty());
        assert_eq!(
            drain_pressed_for_controller(ids.window_id, &ids.other_controller_id).len(),
            1
        );

        let mut other_packet = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            2,
            ids.window_id,
            ids.other_controller_id.clone(),
        );
        other_packet.grant_token = Some(other_token);
        assert!(is_authorized_input(&other_packet));

        revoke(ids.window_id, &ids.other_controller_id);
    }

    #[test]
    fn tokenless_input_is_rejected_after_compatibility_window() {
        assert!(!TOKENLESS_GRANT_COMPATIBILITY_ENABLED);
        let ids = remote_control_test_ids("tokenless_input_is_rejected_after_compatibility_window");
        revoke(ids.window_id, &ids.controller_id);
        authorize_shared(ids.window_id, &ids.controller_id);
        let packet = test_message(
            RemoteControlType::Wheel,
            None,
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        assert!(!is_authorized_input(&packet));
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn trusted_sender_binding_rejects_anonymous_packets() {
        let ids = remote_control_test_ids("trusted_sender_binding_rejects_anonymous_packets");
        let packet = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id,
        );

        assert!(bind_trusted_sender(None, packet).is_none());
    }

    #[test]
    fn trusted_sender_binding_prevents_controller_id_spoofing_before_authorization() {
        let ids = remote_control_test_ids(
            "trusted_sender_binding_prevents_controller_id_spoofing_before_authorization",
        );
        let victim = ids.controller_id.clone();
        let attacker = ids.other_controller_id.clone();
        revoke(ids.window_id, &victim);
        revoke(ids.window_id, &attacker);
        let victim_token = authorize_shared(ids.window_id, &victim);

        let tokenless_packet = test_message(
            RemoteControlType::Wheel,
            None,
            1,
            ids.window_id,
            victim.clone(),
        );
        let bound = bind_trusted_sender(Some(attacker.clone()), tokenless_packet)
            .expect("authenticated LiveKit sender");
        assert_eq!(bound.controller_id, attacker);
        assert!(!is_authorized_input(&bound));

        // Keep this stronger exact-token case: it fails if the overwrite is
        // weakened even while the global tokenless gate remains disabled.
        let mut token_packet = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            2,
            ids.window_id,
            victim.clone(),
        );
        token_packet.grant_token = Some(victim_token);
        let bound = bind_trusted_sender(Some(attacker.clone()), token_packet)
            .expect("authenticated LiveKit sender");
        assert_eq!(bound.controller_id, attacker);
        assert!(!is_authorized_input(&bound));

        revoke(ids.window_id, &victim);
        revoke(ids.window_id, &attacker);
    }

    #[test]
    fn disabled_request_is_dropped_without_authorizing() {
        let ids = remote_control_test_ids("disabled_request_is_dropped_without_authorizing");

        revoke(ids.window_id, &ids.controller_id);
        authorize_shared(ids.window_id, &ids.controller_id);

        assert_eq!(
            apply_request_gate(ids.window_id, &ids.controller_id, RequestGate::Disabled),
            None
        );

        assert!(!is_authorized(ids.window_id, &ids.controller_id));
    }

    #[test]
    fn absent_requester_is_dropped_without_authorizing() {
        let ids = remote_control_test_ids("absent_requester_is_dropped_without_authorizing");

        revoke(ids.window_id, &ids.controller_id);

        assert_eq!(
            apply_request_gate(
                ids.window_id,
                &ids.controller_id,
                RequestGate::RequesterNotPresent
            ),
            None
        );

        assert!(!is_authorized(ids.window_id, &ids.controller_id));
    }

    #[test]
    fn enabled_request_authorizes_immediately() {
        let ids = remote_control_test_ids("enabled_request_authorizes_immediately");

        revoke(ids.window_id, &ids.controller_id);

        assert!(!window_has_active_controller(ids.window_id));
        assert!(matches!(
            apply_request_gate(ids.window_id, &ids.controller_id, RequestGate::Allowed),
            Some(ref token) if token.len() == 32
        ));

        assert!(is_authorized(ids.window_id, &ids.controller_id));
        assert!(window_has_active_controller(ids.window_id));
        revoke(ids.window_id, &ids.controller_id);
        assert!(!window_has_active_controller(ids.window_id));
    }

    #[test]
    fn accessibility_denied_request_revokes_authorization_before_later_input() {
        let ids = remote_control_test_ids(
            "accessibility_denied_request_revokes_authorization_before_later_input",
        );

        revoke(ids.window_id, &ids.controller_id);

        assert!(matches!(
            apply_request_gate(ids.window_id, &ids.controller_id, RequestGate::Allowed),
            Some(ref token) if token.len() == 32
        ));
        assert!(is_authorized(ids.window_id, &ids.controller_id));

        assert!(!apply_request_accessibility_decision(
            ids.window_id,
            &ids.controller_id,
            false
        ));

        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        let later_input = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            2,
            ids.window_id,
            ids.controller_id.clone(),
        );
        assert!(!resolve_task_still_authorized(&later_input));

        assert!(matches!(
            apply_request_gate(ids.window_id, &ids.controller_id, RequestGate::Allowed),
            Some(ref token) if token.len() == 32
        ));
        assert!(apply_request_accessibility_decision(
            ids.window_id,
            &ids.controller_id,
            true
        ));
        assert!(is_authorized(ids.window_id, &ids.controller_id));
        revoke(ids.window_id, &ids.controller_id);
    }

    // ---- consent flow (ask policy) ------------------------------------------

    #[test]
    fn awaiting_consent_gate_parks_the_request_without_authorizing() {
        let ids =
            remote_control_test_ids("awaiting_consent_gate_parks_the_request_without_authorizing");
        revoke(ids.window_id, &ids.controller_id);
        let request = test_message(
            RemoteControlType::Request,
            None,
            7,
            ids.window_id,
            ids.controller_id.clone(),
        );
        // Parking mints nothing ...
        assert_eq!(
            apply_request_gate_for_message(&request, RequestGate::AwaitingConsent),
            None
        );
        assert_eq!(
            apply_request_gate(
                ids.window_id,
                &ids.controller_id,
                RequestGate::AwaitingConsent
            ),
            None
        );
        // ... and, unlike Disabled/RequesterNotPresent, does NOT revoke a
        // grant this controller already holds (a re-request is idempotent).
        authorize_shared(ids.window_id, &ids.controller_id);
        assert_eq!(
            apply_request_gate_for_message(&request, RequestGate::AwaitingConsent),
            None
        );
        assert!(is_authorized(ids.window_id, &ids.controller_id));
        revoke(ids.window_id, &ids.controller_id);

        let key = ControlGrantKey::for_message(&request).expect("grant key");
        remote_control_engine().store_pending_request(key, request.clone());
        assert!(remote_control_engine().has_pending_request(ids.window_id, &ids.controller_id));
        assert_eq!(
            remote_control_engine().pending_request_seq(ids.window_id, &ids.controller_id),
            Some(7)
        );
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        assert!(!window_has_active_controller(ids.window_id));
        // A later input for a parked (unanswered) request is not authorized.
        let later_input = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            8,
            ids.window_id,
            ids.controller_id.clone(),
        );
        assert!(!resolve_task_still_authorized(&later_input));
        assert!(remote_control_engine()
            .take_pending_request(ids.window_id, &ids.controller_id)
            .is_some());
    }

    #[test]
    fn approve_authorizes_and_mints_a_grant_token() {
        let ids = remote_control_test_ids("approve_authorizes_and_mints_a_grant_token");
        revoke(ids.window_id, &ids.controller_id);
        let request = test_message(
            RemoteControlType::Request,
            None,
            11,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let key = ControlGrantKey::for_message(&request).expect("grant key");
        remote_control_engine().store_pending_request(key, request);
        // This is exactly what `answer_consent(approve = true)` does once it
        // has re-checked the gate: take the parked message and authorize it.
        let parked = remote_control_engine()
            .take_pending_request(ids.window_id, &ids.controller_id)
            .expect("parked request");
        assert!(matches!(
            apply_request_gate_for_message(&parked, RequestGate::Allowed),
            Some(ref token) if token.len() == 32
        ));
        assert!(is_authorized(ids.window_id, &ids.controller_id));
        assert!(!remote_control_engine().has_pending_request(ids.window_id, &ids.controller_id));
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn deny_and_timeout_leave_no_authorization() {
        let ids = remote_control_test_ids("deny_and_timeout_leave_no_authorization");
        revoke(ids.window_id, &ids.controller_id);
        let request = test_message(
            RemoteControlType::Request,
            None,
            21,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let key = ControlGrantKey::for_message(&request).expect("grant key");
        remote_control_engine().store_pending_request(key, request);
        // Deny = take the parked request and mint nothing.
        assert!(remote_control_engine()
            .take_pending_request(ids.window_id, &ids.controller_id)
            .is_some());
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        // Timeout: the timer only fires for the seq it armed for. After the
        // request is gone (or replaced by a newer seq) it must be a no-op.
        assert_eq!(
            remote_control_engine().pending_request_seq(ids.window_id, &ids.controller_id),
            None
        );
        let newer = test_message(
            RemoteControlType::Request,
            None,
            22,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let key = ControlGrantKey::for_message(&newer).expect("grant key");
        remote_control_engine().store_pending_request(key, newer);
        assert_ne!(
            remote_control_engine().pending_request_seq(ids.window_id, &ids.controller_id),
            Some(21),
            "a stale timer (seq 21) must not match the re-armed request (seq 22)"
        );
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        remote_control_engine().take_pending_request(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn repeat_request_while_pending_does_not_duplicate() {
        let ids = remote_control_test_ids("repeat_request_while_pending_does_not_duplicate");
        let first = test_message(
            RemoteControlType::Request,
            None,
            31,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let second = test_message(
            RemoteControlType::Request,
            None,
            32,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let engine = remote_control_engine();
        engine.store_pending_request(ControlGrantKey::for_message(&first).unwrap(), first);
        assert!(engine.has_pending_request(ids.window_id, &ids.controller_id));
        // `park_consent_request` keys its prompt on this flag: a second
        // request replaces the parked message (one key) and re-emits
        // awaitingConsent, but never prompts the sharer twice.
        engine.store_pending_request(ControlGrantKey::for_message(&second).unwrap(), second);
        let mine = engine
            .pending_request_keys()
            .into_iter()
            .filter(|(w, c)| *w == ids.window_id && c == &ids.controller_id)
            .count();
        assert_eq!(mine, 1, "one parked entry per (window, controller)");
        assert_eq!(
            engine.pending_request_seq(ids.window_id, &ids.controller_id),
            Some(32)
        );
        engine.take_pending_request(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn repeat_request_while_pending_keeps_original_timer_and_denies() {
        let ids =
            remote_control_test_ids("repeat_request_while_pending_keeps_original_timer_and_denies");
        let engine = remote_control_engine();
        let first = test_message(
            RemoteControlType::Request,
            None,
            51,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let second = test_message(
            RemoteControlType::Request,
            None,
            52,
            ids.window_id,
            ids.controller_id.clone(),
        );
        // First request parks and arms a timer for seq 51.
        engine.store_pending_request(ControlGrantKey::for_message(&first).unwrap(), first);
        let armed_seq = engine
            .pending_request_seq(ids.window_id, &ids.controller_id)
            .unwrap();
        assert_eq!(armed_seq, 51);
        // Repeat request while pending: `park_consent_request` must NOT store
        // the newer message (this mirrors its `if !already_pending` guard).
        if !engine.has_pending_request(ids.window_id, &ids.controller_id) {
            engine.store_pending_request(ControlGrantKey::for_message(&second).unwrap(), second);
        }
        // The original timer's guard still matches, so it will deny.
        assert_eq!(
            engine.pending_request_seq(ids.window_id, &ids.controller_id),
            Some(armed_seq),
            "the parked seq must stay the one the timer was armed for"
        );
        // ... and the deny path (timer -> answer_consent(false)) takes the entry.
        assert!(engine
            .take_pending_request(ids.window_id, &ids.controller_id)
            .is_some());
        assert!(!engine.has_pending_request(ids.window_id, &ids.controller_id));
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
    }

    #[test]
    fn pending_requests_are_cleared_by_revoke_all() {
        let ids = remote_control_test_ids("pending_requests_are_cleared_by_revoke_all");
        let request = test_message(
            RemoteControlType::Request,
            None,
            41,
            ids.window_id,
            ids.controller_id.clone(),
        );
        let engine = remote_control_engine();
        engine.store_pending_request(ControlGrantKey::for_message(&request).unwrap(), request);
        // `revoke_all` / `revoke_window` / `revoke_controller` all route
        // through `deny_pending_requests_where`, whose engine half is this:
        let taken =
            engine.take_pending_requests_where(|w, c| w == ids.window_id && c == ids.controller_id);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].seq, 41);
        assert!(!engine.has_pending_request(ids.window_id, &ids.controller_id));
        assert!(!is_authorized(ids.window_id, &ids.controller_id));
        // The revoke paths in this file must actually call it (wiring, not
        // just the helper): see the source-grep test in
        // apps/desktop/tests/controlConsent.test.ts.
    }

    #[test]
    fn auto_policy_request_still_authorizes_immediately() {
        // The legacy behaviour survives as an explicit opt-in: under `auto`
        // the gate is Allowed and a token is minted on the spot.
        let ids = remote_control_test_ids("auto_policy_request_still_authorizes_immediately");
        revoke(ids.window_id, &ids.controller_id);
        assert!(RemoteControlPolicy::Auto.allows_requests());
        assert!(matches!(
            apply_request_gate(ids.window_id, &ids.controller_id, RequestGate::Allowed),
            Some(ref token) if token.len() == 32
        ));
        assert!(is_authorized(ids.window_id, &ids.controller_id));
        revoke(ids.window_id, &ids.controller_id);
    }

    #[test]
    fn remote_control_policy_mapping_defaults_to_ask_and_never_upgrades_to_auto() {
        assert_eq!(RemoteControlPolicy::default(), RemoteControlPolicy::Ask);
        assert_eq!(
            RemoteControlPolicy::from_wire("garbage"),
            RemoteControlPolicy::Ask
        );
        assert_eq!(
            RemoteControlPolicy::from_wire("auto"),
            RemoteControlPolicy::Auto
        );
        assert_eq!(
            RemoteControlPolicy::from_wire("off"),
            RemoteControlPolicy::Off
        );
        for policy in [
            RemoteControlPolicy::Off,
            RemoteControlPolicy::Ask,
            RemoteControlPolicy::Auto,
        ] {
            assert_eq!(RemoteControlPolicy::from_u8(policy.as_u8()), policy);
            assert_eq!(RemoteControlPolicy::from_wire(policy.as_wire()), policy);
        }
        // The per-meeting pill's boolean: off is off; on restores the
        // default, and an Off default restores to Ask -- never Auto.
        assert_eq!(
            RemoteControlPolicy::from_allowed(false, RemoteControlPolicy::Auto),
            RemoteControlPolicy::Off
        );
        assert_eq!(
            RemoteControlPolicy::from_allowed(true, RemoteControlPolicy::Auto),
            RemoteControlPolicy::Auto
        );
        assert_eq!(
            RemoteControlPolicy::from_allowed(true, RemoteControlPolicy::Ask),
            RemoteControlPolicy::Ask
        );
        assert_eq!(
            RemoteControlPolicy::from_allowed(true, RemoteControlPolicy::Off),
            RemoteControlPolicy::Ask
        );
        assert_eq!(known_status("awaitingConsent"), Some("awaitingConsent"));
        assert_eq!(known_status("denied"), Some("denied"));
        assert!(is_transient_feedback_status("awaitingConsent"));
        assert!(is_transient_feedback_status("denied"));
        assert_eq!(CONSENT_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn status_packet_carries_the_consent_reason_to_the_controller() {
        let ids =
            remote_control_test_ids("status_packet_carries_the_consent_reason_to_the_controller");
        let status = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "denied",
            message: "The sharer declined remote control.".to_string(),
            grant_token: None,
            reason: Some(RemoteControlReason::ConsentTimedOut),
        };
        let packet = status_packet_for(&status, "native-host");
        assert_eq!(packet.reason, Some(RemoteControlReason::ConsentTimedOut));
        assert_eq!(packet.target_user_id, ids.controller_id);
        let json = serde_json::to_value(&packet).expect("serialize");
        assert_eq!(json["status"], "denied");
        assert_eq!(json["reason"], "consentTimedOut");
        // A status without a reason must not emit the key at all (additive wire).
        let plain = RemoteControlStatus {
            reason: None,
            status: "awaitingConsent",
            ..status
        };
        let json =
            serde_json::to_value(&status_packet_for(&plain, "native-host")).expect("serialize");
        assert_eq!(json["status"], "awaitingConsent");
        assert!(json.get("reason").is_none());
    }

    #[test]
    fn status_packet_routes_host_result_back_to_controller() {
        let ids = remote_control_test_ids("status_packet_routes_host_result_back_to_controller");
        let status = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "active",
            message: "Remote control active for shared window".to_string(),
            grant_token: Some("0123456789abcdef0123456789abcdef".to_string()),
            reason: None,
        };

        let packet = status_packet_for(&status, "host-id");

        assert_eq!(packet.message_type, RemoteControlType::Status);
        assert_eq!(packet.target_user_id, ids.controller_id);
        assert_eq!(packet.controller_id, "host-id");
        assert_eq!(packet.window_id, ids.window_id);
        assert_eq!(packet.status.as_deref(), Some("active"));
        assert_eq!(
            packet.message.as_deref(),
            Some("Remote control active for shared window")
        );
        assert_eq!(
            packet.grant_token.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        // #370 corrective pass: an "active" status packet must unconditionally
        // advertise hot-path support -- this is the ONLY signal a controller
        // uses to decide it may switch pointer/wheel sends to binary.
        assert!(packet.supports_binary_hot_path);
        assert!(unreliable_seq_stream(&packet).is_none());
    }

    #[test]
    fn reconnected_reemits_active_status() {
        let ids = remote_control_test_ids("reconnected_reemits_active_status");
        let status = active_status_for_session(
            ids.window_id,
            ids.controller_id.clone(),
            "0123456789abcdef0123456789abcdef".to_string(),
        );

        // Reconnect must bypass the ordinary same-status de-duplication.
        assert!(should_deliver_status(&status, false));
        assert!(should_deliver_status(&status, true));

        let packet = status_packet_for(&status, "host-id");
        assert_eq!(packet.status.as_deref(), Some("active"));
        assert_eq!(packet.target_user_id, ids.controller_id);
        last_emitted_statuses()
            .lock_unpoisoned()
            .remove(&(ids.window_id, status.controller_id));
    }

    #[test]
    fn accessibility_denied_status_packet_routes_host_result_back_to_controller() {
        let ids = remote_control_test_ids(
            "accessibility_denied_status_packet_routes_host_result_back_to_controller",
        );
        let status = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "accessibilityDenied",
            message: "Petal needs Accessibility permission to replay remote input.".to_string(),
            grant_token: None,
            reason: None,
        };

        let packet = status_packet_for(&status, "host-id");

        assert_eq!(
            known_status("accessibilityDenied"),
            Some("accessibilityDenied")
        );
        assert_eq!(packet.message_type, RemoteControlType::Status);
        assert_eq!(packet.target_user_id, ids.controller_id);
        assert_eq!(packet.controller_id, "host-id");
        assert_eq!(packet.window_id, ids.window_id);
        assert_eq!(packet.status.as_deref(), Some("accessibilityDenied"));
        assert_eq!(
            packet.message.as_deref(),
            Some("Petal needs Accessibility permission to replay remote input.")
        );
    }

    #[test]
    fn controller_timeout_status_is_request_failed_feedback() {
        let ids = remote_control_test_ids("controller_timeout_status_is_request_failed_feedback");

        let status = controller_timeout_status(ids.window_id, "host-id".to_string());

        assert_eq!(known_status(status.status), Some("requestFailed"));
        assert_eq!(status.window_id, ids.window_id);
        assert_eq!(status.controller_id, "host-id");
        assert_eq!(status.message, CONTROLLER_REQUEST_TIMEOUT_MESSAGE);
        assert!(status.message.contains("timed out"));
        assert_eq!(CONTROLLER_REQUEST_TIMEOUT_MS, 8_000);
    }

    #[test]
    fn long_remote_text_is_chunked_without_dropping_characters() {
        let text = format!(
            "{}{}{}",
            "a".repeat(MAX_REPLAY_TEXT_CHARS),
            "b".repeat(MAX_REPLAY_TEXT_CHARS),
            "tail"
        );

        let chunks = remote_text_chunks(&text);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "a".repeat(MAX_REPLAY_TEXT_CHARS));
        assert_eq!(chunks[1], "b".repeat(MAX_REPLAY_TEXT_CHARS));
        assert_eq!(chunks[2], "tail");
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn oversized_inbound_text_reports_limit_and_caps_replay() {
        let ids = remote_control_test_ids("oversized_inbound_text_reports_limit_and_caps_replay");
        let mut message = test_message(
            RemoteControlType::Text,
            None,
            1,
            ids.window_id,
            ids.controller_id,
        );
        message.text = Some(format!("{}tail", "a".repeat(MAX_REPLAY_TEXT_CHARS)));

        assert_eq!(
            enforce_replay_text_limit(&mut message),
            Some((MAX_REPLAY_TEXT_CHARS + 4, MAX_REPLAY_TEXT_CHARS))
        );
        assert_eq!(
            message.text.as_deref(),
            Some("a".repeat(MAX_REPLAY_TEXT_CHARS).as_str())
        );
    }

    #[test]
    fn request_failed_status_is_part_of_wire_contract() {
        assert_eq!(known_status("requestFailed"), Some("requestFailed"));
    }

    #[test]
    fn enabled_request_grants_concurrent_controller_without_displacing_previous_one() {
        // #374: a second controller's allowed Request for the SAME window
        // must ADD a concurrent grant, not displace the first controller's
        // existing one (the old exclusive-authorization behavior this test
        // used to assert).
        let ids = remote_control_test_ids(
            "enabled_request_grants_concurrent_controller_without_displacing_previous_one",
        );

        revoke(ids.window_id, &ids.controller_id);
        revoke(ids.window_id, &ids.other_controller_id);
        assert!(matches!(
            apply_request_gate(ids.window_id, &ids.controller_id, RequestGate::Allowed),
            Some(ref token) if token.len() == 32
        ));
        let new_token = apply_request_gate(
            ids.window_id,
            &ids.other_controller_id,
            RequestGate::Allowed,
        )
        .expect("allowed request should issue a grant");
        assert_eq!(new_token.len(), 32);

        assert!(is_authorized(ids.window_id, &ids.controller_id));
        assert!(is_authorized(ids.window_id, &ids.other_controller_id));
        revoke(ids.window_id, &ids.controller_id);
        revoke(ids.window_id, &ids.other_controller_id);
    }

    #[test]
    fn status_emit_latch_allows_only_status_transitions() {
        let ids = remote_control_test_ids("status_emit_latch_allows_only_status_transitions");
        last_emitted_statuses()
            .lock_unpoisoned()
            .remove(&(ids.window_id, ids.controller_id.clone()));

        let denied = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "accessibilityDenied",
            message: "denied".to_string(),
            grant_token: None,
            reason: None,
        };
        assert!(should_emit_status(&denied));
        assert!(!should_emit_status(&denied));

        let unavailable = RemoteControlStatus {
            status: "targetUnavailable",
            message: "unavailable".to_string(),
            ..denied.clone()
        };
        assert!(should_emit_status(&unavailable));
        assert!(!should_emit_status(&unavailable));
        assert!(should_emit_status(&denied));

        last_emitted_statuses()
            .lock_unpoisoned()
            .remove(&(ids.window_id, ids.controller_id));
    }

    #[test]
    fn revoked_resolve_task_is_dropped_before_status_or_replay() {
        let ids =
            remote_control_test_ids("revoked_resolve_task_is_dropped_before_status_or_replay");
        revoke(ids.window_id, &ids.controller_id);
        last_emitted_statuses()
            .lock_unpoisoned()
            .remove(&(ids.window_id, ids.controller_id.clone()));

        let mut message = test_message(
            RemoteControlType::Pointer,
            Some(RemoteControlAction::Move),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        message.x = Some(0.5);
        message.y = Some(0.5);
        let task = ResolveTask {
            message,
            local_identity: "host-id".to_string(),
            admission: None,
            result_sender: None,
        };

        assert!(!resolve_task_still_authorized(&task.message));
        assert_eq!(
            last_emitted_statuses()
                .lock_unpoisoned()
                .get(&(ids.window_id, ids.controller_id.clone()))
                .copied(),
            None
        );
    }

    #[test]
    fn forced_request_grant_status_bypasses_duplicate_latch_and_updates_cache() {
        let ids = remote_control_test_ids(
            "forced_request_grant_status_bypasses_duplicate_latch_and_updates_cache",
        );
        let key = (ids.window_id, ids.controller_id.clone());
        last_emitted_statuses().lock_unpoisoned().remove(&key);
        let status = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "active",
            message: "Remote control active for shared window".to_string(),
            grant_token: None,
            reason: None,
        };

        assert!(should_deliver_status(&status, false));
        assert!(!should_deliver_status(&status, false));
        assert!(should_deliver_status(&status, true));
        assert_eq!(
            last_emitted_statuses().lock_unpoisoned().get(&key).copied(),
            Some("active")
        );
        assert!(!should_deliver_status(&status, false));

        last_emitted_statuses().lock_unpoisoned().remove(&key);
    }

    #[test]
    fn transient_feedback_reemits_after_identical_status() {
        // The 004A defect: after the 3-second warning clears, an identical
        // transient refusal (occluded) must reach the UI again — unlike
        // lifecycle statuses, which are deduplicated. No grant mutation
        // happens anywhere in this pure decision.
        let ids = remote_control_test_ids("transient_feedback_reemits_after_identical_status");
        let key = (ids.window_id, ids.controller_id.clone());
        last_emitted_statuses().lock_unpoisoned().remove(&key);
        let occluded = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "occluded",
            message: "The shared target is covered at that point.".to_string(),
            grant_token: None,
            reason: None,
        };
        assert!(should_deliver_status(&occluded, false));
        assert!(should_deliver_status(&occluded, false));
        assert!(should_deliver_status(&occluded, false));
        // Transient feedback must NOT pollute the lifecycle latch.
        assert_eq!(
            last_emitted_statuses().lock_unpoisoned().get(&key).copied(),
            None
        );
        last_emitted_statuses().lock_unpoisoned().remove(&key);
    }

    #[test]
    fn lifecycle_statuses_remain_deduplicated_but_feedback_does_not() {
        let ids =
            remote_control_test_ids("lifecycle_statuses_remain_deduplicated_but_feedback_does_not");
        let key = (ids.window_id, ids.controller_id.clone());
        last_emitted_statuses().lock_unpoisoned().remove(&key);
        let active = RemoteControlStatus {
            window_id: ids.window_id,
            owner_identity: None,
            controller_id: ids.controller_id.clone(),
            status: "active",
            message: "Remote control active".to_string(),
            grant_token: None,
            reason: None,
        };
        assert!(should_deliver_status(&active, false));
        assert!(!should_deliver_status(&active, false), "active must dedupe");
        let occluded = RemoteControlStatus {
            status: "occluded",
            message: "covered".to_string(),
            ..active.clone()
        };
        assert!(should_deliver_status(&occluded, false));
        assert!(should_deliver_status(&occluded, false));
        // The transient feedback never overwrote the lifecycle latch, so an
        // active transition after it still dedupes against the earlier active.
        assert!(!should_deliver_status(&active, false));
        last_emitted_statuses().lock_unpoisoned().remove(&key);
    }

    /// `successful_replay_outcome` returns "submitted" for a window-scoped
    /// wheel ONLY under `#[cfg(target_os = "windows")]` (see :2492). This
    /// asserted "submitted" unconditionally and so failed on macOS. Assert the
    /// platform-correct value on BOTH rather than gating the test away, which
    /// would drop the macOS half of a cross-platform contract.
    #[test]
    fn window_wheel_success_outcome_is_platform_scoped() {
        let ids = remote_control_test_ids("window_wheel_success_outcome_is_platform_scoped");
        let mut wheel = test_message(
            RemoteControlType::Wheel,
            None,
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );
        wheel.target_kind = Some(RemoteControlTargetKind::Window);
        // Windows drives a window-scoped wheel it cannot observe the effect of,
        // so success is only ever "submitted". Every other platform observes it.
        #[cfg(target_os = "windows")]
        assert_eq!(successful_replay_outcome(&wheel), "submitted");
        #[cfg(not(target_os = "windows"))]
        assert_eq!(successful_replay_outcome(&wheel), "applied");

        // A display-share wheel (global SendInput) still reports applied.
        wheel.target_kind = Some(RemoteControlTargetKind::Display);
        assert_eq!(successful_replay_outcome(&wheel), "applied");

        // Non-wheel operations: in the default cursor-preserving mode a
        // WINDOW op is best-effort (message/restore routes we cannot verify),
        // so Windows reports `submitted`; other platforms observe real
        // delivery. Set FullControl below to assert the `applied` case.
        let key = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            2,
            ids.window_id,
            ids.controller_id,
        );
        #[cfg(target_os = "windows")]
        {
            assert_eq!(successful_replay_outcome(&key), "submitted");
            // Full-control (real global input to the verified foreground)
            // keeps the applied semantics.
            crate::windows_remote_control::set_share_mode(
                ids.window_id,
                RemoteControlMode::FullControl,
            );
            assert_eq!(successful_replay_outcome(&key), "applied");
            crate::windows_remote_control::set_share_mode(
                ids.window_id,
                RemoteControlMode::CursorPreserving,
            );
        }
        #[cfg(not(target_os = "windows"))]
        assert_eq!(successful_replay_outcome(&key), "applied");
    }

    #[test]
    fn controller_id_mismatch_warn_latch_is_window_and_controller_scoped() {
        let ids = remote_control_test_ids(
            "controller_id_mismatch_warn_latch_is_window_and_controller_scoped",
        );

        assert!(should_warn_controller_id_mismatch(
            ids.window_id,
            &ids.controller_id
        ));
        assert!(!should_warn_controller_id_mismatch(
            ids.window_id,
            &ids.controller_id
        ));
        assert!(should_warn_controller_id_mismatch(
            ids.other_window_id,
            &ids.controller_id
        ));
        assert!(should_warn_controller_id_mismatch(
            ids.window_id,
            &ids.other_controller_id
        ));
    }

    // -- #372: replay-failure feedback + periodic latency summary --

    #[test]
    fn replay_failure_emits_throttled_status() {
        let ids = remote_control_test_ids("replay_failure_emits_throttled_status");
        let t0 = Instant::now();

        // First failure for this (window, controller) is always allowed
        // through.
        assert!(should_emit_replay_failure_status(
            ids.window_id,
            &ids.controller_id,
            t0
        ));
        // A sustained failure (e.g. every pointer-move injection failing)
        // must not spam a status per event within the same second.
        assert!(!should_emit_replay_failure_status(
            ids.window_id,
            &ids.controller_id,
            t0 + Duration::from_millis(1)
        ));
        assert!(!should_emit_replay_failure_status(
            ids.window_id,
            &ids.controller_id,
            t0 + Duration::from_millis(999)
        ));
        // Once the throttle window elapses, the next failure is allowed
        // through again.
        assert!(should_emit_replay_failure_status(
            ids.window_id,
            &ids.controller_id,
            t0 + Duration::from_millis(1_001)
        ));
        // A different controller on the same window is throttled
        // independently.
        assert!(should_emit_replay_failure_status(
            ids.window_id,
            &ids.other_controller_id,
            t0
        ));
    }

    #[test]
    fn notify_replay_failure_is_a_no_op_for_a_controller_without_an_active_grant() {
        // Fable review fix (#372): a failed injection is often a SYNTHETIC
        // release firing after its controller was already revoked/
        // displaced/disconnected (synthetic releases bypass the epoch guard
        // so they still fire post-revoke). Without a grant check,
        // notify_replay_failure would nack a controller that no longer
        // holds a grant, potentially after its own terminal "stopped"
        // status. Verify the grant check short-circuits BEFORE the throttle
        // map is even touched.
        let ids = remote_control_test_ids(
            "notify_replay_failure_is_a_no_op_for_a_controller_without_an_active_grant",
        );
        let message = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id.clone(),
        );

        assert!(active_grant_token(ids.window_id, &ids.controller_id).is_none());
        notify_replay_failure(&message, "boom");

        assert!(
            !replay_failure_status_throttle()
                .lock_unpoisoned()
                .contains_key(&(ids.window_id, ids.controller_id.clone())),
            "notify_replay_failure must return before touching the throttle map when the \
             controller holds no active grant"
        );
    }

    #[test]
    fn revoke_clears_the_replay_failure_status_throttle_entry() {
        // Fable review fix (#372): REPLAY_FAILURE_STATUS_THROTTLE was
        // insert-only -- verify revoke() now drops a departed controller's
        // entry, matching the same cleanup already done for
        // warned_tokenless_inputs.
        let ids = remote_control_test_ids("revoke_clears_the_replay_failure_status_throttle_entry");
        assert!(should_emit_replay_failure_status(
            ids.window_id,
            &ids.controller_id,
            Instant::now()
        ));
        assert!(replay_failure_status_throttle()
            .lock_unpoisoned()
            .contains_key(&(ids.window_id, ids.controller_id.clone())));

        revoke(ids.window_id, &ids.controller_id);

        assert!(
            !replay_failure_status_throttle()
                .lock_unpoisoned()
                .contains_key(&(ids.window_id, ids.controller_id.clone())),
            "revoke() must clear this controller's replay-failure-status throttle entry"
        );
    }

    #[test]
    fn drain_window_control_clears_the_replay_failure_status_throttle_and_warned_tokenless_entries()
    {
        // Fable review fix (#372), round 2: drain_window_control() (the
        // sharing-ended teardown path, not an explicit revoke()) removes
        // every sessions() entry for a window but was missing the same
        // per-departed-controller cleanup revoke()/revoke_controller()
        // already do -- leaking a throttle/warned-once entry for the
        // process lifetime every time a share with an active controller
        // ends via the share-stopped/window-disappeared path.
        let ids = remote_control_test_ids(
            "drain_window_control_clears_the_replay_failure_status_throttle_and_warned_tokenless_entries",
        );
        authorize_shared(ids.window_id, &ids.controller_id);

        assert!(should_emit_replay_failure_status(
            ids.window_id,
            &ids.controller_id,
            Instant::now()
        ));
        warned_tokenless_inputs()
            .lock_unpoisoned()
            .insert((ids.window_id, ids.controller_id.clone()));

        assert!(replay_failure_status_throttle()
            .lock_unpoisoned()
            .contains_key(&(ids.window_id, ids.controller_id.clone())));
        assert!(warned_tokenless_inputs()
            .lock_unpoisoned()
            .contains(&(ids.window_id, ids.controller_id.clone())));

        let (revoked, _releases) = drain_window_control(ids.window_id);
        assert_eq!(revoked, vec![ids.controller_id.clone()]);

        assert!(
            !replay_failure_status_throttle()
                .lock_unpoisoned()
                .contains_key(&(ids.window_id, ids.controller_id.clone())),
            "drain_window_control() must clear this controller's replay-failure-status \
             throttle entry"
        );
        assert!(
            !warned_tokenless_inputs()
                .lock_unpoisoned()
                .contains(&(ids.window_id, ids.controller_id.clone())),
            "drain_window_control() must clear this controller's warned-tokenless-inputs entry"
        );
    }

    #[test]
    fn revoke_clears_the_pointer_position_last_status_and_mismatch_entries() {
        // #410 (Fable round-3 review of #372/#409): CONTROLLER_POINTER_POSITIONS,
        // LAST_EMITTED_STATUSES, and WARNED_CONTROLLER_ID_MISMATCHES were
        // insert-only on every departure path -- unlike warned_tokenless_inputs/
        // replay_failure_status_throttle above, which #409 already fixed here.
        // Verify revoke() now drops a departed controller's entry from all
        // three.
        let ids = remote_control_test_ids(
            "revoke_clears_the_pointer_position_last_status_and_mismatch_entries",
        );
        let key = (ids.window_id, ids.controller_id.clone());
        controller_pointer_positions()
            .lock_unpoisoned()
            .insert(key.clone(), (0.1, 0.2));
        last_emitted_statuses()
            .lock_unpoisoned()
            .insert(key.clone(), "granted");
        warned_controller_id_mismatches()
            .lock_unpoisoned()
            .insert(key.clone());

        assert!(controller_pointer_positions()
            .lock_unpoisoned()
            .contains_key(&key));
        assert!(last_emitted_statuses().lock_unpoisoned().contains_key(&key));
        assert!(warned_controller_id_mismatches()
            .lock_unpoisoned()
            .contains(&key));

        revoke(ids.window_id, &ids.controller_id);

        assert!(
            !controller_pointer_positions()
                .lock_unpoisoned()
                .contains_key(&key),
            "revoke() must clear this controller's pointer-position entry"
        );
        assert!(
            !last_emitted_statuses().lock_unpoisoned().contains_key(&key),
            "revoke() must clear this controller's last-emitted-status entry"
        );
        assert!(
            !warned_controller_id_mismatches()
                .lock_unpoisoned()
                .contains(&key),
            "revoke() must clear this controller's warned-controller-id-mismatch entry"
        );
    }

    #[test]
    fn drain_window_control_clears_the_pointer_position_last_status_and_mismatch_entries() {
        // #410 (Fable round-3 review of #372/#409): same insert-only leak
        // class as replay_failure_status_throttle/warned_tokenless_inputs
        // (see drain_window_control_clears_the_replay_failure_status_throttle_
        // and_warned_tokenless_entries above, from #409) for
        // CONTROLLER_POINTER_POSITIONS, LAST_EMITTED_STATUSES, and
        // WARNED_CONTROLLER_ID_MISMATCHES -- none of these three had cleanup
        // on ANY departure path before this fix. drain_window_control is
        // called out in #410 as "the one most likely to be missed, per
        // #372's own history" since it's the sharing-ended teardown path,
        // not an explicit revoke().
        let ids = remote_control_test_ids(
            "drain_window_control_clears_the_pointer_position_last_status_and_mismatch_entries",
        );
        authorize_shared(ids.window_id, &ids.controller_id);

        let key = (ids.window_id, ids.controller_id.clone());
        controller_pointer_positions()
            .lock_unpoisoned()
            .insert(key.clone(), (0.25, 0.75));
        last_emitted_statuses()
            .lock_unpoisoned()
            .insert(key.clone(), "granted");
        warned_controller_id_mismatches()
            .lock_unpoisoned()
            .insert(key.clone());

        assert!(controller_pointer_positions()
            .lock_unpoisoned()
            .contains_key(&key));
        assert!(last_emitted_statuses().lock_unpoisoned().contains_key(&key));
        assert!(warned_controller_id_mismatches()
            .lock_unpoisoned()
            .contains(&key));

        let (revoked, _releases) = drain_window_control(ids.window_id);
        assert_eq!(revoked, vec![ids.controller_id.clone()]);

        assert!(
            !controller_pointer_positions()
                .lock_unpoisoned()
                .contains_key(&key),
            "drain_window_control() must clear this controller's pointer-position entry"
        );
        assert!(
            !last_emitted_statuses().lock_unpoisoned().contains_key(&key),
            "drain_window_control() must clear this controller's last-emitted-status entry"
        );
        assert!(
            !warned_controller_id_mismatches()
                .lock_unpoisoned()
                .contains(&key),
            "drain_window_control() must clear this controller's warned-controller-id-mismatch \
             entry"
        );
    }

    #[test]
    fn replay_failure_status_kind_maps_stable_operation_feedback() {
        // Matches the exact string produced by `input::accessibility_revoked_error()`.
        let (status, _) = replay_failure_status_kind(
            "accessibilityDenied: Accessibility permission was revoked during remote-control replay",
        );
        assert_eq!(status, "accessibilityDenied");

        #[cfg(target_os = "windows")]
        {
            let (status, _) = replay_failure_status_kind("pointer point belongs to another window");
            assert_eq!(status, "occluded");
        }
        #[cfg(not(target_os = "windows"))]
        {
            let (status, _) = replay_failure_status_kind("some other AX/CGEvent failure: Foo(1)");
            assert_eq!(status, "targetUnavailable");
        }
    }

    #[test]
    fn structural_request_unavailability_is_distinct_from_replay_failure() {
        assert_eq!(
            known_status("requestUnavailable"),
            Some("requestUnavailable")
        );
        assert_eq!(
            replay_failure_status_kind("pointer injection failed").0,
            if cfg!(target_os = "windows") {
                "requestFailed"
            } else {
                "targetUnavailable"
            }
        );
        let fixture = contract_fixture();
        let status = fixture
            .remote_control_messages
            .iter()
            .find(|message| message.name == "status-request-unavailable")
            .and_then(|fixture| fixture.message.get("status"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(status, Some("requestUnavailable"));
    }

    #[test]
    fn notify_replay_failure_is_a_no_op_without_a_room_context() {
        // Without `start_receiver_for_room` ever having run in this test
        // binary/thread, REPLAY_STATUS_CONTEXT is empty; this must not panic
        // and must not publish anything (there's nothing to assert on a
        // publish since it's a no-op, but a panic here would fail the test).
        let ids =
            remote_control_test_ids("notify_replay_failure_is_a_no_op_without_a_room_context");
        let message = test_message(
            RemoteControlType::Key,
            Some(RemoteControlAction::Down),
            1,
            ids.window_id,
            ids.controller_id,
        );
        notify_replay_failure(&message, "boom");
    }

    #[test]
    fn latency_summary_computes_percentiles_and_resets_after_reading() {
        let mut state = LatencySummaryState::default();
        assert!(state.take_summary().is_none());

        for elapsed_ms in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            state.record_success(elapsed_ms);
        }
        state.record_failure();
        state.record_failure();

        let (p50, p95, max, success_count, failure_count) =
            state.take_summary().expect("samples were recorded");
        assert_eq!(p50, 60);
        assert_eq!(p95, 100);
        assert_eq!(max, 100);
        assert_eq!(success_count, 10);
        assert_eq!(failure_count, 2);

        // Reading the summary resets everything.
        assert!(state.take_summary().is_none());
    }

    #[test]
    fn latency_summary_ring_buffer_is_capacity_bounded() {
        let mut state = LatencySummaryState::default();
        for elapsed_ms in 0..(LATENCY_SUMMARY_RING_CAPACITY as u64 * 2) {
            state.record_success(elapsed_ms);
        }
        assert_eq!(state.samples.len(), LATENCY_SUMMARY_RING_CAPACITY);
        // The ring dropped the oldest samples, so the max reflects only the
        // most recent capacity's worth.
        let (_, _, max, success_count, _) = state.take_summary().unwrap();
        assert_eq!(max, LATENCY_SUMMARY_RING_CAPACITY as u64 * 2 - 1);
        assert_eq!(success_count, LATENCY_SUMMARY_RING_CAPACITY as u64 * 2);
    }
}
