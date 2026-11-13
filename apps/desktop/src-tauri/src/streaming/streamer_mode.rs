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
//!   `WDA_EXCLUDEFROMCAPTURE`. A window opened mid-stream is built excluded
//!   before it is first shown ([`BuildPetalWindow::build_petal`]); a
//!   once-a-second reconcile is the backstop. Only windows that were
//!   `WDA_NONE` are changed, so only those are restored; always-excluded
//!   overlays stay excluded.
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

/// How every Petal webview window is built (enforced by
/// `tests::every_window_builder_goes_through_build_petal`).
///
/// Windows, while this computer is live: the window is built with
/// `content_protected`, which tao applies in `WM_CREATE`, before the window
/// is first shown, so a display or region stream never composes it. A window
/// that started building just before going live is excluded once built, and
/// the stream's fence stays shut while any window is being built, until it is
/// checked (#487). Elsewhere it is a plain `build()`.
pub trait BuildPetalWindow<R: tauri::Runtime> {
    fn build_petal(self) -> tauri::Result<tauri::WebviewWindow<R>>;

    /// A window that is excluded from capture for its whole life, live or
    /// not (the share overlays): excluded before it is first shown, and never
    /// handed back to capture when a stream stops.
    #[cfg(target_os = "windows")]
    fn build_petal_excluded(self) -> tauri::Result<tauri::WebviewWindow<R>>;
}

impl<R: tauri::Runtime, M: tauri::Manager<R>> BuildPetalWindow<R> for tauri::WebviewWindowBuilder<'_, R, M> {
    #[cfg(target_os = "windows")]
    fn build_petal(self) -> tauri::Result<tauri::WebviewWindow<R>> {
        let ticket = windows_affinity::begin_window_build();
        let built = self.content_protected(ticket.mode() == BuildMode::Protected).build();
        let hwnd = built.as_ref().ok().and_then(|window| window.hwnd().ok()).map(|hwnd| hwnd.0 as isize);
        windows_affinity::finish_window_build(ticket, hwnd);
        built
    }

    #[cfg(not(target_os = "windows"))]
    fn build_petal(self) -> tauri::Result<tauri::WebviewWindow<R>> {
        self.build()
    }

    #[cfg(target_os = "windows")]
    fn build_petal_excluded(self) -> tauri::Result<tauri::WebviewWindow<R>> {
        self.content_protected(true).build()
    }
}

/// How a window is being built relative to the live exclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    /// Live: built excluded, so never shown capturable (checked after).
    Protected,
    /// Not live when it started: it may be shown capturable, so a stream that
    /// starts meanwhile keeps its fence shut until it is excluded.
    Unprotected,
}

/// What to do once a window is built, under the same lock as the fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterBuild {
    /// Excluded as intended: remember it so stopping restores it.
    Record,
    /// Live but not excluded: exclude it now (and move the fence past it).
    ExcludeNow,
    /// Built excluded for a stream that has stopped since: hand it back.
    Restore,
    Nothing,
}

/// Window builds in flight, serialized with the fence (Windows; pure so the
/// ordering rules are testable everywhere). While any build is in flight the
/// fence stays shut: until a window is built and checked, nothing proves it
/// isn't on screen capturable (tao ignores a failed affinity call).
#[derive(Debug, Default)]
pub struct CreationGate {
    in_flight: usize,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl CreationGate {
    pub fn begin(&mut self, live: bool) -> BuildMode {
        self.in_flight += 1;
        if live {
            BuildMode::Protected
        } else {
            BuildMode::Unprotected
        }
    }

    pub fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// The fence may open only while no window is being built.
    pub fn fence_may_open(&self) -> bool {
        self.in_flight == 0
    }

    /// The step after a build in `mode`, given whether we're live now and the
    /// window's affinity now (`None`: no window was built).
    pub fn after_build(mode: BuildMode, live_now: bool, affinity: Option<u32>) -> AfterBuild {
        let Some(affinity) = affinity else { return AfterBuild::Nothing };
        match (mode, live_now) {
            (BuildMode::Protected, true) if affinity == WDA_EXCLUDEFROMCAPTURE => AfterBuild::Record,
            (_, true) => AfterBuild::ExcludeNow,
            (BuildMode::Protected, false) if affinity == WDA_EXCLUDEFROMCAPTURE => AfterBuild::Restore,
            _ => AfterBuild::Nothing,
        }
    }
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

    use super::{AffinityLedger, AfterBuild, BuildMode, CreationGate};
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
        gate: CreationGate,
        /// A pass excluded a window while builds held the fence shut: it must
        /// reopen at "now", not where it was.
        refence_pending: bool,
        cancel: Option<CancellationToken>,
    }

    fn active() -> &'static Mutex<Active> {
        static ACTIVE: OnceLock<Mutex<Active>> = OnceLock::new();
        ACTIVE.get_or_init(|| Mutex::new(Active { ledger: AffinityLedger::default(), gate: CreationGate::default(), refence_pending: false, cancel: None }))
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
            // Restored windows are capturable: the next stream's fence opens
            // at "now", never at a build's saved fence.
            guard.refence_pending = true;
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

    /// A window build is starting; held until [`finish_window_build`].
    pub struct BuildTicket {
        mode: BuildMode,
        /// The fence before a live build shut it, to reopen at when the
        /// window checks out excluded (`i64::MAX`: none).
        fence_before: i64,
        finished: bool,
    }

    impl BuildTicket {
        pub fn mode(&self) -> BuildMode {
            self.mode
        }
    }

    impl Drop for BuildTicket {
        // A build that unwinds still leaves the gate.
        fn drop(&mut self) {
            if !self.finished {
                active().lock_unpoisoned().gate.finish();
            }
        }
    }

    /// Decide, under the fence's lock, whether the window about to be built
    /// is built excluded (live); while live, shut the fence until it's checked.
    pub fn begin_window_build() -> BuildTicket {
        let mut guard = active().lock_unpoisoned();
        let live = guard.cancel.is_some();
        let fence_before = if live { FENCE.swap(i64::MAX, Ordering::AcqRel) } else { i64::MAX };
        BuildTicket { mode: guard.gate.begin(live), fence_before, finished: false }
    }

    /// The window from `ticket` is built (`hwnd`, or `None` if it failed):
    /// record, exclude or restore it, then settle the fence.
    pub fn finish_window_build(mut ticket: BuildTicket, hwnd: Option<isize>) {
        ticket.finished = true;
        let mut guard = active().lock_unpoisoned();
        guard.gate.finish();
        let live = guard.cancel.is_some();
        let affinity = hwnd.and_then(|raw| {
            let mut affinity = 0u32;
            // SAFETY: plain query on a window this process just created.
            unsafe { GetWindowDisplayAffinity(HWND(raw as *mut core::ffi::c_void), &mut affinity) }
                .is_ok()
                .then_some(affinity)
        });
        let raw = hwnd.unwrap_or_default();
        let after = CreationGate::after_build(ticket.mode, live, affinity);
        match after {
            AfterBuild::Record => guard.ledger.record(raw),
            AfterBuild::Restore => {
                // SAFETY: as above.
                let _ = unsafe { SetWindowDisplayAffinity(HWND(raw as *mut core::ffi::c_void), WDA_NONE) };
            }
            AfterBuild::ExcludeNow | AfterBuild::Nothing => {}
        }
        // While live, one full pass excludes this window (and anything
        // else) and decides the fence: moved past a window that may have been
        // on screen capturable, back where it was for one that was built
        // excluded, and never reopened over another window's failure.
        if live {
            let reopen = match after {
                AfterBuild::ExcludeNow => Reopen::Now,
                AfterBuild::Record if ticket.fence_before != i64::MAX => Reopen::At(ticket.fence_before),
                _ => Reopen::Now,
            };
            if !reconcile_locked(&mut guard, reopen, after == AfterBuild::ExcludeNow) {
                log::warn!("streaming: couldn't exclude a new Petal window from capture; not streaming until it is");
            }
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
        reconcile_locked(&mut guard, Reopen::Now, false)
    }

    /// Where a shut fence reopens once every window checks out.
    #[derive(Clone, Copy)]
    enum Reopen {
        /// Only frames from now on stream.
        Now,
        /// Where it was: nothing capturable appeared meanwhile.
        At(i64),
    }

    /// [`reconcile`] with `active()` held. `moved` forces the fence forward
    /// (a window may just have been on screen capturable).
    fn reconcile_locked(guard: &mut Active, reopen: Reopen, moved: bool) -> bool {
        let mut all_excluded = true;
        let mut newly = 0;
        for raw in own_top_level_windows() {
            all_excluded &= exclude(&mut guard.ledger, raw, &mut newly);
        }
        if !all_excluded {
            // Something may be on screen capturable: whenever the fence
            // reopens, it reopens at "now".
            guard.refence_pending = true;
            FENCE.store(i64::MAX, Ordering::Release);
            return false;
        }
        // A window being built may be shown capturable after this
        // enumeration: shut (not failed) until it's built and checked
        // (`finish_window_build` reopens).
        if !guard.gate.fence_may_open() {
            guard.refence_pending |= moved || newly > 0;
            FENCE.store(i64::MAX, Ordering::Release);
            return true;
        }
        if std::mem::take(&mut guard.refence_pending) || moved || newly > 0 {
            return refence();
        }
        if fence() == i64::MAX {
            return match reopen {
                Reopen::Now => refence(),
                Reopen::At(at) => {
                    FENCE.store(at, Ordering::Release);
                    true
                }
            };
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

    #[test]
    fn a_window_built_while_live_is_excluded_before_it_is_shown() {
        let mut gate = CreationGate::default();
        let mode = gate.begin(true);
        assert_eq!(mode, BuildMode::Protected, "live: built content-protected");
        assert!(!gate.fence_may_open(), "shut until the window is checked");
        gate.finish();
        assert!(gate.fence_may_open());
        assert_eq!(CreationGate::after_build(mode, true, Some(WDA_EXCLUDEFROMCAPTURE)), AfterBuild::Record);
        // Windows refused the affinity tao asked for: exclude it ourselves.
        assert_eq!(CreationGate::after_build(mode, true, Some(WDA_NONE)), AfterBuild::ExcludeNow);
        // The stream stopped while it was being built: hand it back.
        assert_eq!(CreationGate::after_build(mode, false, Some(WDA_EXCLUDEFROMCAPTURE)), AfterBuild::Restore);
        assert_eq!(CreationGate::after_build(mode, true, None), AfterBuild::Nothing, "the build failed");
    }

    #[test]
    fn going_live_during_a_capturable_build_keeps_the_fence_shut_until_it_is_excluded() {
        let mut gate = CreationGate::default();
        let first = gate.begin(false);
        let second = gate.begin(false);
        assert_eq!(first, BuildMode::Unprotected);
        // Going live now: the enumeration may miss these windows, so the
        // fence must not open yet.
        assert!(!gate.fence_may_open());
        gate.finish();
        assert_eq!(CreationGate::after_build(first, true, Some(WDA_NONE)), AfterBuild::ExcludeNow);
        assert!(!gate.fence_may_open(), "one build is still in flight");
        gate.finish();
        assert!(gate.fence_may_open());
        // Not live when it finished: nothing to do, nothing recorded.
        assert_eq!(CreationGate::after_build(second, false, Some(WDA_NONE)), AfterBuild::Nothing);
        // Finishing never underflows.
        gate.finish();
        assert!(gate.fence_may_open());
    }

    /// The source with comments removed (line-aware, like the repo's other
    /// source guards).
    fn strip_comments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut in_block = false;
        for line in source.lines() {
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if in_block {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        in_block = false;
                        i += 1;
                    }
                } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    in_block = true;
                    i += 1;
                } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    break;
                } else {
                    out.push(bytes[i] as char);
                }
                i += 1;
            }
            out.push('\n');
        }
        out
    }

    /// Each problem in one file's source: a window builder whose next build
    /// call isn't `build_petal`/`build_petal_excluded`, a renamed builder, or
    /// a non-webview window builder. Whitespace is ignored, so `:: new` or
    /// `.build ()` can't slip past; an unrelated `.build()` between a builder
    /// and its `build_petal` fails safe (rewrite it).
    fn window_builder_violations(source: &str) -> Vec<String> {
        let code: String = strip_comments(source).chars().filter(|c| !c.is_whitespace()).collect();
        let snippet = |at: usize| code[at..].chars().take(60).collect::<String>();
        let mut problems = Vec::new();
        let builder = ["Window", "Builder"].concat();
        for (at, _) in code.match_indices(&[builder.as_str(), "as"].concat()) {
            problems.push(format!("renamed window builder: {}", snippet(at)));
        }
        let bare_builder = [builder.as_str(), "::"].concat();
        let bare_window = ["Window", "::builder("].concat();
        for pattern in [&bare_builder, &bare_window] {
            for (at, _) in code.match_indices(pattern.as_str()) {
                if !code[..at].ends_with("Webview") {
                    problems.push(format!("non-webview window builder: {}", snippet(at)));
                }
            }
        }
        // A builder type may only be named to call a constructor (no stored
        // constructors, no type aliases).
        let webview_builder = ["Webview", "Window", "Builder"].concat();
        for (at, _) in code.match_indices(&[webview_builder.as_str(), "::"].concat()) {
            let rest = &code[at + webview_builder.len() + 2..];
            if !(rest.starts_with("new(") || rest.starts_with("from_config(")) {
                problems.push(format!("window builder used as a value: {}", snippet(at)));
            }
        }
        let webview_window_builder = ["WebviewWindow", "::builder"].concat();
        for (at, _) in code.match_indices(&webview_window_builder) {
            if !code[at + webview_window_builder.len()..].starts_with('(') {
                problems.push(format!("window builder used as a value: {}", snippet(at)));
            }
        }
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        for statement in code.split([';', '{', '}']) {
            let aliases = statement.match_indices("type").any(|(at, _)| {
                let before = &statement[..at];
                let before = before.strip_suffix("pub").unwrap_or(before);
                !before.ends_with(ident) && statement[at + 4..].starts_with(ident)
            });
            if aliases && statement.contains(&builder) {
                problems.push(format!("window builder type alias: {statement}"));
            }
        }
        let ctors = [
            ["WebviewWindow", "Builder::new("].concat(),
            ["WebviewWindow", "Builder::from_config("].concat(),
            ["WebviewWindow", "::builder("].concat(),
        ];
        for ctor in &ctors {
            for (at, _) in code.match_indices(ctor.as_str()) {
                // The next build call after the constructor builds this window.
                let rest = &code[at..];
                let next_build = rest.match_indices(".build").find_map(|(i, _)| {
                    let tail = &rest[i + ".build".len()..];
                    if tail.starts_with('(') {
                        Some(false)
                    } else if tail.starts_with("_petal(") || tail.starts_with("_petal_excluded(") {
                        Some(true)
                    } else {
                        None
                    }
                });
                if next_build != Some(true) {
                    problems.push(format!("built without build_petal: {}", snippet(at)));
                }
            }
        }
        problems
    }

    /// Plain `.build(` calls in one file (`rel`, under `src/`), whitespace
    /// ignored. In this file, `build_petal`'s own builds don't count, matched
    /// by content, and neither do its tests (fixtures, which build nothing).
    fn plain_builds(rel: &str, source: &str) -> usize {
        let mut code: String = strip_comments(source).chars().filter(|c| !c.is_whitespace()).collect();
        if rel == "streaming/streamer_mode.rs" {
            let tests = ["#[cfg(test)]", "mod", "tests{"].concat();
            code.truncate(code.find(&tests).expect("this file's tests"));
            for build in [
                "self.content_protected(ticket.mode()==BuildMode::Protected).build()",
                "self.content_protected(true).build()",
                "self.build()",
            ] {
                let at = code.find(build).expect("build_petal's own build");
                code.replace_range(at..at + build.len(), "");
            }
        }
        code.matches(".build(").count()
    }

    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn the_window_builder_guard_catches_a_bypass() {
        let ctor = ["WebviewWindow", "Builder::new"].concat();
        let ok = format!("let w = {ctor}(app, \"l\", url)\n    .visible(false)\n    .build_petal()?;");
        assert!(window_builder_violations(&ok).is_empty());
        let excluded = format!("let w = {ctor}(app, \"l\", url).build_petal_excluded()?;");
        assert!(window_builder_violations(&excluded).is_empty());
        let bad = [
            format!("let w = {ctor}(app, \"l\", url)\n    .build ()?;"),
            format!("let b = {ctor}(app, \"l\", url);\nlet b = b.title(\"x\");\nmatch b.build() {{}}"),
            format!("let b = {ctor}(app, \"l\", url);"),
            format!("let w = {ctor}(app, \"l\", url).build_petal_unchecked()?;"),
            ["let w = WebviewWindow", "Builder :: new (app, \"l\", url).build()?;"].concat(),
            ["let w = WebviewWindow", "Builder::from_config(app, &cfg)?.build()?;"].concat(),
            ["let w = WebviewWindow", "::builder(app, \"l\", url).build()?;"].concat(),
            ["tauri::window::Window", "Builder::new(app, \"l\").build()?;"].concat(),
            ["tauri::window::Window", "Builder::from_config(app, &cfg)?.build()?;"].concat(),
            ["let w = tauri::Window", "::builder(app, \"l\").build()?;"].concat(),
            ["use tauri::{WebviewWindow", "Builder as B};"].concat(),
            ["let ctor = WebviewWindow", "Builder::new;\nlet w = ctor(app, \"l\", url).build()?;"].concat(),
            ["let ctor = WebviewWindow", "::builder;\nlet w = ctor(app, \"l\", url).build()?;"].concat(),
            ["pub type B<'a> = tauri::WebviewWindow", "Builder<'a, Wry, AppHandle>;\nlet w = B::new(app, \"l\", url).build()?;"].concat(),
            ["#[allow(dead_code)]\npub(super) type B<'a> = tauri::WebviewWindow", "Builder<'a, Wry, AppHandle>;"].concat(),
        ];
        for source in &bad {
            assert!(!window_builder_violations(source).is_empty(), "not caught: {source}");
        }
        let commented = format!("// {ctor}(app).build()");
        assert!(window_builder_violations(&commented).is_empty());

        // A builder kept aside past another window's build_petal passes the
        // constructor check; its plain build is counted, here too.
        let rel = "streaming/streamer_mode.rs";
        let source = include_str!("streamer_mode.rs");
        assert_eq!(plain_builds(rel, source), 0);
        let held = format!(
            "fn held() {{\n    let held = {ctor}(app, \"a\", url);\n    let w = {ctor}(app, \"b\", url).build_petal()?;\n    held.build()?;\n}}\n"
        );
        assert!(window_builder_violations(&held).is_empty(), "the constructor check alone misses it");
        let tests = ["#[cfg(test)]\n", "mod tests"].concat();
        let bypassed = source.replacen(&tests, &[held.as_str(), &tests].concat(), 1);
        assert_eq!(plain_builds(rel, &bypassed), 1);
        let again = source.replacen(&tests, &["fn again(self) { self.build(); }\n", &tests].concat(), 1);
        assert_eq!(plain_builds(rel, &again), 1, "each allowed build is excused once");
    }

    /// #487: every Petal window is built through `build_petal`, so on Windows
    /// one opened while live is excluded before it is first shown. Raw Win32
    /// windows are listed here with why they can't appear in a capture.
    #[test]
    fn every_window_builder_goes_through_build_petal() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        assert!(files.len() > 50, "found the sources");
        // file -> raw CreateWindowEx calls: message-only (HWND_MESSAGE),
        // WS_CHILD of a Petal window (covered by its parent's affinity), or
        // test-only fixtures.
        let raw_windows: &[(&str, usize)] = &[
            ("windows_share_overlay.rs", 4), // HWND_MESSAGE tracker + 3 test fixtures
            ("windows_compositor.rs", 2),    // HWND_MESSAGE pump + WS_CHILD video child
            ("window_change_watcher.rs", 2), // HWND_MESSAGE pump + 1 test fixture
            ("region_lock.rs", 1),           // test fixture
        ];
        // Plain `.build()` calls in files that build windows: each listed one
        // builds something else. A builder kept aside and built plainly
        // later, past another window's `build_petal`, shows up here.
        let other_builds: &[(&str, usize)] = &[
            ("window_picker.rs", 1),   // SCContentFilter
            ("compositor.rs", 2),      // tauri_nspanel PanelBuilder (macOS)
            ("settings_window.rs", 1), // SettingsWindowOps::build -> build_petal
        ];
        let raw_call = ["CreateWindow", "Ex"].concat();
        let mut problems = Vec::new();
        let mut builders = 0;
        for file in &files {
            let source = std::fs::read_to_string(file).expect("read source");
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            let rel = file.strip_prefix(&src).unwrap().display().to_string().replace('\\', "/");
            builders += strip_comments(&source).matches(&["Webview", "Window", "Builder::new("].concat()).count();
            for problem in window_builder_violations(&source) {
                problems.push(format!("{rel}: {problem}"));
            }
            let code = strip_comments(&source);
            let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let builds_windows = squeezed.contains(&["Webview", "Window", "Builder"].concat())
                || squeezed.contains(&["WebviewWindow", "::builder"].concat());
            if builds_windows {
                let plain = plain_builds(&rel, &source);
                let allowed = other_builds.iter().find(|(f, _)| *f == name).map_or(0, |(_, n)| *n);
                if plain != allowed {
                    problems.push(format!(
                        "{rel}: {plain} plain .build() call(s), {allowed} allowed; windows are built with build_petal"
                    ));
                }
            }
            let raw = code
                .match_indices(&raw_call)
                .filter(|(at, _)| {
                    let tail = code[at + raw_call.len()..].trim_start_matches(['A', 'W']).trim_start();
                    tail.starts_with('(')
                })
                .count();
            let allowed = raw_windows.iter().find(|(f, _)| *f == name).map_or(0, |(_, n)| *n);
            if raw != allowed {
                problems.push(format!(
                    "{rel}: {raw} raw {raw_call} call(s), {allowed} allowed; a new raw window must be message-only or a child, or be built through build_petal"
                ));
            }
        }
        assert!(builders >= 14, "the guard sees the webview window builders ({builders})");
        assert!(problems.is_empty(), "Petal windows must be built with build_petal (#487):\n{}", problems.join("\n"));
    }
}
