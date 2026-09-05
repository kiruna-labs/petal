//! Windows window-change watcher — the event source for the window picker's
//! auto-refresh.
//!
//! Watches the desktop for window **creation, destruction, minimization and
//! restoration** via WinEvent hooks (`SetWinEventHook` with
//! `WINEVENT_OUTOFCONTEXT` — the same events the `dev/dtw.py` sample script
//! demonstrates, in-process). When a relevant event burst settles, the
//! watcher invalidates the window-list cache and emits a debounced
//! `desktop-windows-changed` Tauri event; an OPEN picker listens and does a
//! soft refresh, so its grid follows the desktop without the manual Refresh
//! button.
//!
//! ## Why a dedicated thread + message pump
//!
//! `WINEVENT_OUTOFCONTEXT` delivers callbacks to the *installing thread* via
//! its message queue — the thread must run a Win32 message pump
//! (`GetMessageW`/`DispatchMessageW`) or no event ever arrives. That is the
//! same dedicated-thread-per-Win32-surface pattern the rest of the Windows
//! media stack uses (the compositor's pump thread in `windows_compositor.rs`).
//!
//! ## Debounce
//!
//! Desktop events arrive in bursts (a window create trails SHOW/FOREGROUND
//! siblings, an app launch can fire dozens). Each event (re)arms a
//! `SetTimer` for `DEBOUNCE_MS`, so the refresh fires once, trailing the LAST
//! event of the burst.
//!
//! ## Lifecycle and cost
//!
//! Picker-scoped, NOT always-on: the watcher starts when the picker window
//! opens (`window_picker.rs`) and **self-terminates when the picker is no
//! longer visible** — closed, minimized, hidden-on-meeting-exit, or Win+D'd —
//! checked on a 500ms timer inside the pump (the same visible-and-not-minimized
//! idiom as the receiver compositor's `window_on_screen`). On start it fires
//! ONE immediate `desktop-windows-changed` (the picker's grid may be a stale
//! pre-exit snapshot — hide keeps the webview mounted, so `onMount` does not
//! re-run), then only debounced bursts while running. While running it costs
//! one thread blocked in `GetMessageW` plus four OS hooks whose callback only
//! runs when a relevant event occurs. The callback itself only filters +
//! `PostMessageW`s — all work (timer, cache invalidation, emit) happens in
//! the pump loop, never inside the hook callback.
//!
//! The frontend gates on the same window: the listener lives in
//! `WindowPicker.svelte` and only exists while the picker window is mounted,
//! so events can only ever be received by an open picker.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{
    HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent, WINEVENTPROC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_OBJECT_CREATE,
    EVENT_OBJECT_DESTROY, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GA_ROOT,
    GetAncestor, GetMessageW, GetWindowThreadProcessId, HWND_MESSAGE, IsIconic, IsWindowVisible,
    KillTimer, MSG, OBJID_WINDOW, PostMessageW, RegisterClassW, SetTimer, TranslateMessage,
    WINEVENT_OUTOFCONTEXT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_QUIT, WM_TIMER, WNDCLASSW,
    CS_HREDRAW, CS_VREDRAW,
};

/// Tauri event emitted (debounced) after desktop window events. The window
/// picker listens and soft-refreshes its grid.
pub(crate) const DESKTOP_WINDOWS_CHANGED_EVENT: &str = "desktop-windows-changed";

/// Trailing debounce window: a burst of WinEvents collapses into one refresh,
/// fired this long after the LAST event.
const DEBOUNCE_MS: u32 = 400;

/// Custom messages posted to the pump window (WM_APP + 1/+2; message
/// constants live in a different namespace from the EVENT_* constants, so no
/// collision with EVENT_OBJECT_DESTROY = 0x8001 = WM_APP + 1).
const WM_APP_WINDOW_EVENT: u32 = WM_APP + 1;
const WM_APP_STOP: u32 = WM_APP + 2;

/// SetTimer id for the debounce timer on the pump window.
const DEBOUNCE_TIMER_ID: usize = 1;

/// How often the pump re-checks the picker window's visibility; a hidden or
/// minimized picker terminates the watcher (clean unhook + thread exit in
/// the caller). Coarse is fine — visibility changes are user-paced, unlike
/// the debounce timer.
const VISIBILITY_CHECK_MS: u32 = 500;

/// SetTimer id for the periodic picker-visibility check.
const VISIBILITY_TIMER_ID: usize = 2;

/// WinEvent callback filters on this object id: only whole windows count, not
/// menus/carets/tooltips (those fire EVENT_OBJECT_CREATE with another
/// `idObject`).
const CHILDID_SELF: i32 = 0;

/// Idempotency guard: at most one watcher thread per process. Cleared on
/// every normal exit path; a panic inside the pump thread would leak it
/// (permanently disabling later starts) — the pump path has no panic sources
/// today (Result/Option-based, no unwrap), so this stays a documented edge.
static STARTED: AtomicBool = AtomicBool::new(false);
/// Pump hwnd shared with the WinEvent callback (which runs on the pump
/// thread; it only reads this to post a message).
static PUMP_HWND: AtomicUsize = AtomicUsize::new(0);

/// Start the watcher for an OPEN picker. Idempotent: a second call while a
/// watcher is already running is a no-op (the picker is a singleton window
/// that can be re-shown without a rebuild). On spawn failure the guard is
/// released so a later call can retry. The thread self-terminates when the
/// picker stops being visible — see [`picker_window_is_visible`].
///
/// Known benign race (documented, not fixed): if the picker is closed and
/// REOPENED within the ~500ms visibility-check cadence, the old thread has
/// not yet cleared `STARTED`, so this call no-ops and the reopened picker
/// briefly lacks a watcher (auto-refresh silent until the next open). No code
/// path triggers it today — reopen is a human button click, far slower than
/// 500ms — but if a programmatic reopen flow ever appears, stop the pump
/// synchronously from the picker window's Destroyed event instead of the
/// poll.
pub(crate) fn start(app: AppHandle) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("petal-window-change-watcher".to_string())
        .spawn(move || pump_thread_main(&app))
        .is_ok();
    if !spawned {
        log::warn!("window_change_watcher: failed to spawn watcher thread");
        STARTED.store(false, Ordering::Release);
        return;
    }
    log::info!("window_change_watcher: started (picker visible)");
}

/// Debounced fire: make the next picker fetch see the CURRENT window set,
/// then tell the frontend. The thumbnail cache is deliberately left alone —
/// unchanged windows keep their fresh thumbnails; new/restored windows
/// re-capture on their own.
fn fire(app: &AppHandle) {
    crate::window_source::invalidate_list_cache();
    if let Err(e) = app.emit(DESKTOP_WINDOWS_CHANGED_EVENT, ()) {
        log::debug!("window_change_watcher: emit {DESKTOP_WINDOWS_CHANGED_EVENT} failed: {e}");
    } else {
        log::debug!(
            "window_change_watcher: desktop window change -> invalidated list cache, emitted \
             {DESKTOP_WINDOWS_CHANGED_EVENT}"
        );
    }
}

/// Pure decision: does this WinEvent describe a change the picker cares
/// about — a window created, destroyed, minimized, or restored? Requires a
/// whole-window object (`idObject == OBJID_WINDOW`, `idChild == CHILDID_SELF`)
/// so UI-object churn (menus, tooltips) never triggers a refresh, a TOP-LEVEL
/// window (`is_top_level`) so child-window churn in other apps (controls,
/// tool dialogs) never triggers one either, and an owner other than this
/// process so the picker's own open/close cannot self-trigger.
///
/// `EVENT_OBJECT_DESTROY` is the exception to the pid rule: it often arrives
/// AFTER the owning process died (pid unresolvable). A dead process's window
/// can never be OURS — our windows only die while our process is alive — so
/// `hwnd_pid == 0` is tracked for DESTROY (missing a window close leaves a
/// stale picker card; a DESTROY whose pid we cannot see cannot be a
/// self-trigger).
fn should_track(
    event: u32,
    id_object: i32,
    id_child: i32,
    hwnd_pid: u32,
    self_pid: u32,
    is_top_level: bool,
) -> bool {
    match event {
        EVENT_OBJECT_CREATE | EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND => {
            is_top_level
                && id_object == OBJID_WINDOW.0
                && id_child == CHILDID_SELF
                && hwnd_pid != 0
                && hwnd_pid != self_pid
        }
        EVENT_OBJECT_DESTROY => {
            is_top_level
                && id_object == OBJID_WINDOW.0
                && id_child == CHILDID_SELF
                && hwnd_pid != self_pid
        }
        _ => false,
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    // Keep this callback tiny: it runs inside DispatchMessageW on the pump
    // thread. Filter, then hand the raw event to the pump loop via a posted
    // message — all timer/debounce/fire work happens there.
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // Top-level check: child-window churn in other apps must not refresh the
    // picker. A NULL ancestor means the window is already being torn down
    // (DESTROY) — include it rather than risk missing the close.
    let ancestor = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let is_top_level = ancestor == hwnd || ancestor.0.is_null();
    if !should_track(event, id_object, id_child, pid, std::process::id(), is_top_level) {
        return;
    }
    let pump_hwnd = PUMP_HWND.load(Ordering::Acquire);
    if pump_hwnd == 0 {
        return;
    }
    let _ = unsafe {
        PostMessageW(
            Some(HWND(pump_hwnd as *mut core::ffi::c_void)),
            WM_APP_WINDOW_EVENT,
            WPARAM(0),
            LPARAM(0),
        )
    };
}

fn pump_thread_main(app: &AppHandle) {
    if !register_watcher_class() {
        STARTED.store(false, Ordering::Release);
        return;
    }
    let Some(pump_hwnd) = create_watcher_pump_window() else {
        log::error!("window_change_watcher: failed to create pump window");
        STARTED.store(false, Ordering::Release);
        return;
    };
    PUMP_HWND.store(pump_hwnd.0 as usize, Ordering::Release);

    // Initial fire: the picker was just (re)shown after a gap — hide-on-leave
    // keeps the webview MOUNTED, so onMount does not re-run and the grid would
    // otherwise show the pre-exit window set until the next desktop event or a
    // manual Refresh. (The destroy-on-close path is covered anyway: its fresh
    // onMount does a full refresh; this fire's soft refresh is then coalesced
    // by the picker's refreshSeq.)
    log::debug!("window_change_watcher: initial refresh on watcher start");
    fire(app);

    let picker_visible = || picker_window_is_visible(app);
    let fire = || fire(app);
    pump_with_hooks(pump_hwnd, &fire, &picker_visible);
    log::info!("window_change_watcher: stopped (picker no longer visible)");

    let _ = unsafe { DestroyWindow(pump_hwnd) };
    PUMP_HWND.store(0, Ordering::Release);
    STARTED.store(false, Ordering::Release);
}

/// Install the WinEvent hooks, pump until stopped, then unhook. Unhook
/// happens on the SAME thread that installed the hooks (the pump thread), so
/// a late event can never dispatch into a callback whose thread context is
/// already gone.
fn pump_with_hooks(hwnd: HWND, fire: &dyn Fn(), picker_visible: &dyn Fn() -> bool) {
    let hooks = install_hooks(Some(win_event_proc));
    if hooks.is_empty() {
        log::error!("window_change_watcher: no WinEvent hooks installed; watcher disabled");
        return;
    }
    run_pump_loop(hwnd, fire, picker_visible);
    for hook in hooks {
        let _ = unsafe { UnhookWinEvent(hook) };
    }
}

/// Message pump + trailing debounce + picker-visibility life gate. `fire` is
/// invoked on the pump thread once per settled event burst; returns on
/// WM_QUIT/WM_APP_STOP, or when a visibility tick finds the picker no longer
/// visible.
fn run_pump_loop(hwnd: HWND, fire: &dyn Fn(), picker_visible: &dyn Fn() -> bool) {
    // The watcher lives only while the picker is visible; a periodic check
    // stops the pump (and thus the hooks + thread) shortly after the picker
    // closes, minimizes, or gets Win+D'd. A failed timer would leave the pump
    // with NO production termination path (nothing else sends WM_QUIT), so
    // treat it as fatal — same stance as an empty hooks install below.
    let visibility_timer =
        unsafe { SetTimer(Some(hwnd), VISIBILITY_TIMER_ID, VISIBILITY_CHECK_MS, None) };
    if visibility_timer == 0 {
        log::error!("window_change_watcher: failed to arm picker-visibility timer; watcher disabled");
        return;
    }
    let mut msg = MSG::default();
    let mut running = true;
    while running {
        let get_result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if get_result.0 == 0 || get_result.0 == -1 {
            break; // WM_QUIT retrieved, or error
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        match msg.message {
            WM_QUIT => running = false,
            WM_APP_STOP => running = false,
            // (Re)arm the debounce timer on every event: SetTimer with the
            // same id replaces the pending timer, so the fire lands
            // DEBOUNCE_MS after the LAST event of the burst.
            WM_APP_WINDOW_EVENT => {
                let _ = unsafe { SetTimer(Some(hwnd), DEBOUNCE_TIMER_ID, DEBOUNCE_MS, None) };
            }
            WM_TIMER if msg.wParam.0 == DEBOUNCE_TIMER_ID => {
                let _ = unsafe { KillTimer(Some(hwnd), DEBOUNCE_TIMER_ID) };
                fire();
            }
            WM_TIMER if msg.wParam.0 == VISIBILITY_TIMER_ID => {
                if !picker_visible() {
                    running = false;
                }
            }
            _ => {}
        }
    }
}

/// Whether the picker window is actually on the desktop: exists, Win32-visible
/// (not hidden), and not minimized. Same visible-and-not-minimized idiom as
/// the receiver compositor's `window_on_screen`; virtual-desktop awareness is
/// a documented placeholder there and deliberately skipped here too (a picker
/// on another virtual desktop is a rare edge, and the missed refreshes are
/// harmless). This is the watcher's life gate.
fn picker_window_is_visible(app: &AppHandle) -> bool {
    use tauri::Manager;
    let Some(window) = app.get_webview_window(crate::window_picker::WINDOW_PICKER_LABEL) else {
        return false; // picker closed: the expected "stop the watcher" signal
    };
    let Ok(hwnd) = window.hwnd() else {
        // Window registered but native handle momentarily unavailable — an
        // anomalous state, NOT a normal close. Log it so a silently-dead
        // watcher is diagnosable instead of looking like a deliberate stop.
        log::debug!("window_change_watcher: picker window exists but hwnd() failed");
        return false;
    };
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    visible && !minimized
}

fn install_hooks(proc: WINEVENTPROC) -> Vec<HWINEVENTHOOK> {
    let events = [
        EVENT_OBJECT_CREATE,
        EVENT_OBJECT_DESTROY,
        EVENT_SYSTEM_MINIMIZESTART,
        EVENT_SYSTEM_MINIMIZEEND,
    ];
    let mut hooks = Vec::with_capacity(events.len());
    for event in events {
        // WINEVENT_OUTOFCONTEXT: the callback runs on OUR pump thread (the
        // installing thread) as queued messages — that thread is pumping
        // below. None/0/0 -> all processes, all threads.
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                proc,
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            log::warn!("window_change_watcher: SetWinEventHook(0x{event:04X}) failed");
        } else {
            hooks.push(hook);
        }
    }
    hooks
}

/// Hidden message-only pump window class. Registered once; re-registration
/// after the first (e.g. a second pump in tests) is tolerated via
/// ERROR_CLASS_ALREADY_EXISTS, same as `windows_compositor.rs`.
const PUMP_WINDOW_CLASS: &str = "PetalWindowChangeWatcherPump";

fn register_watcher_class() -> bool {
    let instance: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(instance) => instance.into(),
        Err(error) => {
            log::error!("window_change_watcher: GetModuleHandleW failed: {error}");
            return false;
        }
    };
    let name: Vec<u16> = PUMP_WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(watcher_pump_proc),
        hInstance: instance,
        lpszClassName: windows::core::PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    let result = unsafe { RegisterClassW(&class) };
    if result == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            log::error!(
                "window_change_watcher: RegisterClassW failed (0x{:08X})",
                error.0
            );
            return false;
        }
    }
    true
}

fn create_watcher_pump_window() -> Option<HWND> {
    let class_name: Vec<u16> = PUMP_WINDOW_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.ok()?.into();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

unsafe extern "system" fn watcher_pump_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The pump loop handles everything it cares about from the retrieved
    // message; the window proc only needs the default handling.
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    // ---- pure event filter ------------------------------------------------

    #[test]
    fn tracked_events_require_a_whole_window() {
        let other_pid = 4242;
        for event in [
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_DESTROY,
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
        ] {
            assert!(
                should_track(event, OBJID_WINDOW.0, CHILDID_SELF, other_pid, 99, true),
                "event 0x{event:04X} on a whole foreign top-level window must be tracked"
            );
            // A menu/caret/tooltip object (idObject != OBJID_WINDOW) is not a
            // window change — must not refresh the picker.
            assert!(!should_track(event, OBJID_WINDOW.0 + 1, CHILDID_SELF, other_pid, 99, true));
            // Child objects (idChild != CHILDID_SELF) are not window changes.
            assert!(!should_track(event, OBJID_WINDOW.0, CHILDID_SELF + 1, other_pid, 99, true));
            // Child WINDOWS (a control inside another app's window) are not
            // changes to the shareable top-level set — a create/destroy of a
            // dialog control must not refresh the picker.
            assert!(!should_track(event, OBJID_WINDOW.0, CHILDID_SELF, other_pid, 99, false));
        }
    }

    #[test]
    fn own_process_windows_are_not_tracked() {
        // The picker's own open/close (CREATE/DESTROY) must not self-trigger.
        for event in [
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_DESTROY,
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
        ] {
            assert!(!should_track(event, OBJID_WINDOW.0, CHILDID_SELF, 99, 99, true));
        }
    }

    #[test]
    fn unresolved_pid_is_tracked_only_for_destroy() {
        // A CREATE with pid 0 (GetWindowThreadProcessId failed) could be our
        // own window — safer to ignore.
        assert!(!should_track(
            EVENT_OBJECT_CREATE,
            OBJID_WINDOW.0,
            CHILDID_SELF,
            0,
            99,
            true
        ));
        // A DESTROY with pid 0 arrives after the owning process died; it can
        // never be OUR window (ours only die while our process is alive), so
        // it IS tracked — missing it leaves a stale picker card.
        assert!(should_track(
            EVENT_OBJECT_DESTROY,
            OBJID_WINDOW.0,
            CHILDID_SELF,
            0,
            99,
            true
        ));
        assert!(!should_track(
            EVENT_OBJECT_DESTROY,
            OBJID_WINDOW.0,
            CHILDID_SELF,
            99,
            99,
            true
        ));
    }

    #[test]
    fn unrelated_events_are_ignored() {
        assert!(!should_track(
            0x0003, // EVENT_SYSTEM_FOREGROUND: focus changes are not refresh triggers
            OBJID_WINDOW.0,
            CHILDID_SELF,
            4242,
            99,
            true
        ));
    }

    // ---- debounce + pump (real Win32 message pump, no OS events needed) ----

    fn start_test_pump() -> (thread::JoinHandle<()>, mpsc::Receiver<()>, HWND) {
        let (fire_tx, fire_rx) = mpsc::channel();
        // HWND is not Send in the windows crate; ship the raw pointer value.
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let handle = thread::spawn(move || {
            assert!(register_watcher_class(), "test pump class must register");
            let hwnd =
                create_watcher_pump_window().expect("test pump window must be creatable");
            let _ = ready_tx.send(hwnd.0 as usize);
            // Tests keep the pump alive: `picker_visible` always true. The
            // visibility self-termination is covered by its own test below.
            run_pump_loop(hwnd, &|| {
                let _ = fire_tx.send(());
            }, &|| true);
        });
        let hwnd = HWND(
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("pump thread must start") as *mut core::ffi::c_void,
        );
        (handle, fire_rx, hwnd)
    }

    fn stop_test_pump(handle: thread::JoinHandle<()>, hwnd: HWND) {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_STOP, WPARAM(0), LPARAM(0)) };
        handle.join().expect("pump thread must exit cleanly");
    }

    fn post_event(hwnd: HWND) {
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_APP_WINDOW_EVENT,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }

    /// A burst of events must collapse into exactly ONE debounced fire,
    /// delivered AFTER the debounce window (not immediately, not per event).
    /// The lower bound is stall-proof: a slow scheduler makes the fire LATER,
    /// never earlier, so asserting `>= DEBOUNCE_MS` cannot flake.
    #[test]
    fn burst_of_events_fires_exactly_once_after_debounce() {
        let (handle, fire_rx, hwnd) = start_test_pump();
        // Simulate a burst: 10 events over ~200ms (like a window-create
        // cascade). Anchor the timing bound to the FIRST post: a trailing
        // debounce cannot fire before first-post + DEBOUNCE_MS (the timer is
        // armed no earlier than the first event, and each re-arm extends it),
        // and a slow scheduler only makes the fire LATER — so the bound is
        // stall-proof in both directions.
        let first_post_at = Instant::now();
        for _ in 0..10 {
            post_event(hwnd);
            std::thread::sleep(Duration::from_millis(20));
        }
        let first = fire_rx
            .recv_timeout(Duration::from_millis(DEBOUNCE_MS as u64 + 2_000))
            .expect("exactly one debounced fire expected");
        let _ = first;
        assert!(
            Instant::now().duration_since(first_post_at) >= Duration::from_millis(DEBOUNCE_MS as u64),
            "the debounced fire must trail the burst (trailing debounce), not arrive immediately"
        );
        // No second fire: the burst collapsed into one.
        assert!(
            fire_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "a burst must collapse into a single fire, not one per event"
        );
        stop_test_pump(handle, hwnd);
    }

    /// Back-to-back event bursts (separated by more than the debounce window)
    /// fire once EACH — the debounce collapses per burst, not globally.
    #[test]
    fn separated_bursts_fire_once_each() {
        let (handle, fire_rx, hwnd) = start_test_pump();
        post_event(hwnd);
        post_event(hwnd);
        let first = fire_rx
            .recv_timeout(Duration::from_millis(DEBOUNCE_MS as u64 + 2_000))
            .expect("first burst must fire");
        let _ = first;
        // Let the timer state settle, then a second, clearly-separated burst.
        std::thread::sleep(Duration::from_millis(DEBOUNCE_MS as u64 + 500));
        post_event(hwnd);
        let second = fire_rx
            .recv_timeout(Duration::from_millis(DEBOUNCE_MS as u64 + 2_000))
            .expect("second burst must fire once more");
        let _ = second;
        assert!(
            fire_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "each burst must produce exactly one fire"
        );
        stop_test_pump(handle, hwnd);
    }

    // ---- picker-visibility life gate ---------------------------------------

    /// The watcher must self-terminate shortly after the picker stops being
    /// visible (closed / minimized / Win+D) — a picker the user cannot see
    /// must not keep the watcher thread + OS hooks running. The visibility
    /// predicate is injected here (a controllable flag), so this exercises
    /// the pump's life-gate timer; the real `picker_window_is_visible` is a
    /// thin IsWindowVisible/IsIconic check over the picker window's hwnd.
    #[test]
    fn pump_self_terminates_when_picker_no_longer_visible() {
        let visible = Arc::new(AtomicBool::new(true));
        let visible_for_pump = Arc::clone(&visible);
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let handle = thread::spawn(move || {
            assert!(register_watcher_class(), "test pump class must register");
            let hwnd =
                create_watcher_pump_window().expect("test pump window must be creatable");
            let _ = ready_tx.send(hwnd.0 as usize);
            run_pump_loop(
                hwnd,
                &|| {},
                &(move || visible_for_pump.load(Ordering::Relaxed)),
            );
        });
        let _pump_hwnd = HWND(
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("pump thread must start") as *mut core::ffi::c_void,
        );
        // Picker "closes": the pump must exit on its own, without WM_APP_STOP,
        // within the visibility-check cadence plus margin.
        visible.store(false, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            handle.is_finished(),
            "the pump must self-terminate once the picker is no longer visible"
        );
        handle.join().expect("pump must exit cleanly");
    }

    // ---- real OS event path (opt-in) --------------------------------------

    fn register_smoke_class(name: &str) -> bool {
        let instance: HINSTANCE =
            unsafe { GetModuleHandleW(None) }.ok().expect("module handle").into();
        let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(watcher_pump_proc),
            hInstance: instance,
            lpszClassName: windows::core::PCWSTR(name_wide.as_ptr()),
            ..Default::default()
        };
        let result = unsafe { RegisterClassW(&class) };
        result != 0 || unsafe { GetLastError() } == ERROR_CLASS_ALREADY_EXISTS
    }

    /// Unfiltered hook callback for the smoke test: delivers EVERY window
    /// event to the pump, so a window owned by OUR OWN test process can prove
    /// the real OS -> hook -> pump -> debounce -> fire chain end to end. The
    /// production self-pid/OBJID_WINDOW filter is a separate pure function
    /// (`should_track`) covered by its own unit tests above — a filtered
    /// callback here could never observe a self-owned window's minimize.
    unsafe extern "system" fn smoke_event_proc(
        _hook: HWINEVENTHOOK,
        _event: u32,
        _hwnd: HWND,
        _id_object: i32,
        _id_child: i32,
        _event_thread: u32,
        _event_time: u32,
    ) {
        let pump_hwnd = PUMP_HWND.load(Ordering::Acquire);
        if pump_hwnd == 0 {
            return;
        }
        let _ = unsafe {
            PostMessageW(
                Some(HWND(pump_hwnd as *mut core::ffi::c_void)),
                WM_APP_WINDOW_EVENT,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }

    /// Real end-to-end path: real desktop window events (a real window
    /// created + minimized on the desktop) must reach the pump through the
    /// actual WinEvent hook and fire the debounced event. Opt-in via
    /// `PETAL_TEST_REAL_WINEVENTS=1` because it needs an interactive desktop
    /// session and briefly flashes a window — the default test run stays
    /// headless-deterministic (the synthetic-message tests above cover the
    /// pump/debounce; this covers the hook wiring). The smoke window's OWNER
    /// thread (this one) must pump messages for the OS to generate/deliver
    /// the events — observed empirically, hence the pump loop below.
    #[test]
    fn real_winevent_minimize_fires_debounced_event() {
        if std::env::var("PETAL_TEST_REAL_WINEVENTS").as_deref() != Ok("1") {
            eprintln!(
                "skipping real WinEvent smoke test (set PETAL_TEST_REAL_WINEVENTS=1 on an \
                 interactive desktop to run)"
            );
            return;
        }
        use windows::Win32::UI::WindowsAndMessaging::{
            IsIconic, PeekMessageW, ShowWindow, SW_MINIMIZE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
            PM_REMOVE,
        };

        let (fire_tx, fire_rx) = mpsc::channel();
        // HWND is not Send in the windows crate; ship the raw pointer value.
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let handle = thread::spawn(move || {
            assert!(register_watcher_class(), "pump class must register");
            let pump_hwnd = create_watcher_pump_window().expect("pump window");
            let _ = ready_tx.send(pump_hwnd.0 as usize);
            // The hook callback posts through this static; production sets it
            // in pump_thread_main, the smoke test must too.
            PUMP_HWND.store(pump_hwnd.0 as usize, Ordering::Release);
            let hooks = install_hooks(Some(smoke_event_proc));
            assert!(!hooks.is_empty(), "smoke hooks must install");
            run_pump_loop(
                pump_hwnd,
                &|| {
                    let _ = fire_tx.send(());
                },
                &|| true, // smoke test keeps the pump alive; visibility gate is its own test
            );
            PUMP_HWND.store(0, Ordering::Release);
            for hook in hooks {
                let _ = unsafe { UnhookWinEvent(hook) };
            }
        });
        let pump_hwnd = HWND(
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("pump with hooks must start") as *mut core::ffi::c_void,
        );

        const SMOKE_CLASS: &str = "PetalWinEventSmoke";
        assert!(register_smoke_class(SMOKE_CLASS), "smoke class must register");
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.ok().expect("module").into();
        let class_wide: Vec<u16> = SMOKE_CLASS
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let title_wide: Vec<u16> = "Petal watcher smoke".encode_utf16().collect();
        // VISIBLE (WS_VISIBLE): a hidden window minimized is a hide->minimize
        // transition and never generates EVENT_SYSTEM_MINIMIZESTART.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class_wide.as_ptr()),
                windows::core::PCWSTR(title_wide.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                0,
                0,
                240,
                160,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("smoke window must be creatable");

        // Pump THIS thread (the smoke window's owner) so the OS generates and
        // delivers the WinEvents; minimize once creation has settled, and
        // expect the debounced fire within the deadline. The minimize runs on
        // a time check, NOT only when the queue is empty, so a busy message
        // stream can never starve it.
        let start = Instant::now();
        let deadline = start + Duration::from_secs(3);
        let mut minimized = false;
        let mut fired = false;
        while Instant::now() < deadline {
            let mut msg = MSG::default();
            let has = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
            if has {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if !minimized && Instant::now().duration_since(start) > Duration::from_millis(200) {
                let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
                assert!(
                    unsafe { IsIconic(hwnd) }.as_bool(),
                    "the smoke window must actually be minimized for the event to fire"
                );
                minimized = true;
            }
            if fire_rx.try_recv().is_ok() {
                fired = true;
                break;
            }
            if !has {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(minimized, "the smoke window must have been minimized");

        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = unsafe { PostMessageW(Some(pump_hwnd), WM_APP_STOP, WPARAM(0), LPARAM(0)) };
        handle.join().expect("pump must exit cleanly");

        assert!(
            fired,
            "real desktop window events (create/minimize) must fire the debounced event \
             through the real WinEvent hook path"
        );
    }
}
