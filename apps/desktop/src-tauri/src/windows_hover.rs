//! Windows hover "share tab" — a fixed 40x40 right-edge rail button follows the
//! current eligible window. Primary activation shares/stops directly; the
//! native context menu owns secondary options.
//!
//! The shared pure logic (geometry math, hit-test classification, event
//! payloads, share-state bookkeeping) lives in `hover_core.rs`; the macOS
//! implementation is `hover_tab.rs`. This module is the Windows native seam:
//! the window/cursor primitives come from `platform::windows`, the pill is a
//! hidden-until-hover `WebviewWindow` hosting the same `hover-tab` route the
//! macOS panel uses, and the share toggle delegates to the real Windows
//! session (`session_stub::share_window`).
//!
//! Coordinate space: all native geometry (`platform::windows`) is physical
//! pixels; this module converts to logical points with the cursor's monitor
//! `scale_factor()` before feeding the shared `hover_core` math — the exact
//! same logical-point convention macOS uses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::hover_core::{
    current_hover_presentation, hold_hover_tab_through_transient_miss,
    hover_tab_panel_logical_size, hover_tab_presentation, hover_tab_presentation_with_offset,
    last_hover_update, same_hit, set_last_hover_update, HoverTabAttachment, HoverTabDragPhase,
    HoverTabPresentation, HoverTabRect, HoverTabUpdate, MonitorBounds, WindowFrame,
    HOVER_TAB_LABEL,
};
use crate::platform::windows as w32;
use crate::sync_ext::MutexExt;

/// Tracking cadence — matches the macOS hover tab (~60 Hz cursor poll).
const POLL_MS: u64 = 16;
/// How many consecutive poll ticks the cursor may miss the window/bridge
/// before the pill hides (mirrors macOS `HOVER_TAB_HIDE_GRACE_TICKS`).
const HIDE_GRACE_TICKS: u8 = 8;
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Native options menus are separate top-level windows. Freeze hover tracking
/// while one is open so the menu's HWND is not mistaken for leaving the
/// shared target, which would hide the pill before its action callback runs.
static MENU_OPEN: AtomicBool = AtomicBool::new(false);
/// During a button drag the explicit drag command owns native placement; the
/// event follower must not fight it with a concurrent source-frame reconcile.
static DRAG_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The only native follower registration for the Windows hover tab. The
/// source and pill HWNDs stay raw so the event-driven tracker never needs to
/// call Tauri or WebView2 APIs on its message-pump thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeHoverTabAttachment {
    pub(crate) token: u32,
    pub(crate) source_hwnd: isize,
    pub(crate) pill_hwnd: isize,
    pub(crate) generation: u64,
    /// Elevated sources cannot reliably host a medium-integrity tab in their
    /// normal z-order band, so use a temporary topmost fallback.
    pub(crate) topmost_fallback: bool,
}

impl NativeHoverTabAttachment {
    pub(crate) fn replace_token(self, retired_token: u32, replacement_token: u32) -> Option<Self> {
        (self.token == retired_token).then_some(Self {
            token: replacement_token,
            ..self
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NativeHoverTabPlacement {
    pub(crate) attachment: HoverTabAttachment,
    pub(crate) frame: WindowFrame,
}

static NATIVE_HOVER_TAB_ATTACHMENT: LazyLock<Mutex<Option<NativeHoverTabAttachment>>> =
    LazyLock::new(|| Mutex::new(None));

pub(crate) fn native_hover_tab_attachment() -> Option<NativeHoverTabAttachment> {
    *NATIVE_HOVER_TAB_ATTACHMENT.lock_unpoisoned()
}

pub(crate) fn native_attachment_is_current(
    attachment: Option<NativeHoverTabAttachment>,
    token: u32,
    generation: u64,
) -> bool {
    attachment.is_some_and(|current| current.token == token && current.generation == generation)
}

/// Project one DWM-visible source frame into the physical frame used by the
/// native hover HWND. The calculation intentionally happens in physical
/// pixels so the tab and share border can consume the identical source
/// snapshot without a Tauri monitor lookup or a second rounding path.
pub(crate) fn project_hover_tab_native_frame(
    source: WindowFrame,
    work_area: WindowFrame,
    scale: f64,
) -> Option<NativeHoverTabPlacement> {
    project_hover_tab_native_frame_with_offset(
        source,
        work_area,
        scale,
        crate::hover_core::DEFAULT_HOVER_TAB_VERTICAL_OFFSET,
    )
}

pub(crate) fn project_hover_tab_native_frame_with_offset(
    source: WindowFrame,
    work_area: WindowFrame,
    scale: f64,
    vertical_offset: f64,
) -> Option<NativeHoverTabPlacement> {
    if !scale.is_finite()
        || scale <= 0.0
        || source.width <= 0
        || source.height <= 0
        || work_area.width <= 0
        || work_area.height <= 0
    {
        return None;
    }
    let tab_size_f = 40.0 * scale;
    if !tab_size_f.is_finite() {
        return None;
    }
    let tab_size = tab_size_f.round().max(1.0) as i64;
    if tab_size <= 0
        || i64::from(work_area.width) < tab_size
        || i64::from(work_area.height) < tab_size
    {
        return None;
    }
    let source_right = i64::from(source.x) + i64::from(source.width);
    let work_area_left = i64::from(work_area.x);
    let work_area_top = i64::from(work_area.y);
    let work_area_right = work_area_left + i64::from(work_area.width);
    let work_area_bottom = work_area_top + i64::from(work_area.height);
    let outside = source_right >= work_area_left && source_right + tab_size <= work_area_right;
    let raw_x = if outside {
        source_right
    } else {
        source_right - tab_size
    };
    let max_x = work_area_right - tab_size;
    let x = raw_x.clamp(work_area_left, max_x);
    let travel = (i64::from(source.height) - tab_size).max(0) as f64;
    let offset = crate::hover_core::normalize_hover_tab_vertical_offset(vertical_offset);
    let raw_y = i64::from(source.y) + (travel * offset).round() as i64;
    let max_y = work_area_bottom - tab_size;
    let y = raw_y.clamp(work_area_top, max_y);
    if x < work_area_left
        || y < work_area_top
        || x + tab_size > work_area_right
        || y + tab_size > work_area_bottom
    {
        return None;
    }
    Some(NativeHoverTabPlacement {
        attachment: if outside {
            HoverTabAttachment::Outside
        } else {
            HoverTabAttachment::Inset
        },
        frame: WindowFrame {
            x: i32::try_from(x).ok()?,
            y: i32::try_from(y).ok()?,
            width: i32::try_from(tab_size).ok()?,
            height: i32::try_from(tab_size).ok()?,
        },
    })
}

fn hide_native_hover_tab(attachment: NativeHoverTabAttachment, reason: &str) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindow, IsWindowVisible, SetWindowPos, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    };
    let tab = HWND(attachment.pill_hwnd as *mut core::ffi::c_void);
    if tab.0.is_null() || !unsafe { IsWindow(Some(tab)) }.as_bool() {
        log::debug!(
            "windows_hover: cannot hide invalid native tab token={} reason={reason}",
            attachment.token
        );
        return false;
    }
    if !unsafe { IsWindowVisible(tab) }.as_bool() {
        return true;
    }
    let result = unsafe {
        SetWindowPos(
            tab,
            None,
            0,
            0,
            0,
            0,
            SWP_HIDEWINDOW
                | SWP_NOACTIVATE
                | SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOOWNERZORDER
                | SWP_NOZORDER,
        )
    };
    if let Err(error) = result {
        log::warn!(
            "windows_hover: fail-closed native tab hide failed token={} reason={reason}: {error}",
            attachment.token
        );
        return false;
    }
    true
}

fn apply_native_hover_tab_placement(
    attachment: NativeHoverTabAttachment,
    placement: NativeHoverTabPlacement,
) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
        SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    let tab = HWND(attachment.pill_hwnd as *mut core::ffi::c_void);
    let source = HWND(attachment.source_hwnd as *mut core::ffi::c_void);
    if attachment.topmost_fallback {
        if let Err(error) = unsafe {
            SetWindowPos(
                tab,
                Some(HWND_TOPMOST),
                placement.frame.x,
                placement.frame.y,
                placement.frame.width,
                placement.frame.height,
                SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            )
        } {
            log::warn!(
                "windows_hover: elevated native tab topmost placement failed token={} source_hwnd={}: {error}",
                attachment.token,
                attachment.source_hwnd
            );
            let _ = hide_native_hover_tab(attachment, "elevated topmost placement failed");
            return false;
        }
        return true;
    }
    let Some(anchor) = w32::checked_window_above_in_z_order_excluding(source, Some(tab)) else {
        log::warn!(
            "windows_hover: cannot resolve source z-order anchor token={} source_hwnd={}",
            attachment.token,
            attachment.source_hwnd
        );
        let _ = hide_native_hover_tab(attachment, "source z-order anchor unavailable");
        return false;
    };
    let Some(source_topmost) = w32::window_is_topmost(source) else {
        let _ = hide_native_hover_tab(attachment, "source z-order band unavailable");
        return false;
    };
    let Some(tab_topmost) = w32::window_is_topmost(tab) else {
        let _ = hide_native_hover_tab(attachment, "native tab z-order band unavailable");
        return false;
    };

    // SetWindowPos preserves a window's current topmost state when given a
    // real predecessor. Move through the correct band first when a previous
    // target used the other band; keep the tab hidden during that transition.
    if source_topmost != tab_topmost {
        if !hide_native_hover_tab(attachment, "switching source z-order band") {
            return false;
        }
        let band_anchor = if source_topmost {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        if let Err(error) = unsafe {
            SetWindowPos(
                tab,
                Some(band_anchor),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
            )
        } {
            log::warn!(
                "windows_hover: native tab z-order band switch failed token={} source_hwnd={}: {error}",
                attachment.token,
                attachment.source_hwnd
            );
            let _ = hide_native_hover_tab(attachment, "z-order band switch failed");
            return false;
        }
    }

    let result = unsafe {
        SetWindowPos(
            tab,
            Some(anchor),
            placement.frame.x,
            placement.frame.y,
            placement.frame.width,
            placement.frame.height,
            SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    };
    if let Err(error) = result {
        log::warn!(
            "windows_hover: native tab SetWindowPos failed token={} source_hwnd={}: {error}",
            attachment.token,
            attachment.source_hwnd
        );
        let _ = hide_native_hover_tab(attachment, "source-relative placement failed");
        return false;
    }
    true
}

/// Reconcile the native tab directly on the shared WinEvent tracker thread.
/// The attachment lock is held across the short SetWindowPos call so a hide
/// or replacement cannot overtake a stale movement.
pub(crate) fn reconcile_native_hover_tab(
    source_hwnd: isize,
    source_frame: Option<WindowFrame>,
) -> bool {
    if DRAG_ACTIVE.load(Ordering::Acquire) {
        return false;
    }
    let current = NATIVE_HOVER_TAB_ATTACHMENT.lock_unpoisoned();
    let Some(attachment) = current.as_ref().copied() else {
        return false;
    };
    if attachment.source_hwnd != source_hwnd
        || !native_attachment_is_current(Some(attachment), attachment.token, attachment.generation)
    {
        return false;
    }
    let Some(source_frame) = source_frame else {
        return hide_native_hover_tab(attachment, "source frame unavailable");
    };
    let source = windows::Win32::Foundation::HWND(source_hwnd as *mut core::ffi::c_void);
    let Some(scale) = w32::window_dpi_scale(source) else {
        return hide_native_hover_tab(attachment, "source DPI unavailable");
    };
    let Some(work_area) = w32::monitor_work_area_for_window(source) else {
        return hide_native_hover_tab(attachment, "monitor work area unavailable");
    };
    let Some(placement) = project_hover_tab_native_frame_with_offset(
        source_frame,
        work_area,
        scale,
        crate::share_priority::current_hover_tab_vertical_offset(),
    ) else {
        return hide_native_hover_tab(attachment, "no safe work-area placement");
    };
    apply_native_hover_tab_placement(attachment, placement)
}

/// Attach or replace the native follower registration for a discovered
/// target. The tracker is woken after the state is published, so it cannot
/// observe a partially initialized HWND/token pair.
pub(crate) fn attach_hover_tab_follower(
    app: &AppHandle,
    token: u32,
    source_hwnd: isize,
    generation: u64,
) -> Option<NativeHoverTabAttachment> {
    let pill_hwnd = app
        .get_webview_window(HOVER_TAB_LABEL)
        .and_then(|window| window.hwnd().ok())?
        .0 as isize;
    if source_hwnd == 0 || pill_hwnd == 0 {
        return None;
    }
    let source = windows::Win32::Foundation::HWND(source_hwnd as *mut core::ffi::c_void);
    let topmost_fallback = match crate::windows_remote_control::window_integrity_exceeds_petal(
        source,
    ) {
        Ok(exceeds) => exceeds,
        Err(error) => {
            log::warn!(
                "windows_hover: source integrity unavailable; using topmost fallback token={} source_hwnd={}: {error}",
                token,
                source_hwnd
            );
            true
        }
    };
    let attachment = NativeHoverTabAttachment {
        token,
        source_hwnd,
        pill_hwnd,
        generation,
        topmost_fallback,
    };
    *NATIVE_HOVER_TAB_ATTACHMENT.lock_unpoisoned() = Some(attachment);
    crate::windows_share_overlay::wake_tracker();
    Some(attachment)
}

pub(crate) fn replace_hover_tab_follower_token(
    retired_token: u32,
    replacement_token: u32,
) -> Option<NativeHoverTabAttachment> {
    let replacement = {
        let mut current = NATIVE_HOVER_TAB_ATTACHMENT.lock_unpoisoned();
        let updated = current
            .as_ref()
            .and_then(|attachment| attachment.replace_token(retired_token, replacement_token));
        if let Some(updated) = updated {
            *current = Some(updated);
        }
        updated
    };
    if replacement.is_some() {
        crate::windows_share_overlay::wake_tracker();
    }
    replacement
}

pub(crate) fn detach_hover_tab_follower() -> Option<NativeHoverTabAttachment> {
    let detached = NATIVE_HOVER_TAB_ATTACHMENT.lock_unpoisoned().take();
    if detached.is_some() {
        crate::hover_core::begin_hover_tab_presentation();
        crate::windows_share_overlay::wake_tracker();
    }
    detached
}

/// Idempotent start of the background hover-tracking thread. The pill window
/// must already exist (see [`create_pill_window`], called from `setup()` on
/// the MAIN thread — WebView2 cannot be created from this background thread;
/// a lazy build there fails with HRESULT 0x8007139F).
pub fn start(app: &AppHandle) {
    // The WinEvent follower must exist before the first target is discovered;
    // sharing is not a prerequisite for moving the idle hover tab.
    crate::windows_share_overlay::start_tracker(app);
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("windows_hover: tracker started");
    let app = app.clone();
    std::thread::spawn(move || {
        run(&app);
        ACTIVE.store(false, Ordering::SeqCst);
        log::info!("windows_hover: tracker stopped");
    });
}

/// Idempotent stop; hides the pill.
pub fn stop(app: &AppHandle) {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        log::debug!("windows_hover: stop() hiding tab");
        hide_pill(app, "stop");
    }
}

#[tauri::command]
pub fn hover_tab_page_mounted() -> Option<HoverTabUpdate> {
    log::info!("hover_tab: page mounted (webview loaded, IPC bridge live)");
    last_hover_update()
}

/// Keep the hover pill and its current target stable while the native options
/// menu is tracking the pointer outside the pill HWND.
#[tauri::command]
pub fn set_hover_tab_menu_open(open: bool) {
    MENU_OPEN.store(open, Ordering::Release);
    log::info!(
        "windows_hover: native options menu {}",
        if open { "opened" } else { "closed" }
    );
}

fn reset_hover_tab_drag_state(restore: bool) -> Option<crate::hover_core::HoverTabDragSession> {
    let session = crate::hover_core::clear_hover_tab_drag();
    if restore {
        if let Some(session) = session {
            let _ =
                crate::share_priority::preview_hover_tab_vertical_offset(session.original_offset);
        }
    }
    DRAG_ACTIVE.store(false, Ordering::Release);
    session
}

pub(crate) fn cancel_drag_for_lifecycle() {
    reset_hover_tab_drag_state(true);
}

fn drag_target_is_current(window_id: u32) -> bool {
    last_hover_update().is_some_and(|update| update.window_id == window_id)
        && native_hover_tab_attachment().is_some_and(|attachment| attachment.token == window_id)
}

fn native_source_geometry(
    attachment: NativeHoverTabAttachment,
) -> Option<(WindowFrame, WindowFrame, f64)> {
    let source = windows::Win32::Foundation::HWND(attachment.source_hwnd as *mut core::ffi::c_void);
    let scale = w32::window_dpi_scale(source)?;
    let source_frame = w32::visible_window_frame(source)?;
    let work_area = w32::monitor_work_area_for_window(source)?;
    Some((source_frame, work_area, scale))
}

fn update_drag_payload(
    app: &AppHandle,
    window_id: u32,
    requested_frame: WindowFrame,
    offset: f64,
    geometry: (WindowFrame, f64, NativeHoverTabPlacement),
) -> Result<(), String> {
    let (source_frame, scale, placement) = geometry;
    let mut update =
        last_hover_update().ok_or_else(|| "hover-tab target is no longer presented".to_string())?;
    if update.window_id != window_id {
        return Err("hover-tab target changed during drag".to_string());
    }
    let logical_frame = WindowFrame {
        x: (source_frame.x as f64 / scale).round() as i32,
        y: (source_frame.y as f64 / scale).round() as i32,
        width: (source_frame.width as f64 / scale).round() as i32,
        height: (source_frame.height as f64 / scale).round() as i32,
    };
    update.frame = if logical_frame.width > 0 && logical_frame.height > 0 {
        logical_frame
    } else {
        requested_frame
    };
    update.tab_x = placement.frame.x as f64 / scale;
    update.tab_y = placement.frame.y as f64 / scale;
    update.attachment = placement.attachment;
    update.vertical_offset = offset;
    set_last_hover_update(Some(update.clone()));
    let _ = tauri::Emitter::emit(app, "hover-tab-update", &update);
    Ok(())
}

fn apply_drag_position(
    app: &AppHandle,
    window_id: u32,
    requested_frame: WindowFrame,
    offset: f64,
) -> Result<f64, String> {
    // Re-check immediately before the native write. The caller's phase check
    // can race a source hide/token replacement, especially on cancellation.
    if !drag_target_is_current(window_id) {
        return Err("hover-tab target changed during drag".to_string());
    }
    let attachment = native_hover_tab_attachment()
        .filter(|attachment| attachment.token == window_id)
        .ok_or_else(|| "hover-tab native target is stale".to_string())?;
    let (source_frame, work_area, scale) = native_source_geometry(attachment)
        .ok_or_else(|| "hover-tab source geometry is unavailable".to_string())?;
    let placement =
        project_hover_tab_native_frame_with_offset(source_frame, work_area, scale, offset)
            .ok_or_else(|| "hover-tab has no safe work-area placement".to_string())?;
    if !apply_native_hover_tab_placement(attachment, placement) {
        return Err("hover-tab native placement failed".to_string());
    }
    let offset = crate::hover_core::normalize_hover_tab_vertical_offset(offset);
    update_drag_payload(
        app,
        window_id,
        requested_frame,
        offset,
        (source_frame, scale, placement),
    )?;
    Ok(offset)
}

fn rollback_drag_position(
    app: &AppHandle,
    window_id: u32,
    frame: WindowFrame,
    restore_offset: f64,
) {
    let _ = crate::share_priority::preview_hover_tab_vertical_offset(restore_offset);
    let _ = apply_drag_position(app, window_id, frame, restore_offset);
    reset_hover_tab_drag_state(false);
}

/// One phase-based drag bridge shared by the Windows route and its native
/// position presets. The follower is frozen while this command owns movement;
/// token validation and the native attachment remain authoritative.
#[tauri::command]
pub fn hover_tab_drag(
    app: AppHandle,
    phase: HoverTabDragPhase,
    window_id: u32,
    frame: WindowFrame,
    vertical_offset: f64,
) -> Result<f64, String> {
    match phase {
        HoverTabDragPhase::Begin => {
            if !drag_target_is_current(window_id) {
                return Err("hover-tab target is stale".to_string());
            }
            let session = crate::hover_core::begin_hover_tab_drag(window_id)?;
            DRAG_ACTIVE.store(true, Ordering::Release);
            log::debug!(
                "windows_hover: hover-tab drag began token={} generation={}",
                session.window_id,
                session.generation
            );
            Ok(session.original_offset)
        }
        HoverTabDragPhase::Update => {
            let session = crate::hover_core::active_hover_tab_drag(window_id)
                .ok_or_else(|| "hover-tab drag is not active".to_string())?;
            if !drag_target_is_current(window_id) {
                reset_hover_tab_drag_state(true);
                return Err("hover-tab target changed during drag".to_string());
            }
            let offset =
                match crate::share_priority::preview_hover_tab_vertical_offset(vertical_offset) {
                    Ok(offset) => offset,
                    Err(error) => {
                        reset_hover_tab_drag_state(true);
                        return Err(error);
                    }
                };
            if let Err(error) = apply_drag_position(&app, window_id, frame, offset) {
                rollback_drag_position(&app, window_id, frame, session.original_offset);
                return Err(error);
            }
            Ok(offset)
        }
        HoverTabDragPhase::Commit => {
            let active = crate::hover_core::active_hover_tab_drag(window_id);
            let restore_offset = active
                .map(|session| session.original_offset)
                .unwrap_or_else(crate::share_priority::current_hover_tab_vertical_offset);
            if !drag_target_is_current(window_id) {
                let _ = crate::share_priority::preview_hover_tab_vertical_offset(restore_offset);
                reset_hover_tab_drag_state(false);
                return Err("hover-tab target is stale".to_string());
            }
            let offset =
                match crate::share_priority::preview_hover_tab_vertical_offset(vertical_offset) {
                    Ok(offset) => offset,
                    Err(error) => {
                        reset_hover_tab_drag_state(true);
                        return Err(error);
                    }
                };
            if let Err(error) = apply_drag_position(&app, window_id, frame, offset) {
                rollback_drag_position(&app, window_id, frame, restore_offset);
                return Err(error);
            }
            let committed = match crate::share_priority::commit_hover_tab_vertical_offset(offset) {
                Ok(value) => value,
                Err(error) => {
                    rollback_drag_position(&app, window_id, frame, restore_offset);
                    return Err(error);
                }
            };
            if active.is_some() {
                let _ = crate::hover_core::finish_hover_tab_drag(window_id);
            }
            DRAG_ACTIVE.store(false, Ordering::Release);
            Ok(committed)
        }
        HoverTabDragPhase::Cancel => {
            let Some(session) = crate::hover_core::active_hover_tab_drag(window_id) else {
                return Ok(crate::share_priority::current_hover_tab_vertical_offset());
            };
            let restored = match crate::share_priority::preview_hover_tab_vertical_offset(
                session.original_offset,
            ) {
                Ok(offset) => offset,
                Err(error) => {
                    reset_hover_tab_drag_state(false);
                    return Err(error);
                }
            };
            let result = if drag_target_is_current(window_id) {
                apply_drag_position(&app, window_id, frame, restored)
            } else {
                // The native panel may already have been hidden or its token
                // replaced. Restore the preference/session, but never move a
                // stale HWND back onto the desktop.
                Ok(restored)
            };
            let _ = crate::hover_core::finish_hover_tab_drag(window_id);
            DRAG_ACTIVE.store(false, Ordering::Release);
            result.map(|_| restored)
        }
    }
}

/// Share toggle with the SAME wire contract as the macOS command (windowId,
/// frame, color → new shared state), delegating to the real Windows session
/// (`session_stub::share_window`, which starts/stops the WGC capture +
/// LiveKit publish and emits `share-state-changed`/`share-error`).
#[tauri::command]
pub async fn toggle_window_share(
    app: AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    frame: WindowFrame,
    color: Option<String>,
) -> Result<bool, ()> {
    let _ = frame;
    let was_shared = state.shared_window_ids().contains(&window_id);
    log::info!(
        "windows_hover: direct {} action for token {window_id}",
        if was_shared { "Stop" } else { "Share" }
    );
    let result = crate::session::share_window(app, state, window_id, color, None)
        .await
        .map_err(|error| {
            log::warn!("windows_hover: share token {window_id} rejected: {error}");
        });
    if let Ok(now_shared) = &result {
        log::info!(
            "windows_hover: direct {} completed for token {window_id}; shared={now_shared}",
            if *now_shared { "Share" } else { "Stop" }
        );
    }
    result
}

fn run(app: &AppHandle) {
    let mut last: Option<(WindowFrame, u32)> = None;
    let mut missed_bridge_ticks: u8 = 0;
    let mut last_tab_rect: Option<HoverTabRect> = None;
    // One info line per transition INTO not-in-room suppression (issue #22:
    // "invisible failures stay invisible" -- the pill being hidden because
    // the user isn't in a meeting is by-design, but must be observable in
    // petal.log rather than indistinguishable from a bug).
    let mut suppressed_logged = false;

    loop {
        if !ACTIVE.load(Ordering::SeqCst) {
            break;
        }
        let started = std::time::Instant::now();

        // The share pill only makes sense while in a meeting (SPEC.md §4.2:
        // sharing is an in-meeting action). Outside a room, keep the tab
        // hidden entirely instead of inviting a share that would fail with
        // NotInRoom -- mirrors the macOS hover_tab::run gate.
        let in_room = app
            .try_state::<crate::session::SessionState>()
            .map(|s| s.is_in_room())
            .unwrap_or(false);
        if !in_room {
            if last.is_some() || native_hover_tab_attachment().is_some() {
                log::debug!("windows_hover: not in a room -- hiding tab");
                last = None;
                last_tab_rect = None;
                hide_pill(app, "not-in-room");
            }
            if !suppressed_logged {
                log::info!(
                    "windows_hover: suppressed -- not in a room (pill only appears while in a meeting)"
                );
                suppressed_logged = true;
            }
            std::thread::sleep(Duration::from_millis(POLL_MS * 4));
            continue;
        }
        if suppressed_logged {
            log::info!("windows_hover: in a room -- pill tracking active");
            suppressed_logged = false;
        }

        // A panic here would silently kill the tracker thread (panic output
        // never reaches the file log sink); turn it into a visible error so a
        // dead tracker is diagnosable instead of a mystery.
        let iteration = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            track_iteration(app, &mut last, &mut missed_bridge_ticks, &mut last_tab_rect);
        }));
        if let Err(panic) = iteration {
            log::error!(
                "windows_hover: tracker iteration panicked: {:?} -- stopping tracker",
                panic
            );
            ACTIVE.store(false, Ordering::SeqCst);
            break;
        }

        let elapsed = started.elapsed();
        if elapsed < Duration::from_millis(POLL_MS) {
            std::thread::sleep(Duration::from_millis(POLL_MS) - elapsed);
        }
    }
    detach_hover_tab_follower();
}

fn cached_hover_target_is_stale(cached: Option<(WindowFrame, u32)>) -> bool {
    let Some((_, token)) = cached else {
        return false;
    };
    crate::windows_capture_target::resolve(token).is_err()
}

/// Adopt the one-shot replacement for the currently cached hover target. This
/// runs before the generic stale-token path so an outside tab remains visible
/// while its stopped share is replaced by a fresh, unshared hover token.
fn adopt_hover_target_replacement(last: &mut Option<(WindowFrame, u32)>) -> Option<(u32, u32)> {
    let (frame, retired_token) = (*last)?;
    let replacement_token =
        crate::windows_capture_target::consume_hover_replacement(retired_token)?;
    *last = Some((frame, replacement_token));
    let _ = replace_hover_tab_follower_token(retired_token, replacement_token);
    Some((retired_token, replacement_token))
}

fn root_window_at_for_hover(
    cursor: (f64, f64),
    attachment: Option<NativeHoverTabAttachment>,
) -> Option<windows::Win32::Foundation::HWND> {
    let Some(attachment) = attachment.filter(|attachment| attachment.topmost_fallback) else {
        return w32::root_window_at(cursor);
    };
    let root = w32::root_window_at_skipping_self(cursor, attachment.source_hwnd, &[])?;
    Some(windows::Win32::Foundation::HWND(
        root as *mut core::ffi::c_void,
    ))
}

fn track_iteration(
    app: &AppHandle,
    last: &mut Option<(WindowFrame, u32)>,
    missed_bridge_ticks: &mut u8,
    last_tab_rect: &mut Option<HoverTabRect>,
) {
    if MENU_OPEN.load(Ordering::Acquire) || DRAG_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    // A stopped hovered window gets a fresh token before the generic stale
    // path runs. Keep the existing native panel and geometry in place; only
    // refresh the logical identity delivered to the route. The old token is
    // already unresolvable, so this does not weaken queued-input rejection.
    if let Some((retired_token, replacement_token)) = adopt_hover_target_replacement(last) {
        if !native_hover_tab_attachment()
            .is_some_and(|attachment| attachment.token == replacement_token)
        {
            if let Some(source_hwnd) = crate::windows_capture_target::resolve(replacement_token)
                .ok()
                .map(|target| target.raw_handle() as isize)
            {
                let generation = native_hover_tab_attachment()
                    .map(|attachment| attachment.generation)
                    .unwrap_or_else(crate::hover_core::begin_hover_tab_presentation);
                let _ = attach_hover_tab_follower(app, replacement_token, source_hwnd, generation);
            }
        }
        let replacement_update = last_hover_update()
            .filter(|update| update.window_id == retired_token)
            .map(|mut update| {
                update.window_id = replacement_token;
                update.shared = false;
                update
            })
            .or_else(|| {
                let (frame, _) = (*last)?;
                current_hover_presentation()
                    .filter(|presentation| presentation.window_id == retired_token)
                    .map(|presentation| HoverTabUpdate {
                        window_id: replacement_token,
                        frame,
                        tab_x: presentation.rect.x,
                        tab_y: presentation.rect.y,
                        attachment: presentation.attachment,
                        vertical_offset: crate::share_priority::current_hover_tab_vertical_offset(),
                        shared: false,
                        display_like: false,
                    })
            });
        if let Some(update) = replacement_update {
            set_last_hover_update(Some(update.clone()));
            let _ = tauri::Emitter::emit(app, "hover-tab-update", &update);
        } else {
            log::warn!(
                "windows_hover: hover token {retired_token} replaced by {replacement_token} without a current presentation"
            );
        }
    }

    // Stop-time teardown without a matching hover handoff deliberately retires
    // the opaque token and hides/reacquires through the normal fail-closed
    // path.
    if cached_hover_target_is_stale(*last) {
        log::debug!("windows_hover: cached target token is stale -- reacquiring");
        *last = None;
        *last_tab_rect = None;
        *missed_bridge_ticks = 0;
        hide_pill(app, "stale-target");
    }

    let cursor_physical = w32::cursor_position();
    if cursor_physical.is_some_and(crate::region_window::cursor_inside_registered_region) {
        // Petal View is hollow and click-through, so WindowFromPoint would
        // otherwise return the application underneath and expose a second
        // share tab through the selector. Clear any prior ordinary target
        // before skipping native hit-testing altogether.
        if last.is_some()
            || current_hover_presentation().is_some()
            || native_hover_tab_attachment().is_some()
        {
            hide_pill(app, "cursor-inside-region");
        }
        *last = None;
        *last_tab_rect = None;
        *missed_bridge_ticks = 0;
        return;
    }
    let scale = monitor_scale_for_point(app, cursor_physical);
    let cursor = cursor_physical.map(|(x, y)| (x / scale, y / scale));

    let mut blocked_by_surface = false;
    // Hit-test the topmost window, then apply the SAME central decision used by
    // the picker. A rejected visible surface is a blocker, never a reason to
    // see through to the application underneath.
    let hit: Option<(WindowFrame, u32)> = cursor_physical.and_then(|cp| {
        let attachment = native_hover_tab_attachment();
        let elevated_attachment = attachment.filter(|attachment| attachment.topmost_fallback);
        let cursor_over_native_tab = elevated_attachment.is_some_and(|attachment| {
            let tab = windows::Win32::Foundation::HWND(
                attachment.pill_hwnd as *mut core::ffi::c_void,
            );
            w32::window_frame(tab).is_some_and(|frame| {
                cp.0 >= frame.x as f64
                    && cp.0 < (frame.x + frame.width) as f64
                    && cp.1 >= frame.y as f64
                    && cp.1 < (frame.y + frame.height) as f64
            })
        });
        let tab_is_hit = elevated_attachment.is_some_and(|attachment| {
            cursor_over_native_tab
                || w32::root_window_at(cp)
                    .is_some_and(|root| root.0 as isize == attachment.pill_hwnd)
        });
        // The native tab can be transparent to WindowFromPoint at an
        // integrity boundary. A focused or actively shared elevated source
        // still owns its tab; preserve it before resolving the window below.
        if let Some(attachment) = elevated_attachment {
            let source_is_foreground = w32::foreground_root_window()
                .is_some_and(|root| root.0 as isize == attachment.source_hwnd);
            let source_is_shared = app
                .try_state::<crate::session::SessionState>()
                .is_some_and(|state| state.is_share_active(attachment.token));
            if tab_is_hit && (source_is_foreground || source_is_shared) {
                return *last;
            }
        }
        let hwnd = root_window_at_for_hover(cp, attachment)?;
        // Once the elevated source is no longer foreground, let the normal
        // hit-test path retarget the tab to the window under the cursor.
        // Petal-owned bridge surfaces preserve the current target instead of
        // exposing an application underneath them.
        let over_active_sharer_overlay = last.is_some_and(|(_, window_id)| {
            crate::windows_share_overlay::is_draw_active(window_id)
                && crate::windows_share_overlay::hwnd_for_local_share(window_id)
                    == Some(hwnd.0 as isize)
        });
        if over_active_sharer_overlay
            || own_window_is_pill(app, hwnd)
            || own_window_is_control_consent(app, hwnd)
        {
            return *last;
        }
        let Some(inspection) = w32::inspect_window(hwnd, std::process::id()) else {
            blocked_by_surface = true;
            return None;
        };
        let decision = crate::share_target::classify(&inspection.facts);
        if !decision.is_eligible()
            || decision.kind() == Some(crate::share_target::ShareTargetKind::RegisteredRegion)
        {
            blocked_by_surface = true;
            return None;
        }
        let Some(frame) = inspection.frame else {
            blocked_by_surface = true;
            return None;
        };
        let pid = inspection.facts.owner_pid;
        let token = w32::register_window(hwnd, pid)?;
        Some((
            WindowFrame {
                x: (frame.x as f64 / scale).round() as i32,
                y: (frame.y as f64 / scale).round() as i32,
                width: (frame.width as f64 / scale).round() as i32,
                height: (frame.height as f64 / scale).round() as i32,
            },
            token,
        ))
    });

    if blocked_by_surface {
        *last = None;
        *last_tab_rect = None;
        *missed_bridge_ticks = 0;
        if last_hover_update().is_some() || native_hover_tab_attachment().is_some() {
            hide_pill(app, "blocked-surface");
        }
        return;
    }

    match &hit {
        Some((frame, window_id)) => {
            *missed_bridge_ticks = 0;
            // `hide_pill` clears the shared last-update snapshot. If Draw
            // is still active and the cursor re-enters through the
            // click-owning overlay, the hit is the same target but the tab
            // still needs a fresh native show/update.
            if !same_hit(&hit, last) || last_hover_update().is_none() {
                *last = hit;
                let presentation = tab_position(app, frame, scale, *window_id);
                *last_tab_rect = Some(presentation.rect);
                let generation = crate::hover_core::begin_hover_tab_presentation();
                let source_hwnd = crate::windows_capture_target::resolve(*window_id)
                    .ok()
                    .map(|target| target.raw_handle() as isize);
                if source_hwnd
                    .and_then(|source_hwnd| {
                        attach_hover_tab_follower(app, *window_id, source_hwnd, generation)
                    })
                    .is_none()
                {
                    log::warn!(
                        "windows_hover: could not attach native follower for token {}",
                        *window_id
                    );
                }
                let shared = app
                    .try_state::<crate::session::SessionState>()
                    .map(|state| state.shared_window_ids().contains(window_id))
                    .unwrap_or(false);
                let display_like = crate::windows_capture_target::resolve(*window_id)
                    .map(|target| {
                        target.kind() == crate::windows_capture_target::TargetKind::Display
                    })
                    .unwrap_or(false);
                let payload = HoverTabUpdate {
                    window_id: *window_id,
                    frame: *frame,
                    tab_x: presentation.rect.x,
                    tab_y: presentation.rect.y,
                    attachment: presentation.attachment,
                    vertical_offset: crate::share_priority::current_hover_tab_vertical_offset(),
                    shared,
                    display_like,
                };
                set_last_hover_update(Some(payload.clone()));
                let _ = tauri::Emitter::emit(app, "hover-tab-update", &payload);
            }
        }
        None => {
            // Hold through transient misses inside the bridge region,
            // then hide (mirrors macOS `hold_hover_tab_through_transient_miss`).
            let hold = hold_hover_tab_through_transient_miss(
                cursor,
                current_hover_presentation()
                    .map(|presentation| presentation.rect)
                    .or(*last_tab_rect),
                *last,
                *missed_bridge_ticks,
            );
            if hold {
                *missed_bridge_ticks = missed_bridge_ticks.saturating_add(1);
            } else {
                if last.is_some() {
                    log::debug!("windows_hover: cursor left window -- hiding tab");
                }
                let preserve_draw_target = last.is_some_and(|(_, window_id)| {
                    crate::windows_share_overlay::is_draw_active(window_id)
                });
                if !preserve_draw_target {
                    *last = None;
                    *last_tab_rect = None;
                }
                // Avoid re-emitting hide on every poll while Draw is
                // active. Keeping `last` lets the overlay identify the
                // target when the cursor returns and re-show the tab.
                if last_hover_update().is_some() {
                    hide_pill(app, "cursor-left-target");
                }
            }
        }
    }
}

/// Whether `hwnd` belongs to the hover pill webview itself (so a cursor
/// hovering the pill keeps the underlying window's tab alive).
fn own_window_is_pill(app: &AppHandle, hwnd: windows::Win32::Foundation::HWND) -> bool {
    app.get_webview_window(HOVER_TAB_LABEL)
        .and_then(|window| window.hwnd().ok())
        .is_some_and(|pill| pill == hwnd)
}

fn own_window_is_control_consent(
    app: &AppHandle,
    hwnd: windows::Win32::Foundation::HWND,
) -> bool {
    app.get_webview_window(crate::control_consent::CONTROL_CONSENT_LABEL)
        .and_then(|window| window.hwnd().ok())
        .is_some_and(|consent| consent == hwnd)
}

/// Logical fixed-panel geometry for the cursor's monitor.
fn tab_position(
    app: &AppHandle,
    frame: &WindowFrame,
    scale: f64,
    window_id: u32,
) -> HoverTabPresentation {
    let offset = crate::share_priority::current_hover_tab_vertical_offset();
    // The native follower is authoritative for Windows placement. Mirror its
    // physical rcWork projection into the logical event payload so bridge
    // hit-testing and the actual HWND never disagree near a taskbar edge.
    if let Some(target) = crate::windows_capture_target::resolve(window_id).ok() {
        let source =
            windows::Win32::Foundation::HWND(target.raw_handle() as *mut core::ffi::c_void);
        if let (Some(source_frame), Some(work_area), Some(native_scale)) = (
            w32::visible_window_frame(source),
            w32::monitor_work_area_for_window(source),
            w32::window_dpi_scale(source),
        ) {
            if let Some(placement) = project_hover_tab_native_frame_with_offset(
                source_frame,
                work_area,
                native_scale,
                offset,
            ) {
                return HoverTabPresentation {
                    window_id,
                    attachment: placement.attachment,
                    rect: HoverTabRect {
                        x: placement.frame.x as f64 / native_scale,
                        y: placement.frame.y as f64 / native_scale,
                        width: placement.frame.width as f64 / native_scale,
                        height: placement.frame.height as f64 / native_scale,
                    },
                };
            }
        }
    }
    tab_position_with_offset(app, frame, scale, window_id, offset)
}

fn tab_position_with_offset(
    app: &AppHandle,
    frame: &WindowFrame,
    scale: f64,
    window_id: u32,
    vertical_offset: f64,
) -> HoverTabPresentation {
    hover_tab_presentation_with_offset(
        window_id,
        *frame,
        cursor_monitor_logical_bounds(app, scale),
        vertical_offset,
    )
}

fn cursor_monitor_logical_bounds(app: &AppHandle, scale: f64) -> MonitorBounds {
    let Some((cx, cy)) = w32::cursor_position() else {
        return MonitorBounds::new(f64::NEG_INFINITY, 0.0, f64::INFINITY, f64::INFINITY);
    };
    if let Ok(monitors) = app.available_monitors() {
        for monitor in monitors {
            let pos = monitor.position();
            let size = monitor.size();
            let (left, top, right, bottom) = (
                pos.x as f64,
                pos.y as f64,
                pos.x as f64 + size.width as f64,
                pos.y as f64 + size.height as f64,
            );
            if cx >= left && cx < right && cy >= top && cy < bottom {
                return MonitorBounds::new(
                    left / scale,
                    top / scale,
                    right / scale,
                    bottom / scale,
                );
            }
        }
    }
    app.primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let pos = m.position();
            let size = m.size();
            MonitorBounds::new(
                pos.x as f64 / scale,
                pos.y as f64 / scale,
                (pos.x as f64 + size.width as f64) / scale,
                (pos.y as f64 + size.height as f64) / scale,
            )
        })
        .unwrap_or(MonitorBounds::new(
            f64::NEG_INFINITY,
            0.0,
            f64::INFINITY,
            f64::INFINITY,
        ))
}

/// DPI scale of the monitor containing `point` (physical px), defaulting to
/// the primary monitor's scale.
fn monitor_scale_for_point(app: &AppHandle, point: Option<(f64, f64)>) -> f64 {
    let Some((x, y)) = point else {
        return primary_scale(app);
    };
    if let Ok(monitors) = app.available_monitors() {
        for monitor in monitors {
            let pos = monitor.position();
            let size = monitor.size();
            if x >= pos.x as f64
                && x < pos.x as f64 + size.width as f64
                && y >= pos.y as f64
                && y < pos.y as f64 + size.height as f64
            {
                return monitor.scale_factor().max(1.0);
            }
        }
    }
    primary_scale(app)
}

fn primary_scale(app: &AppHandle) -> f64 {
    app.primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor().max(1.0))
        .unwrap_or(1.0)
}

/// Create the hidden hover pill webview. MUST be called from the Tauri MAIN
/// thread during `setup()` (mirrors macOS `create_hover_tab`): WebView2
/// creation from a background thread fails with `0x8007139F`. The tracker
/// thread only positions/shows/hides this pre-created window.
pub fn create_pill_window(app: &AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    let (width, height) = hover_tab_panel_logical_size();
    match WebviewWindowBuilder::new(
        app,
        HOVER_TAB_LABEL,
        WebviewUrl::App("hover-tab.html".into()),
    )
    .title(crate::hover_core::HOVER_TAB_WINDOW_TITLE)
    .decorations(false)
    // Capsule pill: per-pixel alpha required, DWM cannot round this shape —
    // never call make_native_rounded here.
    .transparent(true)
    // CRITICAL: must match every other WebView2 window's environment
    // options. WebView2 fixes environment options per user-data-folder at
    // first creation, and `CreateCoreWebView2EnvironmentWithOptions` with
    // the SAME folder but DIFFERENT options fails with 0x8007139F — the
    // pill's webview was silently failing to create (window shown, nothing
    // rendered) until this was added.
    .additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS)
    // Ordinary sources are placed immediately above the source by the native
    // follower; elevated sources temporarily use a topmost fallback there.
    .skip_taskbar(true)
    .shadow(false)
    // Passive hover presentation must not make this pill the focused window.
    // Repeated reveals use the native no-activate path below as well.
    .focused(false)
    .visible(false)
    .inner_size(width, height)
    .build()
    {
        Ok(window) => {
            // Pre-position off-screen so a brief visible flash cannot occur
            // between build and the first hover show.
            let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
                x: -10000.0,
                y: -10000.0,
            }));
            log::info!("windows_hover: hover pill window created (hidden, off-screen)");
            Ok(())
        }
        Err(e) => {
            log::error!("windows_hover: failed to create hover tab window: {e}");
            Err(e.to_string())
        }
    }
}

fn hide_pill(app: &AppHandle, _reason: &str) {
    // Source loss, room leave, and shutdown are cancellation boundaries for a
    // drag. Restore the previous in-memory offset before clearing the target;
    // never let a hidden/invalid source commit a preview.
    reset_hover_tab_drag_state(true);
    detach_hover_tab_follower();
    set_last_hover_update(None);
    let hide_generation = crate::hover_core::begin_hover_tab_presentation();
    if let Some(window) = app.get_webview_window(HOVER_TAB_LABEL) {
        // Hide on the Tauri main thread, in the same serialized queue as
        // position/show callbacks. A generation check here prevents an old
        // show that was already queued from resurrecting the pill after a
        // rejected shell surface or hide; a newer target may still
        // overtake this hide and present itself normally.
        //
        // Use native SW_HIDE rather than WebviewWindow::hide(): passive show
        // deliberately uses SetWindowPos(SWP_SHOWWINDOW), which can leave
        // tao's cached visibility flag stale and make the latter a no-op.
        if let Err(error) = app.run_on_main_thread(move || {
            if crate::hover_core::hover_tab_presentation_generation() != hide_generation {
                return;
            }
            let Ok(raw) = window.hwnd() else {
                log::warn!("windows_hover: could not resolve hover-pill HWND for hide");
                return;
            };
            let hwnd = windows::Win32::Foundation::HWND(raw.0 as *mut core::ffi::c_void);
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
                );
            }
            log::info!("windows_hover: pill hidden");
        }) {
            log::warn!("windows_hover: hide dispatch failed: {error}");
        }
    }
    // Global `emit` — the route listens with plain `listen()` (issue #22
    // root cause; `emit_to` never reaches it).
    let _ = tauri::Emitter::emit(app, "hover-tab-hide", ());
}

/// `is_shared` exists in `hover_core` for parity with the macOS seam; on
/// Windows the authoritative shared set lives in `SessionState`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_poll_constants_match_macos_grace() {
        // The Windows tracker must not be MORE eager to hide than macOS.
        assert!(HIDE_GRACE_TICKS >= 8);
        assert!(POLL_MS <= 16);
    }

    #[test]
    fn stale_cached_target_is_rejected_until_reacquired() {
        let raw_handle = 0x5a17_0000usize.wrapping_add(std::process::id() as usize);
        let owner_process_id = std::process::id().wrapping_add(1);
        let frame = WindowFrame {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        let token = crate::windows_capture_target::register(raw_handle, owner_process_id)
            .expect("synthetic target should register");
        let cached = Some((frame, token));

        assert!(!cached_hover_target_is_stale(None));
        assert!(!cached_hover_target_is_stale(cached));
        assert!(crate::windows_capture_target::invalidate(token));
        assert!(cached_hover_target_is_stale(cached));

        let replacement = crate::windows_capture_target::register(raw_handle, owner_process_id)
            .expect("replacement target should register");
        assert_ne!(replacement, token);
        assert!(!cached_hover_target_is_stale(Some((frame, replacement))));
        assert!(crate::windows_capture_target::invalidate(replacement));
    }

    #[test]
    fn hover_replacement_is_adopted_without_hide_and_unmatched_stale_still_fails_closed() {
        let raw_handle = 0x6a17_0000usize.wrapping_add(std::process::id() as usize);
        let owner_process_id = std::process::id().wrapping_add(2);
        let frame = WindowFrame {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        let retired = crate::windows_capture_target::register(raw_handle, owner_process_id)
            .expect("synthetic hover target should register");
        let mut last = Some((frame, retired));
        let replacement = crate::windows_capture_target::retire_for_hover(retired)
            .expect("active hover target should receive a replacement");

        assert!(cached_hover_target_is_stale(last));
        assert_eq!(
            adopt_hover_target_replacement(&mut last),
            Some((retired, replacement))
        );
        assert_eq!(last, Some((frame, replacement)));
        assert!(!cached_hover_target_is_stale(last));
        assert_eq!(adopt_hover_target_replacement(&mut last), None);
        assert!(crate::windows_capture_target::invalidate(replacement));

        let stale =
            crate::windows_capture_target::register(raw_handle.wrapping_add(1), owner_process_id)
                .expect("unmatched target should register");
        let mut unmatched = Some((frame, stale));
        assert!(crate::windows_capture_target::invalidate(stale));
        assert_eq!(adopt_hover_target_replacement(&mut unmatched), None);
        assert_eq!(unmatched, Some((frame, stale)));
        assert!(cached_hover_target_is_stale(unmatched));
    }

    #[test]
    fn native_follow_projects_outside_and_inset_from_one_physical_source_snapshot() {
        let source = WindowFrame {
            x: 300,
            y: 400,
            width: 500,
            height: 300,
        };
        let monitor = WindowFrame {
            x: 0,
            y: 0,
            width: 1200,
            height: 800,
        };
        let outside = project_hover_tab_native_frame(source, monitor, 1.0)
            .expect("ordinary source should project a native tab");
        assert_eq!(
            outside.attachment,
            crate::hover_core::HoverTabAttachment::Outside
        );
        assert_eq!(outside.frame.x, source.x + source.width);
        assert_eq!(outside.frame.width, 40);
        assert_eq!(outside.frame.height, 40);
        assert_eq!(
            outside.frame.y,
            source.y + (source.height - outside.frame.height) / 2
        );

        let maximized = WindowFrame {
            x: 0,
            y: 0,
            width: 1200,
            height: 800,
        };
        let inset = project_hover_tab_native_frame(maximized, monitor, 1.0)
            .expect("maximized source should project a native tab");
        assert_eq!(
            inset.attachment,
            crate::hover_core::HoverTabAttachment::Inset
        );
        assert_eq!(
            inset.frame.x + inset.frame.width,
            maximized.x + maximized.width
        );
        assert_eq!(inset.frame.y, (maximized.height - inset.frame.height) / 2);
        // The border and tab consume the same `maximized` snapshot: the
        // tab's right edge is derived directly from the border's source edge.
        assert_eq!(
            inset.frame.x + inset.frame.width,
            maximized.x + maximized.width
        );
    }

    #[test]
    fn native_follow_respects_reserved_work_area_edges_and_negative_origins() {
        let cases = [
            (
                WindowFrame {
                    x: 600,
                    y: 930,
                    width: 400,
                    height: 120,
                },
                WindowFrame {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1040,
                },
            ),
            (
                WindowFrame {
                    x: 120,
                    y: -20,
                    width: 500,
                    height: 120,
                },
                WindowFrame {
                    x: 0,
                    y: 40,
                    width: 1920,
                    height: 1040,
                },
            ),
            (
                WindowFrame {
                    x: -300,
                    y: 200,
                    width: 350,
                    height: 300,
                },
                WindowFrame {
                    x: 80,
                    y: 0,
                    width: 1840,
                    height: 1080,
                },
            ),
            (
                WindowFrame {
                    x: 1400,
                    y: 200,
                    width: 500,
                    height: 300,
                },
                WindowFrame {
                    x: 0,
                    y: 0,
                    width: 1840,
                    height: 1080,
                },
            ),
            (
                WindowFrame {
                    x: -1800,
                    y: 200,
                    width: 500,
                    height: 300,
                },
                WindowFrame {
                    x: -1920,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ),
        ];
        for (source, work_area) in cases {
            let placement = project_hover_tab_native_frame(source, work_area, 1.0)
                .expect("a normal work area should fit a native tab");
            assert!(placement.frame.x >= work_area.x);
            assert!(placement.frame.y >= work_area.y);
            assert!(placement.frame.x + placement.frame.width <= work_area.x + work_area.width);
            assert!(placement.frame.y + placement.frame.height <= work_area.y + work_area.height);
        }

        let work_area = WindowFrame {
            x: 0,
            y: 0,
            width: 1840,
            height: 1080,
        };
        let outside = project_hover_tab_native_frame(
            WindowFrame {
                x: 1300,
                y: 200,
                width: 500,
                height: 300,
            },
            work_area,
            1.0,
        )
        .expect("adjacent work-area slot should use outside attachment");
        assert_eq!(outside.attachment, HoverTabAttachment::Outside);
        assert_eq!(outside.frame.x, 1800);

        let inset = project_hover_tab_native_frame(
            WindowFrame {
                x: 1400,
                y: 200,
                width: 500,
                height: 300,
            },
            work_area,
            1.0,
        )
        .expect("inset placement should fit the work area");
        assert_eq!(inset.attachment, HoverTabAttachment::Inset);
        assert_eq!(
            inset.frame.x + inset.frame.width,
            work_area.x + work_area.width
        );
    }

    #[test]
    fn native_follow_rejects_a_work_area_smaller_than_the_scaled_tab() {
        assert!(project_hover_tab_native_frame(
            WindowFrame {
                x: 0,
                y: 0,
                width: 200,
                height: 200,
            },
            WindowFrame {
                x: 0,
                y: 0,
                width: 39,
                height: 200,
            },
            1.0,
        )
        .is_none());
        assert!(project_hover_tab_native_frame(
            WindowFrame {
                x: 0,
                y: 0,
                width: 200,
                height: 200,
            },
            WindowFrame {
                x: 0,
                y: 0,
                width: 200,
                height: 59,
            },
            1.5,
        )
        .is_none());
    }

    #[test]
    fn native_follow_keeps_a_40_logical_pixel_tab_across_common_dpi_scales() {
        for (scale, width, height) in [(1.0, 1920, 1080), (1.25, 2400, 1350), (1.5, 2880, 1620)] {
            let source = WindowFrame {
                x: 0,
                y: 0,
                width,
                height,
            };
            let placement = project_hover_tab_native_frame(source, source, scale)
                .expect("full-monitor source should project a tab");
            assert_eq!(
                placement.attachment,
                crate::hover_core::HoverTabAttachment::Inset
            );
            let expected = (40.0 * scale).round() as i32;
            assert_eq!(placement.frame.width, expected);
            assert_eq!(placement.frame.height, expected);
            assert_eq!(
                placement.frame.x + placement.frame.width,
                source.x + source.width
            );
        }
    }

    #[test]
    fn native_follow_applies_normalized_offset_before_rc_work_clamping_at_mixed_dpi() {
        let source = WindowFrame {
            x: 300,
            y: 100,
            width: 1200,
            height: 700,
        };
        let work_area = WindowFrame {
            x: 0,
            y: 40,
            width: 1920,
            height: 1040,
        };
        let top = project_hover_tab_native_frame_with_offset(source, work_area, 1.0, 0.0)
            .expect("top placement should fit");
        let center = project_hover_tab_native_frame_with_offset(source, work_area, 1.25, 0.5)
            .expect("center placement should fit");
        let bottom = project_hover_tab_native_frame_with_offset(source, work_area, 1.5, 1.0)
            .expect("bottom placement should fit");
        assert_eq!(top.frame.y, 100);
        assert_eq!(center.frame.height, 50);
        assert_eq!(center.frame.y, 425);
        assert_eq!(bottom.frame.height, 60);
        assert_eq!(bottom.frame.y, 740);
        for placement in [top, center, bottom] {
            assert!(placement.frame.y >= work_area.y);
            assert!(placement.frame.y + placement.frame.height <= work_area.y + work_area.height);
        }
        assert_eq!(
            project_hover_tab_native_frame_with_offset(source, work_area, 1.0, f64::NAN)
                .unwrap()
                .frame
                .y,
            430
        );
    }

    #[test]
    fn native_hover_attachment_rejects_detached_or_stale_generation_and_replaces_token() {
        let attachment = NativeHoverTabAttachment {
            token: 42,
            source_hwnd: 0x1234,
            pill_hwnd: 0x5678,
            generation: 9,
            topmost_fallback: true,
        };
        assert!(native_attachment_is_current(Some(attachment), 42, 9));
        assert!(!native_attachment_is_current(None, 42, 9));
        assert!(!native_attachment_is_current(Some(attachment), 42, 10));

        let replacement = attachment
            .replace_token(42, 43)
            .expect("matching token should preserve the follower attachment");
        assert_eq!(replacement.token, 43);
        assert_eq!(replacement.source_hwnd, attachment.source_hwnd);
        assert_eq!(replacement.pill_hwnd, attachment.pill_hwnd);
        assert_eq!(replacement.generation, attachment.generation);
        assert_eq!(replacement.topmost_fallback, attachment.topmost_fallback);
        assert!(attachment.replace_token(41, 43).is_none());
    }

    #[test]
    fn native_geometry_is_always_40_by_40() {
        let frame = WindowFrame {
            x: 0,
            y: 20,
            width: 1920,
            height: 1000,
        };
        let presentation =
            hover_tab_presentation(9, frame, MonitorBounds::new(0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(presentation.rect.width, 40.0);
        assert_eq!(presentation.rect.height, 40.0);
    }
}
