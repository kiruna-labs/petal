//! Floating native NSPanel for AI chat sessions (#738).
//!
//! Created once at setup as a hidden singleton, never destroyed (CLAUDE.md
//! crash class 2: no `.close()`, hide-and-retire only).
//!
//! Focus semantics:
//! - `can_become_key_window: true` so the panel can take keyboard focus for its
//!   text input and pointer focus for press-and-hold PTT.
//! - `.no_activate(true)` + `nonactivating_panel()` style mask + `raise_panel_only`
//!   so showing the panel (especially remotely-initiated sessions) never steals
//!   key focus or activates the foreground app from what the user is typing in.
//! - `accept_first_mouse(true)` so the first click/press directly reaches the
//!   panel's controls without being swallowed by window activation.

#![cfg(target_os = "macos")]

use crate::platform::cg::WindowFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, WebviewUrl};
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

pub const AI_CHAT_PANEL_LABEL: &str = "ai-chat-panel";
pub const AI_CHAT_PANEL_WIDTH: f64 = 340.0;
pub const AI_CHAT_PANEL_HEIGHT: f64 = 440.0;

static AI_CHAT_PANEL_TRANSPARENCY_APPLIED: AtomicBool = AtomicBool::new(false);

/// Information returned to the frontend panel on present.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatPanelInfo {
    pub owner_app_name: Option<String>,
}

struct LastPositionState {
    window_id: u32,
    x: f64,
    y: f64,
}

static LAST_POSITION: Mutex<Option<LastPositionState>> = Mutex::new(None);

tauri_panel! {
    panel!(AiChatPanel {
        config: {
            can_become_key_window: true,
            is_floating_panel: true
        }
    })
}

/// Create the singleton AI chat panel at app setup.
pub fn create_ai_chat_panel(app: &AppHandle) {
    match PanelBuilder::<_, AiChatPanel>::new(app, AI_CHAT_PANEL_LABEL)
        .url(WebviewUrl::App("ai-chat-panel.html".into()))
        .title("AI Chat")
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: -10000.0,
            y: -10000.0,
        }))
        .level(PanelLevel::Normal)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: AI_CHAT_PANEL_WIDTH,
            height: AI_CHAT_PANEL_HEIGHT,
        }))
        .has_shadow(true)
        .transparent(true)
        .no_activate(true)
        .style_mask(tauri_nspanel::StyleMask::empty().nonactivating_panel())
        .corner_radius(0.0)
        .with_window(|w| w.decorations(false).transparent(true).accept_first_mouse(true))
        .collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary(),
        )
        .build()
    {
        Ok(panel) => {
            panel.hide();
            if let Some(window) = app.get_webview_window(AI_CHAT_PANEL_LABEL) {
                let _ = window.set_ignore_cursor_events(false);
                crate::webview_transparency::apply_or_retry(app, &window);
            }
        }
        Err(e) => {
            log::error!("ai_chat_panel: failed to create panel: {e}");
        }
    }
}

/// Calculate the top-left origin for the AI chat panel relative to the shared window.
///
/// Pure function (unit-testable).
/// Placement: left edge = window right edge, bottom edge = window bottom edge.
/// Clamped horizontally against `work_area` right edge (fallback to right-edge-flush
/// if it wouldn't fit), and clamped vertically within `work_area`.
///
/// If `frame` is `None` (window off-screen or minimized), falls back to centered
/// in `work_area`.
pub fn calculate_ai_chat_panel_position(
    frame: Option<WindowFrame>,
    panel_size: (f64, f64),
    work_area: (f64, f64, f64, f64),
) -> (f64, f64) {
    let (panel_w, panel_h) = panel_size;
    let (wx, wy, ww, wh) = work_area;

    let Some(f) = frame else {
        // Fallback: center on primary/target monitor's work area
        let x = wx + (ww - panel_w) / 2.0;
        let y = wy + (wh - panel_h) / 2.0;
        return (x, y);
    };

    let preferred_x = f.x as f64 + f.width as f64;
    let preferred_y = f.y as f64 + f.height as f64 - panel_h;

    // Horizontal clamping: if right edge exceeds work_area right edge, align right edge to work_area right
    let x = if preferred_x + panel_w > wx + ww {
        (wx + ww) - panel_w
    } else {
        preferred_x
    };
    let max_x = (wx + ww - panel_w).max(wx);
    let x = x.clamp(wx, max_x);

    // Vertical clamping
    let max_y = (wy + wh - panel_h).max(wy);
    let y = preferred_y.clamp(wy, max_y);

    (x, y)
}

/// Find the Tauri `Monitor` containing the shared window's frame (or fallback).
pub fn monitor_containing_frame(app: &AppHandle, frame: &WindowFrame) -> Option<tauri::Monitor> {
    let cx = frame.x as f64 + frame.width as f64 / 2.0;
    let cy = frame.y as f64 + frame.height as f64 / 2.0;
    if let Ok(monitors) = app.available_monitors() {
        for monitor in monitors {
            let scale = monitor.scale_factor().max(1.0);
            let work_area = monitor.work_area();
            let mx = work_area.position.x as f64 / scale;
            let my = work_area.position.y as f64 / scale;
            let mw = work_area.size.width as f64 / scale;
            let mh = work_area.size.height as f64 / scale;
            if cx >= mx && cx < mx + mw && cy >= my && cy < my + mh {
                return Some(monitor);
            }
        }
    }
    app.primary_monitor().ok().flatten()
}

fn get_work_area_and_scale(app: &AppHandle, frame: Option<&WindowFrame>) -> ((f64, f64, f64, f64), f64) {
    let monitor = frame
        .and_then(|f| monitor_containing_frame(app, f))
        .or_else(|| app.primary_monitor().ok().flatten());

    if let Some(mon) = monitor {
        let scale = mon.scale_factor().max(1.0);
        let wa = mon.work_area();
        (
            (
                wa.position.x as f64 / scale,
                wa.position.y as f64 / scale,
                wa.size.width as f64 / scale,
                wa.size.height as f64 / scale,
            ),
            scale,
        )
    } else {
        ((0.0, 0.0, 1440.0, 900.0), 1.0)
    }
}

/// Position, reveal focus-safely (`raise_panel_only`), and return info for `window_id`.
#[tauri::command]
pub fn ai_chat_panel_present(app: AppHandle, window_id: u32) -> Result<AiChatPanelInfo, String> {
    let frame = crate::platform::cg::frame_for_window_id(window_id);
    let owner_app_name = crate::platform::cg::owner_name_for_window_id(window_id);

    let (work_area, _scale) = get_work_area_and_scale(&app, frame.as_ref());
    let (x, y) = calculate_ai_chat_panel_position(frame, (AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT), work_area);

    if std::env::var_os("PETAL_TRACE_PANEL_GEOMETRY").is_some() {
        log::info!(
            "PETAL_TRACE_PANEL_GEOMETRY: ai_chat_panel_present window_id={window_id} frame={frame:?} pos=({x:.2}, {y:.2})"
        );
    }

    if let Ok(mut guard) = LAST_POSITION.lock() {
        *guard = Some(LastPositionState { window_id, x, y });
    }

    let app_main = app.clone();
    crate::platform::on_main(&app, "ai_chat_panel: present", move || {
        let Some(window) = app_main.get_webview_window(AI_CHAT_PANEL_LABEL) else {
            log::warn!("ai_chat_panel: present called but the panel does not exist");
            return;
        };

        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        if !AI_CHAT_PANEL_TRANSPARENCY_APPLIED.load(Ordering::Relaxed)
            && crate::webview_transparency::force_window_transparent(&window)
        {
            AI_CHAT_PANEL_TRANSPARENCY_APPLIED.store(true, Ordering::Relaxed);
        }

        if let Err(e) = crate::platform::appkit::raise_panel_only(&window) {
            log::warn!("ai_chat_panel: failed to raise panel without focus: {e}");
        }
    });

    Ok(AiChatPanelInfo { owner_app_name })
}

/// Hide the AI chat panel (never close -- CLAUDE.md crash class 2).
#[tauri::command]
pub fn ai_chat_panel_dismiss(app: AppHandle) {
    if let Ok(mut guard) = LAST_POSITION.lock() {
        *guard = None;
    }
    let app_main = app.clone();
    crate::platform::on_main(&app, "ai_chat_panel: dismiss", move || {
        if let Some(window) = app_main.get_webview_window(AI_CHAT_PANEL_LABEL) {
            let _ = window.hide();
        }
    });
}

/// Re-raise the active panel without taking key status. Must run on the main thread.
pub(crate) fn raise_ai_chat_panel_if_active_on_main(app: &AppHandle, window_id: u32) {
    // Re-check at execution time so a queued tracker update cannot resurrect a
    // panel that the session end path has already hidden.
    if crate::ai_chat::session::active_window_id() != Some(window_id) {
        return;
    }
    let Some(window) = app.get_webview_window(AI_CHAT_PANEL_LABEL) else {
        return;
    };
    if let Err(e) = crate::platform::appkit::raise_panel_only(&window) {
        log::warn!("ai_chat_panel: failed to re-raise panel without focus: {e}");
    }
}

/// Update position when the shared window moves/resizes (option a hysteresis update).
pub fn update_ai_chat_panel_frame(app: &AppHandle, window_id: u32, frame: WindowFrame) {
    // Only update if AI chat session is active for this window
    if crate::ai_chat::session::active_window_id() != Some(window_id) {
        return;
    }

    let (work_area, _scale) = get_work_area_and_scale(app, Some(&frame));
    let (new_x, new_y) = calculate_ai_chat_panel_position(
        Some(frame),
        (AI_CHAT_PANEL_WIDTH, AI_CHAT_PANEL_HEIGHT),
        work_area,
    );

    let should_update = if let Ok(guard) = LAST_POSITION.lock() {
        if let Some(ref last) = *guard {
            last.window_id != window_id
                || (last.x - new_x).abs() >= 2.0
                || (last.y - new_y).abs() >= 2.0
        } else {
            true
        }
    } else {
        true
    };

    if !should_update {
        return;
    }

    if std::env::var_os("PETAL_TRACE_PANEL_GEOMETRY").is_some() {
        log::info!(
            "PETAL_TRACE_PANEL_GEOMETRY: update_ai_chat_panel_frame window_id={window_id} pos=({new_x:.2}, {new_y:.2})"
        );
    }

    if let Ok(mut guard) = LAST_POSITION.lock() {
        *guard = Some(LastPositionState {
            window_id,
            x: new_x,
            y: new_y,
        });
    }

    let app_main = app.clone();
    crate::platform::on_main(&app, "ai_chat_panel: update_frame", move || {
        if let Some(window) = app_main.get_webview_window(AI_CHAT_PANEL_LABEL) {
            let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
                x: new_x,
                y: new_y,
            }));
        }
        raise_ai_chat_panel_if_active_on_main(&app_main, window_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_item<'a>(source: &'a str, start_marker: &str, next_marker: &str) -> &'a str {
        let start = source
            .find(start_marker)
            .unwrap_or_else(|| panic!("missing source item: {start_marker}"));
        let after_start = &source[start..];
        let end = after_start
            .find(next_marker)
            .unwrap_or_else(|| panic!("missing following source item: {next_marker}"));
        &after_start[..end]
    }

    /// #738: this panel can become key for its text input, so tao's `show()`
    /// (`makeKeyAndOrderFront:`) steals key status even on a nonactivating
    /// panel. The real command must reveal only through `raise_panel_only` /
    /// `orderFrontRegardless`.
    #[test]
    fn ai_chat_panel_present_never_shows_or_keys_the_panel() {
        let source = include_str!("panel.rs");
        let body = source_item(
            source,
            "pub fn ai_chat_panel_present(",
            "\n/// Hide the AI chat panel",
        );

        assert!(
            body.contains("raise_panel_only(&window)"),
            "ai_chat_panel_present must reveal via orderFrontRegardless (#738) -- got:\n{body}"
        );
        assert!(
            !body.contains(".show()"),
            "ai_chat_panel_present must not call tao show/makeKeyAndOrderFront (#738) -- got:\n{body}"
        );
    }

    /// The frame tracker and its reorder path must both restore the panel's
    /// non-key z-order, so clicking the shared window cannot bury a live panel.
    #[test]
    fn ai_chat_panel_tracking_reasserts_nonactivating_order() {
        let panel_source = include_str!("panel.rs");
        let raise = source_item(
            panel_source,
            "pub(crate) fn raise_ai_chat_panel_if_active_on_main(",
            "\n/// Update position when the shared window moves/resizes",
        );
        assert!(
            raise.contains("raise_panel_only(&window)"),
            "tracker raises must use orderFrontRegardless (#738) -- got:\n{raise}"
        );
        assert!(
            !raise.contains(".show()"),
            "tracker raises must never call tao show/makeKeyAndOrderFront (#738) -- got:\n{raise}"
        );

        let update = source_item(
            panel_source,
            "pub fn update_ai_chat_panel_frame(",
            "\n#[cfg(test)]",
        );
        assert!(
            update.contains("raise_ai_chat_panel_if_active_on_main(&app_main, window_id)"),
            "frame updates must re-raise the active AI chat panel (#738) -- got:\n{update}"
        );

        let border_source = include_str!("../share_border.rs");
        let reorder = source_item(
            border_source,
            "BorderAction::Reorder => {",
            "\n                        BorderAction::SetFrame(frame) => {",
        );
        assert!(
            reorder.contains("raise_ai_chat_panel_if_active_on_main"),
            "the periodic border reorder path must re-raise the active AI chat panel (#738) -- got:\n{reorder}"
        );
    }

    #[test]
    fn test_calculate_position_normal_window() {
        let frame = WindowFrame {
            x: 100,
            y: 100,
            width: 500,
            height: 400,
        };
        let panel_size = (340.0, 440.0);
        let work_area = (0.0, 25.0, 1440.0, 875.0);

        let (x, y) = calculate_ai_chat_panel_position(Some(frame), panel_size, work_area);

        // Right edge of window = 100 + 500 = 600
        assert_eq!(x, 600.0);
        // Bottom edge of window = 100 + 400 = 500. Top of panel = 500 - 440 = 60
        assert_eq!(y, 60.0);
    }

    #[test]
    fn test_calculate_position_right_edge_fallback() {
        let frame = WindowFrame {
            x: 1200,
            y: 100,
            width: 500,
            height: 400,
        };
        let panel_size = (340.0, 440.0);
        let work_area = (0.0, 25.0, 1440.0, 875.0);

        let (x, y) = calculate_ai_chat_panel_position(Some(frame), panel_size, work_area);

        // Preferred x = 1700, but 1700 + 340 > 1440. So x = 1440 - 340 = 1100.
        assert_eq!(x, 1100.0);
        assert_eq!(y, 60.0);
    }

    #[test]
    fn test_calculate_position_bottom_dock_clamping() {
        let frame = WindowFrame {
            x: 100,
            y: 600,
            width: 500,
            height: 400,
        };
        let panel_size = (340.0, 440.0);
        let work_area = (0.0, 25.0, 1440.0, 875.0); // max y for top edge is 875 - 440 = 435

        let (x, y) = calculate_ai_chat_panel_position(Some(frame), panel_size, work_area);

        assert_eq!(x, 600.0);
        // Preferred y = 600 + 400 - 440 = 560. Work area bottom is wy + wh =
        // 25 + 875 = 900 (work_area.1 is the vertical OFFSET from screen top,
        // e.g. the menu bar height -- not itself the bottom edge), so
        // max_y = 900 - 440 = 460, matching the offscreen-fallback test's
        // convention two tests below (which correctly includes wy). The
        // original expected value of 435.0 dropped the +25 offset.
        assert_eq!(y, 460.0);
    }

    #[test]
    fn test_calculate_position_offscreen_fallback() {
        let panel_size = (340.0, 440.0);
        let work_area = (0.0, 25.0, 1440.0, 875.0);

        let (x, y) = calculate_ai_chat_panel_position(None, panel_size, work_area);

        // Centered: x = 0 + (1440 - 340)/2 = 550; y = 25 + (875 - 440)/2 = 242.5
        assert_eq!(x, 550.0);
        assert_eq!(y, 242.5);
    }
}
