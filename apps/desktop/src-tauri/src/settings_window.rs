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

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const SETTINGS_WINDOW_LABEL: &str = "settings";

const SETTINGS_WINDOW_WIDTH: f64 = 480.0;
const SETTINGS_WINDOW_HEIGHT: f64 = 720.0;

// Must stay `async`: on Windows, `WebviewWindowBuilder::build()` deadlocks
// when called from a synchronous command (Tauri v2 runs sync commands on the
// main thread, and the WebView2 controller callback needs the message loop
// to pump). Same reason the cockpit and picker commands are async (wry#583).
#[tauri::command]
pub async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(SETTINGS_WINDOW_LABEL) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(());
    }

    let settings_builder = WebviewWindowBuilder::new(
        &app,
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
        if payload.event() == PageLoadEvent::Finished {
            let _ = window.show();
            let _ = window.set_focus();
        }
    });
    // Windows: force WebView2 GPU acceleration (unsupported on macOS/Linux).
    #[cfg(target_os = "windows")]
    let settings_builder =
        settings_builder.additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS);
    let window = settings_builder.build().map_err(|e| e.to_string())?;

    // Windows: opaque window with DWM-native corners (same as the main window);
    // macOS keeps its transparent + CSS-rounded shell.
    #[cfg(target_os = "windows")]
    crate::windows_corner::make_native_rounded(&window);

    // The window is destroyed on close and rebuilt on open, so an
    // unpositioned rebuild drifts down-right on Windows (CW_USEDEFAULT
    // cascade). Pin it centered on the main window's monitor.
    if let Some((x, y)) = crate::window_picker::centered_secondary_window_position(
        &app,
        SETTINGS_WINDOW_WIDTH,
        SETTINGS_WINDOW_HEIGHT,
    ) {
        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
    }

    Ok(())
}
