//! Dedicated Network Cockpit window.
//!
//! The cockpit used to be an in-Gallery overlay, which meant it disappeared
//! whenever the user navigated away from the meeting route. A separate window
//! keeps the diagnostics surface alive while the main app moves between
//! meeting, settings, and home.

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const NETWORK_COCKPIT_LABEL: &str = "network-cockpit";

// Must stay `async`: on Windows, `WebviewWindowBuilder::build()` deadlocks
// when called from a synchronous command (Tauri v2 runs sync commands on the
// main thread, and the WebView2 controller callback needs the message loop
// to pump) — the window opened blank white and the whole app froze. Same
// reason the window picker command is async (wry#583).
#[tauri::command]
pub async fn open_network_cockpit_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(NETWORK_COCKPIT_LABEL) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(());
    }

    let cockpit_builder = WebviewWindowBuilder::new(
        &app,
        NETWORK_COCKPIT_LABEL,
        WebviewUrl::App("network-cockpit.html".into()),
    )
    .title("Petal — Network")
    .inner_size(720.0, 680.0)
    .min_inner_size(560.0, 420.0)
    .decorations(false)
    .transparent(true)
    // Build hidden and reveal only once the page has loaded: on Windows the
    // visible HWND paints blank white before WebView2 attaches, so an eager
    // build flashes a white rectangle for a split second on every open (the
    // window picker and the main window use the same hidden-build pattern).
    .visible(false)
    .on_page_load(|window, payload| {
        if payload.event() == PageLoadEvent::Finished {
            let _ = window.show();
            let _ = window.set_focus();
        }
    });
    // Windows: force WebView2 GPU acceleration (unsupported on macOS/Linux).
    #[cfg(target_os = "windows")]
    let cockpit_builder =
        cockpit_builder.additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS);
    let window = cockpit_builder.build().map_err(|e| e.to_string())?;

    // Windows: opaque window with DWM-native corners (same as the main window);
    // macOS keeps its transparent + CSS-rounded panel.
    #[cfg(target_os = "windows")]
    crate::windows_corner::make_native_rounded(&window);

    // Same CW_USEDEFAULT cascade drift as the window picker: this window is
    // destroyed on close and rebuilt on open, so an unpositioned rebuild
    // creeps ~1 inch per open on Windows until the cascade wraps. Pin the
    // same deterministic position (centered on the main window's monitor).
    if let Some((x, y)) = crate::window_picker::centered_secondary_window_position(&app, 720.0, 680.0) {
        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
    }

    Ok(())
}
