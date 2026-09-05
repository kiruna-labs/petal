//! Windows AI chat panel — a hidden WebviewWindow singleton hosting the
//! `ai-chat-panel.html` route, mirroring the macOS NSPanel surface (#738).
//!
//! Same discipline as the macOS panel:
//! - Created once at setup as a hidden singleton, NEVER destroyed (hide-and-
//!   retire only — CLAUDE.md crash class 2).
//! - Present positions the panel beside the shared window (left edge = window
//!   right edge, bottom-aligned, clamped to the work area) and shows it
//!   WITHOUT stealing activation — a remote participant's start request must
//!   not yank focus away from whatever the host is typing in.
//! - Dismiss hides; the panel is re-presented by a later start.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const AI_CHAT_PANEL_LABEL: &str = "ai-chat-panel";
pub const AI_CHAT_PANEL_WIDTH: f64 = 340.0;
pub const AI_CHAT_PANEL_HEIGHT: f64 = 440.0;

/// Information returned to the frontend panel on present.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatPanelInfo {
    pub owner_app_name: Option<String>,
}

/// Create the singleton AI chat panel at app setup (main thread — WebView2
/// cannot be built off it). Hidden and parked off-screen until first present.
pub fn create_ai_chat_panel(app: &AppHandle) {
    if app.get_webview_window(AI_CHAT_PANEL_LABEL).is_some() {
        return;
    }
    let builder = WebviewWindowBuilder::new(
        app,
        AI_CHAT_PANEL_LABEL,
        WebviewUrl::App("ai-chat-panel.html".into()),
    )
    .title("AI Chat")
    .decorations(false)
    .transparent(true)
    // Transparent WebView2 default background: the panel is shown and hidden
    // repeatedly (its compositor throttles while hidden), and the pre-paint/
    // resume gap must not flash as a hollow white rectangle next to the
    // shared window.
    .background_color(tauri::window::Color(0, 0, 0, 0))
    .resizable(false)
    .skip_taskbar(true)
    .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
    .visible(false)
    .inner_size(AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT);
    match builder.build() {
        Ok(window) => {
            let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
                x: -10000.0,
                y: -10000.0,
            }));
            log::info!("ai_chat_panel: Windows panel created (hidden, off-screen)");
        }
        Err(e) => log::error!("ai_chat_panel: failed to create panel: {e}"),
    }
}

/// Calculate the top-left origin for the AI chat panel relative to the shared
/// window (pure — mirrored from the macOS panel so both platforms place the
/// panel identically). Placement: left edge = window right edge, bottom edge =
/// window bottom edge; clamped to `work_area`; falls back to centered when the
/// window frame is unknown (off-screen/minimized).
fn calculate_ai_chat_panel_position(
    frame: Option<crate::platform::cg::WindowFrame>,
    panel_size: (f64, f64),
    work_area: (f64, f64, f64, f64),
) -> (f64, f64) {
    let (panel_w, panel_h) = panel_size;
    let (wx, wy, ww, wh) = work_area;

    let Some(f) = frame else {
        let x = wx + (ww - panel_w) / 2.0;
        let y = wy + (wh - panel_h) / 2.0;
        return (x, y);
    };

    let preferred_x = f.x as f64 + f.width as f64;
    let preferred_y = f.y as f64 + f.height as f64 - panel_h;

    let x = if preferred_x + panel_w > wx + ww {
        (wx + ww) - panel_w
    } else {
        preferred_x
    };
    let max_x = (wx + ww - panel_w).max(wx);
    let x = x.clamp(wx, max_x);

    let max_y = (wy + wh - panel_h).max(wy);
    let y = preferred_y.clamp(wy, max_y);

    (x, y)
}

/// Work area (physical px) of the monitor containing the shared window's
/// frame, falling back to the primary monitor.
fn work_area_for(
    app: &AppHandle,
    frame: Option<&crate::platform::cg::WindowFrame>,
) -> (f64, f64, f64, f64) {
    let monitor = frame
        .and_then(|f| {
            let cx = f.x as f64 + f.width as f64 / 2.0;
            let cy = f.y as f64 + f.height as f64 / 2.0;
            app.available_monitors().ok().and_then(|monitors| {
                monitors.into_iter().find(|m| {
                    let wa = m.work_area();
                    let mx = wa.position.x as f64;
                    let my = wa.position.y as f64;
                    let mw = wa.size.width as f64;
                    let mh = wa.size.height as f64;
                    cx >= mx && cx < mx + mw && cy >= my && cy < my + mh
                })
            })
        })
        .or_else(|| app.primary_monitor().ok().flatten());

    if let Some(mon) = monitor {
        let wa = mon.work_area();
        (
            wa.position.x as f64,
            wa.position.y as f64,
            wa.size.width as f64,
            wa.size.height as f64,
        )
    } else {
        (0.0, 0.0, 1440.0, 900.0)
    }
}

/// Owning process image name (e.g. `notepad.exe`) for a shared window, used
/// as the panel's `ownerAppName` (macOS parity: the frontend says what the
/// session is about). `None` when the process can't be named.
fn owner_process_name_for(window_id: u32) -> Option<String> {
    let target = crate::windows_capture_target::resolve(window_id).ok()?;
    let pid = target.owner_process_id();
    if pid == 0 {
        return None;
    }
    let handle = unsafe {
        windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
    }
    .ok()?;
    let mut buf = vec![0u16; 1024];
    let mut size = buf.len() as u32;
    let result = unsafe {
        windows::Win32::System::Threading::QueryFullProcessImageNameW(
            handle,
            windows::Win32::System::Threading::PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    result.ok()?;
    buf.truncate(size as usize);
    let path = String::from_utf16_lossy(&buf);
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(path);
    Some(name)
}

/// Show the panel without activating it (SetWindowPos SWP_NOACTIVATE) — a
/// remotely-initiated session must never steal key focus.
fn show_without_activate(window: &tauri::WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as *mut core::ffi::c_void);
    let _ = unsafe {
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
    };
}

/// Position, show focus-safely, and return info for `window_id`.
#[tauri::command]
pub fn ai_chat_panel_present(app: AppHandle, window_id: u32) -> Result<AiChatPanelInfo, String> {
    let frame = crate::windows_capture_target::resolve(window_id)
        .ok()
        .map(|target| {
            crate::platform::windows::window_frame_for_raw(target.raw_handle())
        })
        .flatten();
    let owner_app_name = owner_process_name_for(window_id);

    let work_area = work_area_for(&app, frame.as_ref());
    let (x, y) = calculate_ai_chat_panel_position(
        frame,
        (AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT),
        work_area,
    );

    if std::env::var_os("PETAL_TRACE_PANEL_GEOMETRY").is_some() {
        log::info!(
            "PETAL_TRACE_PANEL_GEOMETRY: ai_chat_panel_present window_id={window_id} frame={frame:?} pos=({x:.2}, {y:.2})"
        );
    }

    let Some(window) = app.get_webview_window(AI_CHAT_PANEL_LABEL) else {
        log::warn!("ai_chat_panel: present called but the panel does not exist");
        return Err("AI chat panel not created".to_string());
    };
    let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
    show_without_activate(&window);

    Ok(AiChatPanelInfo { owner_app_name })
}

/// Hide the AI chat panel (never close — CLAUDE.md crash class 2).
#[tauri::command]
pub fn ai_chat_panel_dismiss(app: AppHandle) {
    if let Some(window) = app.get_webview_window(AI_CHAT_PANEL_LABEL) {
        let _ = window.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_places_beside_window_right_edge_bottom_aligned() {
        let frame = crate::platform::cg::WindowFrame {
            x: 100,
            y: 200,
            width: 800,
            height: 600,
        };
        let (x, y) = calculate_ai_chat_panel_position(
            Some(frame),
            (AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT),
            (0.0, 0.0, 1920.0, 1040.0),
        );
        assert_eq!(x, 900.0); // window right edge
        assert_eq!(y, 360.0); // bottom-aligned
    }

    #[test]
    fn panel_clamps_to_work_area_right_edge() {
        let frame = crate::platform::cg::WindowFrame {
            x: 1600,
            y: 200,
            width: 800,
            height: 600,
        };
        let (x, _) = calculate_ai_chat_panel_position(
            Some(frame),
            (AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT),
            (0.0, 0.0, 1920.0, 1040.0),
        );
        assert_eq!(x, 1580.0); // 1920 - 340
    }

    #[test]
    fn panel_centers_when_frame_unknown() {
        let (x, y) = calculate_ai_chat_panel_position(
            None,
            (AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT),
            (0.0, 0.0, 1920.0, 1040.0),
        );
        assert_eq!(x, 790.0);
        assert_eq!(y, 300.0);
    }
}
