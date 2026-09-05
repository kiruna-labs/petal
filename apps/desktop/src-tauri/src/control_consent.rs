//! Sharer-side remote-control consent surface ("<Name> wants to control
//! <window>" or "<Name> requested full control" -- Allow / Deny) for the
//! `ask` policy
//! (`remote_control_core::RemoteControlPolicy`, the default).
//!
//! An always-present, hidden `NSPanel` + SvelteKit route
//! (`routes/control-consent/`), cloned from `share_notice.rs` -- the one
//! existing precedent for a small, transparent, NON-ACTIVATING panel that
//! hosts a dedicated route in screen coordinates and must appear without a
//! hover. The trust model calls the approval surface "a non-activating
//! prompt" (docs/remote-control-trust-model.md): it must not steal focus from
//! the app the sharer is using mid-share, which rules out the main window's
//! Modal, and it must appear whether or not the cursor is over the shared
//! window, which rules out the hover tab (hover-gated, and fixed-size on
//! macOS).
//!
//! Division of labor, same as share-notice: this module owns create / show /
//! hide and top-center positioning; the route owns content, the request
//! QUEUE (ordinary control and full-control escalation prompts are keyed by
//! kind/window/controller and never replace one another), the visible
//! countdown, and the Allow/Deny command calls.
//! Height is measured by the page and reported through
//! [`control_consent_present`] (resize-to-content, CLAUDE.md "UI text must
//! NEVER truncate": a participant display name is unbounded and WRAPS).
//!
//! Crash-class discipline (CLAUDE.md): SINGLETON panel, created once in
//! `lib.rs`'s `.setup()`, never destroyed -- shown/hidden only. All AppKit
//! work is marshalled onto the main thread via `platform::on_main`. Listed in
//! `apps/desktop/scripts/check-panel-close.mjs`.

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, WebviewUrl};
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

pub const CONTROL_CONSENT_LABEL: &str = "control-consent";

/// Panel width in logical points -- fixed and deliberately generous so only
/// HEIGHT is ever measured. The card's copy column is capped at
/// `CONTROL_CONSENT_COPY_MAX` (see the route's CSS) and wraps past that; the
/// action row is two buttons (~96px each incl. gap). 420 leaves margin above
/// copy + padding so a long participant name only ever grows the card
/// downward, which `control_consent_present` handles.
const CONTROL_CONSENT_WIDTH: f64 = 420.0;
/// Floor for the measured height (a single-line card never needs less).
const CONTROL_CONSENT_DEFAULT_HEIGHT: f64 = 120.0;
/// Cap on how tall the card can grow: several wrapped lines of a very long
/// display name + title still fit; the route wraps rather than truncates and
/// the panel grows to keep every wrapped line on screen.
const CONTROL_CONSENT_MAX_HEIGHT: f64 = 360.0;
/// Clearance from the top of the monitor's WORK AREA (already excludes the
/// menu bar). Offset below the share-notice pill so the two can coexist.
const CONTROL_CONSENT_TOP_MARGIN: f64 = 96.0;

static CONTROL_CONSENT_TRANSPARENCY_APPLIED: AtomicBool = AtomicBool::new(false);

/// Create the singleton, always-present, hidden consent panel. Called once
/// from `lib.rs`'s `.setup()`, right after `create_share_notice_panel`.
pub fn create_control_consent_panel(app: &AppHandle) {
    tauri_panel! {
        panel!(ControlConsentPanel {
            config: {
                can_become_key_window: false,
                is_floating_panel: true
            }
        })
    }

    match PanelBuilder::<_, ControlConsentPanel>::new(app, CONTROL_CONSENT_LABEL)
        .url(WebviewUrl::App("control-consent.html".into()))
        .title("Petal")
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: -10000.0,
            y: -10000.0,
        }))
        // Floating: a consent prompt must stay above the app being shared
        // (which the sharer is actively using) or it can be missed entirely
        // and time out to deny. Unlike the transient share notice, this one
        // waits for an answer.
        .level(PanelLevel::Floating)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: CONTROL_CONSENT_WIDTH,
            height: CONTROL_CONSENT_DEFAULT_HEIGHT,
        }))
        .has_shadow(true)
        .transparent(true)
        .no_activate(true)
        .style_mask(tauri_nspanel::StyleMask::empty().nonactivating_panel())
        .corner_radius(0.0)
        .with_window(|w| w.decorations(false).transparent(true))
        .collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary(),
        )
        .build()
    {
        Ok(panel) => {
            panel.hide();
            if let Some(window) = app.get_webview_window(CONTROL_CONSENT_LABEL) {
                // Allow / Deny are real buttons -- never click-through.
                let _ = window.set_ignore_cursor_events(false);
                crate::webview_transparency::apply_or_retry(app, &window);
            }
        }
        Err(e) => {
            log::error!("control_consent: failed to create panel: {e}");
        }
    }
}

/// Top-left origin that horizontally centers the panel on the monitor under
/// the cursor's work area, `CONTROL_CONSENT_TOP_MARGIN` below its top. Same
/// recipe as `share_notice::top_center_origin`.
fn top_center_origin(app: &AppHandle) -> (f64, f64) {
    let monitor = crate::hover_tab::platform::get_monitor_with_cursor(app)
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return (80.0, CONTROL_CONSENT_TOP_MARGIN);
    };
    let scale = monitor.scale_factor().max(1.0);
    let work_area = monitor.work_area();
    let ax = work_area.position.x as f64 / scale;
    let ay = work_area.position.y as f64 / scale;
    let aw = work_area.size.width as f64 / scale;
    let (x, _) = crate::window_picker::centered_origin_in_work_area(
        (ax, ay, aw, 0.0),
        (CONTROL_CONSENT_WIDTH, 0.0),
    );
    (x, ay + CONTROL_CONSENT_TOP_MARGIN)
}

/// Position (top-center), resize to the page's measured content height, and
/// show the consent panel. Idempotent; always sets position AND size together
/// (see share_notice.rs for why).
#[tauri::command]
pub fn control_consent_present(app: AppHandle, height: f64) {
    let height = height.clamp(CONTROL_CONSENT_DEFAULT_HEIGHT, CONTROL_CONSENT_MAX_HEIGHT);
    let app_main = app.clone();
    crate::platform::on_main(&app, "control_consent: present", move || {
        let (x, y) = top_center_origin(&app_main);
        let Some(window) = app_main.get_webview_window(CONTROL_CONSENT_LABEL) else {
            log::warn!("control_consent: present called but the panel does not exist");
            return;
        };
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: CONTROL_CONSENT_WIDTH,
            height,
        }));
        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        if !CONTROL_CONSENT_TRANSPARENCY_APPLIED.load(Ordering::Relaxed)
            && crate::webview_transparency::force_window_transparent(&window)
        {
            CONTROL_CONSENT_TRANSPARENCY_APPLIED.store(true, Ordering::Relaxed);
        }
        if let Err(e) = window.show() {
            log::warn!("control_consent: failed to show panel: {e}");
        }
    });
}

/// Hide the consent panel (never close -- CLAUDE.md crash class 2). Called by
/// the page once its queue is empty.
#[tauri::command]
pub fn control_consent_dismiss(app: AppHandle) {
    let app_main = app.clone();
    crate::platform::on_main(&app, "control_consent: dismiss", move || {
        if let Some(window) = app_main.get_webview_window(CONTROL_CONSENT_LABEL) {
            let _ = window.hide();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_consent_present_clamps_height_to_the_documented_range() {
        assert_eq!(
            10.0f64.clamp(CONTROL_CONSENT_DEFAULT_HEIGHT, CONTROL_CONSENT_MAX_HEIGHT),
            CONTROL_CONSENT_DEFAULT_HEIGHT
        );
        assert_eq!(
            10_000.0f64.clamp(CONTROL_CONSENT_DEFAULT_HEIGHT, CONTROL_CONSENT_MAX_HEIGHT),
            CONTROL_CONSENT_MAX_HEIGHT
        );
    }

    #[test]
    fn control_consent_width_budget_covers_the_copy_column_and_padding() {
        // Mirrors the route's CSS: card padding 16px each side, copy column
        // capped at 340px (wraps past that), so width never needs measuring.
        let padding = 16.0 * 2.0;
        let copy_max = 340.0;
        assert!(
            CONTROL_CONSENT_WIDTH >= padding + copy_max,
            "CONTROL_CONSENT_WIDTH ({CONTROL_CONSENT_WIDTH}) must cover padding + the copy cap"
        );
        // Two action buttons (min 96px each + 8px gap) must fit too.
        assert!(CONTROL_CONSENT_WIDTH >= padding + 96.0 * 2.0 + 8.0);
    }
}
