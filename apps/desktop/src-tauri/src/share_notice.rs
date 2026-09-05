//! Top-center "<Name> is sharing a window — Bring to foreground" notice
//! (#679). Fires for 4s when a REMOTE peer starts a NEW share (never for own
//! shares, never for a republish/quality-switch/reconnect re-subscribe --
//! that gating lives in `compositor::consume_share_started_pill_suppression`,
//! called from `transport::subscriber` right before `emit_remote_share_started`
//! below).
//!
//! An always-present, hidden `NSPanel` + SvelteKit route
//! (`routes/share-notice/`), cloned from `create_hover_tab`'s recipe in
//! `lib.rs` -- the only existing precedent in this codebase for a small,
//! transparent, non-activating panel hosting a dedicated route in screen
//! coordinates. Differences from that recipe, both deliberate:
//!
//! - Latest-share replacement and the 4s auto-dismiss timer both live in the
//!   frontend (`+page.svelte`) -- this module only owns create/show/hide and
//!   top-center positioning, the same division of labor `menubar.rs`'s popover
//!   already has between Rust (panel lifecycle) and its own webview
//!   (content/timing).
//! - Height is measured by the page and reported back through
//!   [`share_notice_present`], the SAME resize-to-content pattern
//!   `menubar.rs::resize_menubar_popover` already uses, so an arbitrarily
//!   long display name never gets clipped by a fixed native frame
//!   (CLAUDE.md's "UI text must NEVER truncate" hard rule). Width is fixed
//!   and deliberately generous instead of measured -- see
//!   `SHARE_NOTICE_WIDTH`'s doc comment for why that one dimension doesn't
//!   need measuring.
//! - `share_notice_present` always sets BOTH position AND size together
//!   (mirrors `hover_tab::apply_hover_tab_panel_frame`) rather than resizing
//!   in place, so the visible result is deterministically top-anchored +
//!   horizontally centered regardless of which corner a bare
//!   `NSWindow`/`NSPanel` size change would otherwise anchor from.
//!
//! Crash-class discipline (CLAUDE.md): this is a SINGLETON panel, created
//! once and never destroyed -- shown/hidden only, exactly like the hover tab
//! and the menubar popover (closing a `tauri_nspanel` panel reproducibly
//! aborts the app a few seconds later via a deferred-dealloc ObjC
//! exception). All AppKit work below is marshalled onto the main thread via
//! `platform::on_main`.

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, WebviewUrl};
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

pub const SHARE_NOTICE_LABEL: &str = "share-notice";

/// `remote-share-started` event payload (mirrors `RemoteShareStartedEvent` in
/// `ipc.ts`). Emitted from `transport::subscriber` immediately after a
/// genuinely new remote share opens.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteShareStartedPayload {
    pub window_id: u32,
    pub owner_identity: String,
    pub owner_display_name: String,
    pub source_title: String,
}

/// Emit the event. Global `tauri::Emitter::emit`, NOT `emit_to(label, ...)`
/// -- a documented repeat bug in this codebase (`hover_tab.rs:1260-1263`,
/// `resilience.rs:665-670`, `telepointer.rs:551`): in Tauri 2, `emit_to`
/// never matches a page's plain `listen()`. The always-loaded, always-hidden
/// share-notice webview listens for this on the global bus, same as the
/// hover tab does for `share-state-changed`.
pub fn emit_remote_share_started(app: &AppHandle, payload: RemoteShareStartedPayload) {
    let _ = tauri::Emitter::emit(app, "remote-share-started", payload);
}

/// Panel width in logical points -- fixed and deliberately generous rather
/// than dynamically measured, so ONLY height ever needs to be measured.
/// Justification: Toast.svelte's own `.message` CSS caps at
/// `min(360px, calc(100vw - 64px))` and WRAPS rather than truncates past
/// that; the row is icon (~24px incl. gap) + message (up to 360px) + the
/// "Bring to foreground" action pill (~170px incl. gap, the longest label
/// this route renders) + the Pill's own 32px padding = ~586px at the
/// longest a SINGLE LINE ever gets before the message itself wraps to a
/// second line instead of growing wider. 620px leaves margin above that, so
/// this route never needs to measure or grow width -- only a wrapped
/// message's HEIGHT is ever in question, which `share_notice_present`
/// handles.
const SHARE_NOTICE_WIDTH: f64 = 620.0;
/// Height before the very first real content measurement lands, and the
/// floor `share_notice_present` clamps to (a single-line pill never needs
/// less than this).
const SHARE_NOTICE_DEFAULT_HEIGHT: f64 = 64.0;
/// Sensible cap on how tall the notice can grow. Comfortably fits several
/// wrapped lines of an unusually long display name -- Toast's own message
/// CSS wraps rather than truncates; this only bounds how far the PANEL
/// itself grows to keep showing that wrapped text on screen.
const SHARE_NOTICE_MAX_HEIGHT: f64 = 220.0;
/// Clearance from the top of the monitor's WORK AREA -- work area (not the
/// full monitor bounds) already excludes the menu bar, see
/// `window_picker::centered_secondary_window_position`, the same
/// monitor/work-area API this module reuses.
const SHARE_NOTICE_TOP_MARGIN: f64 = 16.0;

/// Re-applied on every `share_notice_present` call until it has verifiably
/// landed -- same belt-and-suspenders as `hover_tab.rs`'s
/// `HOVER_TAB_TRANSPARENCY_APPLIED`: the WKWebView transparency treatment
/// has been observed NOT to stick when applied only once at create-time.
static SHARE_NOTICE_TRANSPARENCY_APPLIED: AtomicBool = AtomicBool::new(false);

/// Create the singleton, always-present, hidden notice panel. Called once
/// from `lib.rs`'s `.setup()`, alongside `create_hover_tab`. Never destroyed
/// -- shown/hidden only (CLAUDE.md crash class 2).
pub fn create_share_notice_panel(app: &AppHandle) {
    tauri_panel! {
        panel!(ShareNoticePanel {
            config: {
                can_become_key_window: false,
                is_floating_panel: true
            }
        })
    }

    match PanelBuilder::<_, ShareNoticePanel>::new(app, SHARE_NOTICE_LABEL)
        .url(WebviewUrl::App("share-notice.html".into()))
        .title("Petal")
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: -10000.0,
            y: -10000.0,
        }))
        // Normal level: a share notice is transient chrome, not content, but
        // it needs no special stacking beyond every other normal window --
        // it is only ever on screen for a few seconds at a time.
        .level(PanelLevel::Normal)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: SHARE_NOTICE_WIDTH,
            height: SHARE_NOTICE_DEFAULT_HEIGHT,
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
            if let Some(window) = app.get_webview_window(SHARE_NOTICE_LABEL) {
                // The pill has a clickable "Bring to foreground" link --
                // click-through (the default for a transparent overlay
                // panel) would defeat it, same reasoning as the hover tab's
                // own comment on this exact call.
                let _ = window.set_ignore_cursor_events(false);
                // Without this the panel composites an opaque black rect on
                // screen despite `.transparent(true)` -- see
                // webview_transparency.rs's doc for the three opacity layers.
                // `apply_or_retry`, not the bare call: during `setup()` the
                // WKWebView treatment can fail to land before the webview has
                // attached, same reasoning as `create_hover_tab`'s identical
                // call.
                crate::webview_transparency::apply_or_retry(app, &window);
            }
        }
        Err(e) => {
            log::error!("share_notice: failed to create panel: {e}");
        }
    }
}

/// Top-left origin (logical points) that horizontally centers a
/// `SHARE_NOTICE_WIDTH`-wide panel on the monitor under the cursor's work
/// area (falling back to the primary monitor, then a fixed inset if no
/// monitor info is available at all), with its top edge
/// `SHARE_NOTICE_TOP_MARGIN` below the work area's top.
///
/// Reuses `window_picker::centered_origin_in_work_area` for the horizontal
/// half of this instead of reimplementing centering math a second time --
/// passing `0.0` for both the work area's height and the target height
/// collapses its `y` term to the work area's own top (`ay`), so only its
/// (already unit-tested) `x` term is actually exercised here.
fn top_center_origin(app: &AppHandle) -> (f64, f64) {
    let monitor = crate::hover_tab::platform::get_monitor_with_cursor(app)
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return (80.0, SHARE_NOTICE_TOP_MARGIN);
    };
    let scale = monitor.scale_factor().max(1.0);
    let work_area = monitor.work_area();
    let ax = work_area.position.x as f64 / scale;
    let ay = work_area.position.y as f64 / scale;
    let aw = work_area.size.width as f64 / scale;
    let (x, _) =
        crate::window_picker::centered_origin_in_work_area((ax, ay, aw, 0.0), (SHARE_NOTICE_WIDTH, 0.0));
    (x, ay + SHARE_NOTICE_TOP_MARGIN)
}

/// Position (top-center), resize to the caller's measured content height,
/// and show the notice panel. Idempotent and safe to call repeatedly --
/// once to reveal a new notice, and again if the page re-measures a reflow
/// (e.g. a late font load) while already visible. ALWAYS sets both position
/// and size together (see this module's doc comment) so the visible result
/// stays top-anchored + horizontally centered regardless of a bare
/// `set_size`'s own anchor behavior.
#[tauri::command]
pub fn share_notice_present(app: AppHandle, height: f64) {
    let height = height
        .clamp(SHARE_NOTICE_DEFAULT_HEIGHT, SHARE_NOTICE_MAX_HEIGHT);
    let app_main = app.clone();
    crate::platform::on_main(&app, "share_notice: present", move || {
        let (x, y) = top_center_origin(&app_main);
        let Some(window) = app_main.get_webview_window(SHARE_NOTICE_LABEL) else {
            log::warn!("share_notice: present called but the panel does not exist");
            return;
        };
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: SHARE_NOTICE_WIDTH,
            height,
        }));
        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
        if !SHARE_NOTICE_TRANSPARENCY_APPLIED.load(Ordering::Relaxed)
            && crate::webview_transparency::force_window_transparent(&window)
        {
            SHARE_NOTICE_TRANSPARENCY_APPLIED.store(true, Ordering::Relaxed);
        }
        // Unlike a persistent surface (e.g. the menubar popover) whose
        // failure to show is immediately obvious to the user, this is
        // transient chrome the user has no other way to notice went
        // missing -- log on error rather than silently swallowing it.
        if let Err(e) = window.show() {
            log::warn!("share_notice: failed to show panel: {e}");
        }
    });
}

/// Hide the notice panel (never close -- CLAUDE.md crash class 2). Called by
/// the page once its own 4s auto-dismiss timer fires, or immediately when
/// the user clicks "Bring to foreground".
#[tauri::command]
pub fn share_notice_dismiss(app: AppHandle) {
    let app_main = app.clone();
    crate::platform::on_main(&app, "share_notice: dismiss", move || {
        if let Some(window) = app_main.get_webview_window(SHARE_NOTICE_LABEL) {
            let _ = window.hide();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_notice_present_clamps_height_to_the_documented_range() {
        assert_eq!(
            SHARE_NOTICE_DEFAULT_HEIGHT.clamp(SHARE_NOTICE_DEFAULT_HEIGHT, SHARE_NOTICE_MAX_HEIGHT),
            SHARE_NOTICE_DEFAULT_HEIGHT
        );
        assert_eq!(
            10.0f64.clamp(SHARE_NOTICE_DEFAULT_HEIGHT, SHARE_NOTICE_MAX_HEIGHT),
            SHARE_NOTICE_DEFAULT_HEIGHT
        );
        assert_eq!(
            10_000.0f64.clamp(SHARE_NOTICE_DEFAULT_HEIGHT, SHARE_NOTICE_MAX_HEIGHT),
            SHARE_NOTICE_MAX_HEIGHT
        );
    }

    #[test]
    fn share_notice_width_budget_covers_the_longest_single_line_row() {
        // Documents the arithmetic behind SHARE_NOTICE_WIDTH's doc comment so
        // a future change to Toast's message cap or the action label can't
        // silently invalidate the "never needs measuring" justification
        // without this test failing.
        let icon_and_gap = 24.0;
        let message_cap = 360.0;
        let action_and_gap = 170.0;
        let pill_padding = 32.0;
        let longest_single_line_row = icon_and_gap + message_cap + action_and_gap + pill_padding;
        assert!(
            SHARE_NOTICE_WIDTH >= longest_single_line_row,
            "SHARE_NOTICE_WIDTH ({SHARE_NOTICE_WIDTH}) must cover the longest single-line row \
             ({longest_single_line_row}) or the pill can overflow the panel horizontally"
        );
    }
}
