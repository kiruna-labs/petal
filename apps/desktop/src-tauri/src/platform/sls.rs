//! SkyLight (SLS) private-API leaf (#748, Phase 4 T0).
//!
//! Every symbol resolves via `dlsym` at runtime (plan §2 distribution stance:
//! a missing symbol demotes a tier, never fails a link or crashes). This
//! module is a LEAF: no registry knowledge, no policy — just the primitives
//! the §3 ladder names, with the CG equivalents as the callers' fallback.
//!
//! Verified spike facts this builds on (plan §9.1–§9.3, measured on this
//! machine, macOS 26.2):
//! - `SLSGetWindowBounds` reads one window's frame in **0.1 µs** (2848×
//!   cheaper than a full CG parse) and needs **no TCC**; a DEAD window
//!   returns **err=1000** — a clean existence check.
//! - The EVENT stream (806/807 moves, 811 create, 804 destroy) requires
//!   Screen Recording (§9.5) — which production Petal always holds — and
//!   per-window `SLSRequestNotificationsForWindows` subscription (yabai's
//!   re-subscribe-on-change pattern).
//! - `SLSFindWindowAndOwner` hit-tests a point in 7.6 µs.
//!
//! PETAL_DISABLE_SLS=1 (degradation drill, §7.4): simulates the private API
//! vanishing on a future macOS — every entry point reports unavailable and
//! callers keep their CG/AX paths.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::sync::OnceLock;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct CgRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

type SlsMainConnectionIdFn = unsafe extern "C" fn() -> i32;
type SlsGetWindowBoundsFn = unsafe extern "C" fn(i32, u32, *mut CgRect) -> i32;
/// `SLSFindWindowAndOwner(cid, 0, 1, 0, screen_point: *const CGPoint,
/// window_point: *mut CGPoint, wid: *mut u32, owner_cid: *mut i32)`.
///
/// EIGHT parameters, not seven. Verified against the SkyLight disassembly on
/// macOS 26.5: the callee unconditionally stores a 16-byte CGPoint through
/// arg 5 (`stp d0, d1, [x22]`), the wid through arg 6, and -- when non-null --
/// the owner connection through arg 7 (`cbz x20; str w8, [x20]`). The
/// previous seven-argument declaration left arg 7 as whatever garbage x7 held
/// at the call site, which is the SIGSEGV at `SLSFindWindowAndOwner + 212`
/// in the 0.9.7 telepointer crash (`sender_loop`, write to a wild pointer).
/// It also made the function write the window-local point over our 4-byte
/// `wid` slot, so the "wid" we read back was the low half of a double (0),
/// and the hit-test silently reported a miss on every tick.
type SlsFindWindowAndOwnerFn = unsafe extern "C" fn(
    i32,
    i32,
    i32,
    i32,
    *const CgPoint,
    *mut CgPoint,
    *mut u32,
    *mut i32,
) -> i32;

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const std::os::raw::c_char) -> *mut c_void;
}
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

fn sls_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var("PETAL_DISABLE_SLS").is_ok())
}

macro_rules! dlsym_fn {
    ($name:literal, $ty:ty) => {{
        static F: OnceLock<Option<usize>> = OnceLock::new();
        F.get_or_init(|| {
            let sym = unsafe {
                dlsym(
                    RTLD_DEFAULT,
                    concat!($name, "\0").as_ptr() as *const std::os::raw::c_char,
                )
            };
            if sym.is_null() {
                None
            } else {
                Some(sym as usize)
            }
        })
        .map(|addr| unsafe { std::mem::transmute::<usize, $ty>(addr) })
    }};
}

fn main_connection() -> Option<i32> {
    if sls_disabled() {
        return None;
    }
    let f = dlsym_fn!("SLSMainConnectionID", SlsMainConnectionIdFn)?;
    static CID: OnceLock<i32> = OnceLock::new();
    Some(*CID.get_or_init(|| unsafe { f() }))
}

/// Whether the SLS read primitives resolve on this machine (canary input).
pub fn sls_reads_available() -> bool {
    main_connection().is_some()
        && dlsym_fn!("SLSGetWindowBounds", SlsGetWindowBoundsFn).is_some()
}

/// One window's RAW frame in **0.1 µs** (§9.2), global top-left points — the
/// same space as CG. `None` when SLS is unavailable OR the call fails.
/// A dead window fails with err=1000, so callers get existence for free.
pub fn window_bounds(wid: u32) -> Option<(f64, f64, f64, f64)> {
    let cid = main_connection()?;
    let f = dlsym_fn!("SLSGetWindowBounds", SlsGetWindowBoundsFn)?;
    let mut rect = CgRect::default();
    // SAFETY: cid is the process's main connection; rect is a valid out ptr.
    let err = unsafe { f(cid, wid, &mut rect) };
    if err != 0 {
        return None;
    }
    Some((rect.x, rect.y, rect.w, rect.h))
}

/// Existence via the bounds read's error semantics (§9.3.3: err=1000 for a
/// dead window). Distinguishes "SLS says gone" (`Some(false)`) from "SLS
/// unavailable — use the CG fallback" (`None`).
pub fn window_exists(wid: u32) -> Option<bool> {
    let cid = main_connection()?;
    let f = dlsym_fn!("SLSGetWindowBounds", SlsGetWindowBoundsFn)?;
    let mut rect = CgRect::default();
    // SAFETY: as above.
    let err = unsafe { f(cid, wid, &mut rect) };
    Some(err == 0)
}

type SlsMoveWindowFn = unsafe extern "C" fn(i32, u32, *const CgPoint) -> i32;
#[repr(C)]
#[derive(Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

/// Move ONE OF OUR OWN windows via WindowServer directly (#761): microseconds,
/// no AppKit, no main-thread hop — the pill's mid-drag moves land in the same
/// compositing cycle as the dragged window's own moves. The §3 "registry is
/// read-only toward WindowServer" rule concerns FOREIGN windows; callers must
/// pass only Petal-owned window ids (AppKit's cached frame is re-synced by
/// the caller's normal path afterward). Returns false when the symbol is
/// unavailable (caller falls back to the AppKit path).
pub fn move_own_window(wid: u32, x: f64, y: f64) -> bool {
    let Some(cid) = main_connection() else {
        return false;
    };
    let Some(f) = dlsym_fn!("SLSMoveWindow", SlsMoveWindowFn) else {
        return false;
    };
    let p = CgPoint { x, y };
    // SAFETY: own-window move on the process's own connection.
    unsafe { f(cid, wid, &p) == 0 }
}

/// Point hit-test via `SLSFindWindowAndOwner` (7.6 µs, §9.3.2). Returns
/// (wid, owner_connection_pid) or `None` on unavailability/miss. NOTE §9.3.2:
/// SLS sees MORE than layer-0 windows — callers apply their own layer policy
/// against the registry record before treating this as a user-window hit.
pub fn find_window_at(x: f64, y: f64) -> Option<(u32, i32)> {
    let cid = main_connection()?;
    let f = dlsym_fn!("SLSFindWindowAndOwner", SlsFindWindowAndOwnerFn)?;
    let screen_point = CgPoint { x, y };
    // The callee writes ALL THREE out-params unconditionally (window_point
    // and wid) or when non-null (owner_cid) -- every one must be a real,
    // correctly-sized buffer. See the `SlsFindWindowAndOwnerFn` doc.
    let mut window_point = CgPoint { x: 0.0, y: 0.0 };
    let mut wid: u32 = 0;
    let mut owner_cid: i32 = 0;
    // SAFETY: signature per yabai's extern.h and the disassembly:
    // (cid, filter_wid=0, 1, 0, &screen_point, &window_point, &wid,
    // &owner_connection). All pointers are valid for the call's duration.
    let err = unsafe {
        f(
            cid,
            0,
            1,
            0,
            &screen_point,
            &mut window_point,
            &mut wid,
            &mut owner_cid,
        )
    };
    let hit = if err != 0 || wid == 0 {
        None
    } else {
        Some((wid, owner_cid))
    };
    note_hit_test(hit);
    hit
}

// ============================================================================
// Hit-test health (the 0.9.7 lesson). `find_window_at` can fail SILENTLY: a
// wrong FFI declaration made it return `None` on every call for an entire
// release while its caller fell back to the registry walk and nothing was
// logged. These counters plus a one-shot LIVE / all-MISS verdict make that
// state visible in the log the release e2e gate greps
// (.github/workflows/nightly-loopback.yml, "SLS hit-test evidence").
// ============================================================================

static HIT_TEST_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HIT_TEST_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HIT_TEST_LIVE_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static HIT_TEST_SILENT_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Calls to `find_window_at` with zero hits before the run is called out as
/// silent. At the telepointer sender's 45 Hz that is ~10 s of a live share --
/// the cursor is always over SOME window (the desktop window spans every
/// display), so a healthy hit-test cannot miss that long.
pub const HIT_TEST_SILENT_THRESHOLD: u64 = 450;

/// What the counters say about the hit-test. Pure; unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTestHealth {
    /// Too few calls to judge (or never called: no share active).
    Undetermined,
    /// At least one real window came back.
    Live,
    /// Many calls, never a window: API/signature drift, the 0.9.7 class.
    Silent,
}

pub fn hit_test_health(calls: u64, hits: u64) -> HitTestHealth {
    if hits > 0 {
        HitTestHealth::Live
    } else if calls >= HIT_TEST_SILENT_THRESHOLD {
        HitTestHealth::Silent
    } else {
        HitTestHealth::Undetermined
    }
}

/// (calls, hits) so far in this process.
pub fn hit_test_counters() -> (u64, u64) {
    (
        HIT_TEST_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        HIT_TEST_HITS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

fn note_hit_test(hit: Option<(u32, i32)>) {
    use std::sync::atomic::Ordering::Relaxed;
    let calls = HIT_TEST_CALLS.fetch_add(1, Relaxed) + 1;
    let hits = match hit {
        Some(_) => HIT_TEST_HITS.fetch_add(1, Relaxed) + 1,
        None => HIT_TEST_HITS.load(Relaxed),
    };
    match hit_test_health(calls, hits) {
        HitTestHealth::Undetermined => {}
        HitTestHealth::Live => {
            if let Some((wid, owner_cid)) = hit {
                if !HIT_TEST_LIVE_LOGGED.swap(true, Relaxed) {
                    log::info!(
                        "winsrv: SLS hit-test LIVE (first hit wid={wid} owner_cid={owner_cid} after {calls} calls)"
                    );
                }
            }
        }
        HitTestHealth::Silent => {
            if !HIT_TEST_SILENT_LOGGED.swap(true, Relaxed) {
                log::warn!(
                    "winsrv: SLS hit-test answered MISS on all {calls} calls -- suspect SkyLight API/signature drift (the 0.9.7 telepointer regression class); callers are running on the registry fallback"
                );
            }
        }
    }
}


// ============================================================================
// Event stream (#748 increment 2). Gated by Screen Recording (§9.5) — which
// production Petal always holds; without it registration succeeds but no
// event ever arrives, so health is judged by DELIVERY, never registration
// (the §9.14 false-positive lesson, applied to SLS).
// ============================================================================

/// Confirmed event codes (plan §9.1, granted spike on this machine).
pub const SLS_EVENT_DESTROYED: u32 = 804;
pub const SLS_EVENT_MOVED: u32 = 806;
pub const SLS_EVENT_RESIZED: u32 = 807;
pub const SLS_EVENT_CREATED: u32 = 811;

/// What one SLS event means for the registry. Pure; unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlsAction {
    /// Window set changed -> mark dirty (sweep reconciles + resubscribes).
    Lifecycle,
    /// Per-step geometry for ONE window -> targeted bounds refresh.
    Geometry,
    Ignore,
}

pub fn classify_sls_event(code: u32) -> SlsAction {
    match code {
        SLS_EVENT_CREATED | SLS_EVENT_DESTROYED => SlsAction::Lifecycle,
        SLS_EVENT_MOVED | SLS_EVENT_RESIZED => SlsAction::Geometry,
        _ => SlsAction::Ignore,
    }
}

/// Extract the wid from an SLS event payload (first 4 LE bytes — the spike's
/// verified layout). Pure; unit-tested.
pub fn payload_wid(data: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = data.get(..4)?.try_into().ok()?;
    let wid = u32::from_le_bytes(bytes);
    (wid != 0).then_some(wid)
}

type SlsRegisterNotifyFn =
    unsafe extern "C" fn(i32, *const c_void, u32, *mut c_void) -> i32;
/// `SLSRemoveConnectionNotifyProc(cid, proc, event, ctx)`. FOUR parameters,
/// same shape as the register — verified against the SkyLight prologue on
/// macOS 26.5, which moves x0..x3 into callee-saved registers exactly like
/// `SLSRegisterConnectionNotifyProc` does (the same disassembly discipline
/// that fixed `SLSFindWindowAndOwner`'s arity above; #88).
type SlsRemoveNotifyFn = SlsRegisterNotifyFn;
type SlsRequestNotificationsFn = unsafe extern "C" fn(i32, *const u32, i32) -> i32;

extern "C" {
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, after_source: bool) -> i32;
    static kCFRunLoopDefaultMode: *const c_void;
}

/// Events delivered since start (health = DELIVERY, not registration).
static EVENTS_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Per-capability delivery counters (§3 "tiers per capability, not global"):
/// moves (806/807) and lifecycle (811/804) can degrade independently.
static MOVE_EVENTS_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LIFECYCLE_EVENTS_SEEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static FIRST_EVENT_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// #88: the WindowServer notification stream is ROOM-SCOPED. Registration is
/// what macOS bills to the Screen Recording grant ("Petal has accessed your
/// screen ... N times"), and the registry it feeds is itself idle-gated, so
/// an always-on registration is pure user-visible privacy cost for zero
/// product value. Off by default; `set_enabled(true)` on room join.
static STREAM_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Events that arrived while disabled and were dropped before touching any
/// Screen-Recording-gated API. Non-zero means WindowServer is still
/// delivering after an unregister — the signature the #88 regression guards.
static SUPPRESSED_WHILE_IDLE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Gated CoreGraphics reads (`CGWindowListCreateDescriptionFromArray`) this
/// callback has performed. The #88 seam: a test drives the REAL callback and
/// asserts this does not move while idle.
static GATED_CG_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether the notification stream is currently registered/allowed to work.
pub fn stream_enabled() -> bool {
    STREAM_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}
/// Count of gated CG reads driven by SLS events (see [`GATED_CG_READS`]).
pub fn gated_cg_reads() -> u64 {
    GATED_CG_READS.load(std::sync::atomic::Ordering::Relaxed)
}
/// Count of events dropped because the stream was disabled.
pub fn suppressed_while_idle() -> u64 {
    SUPPRESSED_WHILE_IDLE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn sls_events_seen() -> u64 {
    EVENTS_SEEN.load(std::sync::atomic::Ordering::Relaxed)
}
/// The T0 feed is LIVE only once real events have arrived (§9.5: registration
/// succeeds even without Screen Recording; delivery does not).
pub fn sls_events_live() -> bool {
    sls_events_seen() > 0
}
/// Moves capability live (806/807 delivered) — the §3 per-capability signal
/// the sweep-demotion condition reads; proven by the nudge canary or any real
/// move.
pub fn sls_moves_live() -> bool {
    MOVE_EVENTS_SEEN.load(std::sync::atomic::Ordering::Relaxed) > 0
}
pub fn sls_lifecycle_live() -> bool {
    LIFECYCLE_EVENTS_SEEN.load(std::sync::atomic::Ordering::Relaxed) > 0
}

extern "C" fn sls_notify_callback(
    code: u32,
    data: *const c_void,
    len: usize,
    _ctx: *mut c_void,
) {
    // #88: hard guard. `SLSRemoveConnectionNotifyProc` is the primary stop,
    // but a delivery already in flight (or a future macOS that ignores the
    // remove) must still never reach the gated CG read below while the app is
    // out of a room. Drop it before any counter or API touch.
    if !STREAM_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        SUPPRESSED_WHILE_IDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    EVENTS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !FIRST_EVENT_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        log::info!("winsrv: SLS event stream LIVE (first code {code})");
    }
    let payload = if data.is_null() || len == 0 {
        &[][..]
    } else {
        // SAFETY: WindowServer owns the buffer for the callback's duration.
        unsafe { std::slice::from_raw_parts(data as *const u8, len.min(64)) }
    };
    match classify_sls_event(code) {
        SlsAction::Ignore => {}
        SlsAction::Lifecycle => {
            LIFECYCLE_EVENTS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::platform::ax_observer::mark_dirty()
        }
        SlsAction::Geometry => {
            MOVE_EVENTS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(wid) = payload_wid(payload) {
                // Targeted per-step refresh. The EVENT is SLS; the GEOMETRY is
                // the CG per-id read (65µs): §9.15.2 — SLS bounds can carry a
                // per-window-class shadow-like margin vs CG, and published
                // frames place the pill/border (#416-class sensitivity), so
                // SLS serves as trigger, never as frame truth.
                GATED_CG_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some((x, y, w, h)) = crate::platform::cg::frame_for_window_id_raw(wid) {
                    if let Some(reg) = crate::window_registry::global() {
                        reg.update_window_frame(wid, x, y, w, h);
                    }
                } else {
                    crate::platform::ax_observer::mark_dirty();
                }
            }
        }
    }
}

/// `MACH_SEND_INVALID_DEST` (mach/message.h:828) -- returned by
/// `SLSRequestNotificationsForWindows` when the window-server Mach port
/// itself is gone. Field evidence (#878): logged 0.16s before a real
/// window-server death on 2026-08-18. Every other nonzero return is a
/// transient per-call failure and must not stop the subscription loop.
const MACH_SEND_INVALID_DEST: i32 = 0x10000003;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlsSendDisposition {
    Continue,
    StopDeadPort,
}

/// Pure decision over a `SLSRequestNotificationsForWindows` return code.
/// Isolated from the send site so the dead-port branch is unit-testable
/// without a live window-server connection (#878).
pub(crate) fn sls_send_disposition(err: i32) -> SlsSendDisposition {
    if err == MACH_SEND_INVALID_DEST {
        SlsSendDisposition::StopDeadPort
    } else {
        SlsSendDisposition::Continue
    }
}

/// Control messages for the `winsrv-sls` thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlsControl {
    /// Converge the per-window 806/807 subscription set.
    Subscribe(Vec<u32>),
    /// Register (join) / unregister (leave) the connection notify procs.
    Enable(bool),
}

/// What the thread must do about registration for a wanted state. Pure so the
/// #88 lifecycle is unit-testable without a WindowServer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamAction {
    Register,
    Unregister,
    Nothing,
}

pub(crate) fn stream_action(registered: bool, want_enabled: bool) -> StreamAction {
    match (registered, want_enabled) {
        (false, true) => StreamAction::Register,
        (true, false) => StreamAction::Unregister,
        _ => StreamAction::Nothing,
    }
}

static CONTROL_TX: OnceLock<std::sync::mpsc::Sender<SlsControl>> = OnceLock::new();

/// Converge the per-window subscription set (ingest thread, on set change —
/// yabai's re-send-the-full-list pattern; the thread dedupes).
pub fn subscribe_windows(wids: Vec<u32>) {
    if let Some(tx) = CONTROL_TX.get() {
        let _ = tx.send(SlsControl::Subscribe(wids));
    }
}

/// Room-membership gate for the whole T0 event feed (#88).
///
/// Registration — not delivery volume — is what attributes Petal to the
/// Screen Recording grant while nothing is being shared, and every consumer
/// of these events (the registry sweep, the hover pill, share borders) is
/// already idle-gated. Enabling flips the in-process guard FIRST so a
/// delivery racing the unregister can never run the gated CG read.
pub fn set_enabled(enabled: bool) {
    let was = STREAM_ENABLED.swap(enabled, std::sync::atomic::Ordering::SeqCst);
    if was && !enabled {
        // Delivery is the ONLY honest health signal (§9.5 / §9.14), so it must
        // not survive an unregister: a second meeting whose re-registration
        // silently failed would otherwise demote the sweep on meeting one's
        // evidence. Reset, and let the ingest loop re-run its move canary.
        EVENTS_SEEN.store(0, std::sync::atomic::Ordering::Relaxed);
        MOVE_EVENTS_SEEN.store(0, std::sync::atomic::Ordering::Relaxed);
        LIFECYCLE_EVENTS_SEEN.store(0, std::sync::atomic::Ordering::Relaxed);
        FIRST_EVENT_LOGGED.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(tx) = CONTROL_TX.get() {
        let _ = tx.send(SlsControl::Enable(enabled));
    }
    if was != enabled {
        log::info!(
            "winsrv: SLS event stream {} (room membership)",
            if enabled { "ENABLED" } else { "DISABLED" }
        );
    }
}

/// Start the winsrv-sls control thread (idempotent).
///
/// #88: this NO LONGER registers anything by itself. The thread parks on its
/// control channel until [`set_enabled(true)`] arrives on a room join, and
/// unregisters again on leave, so an idle Petal holds no WindowServer
/// notification registration at all. Registration without Screen Recording
/// silently yields no events — callers judge the tier by [`sls_events_live`],
/// never by this returning.
pub fn start_event_stream() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(cid) = main_connection() else { return };
    let Some(register) = dlsym_fn!("SLSRegisterConnectionNotifyProc", SlsRegisterNotifyFn)
    else {
        log::info!("winsrv: SLSRegisterConnectionNotifyProc unavailable -- T0 events off");
        return;
    };
    let request = dlsym_fn!(
        "SLSRequestNotificationsForWindows",
        SlsRequestNotificationsFn
    );
    let (tx, rx) = std::sync::mpsc::channel::<SlsControl>();
    let _ = CONTROL_TX.set(tx);
    std::thread::Builder::new()
        .name("winsrv-sls".into())
        .spawn(move || unsafe {
            let remove = dlsym_fn!("SLSRemoveConnectionNotifyProc", SlsRemoveNotifyFn);
            if remove.is_none() {
                // Not fatal: the in-process guard in `sls_notify_callback`
                // still keeps idle Petal off every gated API. But WindowServer
                // would keep delivering, so say so once (#88).
                log::info!(
                    "winsrv: SLSRemoveConnectionNotifyProc unavailable -- SLS registration cannot be released on room exit; the idle guard still blocks all gated reads"
                );
            }
            let mut registered = false;
            let mut subscribed: Vec<u32> = Vec::new();
            'sub: loop {
                // Bounded slices (§9.14.4 lesson: CFRunLoopRun would starve
                // the control channel forever once sources exist).
                while let Ok(msg) = rx.try_recv() {
                    let mut wids = match msg {
                        SlsControl::Enable(want) => {
                            match stream_action(registered, want) {
                                StreamAction::Register => {
                                    for code in [
                                        SLS_EVENT_DESTROYED,
                                        SLS_EVENT_MOVED,
                                        SLS_EVENT_RESIZED,
                                        SLS_EVENT_CREATED,
                                    ] {
                                        let err = register(
                                            cid,
                                            sls_notify_callback as *const c_void,
                                            code,
                                            std::ptr::null_mut(),
                                        );
                                        if err != 0 {
                                            log::info!(
                                                "winsrv: SLS register code {code} err={err}"
                                            );
                                        }
                                    }
                                    registered = true;
                                    log::info!("winsrv: SLS notify procs registered (in room)");
                                }
                                StreamAction::Unregister => {
                                    // Drop the per-window subscription first,
                                    // then the procs themselves.
                                    if let Some(request) = request {
                                        // Aligned, non-null, zero-length --
                                        // never hand a C API a null it might
                                        // deref regardless of the count.
                                        let empty: [u32; 0] = [];
                                        let _ = request(cid, empty.as_ptr(), 0);
                                    }
                                    subscribed.clear();
                                    if let Some(remove) = remove {
                                        for code in [
                                            SLS_EVENT_DESTROYED,
                                            SLS_EVENT_MOVED,
                                            SLS_EVENT_RESIZED,
                                            SLS_EVENT_CREATED,
                                        ] {
                                            let err = remove(
                                                cid,
                                                sls_notify_callback as *const c_void,
                                                code,
                                                std::ptr::null_mut(),
                                            );
                                            if err != 0 {
                                                log::info!(
                                                    "winsrv: SLS remove code {code} err={err}"
                                                );
                                            }
                                        }
                                    }
                                    registered = false;
                                    log::info!(
                                        "winsrv: SLS notify procs released (left room) -- idle Petal holds no window-server notification registration (#88)"
                                    );
                                }
                                StreamAction::Nothing => {}
                            }
                            continue;
                        }
                        SlsControl::Subscribe(wids) => wids,
                    };
                    // A subscription push that arrives while disabled is
                    // dropped: subscribing is itself a screen-gated act, and
                    // the set would otherwise outlive the meeting (the exact
                    // #88 leak -- the old code only ever pushed a NON-empty
                    // set from the in-room branch and never took it back).
                    if !registered {
                        continue;
                    }
                    wids.sort_unstable();
                    wids.dedup();
                    if wids != subscribed {
                        if let Some(request) = request {
                            let err =
                                request(cid, wids.as_ptr(), wids.len().min(1024) as i32);
                            // One line per set CHANGE (a few per meeting):
                            // positive evidence the subscription reached the
                            // server -- its absence cost a blind canary cycle.
                            log::info!(
                                "winsrv: SLS subscription updated n={} err={err}",
                                wids.len()
                            );
                            if sls_send_disposition(err) == SlsSendDisposition::StopDeadPort {
                                log::warn!(
                                    "winsrv: SLSRequestNotificationsForWindows returned \
                                     MACH_SEND_INVALID_DEST (0x10000003) -- the window-server \
                                     Mach port is dead; stopping the winsrv-sls subscription \
                                     loop (#878)"
                                );
                                crate::logging::capture_sentry_diagnostic(
                                    crate::logging::SentryDiagnosticEvent::WindowServerPortDead(
                                        crate::logging::WindowServerPortDeadDiagnostic {
                                            role: crate::logging::DiagnosticRole::Both,
                                        },
                                    ),
                                );
                                // #882 review: the window server's death
                                // SIGKILLs this process ~0.16s after this
                                // point (#878 field timing) -- without a
                                // blocking flush the event above dies in
                                // the in-process queue. This thread is
                                // stopping anyway; spend its last moments
                                // getting the event out.
                                crate::logging::flush_sentry_before_death();
                                break 'sub;
                            }
                        }
                        subscribed = wids;
                    }
                }
                // kCFRunLoopRunFinished (=1): NO sources on this runloop --
                // RunInMode returns IMMEDIATELY and a bare loop spins a full
                // core (live finding: 100% CPU on winsrv-sls; the SLS notify
                // port is not a source on this thread). Sleep-pace instead;
                // event delivery still proved live under this arrangement.
                if CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, false) == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests drive PROCESS-GLOBAL statics (`STREAM_ENABLED` and the
    /// event counters), so they must not interleave with each other.
    static IDLE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// #88: the pure registration lifecycle. Enabling twice must not
    /// re-register (a duplicate notify proc is a duplicate delivery), and
    /// disabling when never registered must not attempt a remove.
    #[test]
    fn stream_action_registers_and_releases_exactly_once() {
        assert_eq!(stream_action(false, true), StreamAction::Register);
        assert_eq!(stream_action(true, true), StreamAction::Nothing);
        assert_eq!(stream_action(true, false), StreamAction::Unregister);
        assert_eq!(stream_action(false, false), StreamAction::Nothing);
    }

    /// #88 REGRESSION (pixels-equivalent for this class: count the real
    /// gated calls, not an event-level proxy).
    ///
    /// Drives the ACTUAL WindowServer callback — not a pure helper it
    /// delegates to — across an idle period, and asserts it performs ZERO
    /// Screen-Recording-gated CoreGraphics reads. A 806 (MOVED) event is the
    /// one that used to run `CGWindowListCreateDescriptionFromArray` per
    /// drag step, forever, for every window that was on screen at the last
    /// in-room sweep. Before the fix this counter moved while out of a room;
    /// that is the "Petal has accessed your screen 110521 times" report.
    #[test]
    fn idle_sls_events_perform_no_gated_screen_reads() {
        let _guard = IDLE_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let payload = 12345u32.to_le_bytes();

        // Out of a room: the production entry point the ingest loop calls.
        crate::window_registry::ingest::apply_room_membership(false);
        assert!(!stream_enabled(), "leaving a room must disarm the SLS stream");

        let before_reads = gated_cg_reads();
        let before_suppressed = suppressed_while_idle();
        for _ in 0..500 {
            sls_notify_callback(
                SLS_EVENT_MOVED,
                payload.as_ptr() as *const c_void,
                payload.len(),
                std::ptr::null_mut(),
            );
            sls_notify_callback(
                SLS_EVENT_RESIZED,
                payload.as_ptr() as *const c_void,
                payload.len(),
                std::ptr::null_mut(),
            );
            sls_notify_callback(SLS_EVENT_CREATED, std::ptr::null(), 0, std::ptr::null_mut());
        }
        assert_eq!(
            gated_cg_reads(),
            before_reads,
            "idle Petal must make ZERO Screen-Recording-gated CG reads from SLS events (#88)"
        );
        assert_eq!(
            suppressed_while_idle() - before_suppressed,
            1500,
            "every idle delivery must be dropped by the guard, not silently counted elsewhere"
        );

        // In a room the same event MUST still drive the targeted refresh --
        // the fix must not turn the feature off, only scope it.
        crate::window_registry::ingest::apply_room_membership(true);
        assert!(stream_enabled(), "joining a room must arm the SLS stream");
        let armed_before = gated_cg_reads();
        sls_notify_callback(
            SLS_EVENT_MOVED,
            payload.as_ptr() as *const c_void,
            payload.len(),
            std::ptr::null_mut(),
        );
        assert_eq!(
            gated_cg_reads(),
            armed_before + 1,
            "an in-room move event must still refresh the window frame"
        );

        // Leave the process disarmed for any other test in this binary.
        crate::window_registry::ingest::apply_room_membership(false);
    }

    /// #88: a lifecycle event carries no window id and never touched CG, but
    /// it must still be dropped while idle -- it is a delivery, and every
    /// delivery is a screen access macOS bills to the user.
    #[test]
    fn idle_lifecycle_events_are_dropped_before_marking_dirty() {
        let _guard = IDLE_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        crate::window_registry::ingest::apply_room_membership(false);
        let before = sls_events_seen();
        sls_notify_callback(SLS_EVENT_DESTROYED, std::ptr::null(), 0, std::ptr::null_mut());
        assert_eq!(
            sls_events_seen(),
            before,
            "an idle delivery must not even count as a live T0 event (#88)"
        );
    }

    /// Regression guard for the 0.9.7 telepointer crash (SIGSEGV inside
    /// `SLSFindWindowAndOwner + 212`): the FFI declaration was missing the
    /// `window_point: *mut CGPoint` parameter, so WindowServer wrote a
    /// 16-byte point over our 4-byte `wid` slot and the owner connection
    /// through whatever garbage x7 held. With the 7-argument declaration this
    /// test fails deterministically (the wid read back is the low half of an
    /// integer-valued double, i.e. 0, so the hit-test reports a miss) --
    /// when it does not fault outright. Needs a WindowServer connection, so
    /// it is `#[ignore]`d like the other live-desktop probes; run with
    /// `cargo test --lib -- --ignored sls::tests::find_window_at_live`.
    #[test]
    #[ignore = "needs a live WindowServer session; run with --ignored"]
    fn find_window_at_live_hit_test_returns_a_window() {
        if sls_disabled() {
            return;
        }
        // Points chosen to sit on SOME window in any session (the desktop
        // window covers the whole display even with nothing else open).
        // Repeat a few times: the pre-fix stack overwrite could survive a
        // single call by luck of register contents, never a loop of them.
        for i in 0..64 {
            let p = 20.0 + i as f64 * 7.0;
            let hit = find_window_at(p, p);
            assert!(
                matches!(hit, Some((wid, _)) if wid != 0),
                "SLSFindWindowAndOwner returned no window at ({p}, {p}): {hit:?}"
            );
        }
    }

    #[test]
    fn hit_test_health_flags_a_silent_run_but_not_a_short_one() {
        assert_eq!(hit_test_health(0, 0), HitTestHealth::Undetermined);
        assert_eq!(
            hit_test_health(HIT_TEST_SILENT_THRESHOLD - 1, 0),
            HitTestHealth::Undetermined,
            "a short run of misses (share just started, cursor off-screen) is not evidence"
        );
        assert_eq!(
            hit_test_health(HIT_TEST_SILENT_THRESHOLD, 0),
            HitTestHealth::Silent,
            "~10 s of a live share with zero hits is the 0.9.7 signature"
        );
        assert_eq!(hit_test_health(1, 1), HitTestHealth::Live);
        assert_eq!(
            hit_test_health(HIT_TEST_SILENT_THRESHOLD * 10, 1),
            HitTestHealth::Live,
            "one real hit ever is enough to rule out a dead FFI path"
        );
    }

    #[test]
    fn sls_send_disposition_stops_only_on_dead_port() {
        assert_eq!(sls_send_disposition(0), SlsSendDisposition::Continue);
        assert_eq!(
            sls_send_disposition(0x10000003),
            SlsSendDisposition::StopDeadPort,
            "MACH_SEND_INVALID_DEST must stop the loop"
        );
        assert_eq!(
            sls_send_disposition(268435460),
            SlsSendDisposition::Continue,
            "a different nonzero mach error is transient, not a dead port"
        );
    }

    #[test]
    fn sls_event_codes_map_to_actions() {
        assert_eq!(classify_sls_event(SLS_EVENT_CREATED), SlsAction::Lifecycle);
        assert_eq!(classify_sls_event(SLS_EVENT_DESTROYED), SlsAction::Lifecycle);
        assert_eq!(classify_sls_event(SLS_EVENT_MOVED), SlsAction::Geometry);
        assert_eq!(classify_sls_event(SLS_EVENT_RESIZED), SlsAction::Geometry);
        assert_eq!(classify_sls_event(808), SlsAction::Ignore);
        assert_eq!(classify_sls_event(1201), SlsAction::Ignore);
    }

    #[test]
    fn payload_wid_reads_first_le_u32_and_rejects_junk() {
        assert_eq!(payload_wid(&[0x39, 0x05, 0, 0, 9, 9]), Some(1337));
        assert_eq!(payload_wid(&[0, 0, 0, 0]), None, "wid 0 is not a window");
        assert_eq!(payload_wid(&[1, 2]), None, "short payload");
        assert_eq!(payload_wid(&[]), None);
    }

    /// The read primitives must resolve on this OS (they underpin the T0
    /// tier), and the drill env must be able to kill them. Symbol resolution
    /// is process-static, so this runs headlessly in ci-local.
    #[test]
    fn sls_read_symbols_resolve_here() {
        assert!(
            sls_reads_available(),
            "SLSMainConnectionID/SLSGetWindowBounds did not dlsym-resolve; \
             if a macOS update removed them the T0 tier must demote (§3) and \
             this test documents the machine where that happened"
        );
    }

    /// SLS bounds SANITY (not parity): §9.15.2 demoted SLS to trigger-only
    /// after a live divergence (a per-window-class shadow-like margin vs CG:
    /// x−8,y−4,w+16,h+8 on one CI run), so exact parity is NOT a product
    /// contract — the contracts that remain are existence semantics and
    /// "real windows never read as the placeholder". Self-skips headless.
    #[test]
    fn sls_bounds_are_sane_for_real_windows() {
        if !sls_reads_available() {
            return;
        }
        let Some(entries) = crate::platform::cg::onscreen_windows_lean() else {
            return;
        };
        // Parity is asserted ONLY for the class the product reads via SLS:
        // layer-0, >=40pt windows (gesture targets / registry records). For
        // system/privileged windows SLS can return a PLACEHOLDER (0,0,1,1)
        // with err=0 -- wrong data, not an error (found by this test's first
        // run against a -15000^2 backstop window; plan §9.15) -- so blanket
        // parity over the raw CG list is not a valid oracle.
        let mut compared = 0;
        let mut placeholders = 0;
        for e in entries.iter().filter(|e| e.layer == 0 && e.w >= 40.0 && e.h >= 40.0) {
            let Ok(wid) = u32::try_from(e.number) else { continue };
            let Some((x, y, w, h)) = window_bounds(wid) else { continue };
            if (x, y, w, h) == (0.0, 0.0, 1.0, 1.0) {
                placeholders += 1; // §9.15 placeholder -- counted, not compared
                continue;
            }
            // Sanity: positive dims, within a loose margin of the CG frame
            // (±32pt covers the observed shadow-class margins; a gross
            // divergence would mean the FFI struct layout broke).
            assert!(
                w > 0.0
                    && h > 0.0
                    && (x - e.x).abs() < 32.0
                    && (y - e.y).abs() < 32.0
                    && (w - e.w).abs() < 64.0
                    && (h - e.h).abs() < 64.0,
                "SLS bounds ({x},{y},{w},{h}) grossly diverge from CG ({},{},{},{}) for wid {wid} -- FFI layout suspect",
                e.x, e.y, e.w, e.h
            );
            compared += 1;
        }
        eprintln!("sls parity: compared={compared} placeholders={placeholders}");
        // At least one REAL window must parity-match on a live session --
        // all-placeholder means SLS bounds are unusable in this trust context
        // and the T0 swap must not proceed on such rigs.
        if entries.iter().any(|e| e.layer == 0 && e.w >= 40.0) {
            assert!(
                compared > 0,
                "every layer-0 window read as the (0,0,1,1) placeholder -- SLS                  bounds unusable in this context (plan §9.15)"
            );
        }
    }

    /// Bounds for a wid that cannot exist: SLS must report failure (None from
    /// window_bounds, Some(false) from window_exists), NOT garbage geometry.
    #[test]
    fn nonexistent_window_reads_as_dead_not_garbage() {
        if !sls_reads_available() {
            return; // covered by the resolve test's failure on such machines
        }
        assert_eq!(window_bounds(u32::MAX - 3), None);
        assert_eq!(window_exists(u32::MAX - 3), Some(false));
    }
}
