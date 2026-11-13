//! Streamer mode: what Petal hides while someone could be watching.
//!
//! Active when this computer's own client-mode stream is running (from the
//! moment going live starts until it stops), or when Settings -> Live
//! streaming -> "Always use streamer mode" is on. While active, the webviews
//! mask meeting names and codes (`streamer-mode-changed`).
//!
//! Capture exclusion follows only this computer's own stream, never the
//! forced setting (OBS and similar tools still capture Petal normally):
//!
//! * Windows: every Petal top-level window except the Petal View selectors
//!   (`region-window-*`, which keep their own lease) gets
//!   `WDA_EXCLUDEFROMCAPTURE`, reconciled every second so windows opened
//!   mid-stream are covered. Only windows that were `WDA_NONE` are changed,
//!   so only those are restored; always-excluded overlays stay excluded.
//! * macOS: whole-display shares rebuild their ScreenCaptureKit filter
//!   without Petal ([`crate::session::apply_streamer_capture_exclusion`]);
//!   region shares already exclude it.
//!
//! Either way a public stream of a display never shows the meeting, its
//! chat, other participants' meeting-only shares, or the go-live dialog.
//!
//! Going live applies the exclusion first ([`begin_live`]) and waits for it.
//! Every captured frame then says whether it was taken after the exclusion
//! took effect (macOS: `CapturedFrame::excludes_petal_windows`; Windows:
//! [`frame_excludes_petal`] on WGC's frame time), and the stream's tap drops
//! the ones that weren't (#487).

use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::sync_ext::MutexExt;

pub const STREAMER_MODE_CHANGED_EVENT: &str = "streamer-mode-changed";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamerModeState {
    /// Mask meeting names and codes.
    pub active: bool,
    /// This computer is streaming (and so excludes Petal from captures).
    pub live: bool,
}

/// The two inputs; the state follows from them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Inputs {
    forced: bool,
    live: bool,
}

impl Inputs {
    fn state(self) -> StreamerModeState {
        StreamerModeState { active: self.forced || self.live, live: self.live }
    }
}

fn inputs() -> &'static Mutex<Inputs> {
    static INPUTS: OnceLock<Mutex<Inputs>> = OnceLock::new();
    INPUTS.get_or_init(|| Mutex::new(Inputs::default()))
}

/// Apply a change; `Some(new state)` when the state changed.
fn change(update: impl FnOnce(&mut Inputs)) -> Option<StreamerModeState> {
    let mut guard = inputs().lock_unpoisoned();
    let before = guard.state();
    update(&mut guard);
    let after = guard.state();
    (after != before).then_some(after)
}

pub fn current() -> StreamerModeState {
    inputs().lock_unpoisoned().state()
}

/// Whether captures must leave Petal out right now (this computer is live).
pub fn capture_exclusion_active() -> bool {
    current().live
}

/// Seed the forced setting at startup, before any window asks.
pub fn initialize(forced: bool) {
    inputs().lock_unpoisoned().forced = forced;
}

/// "Always use streamer mode" changed.
pub fn set_forced(app: &AppHandle, forced: bool) {
    if let Some(state) = change(|inputs| inputs.forced = forced) {
        let _ = app.emit(STREAMER_MODE_CHANGED_EVENT, state);
    }
}

/// This computer's stream started (`true`) or stopped.
pub fn set_live(app: &AppHandle, live: bool) {
    let before = current();
    let Some(state) = change(|inputs| inputs.live = live) else { return };
    if state.live != before.live {
        apply_capture_exclusion(app, state.live);
    }
    log::info!("streaming: streamer mode active={} live={}", state.active, state.live);
    let _ = app.emit(STREAMER_MODE_CHANGED_EVENT, state);
}

pub const EXCLUSION_FAILED_ERROR: &str = "Couldn't keep Petal's windows out of this share. Try again.";

/// Going live: streamer mode on, and Petal's windows out of every capture
/// before anything is tapped. Fails (with streamer mode off again) when the
/// share about to stream couldn't be rebuilt without Petal.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub async fn begin_live(app: &AppHandle, window_id: u32) -> Result<(), String> {
    let before = current();
    let Some(state) = change(|inputs| inputs.live = true) else { return Ok(()) };
    log::info!("streaming: streamer mode active={} live={}", state.active, state.live);
    let _ = app.emit(STREAMER_MODE_CHANGED_EVENT, state);
    if before.live {
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let _ = window_id;
        if !windows_affinity::set_active(app, true) {
            set_live(app, false);
            return Err(EXCLUSION_FAILED_ERROR.into());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let failed = apply_current_exclusion(app).await;
        if failed.contains(&window_id) {
            set_live(app, false);
            return Err(EXCLUSION_FAILED_ERROR.into());
        }
        Ok(())
    }
}

/// Whether a frame WGC composed at `captured_at_100ns` (QPC, 100 ns units)
/// leaves Petal out: taken after the exclusion took effect.
#[cfg(target_os = "windows")]
pub fn frame_excludes_petal(captured_at_100ns: i64) -> bool {
    frame_after_fence(windows_affinity::fence(), captured_at_100ns)
}

/// `fence` is when the exclusion took effect (`i64::MAX` = not in effect).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn frame_after_fence(fence: i64, captured_at: i64) -> bool {
    fence != i64::MAX && captured_at >= fence
}

#[tauri::command]
pub fn streamer_mode_state() -> StreamerModeState {
    current()
}

#[cfg(target_os = "windows")]
fn apply_capture_exclusion(app: &AppHandle, excluded: bool) {
    let _ = windows_affinity::set_active(app, excluded);
}

/// Excludes each Petal window as it is created while live, rather than on
/// the next reconcile (Windows; registered in lib.rs's non-macOS builder).
#[cfg(not(target_os = "macos"))]
pub fn window_creation_plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("petal-streamer-exclusion")
        .on_window_ready(|window| {
            #[cfg(target_os = "windows")]
            if let Ok(hwnd) = window.hwnd() {
                windows_affinity::on_window_created(hwnd.0 as isize, window.label());
            }
            #[cfg(not(target_os = "windows"))]
            let _ = window;
        })
        .build()
}

/// A Petal View selector's exclusion lease ended: while live, streamer mode
/// keeps the selector excluded (and restores it when the stream stops).
#[cfg(target_os = "windows")]
pub fn adopt_selector_exclusion(hwnd: isize) -> bool {
    windows_affinity::adopt_exclusion(hwnd)
}

#[cfg(target_os = "macos")]
fn apply_capture_exclusion(app: &AppHandle, _excluded: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        apply_current_exclusion(&app).await;
    });
}

/// Bring every display share's filter in line with the CURRENT live state,
/// one application at a time: a stop's restore that finishes late can't undo
/// the next go-live's exclusion. The shares that failed are returned.
#[cfg(target_os = "macos")]
async fn apply_current_exclusion(app: &AppHandle) -> Vec<u32> {
    let state = tauri::Manager::state::<crate::session::SessionState>(app);
    reapply_capture_exclusion(state.inner()).await
}

/// Every writer of the display filters' streamer state goes through here
/// (going live, stopping, a share starting or restarting).
#[cfg(target_os = "macos")]
pub async fn reapply_capture_exclusion(state: &crate::session::SessionState) -> Vec<u32> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _serial = LOCK.lock().await;
    crate::session::apply_streamer_capture_exclusion(state, capture_exclusion_active()).await
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn apply_capture_exclusion(_app: &AppHandle, _excluded: bool) {}

/// Display affinity values (`WDA_*`), kept as plain numbers so the restore
/// rule is testable everywhere.
pub const WDA_NONE: u32 = 0x00;
pub const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

/// The windows streamer mode excluded, so stopping restores exactly those.
#[derive(Debug, Default)]
pub struct AffinityLedger {
    excluded: std::collections::BTreeSet<isize>,
}

impl AffinityLedger {
    /// Whether to exclude `hwnd`, given its affinity now. Only a window that
    /// is fully capturable (`WDA_NONE`) is ours to change: one that is
    /// already excluded (an overlay, a border) or monitor-only is left as it
    /// is, and never restored.
    pub fn should_exclude(&self, current_affinity: u32) -> bool {
        current_affinity == WDA_NONE
    }

    /// `hwnd` was excluded by us.
    pub fn record(&mut self, hwnd: isize) {
        self.excluded.insert(hwnd);
    }

    /// Everything to put back to `WDA_NONE`; the ledger is empty after.
    pub fn take_restore_set(&mut self) -> Vec<isize> {
        std::mem::take(&mut self.excluded).into_iter().collect()
    }
}

#[cfg(target_os = "windows")]
mod windows_affinity {
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use std::sync::atomic::{AtomicI64, Ordering};

    use tauri::AppHandle;
    use tokio_util::sync::CancellationToken;
    use windows::core::BOOL;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowDisplayAffinity, GetWindowThreadProcessId, IsWindow,
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    use super::AffinityLedger;
    use crate::sync_ext::MutexExt;

    const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
    /// Margin after the affinity calls before frames count as excluded: DWM
    /// applies `WDA_EXCLUDEFROMCAPTURE` at its next composition.
    const FENCE_MARGIN_100NS: i64 = 500_000; // 50 ms

    /// When the exclusion took effect (QPC, 100 ns units); `i64::MAX` = off.
    static FENCE: AtomicI64 = AtomicI64::new(i64::MAX);

    pub fn fence() -> i64 {
        FENCE.load(Ordering::Acquire)
    }

    /// Now, in the clock of `Direct3D11CaptureFrame::SystemRelativeTime`.
    fn qpc_now_100ns() -> Option<i64> {
        let (mut counter, mut frequency) = (0i64, 0i64);
        // SAFETY: plain queries with valid out-params.
        unsafe {
            QueryPerformanceCounter(&mut counter).ok()?;
            QueryPerformanceFrequency(&mut frequency).ok()?;
        }
        (frequency > 0).then(|| ((counter as i128 * 10_000_000) / frequency as i128) as i64)
    }

    struct Active {
        ledger: AffinityLedger,
        cancel: Option<CancellationToken>,
    }

    fn active() -> &'static Mutex<Active> {
        static ACTIVE: OnceLock<Mutex<Active>> = OnceLock::new();
        ACTIVE.get_or_init(|| Mutex::new(Active { ledger: AffinityLedger::default(), cancel: None }))
    }

    /// Turn the exclusion on or off. On: true once every Petal window is
    /// excluded and the fence is open; false leaves the fence shut (nothing
    /// streams) while the reconcile loop keeps trying.
    pub fn set_active(app: &AppHandle, on: bool) -> bool {
        let mut guard = active().lock_unpoisoned();
        if let Some(cancel) = guard.cancel.take() {
            cancel.cancel();
        }
        if !on {
            FENCE.store(i64::MAX, Ordering::Release);
            let restore = guard.ledger.take_restore_set();
            drop(guard);
            // A Petal View selector whose share is still running keeps the
            // exclusion its lease holds.
            let leased = crate::region_window::leased_selector_hwnds(app);
            let restored = restore
                .into_iter()
                .filter(|raw| !leased.contains(raw))
                .filter(|raw| {
                    let hwnd = HWND(*raw as *mut core::ffi::c_void);
                    // SAFETY: plain calls on a window handle we enumerated
                    // from this process; a closed one fails harmlessly.
                    unsafe { IsWindow(Some(hwnd)).as_bool() && SetWindowDisplayAffinity(hwnd, WDA_NONE).is_ok() }
                })
                .count();
            log::info!("streaming: streamer mode restored capture of {restored} Petal window(s)");
            return true;
        }
        let cancel = CancellationToken::new();
        guard.cancel = Some(cancel.clone());
        drop(guard);
        // Exclude every window now, then open the fence: frames composed
        // before this are never streamed. A window we couldn't exclude, or
        // an unknown clock, keeps it shut.
        let opened = reconcile();
        if !opened {
            log::warn!("streaming: a Petal window couldn't be excluded from capture; not streaming");
        }
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                }
                reconcile();
            }
        });
        opened
    }

    /// Move the fence to now: only frames composed from here on stream.
    /// Called with `active()` held, so fence decisions never interleave.
    fn refence() -> bool {
        match qpc_now_100ns() {
            Some(now) => {
                FENCE.store(now.saturating_add(FENCE_MARGIN_100NS), Ordering::Release);
                true
            }
            None => {
                FENCE.store(i64::MAX, Ordering::Release);
                false
            }
        }
    }

    /// Exclude `raw` if it is still capturable. False when it couldn't be.
    fn exclude(ledger: &mut AffinityLedger, raw: isize, newly: &mut usize) -> bool {
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        let mut affinity = 0u32;
        // SAFETY: plain query on one of this process's windows.
        if unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.is_err() {
            // Gone since the enumeration is fine; anything else is not.
            // SAFETY: as above.
            return !unsafe { IsWindow(Some(hwnd)) }.as_bool();
        }
        if !ledger.should_exclude(affinity) {
            return true;
        }
        // SAFETY: as above; only this process's windows are changed.
        if unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }.is_ok() {
            ledger.record(raw);
            *newly += 1;
            true
        } else {
            false
        }
    }

    /// A Petal window was just created (`on_window_ready`): while live,
    /// exclude it at once instead of waiting for the next reconcile, and move
    /// the fence past it (or shut it).
    pub fn on_window_created(hwnd: isize, label: &str) {
        let mut guard = active().lock_unpoisoned();
        if guard.cancel.is_none() {
            return;
        }
        let mut newly = 0;
        if exclude(&mut guard.ledger, hwnd, &mut newly) {
            refence();
        } else {
            log::warn!("streaming: couldn't exclude new Petal window {label} from capture; not streaming until it is");
            FENCE.store(i64::MAX, Ordering::Release);
        }
    }

    /// A Petal View selector's own exclusion lease ended while live: keep it
    /// excluded, and restore it with the rest when the stream stops. False
    /// when nothing is live (the lease restores it itself).
    pub fn adopt_exclusion(hwnd: isize) -> bool {
        let mut guard = active().lock_unpoisoned();
        if guard.cancel.is_none() {
            return false;
        }
        guard.ledger.record(hwnd);
        true
    }

    /// The input-method helper windows Windows gives every GUI thread: they
    /// never draw content and aren't Petal's to change.
    fn is_system_helper(hwnd: HWND) -> bool {
        let mut class = [0u16; 32];
        // SAFETY: a valid buffer for the class name.
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
        matches!(class.as_str(), "IME" | "MSCTFIME UI")
    }

    /// Petal's own top-level windows, visible or not.
    fn own_top_level_windows() -> Vec<isize> {
        unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: `lparam` is the `Vec` passed below, alive for the
            // whole enumeration.
            let found = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
            let mut pid = 0u32;
            // SAFETY: plain query with a valid out-param.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid == std::process::id() && !is_system_helper(hwnd) {
                found.push(hwnd.0 as isize);
            }
            BOOL(1)
        }
        let mut found: Vec<isize> = Vec::new();
        // SAFETY: the callback only touches `found` through `lparam`.
        let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut found as *mut Vec<isize> as isize)) };
        found
    }

    /// One pass over every Petal window, selectors included, with the fence
    /// decided under the same lock: shut if any window couldn't be excluded
    /// (hidden ones too: they can be shown before the next pass), moved past
    /// any window this pass excluded. True when the fence is open after it.
    fn reconcile() -> bool {
        let mut guard = active().lock_unpoisoned();
        if guard.cancel.is_none() {
            return false;
        }
        let mut all_excluded = true;
        let mut newly = 0;
        for raw in own_top_level_windows() {
            all_excluded &= exclude(&mut guard.ledger, raw, &mut newly);
        }
        if !all_excluded {
            FENCE.store(i64::MAX, Ordering::Release);
            return false;
        }
        if newly > 0 || fence() == i64::MAX {
            return refence();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_when_forced_or_live_and_live_only_when_streaming() {
        let mut inputs = Inputs::default();
        assert_eq!(inputs.state(), StreamerModeState { active: false, live: false });
        inputs.forced = true;
        assert_eq!(inputs.state(), StreamerModeState { active: true, live: false });
        inputs.live = true;
        assert_eq!(inputs.state(), StreamerModeState { active: true, live: true });
        inputs.forced = false;
        assert_eq!(inputs.state(), StreamerModeState { active: true, live: true });
        inputs.live = false;
        assert_eq!(inputs.state(), StreamerModeState { active: false, live: false });
    }

    #[test]
    fn only_capturable_windows_are_excluded_and_restored() {
        const WDA_MONITOR: u32 = 0x01;
        let mut ledger = AffinityLedger::default();
        // Three windows: a meeting window, an always-excluded border, a
        // monitor-only one.
        for (hwnd, affinity) in [(10, WDA_NONE), (20, WDA_EXCLUDEFROMCAPTURE), (30, WDA_MONITOR)] {
            if ledger.should_exclude(affinity) {
                ledger.record(hwnd);
            }
        }
        // The next reconcile sees ours as excluded now: no double entry.
        assert!(!ledger.should_exclude(WDA_EXCLUDEFROMCAPTURE));
        // A window opened mid-stream joins.
        if ledger.should_exclude(WDA_NONE) {
            ledger.record(40);
        }
        assert_eq!(ledger.take_restore_set(), vec![10, 40]);
        assert!(ledger.take_restore_set().is_empty());
    }

    #[test]
    fn only_frames_after_the_fence_leave_petal_out() {
        assert!(!frame_after_fence(i64::MAX, i64::MAX - 1), "no exclusion yet: nothing passes");
        assert!(!frame_after_fence(1_000, 999), "composed before the exclusion");
        assert!(frame_after_fence(1_000, 1_000));
        assert!(!frame_after_fence(1_000, i64::MIN), "a frame with no time can't prove it");
    }

    #[test]
    fn the_state_payload_shape() {
        assert_eq!(
            serde_json::to_value(StreamerModeState { active: true, live: false }).unwrap(),
            serde_json::json!({ "active": true, "live": false })
        );
    }
}
