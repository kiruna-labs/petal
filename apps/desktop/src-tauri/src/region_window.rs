//! Persistent hollow Petal View region sources.
//!
//! This registry is deliberately independent from ordinary window capture:
//! the token identifies a selector window, while its current geometry selects
//! a display ROI. Capture code must resolve the generation before accepting a
//! frame; it must never turn a full-display frame into a region frame later.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::sync_ext::MutexExt;

pub const REGION_WINDOW_LABEL_PREFIX: &str = "region-window-";
pub const REGION_WINDOW_TITLE_PREFIX: &str = "Petal View";
pub const REGION_WINDOW_ROUTE: &str = "region-window.html";

static NEXT_REGION_WINDOW_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CAPTURE_EXCLUSION_LEASE_ID: AtomicU64 = AtomicU64::new(1);

const REGION_PLACEMENT_SETTLED_EVENT: &str = "region-placement-settled";
const REGION_PLACEMENT_RELEASED_EVENT: &str = "region-placement-released";

fn placement_states() -> &'static Mutex<HashMap<String, bool>> {
    static PLACEMENT_STATES: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    PLACEMENT_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_placement_active(label: &str, active: bool) {
    let mut states = placement_states().lock_unpoisoned();
    if active {
        states.insert(label.to_string(), true);
    } else {
        states.remove(label);
    }
}

fn clear_placement_state(label: &str) {
    placement_states().lock_unpoisoned().remove(label);
}

fn emit_placement_settled(app: &tauri::AppHandle, label: &str) {
    use tauri::Emitter;

    // Keep the native state active until the physical mouse-up. A route can
    // mount between the left-button edge and release; retaining this bit
    // keeps that late route opaque instead of letting it enable click-through
    // during the still-active gesture.
    let payload = serde_json::json!({ "selectorLabel": label });
    if let Err(error) = app.emit(REGION_PLACEMENT_SETTLED_EVENT, payload) {
        log::debug!("region window: placement-settled emit failed for {label}: {error}");
    }
}

fn emit_placement_released(app: &tauri::AppHandle, label: &str) {
    use tauri::Emitter;

    clear_placement_state(label);
    let payload = serde_json::json!({ "selectorLabel": label });
    if let Err(error) = app.emit(REGION_PLACEMENT_RELEASED_EVENT, payload) {
        log::debug!("region window: placement-released emit failed for {label}: {error}");
    }
}

fn normalized_user_name(user_name: Option<String>) -> String {
    let normalized = user_name
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character == '\r' || character == '\n' {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        "User".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

/// Create one persistent hollow selector. Its native source is registered
/// immediately after the WebView exists, before it is shown or offered to
/// hover/picker actions, so direct Petal View sharing never depends on a
/// pointer pass over the selector.
#[tauri::command]
pub async fn open_region_window(
    app: tauri::AppHandle,
    user_name: Option<String>,
    follow_cursor: Option<bool>,
) -> Result<String, String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let id = NEXT_REGION_WINDOW_ID.fetch_add(1, Ordering::Relaxed).max(1);
    let label = format!("{REGION_WINDOW_LABEL_PREFIX}{id}");
    let title = format!(
        "{REGION_WINDOW_TITLE_PREFIX}: {} #{id}",
        normalized_user_name(user_name)
    );
    let follow_cursor = follow_cursor.unwrap_or(false);
    if follow_cursor {
        // The route starts opaque before its first async IPC call. Pass the
        // URL flag for an immediate render-side guard, and keep native state
        // too so a route that mounts after the placement thread starts can
        // recover the authoritative initial state.
        set_placement_active(&label, true);
    }
    let route = if follow_cursor {
        format!("{REGION_WINDOW_ROUTE}?placing=1")
    } else {
        REGION_WINDOW_ROUTE.to_string()
    };
    let builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(route.into()))
        .title(&title)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .resizable(true)
        .inner_size(640.0, 400.0)
        .min_inner_size(160.0, 120.0)
        .position(
            120.0 + (id % 5) as f64 * 24.0,
            120.0 + (id % 5) as f64 * 24.0,
        )
        .visible(false);
    // WebView2 fixes browser environment options per user-data folder. Every
    // secondary Windows webview must use the same acceleration arguments as
    // the pre-created main/hover/picker webviews or creation fails with
    // 0x8007139F and no selector appears.
    #[cfg(target_os = "windows")]
    let builder = builder.additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS);
    let window = builder.build().map_err(|error| {
        clear_placement_state(&label);
        format!("region window: create failed: {error}")
    })?;
    #[cfg(target_os = "windows")]
    let native_region_token = match register_windows_region_window(&window, &title) {
        Ok(token) => token,
        Err(error) => {
            clear_placement_state(&label);
            let _ = window.close();
            return Err(error);
        }
    };
    #[cfg(target_os = "macos")]
    let native_region_token = register_macos_region_window(&app, &window, &title);
    log::info!("region window: created label={label} title={title:?}");
    crate::window_source::invalidate_list_cache();

    let cleanup_label = label.clone();
    let cleanup_app = app.clone();
    let _ = window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            let app = cleanup_app.clone();
            let label = cleanup_label.clone();
            tauri::async_runtime::spawn(async move {
                cleanup_region_window_state(&app, &label).await;
            });
        }
    });
    #[cfg(target_os = "windows")]
    if follow_cursor {
        // The placement thread positions the hidden window at the cursor and
        // shows it there; a plain show() here would flash it at the default
        // spot first.
        start_cursor_placement(app, window, native_region_token);
        return Ok(label);
    }
    #[cfg(target_os = "macos")]
    if follow_cursor {
        start_cursor_placement(app, window, native_region_token);
        return Ok(label);
    }
    if let Err(error) = window.show() {
        cleanup_region_window_state(&app, &label).await;
        let _ = window.close();
        return Err(format!("region window: show failed: {error}"));
    }
    Ok(label)
}

/// How long the cursor-placement session waits before auto-cancelling.
const PLACEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const PLACEMENT_POLL: std::time::Duration = std::time::Duration::from_millis(16);

/// Lifecycle state exposed to the route through the native placement event.
/// Keeping this separate from the poll decision makes the consumed-click
/// boundary explicit and testable: a selector can never move back from its
/// terminal placement state into follow-cursor mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlacementLifecycle {
    Active,
    Settled,
    Cancelled,
}

pub(crate) fn placement_lifecycle_after(
    state: PlacementLifecycle,
    decision: PlacementDecision,
) -> PlacementLifecycle {
    match (state, decision) {
        (PlacementLifecycle::Active, PlacementDecision::Settle) => PlacementLifecycle::Settled,
        (PlacementLifecycle::Active, PlacementDecision::Cancel) => PlacementLifecycle::Cancelled,
        (PlacementLifecycle::Active, PlacementDecision::Continue)
        | (PlacementLifecycle::Settled, _)
        | (PlacementLifecycle::Cancelled, _) => state,
    }
}

/// What the placement loop should do on this poll tick. Pure decision so the
/// settle/cancel/timeout logic is unit-testable without live input state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlacementDecision {
    Continue,
    Settle,
    Cancel,
}

pub(crate) fn placement_decision(
    left_down: bool,
    left_was_down: bool,
    right_down: bool,
    right_was_down: bool,
    escape_down: bool,
    timed_out: bool,
) -> PlacementDecision {
    if left_down && !left_was_down {
        return PlacementDecision::Settle;
    }
    if (right_down && !right_was_down) || escape_down || timed_out {
        return PlacementDecision::Cancel;
    }
    PlacementDecision::Continue
}

/// Return the native placement state for a selector route. The route asks
/// once on mount because the placement worker can begin before its WebView has
/// finished loading; a URL-only flag would leave a race between those two
/// lifecycles.
#[tauri::command]
pub fn region_placement_active(window_label: String) -> bool {
    window_label.starts_with(REGION_WINDOW_LABEL_PREFIX)
        && placement_states()
            .lock_unpoisoned()
            .get(&window_label)
            .copied()
            .unwrap_or(false)
}

/// Follow-cursor placement: the selector tracks the system cursor until a
/// left click settles it in place; Escape/right-click or a 60s timeout
/// cancels and destroys the unsettled window (the Destroyed handler cleans
/// the registry).
#[cfg(target_os = "windows")]
fn start_cursor_placement(app: tauri::AppHandle, window: tauri::WebviewWindow, token: u32) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_LBUTTON, VK_RBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };

    let Ok(raw) = window.hwnd() else {
        log::warn!("region window: placement could not resolve HWND; showing at default spot");
        clear_placement_state(window.label());
        unregister(token);
        let _ = crate::windows_capture_target::invalidate(token);
        let _ = window.show();
        return;
    };
    let hwnd_raw = raw.0 as isize;
    let label = window.label().to_string();
    log::info!("region window: cursor placement started ({label})");
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let mut shown = false;
        let mut left_prev = crate::platform::windows::key_is_down(VK_LBUTTON);
        let mut right_prev = crate::platform::windows::key_is_down(VK_RBUTTON);
        loop {
            if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return;
            }
            let Some((cx, cy)) = crate::platform::windows::cursor_position() else {
                std::thread::sleep(PLACEMENT_POLL);
                continue;
            };
            unsafe {
                let mut rect = Default::default();
                if GetWindowRect(hwnd, &mut rect).is_err() {
                    break;
                }
                let width = rect.right - rect.left;
                let height = rect.bottom - rect.top;
                // Move only; showing MUST go through `window.show()` below so
                // tao's internal VISIBLE flag stays in sync. Showing natively
                // (SWP_SHOWWINDOW) desyncs it, and tao's next flag mutation
                // (e.g. the page's click-through toggle) then re-applies
                // "hidden" via ShowWindow(SW_HIDE) -- the selector vanishes
                // while staying alive.
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    cx as i32 - width / 2,
                    cy as i32 - height / 2,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if !shown {
                shown = true;
                if let Err(error) = window.show() {
                    log::warn!("region window: placement show failed: {error}");
                }
            }
            let left = crate::platform::windows::key_is_down(VK_LBUTTON);
            let right = crate::platform::windows::key_is_down(VK_RBUTTON);
            let escape = crate::platform::windows::key_is_down(VK_ESCAPE);
            let timed_out = started.elapsed() >= PLACEMENT_TIMEOUT;
            match placement_decision(left, left_prev, right, right_prev, escape, timed_out) {
                PlacementDecision::Continue => {
                    left_prev = left;
                    right_prev = right;
                    std::thread::sleep(PLACEMENT_POLL);
                }
                PlacementDecision::Settle => {
                    log::info!("region window: cursor placement settled ({label})");
                    emit_placement_settled(&app, &label);
                    while crate::platform::windows::key_is_down(VK_LBUTTON) {
                        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                            clear_placement_state(&label);
                            return;
                        }
                        std::thread::sleep(PLACEMENT_POLL);
                    }
                    emit_placement_released(&app, &label);
                    return;
                }
                PlacementDecision::Cancel => {
                    log::info!("region window: cursor placement cancelled; closing ({label})");
                    cancel_placement(app.clone(), label.clone());
                    return;
                }
            }
        }
        log::warn!("region window: cursor placement lost its HWND; closing ({label})");
        cancel_placement(app, label);
    });
}

#[cfg(target_os = "macos")]
fn mac_button_is_down(button: u32) -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceButtonState(state: u32, button: u32) -> bool;
    }
    // kCGEventSourceStateHIDSystemState = 1.
    unsafe { CGEventSourceButtonState(1, button) }
}

#[cfg(target_os = "macos")]
fn mac_escape_is_down() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceKeyState(state: u32, key: u16) -> bool;
    }
    // HID keycode 53 is Escape; kCGEventSourceStateHIDSystemState = 1.
    unsafe { CGEventSourceKeyState(1, 53) }
}

#[cfg(target_os = "windows")]
fn register_windows_region_window(
    window: &tauri::WebviewWindow,
    title: &str,
) -> Result<u32, String> {
    let raw = window
        .hwnd()
        .map_err(|error| format!("region window: native HWND unavailable: {error}"))?;
    let token = crate::windows_capture_target::register(raw.0 as usize, std::process::id())
        .map_err(|error| format!("region window: native registration failed: {error}"))?;
    let frame = match (window.outer_position(), window.outer_size()) {
        (Ok(position), Ok(size)) => RegionRect::new(
            position.x as f64,
            position.y as f64,
            size.width as f64,
            size.height as f64,
        ),
        _ => RegionRect::new(120.0, 120.0, 640.0, 400.0),
    };
    register(RegionWindowSource::new(
        token,
        std::process::id() as i32,
        title.to_string(),
        frame,
    ));
    log::info!(
        "region window: registered native identity label={title:?} HWND={} frame=({},{} {}x{})",
        raw.0 as usize,
        frame.x,
        frame.y,
        frame.width,
        frame.height
    );
    Ok(token)
}

#[cfg(target_os = "macos")]
fn register_macos_region_window(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    title: &str,
) -> Option<u32> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let window_on_main = window.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        let result = crate::platform::appkit::window_number(&window_on_main);
        let _ = tx.send(result);
    }) {
        log::warn!("region window: native identity dispatch failed: {error}");
        return None;
    }
    let Ok(Ok(token)) = rx.recv() else {
        log::warn!("region window: native identity unavailable; hover attachment will wait for enumeration");
        return None;
    };
    let frame =
        crate::platform::cg::frame_for_window_id(token).unwrap_or(crate::hover_tab::WindowFrame {
            x: 120,
            y: 120,
            width: 640,
            height: 400,
        });
    crate::region_window::register(RegionWindowSource::new(
        token,
        std::process::id() as i32,
        title.to_string(),
        RegionRect::new(
            frame.x as f64,
            frame.y as f64,
            frame.width as f64,
            frame.height as f64,
        ),
    ));
    log::info!(
        "region window: registered native identity label={title:?} CGWindowID={token} frame=({},{} {}x{})",
        frame.x,
        frame.y,
        frame.width,
        frame.height
    );
    Some(token)
}

/// Follow-cursor placement on macOS. AppKit/Tauri window mutations are
/// marshalled to the main thread, while CoreGraphics supplies global cursor
/// and button state from the worker loop.
#[cfg(target_os = "macos")]
fn start_cursor_placement(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    native_region_token: Option<u32>,
) {
    use tauri::{LogicalPosition, Position};

    let label = window.label().to_string();
    // The selector is created at this logical size and is hidden until the
    // first cursor position is applied, so there is no default-position flash.
    const WIDTH: f64 = 640.0;
    const HEIGHT: f64 = 400.0;
    log::info!("region window: cursor placement started ({label})");
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut shown = false;
        let mut left_prev = mac_button_is_down(0);
        let mut right_prev = mac_button_is_down(1);
        loop {
            let Some((cx, cy)) = crate::platform::cg::cursor_position() else {
                std::thread::sleep(PLACEMENT_POLL);
                continue;
            };
            let x = cx - WIDTH / 2.0;
            let y = cy - HEIGHT / 2.0;
            let show = !shown;
            if let Some(token) = native_region_token {
                update_frame(token, RegionRect::new(x, y, WIDTH, HEIGHT));
            }
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let window_on_main = window.clone();
            let dispatch = app.run_on_main_thread(move || {
                let result = window_on_main
                    .set_position(Position::Logical(LogicalPosition { x, y }))
                    .map_err(|error| format!("region window: placement move failed: {error}"))
                    .and_then(|()| {
                        if show {
                            window_on_main.show().map_err(|error| {
                                format!("region window: placement show failed: {error}")
                            })
                        } else {
                            Ok(())
                        }
                    });
                let _ = tx.send(result);
            });
            if dispatch.is_err() || rx.recv().ok().and_then(Result::ok).is_none() {
                log::warn!("region window: cursor placement lost its main-thread window ({label})");
                cancel_placement(app, label);
                return;
            }
            shown = true;

            let left = mac_button_is_down(0);
            let right = mac_button_is_down(1);
            let timed_out = started.elapsed() >= PLACEMENT_TIMEOUT;
            match placement_decision(
                left,
                left_prev,
                right,
                right_prev,
                mac_escape_is_down(),
                timed_out,
            ) {
                PlacementDecision::Continue => {
                    left_prev = left;
                    right_prev = right;
                    std::thread::sleep(PLACEMENT_POLL);
                }
                PlacementDecision::Settle => {
                    log::info!("region window: cursor placement settled ({label})");
                    emit_placement_settled(&app, &label);
                    while mac_button_is_down(0) {
                        std::thread::sleep(PLACEMENT_POLL);
                    }
                    emit_placement_released(&app, &label);
                    return;
                }
                PlacementDecision::Cancel => {
                    log::info!("region window: cursor placement cancelled; closing ({label})");
                    cancel_placement(app, label);
                    return;
                }
            }
        }
    });
}

/// Tear down an unsettled selector through the SAME lifecycle as an explicit
/// user close. `WebviewWindow::close()` dispatched from this background
/// thread proved unreliable (cancelled selectors stayed alive and registered
/// in 013A), so route through the command that meeting-exit teardown already
/// uses successfully.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegionShareState {
    pub active: bool,
}

/// Label-addressed state for the persistent Petal View title-bar actions.
/// `window_id`/capture tokens deliberately do not cross this boundary: a
/// Windows Stop retires the native token and a later action must resolve the
/// selector label again.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegionViewOptionsState {
    pub share_active: bool,
    pub priority: crate::share_priority::SharePriority,
    pub draw_active: bool,
    pub ai_chat_enabled: bool,
    pub ai_chat_active: bool,
    pub controller_name: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegionControlStateChanged {
    pub selector_label: String,
    pub active: bool,
    pub controller_name: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegionViewOptionsChanged {
    pub selector_label: String,
    pub state: RegionViewOptionsState,
}

#[cfg(target_os = "windows")]
fn region_draw_active(token: u32) -> bool {
    crate::windows_share_overlay::share_overlay_draw_active(token)
}

#[cfg(target_os = "macos")]
fn region_draw_active(token: u32) -> bool {
    crate::share_overlay::share_overlay_draw_active(token)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn region_draw_active(_token: u32) -> bool {
    false
}

fn region_view_options_for_token(
    state: &crate::session::SessionState,
    token: u32,
) -> RegionViewOptionsState {
    let ai_chat = crate::ai_chat::commands::ai_chat_settings();
    RegionViewOptionsState {
        share_active: state.is_share_active(token),
        priority: crate::share_priority::current(),
        draw_active: region_draw_active(token),
        ai_chat_enabled: ai_chat.enabled,
        ai_chat_active: crate::ai_chat::commands::ai_chat_is_active(token),
        controller_name: crate::remote_control::active_controller_display_name(state, token),
    }
}

pub(crate) fn emit_region_view_options_changed(
    app: &tauri::AppHandle,
    state: &crate::session::SessionState,
    token: u32,
) {
    let Some(source) = resolve(token) else {
        return;
    };
    let Some(selector_label) = selector_label_from_title(&source.title) else {
        return;
    };
    let payload = RegionViewOptionsChanged {
        selector_label,
        state: region_view_options_for_token(state, token),
    };
    if let Err(error) = tauri::Emitter::emit(app, "region-view-options-changed", payload) {
        log::debug!("region window: options-state emit failed: {error}");
    }
}

pub(crate) fn emit_region_view_options_changed_from_app(app: &tauri::AppHandle, token: u32) {
    use tauri::Manager;
    if let Some(state) = app.try_state::<crate::session::SessionState>() {
        emit_region_view_options_changed(app, &*state, token);
    }
}

/// Project only lifecycle control status onto a stable selector label. The
/// remote-control event itself remains token/ID-rich for existing consumers;
/// this separate event is the only control state the Petal View route reads.
pub(crate) fn emit_region_control_state_for_status(
    app: &tauri::AppHandle,
    status: &crate::remote_control_core::RemoteControlStatus,
) {
    if !matches!(status.status, "active" | "stopped" | "disabled") {
        return;
    }
    let Some(source) = resolve(status.window_id) else {
        return;
    };
    let Some(selector_label) = selector_label_from_title(&source.title) else {
        return;
    };
    use tauri::Manager;
    let current_controller_name =
        app.try_state::<crate::session::SessionState>()
            .and_then(|state| {
                crate::remote_control::active_controller_display_name(&state, status.window_id)
            });
    let active = status.status == "active" || current_controller_name.is_some();
    let controller_name = if active {
        current_controller_name.or_else(|| {
            app.try_state::<crate::session::SessionState>()
                .map(|state| {
                    crate::remote_control::controller_display_name(&state, &status.controller_id)
                })
        })
    } else {
        None
    };
    let payload = RegionControlStateChanged {
        selector_label,
        active,
        controller_name,
    };
    if let Err(error) = tauri::Emitter::emit(app, "region-control-state-changed", payload) {
        log::debug!("region window: control-state emit failed: {error}");
    }
}

fn ensure_region_token(app: &tauri::AppHandle, window_label: &str) -> Result<u32, String> {
    if !window_label.starts_with(REGION_WINDOW_LABEL_PREFIX) {
        return Err("region window: invalid window label".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use tauri::Manager;
        let window = app
            .get_webview_window(window_label)
            .ok_or_else(|| "region window: selector is no longer open".to_string())?;
        let title = window
            .title()
            .map_err(|error| format!("region window: title lookup failed: {error}"))?;
        return register_windows_region_window(&window, &title);
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(token) = registered_token_for_label(window_label) {
            return Ok(token);
        }
        use tauri::Manager;
        let window = app
            .get_webview_window(window_label)
            .ok_or_else(|| "region window: selector is no longer open".to_string())?;
        let title = window
            .title()
            .map_err(|error| format!("region window: title lookup failed: {error}"))?;
        return register_macos_region_window(app, &window, &title)
            .ok_or_else(|| "region window: native selector identity unavailable".to_string());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        token_for_label(window_label)
            .ok_or_else(|| "region window: native selector identity unavailable".to_string())
    }
}

/// Refresh the registry frame after a native selector move/resize. The
/// frontend passes only the stable label; this command re-reads the native
/// geometry and never returns the disposable capture token.
#[tauri::command]
pub async fn sync_region_window_frame(
    app: tauri::AppHandle,
    window_label: String,
) -> Result<(), String> {
    if !window_label.starts_with(REGION_WINDOW_LABEL_PREFIX) {
        return Err("region window: invalid window label".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        use tauri::Manager;
        let window = app
            .get_webview_window(&window_label)
            .ok_or_else(|| "region window: selector is no longer open".to_string())?;
        let title = window
            .title()
            .map_err(|error| format!("region window: title lookup failed: {error}"))?;
        register_windows_region_window(&window, &title).map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        let window = app
            .get_webview_window(&window_label)
            .ok_or_else(|| "region window: selector is no longer open".to_string())?;
        let title = window
            .title()
            .map_err(|error| format!("region window: title lookup failed: {error}"))?;
        register_macos_region_window(&app, &window, &title)
            .map(|_| ())
            .ok_or_else(|| "region window: native selector identity unavailable".to_string())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = app;
        Err("region window: native geometry sync is unsupported on this platform".to_string())
    }
}

/// Authoritative share state for a Petal View selector. The frontend passes a
/// Tauri label, never a cached WGC token; Windows can therefore re-register a
/// fresh opaque token after Stop invalidates the previous one.
#[tauri::command]
pub async fn region_share_state(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
) -> Result<RegionShareState, String> {
    let token = ensure_region_token(&app, &window_label)?;
    Ok(RegionShareState {
        active: state.is_share_active(token),
    })
}

/// Seed all title-bar option state by the stable selector label.
#[tauri::command]
pub async fn region_view_options_state(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
) -> Result<RegionViewOptionsState, String> {
    let token = ensure_region_token(&app, &window_label)?;
    Ok(region_view_options_for_token(&state, token))
}

/// Apply the shared priority preference to the selector's current share, if
/// supported by the platform, while always persisting the default preference.
#[tauri::command]
pub async fn set_region_share_priority(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
    priority: crate::share_priority::SharePriority,
) -> Result<crate::share_priority::SharePriority, String> {
    let token = ensure_region_token(&app, &window_label)?;
    let result =
        crate::share_priority::set_share_priority(app.clone(), priority, Some(token)).await;
    if result.is_ok() {
        emit_region_view_options_changed(&app, &*state, token);
    }
    result
}

/// Toggle Draw on the existing sharer overlay without exposing its disposable
/// capture token to the Petal View route.
#[tauri::command]
pub fn set_region_draw_active(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
    active: bool,
) -> Result<bool, String> {
    let token = ensure_region_token(&app, &window_label)?;
    if !state.is_share_active(token) {
        return Err("Petal View must be actively shared before Draw can change".to_string());
    }
    #[cfg(target_os = "windows")]
    crate::windows_share_overlay::set_region_draw_active(&app, token, active)?;
    #[cfg(target_os = "macos")]
    crate::share_overlay::set_draw_active(&app, token, active)?;
    emit_region_view_options_changed(&app, &*state, token);
    Ok(active)
}

/// Start AI Chat for the currently shared selector, returning the same
/// structured refusal taxonomy as the ordinary hover action.
#[tauri::command]
pub async fn region_ai_chat_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
) -> Result<crate::ai_chat::commands::StartOutcome, String> {
    let token = ensure_region_token(&app, &window_label)?;
    if !state.is_share_active(token) {
        return Err("Petal View must be actively shared before AI Chat can start".to_string());
    }
    let outcome = crate::ai_chat::commands::ai_chat_start(app.clone(), token).await?;
    emit_region_view_options_changed(&app, &*state, token);
    Ok(outcome)
}

/// Stop AI Chat only when the current selector owns the live session. This
/// prevents a stale menu callback from stopping another window's session.
#[tauri::command]
pub fn region_ai_chat_stop(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
) -> Result<bool, String> {
    let token = ensure_region_token(&app, &window_label)?;
    if !state.is_share_active(token) || !crate::ai_chat::commands::ai_chat_is_active(token) {
        return Ok(false);
    }
    crate::ai_chat::commands::ai_chat_stop(app.clone());
    emit_region_view_options_changed(&app, &*state, token);
    Ok(false)
}

/// Toggle one Petal View through the same native session authority as the
/// hover pill/picker. The selector label is the stable address; opaque target
/// tokens remain an internal, disposable capture detail.
#[tauri::command]
pub async fn toggle_region_share(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_label: String,
    color: Option<String>,
) -> Result<bool, String> {
    let token = ensure_region_token(&app, &window_label)?;
    #[cfg(target_os = "windows")]
    {
        let result = crate::session::share_window(app.clone(), state, token, color, None)
            .await
            .map_err(|error| error.to_string());
        emit_region_view_options_changed_from_app(&app, token);
        return result;
    }

    #[cfg(target_os = "macos")]
    {
        let source = resolve(token)
            .ok_or_else(|| "region window: selector source is no longer registered".to_string())?;
        let frame = crate::hover_tab::WindowFrame {
            x: source.frame.x.round() as i32,
            y: source.frame.y.round() as i32,
            width: source.frame.width.round().max(1.0) as i32,
            height: source.frame.height.round().max(1.0) as i32,
        };
        let active =
            crate::hover_tab::toggle_share_for_window_with_color(&app, &state, token, frame, color)
                .await;
        emit_region_view_options_changed(&app, &*state, token);
        return Ok(active);
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (app, state, color);
        Err("region window: sharing is unsupported on this platform".to_string())
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn cancel_placement(app: tauri::AppHandle, label: String) {
    tauri::async_runtime::spawn(async move {
        if let Err(error) = close_region_window(app, label).await {
            log::warn!("region window: placement cancel-close failed: {error}");
        }
    });
}

/// A temporary, label-owned Windows capture-affinity lease for a selector.
/// The HWND is retained only to ensure release cannot affect a later window
/// that happens to reuse the same Tauri label.
#[cfg(target_os = "windows")]
pub(crate) struct SelectorCaptureExclusionLease {
    app: tauri::AppHandle,
    window_label: String,
    hwnd: usize,
    lease_id: u64,
    released: bool,
}

#[cfg(not(target_os = "windows"))]
pub(crate) struct SelectorCaptureExclusionLease;

#[cfg(target_os = "windows")]
fn capture_exclusion_owners() -> &'static Mutex<HashMap<String, u64>> {
    static OWNERS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl SelectorCaptureExclusionLease {
    #[cfg(target_os = "windows")]
    fn restore_captureability(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        use tauri::Manager;
        let mut owners = capture_exclusion_owners().lock_unpoisoned();
        if owners.get(&self.window_label).copied() != Some(self.lease_id) {
            // A newer Share → Stop → Share acquired this selector. Its lease
            // owns the same HWND, so stale teardown must not clear it.
            return;
        }
        let Some(window) = self.app.get_webview_window(&self.window_label) else {
            owners.remove(&self.window_label);
            log::debug!(
                "region window: selector '{}' closed before capture affinity could be restored",
                self.window_label
            );
            return;
        };
        let Ok(raw) = window.hwnd() else {
            owners.remove(&self.window_label);
            log::warn!(
                "region window: selector '{}' HWND unavailable while restoring capture affinity",
                self.window_label
            );
            return;
        };
        let current_hwnd = raw.0 as usize;
        if current_hwnd != self.hwnd {
            owners.remove(&self.window_label);
            log::debug!(
                "region window: selector '{}' HWND changed before capture affinity could be restored",
                self.window_label
            );
            return;
        }
        let hwnd = windows::Win32::Foundation::HWND(raw.0 as *mut core::ffi::c_void);
        if !crate::platform::windows::clear_capture_exclusion(hwnd) {
            log::warn!(
                "region window: WDA_NONE was not accepted for selector '{}'",
                self.window_label
            );
        }
        owners.remove(&self.window_label);
    }

    #[cfg(not(target_os = "windows"))]
    fn restore_captureability(&mut self) {}
}

impl Drop for SelectorCaptureExclusionLease {
    fn drop(&mut self) {
        self.restore_captureability();
    }
}

/// Acquire exclusion for an active Petal View share. Returning `None` keeps
/// the caller on WGC's safe System-indicator fallback.
#[cfg(target_os = "windows")]
pub(crate) fn acquire_selector_capture_exclusion(
    app: &tauri::AppHandle,
    token: u32,
) -> Option<SelectorCaptureExclusionLease> {
    use tauri::Manager;

    let source = resolve(token)?;
    let window_label = selector_label_from_title(&source.title)?;
    let window = app.get_webview_window(&window_label)?;
    let raw = window.hwnd().ok()?;
    let hwnd = windows::Win32::Foundation::HWND(raw.0 as *mut core::ffi::c_void);
    let lease_id = NEXT_CAPTURE_EXCLUSION_LEASE_ID.fetch_add(1, Ordering::Relaxed);
    let mut owners = capture_exclusion_owners().lock_unpoisoned();
    if !crate::platform::windows::set_capture_affinity(hwnd, true) {
        log::warn!(
            "region window: WDA_EXCLUDEFROMCAPTURE was not accepted for selector '{}'",
            window_label
        );
        return None;
    }
    owners.insert(window_label.clone(), lease_id);
    Some(SelectorCaptureExclusionLease {
        app: app.clone(),
        window_label,
        hwnd: raw.0 as usize,
        lease_id,
        released: false,
    })
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn acquire_selector_capture_exclusion(
    _app: &tauri::AppHandle,
    _token: u32,
) -> Option<SelectorCaptureExclusionLease> {
    None
}

/// Release a selector's capture exclusion. The lease is label-addressed and
/// its destructor is a fallback for every startup cancellation/error path.
pub(crate) fn release_selector_capture_exclusion(lease: Option<SelectorCaptureExclusionLease>) {
    drop(lease);
}

/// Close every Petal View selector. Selectors are meeting-scoped surfaces:
/// leaving the room must not leave hollow windows on the desktop. Each close
/// reuses the single-selector lifecycle so its share teardown and registry
/// cleanup stay identical to an explicit user close (idempotent: the
/// Destroyed handler's cleanup is a no-op after the first pass).
pub(crate) async fn close_all_region_windows(app: &tauri::AppHandle) {
    use tauri::Manager;
    let labels = region_window_labels(app.webview_windows().into_keys());
    for label in labels {
        if let Err(error) = close_region_window(app.clone(), label).await {
            log::warn!("region window: meeting-exit close failed: {error}");
        }
    }
}

fn region_window_labels<I>(labels: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut labels: Vec<String> = labels
        .into_iter()
        .filter(|label| label.starts_with(REGION_WINDOW_LABEL_PREFIX))
        .collect();
    labels.sort();
    labels
}

/// Close one selector through the share lifecycle before destroying its
/// native window. Unsharing deliberately does not call this function.
#[tauri::command]
pub async fn close_region_window(
    app: tauri::AppHandle,
    window_label: String,
) -> Result<(), String> {
    if !window_label.starts_with(REGION_WINDOW_LABEL_PREFIX) {
        return Err("region window: invalid window label".to_string());
    }
    cleanup_region_window_state(&app, &window_label).await;
    use tauri::Manager;
    if let Some(window) = app.get_webview_window(&window_label) {
        window
            .close()
            .map_err(|error| format!("region window: close failed: {error}"))?;
    }
    Ok(())
}

async fn cleanup_region_window_state(app: &tauri::AppHandle, window_label: &str) {
    clear_placement_state(window_label);
    // Only act on a source that is still registered. Parsing the numeric
    // suffix after the first cleanup could accidentally target an unrelated
    // ordinary Windows token that later reused the same number.
    let Some(token) = registered_token_for_label(window_label) else {
        return;
    };

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        use tauri::Manager;
        if let Some(state) = app.try_state::<crate::session::SessionState>() {
            if state.is_share_active(token) {
                #[cfg(target_os = "windows")]
                let result = crate::session::stop_share_token(app, state.inner(), token).await;
                #[cfg(target_os = "macos")]
                let result = crate::session::stop_share(app, state.inner(), token).await;
                if let Err(error) = result {
                    log::warn!("region window: failed to stop share {token} before close: {error}");
                }
                emit_region_share_state(app, token, false);
            }
        }
    }

    if unregister(token).is_some() {
        log::info!("region window: unregistered selector {token} ({window_label})");
    }
    #[cfg(target_os = "windows")]
    if crate::windows_capture_target::invalidate(token) {
        log::info!("region window: invalidated selector token {token} ({window_label})");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

fn registered_token_for_label(label: &str) -> Option<u32> {
    registry()
        .lock_unpoisoned()
        .sources
        .iter()
        .find_map(|(token, source)| {
            (selector_label_from_title(&source.title).as_deref() == Some(label)).then_some(*token)
        })
}

fn token_for_label(label: &str) -> Option<u32> {
    registered_token_for_label(label).or_else(|| {
        label
            .strip_prefix(REGION_WINDOW_LABEL_PREFIX)?
            .parse::<u32>()
            .ok()
            .filter(|id| *id > 0)
    })
}

/// A global logical rectangle in desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RegionRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl RegionRect {
    pub(crate) const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub(crate) fn is_positive(self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && self.x.is_finite()
            && self.y.is_finite()
            && self.width > 0.0
            && self.height > 0.0
    }

    pub(crate) fn contains(self, cursor: (f64, f64)) -> bool {
        self.is_positive()
            && cursor.0 >= self.x
            && cursor.0 < self.right()
            && cursor.1 >= self.y
            && cursor.1 < self.bottom()
    }

    pub(crate) fn right(self) -> f64 {
        self.x + self.width
    }

    pub(crate) fn bottom(self) -> f64 {
        self.y + self.height
    }

    pub(crate) fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        let result = Self::new(x, y, right - x, bottom - y);
        result.is_positive().then_some(result)
    }
}

/// A display's global logical frame and point-to-pixel scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RegionDisplay {
    pub id: u32,
    pub frame: RegionRect,
    pub scale: f64,
}

/// The portion of a selector that is visible on its owning display, plus the
/// selector-sized output canvas needed to preserve remote geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClippedPhysicalRegion {
    pub roi: PhysicalRegion,
    pub output_width: u32,
    pub output_height: u32,
    pub offset_x: u32,
    pub offset_y: u32,
}

impl RegionDisplay {
    pub(crate) fn contains(self, region: RegionRect) -> bool {
        region.is_positive()
            && region.x >= self.frame.x
            && region.y >= self.frame.y
            && region.right() <= self.frame.right()
            && region.bottom() <= self.frame.bottom()
    }

    pub(crate) fn local_roi(self, region: RegionRect) -> Option<RegionRect> {
        self.contains(region).then(|| {
            RegionRect::new(
                region.x - self.frame.x,
                region.y - self.frame.y,
                region.width,
                region.height,
            )
        })
    }

    pub(crate) fn physical_roi(self, region: RegionRect) -> Option<PhysicalRegion> {
        self.clipped_physical_roi(region).map(|clipped| clipped.roi)
    }

    pub(crate) fn clipped_physical_roi(self, region: RegionRect) -> Option<ClippedPhysicalRegion> {
        let overlap = self.frame.intersection(region)?;
        let scale = self.scale.max(0.01);
        let output_width = even_dimension((region.width * scale).ceil().max(0.0) as u32);
        let output_height = even_dimension((region.height * scale).ceil().max(0.0) as u32);
        let x = ((overlap.x - self.frame.x) * scale).floor().max(0.0) as u32;
        let y = ((overlap.y - self.frame.y) * scale).floor().max(0.0) as u32;
        let right = ((overlap.right() - self.frame.x) * scale)
            .ceil()
            .max(x as f64) as u32;
        let bottom = ((overlap.bottom() - self.frame.y) * scale)
            .ceil()
            .max(y as f64) as u32;
        let roi_width = even_dimension(right.saturating_sub(x));
        let roi_height = even_dimension(bottom.saturating_sub(y));
        let offset_x = ((overlap.x - region.x) * scale).floor().max(0.0) as u32;
        let offset_y = ((overlap.y - region.y) * scale).floor().max(0.0) as u32;
        (output_width > 0
            && output_height > 0
            && roi_width > 0
            && roi_height > 0
            && offset_x.saturating_add(roi_width) <= output_width
            && offset_y.saturating_add(roi_height) <= output_height)
            .then_some(ClippedPhysicalRegion {
                roi: PhysicalRegion {
                    x,
                    y,
                    width: roi_width,
                    height: roi_height,
                },
                output_width,
                output_height,
                offset_x,
                offset_y,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhysicalRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

fn even_dimension(value: u32) -> u32 {
    value.saturating_sub(value % 2)
}

/// A monotonically increasing source configuration generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RegionCaptureGeneration(pub u64);

impl RegionCaptureGeneration {
    pub(crate) const INITIAL: Self = Self(1);

    pub(crate) fn next(self) -> Self {
        Self(self.0.saturating_add(1).max(1))
    }
}

/// Shared cadence for native region geometry checks. Both platforms use the
/// same latest-wins interval so a drag cannot turn every delivered frame into
/// a capture reconfiguration.
pub(crate) const REGION_GEOMETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(50);

/// A region configuration that has been applied to the native capture stream
/// but is not yet proven by a matching delivered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingRegionConfiguration {
    pub generation: u64,
    pub expected_width: u32,
    pub expected_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionUpdateDecision {
    Noop,
    ApplyLatest { generation: u64 },
    WaitForProof { generation: u64 },
    RetryLatest { generation: u64 },
}

/// Decide how a native capture owner should handle the newest registered
/// region. Intermediate rectangles are intentionally not queued: once the
/// current configuration is proven, the owner applies only the latest one.
pub(crate) fn region_update_decision(
    active_generation: u64,
    newest_generation: u64,
    pending: Option<PendingRegionConfiguration>,
    pending_elapsed: Option<std::time::Duration>,
) -> RegionUpdateDecision {
    if newest_generation <= active_generation {
        return RegionUpdateDecision::Noop;
    }
    let Some(pending) = pending else {
        return RegionUpdateDecision::ApplyLatest {
            generation: newest_generation,
        };
    };
    let elapsed = pending_elapsed.unwrap_or_default();
    if elapsed < REGION_PROOF_TIMEOUT {
        return RegionUpdateDecision::WaitForProof {
            generation: pending.generation,
        };
    }
    RegionUpdateDecision::RetryLatest {
        generation: newest_generation,
    }
}

/// Maximum time to wait for a matching native frame before retrying the newest
/// geometry. It allows several frames even at the 4fps reduced-quality floor.
pub(crate) const REGION_PROOF_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);

/// Return whether a region geometry check is due. `None` means the first check
/// is due immediately; the caller records the check time only when it runs.
pub(crate) fn region_geometry_due(
    last_check: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    last_check.is_none_or(|last| now.duration_since(last) >= REGION_GEOMETRY_INTERVAL)
}

/// The only source kind introduced by this module. It is intentionally not an
/// ordinary `Window`: the capture adapter must choose a display ROI directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionSourceKind {
    DisplayRegion,
}

pub(crate) fn is_region_window_title(title: &str) -> bool {
    let Some(suffix) = title.strip_prefix(REGION_WINDOW_TITLE_PREFIX) else {
        return false;
    };
    suffix.is_empty()
        || suffix
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_whitespace() || matches!(ch, ':' | '-' | '#'))
}

pub(crate) fn is_owned_region_window(title: &str, owner_pid: i32, self_pid: i32) -> bool {
    owner_pid == self_pid && is_region_window_title(title)
}

/// The native Tauri label (`region-window-N`) encoded in a selector's title
/// (`Petal View: <name> #N`). Both are generated from the same counter in
/// `open_region_window`, so this recovers the label without extra registry
/// state -- needed to route per-window events (e.g. the outside-display
/// warning) when capture TOKENS diverge from selector numbers (#015A: token 6
/// vs "region-window-2").
pub(crate) fn selector_label_from_title(title: &str) -> Option<String> {
    let id = title.rsplit_once('#')?.1.trim().parse::<u32>().ok()?;
    Some(format!("{REGION_WINDOW_LABEL_PREFIX}{id}"))
}

#[derive(Debug, Clone)]
pub(crate) struct RegionWindowSource {
    pub token: u32,
    pub owner_pid: i32,
    pub title: String,
    pub frame: RegionRect,
    pub generation: RegionCaptureGeneration,
    pub display: Option<RegionDisplay>,
    pub active_share: bool,
    pub outside_display: bool,
}

impl RegionWindowSource {
    pub(crate) fn new(token: u32, owner_pid: i32, title: String, frame: RegionRect) -> Self {
        Self {
            token,
            owner_pid,
            title,
            frame,
            generation: RegionCaptureGeneration::INITIAL,
            display: None,
            active_share: false,
            outside_display: false,
        }
    }

    pub(crate) fn update_frame(&mut self, frame: RegionRect) -> RegionCaptureGeneration {
        if self.frame != frame {
            self.frame = frame;
            self.generation = self.generation.next();
        }
        self.generation
    }

    pub(crate) fn set_display(&mut self, display: Option<RegionDisplay>) {
        if self.display != display {
            self.display = display;
            self.generation = self.generation.next();
        }
    }
}

#[derive(Default)]
struct Registry {
    sources: HashMap<u32, RegionWindowSource>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

pub(crate) fn register(source: RegionWindowSource) {
    let mut guard = registry().lock_unpoisoned();
    let selector_label = selector_label_from_title(&source.title);
    // A Windows share stop invalidates the opaque capture token, then the
    // next direct/hover action allocates a fresh token for the same selector.
    // Remove the retired mapping before inserting the replacement so label
    // lookup can never choose an arbitrary stale entry from the HashMap.
    if let Some(selector_label) = selector_label.as_deref() {
        guard.sources.retain(|token, existing| {
            *token == source.token
                || selector_label_from_title(&existing.title).as_deref() != Some(selector_label)
        });
    }
    if let Some(existing) = guard.sources.get_mut(&source.token) {
        existing.owner_pid = source.owner_pid;
        existing.title = source.title;
        existing.update_frame(source.frame);
        return;
    }
    guard.sources.insert(source.token, source);
}

pub(crate) fn resolve(token: u32) -> Option<RegionWindowSource> {
    registry().lock_unpoisoned().sources.get(&token).cloned()
}

/// Hit-test only explicitly registered Petal View selectors. This is a
/// fallback for macOS WindowServer snapshots that omit a transparent selector
/// or report a different row identity than ScreenCaptureKit. It does not make
/// arbitrary Petal-owned windows shareable.
pub(crate) fn cursor_inside_registered_region(cursor: (f64, f64)) -> bool {
    registry()
        .lock_unpoisoned()
        .sources
        .values()
        .any(|source| source.frame.contains(cursor))
}

pub(crate) fn registered_hit_test(cursor: (f64, f64)) -> Option<(u32, RegionRect)> {
    registry()
        .lock_unpoisoned()
        .sources
        .iter()
        .filter(|(_, source)| source.frame.contains(cursor))
        .max_by_key(|(token, _)| **token)
        .map(|(token, source)| (*token, source.frame))
}

pub(crate) fn update_frame(token: u32, frame: RegionRect) -> Option<RegionCaptureGeneration> {
    registry()
        .lock_unpoisoned()
        .sources
        .get_mut(&token)
        .map(|source| source.update_frame(frame))
}

pub(crate) fn update_display(
    token: u32,
    display: Option<RegionDisplay>,
) -> Option<RegionCaptureGeneration> {
    registry()
        .lock_unpoisoned()
        .sources
        .get_mut(&token)
        .map(|source| {
            source.set_display(display);
            source.generation
        })
}

pub(crate) fn set_active_share(token: u32, active: bool) -> bool {
    let mut guard = registry().lock_unpoisoned();
    let Some(source) = guard.sources.get_mut(&token) else {
        return false;
    };
    source.active_share = active;
    true
}

/// Emit selector-label-keyed share state. Capture tokens are intentionally
/// included only as diagnostic context; a route must address its own state by
/// the stable Tauri selector label because Windows retires/reissues tokens.
pub(crate) fn emit_region_share_state(app: &tauri::AppHandle, token: u32, active: bool) {
    let Some(source) = resolve(token) else {
        return;
    };
    let Some(selector_label) = selector_label_from_title(&source.title) else {
        return;
    };
    use tauri::Emitter;
    let payload = serde_json::json!({
        "windowId": token,
        "selectorLabel": selector_label,
        "active": active,
    });
    if let Err(error) = app.emit("region-share-state-changed", payload) {
        log::debug!("region window: share-state emit failed for {selector_label}: {error}");
    }
    // Keep the title-bar options in sync for starts, stops, autonomous
    // cleanup, and Share → Stop → Share token rebinding.
    emit_region_view_options_changed_from_app(app, token);
}

/// Warning-lifecycle classification for a region selector: is it outside its
/// latched owning display right now? Returns `None` while no display is
/// latched (capture not prepared yet) -- never warn on unknown state.
///
/// macOS semantics differ from Windows by necessity: SCK `sourceRect` cannot
/// express a padded canvas for partial overlap, so `local_roi` requires full
/// containment and any non-contained frame holds the last good configuration
/// and raises the banner until the selector is fully back inside.
pub(crate) fn classify_outside_display(
    display: Option<RegionDisplay>,
    frame: RegionRect,
) -> Option<bool> {
    let display = display?;
    Some(!display.contains(frame))
}

pub(crate) fn set_outside_display(token: u32, outside: bool) -> Option<bool> {
    let mut guard = registry().lock_unpoisoned();
    let source = guard.sources.get_mut(&token)?;
    if source.outside_display == outside {
        return None;
    }
    source.outside_display = outside;
    Some(outside)
}

pub(crate) fn unregister(token: u32) -> Option<RegionWindowSource> {
    registry().lock_unpoisoned().sources.remove(&token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_display_classification_needs_a_latched_display_and_containment() {
        let display = RegionDisplay {
            id: 1,
            frame: RegionRect::new(0.0, 0.0, 1000.0, 800.0),
            scale: 1.0,
        };
        // No latched display yet -- never warn.
        assert_eq!(
            classify_outside_display(None, RegionRect::new(10.0, 10.0, 50.0, 50.0)),
            None
        );
        // Fully contained -- inside.
        assert_eq!(
            classify_outside_display(Some(display), RegionRect::new(10.0, 10.0, 50.0, 50.0)),
            Some(false)
        );
        // Partial overlap counts as outside on macOS: sourceRect cannot pad,
        // so the last good configuration is held behind the banner.
        assert_eq!(
            classify_outside_display(Some(display), RegionRect::new(980.0, 780.0, 50.0, 50.0)),
            Some(true)
        );
        // Fully off-display -- outside.
        assert_eq!(
            classify_outside_display(Some(display), RegionRect::new(2000.0, 10.0, 50.0, 50.0)),
            Some(true)
        );
    }

    #[test]
    fn region_rect_contains_uses_half_open_edges() {
        let frame = RegionRect::new(-10.0, 20.0, 100.0, 80.0);
        assert!(frame.contains((-10.0, 20.0)));
        assert!(frame.contains((89.999, 99.999)));
        assert!(!frame.contains((90.0, 50.0)));
        assert!(!frame.contains((20.0, 100.0)));
        assert!(!frame.contains((f64::NAN, 40.0)));
        assert!(!RegionRect::new(0.0, 0.0, 0.0, 10.0).contains((0.0, 0.0)));
    }

    #[test]
    fn selector_label_decodes_from_title_suffix() {
        assert_eq!(
            selector_label_from_title("Petal View: winful #12"),
            Some("region-window-12".to_string())
        );
        assert_eq!(selector_label_from_title("Petal View: winful"), None);
        assert_eq!(selector_label_from_title("Petal View: a #b"), None);
    }

    #[test]
    fn selector_rebinding_keeps_one_live_source_per_label() {
        let first_token = 4_000_000_001;
        let second_token = 4_000_000_002;
        let title = "Petal View: Rebound #4000000001".to_string();
        register(RegionWindowSource::new(
            first_token,
            1,
            title.clone(),
            RegionRect::new(0.0, 0.0, 100.0, 100.0),
        ));
        register(RegionWindowSource::new(
            second_token,
            1,
            title,
            RegionRect::new(1.0, 1.0, 100.0, 100.0),
        ));
        assert!(resolve(first_token).is_none());
        assert_eq!(
            resolve(second_token).map(|source| source.frame.x),
            Some(1.0)
        );
        assert_eq!(
            token_for_label("region-window-4000000001"),
            Some(second_token)
        );
        unregister(second_token);
    }

    #[test]
    fn placement_state_is_label_scoped_and_clears_after_release() {
        let first = "region-window-placement-test-a";
        let second = "region-window-placement-test-b";
        clear_placement_state(first);
        clear_placement_state(second);

        set_placement_active(first, true);
        set_placement_active(second, true);
        assert!(region_placement_active(first.to_string()));
        assert!(region_placement_active(second.to_string()));

        clear_placement_state(first);
        assert!(!region_placement_active(first.to_string()));
        assert!(region_placement_active(second.to_string()));
        clear_placement_state(second);
    }

    #[test]
    fn placement_lifecycle_is_terminal_after_settle_or_cancel() {
        use PlacementDecision::*;
        use PlacementLifecycle::*;

        assert_eq!(placement_lifecycle_after(Active, Continue), Active);
        assert_eq!(placement_lifecycle_after(Active, Settle), Settled);
        assert_eq!(placement_lifecycle_after(Active, Cancel), Cancelled);
        assert_eq!(placement_lifecycle_after(Settled, Continue), Settled);
        assert_eq!(placement_lifecycle_after(Settled, Cancel), Settled);
        assert_eq!(placement_lifecycle_after(Cancelled, Continue), Cancelled);
        assert_eq!(placement_lifecycle_after(Cancelled, Settle), Cancelled);
    }

    #[test]
    fn cursor_placement_settles_on_left_click_and_cancels_otherwise() {
        use PlacementDecision::*;
        // Left click = settle (edge, not hold).
        assert_eq!(
            placement_decision(true, false, false, false, false, false),
            Settle
        );
        assert_eq!(
            placement_decision(true, true, false, false, false, false),
            Continue
        );
        // Right click / Escape / timeout cancel.
        assert_eq!(
            placement_decision(false, false, true, false, false, false),
            Cancel
        );
        assert_eq!(
            placement_decision(false, true, true, false, false, false),
            Cancel
        );
        assert_eq!(
            placement_decision(false, false, true, true, false, false),
            Continue
        );
        assert_eq!(
            placement_decision(false, false, false, false, true, false),
            Cancel
        );
        assert_eq!(
            placement_decision(false, false, false, false, false, true),
            Cancel
        );
        assert_eq!(
            placement_decision(false, false, false, false, false, false),
            Continue
        );
    }

    #[test]
    fn meeting_exit_closes_only_region_windows() {
        let labels = region_window_labels([
            "main".to_string(),
            "hover-tab".to_string(),
            "region-window-2".to_string(),
            "region-window-10".to_string(),
            "window-picker".to_string(),
            "petal-pointer-x-3".to_string(),
        ]);
        assert_eq!(labels, vec!["region-window-10", "region-window-2"]);
        assert!(region_window_labels(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn title_requires_petals_region_prefix() {
        assert!(is_region_window_title("Petal View"));
        assert!(is_region_window_title("Petal View: Jordan Kim #1"));
        assert!(is_region_window_title("Petal View 7"));
        assert!(is_region_window_title("Petal View-7"));
        assert!(!is_region_window_title("Petal Viewpoint"));
        assert!(!is_region_window_title("Petal"));
    }

    #[test]
    fn user_name_normalization_keeps_titles_single_line_and_bounded() {
        assert_eq!(
            normalized_user_name(Some(" Jordan\nKim ".to_string())),
            "Jordan Kim"
        );
        assert_eq!(normalized_user_name(Some("   ".to_string())), "User");
        assert_eq!(normalized_user_name(None), "User");
        assert_eq!(normalized_user_name(Some("x".repeat(100))), "x".repeat(80));
    }

    #[test]
    fn ownership_requires_current_process() {
        assert!(is_owned_region_window("Petal View 1", 42, 42));
        assert!(!is_owned_region_window("Petal View 1", 7, 42));
        assert!(!is_owned_region_window("Petal Panel", 42, 42));
    }

    #[test]
    fn display_roi_uses_local_even_physical_coordinates() {
        let display = RegionDisplay {
            id: 2,
            frame: RegionRect::new(-1200.0, 40.0, 1920.0, 1080.0),
            scale: 1.5,
        };
        let region = RegionRect::new(-1100.0, 140.0, 301.0, 201.0);
        assert_eq!(
            display.local_roi(region),
            Some(RegionRect::new(100.0, 100.0, 301.0, 201.0))
        );
        assert_eq!(
            display.physical_roi(region),
            Some(PhysicalRegion {
                x: 150,
                y: 150,
                width: 452,
                height: 302,
            })
        );
    }

    #[test]
    fn roi_must_be_inside_one_display() {
        let display = RegionDisplay {
            id: 1,
            frame: RegionRect::new(0.0, 0.0, 1000.0, 800.0),
            scale: 1.0,
        };
        assert!(display
            .local_roi(RegionRect::new(10.0, 10.0, 20.0, 20.0))
            .is_some());
        assert!(display
            .local_roi(RegionRect::new(990.0, 10.0, 20.0, 20.0))
            .is_none());
    }

    #[test]
    fn clipped_roi_preserves_selector_canvas_and_overlap_offset() {
        let display = RegionDisplay {
            id: 1,
            frame: RegionRect::new(0.0, 0.0, 1000.0, 800.0),
            scale: 1.0,
        };
        assert_eq!(
            display.clipped_physical_roi(RegionRect::new(950.0, 700.0, 100.0, 100.0)),
            Some(ClippedPhysicalRegion {
                roi: PhysicalRegion {
                    x: 950,
                    y: 700,
                    width: 50,
                    height: 100,
                },
                output_width: 100,
                output_height: 100,
                offset_x: 0,
                offset_y: 0,
            })
        );
    }

    #[test]
    fn region_window_labels_decode_only_positive_numeric_ids() {
        assert_eq!(token_for_label("region-window-7"), Some(7));
        assert_eq!(token_for_label("region-window-0"), None);
        assert_eq!(token_for_label("region-window-nope"), None);
        assert_eq!(token_for_label("main"), None);
    }

    #[test]
    fn cleanup_lookup_never_falls_back_to_an_unregistered_numeric_token() {
        let label = "region-window-987654321";
        assert_eq!(registered_token_for_label(label), None);
        assert_eq!(token_for_label(label), Some(987654321));
    }

    #[test]
    fn frame_updates_advance_generation_only_when_changed() {
        let frame = RegionRect::new(0.0, 0.0, 100.0, 100.0);
        let mut source = RegionWindowSource::new(1, 9, "Petal View 1".into(), frame);
        assert_eq!(source.generation, RegionCaptureGeneration::INITIAL);
        assert_eq!(source.update_frame(frame), RegionCaptureGeneration::INITIAL);
        assert_eq!(
            source.update_frame(RegionRect::new(1.0, 0.0, 100.0, 100.0)),
            RegionCaptureGeneration(2)
        );
    }

    #[test]
    fn region_geometry_check_is_due_only_at_the_shared_cadence() {
        let start = std::time::Instant::now();
        assert!(region_geometry_due(None, start));
        assert!(!region_geometry_due(
            Some(start),
            start + REGION_GEOMETRY_INTERVAL - std::time::Duration::from_millis(1)
        ));
        assert!(region_geometry_due(
            Some(start),
            start + REGION_GEOMETRY_INTERVAL
        ));
    }

    #[test]
    fn region_updates_keep_one_pending_generation_and_catch_up_to_latest() {
        let pending = PendingRegionConfiguration {
            generation: 7,
            expected_width: 640,
            expected_height: 400,
        };
        assert_eq!(
            region_update_decision(0, 8, Some(pending), None),
            RegionUpdateDecision::WaitForProof { generation: 7 }
        );
        assert_eq!(
            region_update_decision(
                0,
                8,
                Some(pending),
                Some(REGION_PROOF_TIMEOUT - std::time::Duration::from_millis(1))
            ),
            RegionUpdateDecision::WaitForProof { generation: 7 }
        );
        assert_eq!(
            region_update_decision(0, 8, Some(pending), Some(REGION_PROOF_TIMEOUT)),
            RegionUpdateDecision::RetryLatest { generation: 8 }
        );
        // Once the pending configuration is proven, the next check applies
        // only the newest generation; intermediate rectangles are discarded.
        assert_eq!(
            region_update_decision(7, 12, None, None),
            RegionUpdateDecision::ApplyLatest { generation: 12 }
        );
        assert_eq!(
            region_update_decision(12, 12, None, None),
            RegionUpdateDecision::Noop
        );
    }

    #[test]
    fn continuous_resize_keeps_proven_frames_at_every_capture_rate() {
        for fps in [60u32, 30, 15, 4] {
            let frame_interval = std::time::Duration::from_secs_f64(1.0 / f64::from(fps));
            let start = std::time::Instant::now();
            let mut active_generation = RegionCaptureGeneration::INITIAL.0;
            let mut pending: Option<(PendingRegionConfiguration, std::time::Instant)> = None;
            let mut newest_generation = active_generation;
            let mut next_frame = frame_interval;
            let mut configuration_starts = 0u32;
            let mut accepted_frames = 0u32;
            let mut max_pending = 0u32;

            for elapsed_ms in 0..=2_000u64 {
                let now = start + std::time::Duration::from_millis(elapsed_ms);
                if elapsed_ms % REGION_GEOMETRY_INTERVAL.as_millis() as u64 == 0 {
                    newest_generation = newest_generation.saturating_add(1);
                    let pending_for_decision = pending.map(|(configuration, _)| configuration);
                    let elapsed = pending.map(|(_, began)| now.duration_since(began));
                    match region_update_decision(
                        active_generation,
                        newest_generation,
                        pending_for_decision,
                        elapsed,
                    ) {
                        RegionUpdateDecision::ApplyLatest { generation }
                        | RegionUpdateDecision::RetryLatest { generation } => {
                            pending = Some((
                                PendingRegionConfiguration {
                                    generation,
                                    expected_width: 640,
                                    expected_height: 400,
                                },
                                now,
                            ));
                            configuration_starts += 1;
                        }
                        RegionUpdateDecision::Noop | RegionUpdateDecision::WaitForProof { .. } => {}
                    }
                }
                if now.duration_since(start) >= next_frame {
                    next_frame += frame_interval;
                    if let Some((configuration, _)) = pending.take() {
                        active_generation = configuration.generation;
                    }
                    accepted_frames += 1;
                }
                max_pending = max_pending.max(u32::from(pending.is_some()));
            }

            assert!(accepted_frames > 0, "{fps}fps resize accepted no frames");
            assert!(
                max_pending <= 1,
                "{fps}fps resize queued multiple generations"
            );
            let max_configuration_starts = 1 + 2_000 / REGION_GEOMETRY_INTERVAL.as_millis() as u32;
            assert!(
                configuration_starts <= max_configuration_starts,
                "{fps}fps resize started {configuration_starts} configurations in 2s"
            );
        }
    }
}
