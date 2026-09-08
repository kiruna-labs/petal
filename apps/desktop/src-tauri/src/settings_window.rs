//! Dedicated Settings window.
//!
//! Settings used to be a route inside the main window. Reaching it from a
//! live meeting meant navigating the main webview away from `/meeting/<room>`,
//! which runs that route's `onDestroy` (drops the self-view preview, restores
//! the home window geometry) while the user is still joined -- the same
//! hazard that made the menubar's "Open Petal" show-only (#782). A separate
//! window leaves the meeting route untouched no matter where Settings is
//! opened from: home, the menubar popover, or the in-meeting "More" menu.
//!
//! Same recipe as the Network Cockpit (`network_cockpit.rs`): singleton by
//! label, hidden build revealed on page load, deterministic position.
//!
//! The decision logic lives behind [`SettingsWindowOps`] rather than inline in
//! the command, because CLAUDE.md's "native window-lifecycle changes need a
//! live-exercising test" rule is not satisfied by unit tests on pure helpers:
//! `WebviewWindowBuilder::build` needs a live event loop and cannot run under
//! `cargo test --lib`, so the ORDERED sequence the command performs
//! (reuse-vs-build, show/unminimize/focus, reposition, the page-load reveal)
//! is driven in tests against a fake window registry instead.

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const SETTINGS_WINDOW_LABEL: &str = "settings";

const SETTINGS_WINDOW_WIDTH: f64 = 480.0;
const SETTINGS_WINDOW_HEIGHT: f64 = 720.0;

/// Which half of a page load a reveal decision is being made for. Mirrors the
/// only two `PageLoadEvent` variants this window cares about; a local enum so
/// the reveal rule is testable without a real webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageLoadPhase {
    Started,
    Finished,
}

impl From<PageLoadEvent> for PageLoadPhase {
    fn from(event: PageLoadEvent) -> Self {
        match event {
            PageLoadEvent::Finished => PageLoadPhase::Finished,
            _ => PageLoadPhase::Started,
        }
    }
}

/// The window is built hidden so Windows never paints a blank white HWND
/// before WebView2 attaches; it must therefore be revealed by the page-load
/// callback, and ONLY once the load finished. Revealing on `Started` puts the
/// white rectangle back.
pub(crate) fn reveal_on_page_load(phase: PageLoadPhase, show: impl FnOnce(), focus: impl FnOnce()) {
    if phase != PageLoadPhase::Finished {
        return;
    }
    show();
    focus();
}

/// Everything [`open_settings_window_with`] does to the window system. The
/// real implementation talks to Tauri; the test implementation records the
/// call order against a fake registry.
pub(crate) trait SettingsWindowOps {
    /// Is a Settings window already alive? Implementations latch the handle
    /// so the show/unminimize/focus calls below act on it.
    fn adopt_existing(&self) -> bool;
    fn show(&self);
    fn unminimize(&self);
    fn focus(&self);
    fn build(&self) -> Result<(), String>;
    fn centered_position(&self, width: f64, height: f64) -> Option<(f64, f64)>;
    fn set_position(&self, x: f64, y: f64);
}

/// The command's real body.
///
/// Reuse branch: an existing window is shown, unminimized and focused, in
/// that order -- `set_focus` on a minimized window is a no-op on both
/// platforms, so unminimize has to precede it -- and is NEVER rebuilt or
/// repositioned (a reposition would yank a window the user had placed).
///
/// Create branch: build (hidden; the page-load callback reveals it) and then
/// pin a deterministic position. The window is destroyed on close and rebuilt
/// on the next open, so an unpositioned rebuild drifts down-right on Windows
/// (CW_USEDEFAULT cascade).
pub(crate) fn open_settings_window_with<O: SettingsWindowOps>(ops: &O) -> Result<(), String> {
    if ops.adopt_existing() {
        ops.show();
        ops.unminimize();
        ops.focus();
        return Ok(());
    }
    ops.build()?;
    if let Some((x, y)) =
        ops.centered_position(SETTINGS_WINDOW_WIDTH, SETTINGS_WINDOW_HEIGHT)
    {
        ops.set_position(x, y);
    }
    Ok(())
}

struct TauriSettingsWindow<'a> {
    app: &'a AppHandle,
    window: std::cell::RefCell<Option<tauri::WebviewWindow>>,
}

impl SettingsWindowOps for TauriSettingsWindow<'_> {
    fn adopt_existing(&self) -> bool {
        match self.app.get_webview_window(SETTINGS_WINDOW_LABEL) {
            Some(window) => {
                *self.window.borrow_mut() = Some(window);
                true
            }
            None => false,
        }
    }

    fn show(&self) {
        if let Some(window) = self.window.borrow().as_ref() {
            let _ = window.show();
        }
    }

    fn unminimize(&self) {
        if let Some(window) = self.window.borrow().as_ref() {
            let _ = window.unminimize();
        }
    }

    fn focus(&self) {
        if let Some(window) = self.window.borrow().as_ref() {
            let _ = window.set_focus();
        }
    }

    fn build(&self) -> Result<(), String> {
        let settings_builder = WebviewWindowBuilder::new(
            self.app,
            SETTINGS_WINDOW_LABEL,
            WebviewUrl::App("settings.html".into()),
        )
        .title("Petal — Settings")
        .inner_size(SETTINGS_WINDOW_WIDTH, SETTINGS_WINDOW_HEIGHT)
        .min_inner_size(400.0, 520.0)
        .decorations(false)
        .transparent(true)
        // Build hidden and reveal only once the page has loaded: on Windows the
        // visible HWND paints blank white before WebView2 attaches (same
        // hidden-build pattern as the cockpit, picker, and main window).
        .visible(false)
        .on_page_load(|window, payload| {
            reveal_on_page_load(
                PageLoadPhase::from(payload.event()),
                || {
                    let _ = window.show();
                },
                || {
                    let _ = window.set_focus();
                },
            );
        });
        // Windows: force WebView2 GPU acceleration (unsupported on macOS/Linux).
        #[cfg(target_os = "windows")]
        let settings_builder =
            settings_builder.additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS);
        let window = settings_builder.build().map_err(|e| e.to_string())?;

        // Windows: opaque window with DWM-native corners (same as the main
        // window); macOS keeps its transparent + CSS-rounded shell.
        #[cfg(target_os = "windows")]
        crate::windows_corner::make_native_rounded(&window);

        *self.window.borrow_mut() = Some(window);
        Ok(())
    }

    fn centered_position(&self, width: f64, height: f64) -> Option<(f64, f64)> {
        crate::window_picker::centered_secondary_window_position(self.app, width, height)
    }

    fn set_position(&self, x: f64, y: f64) {
        if let Some(window) = self.window.borrow().as_ref() {
            let _ = window
                .set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        }
    }
}

// Must stay `async`: on Windows, `WebviewWindowBuilder::build()` deadlocks
// when called from a synchronous command (Tauri v2 runs sync commands on the
// main thread, and the WebView2 controller callback needs the message loop
// to pump). Same reason the cockpit and picker commands are async (wry#583).
#[tauri::command]
pub async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    open_settings_window_with(&TauriSettingsWindow {
        app: &app,
        window: std::cell::RefCell::new(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Step {
        Show,
        Unminimize,
        Focus,
        Build,
        Position(f64, f64),
    }

    /// A fake window registry. `exists` is the registry: `build` puts the
    /// window in it, `close` takes it out, exactly as Tauri's own
    /// `get_webview_window` sees a window that was destroyed on close.
    struct FakeWindows {
        exists: Cell<bool>,
        build_error: RefCell<Option<String>>,
        centered: Cell<Option<(f64, f64)>>,
        steps: RefCell<Vec<Step>>,
        centered_asks: RefCell<Vec<(f64, f64)>>,
    }

    impl FakeWindows {
        fn new() -> Self {
            Self {
                exists: Cell::new(false),
                build_error: RefCell::new(None),
                centered: Cell::new(Some((120.0, 80.0))),
                steps: RefCell::new(Vec::new()),
                centered_asks: RefCell::new(Vec::new()),
            }
        }

        fn close(&self) {
            self.exists.set(false);
        }

        fn take_steps(&self) -> Vec<Step> {
            self.steps.borrow_mut().drain(..).collect()
        }
    }

    impl SettingsWindowOps for FakeWindows {
        fn adopt_existing(&self) -> bool {
            self.exists.get()
        }
        fn show(&self) {
            self.steps.borrow_mut().push(Step::Show);
        }
        fn unminimize(&self) {
            self.steps.borrow_mut().push(Step::Unminimize);
        }
        fn focus(&self) {
            self.steps.borrow_mut().push(Step::Focus);
        }
        fn build(&self) -> Result<(), String> {
            if let Some(error) = self.build_error.borrow().clone() {
                return Err(error);
            }
            self.steps.borrow_mut().push(Step::Build);
            self.exists.set(true);
            Ok(())
        }
        fn centered_position(&self, width: f64, height: f64) -> Option<(f64, f64)> {
            self.centered_asks.borrow_mut().push((width, height));
            self.centered.get()
        }
        fn set_position(&self, x: f64, y: f64) {
            self.steps.borrow_mut().push(Step::Position(x, y));
        }
    }

    #[test]
    fn first_open_builds_hidden_and_pins_a_deterministic_position() {
        let windows = FakeWindows::new();

        open_settings_window_with(&windows).expect("first open");

        // No Show here: the window is built hidden and revealed by the
        // page-load callback. An eager show is the blank-white-flash bug.
        assert_eq!(
            windows.take_steps(),
            vec![Step::Build, Step::Position(120.0, 80.0)]
        );
        assert_eq!(
            windows.centered_asks.borrow().as_slice(),
            &[(SETTINGS_WINDOW_WIDTH, SETTINGS_WINDOW_HEIGHT)]
        );
    }

    #[test]
    fn reopening_reuses_the_singleton_instead_of_building_a_second_window() {
        let windows = FakeWindows::new();
        open_settings_window_with(&windows).expect("first open");
        windows.take_steps();

        open_settings_window_with(&windows).expect("second open");

        // Unminimize BEFORE focus (set_focus does nothing to a minimized
        // window), and no rebuild and no reposition -- repositioning would
        // yank a window the user had already placed.
        assert_eq!(
            windows.take_steps(),
            vec![Step::Show, Step::Unminimize, Step::Focus]
        );
        assert_eq!(windows.centered_asks.borrow().len(), 1);
    }

    #[test]
    fn close_then_reopen_builds_and_repositions_again() {
        let windows = FakeWindows::new();
        open_settings_window_with(&windows).expect("first open");
        windows.take_steps();
        windows.close();

        open_settings_window_with(&windows).expect("reopen after close");

        // The window is destroyed on close, so the reopen must rebuild AND
        // re-pin the position: an unpositioned rebuild cascades down-right.
        assert_eq!(
            windows.take_steps(),
            vec![Step::Build, Step::Position(120.0, 80.0)]
        );
    }

    #[test]
    fn a_monitor_without_a_center_still_opens_the_window() {
        let windows = FakeWindows::new();
        windows.centered.set(None);

        open_settings_window_with(&windows).expect("open without a monitor");

        assert_eq!(windows.take_steps(), vec![Step::Build]);
    }

    #[test]
    fn a_failed_build_reports_the_error_and_positions_nothing() {
        let windows = FakeWindows::new();
        *windows.build_error.borrow_mut() = Some("no event loop".to_string());

        let error = open_settings_window_with(&windows).expect_err("build must propagate");

        assert_eq!(error, "no event loop");
        assert!(windows.take_steps().is_empty());
        assert!(windows.centered_asks.borrow().is_empty());
    }

    #[test]
    fn the_hidden_window_is_revealed_only_when_the_page_load_finishes() {
        let revealed = RefCell::new(Vec::<&'static str>::new());

        reveal_on_page_load(
            PageLoadPhase::Started,
            || revealed.borrow_mut().push("show"),
            || revealed.borrow_mut().push("focus"),
        );
        assert!(
            revealed.borrow().is_empty(),
            "revealing on Started is the blank-white-flash bug the hidden build exists to avoid"
        );

        reveal_on_page_load(
            PageLoadPhase::Finished,
            || revealed.borrow_mut().push("show"),
            || revealed.borrow_mut().push("focus"),
        );
        assert_eq!(revealed.borrow().as_slice(), &["show", "focus"]);
    }

    #[test]
    fn page_load_events_map_to_the_reveal_phases() {
        assert_eq!(
            PageLoadPhase::from(PageLoadEvent::Finished),
            PageLoadPhase::Finished
        );
        assert_eq!(
            PageLoadPhase::from(PageLoadEvent::Started),
            PageLoadPhase::Started
        );
    }
}
