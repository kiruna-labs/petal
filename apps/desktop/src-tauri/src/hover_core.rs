//! Platform-neutral hover-tab core, shared by the macOS hover tab
//! (`hover_tab.rs`) and the Windows port.
//!
//! Everything in this module is pure: payload types, tab geometry math,
//! hit-test classification, share-color bookkeeping, and the shared-window
//! border/overlay bookkeeping maps. It must compile on EVERY platform — no
//! AppKit/CoreGraphics/Win32 calls, no `SessionState` access outside the two
//! `is_shared` cfg branches. Platform-native seams (cursor position, window
//! enumeration + frames, panel placement, z-order) stay in the per-platform
//! callers; this module only ever consumes plain inputs and produces plain
//! outputs.
//!
//! `hover_tab.rs` re-exports these names (root + its macOS `platform` mod) so
//! the existing macOS call sites and tests keep working unchanged.

use std::collections::HashMap;
#[cfg(not(target_os = "macos"))]
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::sync_ext::MutexExt;

#[cfg(not(target_os = "macos"))]
use tauri::AppHandle;
#[cfg(target_os = "macos")]
use tauri::{AppHandle, Manager};

pub use crate::platform::cg::WindowFrame;

// =============================================================================
// Payload types (frontend wire contract — `ipc.ts` + the `hover-tab` route)
// =============================================================================

/// Payload for the `hover-tab-update` event.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverTabUpdate {
    pub window_id: u32,
    pub frame: WindowFrame,
    pub tab_x: f64,
    pub tab_y: f64,
    pub attachment: HoverTabAttachment,
    /// The app-wide normalized vertical position used for this presentation.
    /// `0` is the source top, `0.5` is center, and `1` is the source bottom.
    pub vertical_offset: f64,
    /// Whether this window is currently in the shared set — lets the pill
    /// render the correct share/unshare label immediately on hover, without
    /// waiting on a separate round trip.
    pub shared: bool,
    /// Display-style control semantics: true for monitor-backed full-display
    /// shares, false for ordinary HWND shares. Petal View regions are blocked
    /// from hover targeting and use their own title-bar controls.
    pub display_like: bool,
}

/// Visible share-state transition emitted at the capture lifecycle boundary.
/// Stop transitions are separate from hover updates so an in-flight start can
/// keep its optimistic pill state.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareStateChanged {
    pub window_id: u32,
    pub shared: bool,
}

// =============================================================================
// Constants
// =============================================================================

/// Label of the pre-created, always-present-but-hidden hover-tab webview
/// window (a SvelteKit route, not a raw HTML file — see `lib.rs`).
pub const HOVER_TAB_LABEL: &str = "hover-tab";
/// Native window title visible to platform window-list hit-testing.
pub const HOVER_TAB_WINDOW_TITLE: &str = "Hover Tab";

/// Fallback identity color for shared-window borders when an older caller does
/// not pass the frontend's resolved local identity color.
pub(crate) const DEFAULT_SHARE_COLOR: &str = "#f06cc9"; // --id-plum

/// The hover tab is a fixed 40x40 right-edge rail button. Its native panel
/// uses this size too; hiding DOM content alone is unsafe because transparent
/// webview pixels still intercept clicks.
pub const HOVER_TAB_COMPACT_WIDTH: f64 = 40.0;
pub const HOVER_TAB_COMPACT_HEIGHT: f64 = 40.0;
/// The legacy/default rail position: centered on the source window.
pub const DEFAULT_HOVER_TAB_VERTICAL_OFFSET: f64 = 0.5;
/// Motion beyond this distance changes a primary click into a drag.
pub const HOVER_TAB_DRAG_THRESHOLD_PX: f64 = 6.0;

pub(crate) const HOVER_TAB_CURSOR_SLOP_X: f64 = 8.0;
pub(crate) const HOVER_TAB_CURSOR_SLOP_Y: f64 = 8.0;
pub(crate) const HOVER_TAB_BRIDGE_TOP_PADDING: f64 = 32.0;
pub(crate) const HOVER_TAB_BRIDGE_WINDOW_OVERLAP: f64 = 24.0;
pub(crate) const HOVER_TAB_HIDE_GRACE_TICKS: u8 = 8;
/// #743: z-order reassert cadence (every Nth tick).
pub(crate) const ORDER_REASSERT_TICKS: u64 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HoverTabAttachment {
    Outside,
    Inset,
}

/// Closed command vocabulary for the native-to-web drag bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HoverTabDragPhase {
    Begin,
    Update,
    Commit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HoverTabDragSession {
    pub(crate) window_id: u32,
    pub(crate) original_offset: f64,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorBounds {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl MonitorBounds {
    pub const fn new(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverTabRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl HoverTabRect {
    pub const fn right(self) -> f64 {
        self.x + self.width
    }
    pub const fn bottom(self) -> f64 {
        self.y + self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverTabPresentation {
    pub window_id: u32,
    pub attachment: HoverTabAttachment,
    pub rect: HoverTabRect,
}

pub(crate) const fn hover_tab_panel_logical_size() -> (f64, f64) {
    (HOVER_TAB_COMPACT_WIDTH, HOVER_TAB_COMPACT_HEIGHT)
}

fn clamp_origin(origin: f64, min: f64, max: f64) -> f64 {
    if max < min {
        min
    } else {
        origin.clamp(min, max)
    }
}

/// Clamp persisted or previewed rail positions. Non-finite values fail safe
/// to the legacy center rather than poisoning native geometry.
pub fn normalize_hover_tab_vertical_offset(offset: f64) -> f64 {
    if offset.is_finite() {
        offset.clamp(0.0, 1.0)
    } else {
        DEFAULT_HOVER_TAB_VERTICAL_OFFSET
    }
}

/// Map a normalized rail position to the source-relative top-left of the
/// fixed square. The available travel is the source height minus the tab
/// height, so Top/Bottom keep the whole 40px surface inside the source span
/// before the monitor/work-area clamp is applied.
pub fn hover_tab_vertical_origin(frame: WindowFrame, height: f64, offset: f64) -> f64 {
    let travel = (frame.height as f64 - height).max(0.0);
    frame.y as f64 + travel * normalize_hover_tab_vertical_offset(offset)
}

/// Pure, monitor-bounded layout for the fixed right-edge button at a supplied
/// normalized vertical position. The tab sits outside a target when the
/// adjacent 40px slot fits; otherwise it is inset into the target's right
/// edge. No presentation state changes its geometry.
pub fn hover_tab_presentation_with_offset(
    window_id: u32,
    frame: WindowFrame,
    monitor: MonitorBounds,
    vertical_offset: f64,
) -> HoverTabPresentation {
    let width = HOVER_TAB_COMPACT_WIDTH;
    let height = HOVER_TAB_COMPACT_HEIGHT;
    let frame_right = frame.x as f64 + frame.width as f64;
    let outside = frame_right >= monitor.left && frame_right + width <= monitor.right;
    let attachment = if outside {
        HoverTabAttachment::Outside
    } else {
        HoverTabAttachment::Inset
    };
    let raw_x = if outside {
        frame_right
    } else {
        frame_right - width
    };
    let x = clamp_origin(
        raw_x,
        monitor.left,
        (monitor.right - width).max(monitor.left),
    );
    let raw_y = hover_tab_vertical_origin(frame, height, vertical_offset);
    let y = clamp_origin(
        raw_y,
        monitor.top,
        (monitor.bottom - height).max(monitor.top),
    );
    HoverTabPresentation {
        window_id,
        attachment,
        rect: HoverTabRect {
            x,
            y,
            width,
            height,
        },
    }
}

/// Center-compatible façade retained for existing callers and fixtures.
pub fn hover_tab_presentation(
    window_id: u32,
    frame: WindowFrame,
    monitor: MonitorBounds,
) -> HoverTabPresentation {
    hover_tab_presentation_with_offset(window_id, frame, monitor, DEFAULT_HOVER_TAB_VERTICAL_OFFSET)
}

// =============================================================================
// Shared-window state + border bookkeeping
// =============================================================================

pub(crate) struct ShareState {
    #[cfg(not(target_os = "macos"))]
    /// Non-macOS stubs keep a local toggle set; the real Windows hover-tab
    /// command uses the platform `SessionState` capture path instead.
    pub(crate) shared: HashSet<u32>,
    /// window_id -> border_id, so unsharing can find + hide the right border
    /// panel. A fresh `border_id` is minted per share (see `share_border`'s
    /// registry), decoupling border lifecycle from window_id reuse.
    pub(crate) borders: HashMap<u32, u32>,
    /// window_id -> overlay_id for the click-through sharer-side surface that
    /// renders remote telepointers and draw strokes on the owner's real
    /// shared window (#196).
    pub(crate) overlays: HashMap<u32, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnsureBorderResult {
    Created(u32),
    Existing(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnsureOverlayResult {
    Created(u32),
    Existing(u32),
}

impl ShareState {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(not(target_os = "macos"))]
            shared: HashSet::new(),
            borders: HashMap::new(),
            overlays: HashMap::new(),
        }
    }

    pub(crate) fn ensure_border(
        &mut self,
        window_id: u32,
        create_border: impl FnOnce() -> u32,
    ) -> EnsureBorderResult {
        if let Some(border_id) = self.borders.get(&window_id) {
            return EnsureBorderResult::Existing(*border_id);
        }

        let border_id = create_border();
        self.borders.insert(window_id, border_id);
        EnsureBorderResult::Created(border_id)
    }

    pub(crate) fn remove_border(&mut self, window_id: u32) -> Option<u32> {
        self.borders.remove(&window_id)
    }

    pub(crate) fn drain_borders(&mut self) -> Vec<u32> {
        self.borders
            .drain()
            .map(|(_, border_id)| border_id)
            .collect()
    }

    pub(crate) fn ensure_overlay(
        &mut self,
        window_id: u32,
        create_overlay: impl FnOnce() -> u32,
    ) -> EnsureOverlayResult {
        if let Some(overlay_id) = self.overlays.get(&window_id) {
            return EnsureOverlayResult::Existing(*overlay_id);
        }

        let overlay_id = create_overlay();
        self.overlays.insert(window_id, overlay_id);
        EnsureOverlayResult::Created(overlay_id)
    }

    pub(crate) fn remove_overlay(&mut self, window_id: u32) -> Option<u32> {
        self.overlays.remove(&window_id)
    }

    pub(crate) fn drain_overlays(&mut self) -> Vec<u32> {
        self.overlays
            .drain()
            .map(|(_, overlay_id)| overlay_id)
            .collect()
    }
}

static SHARE_STATE: Mutex<Option<ShareState>> = Mutex::new(None);
static LAST_HOVER_UPDATE: Mutex<Option<HoverTabUpdate>> = Mutex::new(None);
static CURRENT_HOVER_PRESENTATION: Mutex<Option<HoverTabPresentation>> = Mutex::new(None);
static LAST_SHARE_COLOR: Mutex<Option<String>> = Mutex::new(None);
static HOVER_TAB_PRESENTATION_GENERATION: AtomicU64 = AtomicU64::new(0);
static HOVER_TAB_DRAG_SESSION: Mutex<Option<HoverTabDragSession>> = Mutex::new(None);

pub(crate) fn with_share_state<R>(f: impl FnOnce(&mut ShareState) -> R) -> R {
    let mut guard = SHARE_STATE.lock_unpoisoned();
    let state = guard.get_or_insert_with(ShareState::new);
    f(state)
}

#[cfg(target_os = "macos")]
pub(crate) fn is_shared(app: &AppHandle, window_id: u32) -> bool {
    app.try_state::<crate::session::SessionState>()
        .map(|state| state.is_share_active(window_id))
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn is_shared(app: &AppHandle, window_id: u32) -> bool {
    let _ = app;
    with_share_state(|s| s.shared.contains(&window_id))
}

pub(crate) fn set_last_hover_update(update: Option<HoverTabUpdate>) {
    let presentation = update.as_ref().map(|update| HoverTabPresentation {
        window_id: update.window_id,
        attachment: update.attachment,
        rect: HoverTabRect {
            x: update.tab_x,
            y: update.tab_y,
            width: HOVER_TAB_COMPACT_WIDTH,
            height: HOVER_TAB_COMPACT_HEIGHT,
        },
    });
    *CURRENT_HOVER_PRESENTATION.lock_unpoisoned() = presentation;
    *LAST_HOVER_UPDATE.lock_unpoisoned() = update;
}

pub(crate) fn current_hover_presentation() -> Option<HoverTabPresentation> {
    CURRENT_HOVER_PRESENTATION.lock_unpoisoned().clone()
}

pub(crate) fn last_hover_update() -> Option<HoverTabUpdate> {
    LAST_HOVER_UPDATE.lock_unpoisoned().clone()
}

/// Start a new native presentation generation. Queued AppKit/Win32 work must
/// check this value before mutating or revealing the singleton panel, because
/// a hide or a newer target can overtake an older main-thread dispatch.
pub(crate) fn begin_hover_tab_presentation() -> u64 {
    HOVER_TAB_PRESENTATION_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

pub(crate) fn hover_tab_presentation_generation() -> u64 {
    HOVER_TAB_PRESENTATION_GENERATION.load(Ordering::Acquire)
}

/// Begin one drag against the currently presented target. Incrementing the
/// native presentation generation invalidates queued placement from before the
/// gesture; platform adapters still validate the target/token before moving.
pub(crate) fn begin_hover_tab_drag(window_id: u32) -> Result<HoverTabDragSession, String> {
    let mut guard = HOVER_TAB_DRAG_SESSION.lock_unpoisoned();
    if let Some(active) = *guard {
        if active.window_id == window_id {
            return Ok(active);
        }
        return Err("another hover-tab drag is already active".to_string());
    }
    let Some(update) = LAST_HOVER_UPDATE.lock_unpoisoned().clone() else {
        return Err("hover-tab target is no longer presented".to_string());
    };
    if update.window_id != window_id {
        return Err("hover-tab target changed before drag began".to_string());
    }
    let session = HoverTabDragSession {
        window_id,
        original_offset: crate::share_priority::current_hover_tab_vertical_offset(),
        generation: begin_hover_tab_presentation(),
    };
    *guard = Some(session);
    Ok(session)
}

pub(crate) fn active_hover_tab_drag(window_id: u32) -> Option<HoverTabDragSession> {
    HOVER_TAB_DRAG_SESSION
        .lock_unpoisoned()
        .as_ref()
        .copied()
        .filter(|session| session.window_id == window_id)
}

pub(crate) fn finish_hover_tab_drag(window_id: u32) -> Option<HoverTabDragSession> {
    let mut guard = HOVER_TAB_DRAG_SESSION.lock_unpoisoned();
    if guard
        .as_ref()
        .is_some_and(|session| session.window_id == window_id)
    {
        guard.take()
    } else {
        None
    }
}

pub(crate) fn clear_hover_tab_drag() -> Option<HoverTabDragSession> {
    HOVER_TAB_DRAG_SESSION.lock_unpoisoned().take()
}

// =============================================================================
// Pure tab geometry + interaction regions
// =============================================================================

/// Pure interaction-region math for the actual native hover-tab panel. The
/// slop is intentionally applied to the real 40px rectangle so transparent
/// pixels cannot create a larger hidden hit region over the source application.
pub fn cursor_over_tab(cursor: (f64, f64), rect: HoverTabRect) -> bool {
    let (cx, cy) = cursor;
    cx >= rect.x - HOVER_TAB_CURSOR_SLOP_X
        && cx < rect.right() + HOVER_TAB_CURSOR_SLOP_X
        && cy >= rect.y - HOVER_TAB_CURSOR_SLOP_Y
        && cy < rect.bottom() + HOVER_TAB_CURSOR_SLOP_Y
}

pub(crate) fn cursor_in_hover_tab_bridge(
    cursor: (f64, f64),
    rect: HoverTabRect,
    frame: WindowFrame,
) -> bool {
    let (cx, cy) = cursor;
    let frame_left = frame.x as f64;
    let frame_top = frame.y as f64;
    let frame_right = frame_left + frame.width as f64;
    let frame_bottom = frame_top + frame.height as f64;
    let min_x = rect.x.min(frame_left) - HOVER_TAB_CURSOR_SLOP_X;
    let max_x = rect.right().max(frame_right) + HOVER_TAB_CURSOR_SLOP_X;
    let min_y = rect.y.min(frame_top) - HOVER_TAB_BRIDGE_TOP_PADDING;
    let max_y = rect.bottom().max(frame_bottom) + HOVER_TAB_BRIDGE_WINDOW_OVERLAP;
    cx >= min_x && cx < max_x && cy >= min_y && cy < max_y
}

pub(crate) fn hold_hover_tab_through_transient_miss(
    cursor: Option<(f64, f64)>,
    rect: Option<HoverTabRect>,
    last: Option<(WindowFrame, u32)>,
    missed_bridge_ticks: u8,
) -> bool {
    if missed_bridge_ticks >= HOVER_TAB_HIDE_GRACE_TICKS {
        return false;
    }
    let (Some(cursor), Some(rect), Some((frame, _))) = (cursor, rect, last) else {
        return false;
    };
    cursor_in_hover_tab_bridge(cursor, rect, frame)
}

pub(crate) fn same_hit(a: &Option<(WindowFrame, u32)>, b: &Option<(WindowFrame, u32)>) -> bool {
    match (a, b) {
        (Some((a_frame, a_id)), Some((b_frame, b_id))) => {
            a_id == b_id
                && a_frame.x == b_frame.x
                && a_frame.y == b_frame.y
                && a_frame.width == b_frame.width
                && a_frame.height == b_frame.height
        }
        (None, None) => true,
        _ => false,
    }
}

// =============================================================================
// Pure hit-test classification
// =============================================================================

#[derive(Debug, Clone, Copy)]
pub(crate) struct HoverStackEntry {
    pub(crate) number: i64,
    pub(crate) owner_pid: i64,
}

pub(crate) fn hover_tab_needs_reorder(
    stack: &[HoverStackEntry],
    tab_number: i64,
    target_number: i64,
    self_pid: i64,
) -> bool {
    if tab_number <= 0 || target_number <= 0 {
        return true;
    }

    let relevant_stack: Vec<&HoverStackEntry> = stack
        .iter()
        .filter(|entry| {
            entry.number == tab_number
                || entry.number == target_number
                || entry.owner_pid != self_pid
        })
        .collect();
    let Some(target_idx) = relevant_stack
        .iter()
        .position(|entry| entry.number == target_number)
    else {
        return false;
    };
    let Some(tab_idx) = relevant_stack
        .iter()
        .position(|entry| entry.number == tab_number)
    else {
        return true;
    };

    tab_idx + 1 != target_idx
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HoverWindowSnapshot<'a> {
    pub(crate) number: i64,
    pub(crate) layer: i64,
    pub(crate) owner_pid: i64,
    pub(crate) owner_bundle_id: Option<&'a str>,
    /// Whether this is one of Petal's OWN decorative panels
    /// (share-border/overlay/hover-tab) that the hit-test skips, vs own
    /// content (main window, remote renders) that blocks.
    pub(crate) decorative: bool,
    pub(crate) region_selector: bool,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum HoverHitTestDecision {
    NoHit,
    BlockedByOwnProcess,
    BlockedByExternalWindow,
    ShareableCandidate { window_id: u32, frame: WindowFrame },
}

pub(crate) fn hover_hit_test_decision<'a>(
    windows: impl IntoIterator<Item = HoverWindowSnapshot<'a>>,
    cursor: (f64, f64),
    self_pid: i64,
) -> HoverHitTestDecision {
    const MIN_WINDOW_SIDE: f64 = 40.0;

    for entry in windows {
        // Petal View is an always-on-top hollow selector on macOS. It is a
        // blocker, not a share candidate: its interior is click-through at the
        // native boundary, so allowing this entry through would expose the
        // underlying desktop window to the hover tab.
        if entry.region_selector {
            if entry.w >= MIN_WINDOW_SIDE
                && entry.h >= MIN_WINDOW_SIDE
                && cursor.0 >= entry.x
                && cursor.0 < entry.x + entry.w
                && cursor.1 >= entry.y
                && cursor.1 < entry.y + entry.h
            {
                return HoverHitTestDecision::BlockedByOwnProcess;
            }
            continue;
        }
        if cursor.0 < entry.x
            || cursor.0 >= entry.x + entry.w
            || cursor.1 < entry.y
            || cursor.1 >= entry.y + entry.h
        {
            continue;
        }

        let decision = crate::share_target::classify(&crate::share_target::mac_window_facts(
            entry.layer,
            entry.w,
            entry.h,
            entry.owner_pid,
            self_pid,
            entry.owner_bundle_id,
            entry.region_selector,
            entry.owner_pid == self_pid && entry.decorative,
        ));
        match decision {
            crate::share_target::ShareTargetDecision::Eligible(
                crate::share_target::ShareTargetKind::Window,
            ) => {
                let frame = WindowFrame {
                    x: entry.x.round() as i32,
                    y: entry.y.round() as i32,
                    width: entry.w.round() as i32,
                    height: entry.h.round() as i32,
                };
                let Ok(window_id) = u32::try_from(entry.number) else {
                    return HoverHitTestDecision::BlockedByExternalWindow;
                };
                return HoverHitTestDecision::ShareableCandidate { window_id, frame };
            }
            crate::share_target::ShareTargetDecision::Eligible(
                crate::share_target::ShareTargetKind::RegisteredRegion,
            ) => return HoverHitTestDecision::BlockedByOwnProcess,
            crate::share_target::ShareTargetDecision::Rejected(
                crate::share_target::ShareTargetRejection::NonNormalLayer
                | crate::share_target::ShareTargetRejection::DenylistedBundle
                | crate::share_target::ShareTargetRejection::TooSmall,
            ) => continue,
            crate::share_target::ShareTargetDecision::Rejected(
                crate::share_target::ShareTargetRejection::PetalChrome,
            ) if entry.decorative => continue,
            crate::share_target::ShareTargetDecision::Rejected(
                crate::share_target::ShareTargetRejection::OwnPetalWindow
                | crate::share_target::ShareTargetRejection::PetalChrome,
            ) => return HoverHitTestDecision::BlockedByOwnProcess,
            crate::share_target::ShareTargetDecision::Rejected(_)
                if entry.owner_pid == self_pid =>
            {
                return HoverHitTestDecision::BlockedByOwnProcess;
            }
            crate::share_target::ShareTargetDecision::Rejected(_) => {
                return HoverHitTestDecision::BlockedByExternalWindow;
            }
        }
    }
    HoverHitTestDecision::NoHit
}

/// The own-chrome title check shared by the macOS projection and the
/// registry's `NameChromeOracle` so both compute `decorative` identically
/// (§9.9). A window is decorative only if it is OURS *and* one of these.
pub(crate) fn is_own_chrome_title(name: &str) -> bool {
    name == crate::share_border::SHARE_BORDER_WINDOW_TITLE
        || name == crate::share_overlay::SHARE_OVERLAY_WINDOW_TITLE
        || name == HOVER_TAB_WINDOW_TITLE
        || name == HOVER_TAB_LABEL
}

// =============================================================================
// Share color bookkeeping
// =============================================================================

pub(crate) fn normalize_share_color(color: &str) -> Option<String> {
    let color = color.trim();
    let valid_hex = color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|c| c.is_ascii_hexdigit());
    if valid_hex {
        Some(color.to_string())
    } else {
        None
    }
}

pub(crate) fn remember_share_color(color: &str) {
    *LAST_SHARE_COLOR.lock_unpoisoned() = Some(color.to_string());
}

pub(crate) fn remembered_share_color() -> Option<String> {
    LAST_SHARE_COLOR.lock_unpoisoned().clone()
}

pub(crate) fn autotest_share_color() -> Option<String> {
    std::env::var("PETAL_AUTOTEST_SHARE_COLOR")
        .ok()
        .and_then(|color| normalize_share_color(&color))
}

pub(crate) fn share_color_or_default(color: Option<&str>) -> String {
    if let Some(color) = color {
        if let Some(color) = normalize_share_color(color) {
            remember_share_color(&color);
            return color;
        }
        return DEFAULT_SHARE_COLOR.to_string();
    }

    remembered_share_color()
        .or_else(autotest_share_color)
        .unwrap_or_else(|| DEFAULT_SHARE_COLOR.to_string())
}

// =============================================================================
// Tests — pure, run on every platform
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_panel_is_the_only_native_size() {
        assert_eq!(hover_tab_panel_logical_size(), (40.0, 40.0));
        assert_eq!(HOVER_TAB_COMPACT_WIDTH, 40.0);
        assert_eq!(HOVER_TAB_COMPACT_HEIGHT, 40.0);
    }

    #[test]
    fn attachment_serializes_as_outside_or_inset() {
        assert_eq!(
            serde_json::to_value(HoverTabAttachment::Outside).unwrap(),
            "outside"
        );
        assert_eq!(
            serde_json::to_value(HoverTabAttachment::Inset).unwrap(),
            "inset"
        );
    }

    #[test]
    fn every_window_gets_the_same_right_center_compact_square() {
        let frame = WindowFrame {
            x: 300,
            y: 400,
            width: 500,
            height: 300,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 1200.0, 800.0);
        let compact = hover_tab_presentation(7, frame, monitor);
        assert_eq!(compact.attachment, HoverTabAttachment::Outside);
        assert_eq!(
            compact.rect,
            HoverTabRect {
                x: 800.0,
                y: 530.0,
                width: 40.0,
                height: 40.0
            }
        );
    }

    #[test]
    fn normalized_offsets_follow_source_top_center_and_bottom() {
        let frame = WindowFrame {
            x: 300,
            y: 100,
            width: 500,
            height: 300,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 1200.0, 800.0);
        let top = hover_tab_presentation_with_offset(7, frame, monitor, 0.0);
        let center = hover_tab_presentation_with_offset(7, frame, monitor, 0.5);
        let bottom = hover_tab_presentation_with_offset(7, frame, monitor, 1.0);
        assert_eq!(top.rect.y, 100.0);
        assert_eq!(center.rect.y, 230.0);
        assert_eq!(bottom.rect.y, 360.0);
        assert!(top.rect.y < center.rect.y && center.rect.y < bottom.rect.y);
    }

    #[test]
    fn normalized_offsets_clamp_and_non_finite_values_fail_safe_to_center() {
        let frame = WindowFrame {
            x: 100,
            y: 100,
            width: 400,
            height: 200,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 800.0, 600.0);
        assert_eq!(normalize_hover_tab_vertical_offset(-1.0), 0.0);
        assert_eq!(normalize_hover_tab_vertical_offset(2.0), 1.0);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::NAN), 0.5);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::INFINITY), 0.5);
        assert_eq!(
            hover_tab_presentation_with_offset(1, frame, monitor, -1.0)
                .rect
                .y,
            hover_tab_presentation_with_offset(1, frame, monitor, 0.0)
                .rect
                .y
        );
        assert_eq!(
            hover_tab_presentation_with_offset(1, frame, monitor, f64::NAN)
                .rect
                .y,
            hover_tab_presentation_with_offset(1, frame, monitor, 0.5)
                .rect
                .y
        );
    }

    #[test]
    fn short_and_partially_offscreen_sources_remain_monitor_bounded() {
        let short = WindowFrame {
            x: -20,
            y: -30,
            width: 80,
            height: 20,
        };
        let monitor = MonitorBounds::new(-100.0, -50.0, 500.0, 400.0);
        for offset in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let presentation = hover_tab_presentation_with_offset(1, short, monitor, offset);
            assert!(presentation.rect.y >= monitor.top);
            assert!(presentation.rect.bottom() <= monitor.bottom);
            assert_eq!(presentation.rect.height, HOVER_TAB_COMPACT_HEIGHT);
        }

        let partial = WindowFrame {
            x: 450,
            y: 350,
            width: 200,
            height: 300,
        };
        let presentation = hover_tab_presentation_with_offset(2, partial, monitor, 1.0);
        assert!(presentation.rect.right() <= monitor.right);
        assert!(presentation.rect.bottom() <= monitor.bottom);
    }

    #[test]
    fn normalized_offset_does_not_change_right_edge_attachment_policy() {
        let ordinary = WindowFrame {
            x: 100,
            y: 100,
            width: 400,
            height: 200,
        };
        let edge = WindowFrame {
            x: 0,
            y: 100,
            width: 800,
            height: 400,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 800.0, 600.0);
        for offset in [0.0, 0.5, 1.0] {
            assert_eq!(
                hover_tab_presentation_with_offset(1, ordinary, monitor, offset).attachment,
                HoverTabAttachment::Outside
            );
            assert_eq!(
                hover_tab_presentation_with_offset(1, edge, monitor, offset).attachment,
                HoverTabAttachment::Inset
            );
        }
    }

    #[test]
    fn monitor_edge_uses_inset_attachment_and_fixed_square() {
        let frame = WindowFrame {
            x: 0,
            y: 26,
            width: 1920,
            height: 1054,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 1920.0, 1080.0);
        let presentation = hover_tab_presentation(7, frame, monitor);
        assert_eq!(presentation.attachment, HoverTabAttachment::Inset);
        assert_eq!(presentation.rect.x, 1880.0);
        assert_eq!(presentation.rect.width, HOVER_TAB_COMPACT_WIDTH);
        assert_eq!(presentation.rect.height, HOVER_TAB_COMPACT_HEIGHT);
        assert!(presentation.rect.right() <= monitor.right);
    }

    #[test]
    fn negative_monitor_and_short_work_area_remain_bounded() {
        let frame = WindowFrame {
            x: -1100,
            y: -880,
            width: 80,
            height: 60,
        };
        let monitor = MonitorBounds::new(-1200.0, -900.0, 0.0, 0.0);
        let presentation = hover_tab_presentation(7, frame, monitor);
        assert!(presentation.rect.x >= monitor.left);
        assert!(presentation.rect.right() <= monitor.right);
        assert!(presentation.rect.y >= monitor.top);
        assert!(presentation.rect.bottom() <= monitor.bottom);
        assert_eq!(presentation.rect.width, HOVER_TAB_COMPACT_WIDTH);
        assert_eq!(presentation.rect.height, HOVER_TAB_COMPACT_HEIGHT);
    }

    #[test]
    fn narrow_left_edge_window_can_use_the_adjacent_fixed_slot() {
        let frame = WindowFrame {
            x: 0,
            y: 200,
            width: 80,
            height: 200,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 1200.0, 800.0);
        let presentation = hover_tab_presentation(7, frame, monitor);
        assert_eq!(presentation.attachment, HoverTabAttachment::Outside);
        assert_eq!(presentation.rect.x, 80.0);
        assert_eq!(presentation.rect.width, HOVER_TAB_COMPACT_WIDTH);
    }

    #[test]
    fn bridge_and_grace_use_the_actual_right_edge_rect() {
        let frame = WindowFrame {
            x: 300,
            y: 400,
            width: 500,
            height: 300,
        };
        let monitor = MonitorBounds::new(0.0, 0.0, 1200.0, 800.0);
        let presentation = hover_tab_presentation(7, frame, monitor);
        assert!(cursor_in_hover_tab_bridge(
            (799.0, 550.0),
            presentation.rect,
            frame
        ));
        assert!(hold_hover_tab_through_transient_miss(
            Some((799.0, 550.0)),
            Some(presentation.rect),
            Some((frame, 7)),
            0
        ));
        assert!(!hold_hover_tab_through_transient_miss(
            Some((799.0, 550.0)),
            Some(presentation.rect),
            Some((frame, 7)),
            HOVER_TAB_HIDE_GRACE_TICKS
        ));
    }

    #[test]
    fn own_petal_content_blocks_but_foreign_window_is_a_candidate() {
        let foreign = HoverWindowSnapshot {
            number: 1,
            layer: 0,
            owner_pid: 100,
            owner_bundle_id: None,
            decorative: false,
            region_selector: false,
            x: 0.0,
            y: 0.0,
            w: 500.0,
            h: 400.0,
        };
        let own = HoverWindowSnapshot {
            owner_pid: 999,
            number: 2,
            ..foreign
        };
        assert!(matches!(
            hover_hit_test_decision([foreign], (10.0, 10.0), 999),
            HoverHitTestDecision::ShareableCandidate { window_id: 1, .. }
        ));
        assert_eq!(
            hover_hit_test_decision([own], (10.0, 10.0), 999),
            HoverHitTestDecision::BlockedByOwnProcess
        );
    }

    #[test]
    fn cursor_over_tab_uses_actual_compact_rectangle() {
        let rect = HoverTabRect {
            x: 100.0,
            y: 200.0,
            width: HOVER_TAB_COMPACT_WIDTH,
            height: HOVER_TAB_COMPACT_HEIGHT,
        };
        assert!(cursor_over_tab((100.0, 200.0), rect));
        assert!(cursor_over_tab(
            (100.0 + HOVER_TAB_COMPACT_WIDTH - 1.0, 200.0),
            rect
        ));
        assert!(!cursor_over_tab(
            (
                100.0 + HOVER_TAB_COMPACT_WIDTH + HOVER_TAB_CURSOR_SLOP_X,
                200.0
            ),
            rect
        ));
        assert!(!cursor_over_tab(
            (100.0, 200.0 - HOVER_TAB_CURSOR_SLOP_Y - 1.0),
            rect
        ));
    }

    #[test]
    fn hover_tab_reorder_needed_only_when_out_of_order() {
        let stack = [
            HoverStackEntry {
                number: 10,
                owner_pid: 100,
            },
            HoverStackEntry {
                number: 20,
                owner_pid: 200,
            },
        ];
        assert!(!hover_tab_needs_reorder(&stack, 10, 20, 300));
        assert!(hover_tab_needs_reorder(&stack, 20, 10, 300));
    }

    #[test]
    fn hit_test_finds_shareable_candidate_and_blocks_own_content() {
        let snapshots = [
            HoverWindowSnapshot {
                number: 1,
                layer: 0,
                owner_pid: 100,
                owner_bundle_id: None,
                decorative: false,
                region_selector: false,
                x: 0.0,
                y: 0.0,
                w: 500.0,
                h: 400.0,
            },
            HoverWindowSnapshot {
                number: 2,
                layer: 0,
                owner_pid: 999,
                owner_bundle_id: None,
                decorative: false,
                region_selector: false,
                x: 600.0,
                y: 0.0,
                w: 500.0,
                h: 400.0,
            },
        ];
        assert_eq!(
            hover_hit_test_decision(snapshots.iter().copied(), (100.0, 100.0), 999),
            HoverHitTestDecision::ShareableCandidate {
                window_id: 1,
                frame: WindowFrame {
                    x: 0,
                    y: 0,
                    width: 500,
                    height: 400
                },
            }
        );
        assert_eq!(
            hover_hit_test_decision(snapshots.iter().copied(), (700.0, 100.0), 999),
            HoverHitTestDecision::BlockedByOwnProcess
        );
        assert_eq!(
            hover_hit_test_decision(snapshots.iter().copied(), (500.0, 500.0), 999),
            HoverHitTestDecision::NoHit
        );
    }

    #[test]
    fn petal_view_region_blocks_underlying_shareable_windows_even_with_stale_owner() {
        let region = HoverWindowSnapshot {
            number: 42,
            layer: 3,
            owner_pid: 999,
            owner_bundle_id: None,
            decorative: false,
            region_selector: true,
            x: 10.0,
            y: 20.0,
            w: 640.0,
            h: 400.0,
        };
        let underlying = HoverWindowSnapshot {
            number: 7,
            layer: 0,
            owner_pid: 123,
            owner_bundle_id: None,
            decorative: false,
            region_selector: false,
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 800.0,
        };
        assert_eq!(
            hover_hit_test_decision([region, underlying], (100.0, 100.0), 999),
            HoverHitTestDecision::BlockedByOwnProcess
        );
        let stale_owner = HoverWindowSnapshot {
            owner_pid: 123,
            ..region
        };
        assert_eq!(
            hover_hit_test_decision([stale_owner, underlying], (100.0, 100.0), 999),
            HoverHitTestDecision::BlockedByOwnProcess
        );
        assert_eq!(
            hover_hit_test_decision([region, underlying], (900.0, 700.0), 999),
            HoverHitTestDecision::ShareableCandidate {
                window_id: 7,
                frame: WindowFrame {
                    x: 0,
                    y: 0,
                    width: 1000,
                    height: 800,
                },
            }
        );
    }

    #[test]
    fn overlapping_petal_view_regions_still_block_the_underlying_window() {
        let regions = [
            HoverWindowSnapshot {
                number: 42,
                layer: 3,
                owner_pid: 999,
                owner_bundle_id: None,
                decorative: false,
                region_selector: true,
                x: 10.0,
                y: 20.0,
                w: 300.0,
                h: 200.0,
            },
            HoverWindowSnapshot {
                number: 43,
                layer: 3,
                owner_pid: 999,
                owner_bundle_id: None,
                decorative: false,
                region_selector: true,
                x: 200.0,
                y: 100.0,
                w: 300.0,
                h: 200.0,
            },
        ];
        let underlying = HoverWindowSnapshot {
            number: 7,
            layer: 0,
            owner_pid: 123,
            owner_bundle_id: None,
            decorative: false,
            region_selector: false,
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 800.0,
        };
        assert_eq!(
            hover_hit_test_decision(regions.into_iter().chain([underlying]), (250.0, 150.0), 999),
            HoverHitTestDecision::BlockedByOwnProcess
        );
    }

    #[test]
    fn own_chrome_titles_are_decorative() {
        assert!(is_own_chrome_title(HOVER_TAB_LABEL));
        assert!(is_own_chrome_title(HOVER_TAB_WINDOW_TITLE));
        assert!(!is_own_chrome_title("Finder"));
    }

    #[test]
    fn share_color_normalization_and_remembering() {
        assert_eq!(normalize_share_color("#ff0000").as_deref(), Some("#ff0000"));
        assert_eq!(
            normalize_share_color("  #0aBcDf  ").as_deref(),
            Some("#0aBcDf")
        );
        assert_eq!(normalize_share_color("red"), None);
        assert_eq!(normalize_share_color("#ff00"), None);
        remember_share_color("#123456");
        assert_eq!(remembered_share_color().as_deref(), Some("#123456"));
        assert_eq!(share_color_or_default(None), "#123456");
        assert_eq!(share_color_or_default(Some("nope")), DEFAULT_SHARE_COLOR);
        assert_eq!(share_color_or_default(Some("#abcdef")), "#abcdef");
    }

    #[test]
    fn same_hit_compares_frame_and_id() {
        let a = Some((
            WindowFrame {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            5u32,
        ));
        assert!(same_hit(&a, &a));
        assert!(!same_hit(
            &a,
            &Some((
                WindowFrame {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 5,
                },
                5,
            ))
        ));
        assert!(same_hit(&None, &None));
        assert!(!same_hit(&a, &None));
    }
}
