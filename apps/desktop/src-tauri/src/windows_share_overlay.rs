//! Windows sharer-side telepointer surface — mirrors macOS `share_overlay.rs`.
//!
//! Receiver compositor windows already host `compositor/pointer.html` above
//! the remote video content. The local sharer had no equivalent render surface
//! over its real app window, so participants' name-tagged cursors over a
//! locally shared window were received and then dropped. This module provides
//! that surface: a transparent, click-through, non-activating overlay per
//! locally shared window, hosting the existing `compositor/pointer` route with
//! `?surface=sharer`, so the sharer sees who is pointing at (and about to
//! control) which part of its own window. For ordinary window/display shares
//! it can also paint Petal's local 4px identity border; that mode is enabled
//! only after Windows borderless consent and display capture exclusion are
//! both ready.
//!
//! The pointer overlay must track the shared window's on-screen rect in
//! physical pixels and follow it on move/resize. The telepointer sender's
//! 9Hz frame refresh already walks the same windows, but repositioning an
//! overlay here keeps the sharer surface honest even before the share-frame
//! convention is canonicalized (that deferred oscillation work).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::sync_ext::MutexExt;
use crate::windows_capture_target::TargetKind;
use crate::windows_screen_capture::CaptureIndicatorMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShareOverlayReadiness {
    pub shown: bool,
    pub capture_excluded: bool,
    pub custom_indicator_ready: bool,
}

type OverlayFrame = crate::platform::cg::WindowFrame;

#[derive(Debug, Clone, Copy, PartialEq)]
struct AppliedOverlayState {
    frame: Option<OverlayFrame>,
    visible: bool,
    z_ordered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayStackingMode {
    /// A same-integrity ordinary window uses the source HWND as its Win32
    /// owner, which gives the native window manager the required stacking.
    SourceOwned,
    /// Higher-integrity or integrity-unknown sources get only a passive,
    /// unowned telepointer surface. WGC remains the authoritative indicator.
    Passive,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OverlayPlacementAction {
    Noop,
    MoveOnly { frame: OverlayFrame },
    ResizeOnly { frame: OverlayFrame },
    GeometryOnly { frame: OverlayFrame },
    DisplayFrameAndTopmost { frame: OverlayFrame },
    DisplayTopmostOnly,
    Show { frame: OverlayFrame },
    Hide,
}

fn overlay_placement_action(
    applied: Option<AppliedOverlayState>,
    desired: AppliedOverlayState,
    target_kind: TargetKind,
    _stacking_mode: OverlayStackingMode,
) -> OverlayPlacementAction {
    if !desired.visible || desired.frame.is_none() {
        return if applied.is_some_and(|state| state.visible) {
            OverlayPlacementAction::Hide
        } else {
            OverlayPlacementAction::Noop
        };
    }
    let frame = desired.frame.expect("visible overlay must have a frame");
    let Some(applied) = applied else {
        return OverlayPlacementAction::Show { frame };
    };
    if !applied.visible {
        return OverlayPlacementAction::Show { frame };
    }
    if applied.frame != Some(frame) {
        let old = applied
            .frame
            .expect("visible applied overlay must have a frame");
        let moved = old.x != frame.x || old.y != frame.y;
        let resized = old.width != frame.width || old.height != frame.height;
        if target_kind == TargetKind::Display && !desired.z_ordered {
            return OverlayPlacementAction::DisplayFrameAndTopmost { frame };
        }
        return match (moved, resized) {
            (true, true) => OverlayPlacementAction::GeometryOnly { frame },
            (true, false) => OverlayPlacementAction::MoveOnly { frame },
            (false, true) => OverlayPlacementAction::ResizeOnly { frame },
            (false, false) => OverlayPlacementAction::Noop,
        };
    }
    if target_kind == TargetKind::Display && !desired.z_ordered {
        OverlayPlacementAction::DisplayTopmostOnly
    } else {
        OverlayPlacementAction::Noop
    }
}

/// ~9Hz was sufficient for telepointer refresh but is visibly late for a
/// native border. WinEvent callbacks wake the tracker immediately; this timer
/// is only a missed-event/display reconciliation safety net.
const RECONCILE_INTERVAL_MS: u32 = 250;
const TRACKER_RECONCILE_MESSAGE: u32 = 0x8000 + 1;
const TRACKER_RECONCILE_TIMER_ID: usize = 1;

/// A replacement indicator must have a loaded WebView before WGC's border is
/// suppressed. A hung local page therefore takes the safe system-indicator
/// path instead of leaving a blank transparent overlay on the desktop.
const OVERLAY_PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// window_id -> overlay webview label.
static OVERLAY_LABELS: LazyLock<Mutex<HashMap<u32, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// window_id -> overlay native HWND (recorded at creation; used by the tracker
/// thread to SetWindowPos synchronously with the shared window rect).
static OVERLAY_HWNDS: LazyLock<Mutex<HashMap<u32, isize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// window_id -> whether the sharer overlay currently owns pointer input for Draw.
static OVERLAY_DRAW_ACTIVE: LazyLock<Mutex<HashMap<u32, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Draw activation temporarily changes the native WebView visibility/style.
/// The tracker must not interpret that transition as a lost custom indicator.
static OVERLAY_DRAW_TRANSITIONING: LazyLock<Mutex<HashMap<u32, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Per-share indicator readiness retained so an idempotent create call cannot
/// accidentally claim a previously system-indicated overlay is custom-ready.
static OVERLAY_READINESS: LazyLock<Mutex<HashMap<u32, ShareOverlayReadiness>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Shares whose custom border was disabled after WGC rejected the borderless
/// property. The tracker must not immediately show them again on its next
/// geometry tick.
static OVERLAY_CAPTURE_FALLBACK: LazyLock<Mutex<HashMap<u32, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Passive elevated telepointers are auxiliary only. A native placement error
/// disables that surface without changing the already-safe WGC system mode.
static OVERLAY_PASSIVE_DISABLED: LazyLock<Mutex<HashMap<u32, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Last native state successfully applied to each overlay. Geometry updates are
/// compared here before touching Win32, so an unchanged source never receives
/// another move/resize/show call.
static OVERLAY_PLACEMENT: LazyLock<Mutex<HashMap<u32, AppliedOverlayState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Ordinary windows use a Win32 owner when integrity permits it. Elevated or
/// integrity-unknown sources use a passive unowned telepointer surface; WGC's
/// system border remains their authoritative indicator.
static OVERLAY_STACKING: LazyLock<Mutex<HashMap<u32, OverlayStackingMode>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TRACKER_STARTED: AtomicBool = AtomicBool::new(false);
/// HWND of the dedicated WinEvent/message-pump thread. Creation, close, and
/// native callbacks post one coalesced reconcile message here.
static TRACKER_HWND: AtomicUsize = AtomicUsize::new(0);
/// At most one reconcile message may be queued at a time. A move can emit
/// several WinEvents (and SetWindowPos emits overlay events of its own);
/// coalescing those notifications keeps the pump responsive without delaying
/// the latest geometry.
static TRACKER_RECONCILE_QUEUED: AtomicBool = AtomicBool::new(false);

pub(crate) fn share_overlay_label(window_id: u32) -> String {
    format!("petal-sharer-pointer-{window_id}")
}

/// Label of the sharer overlay for `window_id`, if one is currently registered
/// (i.e. this machine is sharing that window). The real telepointer receiver
/// uses this to render every participant's cursor over the sharer's own window.
pub(crate) fn labels_for_local_share(window_id: u32) -> Vec<String> {
    OVERLAY_LABELS
        .lock_unpoisoned()
        .get(&window_id)
        .cloned()
        .into_iter()
        .collect()
}

pub(crate) fn hwnd_for_local_share(window_id: u32) -> Option<isize> {
    OVERLAY_HWNDS.lock_unpoisoned().get(&window_id).copied()
}

pub(crate) fn is_draw_active(window_id: u32) -> bool {
    OVERLAY_DRAW_ACTIVE
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .unwrap_or(false)
}

#[tauri::command]
pub(crate) fn share_overlay_draw_active(window_id: u32) -> bool {
    is_draw_active(window_id)
}

pub(crate) fn set_draw_active(app: &AppHandle, window_id: u32, active: bool) -> Result<(), String> {
    if active
        && OVERLAY_CAPTURE_FALLBACK
            .lock_unpoisoned()
            .get(&window_id)
            .copied()
            .unwrap_or(false)
    {
        return Err(format!(
            "share overlay for window {window_id} is using the system capture indicator"
        ));
    }
    if active {
        let target = crate::windows_capture_target::resolve(window_id)
            .map_err(|_| format!("shared window {window_id} is no longer available"))?;
        if target.kind() == TargetKind::Window {
            let hwnd = windows::Win32::Foundation::HWND(target.raw_handle() as *mut _);
            if crate::windows_remote_control::window_integrity_exceeds_petal(hwnd)? {
                return Err(
                    "Draw is unavailable for windows running with higher privileges than Petal"
                        .to_string(),
                );
            }
        }
    }
    let label = labels_for_local_share(window_id)
        .into_iter()
        .next()
        .ok_or_else(|| format!("share overlay for window {window_id} is not open"))?;
    let window = app
        .get_webview_window(&label)
        .ok_or_else(|| format!("share overlay '{label}' is not open"))?;
    let previous = OVERLAY_DRAW_ACTIVE
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .unwrap_or(false);
    let restore = |window: &tauri::WebviewWindow| {
        let _ = window.set_ignore_cursor_events(!previous);
        if !previous {
            let _ = window.hide();
        }
        OVERLAY_DRAW_ACTIVE
            .lock_unpoisoned()
            .insert(window_id, previous);
    };

    OVERLAY_DRAW_TRANSITIONING
        .lock_unpoisoned()
        .insert(window_id, true);
    let result = (|| {
        window
            .set_ignore_cursor_events(!active)
            .map_err(|error| format!("set sharer overlay click-through failed: {error}"))?;
        if active {
            if let Err(error) = window.show() {
                restore(&window);
                return Err(format!("show sharer draw overlay failed: {error}"));
            }
            // WebView2 overlays created click-through do not reliably receive the
            // first pointer stream merely because their hit-test style changed.
            // Focus the overlay after making it interactive, matching the macOS
            // sharer path and ensuring the route can capture the initial stroke.
            if let Err(error) = window.set_focus() {
                restore(&window);
                return Err(format!("focus sharer draw overlay failed: {error}"));
            }
            // `set_focus` can briefly disturb an owned popup's visibility on
            // WebView2. Re-show after focus so the tracker sees the settled
            // interactive surface rather than a transient hidden state.
            if let Err(error) = window.show() {
                restore(&window);
                return Err(format!("re-show sharer draw overlay failed: {error}"));
            }
        }
        let active_json = if active { "true" } else { "false" };
        if let Err(error) = window.eval(format!("window.__petalDrawSetActive?.({active_json});")) {
            restore(&window);
            return Err(format!("sharer draw overlay eval failed: {error}"));
        }
        OVERLAY_DRAW_ACTIVE
            .lock_unpoisoned()
            .insert(window_id, active);
        Ok(())
    })();
    OVERLAY_DRAW_TRANSITIONING
        .lock_unpoisoned()
        .remove(&window_id);
    result
}

/// Frontend trigger for drawing on this machine's real shared window.
#[tauri::command]
pub(crate) fn share_overlay_set_draw_active(
    app: AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    active: bool,
) -> Result<(), String> {
    log::info!("windows share overlay: draw request window={window_id} active={active}");
    if !state.is_share_active(window_id) {
        let error = format!("window {window_id} is not actively shared");
        log::warn!("windows share overlay: draw request rejected: {error}");
        return Err(error);
    }
    let result = set_draw_active(&app, window_id, active);
    match &result {
        Ok(()) => log::info!(
            "windows share overlay: draw request applied window={window_id} active={active}"
        ),
        Err(error) => log::warn!(
            "windows share overlay: draw request failed window={window_id} active={active}: {error}"
        ),
    }
    result
}

/// Petal View's label-addressed Draw command uses the same native overlay
/// seam as the ordinary hover action without exposing the disposable token
/// beyond the region-window adapter.
pub(crate) fn set_region_draw_active(
    app: &AppHandle,
    window_id: u32,
    active: bool,
) -> Result<(), String> {
    set_draw_active(app, window_id, active)
}

fn overlay_frame_is_visible(frame: OverlayFrame, minimized: bool) -> bool {
    !minimized && frame.width > 1 && frame.height > 1
}

pub(crate) fn custom_indicator_is_ready(
    indicator_mode: CaptureIndicatorMode,
    region_share: bool,
    target_kind: TargetKind,
    owner_verified: bool,
    shown: bool,
    capture_excluded: bool,
) -> bool {
    indicator_mode == CaptureIndicatorMode::Petal
        && !region_share
        && (target_kind != TargetKind::Window || owner_verified)
        && shown
        && (target_kind != TargetKind::Display || capture_excluded)
}

fn region_overlay_frame(window_id: u32) -> Option<OverlayFrame> {
    let source = crate::region_window::resolve(window_id)?;
    let frame = OverlayFrame {
        x: source.frame.x.round() as i32,
        y: source.frame.y.round() as i32,
        width: source.frame.width.round() as i32,
        height: source.frame.height.round() as i32,
    };
    overlay_frame_is_visible(frame, false).then_some(frame)
}

fn overlay_frame(
    window_id: u32,
    raw_hwnd: usize,
    target_kind: TargetKind,
    region_share: bool,
) -> Option<OverlayFrame> {
    if region_share {
        return region_overlay_frame(window_id);
    }
    match target_kind {
        TargetKind::Window => crate::platform::windows::visible_window_frame(
            windows::Win32::Foundation::HWND(raw_hwnd as *mut _),
        ),
        TargetKind::Display => crate::platform::windows::display_frame_for_raw(raw_hwnd),
    }
}

fn window_is_topmost(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TOPMOST};
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 }
}

fn overlay_is_window(overlay_hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    let overlay = HWND(overlay_hwnd as *mut _);
    !overlay.0.is_null() && unsafe { IsWindow(Some(overlay)) }.as_bool()
}

fn overlay_is_visible(overlay_hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let overlay = HWND(overlay_hwnd as *mut _);
    overlay_is_window(overlay_hwnd) && unsafe { IsWindowVisible(overlay) }.as_bool()
}

fn source_owned_overlay_needs_fallback(
    custom_indicator_ready: bool,
    source_visible: bool,
    overlay_exists: bool,
    overlay_visible: bool,
    owner_matches: bool,
) -> bool {
    custom_indicator_ready
        && source_visible
        && (!overlay_exists || !overlay_visible || !owner_matches)
}

fn source_owned_overlay_fallback_allowed(
    window_id: u32,
    custom_indicator_ready: bool,
    source_visible: bool,
    overlay_exists: bool,
    overlay_visible: bool,
    owner_matches: bool,
) -> bool {
    !OVERLAY_DRAW_TRANSITIONING
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .unwrap_or(false)
        && source_owned_overlay_needs_fallback(
            custom_indicator_ready,
            source_visible,
            overlay_exists,
            overlay_visible,
            owner_matches,
        )
}

fn disable_passive_overlay(window_id: u32, overlay_hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    };
    OVERLAY_PASSIVE_DISABLED
        .lock_unpoisoned()
        .insert(window_id, true);
    if let Err(error) = set_overlay_window_pos(
        overlay_hwnd,
        None,
        None,
        SWP_HIDEWINDOW
            | SWP_NOACTIVATE
            | SWP_NOMOVE
            | SWP_NOSIZE
            | SWP_NOOWNERZORDER
            | SWP_NOZORDER,
        "passive-placement-failure",
    ) {
        log::warn!(
            "windows share overlay: passive telepointer could not be hidden window={window_id} overlay_hwnd={overlay_hwnd}: {error}"
        );
    }
}

fn overlay_owner_matches(overlay_hwnd: isize, source_hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindow, GW_OWNER};
    let overlay = HWND(overlay_hwnd as *mut _);
    let source = HWND(source_hwnd as *mut _);
    unsafe { GetWindow(overlay, GW_OWNER) }
        .ok()
        .is_some_and(|owner| owner.0 == source.0)
}

fn overlay_is_topmost(overlay_hwnd: isize) -> bool {
    let hwnd = windows::Win32::Foundation::HWND(overlay_hwnd as *mut _);
    window_is_topmost(hwnd)
}

fn desired_overlay_state(
    window_id: u32,
    overlay_hwnd: isize,
    raw_hwnd: usize,
    target_kind: TargetKind,
    stacking_mode: OverlayStackingMode,
    region_share: bool,
    source_frame: Option<OverlayFrame>,
) -> AppliedOverlayState {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    let source_hwnd = HWND(raw_hwnd as *mut _);
    let frame =
        source_frame.or_else(|| overlay_frame(window_id, raw_hwnd, target_kind, region_share));
    let minimized = target_kind == TargetKind::Window && unsafe { IsIconic(source_hwnd) }.as_bool();
    let visible = frame.is_some_and(|frame| overlay_frame_is_visible(frame, minimized));
    let z_ordered = if visible {
        match target_kind {
            TargetKind::Window => match stacking_mode {
                OverlayStackingMode::SourceOwned => {
                    overlay_is_window(overlay_hwnd)
                        && overlay_is_visible(overlay_hwnd)
                        && overlay_owner_matches(overlay_hwnd, source_hwnd.0 as isize)
                }
                // Passive overlays deliberately make no z-order claim. Their
                // system WGC border, not this telepointer HWND, is authoritative.
                OverlayStackingMode::Passive => true,
            },
            TargetKind::Display => overlay_is_topmost(overlay_hwnd),
        }
    } else {
        true
    };
    AppliedOverlayState {
        frame,
        visible,
        z_ordered,
    }
}

fn set_overlay_window_pos(
    overlay_hwnd: isize,
    insert_after: Option<isize>,
    frame: Option<OverlayFrame>,
    flags: windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS,
    anchor_description: &str,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
    let (x, y, width, height) = frame
        .map(|frame| (frame.x, frame.y, frame.width, frame.height))
        .unwrap_or((0, 0, 0, 0));
    let insert_after = insert_after.map(|raw| HWND(raw as *mut _));
    unsafe {
        SetWindowPos(
            HWND(overlay_hwnd as *mut _),
            insert_after,
            x,
            y,
            width,
            height,
            flags,
        )
        .map_err(|error| {
            format!(
                "SetWindowPos failed anchor={anchor_description} insert_after={insert_after:?}: {error}"
            )
        })
    }
}

fn apply_native_action(
    overlay_hwnd: isize,
    target_kind: TargetKind,
    action: OverlayPlacementAction,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
        SWP_NOZORDER, SWP_SHOWWINDOW,
    };
    let stable_flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    match action {
        OverlayPlacementAction::Noop => Ok(()),
        OverlayPlacementAction::MoveOnly { frame } => set_overlay_window_pos(
            overlay_hwnd,
            None,
            Some(frame),
            stable_flags | SWP_NOSIZE | SWP_NOZORDER,
            "none",
        ),
        OverlayPlacementAction::ResizeOnly { frame } => set_overlay_window_pos(
            overlay_hwnd,
            None,
            Some(frame),
            stable_flags | SWP_NOMOVE | SWP_NOZORDER,
            "none",
        ),
        OverlayPlacementAction::GeometryOnly { frame } => set_overlay_window_pos(
            overlay_hwnd,
            None,
            Some(frame),
            stable_flags | SWP_NOZORDER,
            "none",
        ),
        OverlayPlacementAction::DisplayFrameAndTopmost { frame } => {
            debug_assert_eq!(target_kind, TargetKind::Display);
            set_overlay_window_pos(
                overlay_hwnd,
                Some(HWND_TOPMOST.0 as isize),
                Some(frame),
                stable_flags,
                "display-topmost",
            )
        }
        OverlayPlacementAction::DisplayTopmostOnly => {
            debug_assert_eq!(target_kind, TargetKind::Display);
            set_overlay_window_pos(
                overlay_hwnd,
                Some(HWND_TOPMOST.0 as isize),
                None,
                stable_flags | SWP_NOMOVE | SWP_NOSIZE,
                "display-topmost",
            )
        }
        OverlayPlacementAction::Show { frame } => {
            let (insert_after, flags, anchor_description) = match target_kind {
                TargetKind::Window => (
                    None,
                    stable_flags | SWP_SHOWWINDOW | SWP_NOZORDER,
                    "ordinary-window",
                ),
                TargetKind::Display => (
                    Some(HWND_TOPMOST.0 as isize),
                    stable_flags | SWP_SHOWWINDOW,
                    "display-topmost",
                ),
            };
            set_overlay_window_pos(
                overlay_hwnd,
                insert_after,
                Some(frame),
                flags,
                anchor_description,
            )
        }
        OverlayPlacementAction::Hide => set_overlay_window_pos(
            overlay_hwnd,
            None,
            None,
            stable_flags | SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
            "none",
        ),
    }
}

fn state_after_action(
    applied: Option<AppliedOverlayState>,
    desired: AppliedOverlayState,
    action: OverlayPlacementAction,
) -> AppliedOverlayState {
    let prior = applied.unwrap_or(AppliedOverlayState {
        frame: None,
        visible: false,
        z_ordered: false,
    });
    match action {
        OverlayPlacementAction::MoveOnly { frame }
        | OverlayPlacementAction::ResizeOnly { frame }
        | OverlayPlacementAction::GeometryOnly { frame } => AppliedOverlayState {
            frame: Some(frame),
            visible: true,
            z_ordered: prior.z_ordered,
        },
        OverlayPlacementAction::DisplayFrameAndTopmost { frame } => AppliedOverlayState {
            frame: Some(frame),
            visible: true,
            z_ordered: true,
        },
        OverlayPlacementAction::DisplayTopmostOnly => AppliedOverlayState {
            frame: prior.frame.or(desired.frame),
            visible: true,
            z_ordered: true,
        },
        OverlayPlacementAction::Show { frame } => AppliedOverlayState {
            frame: Some(frame),
            visible: true,
            z_ordered: true,
        },
        OverlayPlacementAction::Hide => AppliedOverlayState {
            frame: prior.frame.or(desired.frame),
            visible: false,
            z_ordered: prior.z_ordered,
        },
        OverlayPlacementAction::Noop => prior,
    }
}

fn apply_overlay_state(
    window_id: u32,
    overlay_hwnd: isize,
    raw_hwnd: usize,
    target_kind: TargetKind,
    stacking_mode: OverlayStackingMode,
    desired: AppliedOverlayState,
) -> bool {
    let custom_indicator_ready = OVERLAY_READINESS
        .lock_unpoisoned()
        .get(&window_id)
        .is_some_and(|readiness| readiness.custom_indicator_ready);
    let overlay_exists = overlay_is_window(overlay_hwnd);
    let overlay_visible = overlay_exists && overlay_is_visible(overlay_hwnd);
    let owner_matches = overlay_exists && overlay_owner_matches(overlay_hwnd, raw_hwnd as isize);
    if target_kind == TargetKind::Window
        && stacking_mode == OverlayStackingMode::SourceOwned
        && source_owned_overlay_fallback_allowed(
            window_id,
            custom_indicator_ready,
            desired.visible,
            overlay_exists,
            overlay_visible,
            owner_matches,
        )
    {
        let reason = if !overlay_exists {
            "ordinary sharer overlay HWND was destroyed"
        } else if !overlay_visible {
            "ordinary sharer overlay became hidden"
        } else {
            "ordinary sharer overlay lost its source owner"
        };
        log::warn!(
            "windows share overlay: {reason} window={window_id} source_hwnd={} overlay_hwnd={overlay_hwnd}",
            raw_hwnd as isize
        );
        if custom_indicator_ready {
            let queued =
                crate::windows_screen_capture::request_system_indicator_fallback(window_id);
            log::warn!(
                "windows share overlay: requested system indicator fallback window={window_id} queued={queued} reason={reason}"
            );
            return false;
        }
    }
    let applied = OVERLAY_PLACEMENT
        .lock_unpoisoned()
        .get(&window_id)
        .copied()
        .map(|state| AppliedOverlayState {
            visible: overlay_visible,
            ..state
        });
    let action = overlay_placement_action(applied, desired, target_kind, stacking_mode);
    if action == OverlayPlacementAction::Noop {
        return true;
    }
    if let Err(error) = apply_native_action(overlay_hwnd, target_kind, action) {
        log::warn!(
            "windows share overlay: native placement action failed window={window_id} source_hwnd={} overlay_hwnd={overlay_hwnd} action={action:?}: {error}",
            raw_hwnd as isize
        );
        if custom_indicator_ready {
            let queued =
                crate::windows_screen_capture::request_system_indicator_fallback(window_id);
            log::warn!(
                "windows share overlay: requested system indicator fallback window={window_id} queued={queued} action={action:?}"
            );
        } else if stacking_mode == OverlayStackingMode::Passive {
            disable_passive_overlay(window_id, overlay_hwnd);
        }
        return false;
    }
    let next = state_after_action(applied, desired, action);
    OVERLAY_PLACEMENT.lock_unpoisoned().insert(window_id, next);
    if matches!(
        action,
        OverlayPlacementAction::Show { .. }
            | OverlayPlacementAction::Hide
            | OverlayPlacementAction::DisplayTopmostOnly
            | OverlayPlacementAction::DisplayFrameAndTopmost { .. }
    ) {
        log::debug!("windows share overlay: placement state window={window_id} action={action:?}");
    }
    true
}

fn reconcile_overlay(
    window_id: u32,
    overlay_hwnd: isize,
    raw_hwnd: usize,
    target_kind: TargetKind,
    stacking_mode: OverlayStackingMode,
    region_share: bool,
    source_frame: Option<OverlayFrame>,
) -> bool {
    let desired = desired_overlay_state(
        window_id,
        overlay_hwnd,
        raw_hwnd,
        target_kind,
        stacking_mode,
        region_share,
        source_frame,
    );
    apply_overlay_state(
        window_id,
        overlay_hwnd,
        raw_hwnd,
        target_kind,
        stacking_mode,
        desired,
    )
}

fn position_overlay_hidden(overlay_hwnd: isize, frame: OverlayFrame) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
    };
    set_overlay_window_pos(
        overlay_hwnd,
        None,
        Some(frame),
        SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
        "none",
    )
}

fn post_tracker_reconcile() {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    let tracker_hwnd = TRACKER_HWND.load(Ordering::Acquire);
    if tracker_hwnd == 0 || TRACKER_RECONCILE_QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    if unsafe {
        PostMessageW(
            Some(HWND(tracker_hwnd as *mut _)),
            TRACKER_RECONCILE_MESSAGE,
            WPARAM(0),
            LPARAM(0),
        )
    }
    .is_err()
    {
        TRACKER_RECONCILE_QUEUED.store(false, Ordering::Release);
    }
}

fn reconcile_all_overlays() {
    let overlays = OVERLAY_HWNDS.lock_unpoisoned().clone();
    // Cache one DWM-visible source frame per native target for this reconcile.
    // The hover tab and a sharer border for the same token must never read or
    // round two different frames from the same move event.
    let mut source_frames: HashMap<usize, Option<OverlayFrame>> = HashMap::new();
    for (window_id, overlay_hwnd) in overlays {
        if OVERLAY_CAPTURE_FALLBACK
            .lock_unpoisoned()
            .get(&window_id)
            .copied()
            .unwrap_or(false)
            || OVERLAY_PASSIVE_DISABLED
                .lock_unpoisoned()
                .get(&window_id)
                .copied()
                .unwrap_or(false)
        {
            continue;
        }
        if let Ok(target) = crate::windows_capture_target::resolve(window_id) {
            let raw_hwnd = target.raw_handle();
            let region_share = crate::region_window::resolve(window_id).is_some();
            let source_frame = if region_share {
                overlay_frame(window_id, raw_hwnd, target.kind(), true)
            } else {
                *source_frames
                    .entry(raw_hwnd)
                    .or_insert_with(|| overlay_frame(window_id, raw_hwnd, target.kind(), false))
            };
            let stacking_mode = OVERLAY_STACKING
                .lock_unpoisoned()
                .get(&window_id)
                .copied()
                .unwrap_or(OverlayStackingMode::SourceOwned);
            let _ = reconcile_overlay(
                window_id,
                overlay_hwnd,
                raw_hwnd,
                target.kind(),
                stacking_mode,
                region_share,
                source_frame,
            );
        } else {
            let _ = apply_overlay_state(
                window_id,
                overlay_hwnd,
                0,
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
                AppliedOverlayState {
                    frame: None,
                    visible: false,
                    z_ordered: true,
                },
            );
        }
    }

    // The hover tab is a follower even before a share overlay exists. Reuse a
    // cached frame when both surfaces point at the same source HWND.
    if let Some(attachment) = crate::windows_hover::native_hover_tab_attachment() {
        let source_frame = match crate::windows_capture_target::resolve(attachment.token) {
            Ok(target)
                if target.kind() == TargetKind::Window
                    && target.raw_handle() as isize == attachment.source_hwnd =>
            {
                let raw_hwnd = target.raw_handle();
                *source_frames.entry(raw_hwnd).or_insert_with(|| {
                    overlay_frame(attachment.token, raw_hwnd, TargetKind::Window, false)
                })
            }
            _ => None,
        };
        let _ =
            crate::windows_hover::reconcile_native_hover_tab(attachment.source_hwnd, source_frame);
    }
}

fn native_reorder_event_is_top_level(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOT};
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    if hwnd.0.is_null() {
        return false;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    !root.0.is_null() && root == hwnd
}

fn native_event_targets_follower(
    event: u32,
    id_object: i32,
    id_child: i32,
    hwnd: isize,
    hover_source: Option<isize>,
    follower_hwnds: &[isize],
    ignored_hwnd: Option<isize>,
    reorder_is_top_level: bool,
) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER,
        EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
        EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART,
        OBJID_WINDOW,
    };
    let object_event = matches!(
        event,
        EVENT_OBJECT_DESTROY
            | EVENT_OBJECT_HIDE
            | EVENT_OBJECT_LOCATIONCHANGE
            | EVENT_OBJECT_REORDER
            | EVENT_OBJECT_SHOW
    );
    let system_event = matches!(
        event,
        EVENT_SYSTEM_FOREGROUND
            | EVENT_SYSTEM_MINIMIZEEND
            | EVENT_SYSTEM_MINIMIZESTART
            | EVENT_SYSTEM_MOVESIZEEND
            | EVENT_SYSTEM_MOVESIZESTART
    );
    if !object_event && !system_event {
        return false;
    }
    // Reordering the tab itself emits another object event. Ignore it before
    // checking source/follower membership so a placement cannot self-trigger
    // an unbounded reconcile loop.
    if ignored_hwnd == Some(hwnd) {
        return false;
    }
    // Object events identify a whole window explicitly. System events use
    // the HWND as their authority but do not consistently populate object/id
    // fields across Windows shells, so do not discard foreground/minimize/
    // move-size notifications solely on those fields.
    if object_event && (id_object != OBJID_WINDOW.0 || id_child != 0) {
        return false;
    }
    // Child reorder notifications describe internal browser/pane layout, not
    // desktop z-order. The caller supplies the fail-closed GA_ROOT result so
    // this pure admission function remains directly testable.
    if event == EVENT_OBJECT_REORDER && !reorder_is_top_level {
        return false;
    }
    // While a hover source is active, any top-level foreground/reorder event
    // can insert an occluder immediately above the source. Geometry/show/hide
    // events remain restricted to the source and known follower HWNDs.
    let z_order_event = event == EVENT_OBJECT_REORDER || event == EVENT_SYSTEM_FOREGROUND;
    (z_order_event && hover_source.is_some())
        || hover_source == Some(hwnd)
        || follower_hwnds.contains(&hwnd)
}

fn overlay_event_targets_active_share(
    event: u32,
    id_object: i32,
    id_child: i32,
    hwnd: isize,
) -> bool {
    let mut follower_hwnds = Vec::new();
    let overlays = OVERLAY_HWNDS.lock_unpoisoned();
    for (window_id, overlay_hwnd) in overlays.iter() {
        follower_hwnds.push(*overlay_hwnd);
        if let Some(target) = crate::windows_capture_target::resolve(*window_id).ok() {
            if target.kind() == TargetKind::Window {
                follower_hwnds.push(target.raw_handle() as isize);
            }
        }
    }
    native_event_targets_follower(
        event,
        id_object,
        id_child,
        hwnd,
        None,
        &follower_hwnds,
        None,
        event != windows::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_REORDER
            || native_reorder_event_is_top_level(hwnd),
    )
}

fn active_follower_event_targets(event: u32, id_object: i32, id_child: i32, hwnd: isize) -> bool {
    let attachment = crate::windows_hover::native_hover_tab_attachment();
    let hover_source = attachment.map(|attachment| attachment.source_hwnd);
    let ignored_hwnd = attachment.map(|attachment| attachment.pill_hwnd);
    let mut follower_hwnds = Vec::new();
    let overlays = OVERLAY_HWNDS.lock_unpoisoned();
    for (window_id, overlay_hwnd) in overlays.iter() {
        follower_hwnds.push(*overlay_hwnd);
        if let Some(target) = crate::windows_capture_target::resolve(*window_id).ok() {
            if target.kind() == TargetKind::Window {
                follower_hwnds.push(target.raw_handle() as isize);
            }
        }
    }
    native_event_targets_follower(
        event,
        id_object,
        id_child,
        hwnd,
        hover_source,
        &follower_hwnds,
        ignored_hwnd,
        event != windows::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_REORDER
            || native_reorder_event_is_top_level(hwnd),
    )
}

unsafe extern "system" fn overlay_win_event_proc(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    hwnd: windows::Win32::Foundation::HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    let accepted = active_follower_event_targets(event, id_object, id_child, hwnd.0 as isize);
    if !accepted {
        return;
    }
    post_tracker_reconcile();
}

fn install_overlay_hooks(
    proc: windows::Win32::UI::Accessibility::WINEVENTPROC,
) -> Vec<windows::Win32::UI::Accessibility::HWINEVENTHOOK> {
    use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER,
        EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
        EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART,
        WINEVENT_OUTOFCONTEXT,
    };
    let events = [
        EVENT_OBJECT_DESTROY,
        EVENT_OBJECT_HIDE,
        EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_REORDER,
        EVENT_OBJECT_SHOW,
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZEEND,
        EVENT_SYSTEM_MINIMIZESTART,
        EVENT_SYSTEM_MOVESIZEEND,
        EVENT_SYSTEM_MOVESIZESTART,
    ];
    let mut hooks = Vec::with_capacity(events.len());
    for event in events {
        let hook =
            unsafe { SetWinEventHook(event, event, None, proc, 0, 0, WINEVENT_OUTOFCONTEXT) };
        if hook.is_invalid() {
            log::warn!("windows share overlay: SetWinEventHook(0x{event:04X}) failed");
        } else {
            hooks.push(hook);
        }
    }
    hooks
}

const OVERLAY_TRACKER_CLASS: &str = "PetalSharerOverlayTracker";

fn register_overlay_tracker_class() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        RegisterClassW, CS_HREDRAW, CS_VREDRAW, WNDCLASSW,
    };
    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        return false;
    };
    let name: Vec<u16> = OVERLAY_TRACKER_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_tracker_window_proc),
        hInstance: HINSTANCE::from(instance),
        lpszClassName: windows::core::PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    let result = unsafe { RegisterClassW(&class) };
    result != 0 || unsafe { GetLastError() } == ERROR_CLASS_ALREADY_EXISTS
}

fn create_overlay_tracker_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.ok()?.into();
    let name: Vec<u16> = OVERLAY_TRACKER_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::PCWSTR(name.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

unsafe extern "system" fn overlay_tracker_window_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn overlay_tracker_thread_main() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, KillTimer, SetTimer, TranslateMessage, MSG, WM_QUIT,
        WM_TIMER,
    };
    if !register_overlay_tracker_class() {
        log::error!("windows share overlay: tracker window class registration failed");
        TRACKER_STARTED.store(false, Ordering::Release);
        return;
    }
    let Some(hwnd) = create_overlay_tracker_window() else {
        log::error!("windows share overlay: tracker message window creation failed");
        TRACKER_STARTED.store(false, Ordering::Release);
        return;
    };
    TRACKER_HWND.store(hwnd.0 as usize, Ordering::Release);
    let hooks = install_overlay_hooks(Some(overlay_win_event_proc));
    let timer = unsafe {
        SetTimer(
            Some(hwnd),
            TRACKER_RECONCILE_TIMER_ID,
            RECONCILE_INTERVAL_MS,
            None,
        )
    };
    if timer == 0 {
        log::warn!("windows share overlay: reconciliation timer could not be armed");
    }
    reconcile_all_overlays();
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if msg.message == TRACKER_RECONCILE_MESSAGE {
            TRACKER_RECONCILE_QUEUED.store(false, Ordering::Release);
            reconcile_all_overlays();
        } else if msg.message == WM_TIMER && msg.wParam.0 == TRACKER_RECONCILE_TIMER_ID {
            reconcile_all_overlays();
        } else if msg.message == WM_QUIT {
            break;
        }
    }
    if timer != 0 {
        let _ = unsafe { KillTimer(Some(hwnd), TRACKER_RECONCILE_TIMER_ID) };
    }
    for hook in hooks {
        let _ = unsafe { windows::Win32::UI::Accessibility::UnhookWinEvent(hook) };
    }
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
    TRACKER_HWND.store(0, Ordering::Release);
    TRACKER_RECONCILE_QUEUED.store(false, Ordering::Release);
    TRACKER_STARTED.store(false, Ordering::Release);
}

/// One dedicated WinEvent/message-pump thread keeps the sharer overlay glued
/// to the source. Location events are immediate; the timer only reconciles
/// missed events and monitor/display changes.
fn ensure_tracker(_app: &AppHandle) {
    if TRACKER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let spawned = std::thread::Builder::new()
            .name("petal-sharer-pointer-tracker".to_string())
            .spawn(overlay_tracker_thread_main);
        if spawned.is_err() {
            log::error!("windows share overlay: failed to spawn tracker thread");
            TRACKER_STARTED.store(false, Ordering::Release);
        }
    }
    post_tracker_reconcile();
}

/// Start the single native follower before any share overlay exists. The
/// hover tab uses the same WinEvent/message-pump thread while idle.
pub(crate) fn start_tracker(app: &AppHandle) {
    ensure_tracker(app);
}

/// Wake the already-started follower after a hover target attach/detach.
pub(crate) fn wake_tracker() {
    post_tracker_reconcile();
}

/// Create (idempotently) the sharer-side telepointer overlay for `window_id`,
/// sized initially to the shared window and kept in sync by the tracker. This
/// runs before WGC `StartCapture`: only a verified source-owned ordinary
/// overlay may paint the visible identity border and authorize suppressing
/// WGC's system indicator. Elevated sources receive passive telepointers.
pub(crate) fn create_share_overlay(
    app: &AppHandle,
    window_id: u32,
    owner_identity: &str,
    show_draw_toolbar: bool,
    indicator_mode: CaptureIndicatorMode,
    region_share: bool,
    border_color: &str,
) -> Result<ShareOverlayReadiness, String> {
    if OVERLAY_LABELS.lock_unpoisoned().contains_key(&window_id) {
        return Ok(OVERLAY_READINESS
            .lock_unpoisoned()
            .get(&window_id)
            .copied()
            .unwrap_or(ShareOverlayReadiness {
                shown: true,
                capture_excluded: false,
                custom_indicator_ready: false,
            }));
    }

    OVERLAY_PASSIVE_DISABLED
        .lock_unpoisoned()
        .remove(&window_id);
    let target = crate::windows_capture_target::resolve(window_id)
        .map_err(|error| format!("shared window {window_id} is no longer available: {error}"))?;
    let label = share_overlay_label(window_id);
    let owner_identity =
        percent_encoding::utf8_percent_encode(owner_identity, percent_encoding::NON_ALPHANUMERIC);
    let normalized_color = crate::hover_core::share_color_or_default(Some(border_color));
    let encoded_color = percent_encoding::utf8_percent_encode(
        &normalized_color,
        percent_encoding::NON_ALPHANUMERIC,
    );
    let stacking_mode = if target.kind() == TargetKind::Window {
        let source_hwnd =
            windows::Win32::Foundation::HWND(target.raw_handle() as *mut core::ffi::c_void);
        match crate::windows_remote_control::window_integrity_exceeds_petal(source_hwnd) {
            Ok(true) => OverlayStackingMode::Passive,
            Ok(false) => OverlayStackingMode::SourceOwned,
            Err(error) => {
                log::warn!(
                    "windows share overlay: source integrity unavailable for window {window_id}; using passive system-indicated stacking: {error}"
                );
                OverlayStackingMode::Passive
            }
        }
    } else {
        OverlayStackingMode::SourceOwned
    };
    let custom_indicator_requested = indicator_mode == CaptureIndicatorMode::Petal
        && !region_share
        && stacking_mode == OverlayStackingMode::SourceOwned;
    log::info!("windows share overlay: stacking mode window={window_id} mode={stacking_mode:?}");
    let draw_toolbar = if show_draw_toolbar { "1" } else { "0" };
    let border_query = if custom_indicator_requested {
        format!("&shareBorder=1&shareBorderColor={encoded_color}")
    } else {
        String::new()
    };
    let url = format!(
        "compositor/pointer.html?windowId={window_id}&surface=sharer&drawToolbar={draw_toolbar}&ownerIdentity={owner_identity}{border_query}"
    );
    let page_loaded = Arc::new(AtomicBool::new(false));
    let page_loaded_callback = page_loaded.clone();
    let mut builder = WebviewWindowBuilder::new(app, label.clone(), WebviewUrl::App(url.into()));
    if target.kind() == TargetKind::Window && stacking_mode == OverlayStackingMode::SourceOwned {
        let source_hwnd =
            windows::Win32::Foundation::HWND(target.raw_handle() as *mut core::ffi::c_void);
        // A Win32 owned popup is kept above its source by the window manager
        // through activation, minimize/restore, and topmost-band changes. It
        // also remains below unrelated windows, unlike blanket topmost.
        builder = builder.owner_raw(source_hwnd);
    }
    let builder = builder
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
        .title("Petal Sharer Pointer")
        // The route paints the border immediately on document render. Keep
        // the native window hidden until click-through, display affinity, and
        // the pointer route's first page load have been configured, so a
        // custom indicator is never half-ready.
        .visible(false)
        .inner_size(1.0, 1.0)
        .on_page_load(move |_window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                page_loaded_callback.store(true, Ordering::Release);
            }
        });
    let overlay = builder
        .build()
        .map_err(|error| format!("failed to build overlay for window {window_id}: {error}"))?;
    if let Err(error) = overlay.set_ignore_cursor_events(true) {
        let _ = overlay.close();
        return Err(format!(
            "failed to make overlay click-through for window {window_id}: {error}"
        ));
    }
    let overlay_hwnd = overlay.hwnd().map(|h| h.0 as isize).unwrap_or_default();
    if overlay_hwnd == 0 {
        let _ = overlay.close();
        return Err(format!("overlay for window {window_id} has no native HWND"));
    }
    if target.kind() == TargetKind::Window
        && stacking_mode == OverlayStackingMode::SourceOwned
        && !overlay_owner_matches(overlay_hwnd, target.raw_handle() as isize)
    {
        let _ = overlay.close();
        return Err(format!(
            "sharer overlay for window {window_id} was not owned by its source"
        ));
    }

    let capture_excluded = if target.kind() == TargetKind::Display || region_share {
        let hwnd = windows::Win32::Foundation::HWND(overlay_hwnd as *mut core::ffi::c_void);
        let excluded = crate::platform::windows::set_capture_exclusion(hwnd);
        if !excluded {
            log::warn!(
                "windows share overlay: capture exclusion unavailable for window {window_id}; retaining the WGC system indicator"
            );
        }
        excluded
    } else {
        true
    };
    // A display capture includes every visible pixel on that monitor. If
    // affinity cannot exclude this process-owned overlay, do not show even
    // the pointer/draw layer: the system WGC border remains the only trusted
    // indicator and no local Petal chrome can leak into outgoing pixels.
    if (target.kind() == TargetKind::Display || region_share) && !capture_excluded {
        let _ = overlay.close();
        return Ok(ShareOverlayReadiness {
            shown: false,
            capture_excluded: false,
            custom_indicator_ready: false,
        });
    }

    // Resolve the initial geometry before exposing the WebView. The same
    // visible-frame source is used for the hidden pre-load position and the
    // first shown position, so no stale WGC/session dimensions can flash in.
    let initial = desired_overlay_state(
        window_id,
        overlay_hwnd,
        target.raw_handle(),
        target.kind(),
        stacking_mode,
        region_share,
        None,
    );
    let Some(initial_frame) = initial.frame else {
        let _ = overlay.close();
        return Err(format!(
            "failed to resolve source frame for window {window_id}"
        ));
    };
    if !initial.visible {
        let _ = overlay.close();
        return Err(format!(
            "source for window {window_id} is hidden or minimized"
        ));
    }
    if let Err(error) = position_overlay_hidden(overlay_hwnd, initial_frame) {
        let _ = overlay.close();
        return Err(format!(
            "failed to position overlay for window {window_id}: {error}"
        ));
    }
    // A custom border is only exposed after the pointer route has loaded, so
    // a hung WebView takes the safe WGC system-border path instead of leaving
    // a blank transparent replacement. System-indicator and Petal View modes
    // do not rely on this overlay for their visible indicator and can show it
    // immediately for telepointer/Draw use.
    if custom_indicator_requested {
        let page_deadline = std::time::Instant::now() + OVERLAY_PAGE_LOAD_TIMEOUT;
        while !page_loaded.load(Ordering::Acquire) && std::time::Instant::now() < page_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !page_loaded.load(Ordering::Acquire) {
            let _ = overlay.close();
            return Err(format!(
                "sharer overlay for window {window_id} did not finish loading"
            ));
        }
    }
    if let Err(error) = apply_native_action(
        overlay_hwnd,
        target.kind(),
        OverlayPlacementAction::Show {
            frame: initial_frame,
        },
    ) {
        let _ = overlay.close();
        return Err(format!(
            "failed to show overlay for window {window_id}: {error}"
        ));
    }
    let shown = desired_overlay_state(
        window_id,
        overlay_hwnd,
        target.raw_handle(),
        target.kind(),
        stacking_mode,
        region_share,
        None,
    );
    if !shown.visible || !shown.z_ordered {
        let _ = overlay.close();
        return Err(format!(
            "sharer overlay for window {window_id} could not prove visible source stacking"
        ));
    }
    OVERLAY_LABELS.lock_unpoisoned().insert(window_id, label);
    OVERLAY_STACKING
        .lock_unpoisoned()
        .insert(window_id, stacking_mode);
    OVERLAY_HWNDS
        .lock_unpoisoned()
        .insert(window_id, overlay_hwnd);
    OVERLAY_DRAW_ACTIVE
        .lock_unpoisoned()
        .insert(window_id, false);
    OVERLAY_PLACEMENT.lock_unpoisoned().insert(
        window_id,
        AppliedOverlayState {
            frame: shown.frame,
            visible: shown.visible,
            z_ordered: shown.z_ordered,
        },
    );
    ensure_tracker(app);
    let readiness = ShareOverlayReadiness {
        shown: true,
        capture_excluded,
        custom_indicator_ready: custom_indicator_is_ready(
            indicator_mode,
            region_share,
            target.kind(),
            target.kind() != TargetKind::Window
                || (stacking_mode == OverlayStackingMode::SourceOwned && shown.z_ordered),
            true,
            capture_excluded,
        ),
    };
    OVERLAY_READINESS
        .lock_unpoisoned()
        .insert(window_id, readiness);
    log::info!(
        "windows share overlay: readiness window={window_id} shown={} excluded={} custom={}",
        readiness.shown,
        readiness.capture_excluded,
        readiness.custom_indicator_ready
    );
    Ok(readiness)
}

/// Tear down the sharer overlay for a locally shared window on share stop.
pub(crate) fn close_share_overlay(app: &AppHandle, window_id: u32) {
    OVERLAY_HWNDS.lock_unpoisoned().remove(&window_id);
    OVERLAY_DRAW_ACTIVE.lock_unpoisoned().remove(&window_id);
    OVERLAY_DRAW_TRANSITIONING
        .lock_unpoisoned()
        .remove(&window_id);
    OVERLAY_PLACEMENT.lock_unpoisoned().remove(&window_id);
    OVERLAY_STACKING.lock_unpoisoned().remove(&window_id);
    OVERLAY_READINESS.lock_unpoisoned().remove(&window_id);
    OVERLAY_CAPTURE_FALLBACK
        .lock_unpoisoned()
        .remove(&window_id);
    OVERLAY_PASSIVE_DISABLED
        .lock_unpoisoned()
        .remove(&window_id);
    if let Some(label) = OVERLAY_LABELS.lock_unpoisoned().remove(&window_id) {
        if let Some(overlay) = app.get_webview_window(&label) {
            let _ = overlay.close();
        }
    }
    post_tracker_reconcile();
}

/// Hide the local overlay if WGC rejected a requested custom indicator. This
/// function intentionally does not destroy the WebView: the regular close
/// lifecycle still owns HWND cleanup, while the fallback bit prevents the
/// tracker or Draw toggle from resurrecting custom chrome mid-share.
pub(crate) fn disable_custom_indicator_for_fallback(window_id: u32) {
    OVERLAY_CAPTURE_FALLBACK
        .lock_unpoisoned()
        .insert(window_id, true);
    OVERLAY_DRAW_ACTIVE
        .lock_unpoisoned()
        .insert(window_id, false);
    if let Some(hwnd) = OVERLAY_HWNDS.lock_unpoisoned().get(&window_id).copied() {
        let stacking_mode = OVERLAY_STACKING
            .lock_unpoisoned()
            .get(&window_id)
            .copied()
            .unwrap_or(OverlayStackingMode::SourceOwned);
        let applied = OVERLAY_PLACEMENT.lock_unpoisoned().get(&window_id).copied();
        if applied.is_some_and(|state| state.visible) {
            let _ = apply_overlay_state(
                window_id,
                hwnd,
                0,
                TargetKind::Window,
                stacking_mode,
                AppliedOverlayState {
                    frame: applied.and_then(|state| state.frame),
                    visible: false,
                    z_ordered: true,
                },
            );
        }
    }
    if let Some(readiness) = OVERLAY_READINESS.lock_unpoisoned().get_mut(&window_id) {
        readiness.custom_indicator_ready = false;
    }
    log::warn!(
        "windows share overlay: custom indicator disabled for window {window_id}; using the WGC system indicator"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_for_local_share_reflects_registered_overlays() {
        OVERLAY_LABELS.lock_unpoisoned().clear();
        assert!(labels_for_local_share(7).is_empty());
        OVERLAY_LABELS
            .lock_unpoisoned()
            .insert(7, share_overlay_label(7));
        assert_eq!(labels_for_local_share(7), vec![share_overlay_label(7)]);
        assert!(labels_for_local_share(8).is_empty());
        OVERLAY_LABELS.lock_unpoisoned().clear();
        OVERLAY_DRAW_ACTIVE.lock_unpoisoned().clear();
        OVERLAY_READINESS.lock_unpoisoned().clear();
    }

    #[test]
    fn share_overlay_label_is_stable_and_collision_safe() {
        assert_eq!(share_overlay_label(42), "petal-sharer-pointer-42");
        assert_ne!(share_overlay_label(42), share_overlay_label(43));
    }

    fn placement_frame(x: i32, y: i32, width: i32, height: i32) -> OverlayFrame {
        OverlayFrame {
            x,
            y,
            width,
            height,
        }
    }

    fn placement_state(
        frame: Option<OverlayFrame>,
        visible: bool,
        z_ordered: bool,
    ) -> AppliedOverlayState {
        AppliedOverlayState {
            frame,
            visible,
            z_ordered,
        }
    }

    #[test]
    fn placement_state_diff_uses_geometry_actions_for_window_ownership() {
        let old = placement_state(Some(placement_frame(10, 20, 300, 200)), true, true);
        assert_eq!(
            overlay_placement_action(
                None,
                old,
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Show {
                frame: placement_frame(10, 20, 300, 200),
            }
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                old,
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Noop
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(Some(placement_frame(11, 21, 300, 200)), true, true),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::MoveOnly {
                frame: placement_frame(11, 21, 300, 200),
            }
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(Some(placement_frame(10, 20, 301, 201)), true, true),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::ResizeOnly {
                frame: placement_frame(10, 20, 301, 201),
            }
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(Some(placement_frame(11, 21, 301, 201)), true, true),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::GeometryOnly {
                frame: placement_frame(11, 21, 301, 201),
            }
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(old.frame, true, false),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Noop
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(old.frame, true, false),
                TargetKind::Display,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::DisplayTopmostOnly
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(old.frame, true, false),
                TargetKind::Window,
                OverlayStackingMode::Passive,
            ),
            OverlayPlacementAction::Noop
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(Some(placement_frame(12, 22, 300, 200)), true, false),
                TargetKind::Window,
                OverlayStackingMode::Passive,
            ),
            OverlayPlacementAction::MoveOnly {
                frame: placement_frame(12, 22, 300, 200),
            }
        );
        assert_eq!(
            overlay_placement_action(
                Some(old),
                placement_state(old.frame, false, true),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Hide
        );
        assert_eq!(
            overlay_placement_action(
                Some(placement_state(old.frame, false, true)),
                old,
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Show {
                frame: placement_frame(10, 20, 300, 200),
            }
        );
    }

    #[test]
    fn hidden_and_missing_sources_do_not_repeat_hide_or_emit_geometry() {
        let hidden = placement_state(Some(placement_frame(10, 20, 300, 200)), false, true);
        assert_eq!(
            overlay_placement_action(
                Some(hidden),
                hidden,
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Noop
        );
        assert_eq!(
            overlay_placement_action(
                Some(hidden),
                placement_state(None, false, true),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Noop
        );
        assert_eq!(
            overlay_placement_action(
                Some(placement_state(None, false, false)),
                placement_state(Some(placement_frame(10, 20, 300, 200)), true, false),
                TargetKind::Window,
                OverlayStackingMode::SourceOwned,
            ),
            OverlayPlacementAction::Show {
                frame: placement_frame(10, 20, 300, 200),
            }
        );
    }

    #[test]
    fn hover_follower_event_admission_matrix() {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER,
            EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW,
        };
        let source = 0x7fff_0011isize;
        let child = 0x7fff_0012isize;
        let pill = 0x7fff_0022isize;
        let unrelated = 0x7fff_0033isize;
        let cases = [
            (
                "source location",
                EVENT_OBJECT_LOCATIONCHANGE,
                OBJID_WINDOW.0,
                0,
                source,
                true,
                true,
            ),
            (
                "source foreground",
                EVENT_SYSTEM_FOREGROUND,
                -1,
                99,
                source,
                true,
                true,
            ),
            (
                "unrelated location",
                EVENT_OBJECT_LOCATIONCHANGE,
                OBJID_WINDOW.0,
                0,
                unrelated,
                true,
                false,
            ),
            (
                "pill self-reorder",
                EVENT_OBJECT_REORDER,
                OBJID_WINDOW.0,
                0,
                pill,
                true,
                false,
            ),
            (
                "source top-level reorder",
                EVENT_OBJECT_REORDER,
                OBJID_WINDOW.0,
                0,
                source,
                true,
                true,
            ),
            (
                "unrelated top-level reorder",
                EVENT_OBJECT_REORDER,
                OBJID_WINDOW.0,
                0,
                unrelated,
                true,
                true,
            ),
            (
                "unrelated foreground",
                EVENT_SYSTEM_FOREGROUND,
                -1,
                99,
                unrelated,
                true,
                true,
            ),
            (
                "child reorder",
                EVENT_OBJECT_REORDER,
                OBJID_WINDOW.0,
                0,
                child,
                false,
                false,
            ),
            (
                "child location",
                EVENT_OBJECT_LOCATIONCHANGE,
                OBJID_WINDOW.0,
                0,
                child,
                true,
                false,
            ),
            (
                "child show",
                EVENT_OBJECT_SHOW,
                OBJID_WINDOW.0,
                0,
                child,
                true,
                false,
            ),
            (
                "child hide",
                EVENT_OBJECT_HIDE,
                OBJID_WINDOW.0,
                0,
                child,
                true,
                false,
            ),
        ];

        for (name, event, id_object, id_child, hwnd, reorder_is_top_level, expected) in cases {
            assert_eq!(
                native_event_targets_follower(
                    event,
                    id_object,
                    id_child,
                    hwnd,
                    Some(source),
                    &[],
                    Some(pill),
                    reorder_is_top_level,
                ),
                expected,
                "{name}",
            );
        }
        assert!(!native_reorder_event_is_top_level(0));
        assert!(!native_reorder_event_is_top_level(
            HWND::default().0 as isize
        ));
    }

    #[test]
    fn native_events_filter_to_active_window_or_overlay_handles() {
        use windows::Win32::UI::WindowsAndMessaging::{
            EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW,
        };
        let source_raw = 0x7fff_0001usize;
        let overlay_raw = 0x7fff_0002isize;
        let token = crate::windows_capture_target::register(source_raw, 1234).unwrap();
        OVERLAY_HWNDS.lock_unpoisoned().insert(token, overlay_raw);
        OVERLAY_STACKING
            .lock_unpoisoned()
            .insert(token, OverlayStackingMode::Passive);
        assert!(overlay_event_targets_active_share(
            EVENT_OBJECT_LOCATIONCHANGE,
            OBJID_WINDOW.0,
            0,
            source_raw as isize,
        ));
        assert!(overlay_event_targets_active_share(
            EVENT_SYSTEM_FOREGROUND,
            OBJID_WINDOW.0,
            0,
            overlay_raw,
        ));
        assert!(overlay_event_targets_active_share(
            EVENT_SYSTEM_FOREGROUND,
            -1,
            99,
            source_raw as isize,
        ));
        assert!(!overlay_event_targets_active_share(
            EVENT_OBJECT_LOCATIONCHANGE,
            OBJID_WINDOW.0 + 1,
            0,
            source_raw as isize,
        ));
        assert!(!overlay_event_targets_active_share(
            EVENT_OBJECT_LOCATIONCHANGE,
            OBJID_WINDOW.0,
            1,
            source_raw as isize,
        ));
        assert!(!overlay_event_targets_active_share(
            EVENT_SYSTEM_FOREGROUND,
            OBJID_WINDOW.0,
            0,
            0x7fff_0003,
        ));
        assert!(!overlay_event_targets_active_share(
            0x7FFF,
            OBJID_WINDOW.0,
            0,
            source_raw as isize,
        ));
        OVERLAY_HWNDS.lock_unpoisoned().remove(&token);
        OVERLAY_STACKING.lock_unpoisoned().remove(&token);
        let _ = crate::windows_capture_target::invalidate(token);
    }

    #[test]
    fn state_after_geometry_preserves_z_order_until_a_separate_reconcile() {
        let old = placement_state(Some(placement_frame(10, 20, 300, 200)), true, false);
        let desired = placement_state(Some(placement_frame(11, 21, 301, 201)), true, true);
        assert_eq!(
            state_after_action(
                Some(old),
                desired,
                OverlayPlacementAction::GeometryOnly {
                    frame: placement_frame(11, 21, 301, 201),
                },
            ),
            placement_state(Some(placement_frame(11, 21, 301, 201)), true, false)
        );
        assert_eq!(
            state_after_action(
                Some(old),
                desired,
                OverlayPlacementAction::DisplayTopmostOnly
            ),
            placement_state(Some(placement_frame(10, 20, 300, 200)), true, true)
        );
    }

    #[test]
    fn readiness_never_claims_an_unowned_or_hidden_overlay_as_a_custom_indicator() {
        let system = ShareOverlayReadiness {
            shown: true,
            capture_excluded: true,
            custom_indicator_ready: false,
        };
        let hidden = ShareOverlayReadiness {
            shown: false,
            capture_excluded: false,
            custom_indicator_ready: false,
        };
        assert!(!system.custom_indicator_ready);
        assert!(!hidden.shown);
        assert!(custom_indicator_is_ready(
            CaptureIndicatorMode::Petal,
            false,
            TargetKind::Window,
            true,
            true,
            false,
        ));
        assert!(!custom_indicator_is_ready(
            CaptureIndicatorMode::Petal,
            false,
            TargetKind::Window,
            false,
            true,
            true,
        ));
        assert!(!custom_indicator_is_ready(
            CaptureIndicatorMode::Petal,
            false,
            TargetKind::Display,
            false,
            true,
            false,
        ));
        assert!(custom_indicator_is_ready(
            CaptureIndicatorMode::Petal,
            false,
            TargetKind::Display,
            false,
            true,
            true,
        ));
        assert!(!custom_indicator_is_ready(
            CaptureIndicatorMode::Petal,
            true,
            TargetKind::Window,
            true,
            true,
            true,
        ));
        assert!(!custom_indicator_is_ready(
            CaptureIndicatorMode::System,
            false,
            TargetKind::Window,
            true,
            true,
            true,
        ));
    }

    #[test]
    fn source_owned_overlay_loss_requires_fallback_only_when_visible_and_custom() {
        assert!(source_owned_overlay_needs_fallback(
            true, true, true, true, false
        ));
        assert!(source_owned_overlay_needs_fallback(
            true, true, true, false, true
        ));
        assert!(source_owned_overlay_needs_fallback(
            true, true, false, false, false
        ));
        assert!(!source_owned_overlay_needs_fallback(
            true, false, false, false, false
        ));
        assert!(!source_owned_overlay_needs_fallback(
            false, true, false, false, false
        ));
        assert!(!source_owned_overlay_needs_fallback(
            true, true, true, true, true
        ));
    }

    #[test]
    fn draw_activation_transition_does_not_trigger_indicator_fallback() {
        let window_id = 0xD7A7;
        OVERLAY_DRAW_TRANSITIONING
            .lock_unpoisoned()
            .insert(window_id, true);
        assert!(!source_owned_overlay_fallback_allowed(
            window_id, true, true, true, false, true
        ));
        OVERLAY_DRAW_TRANSITIONING
            .lock_unpoisoned()
            .remove(&window_id);
        assert!(source_owned_overlay_fallback_allowed(
            window_id, true, true, true, false, true
        ));
    }

    #[test]
    fn failed_wgc_border_suppression_marks_the_overlay_for_system_fallback() {
        OVERLAY_CAPTURE_FALLBACK.lock_unpoisoned().clear();
        disable_custom_indicator_for_fallback(91);
        assert_eq!(
            OVERLAY_CAPTURE_FALLBACK.lock_unpoisoned().get(&91),
            Some(&true)
        );
        OVERLAY_CAPTURE_FALLBACK.lock_unpoisoned().remove(&91);
    }

    #[test]
    fn overlay_readiness_and_colors_are_independent_per_share() {
        OVERLAY_LABELS.lock_unpoisoned().clear();
        OVERLAY_READINESS.lock_unpoisoned().clear();
        let first_readiness = ShareOverlayReadiness {
            shown: true,
            capture_excluded: true,
            custom_indicator_ready: true,
        };
        let second_readiness = ShareOverlayReadiness {
            shown: true,
            capture_excluded: true,
            custom_indicator_ready: false,
        };
        OVERLAY_LABELS
            .lock_unpoisoned()
            .insert(11, share_overlay_label(11));
        OVERLAY_LABELS
            .lock_unpoisoned()
            .insert(12, share_overlay_label(12));
        OVERLAY_READINESS
            .lock_unpoisoned()
            .insert(11, first_readiness);
        OVERLAY_READINESS
            .lock_unpoisoned()
            .insert(12, second_readiness);
        assert_eq!(
            OVERLAY_READINESS.lock_unpoisoned().get(&11),
            Some(&first_readiness)
        );
        assert_eq!(
            OVERLAY_READINESS.lock_unpoisoned().get(&12),
            Some(&second_readiness)
        );
        OVERLAY_LABELS.lock_unpoisoned().clear();
        OVERLAY_READINESS.lock_unpoisoned().clear();
    }

    #[test]
    fn overlay_visibility_hides_minimized_or_degenerate_frames() {
        let frame = crate::platform::cg::WindowFrame {
            x: 10,
            y: 20,
            width: 100,
            height: 80,
        };
        assert!(overlay_frame_is_visible(frame, false));
        assert!(!overlay_frame_is_visible(frame, true));
        assert!(!overlay_frame_is_visible(
            crate::platform::cg::WindowFrame { width: 1, ..frame },
            false,
        ));
        assert!(!overlay_frame_is_visible(
            crate::platform::cg::WindowFrame { height: 1, ..frame },
            false,
        ));
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn real_overlay_owner_tracks_activation_and_topmost_transitions() {
        if std::env::var("PETAL_TEST_REAL_OVERLAY_WINEVENTS").as_deref() != Ok("1") {
            eprintln!(
                "skipping real sharer-overlay owner test (set \
                 PETAL_TEST_REAL_OVERLAY_WINEVENTS=1 on an interactive desktop to run)"
            );
            return;
        }

        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, SetWindowPos, ShowWindow, HWND_NOTOPMOST, HWND_TOPMOST,
            SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNA, WS_OVERLAPPEDWINDOW, WS_POPUP,
            WS_VISIBLE,
        };

        assert!(register_overlay_tracker_class());
        let instance = unsafe { GetModuleHandleW(None) }
            .expect("module handle")
            .into();
        let class_name: Vec<u16> = OVERLAY_TRACKER_CLASS
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let make_window = |style, owner, visible| {
            unsafe {
                CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                    windows::core::PCWSTR(class_name.as_ptr()),
                    windows::core::PCWSTR(std::ptr::null()),
                    style
                        | if visible {
                            WS_VISIBLE
                        } else {
                            Default::default()
                        },
                    180,
                    140,
                    640,
                    480,
                    owner,
                    None,
                    Some(instance),
                    None,
                )
            }
            .expect("synthetic placement window")
        };
        let source = make_window(WS_OVERLAPPEDWINDOW, None, true);
        let occluder = make_window(WS_POPUP, None, false);
        let overlay = make_window(WS_POPUP, Some(source), false);
        let passive = make_window(WS_POPUP, None, false);
        let frame = crate::platform::windows::visible_window_frame(source)
            .expect("synthetic source visible frame");
        let result = (|| {
            assert!(overlay_owner_matches(overlay.0 as isize, source.0 as isize));
            assert!(apply_native_action(
                overlay.0 as isize,
                TargetKind::Window,
                OverlayPlacementAction::Show { frame },
            )
            .is_ok());
            assert!(
                unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(overlay) }
                    .as_bool()
            );
            assert!(overlay_owner_matches(overlay.0 as isize, source.0 as isize));
            assert!(!overlay_owner_matches(
                passive.0 as isize,
                source.0 as isize
            ));
            assert!(apply_native_action(
                passive.0 as isize,
                TargetKind::Window,
                OverlayPlacementAction::Show { frame },
            )
            .is_ok());
            assert!(
                unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(passive) }
                    .as_bool()
            );
            assert!(apply_native_action(
                passive.0 as isize,
                TargetKind::Window,
                OverlayPlacementAction::GeometryOnly {
                    frame: OverlayFrame {
                        x: frame.x + 3,
                        ..frame
                    },
                },
            )
            .is_ok());
            assert!(!overlay_owner_matches(
                passive.0 as isize,
                source.0 as isize
            ));

            assert!(unsafe { ShowWindow(occluder, SW_SHOWNA) }.as_bool() == false);
            assert!(unsafe {
                SetWindowPos(
                    occluder,
                    Some(source),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                )
            }
            .is_ok());
            assert!(overlay_owner_matches(overlay.0 as isize, source.0 as isize));

            assert!(unsafe {
                SetWindowPos(
                    source,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                )
            }
            .is_ok());
            assert!(overlay_is_topmost(overlay.0 as isize));
            assert!(overlay_owner_matches(overlay.0 as isize, source.0 as isize));

            assert!(unsafe {
                SetWindowPos(
                    source,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                )
            }
            .is_ok());
            assert!(!overlay_is_topmost(overlay.0 as isize));
            assert!(overlay_owner_matches(overlay.0 as isize, source.0 as isize));
            Ok::<(), &'static str>(())
        })();
        unsafe {
            // Destroy the owner first: the real source can disappear before
            // the share-stop cleanup reaches its owned overlay HWND.
            let _ = DestroyWindow(source);
            let _ = DestroyWindow(overlay);
            let _ = DestroyWindow(passive);
            let _ = DestroyWindow(occluder);
        }
        if let Err(error) = result {
            panic!("{error}");
        }
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn real_winevent_tracker_reconciles_move_maximize_and_activation() {
        if std::env::var("PETAL_TEST_REAL_OVERLAY_WINEVENTS").as_deref() != Ok("1") {
            eprintln!(
                "skipping real sharer-overlay WinEvent smoke test (set \
                 PETAL_TEST_REAL_OVERLAY_WINEVENTS=1 on an interactive desktop to run)"
            );
            return;
        }

        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
            SetForegroundWindow, SetWindowPos, ShowWindow, TranslateMessage, MSG, SWP_NOACTIVATE,
            SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE, WS_OVERLAPPEDWINDOW,
            WS_POPUP, WS_VISIBLE,
        };

        const TEST_STOP_MESSAGE: u32 = TRACKER_RECONCILE_MESSAGE + 1;
        let (ready_tx, ready_rx) = mpsc::channel::<(usize, usize, usize, u32)>();
        let (reconciled_tx, reconciled_rx) = mpsc::channel::<()>();
        let tracker = std::thread::spawn(move || {
            assert!(register_overlay_tracker_class());
            let pump = create_overlay_tracker_window().expect("tracker pump window");
            let instance = unsafe { GetModuleHandleW(None) }
                .expect("module handle")
                .into();
            let class_name: Vec<u16> = OVERLAY_TRACKER_CLASS
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let title: Vec<u16> = "Petal overlay WinEvent source".encode_utf16().collect();
            let source = unsafe {
                CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                    windows::core::PCWSTR(class_name.as_ptr()),
                    windows::core::PCWSTR(title.as_ptr()),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    160,
                    120,
                    640,
                    480,
                    None,
                    None,
                    Some(instance),
                    None,
                )
            }
            .expect("synthetic source window");
            let overlay_title: Vec<u16> = "Petal Sharer Pointer".encode_utf16().collect();
            let overlay = unsafe {
                CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                    windows::core::PCWSTR(class_name.as_ptr()),
                    windows::core::PCWSTR(overlay_title.as_ptr()),
                    WS_POPUP | WS_VISIBLE,
                    0,
                    0,
                    1,
                    1,
                    Some(source),
                    None,
                    Some(instance),
                    None,
                )
            }
            .expect("synthetic overlay window");
            let token =
                crate::windows_capture_target::register(source.0 as usize, std::process::id())
                    .expect("synthetic source token");
            OVERLAY_HWNDS
                .lock_unpoisoned()
                .insert(token, overlay.0 as isize);
            TRACKER_RECONCILE_QUEUED.store(false, Ordering::Release);
            TRACKER_HWND.store(pump.0 as usize, Ordering::Release);
            let hooks = install_overlay_hooks(Some(overlay_win_event_proc));
            assert!(!hooks.is_empty(), "overlay WinEvent hooks must install");
            let _ = ready_tx.send((
                pump.0 as usize,
                source.0 as usize,
                overlay.0 as usize,
                token,
            ));

            let mut message = MSG::default();
            loop {
                let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
                if result.0 <= 0 || message.message == TEST_STOP_MESSAGE {
                    break;
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if message.message == TRACKER_RECONCILE_MESSAGE {
                    TRACKER_RECONCILE_QUEUED.store(false, Ordering::Release);
                    reconcile_all_overlays();
                    let _ = reconciled_tx.send(());
                }
            }
            for hook in hooks {
                let _ = unsafe { windows::Win32::UI::Accessibility::UnhookWinEvent(hook) };
            }
            OVERLAY_HWNDS.lock_unpoisoned().remove(&token);
            OVERLAY_PLACEMENT.lock_unpoisoned().remove(&token);
            let _ = crate::windows_capture_target::invalidate(token);
            unsafe {
                // Source-first teardown must leave the tracker cleanup safe
                // even when Windows has already destroyed the owned popup.
                let _ = DestroyWindow(source);
                let _ = DestroyWindow(overlay);
                let _ = DestroyWindow(pump);
            }
            TRACKER_HWND.store(0, Ordering::Release);
            TRACKER_RECONCILE_QUEUED.store(false, Ordering::Release);
        });

        let (pump_raw, source_raw, overlay_raw, _token) = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("synthetic overlay tracker must start");
        let source = HWND(source_raw as *mut core::ffi::c_void);
        let overlay = HWND(overlay_raw as *mut core::ffi::c_void);
        let frame_matches = |timeout: Duration| {
            let deadline = Instant::now() + timeout;
            let mut saw_event = false;
            while Instant::now() < deadline {
                if reconciled_rx
                    .recv_timeout(Duration::from_millis(10))
                    .is_ok()
                {
                    saw_event = true;
                }
                let source_frame = crate::platform::windows::visible_window_frame(source);
                let overlay_frame = crate::platform::windows::window_frame(overlay);
                if saw_event
                    && source_frame.zip(overlay_frame).is_some_and(
                        |(source_frame, overlay_frame)| {
                            let delta = [
                                (source_frame.x - overlay_frame.x).abs(),
                                (source_frame.y - overlay_frame.y).abs(),
                                (source_frame.width - overlay_frame.width).abs(),
                                (source_frame.height - overlay_frame.height).abs(),
                            ];
                            delta.into_iter().max().unwrap_or_default() <= 2
                        },
                    )
                    && overlay_owner_matches(overlay.0 as isize, source.0 as isize)
                {
                    return true;
                }
            }
            false
        };
        let moved = unsafe {
            SetWindowPos(
                source,
                None,
                320,
                180,
                640,
                480,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        };
        assert!(moved.is_ok(), "synthetic source move must succeed");
        let result = (|| {
            assert!(
                frame_matches(Duration::from_millis(500)),
                "move did not reconcile promptly"
            );
            let maximized = unsafe { ShowWindow(source, SW_MAXIMIZE) };
            assert!(
                maximized.as_bool(),
                "synthetic source maximize must change state"
            );
            assert!(
                frame_matches(Duration::from_millis(750)),
                "maximize did not reconcile promptly"
            );
            let restored = unsafe { ShowWindow(source, SW_RESTORE) };
            assert!(
                restored.as_bool(),
                "synthetic source restore must change state"
            );
            assert!(
                frame_matches(Duration::from_millis(750)),
                "restore did not reconcile promptly"
            );
            if unsafe { SetForegroundWindow(source) }.as_bool() {
                assert!(
                    frame_matches(Duration::from_millis(500)),
                    "foreground activation did not reconcile promptly"
                );
            } else {
                // Windows may deny foreground activation to a test process
                // launched without the foreground lock. The interactive
                // PowerShell exercise owns the title-bar activation check.
                eprintln!("foreground activation denied by Windows; activation check skipped");
            }
            Ok::<(), &'static str>(())
        })();
        let _ = unsafe {
            PostMessageW(
                Some(HWND(pump_raw as *mut core::ffi::c_void)),
                TEST_STOP_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            )
        };
        tracker
            .join()
            .expect("overlay tracker smoke thread must exit");
        if let Err(error) = result {
            panic!("{error}");
        }
    }
}
