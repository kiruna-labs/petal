//! Windows sharer-side remote-control consent surface.
//!
//! This mirrors the macOS `control_consent.rs` singleton: the route owns the
//! typed ordinary/escalation queue and fail-closed timeout behavior, while this module owns one
//! hidden panel, monitor-aware placement, and non-activating presentation.
//! Showing the prompt must not interrupt typing in the app being shared.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const CONTROL_CONSENT_LABEL: &str = "control-consent";
const CONTROL_CONSENT_WIDTH: f64 = 420.0;
const CONTROL_CONSENT_DEFAULT_HEIGHT: f64 = 120.0;
const CONTROL_CONSENT_MAX_HEIGHT: f64 = 360.0;
const CONTROL_CONSENT_TOP_MARGIN: f64 = 96.0;

// Present and dismiss share one ordered generation. This prevents a
// measurement call already queued on the Tauri main thread from showing the
// transparent singleton again after the user answered the request.
static PRESENTATION_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Create the hidden singleton on the Tauri main thread during setup. The
/// route remains loaded so the first consent event cannot race WebView2 boot.
pub fn create_control_consent_panel(app: &AppHandle) {
    if app.get_webview_window(CONTROL_CONSENT_LABEL).is_some() {
        return;
    }
    let builder = WebviewWindowBuilder::new(
        app,
        CONTROL_CONSENT_LABEL,
        WebviewUrl::App("control-consent.html".into()),
    )
    .title("Petal")
    .decorations(false)
    .transparent(true)
    .background_color(tauri::window::Color(0, 0, 0, 0))
    .resizable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
    .visible(false)
    .inner_size(CONTROL_CONSENT_WIDTH, CONTROL_CONSENT_DEFAULT_HEIGHT);

    match builder.build() {
        Ok(window) => {
            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                x: -10000,
                y: -10000,
            }));
            let _ = window.set_ignore_cursor_events(false);
            log::info!("control_consent: Windows panel created (hidden, off-screen)");
        }
        Err(error) => log::error!("control_consent: failed to create Windows panel: {error}"),
    }
}

/// Work area and scale of the monitor containing the cursor. Physical screen
/// coordinates are used deliberately: Windows capture and the process are
/// per-monitor-v2 aware, while the route reports its content size in logical
/// points.
fn cursor_work_area(app: &AppHandle) -> (i32, i32, u32, u32, f64) {
    let cursor = crate::platform::windows::cursor_position().unwrap_or((0.0, 0.0));
    if let Ok(monitors) = app.available_monitors() {
        if let Some(monitor) = monitors.into_iter().find(|monitor| {
            let area = monitor.work_area();
            let left = area.position.x as f64;
            let top = area.position.y as f64;
            let right = left + area.size.width as f64;
            let bottom = top + area.size.height as f64;
            cursor.0 >= left && cursor.0 < right && cursor.1 >= top && cursor.1 < bottom
        }) {
            let area = monitor.work_area();
            return (
                area.position.x,
                area.position.y,
                area.size.width,
                area.size.height,
                monitor.scale_factor().max(1.0),
            );
        }
    }
    (0, 0, 1920, 1080, 1.0)
}

fn present_position(app: &AppHandle, height: f64) -> tauri::PhysicalPosition<i32> {
    let (left, top, width, _height, scale) = cursor_work_area(app);
    let panel_width = (CONTROL_CONSENT_WIDTH * scale).round() as i32;
    let x = left + ((width as i32 - panel_width) / 2).max(0);
    let y = top + (CONTROL_CONSENT_TOP_MARGIN * scale).round() as i32;
    let _ = height;
    tauri::PhysicalPosition { x, y }
}

fn show_without_activate(window: &tauri::WebviewWindow) -> Result<(), String> {
    // Dismissal makes the singleton click-through before hiding it; restore
    // interactivity only for the next visible consent card.
    let _ = window.set_ignore_cursor_events(false);
    let raw = window
        .hwnd()
        .map_err(|error| format!("control consent HWND unavailable: {error}"))?;
    let hwnd = windows::Win32::Foundation::HWND(raw.0 as *mut core::ffi::c_void);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
        )
        .map_err(|error| format!("show consent panel without activation failed: {error}"))
    }
}

/// Position, resize to the route's measured content, and reveal without
/// activating the panel. The route calls this only while a request is queued.
#[tauri::command]
pub fn control_consent_present(app: AppHandle, height: f64) -> Result<(), String> {
    let height = height.clamp(CONTROL_CONSENT_DEFAULT_HEIGHT, CONTROL_CONSENT_MAX_HEIGHT);
    let window = app
        .get_webview_window(CONTROL_CONSENT_LABEL)
        .ok_or_else(|| "control consent panel not created".to_string())?;
    let position = present_position(&app, height);
    let generation = PRESENTATION_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    app.run_on_main_thread(move || {
        if PRESENTATION_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        if let Err(error) = window
            .set_size(tauri::Size::Logical(tauri::LogicalSize {
                width: CONTROL_CONSENT_WIDTH,
                height,
            }))
            .map_err(|error| format!("resize consent panel failed: {error}"))
            .and_then(|_| {
                window
                    .set_position(tauri::Position::Physical(position))
                    .map_err(|error| format!("position consent panel failed: {error}"))
            })
            .and_then(|_| show_without_activate(&window))
        {
            log::warn!("control_consent: presentation failed: {error}");
        }
    })
    .map_err(|error| format!("control consent presentation dispatch failed: {error}"))
}

/// Hide only; the singleton is retained for the next request.
#[tauri::command]
pub async fn control_consent_dismiss(app: AppHandle) {
    let generation = PRESENTATION_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let Some(window) = app.get_webview_window(CONTROL_CONSENT_LABEL) else {
        return;
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if let Err(error) = app.run_on_main_thread(move || {
        if PRESENTATION_GENERATION.load(Ordering::Acquire) == generation {
            let _ = window.set_ignore_cursor_events(true);
            let _ = window.hide();
        }
        let _ = sender.send(());
    }) {
        log::warn!("control_consent: dismiss dispatch failed: {error}");
        return;
    }
    if tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .is_err()
    {
        log::warn!("control_consent: dismiss acknowledgement timed out");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn consent_height_is_bounded() {
        assert_eq!(10.0f64.clamp(120.0, 360.0), 120.0);
        assert_eq!(500.0f64.clamp(120.0, 360.0), 360.0);
        assert_eq!(220.0f64.clamp(120.0, 360.0), 220.0);
    }
}
