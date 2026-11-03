//! Streamer mode: what Petal hides while someone could be watching.
//!
//! Active when this computer's own client-mode stream is running (from the
//! moment `start_client` succeeds until it stops), or when Settings -> Live
//! streaming -> "Always use streamer mode" is on. While active, the webviews
//! mask meeting names and codes (`streamer-mode-changed`).
//!
//! Capture exclusion follows only this computer's own stream, never the
//! forced setting (OBS and similar tools still capture Petal normally):
//!
//! * Windows: while the stream's source can show Petal (a display or a Petal
//!   View share, [`source_shows_petal`]), every Petal top-level window except
//!   the Petal View selectors (`region-window-*`, which keep their own lease)
//!   and popup menus gets `WDA_EXCLUDEFROMCAPTURE`, reconciled every second
//!   so windows opened mid-stream are covered. Only windows that were
//!   `WDA_NONE` are changed, so only those are restored; always-excluded
//!   overlays stay excluded. The exclusion is in place, and one DWM
//!   composition has passed, before the first frame from such a source is
//!   tapped: when going live and when switching to it.
//!
//!   `WDA_EXCLUDEFROMCAPTURE` hides Petal from every capture on the PC, a
//!   remote viewer (Moonlight, RustDesk, Parsec) included, so a streamer
//!   driving the PC remotely would lose every control. A window share can't
//!   show Petal's windows (the capture holds that window's own pixels), so
//!   while one is streaming Petal stays capturable and its Stop buttons stay
//!   reachable. For display streams the notification-area icon
//!   (`live_tray`) is drawn by Explorer and offers "Stop live stream".
//! * macOS: whole-display shares rebuild their ScreenCaptureKit filter
//!   without Petal ([`crate::session::apply_streamer_capture_exclusion`]);
//!   region shares already exclude it. That filter is Petal's own, so other
//!   apps' captures still see Petal; macOS has no source rule.
//!
//! Either way a public stream of a display never shows the meeting, its
//! chat, other participants' meeting-only shares, or the go-live dialog.

use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::sync_ext::MutexExt;
use crate::transport::publisher::SharedSourceKind;

pub const STREAMER_MODE_CHANGED_EVENT: &str = "streamer-mode-changed";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamerModeState {
    /// Mask meeting names and codes.
    pub active: bool,
    /// This computer is streaming (and so excludes Petal from captures).
    pub live: bool,
}

/// The inputs; the state and the exclusion follow from them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Inputs {
    forced: bool,
    live: bool,
    /// What the stream shows: `None` while not live, or where the platform
    /// has no source rule (macOS), which counts as one that shows Petal.
    source: Option<SharedSourceKind>,
}

impl Inputs {
    fn state(self) -> StreamerModeState {
        StreamerModeState { active: self.forced || self.live, live: self.live }
    }

    /// Whether Petal's windows are left out of captures.
    fn excludes_petal(self) -> bool {
        self.live && self.source.is_none_or(source_shows_petal)
    }
}

/// Whether a stream of this source can show Petal's own windows: a display
/// (or a Petal View region of one) can; a window share holds only that
/// window's pixels.
pub fn source_shows_petal(kind: SharedSourceKind) -> bool {
    match kind {
        SharedSourceKind::Window => false,
        SharedSourceKind::Display | SharedSourceKind::DisplayRegion => true,
    }
}

/// The exclusion's new value, when a change of inputs turns it on or off.
fn exclusion_change(before: Inputs, after: Inputs) -> Option<bool> {
    let excluded = after.excludes_petal();
    (excluded != before.excludes_petal()).then_some(excluded)
}

fn inputs() -> &'static Mutex<Inputs> {
    static INPUTS: OnceLock<Mutex<Inputs>> = OnceLock::new();
    INPUTS.get_or_init(|| Mutex::new(Inputs::default()))
}

/// Apply a change; the inputs before and after it.
fn change(update: impl FnOnce(&mut Inputs)) -> (Inputs, Inputs) {
    let mut guard = inputs().lock_unpoisoned();
    let before = *guard;
    update(&mut guard);
    (before, *guard)
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
    let (before, after) = change(|inputs| inputs.forced = forced);
    if after.state() != before.state() {
        let _ = app.emit(STREAMER_MODE_CHANGED_EVENT, after.state());
    }
}

/// This computer's stream started on `source` (`true`) or stopped.
pub fn set_live(app: &AppHandle, live: bool, source: Option<SharedSourceKind>) {
    let (before, after) = change(|inputs| {
        inputs.live = live;
        inputs.source = if live { source } else { None };
    });
    apply(app, before, after);
}

/// The live stream moved to `source` ("Switch/Resume live stream here").
/// Moving onto a source that shows Petal must come before the tap rebinds;
/// moving off one, after.
pub fn set_live_source(app: &AppHandle, source: Option<SharedSourceKind>) {
    let (before, after) = change(|inputs| {
        if inputs.live {
            inputs.source = source;
        }
    });
    apply(app, before, after);
}

/// The live stream's source right now.
pub fn live_source() -> Option<SharedSourceKind> {
    inputs().lock_unpoisoned().source
}

fn apply(app: &AppHandle, before: Inputs, after: Inputs) {
    if let Some(excluded) = exclusion_change(before, after) {
        log::info!("streaming: streamer mode excludes Petal from captures={excluded} (source {:?})", after.source);
        apply_capture_exclusion(app, excluded);
    }
    if after.state() != before.state() {
        let state = after.state();
        log::info!("streaming: streamer mode active={} live={}", state.active, state.live);
        let _ = app.emit(STREAMER_MODE_CHANGED_EVENT, state);
    }
}

/// Petal left out of captures ahead of going live, so the stream's first
/// frame already leaves it out (Windows, sources that show Petal). Dropped
/// without [`GoLiveExclusion::hand_over`] (going live failed), it puts the
/// windows back.
#[must_use]
pub struct GoLiveExclusion {
    excluded: Option<AppHandle>,
}

pub fn exclude_before_going_live(app: &AppHandle, source: Option<SharedSourceKind>) -> GoLiveExclusion {
    #[cfg(target_os = "windows")]
    if source.is_none_or(source_shows_petal) {
        log::info!("streaming: streamer mode excludes Petal ahead of going live (source {source:?})");
        apply_capture_exclusion(app, true);
        return GoLiveExclusion { excluded: Some(app.clone()) };
    }
    let _ = (app, source);
    GoLiveExclusion { excluded: None }
}

impl GoLiveExclusion {
    /// The stream is live: [`set_live`] owns the exclusion from here.
    pub fn hand_over(mut self) {
        self.excluded = None;
    }
}

impl Drop for GoLiveExclusion {
    fn drop(&mut self) {
        if let Some(app) = self.excluded.take() {
            apply_capture_exclusion(&app, false);
        }
    }
}

#[tauri::command]
pub fn streamer_mode_state() -> StreamerModeState {
    current()
}

#[cfg(target_os = "windows")]
fn apply_capture_exclusion(app: &AppHandle, excluded: bool) {
    if excluded {
        windows_affinity::exclude_now(app);
    } else {
        windows_affinity::set_active(app, false);
    }
}

#[cfg(target_os = "macos")]
fn apply_capture_exclusion(app: &AppHandle, excluded: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = tauri::Manager::state::<crate::session::SessionState>(&app);
        crate::session::apply_streamer_capture_exclusion(state.inner(), excluded).await;
    });
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

    use tauri::{AppHandle, Manager};
    use tokio_util::sync::CancellationToken;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowDisplayAffinity, GetWindowThreadProcessId, IsWindow,
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    use super::AffinityLedger;
    use crate::sync_ext::MutexExt;

    const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

    struct Active {
        ledger: AffinityLedger,
        cancel: Option<CancellationToken>,
    }

    fn active() -> &'static Mutex<Active> {
        static ACTIVE: OnceLock<Mutex<Active>> = OnceLock::new();
        ACTIVE.get_or_init(|| Mutex::new(Active { ledger: AffinityLedger::default(), cancel: None }))
    }

    /// Exclude now, not at the next reconcile tick, and wait for one DWM
    /// composition so the next captured frame already leaves Petal out.
    pub fn exclude_now(app: &AppHandle) {
        set_active(app, true);
        reconcile(app);
        // SAFETY: plain call; it only waits for the next composition (and
        // fails harmlessly when there is none).
        let _ = unsafe { DwmFlush() };
    }

    pub fn set_active(app: &AppHandle, on: bool) {
        let mut guard = active().lock_unpoisoned();
        if let Some(cancel) = guard.cancel.take() {
            cancel.cancel();
        }
        if !on {
            let restore = guard.ledger.take_restore_set();
            drop(guard);
            let restored = restore
                .into_iter()
                .filter(|raw| {
                    let hwnd = HWND(*raw as *mut core::ffi::c_void);
                    // SAFETY: plain calls on a window handle we enumerated
                    // from this process; a closed one fails harmlessly.
                    unsafe { IsWindow(Some(hwnd)).as_bool() && SetWindowDisplayAffinity(hwnd, WDA_NONE).is_ok() }
                })
                .count();
            log::info!("streaming: streamer mode restored capture of {restored} Petal window(s)");
            return;
        }
        let cancel = CancellationToken::new();
        guard.cancel = Some(cancel.clone());
        drop(guard);
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                reconcile(&app);
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                }
            }
        });
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
            if pid == std::process::id() {
                found.push(hwnd.0 as isize);
            }
            BOOL(1)
        }
        let mut found: Vec<isize> = Vec::new();
        // SAFETY: the callback only touches `found` through `lparam`.
        let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut found as *mut Vec<isize> as isize)) };
        found
    }

    /// The Petal View selectors: their exclusion is a lease of their own
    /// (`region_window::acquire_selector_capture_exclusion`).
    fn region_selector_windows(app: &AppHandle) -> Vec<isize> {
        app.webview_windows()
            .into_iter()
            .filter(|(label, _)| label.starts_with(crate::region_window::REGION_WINDOW_LABEL_PREFIX))
            .filter_map(|(_, window)| window.hwnd().ok().map(|hwnd| hwnd.0 as isize))
            .collect()
    }

    fn reconcile(app: &AppHandle) {
        let skip = region_selector_windows(app);
        let mut guard = active().lock_unpoisoned();
        if guard.cancel.is_none() {
            return;
        }
        for raw in own_top_level_windows() {
            if skip.contains(&raw) {
                continue;
            }
            let hwnd = HWND(raw as *mut core::ffi::c_void);
            let mut affinity = 0u32;
            // SAFETY: plain query on one of this process's windows.
            if unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.is_err() {
                continue;
            }
            if !guard.ledger.should_exclude(affinity) {
                continue;
            }
            // SAFETY: as above; only this process's windows are changed.
            if unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }.is_ok() {
                guard.ledger.record(raw);
            }
        }
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
    fn only_display_sources_show_petal() {
        assert!(!source_shows_petal(SharedSourceKind::Window));
        assert!(source_shows_petal(SharedSourceKind::Display));
        assert!(source_shows_petal(SharedSourceKind::DisplayRegion));
    }

    #[test]
    fn the_exclusion_follows_the_live_source() {
        let idle = Inputs::default();
        let live_on = |source| Inputs { live: true, source: Some(source), ..idle };
        let window = live_on(SharedSourceKind::Window);
        let display = live_on(SharedSourceKind::Display);
        let region = live_on(SharedSourceKind::DisplayRegion);

        // Going live.
        assert_eq!(exclusion_change(idle, window), None);
        assert_eq!(exclusion_change(idle, display), Some(true));
        assert_eq!(exclusion_change(idle, region), Some(true));
        // Switching while live.
        assert_eq!(exclusion_change(window, display), Some(true));
        assert_eq!(exclusion_change(display, window), Some(false));
        assert_eq!(exclusion_change(display, region), None);
        // The live share ended (the slate): the source stays as it was.
        assert_eq!(exclusion_change(display, display), None);
        assert_eq!(exclusion_change(window, window), None);
        // Stopping.
        assert_eq!(exclusion_change(display, idle), Some(false));
        assert_eq!(exclusion_change(window, idle), None);
        // "Always use streamer mode" alone masks but never excludes.
        let forced = Inputs { forced: true, ..idle };
        assert_eq!(exclusion_change(idle, forced), None);
        assert_eq!(exclusion_change(forced, Inputs { forced: true, ..window }), None);
    }

    #[test]
    fn without_a_source_rule_the_exclusion_is_live() {
        // macOS passes no source: its exclusion stays "live", as
        // `capture_exclusion_active` reports.
        let idle = Inputs::default();
        let live = Inputs { live: true, ..idle };
        assert!(live.excludes_petal() && live.state().live);
        assert!(!idle.excludes_petal() && !idle.state().live);
        assert_eq!(exclusion_change(idle, live), Some(true));
        assert_eq!(exclusion_change(live, idle), Some(false));
        assert!(!Inputs { forced: true, ..idle }.excludes_petal());
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
    fn the_state_payload_shape() {
        assert_eq!(
            serde_json::to_value(StreamerModeState { active: true, live: false }).unwrap(),
            serde_json::json!({ "active": true, "live": false })
        );
    }
}
