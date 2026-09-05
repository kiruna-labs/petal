//! Hover "share tab" — one fixed 40px right-edge rail button that follows the
//! current eligible window. Primary activation shares/stops directly; the
//! native options menu owns secondary controls without changing geometry.
//!
//! Ported from takt's `capture_tab.rs` + `window_picker.rs` (see those files
//! for the original design notes). Adapted for Petal:
//!   - No dependency on `window_source.rs` (the sibling ScreenCaptureKit-based
//!     enumeration module) — the cursor hit-test here uses raw CoreGraphics
//!     (`CGWindowListCopyWindowInfo`) directly, exactly like takt's
//!     `window_picker::cg` module, so this compiles and works independently
//!     of whether that module has landed.
//!   - Share/unshare controls instead of takt's one-shot screenshot action.
//!   - Emits events to a dedicated SvelteKit route's webview (`hover-tab`)
//!     instead of takt's Vite multi-entry `src/tab/index.html`.
//!
//! macOS-only; no-op stubs elsewhere (matching takt's platform-gating style).

use crate::sync_ext::MutexExt;
#[cfg(target_os = "macos")]
use screencapturekit::stream::content_filter::SCContentFilter;
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};
use tauri::AppHandle;

pub use crate::platform::cg::WindowFrame;

// Platform-neutral hover-tab core (payload types, geometry math, hit-test
// classification, share-state/color bookkeeping) — see `hover_core.rs`. Re-
// exported here so the macOS call sites and tests below keep their existing
// `super::`/`platform::` paths unchanged.
pub(crate) use crate::hover_core::{
    autotest_share_color, current_hover_presentation, cursor_over_tab,
    hover_tab_panel_logical_size, hover_tab_presentation, hover_tab_presentation_with_offset,
    is_shared, last_hover_update, normalize_share_color, remember_share_color,
    remembered_share_color, set_last_hover_update, share_color_or_default, with_share_state,
    EnsureBorderResult, EnsureOverlayResult, HoverTabAttachment, HoverTabDragPhase,
    HoverTabPresentation, HoverTabRect, HoverTabUpdate, MonitorBounds, ShareState,
    ShareStateChanged, DEFAULT_HOVER_TAB_VERTICAL_OFFSET, DEFAULT_SHARE_COLOR,
    HOVER_TAB_COMPACT_HEIGHT, HOVER_TAB_COMPACT_WIDTH, HOVER_TAB_DRAG_THRESHOLD_PX,
    HOVER_TAB_LABEL, HOVER_TAB_WINDOW_TITLE,
};

/// Payload for the `share-error` event -- emitted to the same `hover-tab`
/// webview as `hover-tab-update`/`hover-tab-hide` (see `HOVER_TAB_LABEL`),
/// following that existing event-channel pattern rather than inventing a
/// second one. Fired when starting or stopping a real capture+publish fails
/// (e.g. Screen Recording permission revoked mid-session, LiveKit connect
/// failure) -- `window_id` lets the frontend un-toggle just that pill/tab
/// rather than guessing which one failed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareErrorPayload {
    pub window_id: u32,
    /// True if the failure happened while trying to START sharing (the
    /// caller's optimistic toggle should be rolled back to "not shared");
    /// false if it happened while STOPPING (state is already removed
    /// regardless, since `session::stop_share` always clears bookkeeping
    /// even when the underlying unpublish call itself errors).
    pub was_starting: bool,
    pub error: crate::session::ShareSessionError,
}

/// `HoverTabUpdate`/`ShareStateChanged`/constants/`ShareState` bookkeeping:
/// moved to `hover_core.rs`, re-exported above.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShareBorderStartTiming {
    Optimistic,
    AfterPublish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShareStartSurface {
    HoverTab,
    SystemPicker,
}

fn share_border_start_timing(surface: ShareStartSurface) -> ShareBorderStartTiming {
    match surface {
        ShareStartSurface::HoverTab => ShareBorderStartTiming::Optimistic,
        ShareStartSurface::SystemPicker => ShareBorderStartTiming::AfterPublish,
    }
}

// (SHARE_STATE / LAST_HOVER_UPDATE / LAST_SHARE_COLOR statics + `ShareState`
// impl moved to `hover_core.rs` — see the re-exports at the top of this file.)
#[cfg(target_os = "macos")]
static MENU_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(target_os = "macos")]
static SHARE_TOGGLE_LOCKS: OnceLock<Mutex<HashMap<u32, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();
#[cfg(target_os = "macos")]
static SHARE_FOCUS_ATTEMPTS: Mutex<ShareFocusAttempts> = Mutex::new(ShareFocusAttempts::new());

/// A selection-time focus handback is only useful in the short gap between a
/// pill/picker selection and capture startup. A late queued callback would
/// recreate the original flash by activating an app after the user has moved
/// on, so it expires instead of trying to repair focus after publication.
///
/// #677: 250ms was too short for WebKit/IPC-delayed activation of Petal after
/// a hover-pill click. The handback only acts while `petal_active` is true, so
/// extending this window does not yank focus from a third app the user moved
/// to — it only recovers when Petal itself still holds the foreground.
#[cfg(target_os = "macos")]
const SHARE_FOCUS_HANDOFF_MAX_DELAY: Duration = Duration::from_millis(800);

/// Gap-free frontmost sampling window after share selection (#677 Step 0).
#[cfg(target_os = "macos")]
const SHARE_FOCUS_MEASURE_MS: u64 = 1500;
#[cfg(target_os = "macos")]
const SHARE_FOCUS_MEASURE_INTERVAL_MS: u64 = 50;

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShareFocusAttempt {
    generation: u64,
    window_id: u32,
    selected_owner_pid: Option<i32>,
    source_pid: Option<i32>,
    selected_frontmost_snapshot: u64,
    selected_at: Instant,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct ShareFocusAttempts {
    next_generation: u64,
    current: Option<ShareFocusAttempt>,
}

/// The small lifecycle seam shared by both real share entry points and their
/// teardown paths. Keeping invalidation here lets the command-path adapter
/// tests exercise the same transition that the AppHandle wrappers use, without
/// needing a live ScreenCaptureKit/LiveKit start.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
enum ShareFocusLifecycle {
    Selected {
        window_id: u32,
        selected_owner_pid: Option<i32>,
        source_pid: Option<i32>,
        selected_frontmost_snapshot: u64,
        selected_at: Instant,
    },
    StartFailed(ShareFocusAttempt),
    Unshared {
        window_id: u32,
    },
    WindowCleared {
        window_id: u32,
    },
    RoomLeft,
}

#[cfg(target_os = "macos")]
impl ShareFocusAttempts {
    const fn new() -> Self {
        Self {
            next_generation: 0,
            current: None,
        }
    }

    fn begin(
        &mut self,
        window_id: u32,
        selected_owner_pid: Option<i32>,
        source_pid: Option<i32>,
        selected_frontmost_snapshot: u64,
        selected_at: Instant,
    ) -> ShareFocusAttempt {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let attempt = ShareFocusAttempt {
            generation: self.next_generation,
            window_id,
            selected_owner_pid,
            source_pid,
            selected_frontmost_snapshot,
            selected_at,
        };
        self.current = Some(attempt);
        attempt
    }

    fn is_current(&self, attempt: ShareFocusAttempt) -> bool {
        self.current == Some(attempt)
    }

    fn invalidate_window(&mut self, window_id: u32) {
        if self
            .current
            .is_some_and(|attempt| attempt.window_id == window_id)
        {
            self.current = None;
        }
    }

    fn invalidate_attempt(&mut self, attempt: ShareFocusAttempt) {
        if self.is_current(attempt) {
            self.current = None;
        }
    }

    fn invalidate_all(&mut self) {
        self.current = None;
    }
}

#[cfg(target_os = "macos")]
fn apply_share_focus_lifecycle(
    attempts: &mut ShareFocusAttempts,
    event: ShareFocusLifecycle,
) -> Option<ShareFocusAttempt> {
    match event {
        ShareFocusLifecycle::Selected {
            window_id,
            selected_owner_pid,
            source_pid,
            selected_frontmost_snapshot,
            selected_at,
        } => Some(attempts.begin(
            window_id,
            selected_owner_pid,
            source_pid,
            selected_frontmost_snapshot,
            selected_at,
        )),
        ShareFocusLifecycle::StartFailed(attempt) => {
            attempts.invalidate_attempt(attempt);
            None
        }
        ShareFocusLifecycle::Unshared { window_id }
        | ShareFocusLifecycle::WindowCleared { window_id } => {
            attempts.invalidate_window(window_id);
            None
        }
        ShareFocusLifecycle::RoomLeft => {
            attempts.invalidate_all();
            None
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusHandoffAction {
    None,
    ActivateSource(i32),
    YieldForeground(Option<i32>),
}

/// Decide whether to hand focus back after a share selection.
///
/// #677 root cause (confirmed against the decision table + live logs):
/// requiring `frontmost_matches_selection` suppressed the exact steal case
/// we care about. Selection snapshots the pre-click frontmost app (e.g.
/// Sublime). If Petal then becomes frontmost, `frontmost_matches_selection`
/// is false and the old table returned `None` while `petal_active` was true
/// — permanently leaving Petal in the foreground. User moving to a *third*
/// app is already covered by `petal_active == false` (Petal is no longer
/// active, so we do not touch focus).
#[cfg(target_os = "macos")]
fn focus_handoff_action(
    current: bool,
    petal_active: bool,
    owner_matches_selection: bool,
    cockpit_source_visible: bool,
    still_immediate: bool,
    source_pid: Option<i32>,
) -> FocusHandoffAction {
    if !current
        || !petal_active
        || !owner_matches_selection
        || cockpit_source_visible
        || !still_immediate
    {
        FocusHandoffAction::None
    } else if let Some(pid) = source_pid {
        FocusHandoffAction::ActivateSource(pid)
    } else {
        FocusHandoffAction::YieldForeground(None)
    }
}

#[cfg(target_os = "macos")]
fn frontmost_app_snapshot() -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    crate::platform::appkit::frontmost_app_label().hash(&mut hasher);
    hasher.finish()
}

#[cfg(target_os = "macos")]
fn begin_share_focus_attempt(window_id: u32) -> ShareFocusAttempt {
    let self_pid = std::process::id() as i32;
    let selected_owner_pid = crate::window_registry::global()
        .map(|r| r.owner_pid_fresh(window_id))
        .unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(window_id));
    let source_pid = shared_source_refocus_pid(selected_owner_pid, self_pid);
    let mut attempts = SHARE_FOCUS_ATTEMPTS.lock_unpoisoned();
    apply_share_focus_lifecycle(
        &mut attempts,
        ShareFocusLifecycle::Selected {
            window_id,
            selected_owner_pid,
            source_pid,
            selected_frontmost_snapshot: frontmost_app_snapshot(),
            selected_at: Instant::now(),
        },
    )
    .expect("selected focus lifecycle always creates an attempt")
}

#[cfg(target_os = "macos")]
fn invalidate_share_focus_attempt(event: ShareFocusLifecycle) {
    debug_assert!(matches!(
        event,
        ShareFocusLifecycle::StartFailed(_)
            | ShareFocusLifecycle::Unshared { .. }
            | ShareFocusLifecycle::WindowCleared { .. }
    ));
    apply_share_focus_lifecycle(&mut SHARE_FOCUS_ATTEMPTS.lock_unpoisoned(), event);
}

#[cfg(target_os = "macos")]
fn invalidate_all_share_focus_attempts() {
    apply_share_focus_lifecycle(
        &mut SHARE_FOCUS_ATTEMPTS.lock_unpoisoned(),
        ShareFocusLifecycle::RoomLeft,
    );
}

/// Restored after `9d97387a`'s hover_core.rs split deleted this function
/// while leaving its caller (`platform::gesture_tap`'s #761 event-driven
/// drag path) intact, breaking the macOS build. That refactor also removed
/// `pill_wid()`/`PILL_WID` (the cached CGWindowID used for a same-thread
/// WindowServer-direct move) with no replacement, so the #761 fast-path
/// optimization ("zero main-thread queue latency") this function's comment
/// used to describe is NOT restored here -- this always takes the
/// documented fallback (`position_tab`, the normal main-thread path).
/// Correctness is preserved; the latency optimization needs its own
/// follow-up once the new pill-webview architecture's window-id story is
/// understood.
pub(crate) fn drag_nudge(app: &AppHandle, window_id: u32, frame: crate::platform::cg::WindowFrame) {
    let Some(mut update) = last_hover_update() else {
        return;
    };
    if update.window_id != window_id {
        return;
    }
    let presentation = platform::tab_position(app, &frame, window_id);
    platform::note_drag_nudge();
    position_tab(app, presentation);
    update.frame = frame;
    update.tab_x = presentation.rect.x;
    update.tab_y = presentation.rect.y;
    update.attachment = presentation.attachment;
    set_last_hover_update(Some(update.clone()));
    // The webview's inner UI does not need ~120Hz updates; emit at most every
    // 50ms (the panel FRAME moves every event regardless).
    if platform::emit_throttle_ok(50) {
        let _ = tauri::Emitter::emit(app, "hover-tab-update", update);
    }
}

/// Toggle share state for `window_id`. Returns the NEW shared state (true =
/// now shared). Shows/hides the colored border overlay for that window as a
/// side effect, and starts/stops the real capture+publish session
/// (`session::start_share`/`stop_share`) -- see `session.rs` for what's real
/// vs. a stand-in (room-join in particular).
///
/// The border/UI toggle is applied optimistically (matches the pre-existing
/// behavior of this command); if the underlying real capture/publish call
/// fails, it's rolled back and a `share-error` event is emitted to the
/// `hover-tab` webview (same channel `hover-tab-update`/`hover-tab-hide`
/// already use) so the frontend can react (e.g. un-toggle its own button
/// state, show a toast) instead of silently diverging from the real state.
///
/// Extracted from the `toggle_window_share` Tauri command so the global
/// keyboard shortcut (SPEC.md §4.2: "a global shortcut to toggle the
/// last-shared window") can call the exact same toggle path -- border
/// bookkeeping, optimistic-then-rollback semantics, `share-error` emission,
/// all of it -- instead of a second, parallel toggle implementation. See
/// `shortcuts.rs` for the caller.
#[cfg(target_os = "macos")]
pub async fn toggle_share_for_window(
    app: &AppHandle,
    state: &crate::session::SessionState,
    window_id: u32,
    frame: WindowFrame,
) -> bool {
    toggle_share_for_window_with_color(app, state, window_id, frame, None).await
}

#[cfg(target_os = "macos")]
pub(crate) async fn toggle_share_for_window_with_color(
    app: &AppHandle,
    state: &crate::session::SessionState,
    window_id: u32,
    frame: WindowFrame,
    color: Option<String>,
) -> bool {
    let lock = share_toggle_lock(window_id);
    let _guard = lock.lock().await;
    let source_kind = if crate::region_window::resolve(window_id).is_some() {
        crate::transport::publisher::SharedSourceKind::DisplayRegion
    } else {
        crate::transport::publisher::SharedSourceKind::Window
    };
    let now_shared = !state.is_share_active(window_id);
    log::info!(
        "hover_tab: toggle_share_for_window(window {window_id}) serialized -> {}",
        if now_shared {
            "START sharing"
        } else {
            "STOP sharing"
        }
    );

    if now_shared {
        // Capture the selected source before any capture/publish await. The
        // hover-tab click can activate Petal, and waiting for publication to
        // hand focus back makes that activation visibly flash the gallery.
        let focus_attempt = begin_share_focus_attempt(window_id);
        hand_back_selected_source_immediately(app, focus_attempt);
        let color = share_color_or_default(color.as_deref());
        if share_border_start_timing(ShareStartSurface::HoverTab)
            == ShareBorderStartTiming::Optimistic
        {
            show_share_border_optimistically(app, source_kind, window_id, frame, &color);
        }
        if let Err(error) = crate::session::start_share(app, state, window_id, frame).await {
            invalidate_share_focus_attempt(ShareFocusLifecycle::StartFailed(focus_attempt));
            log::error!("toggle_share_for_window: start_share({window_id}) failed: {error}");
            reconcile_share_border(app, state, source_kind, window_id, frame, Some(&color));
            reconcile_share_overlay(app, state, window_id, frame, source_kind);
            emit_share_error(app, window_id, true, error);
            return false;
        }
        reconcile_share_border(app, state, source_kind, window_id, frame, Some(&color));
        reconcile_share_overlay(app, state, window_id, frame, source_kind);
    } else {
        invalidate_share_focus_attempt(ShareFocusLifecycle::Unshared { window_id });
        crate::remote_control::revoke_window(app, window_id, "share stopped");
        if let Err(error) = crate::session::stop_share(app, state, window_id).await {
            log::error!("toggle_share_for_window: stop_share({window_id}) failed: {error}");
            // Bookkeeping (capture/track map entry) is already cleared by
            // `stop_share` regardless of whether the unpublish call itself
            // errored -- so there's nothing to roll back here, just surface
            // the error for visibility (e.g. LiveKit connection already
            // dropped server-side).
            emit_share_error(app, window_id, false, error);
        }
        reconcile_share_border(app, state, source_kind, window_id, frame, color.as_deref());
        reconcile_share_overlay(app, state, window_id, frame, source_kind);
    }

    let actual_shared = state.is_share_active(window_id);
    log::info!(
        "hover_tab: toggle_share_for_window(window {window_id}) done -- backend shared={actual_shared}"
    );
    crate::region_window::emit_region_share_state(app, window_id, actual_shared);
    actual_shared
}

#[cfg(target_os = "macos")]
pub(crate) async fn start_share_for_system_picker_selection(
    app: &AppHandle,
    state: &crate::session::SessionState,
    window_id: u32,
    frame: WindowFrame,
    filter: SCContentFilter,
    logical_width: f64,
    logical_height: f64,
    point_pixel_scale: f64,
    source_kind: crate::transport::publisher::SharedSourceKind,
    source_title: Option<String>,
    color: Option<String>,
) -> bool {
    let lock = share_toggle_lock(window_id);
    let _guard = lock.lock().await;
    if state.is_share_active(window_id) {
        log::info!(
            "hover_tab: system picker selected window {window_id}, no-op -- already sharing"
        );
        return true;
    }

    // `color` is the frontend's resolved local identity color, threaded
    // through from `open_window_picker_window` (see window_picker.rs). Before
    // this fix this call was always `share_color_or_default(None)`, so the
    // system-picker share flow (the primary "Share" button whenever
    // `SCContentSharingPicker` is available) never received the local user's
    // own color and silently fell back to a stale process-global "remembered"
    // color or the hardcoded default plum -- producing a share border/bar
    // that didn't match the user's actually-picked identity swatch.
    let color = share_color_or_default(color.as_deref());
    let source_label = shared_source_kind_label(source_kind);
    log::info!(
        "hover_tab: system picker selected {source_label} {window_id} -> START sharing color={color}"
    );
    // The picker makes Petal active before it yields its selection. Schedule
    // the one permitted handback at that boundary, not after the slow media
    // start below. A newer selection invalidates this global attempt.
    let focus_attempt = begin_share_focus_attempt(window_id);
    hand_back_selected_source_immediately(app, focus_attempt);
    // issue #249: the system picker path can spend user-visible time waiting
    // for picker selection/capture/publish. Do not show the "live" border
    // until `start_share_with_system_picker_filter` has actually published
    // and recorded an active share; otherwise the sharer sees "sharing" while
    // viewers may still have no track.
    if share_border_start_timing(ShareStartSurface::SystemPicker)
        == ShareBorderStartTiming::Optimistic
    {
        show_share_border_optimistically(app, source_kind, window_id, frame, &color);
    } else {
        log::info!(
            "hover_tab: system picker deferring share border for {source_label} {window_id} until publish succeeds"
        );
    }
    if let Err(error) = crate::session::start_share_with_system_picker_filter(
        app,
        state,
        window_id,
        frame,
        filter,
        logical_width,
        logical_height,
        point_pixel_scale,
        source_kind,
        source_title,
    )
    .await
    {
        invalidate_share_focus_attempt(ShareFocusLifecycle::StartFailed(focus_attempt));
        log::error!("hover_tab: system picker start_share({window_id}) failed: {error}");
        reconcile_share_border(app, state, source_kind, window_id, frame, Some(&color));
        if source_kind == crate::transport::publisher::SharedSourceKind::Window {
            reconcile_share_overlay(app, state, window_id, frame, source_kind);
        }
        emit_share_error(app, window_id, true, error);
        return false;
    }

    reconcile_share_border(app, state, source_kind, window_id, frame, Some(&color));
    if source_kind == crate::transport::publisher::SharedSourceKind::Window {
        reconcile_share_overlay(app, state, window_id, frame, source_kind);
    }
    let actual_shared = state.is_share_active(window_id);
    log::info!(
        "hover_tab: system picker share for {source_label} {window_id} done -- backend shared={actual_shared}"
    );
    crate::region_window::emit_region_share_state(app, window_id, actual_shared);
    actual_shared
}

#[cfg(target_os = "macos")]
fn shared_source_kind_label(
    source_kind: crate::transport::publisher::SharedSourceKind,
) -> &'static str {
    match source_kind {
        crate::transport::publisher::SharedSourceKind::Window => "window",
        crate::transport::publisher::SharedSourceKind::Display => "display",
        crate::transport::publisher::SharedSourceKind::DisplayRegion => "display-region",
    }
}

#[cfg(target_os = "macos")]
fn shared_source_refocus_pid(owner_pid: Option<i32>, self_pid: i32) -> Option<i32> {
    owner_pid.filter(|pid| *pid > 0 && *pid != self_pid)
}

/// The cockpit's app-owned deterministic source must remain on screen while
/// its strict WindowServer oracle samples it. This is deliberately unavailable
/// to normal builds and applies only to the explicitly registered QA source;
/// ordinary app-owned/display shares retain the usual handback behavior.
#[cfg(all(target_os = "macos", feature = "cockpit-privileged"))]
fn keep_cockpit_source_visible(window_id: u32) -> bool {
    crate::test_cockpit::cockpit_source_requires_visible_handback(window_id)
}

#[cfg(all(target_os = "macos", not(feature = "cockpit-privileged")))]
fn keep_cockpit_source_visible(_window_id: u32) -> bool {
    false
}

/// Attempt one handback decision on the main thread. Returns true if a
/// handback action was dispatched (source activated or foreground yielded).
#[cfg(target_os = "macos")]
fn try_selection_handback_on_main(attempt: ShareFocusAttempt, pass: &str) -> bool {
    let current = SHARE_FOCUS_ATTEMPTS.lock_unpoisoned().is_current(attempt);
    let petal_active = crate::platform::appkit::app_is_active();
    let owner_matches_selection = crate::window_registry::global()
        .map(|r| r.owner_pid_fresh(attempt.window_id))
        .unwrap_or_else(|| crate::platform::cg::owner_pid_for_window_id(attempt.window_id))
        == attempt.selected_owner_pid;
    let frontmost_matches_selection =
        frontmost_app_snapshot() == attempt.selected_frontmost_snapshot;
    let still_immediate = attempt.selected_at.elapsed() <= SHARE_FOCUS_HANDOFF_MAX_DELAY;
    let frontmost_label = crate::platform::appkit::frontmost_app_label();
    let action = focus_handoff_action(
        current,
        petal_active,
        owner_matches_selection,
        keep_cockpit_source_visible(attempt.window_id),
        still_immediate,
        attempt.source_pid,
    );
    if action == FocusHandoffAction::None {
        log::info!(
            "hover_tab: [focus] skipped selection handback ({pass}) window {} generation {} current={} petal_active={} owner_matches={} frontmost_matches={} immediate={} frontmost={}",
            attempt.window_id,
            attempt.generation,
            current,
            petal_active,
            owner_matches_selection,
            frontmost_matches_selection,
            still_immediate,
            frontmost_label,
        );
        return false;
    }

    log::info!(
        "hover_tab: [focus] selection handback ({pass}) window {} generation {} frontmost={}, petal_active={}, source_pid={:?}",
        attempt.window_id,
        attempt.generation,
        frontmost_label,
        petal_active,
        attempt.source_pid,
    );

    let handed_back = match action {
        FocusHandoffAction::ActivateSource(pid) => {
            match crate::platform::appkit::activate_running_app(pid) {
                Ok(true) => true,
                Ok(false) => {
                    log::warn!(
                        "hover_tab: [focus] source app pid {pid} declined selection handback (window {})",
                        attempt.window_id
                    );
                    false
                }
                Err(error) => {
                    log::warn!(
                        "hover_tab: [focus] source app pid {pid} selection handback failed (window {}): {error}",
                        attempt.window_id
                    );
                    false
                }
            }
        }
        FocusHandoffAction::YieldForeground(source_pid) => {
            if let Err(error) =
                crate::platform::appkit::yield_active_app_to(source_pid.unwrap_or(0))
            {
                log::warn!(
                    "hover_tab: [focus] selection foreground yield failed (window {}): {error}",
                    attempt.window_id
                );
            }
            true
        }
        FocusHandoffAction::None => unreachable!("handled above"),
    };

    if !handed_back {
        // Source unavailable or declined. Fall back to resigning Petal's
        // active status so we never keep the stolen foreground.
        if let Err(error) =
            crate::platform::appkit::yield_active_app_to(attempt.source_pid.unwrap_or(0))
        {
            log::warn!(
                "hover_tab: [focus] selection fallback yield failed (window {}): {error}",
                attempt.window_id
            );
        }
    }
    log::info!(
        "hover_tab: [focus] selection handback dispatched ({pass}) window {} generation {} frontmost={}, petal_active={}",
        attempt.window_id,
        attempt.generation,
        crate::platform::appkit::frontmost_app_label(),
        crate::platform::appkit::app_is_active(),
    );
    true
}

#[cfg(target_os = "macos")]
fn hand_back_selected_source_immediately(app: &AppHandle, attempt: ShareFocusAttempt) {
    // Immediate pass — covers the case where the pill/picker click already
    // activated Petal before the Rust command ran.
    if let Err(e) = app.run_on_main_thread({
        let attempt = attempt;
        move || {
            let _ = try_selection_handback_on_main(attempt, "immediate");
        }
    }) {
        log::warn!(
            "hover_tab: failed to schedule selection-time focus handback for window {} generation {}: {e}",
            attempt.window_id,
            attempt.generation,
        );
    }

    // #677: WebKit/IPC can activate Petal *after* the immediate pass sees
    // petal_active=false. Retry on a short cadence while the attempt is still
    // current and within SHARE_FOCUS_HANDOFF_MAX_DELAY. Only acts when Petal
    // is active, so a user who left for another app is never yanked back.
    let app_for_retry = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut handed = false;
        let deadline = attempt.selected_at + SHARE_FOCUS_HANDOFF_MAX_DELAY;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(SHARE_FOCUS_MEASURE_INTERVAL_MS)).await;
            if handed {
                break;
            }
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let attempt_for_main = attempt;
            if app_for_retry
                .run_on_main_thread(move || {
                    let did = try_selection_handback_on_main(attempt_for_main, "retry");
                    let _ = done_tx.send(did);
                })
                .is_err()
            {
                break;
            }
            match done_rx.await {
                Ok(true) => handed = true,
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    // #677 Step 0 measurement: gap-free frontmost polling for ~1.5s so a
    // future log can prove whether Petal ever becomes frontmost on the
    // hover-pill path (and for how long). Observation only — never activates.
    let app_for_measure = app.clone();
    tauri::async_runtime::spawn(async move {
        let samples = (SHARE_FOCUS_MEASURE_MS / SHARE_FOCUS_MEASURE_INTERVAL_MS) as usize;
        let mut last_frontmost = String::new();
        let mut last_active: Option<bool> = None;
        let mut petal_active_samples = 0u32;
        for i in 0..=samples {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if app_for_measure
                .run_on_main_thread(move || {
                    let frontmost = crate::platform::appkit::frontmost_app_label();
                    let active = crate::platform::appkit::app_is_active();
                    let _ = tx.send((frontmost, active));
                })
                .is_err()
            {
                break;
            }
            let Ok((frontmost, active)) = rx.await else {
                break;
            };
            if active {
                petal_active_samples += 1;
            }
            if frontmost != last_frontmost || Some(active) != last_active {
                log::info!(
                    "hover_tab: [focus] measure window {} generation {} t={}ms frontmost={} petal_active={}",
                    attempt.window_id,
                    attempt.generation,
                    i as u64 * SHARE_FOCUS_MEASURE_INTERVAL_MS,
                    frontmost,
                    active,
                );
                last_frontmost = frontmost;
                last_active = Some(active);
            }
            if i < samples {
                tokio::time::sleep(Duration::from_millis(SHARE_FOCUS_MEASURE_INTERVAL_MS)).await;
            }
        }
        log::info!(
            "hover_tab: [focus] measure summary window {} generation {} petal_active_samples={petal_active_samples}/{} final_frontmost={} final_petal_active={}",
            attempt.window_id,
            attempt.generation,
            samples + 1,
            last_frontmost,
            last_active.unwrap_or(false),
        );
    });
}

#[cfg(target_os = "macos")]
fn share_toggle_lock(window_id: u32) -> Arc<tokio::sync::Mutex<()>> {
    let locks = SHARE_TOGGLE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = locks.lock_unpoisoned();
    guard
        .entry(window_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(target_os = "macos")]
fn show_share_border_optimistically(
    app: &AppHandle,
    source_kind: crate::transport::publisher::SharedSourceKind,
    window_id: u32,
    frame: WindowFrame,
    color: &str,
) {
    let result = with_share_state(|s| {
        s.ensure_border(window_id, || match source_kind {
            crate::transport::publisher::SharedSourceKind::Window => {
                crate::share_border::show_share_border(app, window_id, frame, color)
            }
            crate::transport::publisher::SharedSourceKind::Display
            | crate::transport::publisher::SharedSourceKind::DisplayRegion => {
                crate::share_border::show_share_border_for_source(
                    app,
                    source_kind,
                    window_id,
                    frame,
                    color,
                )
            }
        })
    });

    match result {
        EnsureBorderResult::Created(border_id) => log::info!(
            "hover_tab: optimistically showing share border {border_id} for starting window {window_id}"
        ),
        EnsureBorderResult::Existing(border_id) => log::info!(
            "hover_tab: optimistic share border already present as {border_id} for starting window {window_id}"
        ),
    }
}

#[cfg(target_os = "macos")]
fn reconcile_share_border(
    app: &AppHandle,
    state: &crate::session::SessionState,
    source_kind: crate::transport::publisher::SharedSourceKind,
    window_id: u32,
    frame: WindowFrame,
    color: Option<&str>,
) {
    let active = state.is_share_active(window_id);
    if active {
        let color = share_color_or_default(color);
        let result = with_share_state(|s| {
            s.ensure_border(window_id, || {
                crate::share_border::show_share_border_for_source(
                    app,
                    source_kind,
                    window_id,
                    frame,
                    &color,
                )
            })
        });
        if let EnsureBorderResult::Created(border_id) = result {
            log::info!(
                "hover_tab: reconciling border -- showing share border {border_id} for active window {window_id}"
            );
        }
    } else if let Some(border_id) = with_share_state(|s| s.remove_border(window_id)) {
        log::info!(
            "hover_tab: reconciling border -- hiding share border {border_id} for inactive window {window_id}"
        );
        crate::share_border::hide_share_border(app, border_id);
    }
}

/// #764: the post-wake share restart tears the border down via
/// `stop_share` -> `clear_share_state_for_window` (#420) and the session-layer
/// restart has no border-creation path, so a restarted share published with no
/// visible border. Reuses `reconcile_share_border` (idempotent `ensure_border`)
/// rather than duplicating border creation into the session layer.
///
/// Restores the OVERLAY too: `clear_share_state_for_window` drops both, so
/// restoring only the border would silently leave the sharer unable to see
/// remote telepointers/draw strokes on their own window for the rest of the
/// meeting -- the same teardown-without-rebuild bug, one surface over.
#[cfg(target_os = "macos")]
pub(crate) fn restore_share_border_after_restart(
    app: &AppHandle,
    state: &crate::session::SessionState,
    source_kind: crate::transport::publisher::SharedSourceKind,
    window_id: u32,
    frame: WindowFrame,
    color: &str,
) {
    log::info!(
        "hover_tab: restoring share border + overlay after post-wake restart for {} {window_id}",
        shared_source_kind_label(source_kind)
    );
    reconcile_share_border(app, state, source_kind, window_id, frame, Some(color));
    reconcile_share_overlay(app, state, window_id, frame, source_kind);
}

#[cfg(target_os = "macos")]
fn reconcile_share_overlay(
    app: &AppHandle,
    state: &crate::session::SessionState,
    window_id: u32,
    frame: WindowFrame,
    source_kind: crate::transport::publisher::SharedSourceKind,
) {
    let active = state.is_share_active(window_id);
    if active {
        let Some((_, owner_identity)) = state.control_channel_snapshot() else {
            log::warn!(
                "hover_tab: sharer overlay skipped for active window {window_id}; local identity unavailable"
            );
            return;
        };
        let result = with_share_state(|s| {
            s.ensure_overlay(window_id, || {
                crate::share_overlay::show_share_overlay(
                    app,
                    &owner_identity,
                    window_id,
                    frame,
                    source_kind == crate::transport::publisher::SharedSourceKind::Display,
                )
            })
        });
        if let EnsureOverlayResult::Created(overlay_id) = result {
            log::info!(
                "hover_tab: reconciling overlay -- showing share overlay {overlay_id} for active window {window_id}"
            );
        }
    } else if let Some(overlay_id) = with_share_state(|s| s.remove_overlay(window_id)) {
        log::info!(
            "hover_tab: reconciling overlay -- hiding share overlay {overlay_id} for inactive window {window_id}"
        );
        crate::share_overlay::hide_share_overlay(app, overlay_id);
    }
}

/// Clear this module's border bookkeeping and hide every border panel.
/// Called by `session::leave_room` (issue #13): leave-room tears shares down
/// via `session::stop_share` directly, bypassing `toggle_share_for_window` --
/// without this, border panels stayed visible after the session authority had
/// already removed its active shares. Share membership itself is now derived
/// from `SessionState`, so there is no second hover-tab shared-window set to
/// clear.
/// Border panels are hidden + retired (never destroyed) via
/// `hide_share_border`'s existing path.
pub fn clear_share_state_on_leave(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        platform::reset_drag_state(true);
        invalidate_all_share_focus_attempts();
    }
    let (border_ids, overlay_ids): (Vec<u32>, Vec<u32>) =
        with_share_state(|s| (s.drain_borders(), s.drain_overlays()));
    if !border_ids.is_empty() || !overlay_ids.is_empty() {
        log::info!(
            "hover_tab: clearing share state on leave-room -- hiding {} border panel(s), {} overlay panel(s)",
            border_ids.len(),
            overlay_ids.len()
        );
    }
    for border_id in border_ids {
        crate::share_border::hide_share_border(app, border_id);
    }
    for overlay_id in overlay_ids {
        crate::share_overlay::hide_share_overlay(app, overlay_id);
    }
    crate::share_overlay::retire_all_overlays(app);
}

/// Clear one shared window from hover-tab/border bookkeeping because the
/// underlying CGWindow disappeared outside the normal pill toggle path
/// (display disconnect, app closed, moved to an unavailable Space). Session
/// teardown is still owned by `session::stop_share`; this keeps the visible
/// UI from showing a stale "Stop sharing" state or orphan border.
pub fn clear_share_state_for_window(app: &AppHandle, window_id: u32) {
    #[cfg(target_os = "macos")]
    {
        if last_hover_update().is_some_and(|update| update.window_id == window_id) {
            platform::reset_drag_state(true);
        }
        invalidate_share_focus_attempt(ShareFocusLifecycle::WindowCleared { window_id });
    }
    let border_id = with_share_state(|s| s.remove_border(window_id));
    if let Some(border_id) = border_id {
        log::info!("hover_tab: hiding share border {border_id} for lost window {window_id}");
        crate::share_border::hide_share_border(app, border_id);
    }
    let overlay_id = with_share_state(|s| s.remove_overlay(window_id));
    if let Some(overlay_id) = overlay_id {
        log::info!("hover_tab: hiding share overlay {overlay_id} for lost window {window_id}");
        crate::share_overlay::hide_share_overlay(app, overlay_id);
    }
    crate::share_overlay::retire_overlays_for_window(app, window_id);
    emit_share_state_changed(app, window_id, false);
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn toggle_window_share(
    app: AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    frame: WindowFrame,
    color: Option<String>,
) -> Result<bool, ()> {
    Ok(toggle_share_for_window_with_color(&app, &state, window_id, frame, color).await)
}

/// Picker-friendly share toggle (in-meeting "Share" button -> traditional
/// window picker). The picker only has a `window_id` from
/// `list_shareable_windows`, not a live frame, so we look up a fresh
/// on-screen frame here (the same `CGWindowListCopyWindowInfo` scan the
/// global-shortcut path uses) and route through the exact same
/// `toggle_share_for_window` path the hover-tab pill uses -- so a share
/// started from the picker is byte-for-byte identical (capture + publish +
/// border + `share-error` plumbing) to one started from the pill. Returns
/// the new shared state for this window.
#[cfg(target_os = "macos")]
async fn toggle_window_share_from_picker(
    app: &AppHandle,
    state: &crate::session::SessionState,
    window_id: u32,
    color: Option<String>,
) -> bool {
    // Display source ids (DISPLAY_SOURCE_MARKER | CGDirectDisplayID) from the
    // picker's "Screen N" cards share through the same display-filter path the
    // system picker uses, instead of the window-id capture path.
    if crate::window_source::is_display_source_id(window_id) {
        return toggle_display_share_from_picker(app, state, window_id, color).await;
    }
    // A frame we couldn't resolve (window closed since enumeration) is
    // non-fatal: capture keys off the window_id via SCContentFilter, not the
    // frame -- the frame only positions the colored border/telepointer. Fall
    // back to a zero frame (no visible border) rather than refusing to share.
    let frame = crate::window_registry::global()
        .map(|r| r.frame_fresh(window_id))
        .unwrap_or_else(|| crate::platform::cg::frame_for_window_id(window_id))
        .unwrap_or(WindowFrame {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
    toggle_share_for_window_with_color(app, state, window_id, frame, color).await
}

/// Toggle share for a display source id picked from the custom picker. Stop
/// if already shared; otherwise build a display `SCContentFilter` and route
/// through the same `start_share_for_system_picker_selection` the system
/// picker's SingleDisplay mode uses (capture + publish + border bookkeeping
/// for `SharedSourceKind::Display`).
#[cfg(target_os = "macos")]
async fn toggle_display_share_from_picker(
    app: &AppHandle,
    state: &crate::session::SessionState,
    window_id: u32,
    color: Option<String>,
) -> bool {
    use screencapturekit::shareable_content::SCShareableContent;
    use screencapturekit::stream::content_filter::SCContentFilter;

    if state.is_share_active(window_id) {
        if let Err(error) = crate::session::stop_share(app, state, window_id).await {
            log::warn!("hover_tab: display {window_id} stop_share failed: {error}");
        }
        return state.is_share_active(window_id);
    }

    let display_id = crate::window_source::display_id_from_source_id(window_id);
    let content = match SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
    {
        Ok(content) => content,
        Err(e) => {
            emit_share_error(
                app,
                window_id,
                true,
                crate::session::ShareSessionError::Capture(format!(
                    "display {display_id} enumeration failed: {e}"
                )),
            );
            return false;
        }
    };
    let Some(display) = content
        .displays()
        .into_iter()
        .find(|display| display.display_id() == display_id)
    else {
        emit_share_error(
            app,
            window_id,
            true,
            crate::session::ShareSessionError::Capture(format!(
                "display {display_id} not found in SCShareableContent"
            )),
        );
        return false;
    };
    let frame = display.frame();
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();
    let point_pixel_scale = f64::from(filter.point_pixel_scale()).max(1.0);
    let title = Some(format!("Screen {display_id}"));
    start_share_for_system_picker_selection(
        app,
        state,
        window_id,
        WindowFrame {
            x: frame.origin.x.round() as i32,
            y: frame.origin.y.round() as i32,
            width: frame.size.width.round().max(1.0) as i32,
            height: frame.size.height.round().max(1.0) as i32,
        },
        filter,
        frame.size.width,
        frame.size.height,
        point_pixel_scale,
        crate::transport::publisher::SharedSourceKind::Display,
        title,
        color,
    )
    .await
}

/// IPC wrapper for the picker-friendly toggle. The command name stays
/// `share_window` because the frontend invokes that string directly; the
/// behavior-accurate name lives in `toggle_window_share_from_picker`.
#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn share_window(
    app: AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    color: Option<String>,
) -> Result<bool, ()> {
    Ok(toggle_window_share_from_picker(&app, &state, window_id, color).await)
}

/// Which windows this process is currently sharing (so the picker can show a
/// "Sharing"/"Stop" state per window). Reads the same shared set the hover-tab
/// pill and global shortcut use.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn shared_window_ids(state: tauri::State<'_, crate::session::SessionState>) -> Vec<u32> {
    state.active_share_ids()
}

#[cfg(target_os = "macos")]
pub(crate) fn autotest_ui_shared_window_ids() -> Vec<u32> {
    let mut ids = with_share_state(|s| {
        s.borders
            .keys()
            .chain(s.overlays.keys())
            .copied()
            .collect::<Vec<_>>()
    });
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Diagnostic breadcrumb from the hover-tab page itself (issue #22): the
/// route invokes this once on mount, proving in petal.log that (a) the
/// webview actually loaded the route and (b) the Tauri IPC bridge works in
/// that webview -- the two silent-failure modes that made the invisible pill
/// so hard to attribute. All platforms (the page is platform-agnostic).
#[tauri::command]
pub fn hover_tab_page_mounted() -> Option<HoverTabUpdate> {
    log::info!("hover_tab: page mounted (webview loaded, IPC bridge live)");
    last_hover_update()
}

/// Keep the macOS tooltip in AppKit rather than relying only on the HTML
/// `title` path, whose WKWebView tracking is unreliable in this non-key panel.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn set_hover_tab_tooltip(window: tauri::WebviewWindow, tooltip: String) -> Result<(), String> {
    // Tauri guarantees this callback runs on the AppKit main thread. The
    // helper also checks the marker before touching NSView.
    window
        .with_webview(move |webview| {
            let view: &objc2_app_kit::NSView = unsafe { &*webview.inner().cast() };
            if let Err(error) = crate::platform::appkit::set_view_tooltip(view, &tooltip) {
                log::warn!("hover_tab: failed to set native tooltip: {error}");
            }
        })
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn set_hover_tab_tooltip(
    _window: tauri::WebviewWindow,
    _tooltip: String,
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn apply_mac_drag_position(
    app: &AppHandle,
    window_id: u32,
    requested_frame: WindowFrame,
    offset: f64,
) -> Result<f64, String> {
    // Check before queueing AppKit work. A hide/target replacement may have
    // won the race with the phase check; scheduling the stale position first
    // could otherwise resurrect the singleton panel after hide.
    if !last_hover_update().is_some_and(|update| update.window_id == window_id) {
        return Err("hover-tab target changed during drag".to_string());
    }
    let source_frame = crate::platform::cg::frame_for_window_id(window_id)
        .ok_or_else(|| "hover-tab source frame is unavailable".to_string())?;
    let offset = crate::hover_core::normalize_hover_tab_vertical_offset(offset);
    let presentation = platform::tab_position_with_offset(app, &source_frame, window_id, offset);
    position_tab(app, presentation);
    let mut update =
        last_hover_update().ok_or_else(|| "hover-tab target is no longer presented".to_string())?;
    if update.window_id != window_id {
        return Err("hover-tab target changed during drag".to_string());
    }
    update.frame = if source_frame.width > 0 && source_frame.height > 0 {
        source_frame
    } else {
        requested_frame
    };
    update.tab_x = presentation.rect.x;
    update.tab_y = presentation.rect.y;
    update.attachment = presentation.attachment;
    update.vertical_offset = offset;
    set_last_hover_update(Some(update.clone()));
    let _ = tauri::Emitter::emit(app, "hover-tab-update", &update);
    Ok(offset)
}

#[cfg(target_os = "macos")]
fn rollback_mac_drag_position(
    app: &AppHandle,
    window_id: u32,
    frame: WindowFrame,
    restore_offset: f64,
) {
    let _ = crate::share_priority::preview_hover_tab_vertical_offset(restore_offset);
    let _ = apply_mac_drag_position(app, window_id, frame, restore_offset);
    platform::reset_drag_state(false);
}

/// One phase-based drag bridge for the macOS nonactivating panel. The same
/// command is used by Top/Center/Bottom native-menu presets.
#[cfg(target_os = "macos")]
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
            if !last_hover_update().is_some_and(|update| update.window_id == window_id) {
                return Err("hover-tab target is stale".to_string());
            }
            let session = crate::hover_core::begin_hover_tab_drag(window_id)?;
            platform::set_drag_active(true);
            log::debug!(
                "hover_tab: drag began window={} generation={}",
                session.window_id,
                session.generation
            );
            Ok(session.original_offset)
        }
        HoverTabDragPhase::Update => {
            let session = crate::hover_core::active_hover_tab_drag(window_id)
                .ok_or_else(|| "hover-tab drag is not active".to_string())?;
            if !last_hover_update().is_some_and(|update| update.window_id == window_id) {
                platform::reset_drag_state(true);
                return Err("hover-tab target changed during drag".to_string());
            }
            let offset =
                match crate::share_priority::preview_hover_tab_vertical_offset(vertical_offset) {
                    Ok(offset) => offset,
                    Err(error) => {
                        platform::reset_drag_state(true);
                        return Err(error);
                    }
                };
            if let Err(error) = apply_mac_drag_position(&app, window_id, frame, offset) {
                rollback_mac_drag_position(&app, window_id, frame, session.original_offset);
                return Err(error);
            }
            Ok(offset)
        }
        HoverTabDragPhase::Commit => {
            let active = crate::hover_core::active_hover_tab_drag(window_id);
            let restore_offset = active
                .map(|session| session.original_offset)
                .unwrap_or_else(crate::share_priority::current_hover_tab_vertical_offset);
            if !last_hover_update().is_some_and(|update| update.window_id == window_id) {
                let _ = crate::share_priority::preview_hover_tab_vertical_offset(restore_offset);
                platform::reset_drag_state(false);
                return Err("hover-tab target is stale".to_string());
            }
            let offset =
                match crate::share_priority::preview_hover_tab_vertical_offset(vertical_offset) {
                    Ok(offset) => offset,
                    Err(error) => {
                        platform::reset_drag_state(true);
                        return Err(error);
                    }
                };
            if let Err(error) = apply_mac_drag_position(&app, window_id, frame, offset) {
                rollback_mac_drag_position(&app, window_id, frame, restore_offset);
                return Err(error);
            }
            let committed = match crate::share_priority::commit_hover_tab_vertical_offset(offset) {
                Ok(value) => value,
                Err(error) => {
                    rollback_mac_drag_position(&app, window_id, frame, restore_offset);
                    return Err(error);
                }
            };
            if active.is_some() {
                let _ = crate::hover_core::finish_hover_tab_drag(window_id);
                platform::set_drag_active(false);
            }
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
                    platform::reset_drag_state(false);
                    return Err(error);
                }
            };
            let result = if last_hover_update().is_some_and(|update| update.window_id == window_id)
            {
                apply_mac_drag_position(&app, window_id, frame, restored)
            } else {
                // The panel may already have been hidden or its target
                // replaced. Restore the preference/session without queueing a
                // stale AppKit move that could show it again.
                Ok(restored)
            };
            let _ = crate::hover_core::finish_hover_tab_drag(window_id);
            platform::set_drag_active(false);
            result.map(|_| restored)
        }
    }
}

/// Room/source teardown must cancel an in-flight placement before the target
/// is retired. This is a lifecycle hook, not a user-visible command.
pub(crate) fn cancel_drag_for_lifecycle() {
    #[cfg(target_os = "macos")]
    platform::reset_drag_state(true);
    #[cfg(target_os = "windows")]
    crate::windows_hover::cancel_drag_for_lifecycle();
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn set_hover_tab_menu_open(open: bool) {
    MENU_OPEN.store(open, std::sync::atomic::Ordering::Release);
}

/// Non-macOS stubs mirroring the command surface above where possible.
/// These intentionally do not take `SessionState`: the real session module is
/// macOS-gated, and adding a cross-platform dummy state would change app
/// setup just to satisfy a stub. The command names remain the frontend
/// contract.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn share_window(_window_id: u32, _color: Option<String>) -> Result<bool, ()> {
    Ok(false)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn shared_window_ids() -> Vec<u32> {
    Vec::new()
}

/// Non-macOS stub: no real capture/publish exists on this platform (see
/// `capture.rs`/`session.rs`, both macOS-only), so this just preserves the
/// old boolean-toggle + border-only behavior.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn toggle_window_share(
    app: AppHandle,
    window_id: u32,
    frame: WindowFrame,
) -> Result<bool, ()> {
    if crate::region_window::resolve(window_id).is_some() {
        log::warn!(
            "hover_tab: Windows display-region capture is unsupported; refusing region {window_id}"
        );
        return Ok(false);
    }
    let now_shared = with_share_state(|s| {
        if s.shared.contains(&window_id) {
            s.shared.remove(&window_id);
            false
        } else {
            s.shared.insert(window_id);
            true
        }
    });

    if now_shared {
        let border_id =
            crate::share_border::show_share_border(&app, window_id, frame, DEFAULT_SHARE_COLOR);
        with_share_state(|s| {
            s.borders.insert(window_id, border_id);
        });
    } else if let Some(border_id) = with_share_state(|s| s.borders.remove(&window_id)) {
        crate::share_border::hide_share_border(&app, border_id);
    }

    Ok(now_shared)
}

#[cfg(target_os = "macos")]
pub(crate) fn emit_share_error(
    app: &AppHandle,
    window_id: u32,
    was_starting: bool,
    error: crate::session::ShareSessionError,
) {
    // Global `emit`, NOT `emit_to(HOVER_TAB_LABEL, ...)` -- see the root-cause
    // comment on the `hover-tab-update` emit in `platform::run` below (issue
    // #22): `emit_to` never matches the page's plain `listen()` in Tauri 2.
    let _ = tauri::Emitter::emit(
        app,
        "share-error",
        ShareErrorPayload {
            window_id,
            was_starting,
            error,
        },
    );
}

pub(crate) fn emit_share_state_changed(app: &AppHandle, window_id: u32, shared: bool) {
    let _ = tauri::Emitter::emit(
        app,
        "share-state-changed",
        ShareStateChanged { window_id, shared },
    );
    crate::region_window::emit_region_share_state(app, window_id, shared);
}

// =============================================================================
// macOS implementation
// =============================================================================

// `pub(crate)`: `share_notice` (#679) reuses `get_monitor_with_cursor` for
// its own top-center positioning rather than reimplementing the
// cursor-to-monitor lookup a second time.
#[cfg(target_os = "macos")]
pub(crate) mod platform {
    use super::WindowFrame;
    use crate::sync_ext::MutexExt;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    // Pure hover-tab core, re-exported so every macOS call site and test
    // below keeps its existing `platform::`/`super::platform::` path.
    pub use crate::hover_core::cursor_over_tab;
    pub(crate) use crate::hover_core::{
        current_hover_presentation, cursor_in_hover_tab_bridge,
        hold_hover_tab_through_transient_miss, hover_hit_test_decision,
        hover_tab_needs_reorder, hover_tab_presentation, hover_tab_presentation_with_offset,
        is_own_chrome_title, same_hit, HoverHitTestDecision, HoverStackEntry, HoverTabAttachment,
        HoverTabPresentation, HoverTabRect, HoverWindowSnapshot, MonitorBounds,
        HOVER_TAB_BRIDGE_TOP_PADDING, HOVER_TAB_BRIDGE_WINDOW_OVERLAP, HOVER_TAB_CURSOR_SLOP_X,
        HOVER_TAB_CURSOR_SLOP_Y, HOVER_TAB_HIDE_GRACE_TICKS, ORDER_REASSERT_TICKS,
    };

    /// Tracking cadence — matches takt's window picker / capture tab (~60 Hz).
    pub const POLL_MS: u64 = 16;
    /// #743: refresh the shared CoreGraphics window snapshot every Nth 16ms
    /// tick (~10Hz), instead of enumerating every tick (60Hz). The cursor is
    /// still polled at 60Hz and hit-tested against the cached snapshot; window
    /// moves surface at ~10Hz, matching the follow cadence the app already
    /// exhibits live (§9.7 baseline p50=142ms).
    const STACK_REFRESH_TICKS: u64 = 6;

    static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// Explicit hover-tab drag placement temporarily owns the panel; the
    /// follower must not queue a competing source-driven move.
    static DRAG_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static OWNER_BUNDLE_ID_CACHE: OnceLock<Mutex<HashMap<i64, Option<String>>>> = OnceLock::new();
    static HOVER_TAB_WINDOW_NUMBER: Mutex<i64> = Mutex::new(0);

    pub(super) fn set_drag_active(active: bool) {
        DRAG_ACTIVE.store(active, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn reset_drag_state(
        restore: bool,
    ) -> Option<crate::hover_core::HoverTabDragSession> {
        let session = crate::hover_core::clear_hover_tab_drag();
        if restore {
            if let Some(session) = session {
                let _ = crate::share_priority::preview_hover_tab_vertical_offset(
                    session.original_offset,
                );
            }
        }
        set_drag_active(false);
        session
    }

    /// Get the current cursor position in global logical points via a raw
    /// CoreGraphics `CGEventCreate` + `CGEventGetLocation` call. This avoids
    /// adding a new dependency (no enigo, no extra objc2 features) — just the
    /// CoreGraphics framework link that's already needed for the window
    /// hit-test below.
    pub fn get_cursor_position() -> Option<(f64, f64)> {
        crate::platform::cg::cursor_position()
    }

    /// Idempotent start of the background hover-tracking thread.
    pub fn start(app: &tauri::AppHandle) {
        if ACTIVE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        log::debug!("hover_tab: tracker started");
        let app = app.clone();
        std::thread::spawn(move || {
            run(&app);
            ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// Idempotent stop; hides the tab.
    #[allow(dead_code)]
    pub fn stop(app: &tauri::AppHandle) {
        if ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            log::debug!("hover_tab: stop() hiding tab");
            hide_tab(app);
        }
    }

    fn run(app: &tauri::AppHandle) {
        let mut last: Option<(WindowFrame, u32)> = None;
        let mut shown = false;
        // #761 velocity-lead state: previous cursor sample + EMA velocity.
        let mut prev_cursor: Option<((f64, f64), std::time::Instant)> = None;
        let mut cursor_vel: (f64, f64) = (0.0, 0.0);
        // One info line per transition INTO not-in-room suppression (issue
        // #22: "invisible failures stay invisible" -- the pill being hidden
        // because the user isn't in a meeting is by-design, but must be
        // observable in petal.log rather than indistinguishable from a bug).
        let mut suppressed_logged = false;
        // The tab's current on-screen rect (logical points), so we can tell
        // when the cursor is over the TAB itself (frozen tracking — moving
        // onto the tab to click it must not resolve the window behind it).
        let mut tab_rect: Option<HoverTabRect> = None;
        let mut order_poll_tick = 0_u64;
        let mut missed_tab_bridge_ticks = 0_u8;
        // #743: shared window snapshot, refreshed at ~10Hz (STACK_REFRESH_TICKS).
        let mut cached_stack: Option<CachedHoverStack> = None;
        let mut stack_refresh_tick = 0_u64;

        loop {
            if !ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
                hide_tab(app);
                return;
            }
            if super::MENU_OPEN.load(std::sync::atomic::Ordering::Acquire)
                || DRAG_ACTIVE.load(std::sync::atomic::Ordering::Acquire)
            {
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                continue;
            }

            // The share pill only makes sense while in a meeting (SPEC.md §4.2:
            // sharing is an in-meeting action). Outside a room, keep the tab
            // hidden entirely instead of inviting a share that would fail with
            // NotInRoom.
            let in_room = tauri::Manager::try_state::<crate::session::SessionState>(app)
                .map(|s| s.current_room_name().is_some())
                .unwrap_or(false);
            if !in_room {
                if shown {
                    hide_tab(app);
                    shown = false;
                    tab_rect = None;
                    last = None;
                    missed_tab_bridge_ticks = 0;
                }
                if !suppressed_logged {
                    log::info!(
                        "hover_tab: suppressed -- not in a room (pill only appears while in a meeting)"
                    );
                    suppressed_logged = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS * 4));
                continue;
            }
            if suppressed_logged {
                log::info!("hover_tab: in a room -- pill tracking active");
                suppressed_logged = false;
            }

            // #743: adaptive snapshot cadence. While a pill is SHOWN it is
            // actively following a window, so refresh every tick (60Hz) to keep
            // follow latency at the §9.7 baseline. While HIDDEN -- the common
            // idle case, and the real WindowServer waste (§9.6) -- refresh at
            // ~10Hz; a stale window position cannot matter when no pill is up.
            // Keep the last good snapshot across a transient enumeration failure.
            let refresh_ticks = if shown { 1 } else { STACK_REFRESH_TICKS };
            if cached_stack.is_none() || stack_refresh_tick % refresh_ticks == 0 {
                // #744: while a pill is SHOWN, force a fresh registry sweep so
                // follow latency stays at the §9.7 baseline (the 10Hz ingest
                // alone would follow a moving window at only ~10Hz -- the exact
                // regression the live harness caught). HIDDEN, read the cheap
                // 10Hz snapshot. #747 §4: when the gesture tap is actively
                // tracking the FOLLOWED window, the snapshot is already fresh
                // for it (per-id reads at device rate) and the sweep is
                // skipped -- zero full enumerations during a tracked drag.
                if let Some(fresh) = refresh_hover_stack(shown, last.map(|(_, wid)| wid)) {
                    cached_stack = Some(fresh);
                }
            }
            stack_refresh_tick = stack_refresh_tick.wrapping_add(1);
            let Some(stack) = cached_stack.as_ref() else {
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                continue;
            };

            let cursor = get_cursor_position();
            let petal_view_blocked =
                cursor.is_some_and(crate::region_window::cursor_inside_registered_region);
            if petal_view_blocked {
                if shown {
                    hide_tab(app);
                    shown = false;
                }
                tab_rect = None;
                last = None;
                missed_tab_bridge_ticks = 0;
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                continue;
            }
            let over_tab = matches!((cursor, current_hover_presentation()), (Some(cursor), Some(presentation))
                if cursor_over_tab(cursor, presentation.rect));
            if over_tab {
                missed_tab_bridge_ticks = 0;
                if let Some((_, window_id)) = last {
                    maybe_reassert_hover_tab_order(app, window_id, &mut order_poll_tick, stack);
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                continue;
            }

            // #761: during a RIGID drag of the followed window, the hit IS
            // that window by invariant -- the mouse is held down on its title
            // bar until mouse-up. Geometric hit-testing against the snapshot
            // frame (which trails a fast cursor) intermittently concluded
            // "cursor left the window" and flickered the pill hide/show
            // (live round-4 finding). The gesture pin bypasses the hit-test
            // entirely for the duration of the rigid gesture; every other
            // situation keeps the normal test.
            let gesture_pin: Option<(WindowFrame, u32)> = match (last, cursor) {
                (Some((_, wid)), Some(_)) => {
                    crate::platform::gesture_tap::gesture_track_for(wid, 100)
                        .filter(|t| t.rigid)
                        .map(|t| (reckoned_frame(&t, cursor.unwrap_or((t.cx, t.cy))), wid))
                }
                _ => None,
            };
            let mut blocked_by_surface = false;
            let mut hit = gesture_pin.or_else(|| {
                let Some(cursor) = cursor else {
                    return None;
                };
                match shareable_window_in(cursor, stack) {
                    HoverTargetResolution::Candidate(hit) => Some(hit),
                    HoverTargetResolution::Blocked => {
                        blocked_by_surface = true;
                        None
                    }
                    HoverTargetResolution::NoHit => None,
                }
            });

            // #761: EMA cursor velocity (points/sec) for the lead term. Cheap
            // and always maintained; only APPLIED during rigid drags below.
            if let Some(c) = cursor {
                let now = std::time::Instant::now();
                if let Some((pc, pt)) = prev_cursor {
                    let dt = now.duration_since(pt).as_secs_f64();
                    if dt > 0.001 {
                        let vx = (c.0 - pc.0) / dt;
                        let vy = (c.1 - pc.1) / dt;
                        // EMA damps sampling noise without lagging much.
                        cursor_vel.0 = cursor_vel.0 * 0.6 + vx * 0.4;
                        cursor_vel.1 = cursor_vel.1 * 0.6 + vy * 0.4;
                    }
                }
                prev_cursor = Some((c, now));
            }

            if blocked_by_surface {
                if shown {
                    hide_tab(app);
                    shown = false;
                }
                tab_rect = None;
                last = None;
                missed_tab_bridge_ticks = 0;
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                continue;
            }

            // #761 cursor-lock dead reckoning: during a RIGID title-bar drag
            // the window is cursor-attached, so the freshest window position
            // is last_real_frame + (cursor_now - cursor_at_that_frame) -- an
            // exact reconstruction (not an extrapolation) -- PLUS a small
            // velocity lead that cancels the fixed pipeline latency (hover
            // tick -> main-thread hop -> render), which live testing showed
            // still trails at drag speed. Only while the gesture track is
            // fresh AND proven rigid; content drags keep the real frame.
            if let (Some((frame, wid)), Some(c)) = (hit.as_mut(), cursor) {
                if let Some(track) = crate::platform::gesture_tap::gesture_track_for(*wid, 100) {
                    if track.rigid {
                        let (lx, ly) = lead_offset(cursor_vel, 0.022, 48.0);
                        let mut f = reckoned_frame(&track, c);
                        f.x += lx.round() as i32;
                        f.y += ly.round() as i32;
                        *frame = f;
                    }
                }
            }

            if !same_hit(&hit, &last) {
                match hit {
                    Some((frame, window_id)) => {
                        let presentation = tab_position(app, &frame, window_id);
                        crate::hover_tab::position_tab(app, presentation);
                        order_hover_tab_above(app, window_id);
                        order_poll_tick = 0;
                        tab_rect = Some(presentation.rect);
                        missed_tab_bridge_ticks = 0;
                        shown = true;
                        log::info!(
                            "hover_tab: show at ({:.0},{:.0}) for window {window_id} attachment={:?}",
                            presentation.rect.x,
                            presentation.rect.y,
                            presentation.attachment
                        );

                        let payload = super::HoverTabUpdate {
                            window_id,
                            frame,
                            tab_x: presentation.rect.x,
                            tab_y: presentation.rect.y,
                            attachment: presentation.attachment,
                            vertical_offset:
                                crate::share_priority::current_hover_tab_vertical_offset(),
                            shared: super::is_shared(app, window_id),
                            display_like: false,
                        };
                        super::set_last_hover_update(Some(payload.clone()));
                        // ROOT CAUSE OF ISSUE #22 -- this MUST be a global
                        // `emit`, not `emit_to(HOVER_TAB_LABEL, ...)`.
                        // Verified in tauri 2.11's own source
                        // (`manager/mod.rs::emit_to::filter_target`):
                        // `emit_to("<label>")` becomes
                        // `EventTarget::AnyLabel { label }`, which only
                        // delivers to listeners registered with a
                        // label-specific target. The page's plain JS
                        // `listen(...)` (see `@tauri-apps/api/event`)
                        // registers `EventTarget::Any`, which falls through
                        // `filter_target`'s `_ => false` arm -- so the
                        // update/hide events were NEVER delivered, the page's
                        // `visible` state stayed false, and the pill panel
                        // was shown on screen while painting zero pixels
                        // (`visibility: hidden`). Global `emit` delivers to
                        // `Any` listeners -- the same delivery pattern
                        // already live-proven by `presence-update`/
                        // `room-left` (see session.rs's own comment on
                        // `room-left`).
                        let _ = tauri::Emitter::emit(app, "hover-tab-update", payload);
                    }
                    None => {
                        if shown
                            && hold_hover_tab_through_transient_miss(
                                cursor,
                                current_hover_presentation().map(|p| p.rect).or(tab_rect),
                                last,
                                missed_tab_bridge_ticks,
                            )
                        {
                            missed_tab_bridge_ticks = missed_tab_bridge_ticks.saturating_add(1);
                            if let Some((_, window_id)) = last {
                                maybe_reassert_hover_tab_order(
                                    app,
                                    window_id,
                                    &mut order_poll_tick,
                                    stack,
                                );
                            }
                            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                            continue;
                        }
                        if shown {
                            hide_tab(app);
                            shown = false;
                            tab_rect = None;
                            missed_tab_bridge_ticks = 0;
                            log::info!(
                                "hover_tab: hide -- cursor left all visible shareable windows or a frontmost window is blocking the tab"
                            );
                        }
                    }
                }
                last = hit;
            } else if shown {
                missed_tab_bridge_ticks = 0;
                if let Some((_, window_id)) = last {
                    maybe_reassert_hover_tab_order(app, window_id, &mut order_poll_tick, stack);
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    }

    fn maybe_reassert_hover_tab_order(
        app: &tauri::AppHandle,
        window_id: u32,
        order_poll_tick: &mut u64,
        stack: &CachedHoverStack,
    ) {
        *order_poll_tick = order_poll_tick.wrapping_add(1);
        if *order_poll_tick % ORDER_REASSERT_TICKS != 0 {
            return;
        }

        let tab_number = *HOVER_TAB_WINDOW_NUMBER.lock_unpoisoned();
        // #743: read the loop's shared snapshot instead of a second full
        // enumeration.
        let stack_entries = stack
            .snap
            .records_front_to_back()
            .map(|r| HoverStackEntry {
                number: r.wid as i64,
                owner_pid: r.owner_pid as i64,
            })
            .collect::<Vec<_>>();
        if hover_tab_needs_reorder(
            &stack_entries,
            tab_number,
            window_id as i64,
            std::process::id() as i64,
        ) {
            order_hover_tab_above(app, window_id);
        }
    }

    fn order_hover_tab_above(app: &tauri::AppHandle, target_window_id: u32) {
        let app_main = app.clone();
        let generation = crate::hover_core::hover_tab_presentation_generation();
        if let Err(e) = app.run_on_main_thread(move || {
            use objc2::msg_send;
            use objc2::runtime::AnyObject;
            use tauri::Manager;

            if crate::hover_core::hover_tab_presentation_generation() != generation {
                return;
            }
            let Some(window) = app_main.get_webview_window(super::HOVER_TAB_LABEL) else {
                return;
            };
            if !window.is_visible().unwrap_or(false) {
                return;
            }
            let Ok(ns_ptr) = window.ns_window() else {
                log::warn!(
                    "hover_tab: ns_window() unavailable; cannot order above window {target_window_id}"
                );
                return;
            };
            let target_is_petal_view = crate::region_window::resolve(target_window_id).is_some();
            unsafe {
                let ns = ns_ptr as *mut AnyObject;
                // Petal View selectors are always-on-top panels. Match their
                // floating band only for this target; ordinary targets stay
                // in the normal band and never leave the pill globally above
                // unrelated applications.
                let level = if target_is_petal_view { 3isize } else { 0isize };
                let _: () = msg_send![ns, setLevel: level];
                if target_is_petal_view {
                    // Region ids are Petal registry tokens, not AppKit window
                    // numbers. Ordering relative to that token is a no-op;
                    // the floating-band front order is the reliable path.
                    let _: () = msg_send![ns, orderFrontRegardless];
                } else {
                    let _: () =
                        msg_send![ns, orderWindow: 1isize, relativeTo: target_window_id as isize];
                }
                let number: i64 = msg_send![ns, windowNumber];
                *HOVER_TAB_WINDOW_NUMBER.lock_unpoisoned() = number;
            }
        }) {
            log::warn!(
                "hover_tab: failed to schedule z-order assertion above window {target_window_id}: {e}"
            );
        }
    }

    /// CHARACTERIZATION NOTE (#742): this cache is never invalidated. A pid
    /// reused by a different app after the first lookup keeps the ORIGINAL
    /// app's bundle id for the life of the process, which matters because the
    /// value feeds the share denylist. A window registry that resolves bundle
    /// ids per-record must decide deliberately whether to keep that behaviour.
    fn cached_owner_bundle_id_for_pid(pid: i64) -> Option<String> {
        let Ok(pid_i32) = i32::try_from(pid) else {
            return None;
        };
        let cache = OWNER_BUNDLE_ID_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(cached) = cache.lock_unpoisoned().get(&pid).cloned() {
            return cached;
        }

        let resolved = crate::share_target::bundle_id_for_pid(pid_i32);
        cache.lock_unpoisoned().insert(pid, resolved.clone());
        resolved
    }

    /// Project a CoreGraphics on-screen snapshot into hit-test input.
    ///
    /// Extracted as a seam (#742) so the projection can be driven from
    /// fixtures. `owner_bundle_ids` is index-parallel to `windows`; pairing is
    /// POSITIONAL, so a caller that filters one list and not the other would
    /// silently mis-attribute bundle ids to windows. Pinned by
    /// `hover_snapshots_*` tests.
    /// The 4-title own-chrome check is shared with the registry's
    /// `NameChromeOracle` via `hover_core::is_own_chrome_title` (re-exported
    /// above) so both compute `decorative` identically (§9.9).

    pub(super) fn hover_snapshots<'a>(
        windows: &'a [crate::platform::cg::WindowEntry],
        owner_bundle_ids: &'a [Option<String>],
        self_pid: i64,
    ) -> Vec<HoverWindowSnapshot<'a>> {
        windows
            .iter()
            .zip(owner_bundle_ids.iter())
            .map(|(entry, owner_bundle_id)| HoverWindowSnapshot {
                number: entry.number,
                layer: entry.layer,
                owner_pid: entry.owner_pid,
                owner_bundle_id: owner_bundle_id.as_deref(),
                decorative: entry.owner_pid == self_pid && is_own_chrome_title(&entry.name),
                region_selector: u32::try_from(entry.number)
                    .ok()
                    .and_then(crate::region_window::resolve)
                    .is_some()
                    || (entry.owner_pid == self_pid
                        && crate::region_window::is_region_window_title(&entry.name)),
                x: entry.x,
                y: entry.y,
                w: entry.w,
                h: entry.h,
            })
            .collect()
    }

    /// One CoreGraphics window-list read plus its per-window bundle ids,
    /// cached in the tracker loop and refreshed at ~10Hz (#743). The 60Hz
    /// cursor poll hit-tests against this; a window move surfaces at the
    /// refresh cadence, which matches the ~10Hz follow the app already shows
    /// live (§9.7 baseline). Before #743 both the hit-test and the z-order
    /// reassert did their OWN full enumeration every tick -- up to ~70
    /// enumerations/second; now it is one shared read at ~10Hz.
    pub(super) struct CachedHoverStack {
        /// #744: the shared registry snapshot (no more hover-private
        /// enumeration -- the winsrv ingest thread produces this at ~10Hz).
        snap: std::sync::Arc<crate::window_registry::Snapshot>,
        /// Per-record owner bundle id, index-parallel to
        /// `snap.records_front_to_back()`, resolved through hover's own cache
        /// (kept here rather than in the registry: only hover needs it, and
        /// only for the denylist check).
        bundle_ids: Vec<Option<String>>,
    }

    fn refresh_hover_stack(force_fresh: bool, followed: Option<u32>) -> Option<CachedHoverStack> {
        let reg = crate::window_registry::global()?;
        // Shown -> fresh-for-follow read (a full sweep unless the gesture tap
        // is already feeding the followed window, #747 §4); hidden -> the
        // ~10Hz ingest snapshot. See the call site (#744).
        let snap = if force_fresh {
            reg.refresh_for_follow(followed)
        } else {
            reg.snapshot()
        };
        if snap.order.is_empty() {
            return None;
        }
        let bundle_ids = snap
            .records_front_to_back()
            .map(|r| cached_owner_bundle_id_for_pid(r.owner_pid as i64))
            .collect::<Vec<_>>();
        Some(CachedHoverStack { snap, bundle_ids })
    }

    pub(super) fn reckoned_frame(
        track: &crate::platform::gesture_tap::GestureTrack,
        cursor: (f64, f64),
    ) -> WindowFrame {
        let dx = cursor.0 - track.cx;
        let dy = cursor.1 - track.cy;
        WindowFrame {
            x: (track.fx + dx).round() as i32,
            y: (track.fy + dy).round() as i32,
            width: track.fw.round() as i32,
            height: track.fh.round() as i32,
        }
    }

    /// #761 nudge bookkeeping: the tick must not overwrite a fresher
    /// event-driven position with its own older sample (that read as a
    /// backwards jump every tick). Nanos-since-epoch of the last nudge.
    static LAST_NUDGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub(super) fn note_drag_nudge() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        LAST_NUDGE.store(now, std::sync::atomic::Ordering::Relaxed);
    }
    /// #761: coarse emit throttle for per-event nudges.
    static LAST_EMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub(super) fn emit_throttle_ok(min_interval_ms: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_EMIT.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) >= min_interval_ms {
            LAST_EMIT.store(now, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub(super) fn drag_nudge_fresh(max_ms: u64) -> bool {
        let last = LAST_NUDGE.load(std::sync::atomic::Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now.saturating_sub(last) <= max_ms
    }

    /// #761 velocity lead: offset that cancels the fixed pill pipeline
    /// latency (hover tick -> main-thread hop -> WindowServer render, ~1-2
    /// frames). `vel` is the EMA-smoothed cursor velocity in points/sec;
    /// the offset is capped so a sampling glitch can never fling the pill.
    /// Applied ONLY during rigid drags, so drag-end overshoot is bounded by
    /// one reconcile snap. Pure; unit-tested.
    pub(super) fn lead_offset(vel: (f64, f64), lead_secs: f64, cap: f64) -> (f64, f64) {
        let lx = (vel.0 * lead_secs).clamp(-cap, cap);
        let ly = (vel.1 * lead_secs).clamp(-cap, cap);
        (lx, ly)
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum HoverTargetResolution {
        NoHit,
        Blocked,
        Candidate((WindowFrame, u32)),
    }

    fn shareable_window_in(cursor: (f64, f64), stack: &CachedHoverStack) -> HoverTargetResolution {
        // The hollow selector is a blocker even when its native surface is
        // click-through. The caller clears the existing pill before this
        // candidate walk; keep this guard here as a second line of defense for
        // callers that use the pure hit-test path directly.
        if crate::region_window::cursor_inside_registered_region(cursor) {
            return HoverTargetResolution::Blocked;
        }
        let self_pid = std::process::id() as i64;
        // Build hit-test input from the registry records: own-chrome comes from
        // the precomputed PetalOwned{decorative} class (§9.9), geometry from the
        // RAW f64 (the decision rounds the returned frame, as before), bundle
        // ids from hover's cache. Byte-identical to the old cg-projection path,
        // proven by hover_hit_decisions_match_registry_transfer.
        let snapshots: Vec<HoverWindowSnapshot> = stack
            .snap
            .records_front_to_back()
            .zip(stack.bundle_ids.iter())
            .map(|(r, bid)| {
                if matches!(r.class, crate::window_registry::WindowClass::RegionSelector) {
                    crate::region_window::register(crate::region_window::RegionWindowSource::new(
                        r.wid,
                        r.owner_pid,
                        crate::region_window::resolve(r.wid)
                            .map(|source| source.title)
                            .unwrap_or_else(|| {
                                crate::region_window::REGION_WINDOW_TITLE_PREFIX.to_string()
                            }),
                        crate::region_window::RegionRect::new(r.rx, r.ry, r.rw, r.rh),
                    ));
                }
                HoverWindowSnapshot {
                    number: r.wid as i64,
                    layer: r.layer,
                    owner_pid: r.owner_pid as i64,
                    owner_bundle_id: bid.as_deref(),
                    decorative: matches!(
                        r.class,
                        crate::window_registry::WindowClass::PetalOwned { decorative: true }
                    ),
                    region_selector: matches!(
                        r.class,
                        crate::window_registry::WindowClass::RegionSelector
                    ),
                    x: r.rx,
                    y: r.ry,
                    w: r.rw,
                    h: r.rh,
                }
            })
            .collect();
        match hover_hit_test_decision(snapshots, cursor, self_pid) {
            HoverHitTestDecision::ShareableCandidate { window_id, frame } => {
                // #747 stage-1: a window AX-classified as a chrome-less Popup is
                // not a real shareable window (alt-tab/AeroSpace, §3). Pure
                // cache reads only (no AX on the 60Hz path). While a
                // classification is genuinely IN FLIGHT (stage-1 live, not yet
                // settled, app not AXDead) the pill is DEFERRED instead of
                // shown: resolution runs on the 10Hz ingest thread, so a fresh
                // popup's kind lands ~100ms after the window appears, and
                // showing during that gap flashed the pill on popups (live
                // popup gate catch, #747 audit). Bounded ~1s worst case; on
                // degraded/T2 rigs the gate is false and the pill shows as
                // before, so offline goldens are unchanged.
                if let Some(reg) = crate::window_registry::global() {
                    if reg.window_kind(window_id) == Some(crate::window_registry::AxKind::Popup) {
                        return HoverTargetResolution::Blocked;
                    }
                    let owner_pid = stack.snap.owner_pid(window_id).unwrap_or_default();
                    if reg.kind_resolution_in_flight(window_id, owner_pid) {
                        return HoverTargetResolution::Blocked;
                    }
                }
                HoverTargetResolution::Candidate((frame, window_id))
            }
            HoverHitTestDecision::BlockedByOwnProcess
            | HoverHitTestDecision::BlockedByExternalWindow => HoverTargetResolution::Blocked,
            HoverHitTestDecision::NoHit => HoverTargetResolution::NoHit,
        }
    }

    fn hide_tab(app: &tauri::AppHandle) {
        // A hidden source is a cancellation boundary; do not commit a
        // previewed position after the target has disappeared.
        reset_drag_state(true);
        super::set_last_hover_update(None);
        crate::hover_tab::hide_tab_window(app);
        // Global `emit` -- same issue-#22 root cause as `hover-tab-update`
        // above: `emit_to` never reaches the page's plain `listen()`.
        let _ = tauri::Emitter::emit(app, "hover-tab-hide", ());
    }

    /// Compute the actual native panel rectangle in logical points. Every
    /// target uses the same fixed right-edge square and shared offset.
    pub(super) fn tab_position(
        app: &tauri::AppHandle,
        frame: &WindowFrame,
        window_id: u32,
    ) -> HoverTabPresentation {
        tab_position_with_offset(
            app,
            frame,
            window_id,
            crate::share_priority::current_hover_tab_vertical_offset(),
        )
    }

    pub(super) fn tab_position_with_offset(
        app: &tauri::AppHandle,
        frame: &WindowFrame,
        window_id: u32,
        vertical_offset: f64,
    ) -> HoverTabPresentation {
        let monitor = get_monitor_with_cursor(app);
        let bounds = monitor
            .as_ref()
            .map(|m| {
                let scale = m.scale_factor();
                // Tauri's monitor work_area is backed by AppKit's
                // NSScreen.visibleFrame on macOS, excluding the menu bar and
                // Dock. Keep source-relative Top/Bottom inside that usable
                // rectangle instead of the full display frame.
                let work = m.work_area();
                let pos = work.position;
                let size = work.size;
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
            ));
        hover_tab_presentation_with_offset(window_id, *frame, bounds, vertical_offset)
    }

    /// Find the Tauri `Monitor` containing the cursor's current position.
    /// Own minimal equivalent of takt's `overlay::get_monitor_with_cursor`.
    pub fn get_monitor_with_cursor(app: &tauri::AppHandle) -> Option<tauri::Monitor> {
        let (cx, cy) = get_cursor_position()?;
        if let Ok(monitors) = app.available_monitors() {
            for monitor in monitors {
                let scale = monitor.scale_factor();
                let mx = monitor.position().x as f64 / scale;
                let my = monitor.position().y as f64 / scale;
                let mw = monitor.size().width as f64 / scale;
                let mh = monitor.size().height as f64 / scale;
                if cx >= mx && cx < mx + mw && cy >= my && cy < my + mh {
                    return Some(monitor);
                }
            }
        }
        app.primary_monitor().ok().flatten()
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn start(_app: &tauri::AppHandle) {}
    pub fn stop(_app: &tauri::AppHandle) {}
}

/// Start the hover-tab tracker. Idempotent; macOS-only (no-op elsewhere).
pub fn start(app: &AppHandle) {
    platform::start(app);
}

/// Stop the hover-tab tracker and hide the pill. Idempotent.
///
/// Not called anywhere yet — `run()` in `lib.rs` currently starts the tracker
/// unconditionally at launch (there's no "screenshare session" lifecycle in
/// this scaffold yet to gate it on). Exposed now so a future session-stop
/// hook can call it without further plumbing.
#[allow(dead_code)]
pub fn stop(app: &AppHandle) {
    platform::stop(app);
}

/// `true` once `force_window_transparent` has verifiably found+treated the
/// pill's WKWebView. Until then, every show re-applies the treatment: the
/// create-time application in `lib.rs::create_hover_tab` runs during Tauri's
/// `setup()`, where the treatment has been observed NOT to stick (the pill
/// rendered an opaque black rect on a build that already contained the
/// create-time call), so re-applying at show time -- when the webview is
/// guaranteed attached and loaded -- is the reliable point.
static HOVER_TAB_TRANSPARENCY_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Move, resize, and show the fixed hover-tab panel in one AppKit turn. The
/// size is applied before the position so a stale queued move cannot paint at
/// the wrong geometry.
#[cfg(target_os = "macos")]
fn position_tab(app: &AppHandle, presentation: HoverTabPresentation) {
    use std::sync::atomic::Ordering;
    use tauri::Manager;
    let generation = crate::hover_core::begin_hover_tab_presentation();
    let x = presentation.rect.x;
    let y = presentation.rect.y;
    if let Some(w) = app.get_webview_window(HOVER_TAB_LABEL) {
        crate::platform::on_main(
            app,
            format!("hover_tab: position/show at ({x:.0},{y:.0})"),
            move || {
                if crate::hover_core::hover_tab_presentation_generation() != generation {
                    return;
                }
                apply_hover_tab_panel_frame(&w, presentation.rect);
                if !HOVER_TAB_TRANSPARENCY_APPLIED.load(Ordering::Relaxed)
                    && crate::webview_transparency::force_window_transparent(&w)
                {
                    HOVER_TAB_TRANSPARENCY_APPLIED.store(true, Ordering::Relaxed);
                }
                refresh_hover_tab_layout(&w);
                show_hover_tab_panel_if_hidden(&w);
                refresh_hover_tab_layout(&w);
            },
        );
    }
}

/// The `w.show()` suppression itself (#680 Rank 3), pulled out of
/// `position_tab`'s closure so a test can drive this exact conditional --
/// not a reimplemented copy of its logic -- against a fake that records real
/// call counts. `tauri::WebviewWindow` (below) is a pure passthrough, so
/// production behavior is unchanged.
#[cfg(target_os = "macos")]
fn show_hover_tab_panel_if_hidden<W: HoverTabPanelHandle>(window: &W) {
    if !window.hover_tab_panel_is_visible() {
        window.hover_tab_panel_show();
    }
}

#[cfg(target_os = "macos")]
trait HoverTabPanelHandle {
    fn hover_tab_panel_is_visible(&self) -> bool;
    fn hover_tab_panel_show(&self);
}

#[cfg(target_os = "macos")]
impl HoverTabPanelHandle for tauri::WebviewWindow {
    fn hover_tab_panel_is_visible(&self) -> bool {
        tauri::WebviewWindow::is_visible(self).unwrap_or(false)
    }

    fn hover_tab_panel_show(&self) {
        let _ = tauri::WebviewWindow::show(self);
    }
}

#[cfg(target_os = "macos")]
trait HoverTabGeometryHandle {
    fn resize(&self, width: f64, height: f64) -> Result<(), String>;
    fn move_to(&self, x: f64, y: f64) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
impl HoverTabGeometryHandle for tauri::WebviewWindow {
    fn resize(&self, width: f64, height: f64) -> Result<(), String> {
        self.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }))
            .map_err(|error| error.to_string())
    }

    fn move_to(&self, x: f64, y: f64) -> Result<(), String> {
        self.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }))
            .map_err(|error| error.to_string())
    }
}

#[cfg(target_os = "macos")]
fn apply_geometry<W: HoverTabGeometryHandle>(window: &W, rect: HoverTabRect) -> Result<(), String> {
    window.resize(rect.width, rect.height)?;
    window.move_to(rect.x, rect.y)
}

#[cfg(target_os = "macos")]
fn apply_hover_tab_panel_frame(window: &tauri::WebviewWindow, rect: HoverTabRect) {
    if let Err(error) = apply_geometry(window, rect) {
        log::warn!("hover_tab: failed to apply panel geometry: {error}");
    }
}

#[cfg(target_os = "macos")]
fn refresh_hover_tab_layout(window: &tauri::WebviewWindow) {
    if let Err(e) = window.eval(hover_tab_layout_refresh_script()) {
        log::warn!(
            "hover_tab: failed to refresh layout for '{}': {e}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn hover_tab_layout_refresh_script() -> &'static str {
    r#"(() => {
  const measure = () => {
    const root = document.documentElement;
    const host = document.querySelector('.hover-tab-host');
    const pill = document.querySelector('.pill.attach');
    const button = document.querySelector('.hover-tab-action');
    root.style.setProperty('--petal-hover-tab-refresh', String(performance.now()));
    if (host) host.toggleAttribute('data-petal-layout-refresh');
    void root.offsetWidth;
    if (host) void host.offsetWidth;
    if (pill) void pill.offsetWidth;
    if (button) void button.offsetWidth;
  };
  measure();
  window.dispatchEvent(new Event('resize'));
  requestAnimationFrame(measure);
})();"#
}

/// Hide the hover-tab panel.
#[cfg(target_os = "macos")]
fn hide_tab_window(app: &AppHandle) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use tauri::Manager;
    let generation = crate::hover_core::begin_hover_tab_presentation();
    if let Some(w) = app.get_webview_window(HOVER_TAB_LABEL) {
        crate::platform::on_main(app, "hover_tab: hide/reset level".to_string(), move || {
            if crate::hover_core::hover_tab_presentation_generation() != generation {
                return;
            }
            if let Ok(ns_ptr) = w.ns_window() {
                unsafe {
                    let ns = ns_ptr as *mut AnyObject;
                    let _: () = msg_send![ns, setLevel: 0isize];
                }
            }
            let _ = w.hide();
        });
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::platform::{
        cursor_in_hover_tab_bridge, cursor_over_tab, hold_hover_tab_through_transient_miss,
        hover_snapshots, hover_tab_presentation, is_own_chrome_title, HoverTabRect, MonitorBounds,
        HOVER_TAB_BRIDGE_TOP_PADDING, HOVER_TAB_BRIDGE_WINDOW_OVERLAP, HOVER_TAB_CURSOR_SLOP_X,
        HOVER_TAB_CURSOR_SLOP_Y, HOVER_TAB_HIDE_GRACE_TICKS,
    };
    // The compact tab dimensions live in `hover_core`, not in `hover_tab`'s
    // own `platform` module -- importing them from `platform` here does not
    // compile.
    use super::{HOVER_TAB_COMPACT_HEIGHT, HOVER_TAB_COMPACT_WIDTH};
    #[cfg(target_os = "macos")]
    use super::HoverTabPanelHandle;
    use super::WindowFrame;

    #[test]
    fn lead_offset_is_proportional_capped_and_zero_at_rest() {
        assert_eq!(
            super::platform::lead_offset((0.0, 0.0), 0.022, 48.0),
            (0.0, 0.0)
        );
        // 1000 px/s * 22ms = 22px lead
        let (lx, ly) = super::platform::lead_offset((1000.0, -500.0), 0.022, 48.0);
        assert!((lx - 22.0).abs() < 0.01 && (ly + 11.0).abs() < 0.01);
        // flick at 5000 px/s: capped at 48px, sign preserved
        let (cx2, cy2) = super::platform::lead_offset((5000.0, -5000.0), 0.022, 48.0);
        assert_eq!((cx2, cy2), (48.0, -48.0));
    }

    #[test]
    fn reckoned_frame_translates_by_cursor_delta_only() {
        let track = crate::platform::gesture_tap::GestureTrack {
            wid: 7,
            fx: 100.0,
            fy: 200.0,
            fw: 640.0,
            fh: 480.0,
            cx: 150.0,
            cy: 210.0,
            rigid: true,
            diverge_streak: 0,
            vx: 0.0,
            vy: 0.0,
            at: std::time::Instant::now(),
        };
        let f = super::platform::reckoned_frame(&track, (174.0, 190.0)); // cursor +24, -20
        assert_eq!((f.x, f.y), (124, 180));
        assert_eq!((f.width, f.height), (640, 480), "size never reckoned");
        // zero delta = exactly the last real frame
        let f0 = super::platform::reckoned_frame(&track, (150.0, 210.0));
        assert_eq!((f0.x, f0.y), (100, 200));
    }

    /// A deterministic adapter for the two production entry points. `begin_*`
    /// models the selection boundary and deliberately leaves media start
    /// pending until `complete_start` is called, so tests can prove focus is
    /// dispatched before capture/publish is allowed to resolve.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FocusStartEntry {
        Direct,
        SystemPicker,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FocusStartEvent {
        Handback(FocusStartEntry, super::FocusHandoffAction),
        StartPending(FocusStartEntry),
        StartCompleted(FocusStartEntry, bool),
    }

    #[derive(Debug, Clone, Copy)]
    struct PendingFocusStart {
        entry: FocusStartEntry,
        attempt: super::ShareFocusAttempt,
    }

    #[derive(Default)]
    struct FocusStartAdapter {
        attempts: super::ShareFocusAttempts,
        events: Vec<FocusStartEvent>,
    }

    impl FocusStartAdapter {
        fn begin(
            &mut self,
            entry: FocusStartEntry,
            window_id: u32,
            source_pid: Option<i32>,
            petal_active: bool,
        ) -> PendingFocusStart {
            self.begin_with_revalidation(entry, window_id, source_pid, petal_active, true)
        }

        fn begin_with_revalidation(
            &mut self,
            entry: FocusStartEntry,
            window_id: u32,
            source_pid: Option<i32>,
            petal_active: bool,
            owner_matches_selection: bool,
        ) -> PendingFocusStart {
            let attempt = super::apply_share_focus_lifecycle(
                &mut self.attempts,
                super::ShareFocusLifecycle::Selected {
                    window_id,
                    selected_owner_pid: source_pid,
                    source_pid,
                    selected_frontmost_snapshot: 1,
                    selected_at: std::time::Instant::now(),
                },
            )
            .expect("selection creates a focus attempt");
            // #677: frontmost_matches is no longer a handback gate — Petal
            // becoming frontmost after the selection snapshot is exactly the
            // steal case we must recover from.
            let action = super::focus_handoff_action(
                self.attempts.is_current(attempt),
                petal_active,
                owner_matches_selection,
                false,
                true,
                attempt.source_pid,
            );
            self.events.push(FocusStartEvent::Handback(entry, action));
            self.events.push(FocusStartEvent::StartPending(entry));
            PendingFocusStart { entry, attempt }
        }

        fn begin_direct(
            &mut self,
            window_id: u32,
            source_pid: Option<i32>,
            petal_active: bool,
        ) -> PendingFocusStart {
            self.begin(FocusStartEntry::Direct, window_id, source_pid, petal_active)
        }

        fn begin_system_picker(
            &mut self,
            window_id: u32,
            source_pid: Option<i32>,
            petal_active: bool,
        ) -> PendingFocusStart {
            self.begin(
                FocusStartEntry::SystemPicker,
                window_id,
                source_pid,
                petal_active,
            )
        }

        fn complete_start(&mut self, pending: PendingFocusStart, result: Result<(), ()>) {
            if result.is_err() {
                super::apply_share_focus_lifecycle(
                    &mut self.attempts,
                    super::ShareFocusLifecycle::StartFailed(pending.attempt),
                );
            }
            self.events.push(FocusStartEvent::StartCompleted(
                pending.entry,
                result.is_ok(),
            ));
        }

        fn unshare(&mut self, window_id: u32) {
            super::apply_share_focus_lifecycle(
                &mut self.attempts,
                super::ShareFocusLifecycle::Unshared { window_id },
            );
        }

        fn clear_window(&mut self, window_id: u32) {
            super::apply_share_focus_lifecycle(
                &mut self.attempts,
                super::ShareFocusLifecycle::WindowCleared { window_id },
            );
        }

        fn leave_room(&mut self) {
            super::apply_share_focus_lifecycle(
                &mut self.attempts,
                super::ShareFocusLifecycle::RoomLeft,
            );
        }
    }

    #[test]
    fn hover_tab_width_is_fixed_at_40_pixels() {
        assert_eq!(HOVER_TAB_COMPACT_WIDTH, 40.0);
        assert_eq!(HOVER_TAB_COMPACT_HEIGHT, 40.0);
    }

    #[test]
    fn hover_tab_panel_size_matches_positioning_geometry() {
        let (width, height) = super::hover_tab_panel_logical_size();

        assert_eq!(
            (width, height),
            (HOVER_TAB_COMPACT_WIDTH, HOVER_TAB_COMPACT_HEIGHT)
        );
        assert_eq!(height, HOVER_TAB_COMPACT_HEIGHT);
    }

    #[test]
    fn hover_tab_layout_refresh_script_forces_measurement_and_resize() {
        let js = super::hover_tab_layout_refresh_script();

        assert!(js.contains(".hover-tab-host"));
        assert!(js.contains(".pill.attach"));
        assert!(js.contains(".hover-tab-action"));
        assert!(js.contains("offsetWidth"));
        assert!(js.contains("dispatchEvent(new Event('resize'))"));
        assert!(js.contains("requestAnimationFrame"));
        assert!(js.contains("data-petal-layout-refresh"));
    }

    #[test]
    fn every_window_starts_with_a_right_center_square() {
        let frame = WindowFrame {
            x: 300,
            y: 400,
            width: 500,
            height: 300,
        };
        let presentation =
            hover_tab_presentation(7, frame, MonitorBounds::new(0.0, 0.0, 1200.0, 800.0));
        assert_eq!(presentation.attachment, super::HoverTabAttachment::Outside);
        assert_eq!(presentation.rect.x, 800.0);
        assert_eq!(presentation.rect.y, 530.0);
        assert_eq!(presentation.rect.width, HOVER_TAB_COMPACT_WIDTH);
        assert_eq!(presentation.rect.height, HOVER_TAB_COMPACT_HEIGHT);
    }

    #[test]
    fn top_constrained_window_uses_the_same_right_center_square() {
        let frame = WindowFrame {
            x: 300,
            y: 10,
            width: 500,
            height: 300,
        };
        let presentation =
            hover_tab_presentation(7, frame, MonitorBounds::new(0.0, 0.0, 1200.0, 800.0));
        assert_eq!(presentation.attachment, super::HoverTabAttachment::Outside);
        assert_eq!(presentation.rect.x, 800.0);
        assert_eq!(presentation.rect.width, super::HOVER_TAB_COMPACT_WIDTH);
        assert_eq!(presentation.rect.height, super::HOVER_TAB_COMPACT_HEIGHT);
    }

    #[test]
    fn maximized_window_uses_bounded_inside_right_handle() {
        let frame = WindowFrame {
            x: 0,
            y: 26,
            width: 1920,
            height: 1054,
        };
        let presentation =
            hover_tab_presentation(7, frame, MonitorBounds::new(0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(presentation.attachment, super::HoverTabAttachment::Inset);
        assert_eq!(presentation.rect.x, 1920.0 - super::HOVER_TAB_COMPACT_WIDTH);
        assert!(presentation.rect.bottom() <= 1080.0);
    }

    #[test]
    fn cursor_and_bridge_use_actual_side_or_above_rectangles() {
        let frame = WindowFrame {
            x: 300,
            y: 400,
            width: 500,
            height: 300,
        };
        let above = hover_tab_presentation(7, frame, MonitorBounds::new(0.0, 0.0, 1200.0, 800.0));
        assert!(cursor_over_tab((above.rect.x, above.rect.y), above.rect));
        assert!(cursor_in_hover_tab_bridge(
            (above.rect.x + 20.0, frame.y as f64 - 2.0),
            above.rect,
            frame,
        ));
        let side = HoverTabRect {
            x: 260.0,
            y: 500.0,
            width: 40.0,
            height: 40.0,
        };
        assert!(cursor_over_tab((260.0, 500.0), side));
        assert!(cursor_in_hover_tab_bridge((295.0, 520.0), side, frame,));
    }

    #[test]
    fn transient_hover_tab_miss_holds_state_inside_actual_bridge() {
        let frame = WindowFrame {
            x: 300,
            y: 400,
            width: 500,
            height: 300,
        };
        let presentation =
            hover_tab_presentation(7, frame, MonitorBounds::new(0.0, 0.0, 1200.0, 800.0));
        let cursor = (
            presentation.rect.x + presentation.rect.width + 2.0,
            frame.y as f64 - 4.0,
        );
        assert!(!cursor_over_tab(cursor, presentation.rect));
        assert!(hold_hover_tab_through_transient_miss(
            Some(cursor),
            Some(presentation.rect),
            Some((frame, 123)),
            0,
        ));
        assert!(!hold_hover_tab_through_transient_miss(
            Some(cursor),
            Some(presentation.rect),
            Some((frame, 123)),
            HOVER_TAB_HIDE_GRACE_TICKS,
        ));
    }

    use super::platform::{
        hover_hit_test_decision, hover_tab_needs_reorder, HoverHitTestDecision, HoverStackEntry,
        HoverWindowSnapshot,
    };

    fn snapshot(
        number: i64,
        owner_pid: i64,
        name: &'static str,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> HoverWindowSnapshot<'static> {
        HoverWindowSnapshot {
            number,
            layer: 0,
            owner_pid,
            owner_bundle_id: None,
            // #744: decorative is what the hit-test consults (only for own-pid
            // windows); derive it from the test's title exactly as production
            // does via the class.
            decorative: is_own_chrome_title(name),
            region_selector: crate::region_window::is_region_window_title(name),
            x,
            y,
            w,
            h,
        }
    }

    /// GOLDEN REPLAY (#742, plan §7.1): the recorded cursor track hit-tested
    /// against the recorded window stream, through the real projection + the
    /// real `hover_hit_test_decision`. Replay conventions (deterministic, no
    /// live lookups): `self_pid` is the RECORDING app's pid (Petal's own
    /// windows in the fixture keep their real names, so the own-chrome
    /// exceptions exercise); bundle ids are all `None` (the live pid->bundle
    /// cache must not run in tests), so denylist rows never fire here.
    #[test]
    fn hover_hit_decisions_match_golden_over_recorded_session() {
        for fixture in crate::window_fixtures::REPLAY_FIXTURES {
            hover_hit_golden_one(fixture);
        }
    }

    /// GOLDEN TRANSFER (#744): the registry-fed hover path (records + class-based
    /// decorative + raw f64) must produce the SAME hit decision as the direct
    /// cg-projection path, for every fixture frame. Proves the migration to the
    /// registry is behaviour-preserving; the goldens above pin what that
    /// behaviour is.
    #[test]
    fn hover_hit_decisions_match_registry_transfer() {
        use crate::window_registry::{OwnChromeOracle, WindowClass, WindowRegistry};
        struct Chrome;
        impl OwnChromeOracle for Chrome {
            fn is_decorative(&self, name: &str) -> bool {
                is_own_chrome_title(name)
            }
        }
        let decide = |snaps: Vec<super::platform::HoverWindowSnapshot>, cursor, pid| {
            format!(
                "{:?}",
                super::platform::hover_hit_test_decision(snaps, cursor, pid)
            )
        };
        for fixture_name in crate::window_fixtures::REPLAY_FIXTURES {
            let fixture = crate::window_fixtures::load(
                &crate::window_fixtures::fixtures_dir().join(format!("{fixture_name}.jsonl")),
            );
            let self_pid = fixture
                .iter()
                .flat_map(|f| f.windows.iter())
                .find(|w| w.owner_name == "desktop" || w.owner_name.contains("Petal"))
                .map(|w| w.owner_pid)
                .expect("fixture must contain own windows");
            for f in &fixture {
                let Some(cursor) = f.cursor else { continue };
                // Direct cg-projection decision.
                let entries: Vec<crate::platform::cg::WindowEntry> =
                    f.windows.iter().map(|w| w.to_entry()).collect();
                let bundle_ids = vec![None; entries.len()];
                let direct = decide(
                    hover_snapshots(&entries, &bundle_ids, self_pid),
                    cursor,
                    self_pid,
                );
                // Registry-fed decision.
                let reg = WindowRegistry::new();
                let rows: Vec<(u32, f64, f64, f64, f64, i64, f64, i32, String)> = f
                    .windows
                    .iter()
                    .filter_map(|w| {
                        let wid = u32::try_from(w.number).ok()?;
                        Some((
                            wid,
                            w.x,
                            w.y,
                            w.w,
                            w.h,
                            w.layer,
                            w.alpha,
                            i32::try_from(w.owner_pid).unwrap_or(-1),
                            w.name.clone(),
                        ))
                    })
                    .collect();
                reg.ingest_rows(&rows, self_pid as i32, &Chrome);
                let snap = reg.snapshot();
                let snaps: Vec<super::platform::HoverWindowSnapshot> = snap
                    .records_front_to_back()
                    .map(|r| super::platform::HoverWindowSnapshot {
                        number: r.wid as i64,
                        layer: r.layer,
                        owner_pid: r.owner_pid as i64,
                        owner_bundle_id: None,
                        decorative: matches!(r.class, WindowClass::PetalOwned { decorative: true }),
                        region_selector: matches!(r.class, WindowClass::RegionSelector),
                        x: r.rx,
                        y: r.ry,
                        w: r.rw,
                        h: r.rh,
                    })
                    .collect();
                let via_registry = decide(snaps, cursor, self_pid);
                // Negative window numbers (BlockedByExternalWindow synthetic
                // case) are absent from the registry by construction; the direct
                // path may hit them first. Only compare when neither hinges on a
                // negative id -- i.e. when the direct decision is not the
                // synthetic BlockedByExternalWindow.
                if direct == "BlockedByExternalWindow" {
                    continue;
                }
                assert_eq!(
                    via_registry, direct,
                    "hover decision diverges for {fixture_name} at cursor {cursor:?}"
                );
            }
        }
    }

    fn hover_hit_golden_one(fixture_name: &str) {
        let fixture = crate::window_fixtures::load(
            &crate::window_fixtures::fixtures_dir().join(format!("{fixture_name}.jsonl")),
        );
        assert!(fixture.len() >= 10, "fixture {fixture_name} too short");
        // The recorder ran inside Petal: its pid owns the panel-titled windows.
        let self_pid = fixture
            .iter()
            .flat_map(|f| f.windows.iter())
            .find(|w| w.owner_name == "desktop" || w.owner_name.contains("Petal"))
            .map(|w| w.owner_pid)
            .expect("fixture must contain the recording app's own windows");

        #[derive(serde::Serialize)]
        struct HitDecision {
            t_ms: u64,
            cursor: Option<(f64, f64)>,
            decision: String,
        }
        let decisions = fixture
            .iter()
            .map(|f| {
                let entries: Vec<crate::platform::cg::WindowEntry> =
                    f.windows.iter().map(|w| w.to_entry()).collect();
                let bundle_ids: Vec<Option<String>> = vec![None; entries.len()];
                let decision = match f.cursor {
                    None => "NoCursor".to_string(),
                    Some(cursor) => {
                        let snaps = hover_snapshots(&entries, &bundle_ids, self_pid);
                        match super::platform::hover_hit_test_decision(snaps, cursor, self_pid) {
                            super::platform::HoverHitTestDecision::NoHit => "NoHit".into(),
                            super::platform::HoverHitTestDecision::BlockedByOwnProcess => {
                                "BlockedByOwnProcess".into()
                            }
                            super::platform::HoverHitTestDecision::BlockedByExternalWindow => {
                                "BlockedByExternalWindow".into()
                            }
                            super::platform::HoverHitTestDecision::ShareableCandidate {
                                window_id,
                                frame,
                            } => format!(
                                "Shareable({window_id} @ {},{},{}x{})",
                                frame.x, frame.y, frame.width, frame.height
                            ),
                        }
                    }
                };
                HitDecision {
                    t_ms: f.t_ms,
                    cursor: f.cursor,
                    decision,
                }
            })
            .collect::<Vec<_>>();
        crate::window_fixtures::assert_golden(&format!("hover-hit.{fixture_name}"), &decisions);
    }

    // --- hover_snapshots projection (#742 characterization) ---

    fn cg_entry(number: i64, owner_pid: i64, name: &str) -> crate::platform::cg::WindowEntry {
        crate::platform::cg::WindowEntry {
            number,
            owner_pid,
            owner_name: "Owner".to_string(),
            name: name.to_string(),
            layer: 0,
            alpha: 1.0,
            x: 10.5,
            y: 20.5,
            w: 300.5,
            h: 400.5,
        }
    }

    /// Unlike share_border's projection, hover_tab keeps the FULL f64
    /// precision (no truncation) and carries layer/alpha-relevant fields
    /// through for `hover_hit_test_decision` to filter. Pinned so a shared
    /// registry record cannot quietly normalise one to the other.
    #[test]
    fn hover_snapshots_preserve_full_precision_and_fields() {
        let windows = vec![cg_entry(7, 100, "Doc.txt")];
        let bundle_ids = vec![Some("com.example.app".to_string())];
        let snaps = hover_snapshots(&windows, &bundle_ids, 999);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].number, 7);
        assert_eq!(snaps[0].owner_pid, 100);
        assert!(!snaps[0].decorative, "foreign window is not own-decorative");
        assert_eq!(snaps[0].owner_bundle_id, Some("com.example.app"));
        assert_eq!(snaps[0].x, 10.5, "no truncation on the hover path");
        assert_eq!(snaps[0].h, 400.5);
    }

    /// Bundle ids are paired POSITIONALLY with windows. This is the invariant a
    /// registry most easily breaks (e.g. by filtering windows but not ids).
    #[test]
    fn hover_snapshots_pair_bundle_ids_by_index() {
        let windows = vec![cg_entry(1, 10, "a"), cg_entry(2, 20, "b")];
        let bundle_ids = vec![None, Some("com.second.app".to_string())];
        let snaps = hover_snapshots(&windows, &bundle_ids, 999);
        assert_eq!(snaps[0].owner_bundle_id, None);
        assert_eq!(snaps[1].owner_bundle_id, Some("com.second.app"));
    }

    /// `zip` stops at the shorter list: a truncated id list silently DROPS
    /// windows from the hit-test rather than erroring, so a window would stop
    /// being hoverable. Pinned as current behaviour.
    #[test]
    fn hover_snapshots_zip_drops_windows_when_bundle_ids_are_short() {
        let windows = vec![cg_entry(1, 10, "a"), cg_entry(2, 20, "b")];
        let snaps = hover_snapshots(&windows, &[None], 999);
        assert_eq!(
            snaps.len(),
            1,
            "positional zip truncates; a registry must keep the lists in lockstep"
        );
    }

    fn snapshot_with_bundle_id(
        number: i64,
        owner_pid: i64,
        owner_bundle_id: &'static str,
        name: &'static str,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> HoverWindowSnapshot<'static> {
        HoverWindowSnapshot {
            owner_bundle_id: Some(owner_bundle_id),
            ..snapshot(number, owner_pid, name, x, y, w, h)
        }
    }

    #[test]
    fn own_process_window_blocks_hover_hit_testing_instead_of_falling_through() {
        let self_pid = 42;
        let decision = hover_hit_test_decision(
            [
                snapshot(501, self_pid, "Remote Window", 10.0, 20.0, 500.0, 300.0),
                snapshot(123, 99, "Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (40.0, 50.0),
            self_pid,
        );

        assert_eq!(decision, HoverHitTestDecision::BlockedByOwnProcess);
    }

    #[test]
    fn external_window_can_still_become_hover_candidate() {
        let decision = hover_hit_test_decision(
            [snapshot(123, 99, "Source", 10.0, 20.0, 500.0, 300.0)],
            (40.0, 50.0),
            42,
        );

        assert_eq!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 123,
                frame: WindowFrame {
                    x: 10,
                    y: 20,
                    width: 500,
                    height: 300,
                },
            }
        );
    }

    #[test]
    fn own_share_border_panel_is_transparent_to_hover_hit_testing() {
        let self_pid = 42;
        let decision = hover_hit_test_decision(
            [
                snapshot(
                    900,
                    self_pid,
                    crate::share_border::SHARE_BORDER_WINDOW_TITLE,
                    10.0,
                    20.0,
                    500.0,
                    300.0,
                ),
                snapshot(123, 99, "Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (40.0, 50.0),
            self_pid,
        );

        assert_eq!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 123,
                frame: WindowFrame {
                    x: 10,
                    y: 20,
                    width: 500,
                    height: 300,
                },
            }
        );
    }

    #[test]
    fn own_share_overlay_panel_is_transparent_to_hover_hit_testing() {
        let self_pid = 42;
        let decision = hover_hit_test_decision(
            [
                snapshot(
                    900,
                    self_pid,
                    crate::share_overlay::SHARE_OVERLAY_WINDOW_TITLE,
                    10.0,
                    20.0,
                    500.0,
                    300.0,
                ),
                snapshot(123, 99, "Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (40.0, 50.0),
            self_pid,
        );

        assert_eq!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 123,
                frame: WindowFrame {
                    x: 10,
                    y: 20,
                    width: 500,
                    height: 300,
                },
            }
        );
    }

    #[test]
    fn own_hover_tab_panel_is_transparent_to_hover_hit_testing() {
        let self_pid = 42;
        let decision = hover_hit_test_decision(
            [
                snapshot(
                    900,
                    self_pid,
                    super::HOVER_TAB_WINDOW_TITLE,
                    10.0,
                    20.0,
                    500.0,
                    300.0,
                ),
                snapshot(123, 99, "Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (40.0, 50.0),
            self_pid,
        );

        assert_eq!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 123,
                frame: WindowFrame {
                    x: 10,
                    y: 20,
                    width: 500,
                    height: 300,
                },
            }
        );
    }

    #[test]
    fn denylisted_overlay_is_transparent_to_hover_hit_testing() {
        let decision = hover_hit_test_decision(
            [
                snapshot_with_bundle_id(
                    900,
                    88,
                    "com.apple.controlcenter",
                    "Control Center",
                    10.0,
                    20.0,
                    500.0,
                    300.0,
                ),
                snapshot(123, 99, "Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (40.0, 50.0),
            42,
        );

        assert_eq!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 123,
                frame: WindowFrame {
                    x: 10,
                    y: 20,
                    width: 500,
                    height: 300,
                },
            }
        );
    }

    #[test]
    fn lone_denylisted_overlay_returns_no_hover_hit() {
        let decision = hover_hit_test_decision(
            [snapshot_with_bundle_id(
                900,
                88,
                "com.apple.WindowManager",
                "Stage Manager",
                10.0,
                20.0,
                500.0,
                300.0,
            )],
            (40.0, 50.0),
            42,
        );

        assert_eq!(decision, HoverHitTestDecision::NoHit);
    }

    #[test]
    fn covered_shared_window_does_not_fall_through_to_hidden_source() {
        let decision = hover_hit_test_decision(
            [
                snapshot(301, 88, "Covering Window", 40.0, 50.0, 320.0, 220.0),
                snapshot(200, 99, "Shared Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (80.0, 90.0),
            42,
        );

        assert_ne!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 200,
                frame: WindowFrame {
                    x: 10,
                    y: 20,
                    width: 500,
                    height: 300,
                },
            }
        );
        assert_eq!(
            decision,
            HoverHitTestDecision::ShareableCandidate {
                window_id: 301,
                frame: WindowFrame {
                    x: 40,
                    y: 50,
                    width: 320,
                    height: 220,
                },
            }
        );
    }

    #[test]
    fn unidentifiable_frontmost_window_blocks_hidden_source_hover_tab() {
        let decision = hover_hit_test_decision(
            [
                snapshot(-1, 88, "Covering Window", 40.0, 50.0, 320.0, 220.0),
                snapshot(200, 99, "Shared Source", 10.0, 20.0, 500.0, 300.0),
            ],
            (80.0, 90.0),
            42,
        );

        assert_eq!(decision, HoverHitTestDecision::BlockedByExternalWindow);
    }

    #[test]
    fn share_color_validation_accepts_only_hex_triplets() {
        assert_eq!(
            super::normalize_share_color(" #6e8bff "),
            Some("#6e8bff".to_string())
        );
        assert_eq!(
            super::normalize_share_color("#6E8BFF"),
            Some("#6E8BFF".to_string())
        );
        assert_eq!(super::normalize_share_color("plum"), None);
        assert_eq!(super::normalize_share_color("#12345678"), None);
    }

    #[test]
    fn share_start_refocus_targets_only_external_source_apps() {
        assert_eq!(super::shared_source_refocus_pid(Some(99), 42), Some(99));
        assert_eq!(super::shared_source_refocus_pid(Some(42), 42), None);
        assert_eq!(super::shared_source_refocus_pid(Some(0), 42), None);
        assert_eq!(super::shared_source_refocus_pid(Some(-1), 42), None);
        assert_eq!(super::shared_source_refocus_pid(None, 42), None);
    }

    #[test]
    fn direct_and_system_picker_selections_share_one_latest_focus_generation() {
        let mut attempts = super::ShareFocusAttempts::new();
        let direct = attempts.begin(101, Some(700), Some(700), 1, std::time::Instant::now());
        let picker = attempts.begin(202, Some(800), Some(800), 2, std::time::Instant::now());

        assert!(!attempts.is_current(direct));
        assert!(attempts.is_current(picker));
        assert_eq!(picker.source_pid, Some(800));
    }

    #[test]
    fn direct_and_system_picker_adapters_hand_back_before_delayed_start_completes() {
        let mut adapter = FocusStartAdapter::default();

        let direct = adapter.begin_direct(101, Some(700), true);
        assert_eq!(
            adapter.events,
            vec![
                FocusStartEvent::Handback(
                    FocusStartEntry::Direct,
                    super::FocusHandoffAction::ActivateSource(700)
                ),
                FocusStartEvent::StartPending(FocusStartEntry::Direct),
            ]
        );
        assert!(adapter.attempts.is_current(direct.attempt));

        // The test controls when the media future resolves. Focus has already
        // been dispatched while direct start remains pending.
        let picker = adapter.begin_system_picker(202, Some(800), true);
        assert_eq!(
            &adapter.events[2..],
            &[
                FocusStartEvent::Handback(
                    FocusStartEntry::SystemPicker,
                    super::FocusHandoffAction::ActivateSource(800)
                ),
                FocusStartEvent::StartPending(FocusStartEntry::SystemPicker),
            ]
        );
        assert!(!adapter.attempts.is_current(direct.attempt));
        assert!(adapter.attempts.is_current(picker.attempt));

        adapter.complete_start(picker, Ok(()));
        assert_eq!(
            adapter.events.last(),
            Some(&FocusStartEvent::StartCompleted(
                FocusStartEntry::SystemPicker,
                true
            ))
        );
    }

    #[test]
    fn inactive_petal_suppresses_both_entry_adapter_handbacks_without_blocking_start() {
        let mut adapter = FocusStartAdapter::default();

        let direct = adapter.begin_direct(101, Some(700), false);
        let picker = adapter.begin_system_picker(202, Some(800), false);
        assert_eq!(
            adapter.events,
            vec![
                FocusStartEvent::Handback(FocusStartEntry::Direct, super::FocusHandoffAction::None),
                FocusStartEvent::StartPending(FocusStartEntry::Direct),
                FocusStartEvent::Handback(
                    FocusStartEntry::SystemPicker,
                    super::FocusHandoffAction::None
                ),
                FocusStartEvent::StartPending(FocusStartEntry::SystemPicker),
            ]
        );

        adapter.complete_start(direct, Ok(()));
        adapter.complete_start(picker, Ok(()));
        assert!(adapter.attempts.is_current(picker.attempt));
    }

    #[test]
    fn focus_revalidation_rejects_changed_owner_before_handoff() {
        let mut adapter = FocusStartAdapter::default();
        // Owner pid no longer matches the selection → no handback.
        let owner_changed =
            adapter.begin_with_revalidation(FocusStartEntry::Direct, 101, Some(700), true, false);
        // #677: a frontmost change to Petal is NOT a reject — that is the
        // steal case. Owner still matches → handback runs.
        let steal_after_snapshot = adapter.begin_with_revalidation(
            FocusStartEntry::SystemPicker,
            202,
            Some(800),
            true,
            true,
        );

        assert_eq!(
            adapter.events,
            vec![
                FocusStartEvent::Handback(FocusStartEntry::Direct, super::FocusHandoffAction::None),
                FocusStartEvent::StartPending(FocusStartEntry::Direct),
                FocusStartEvent::Handback(
                    FocusStartEntry::SystemPicker,
                    super::FocusHandoffAction::ActivateSource(800)
                ),
                FocusStartEvent::StartPending(FocusStartEntry::SystemPicker),
            ]
        );
        // Revalidation suppresses only foreground handback; the media start
        // is still allowed to complete and the newer attempt stays current.
        adapter.complete_start(owner_changed, Ok(()));
        adapter.complete_start(steal_after_snapshot, Ok(()));
        assert!(adapter.attempts.is_current(steal_after_snapshot.attempt));
    }

    #[test]
    fn failed_start_clear_leave_and_unshare_invalidate_the_command_path_attempt() {
        let mut adapter = FocusStartAdapter::default();

        let failed = adapter.begin_direct(101, Some(700), true);
        adapter.complete_start(failed, Err(()));
        assert!(!adapter.attempts.is_current(failed.attempt));

        let unshared = adapter.begin_system_picker(202, Some(800), true);
        adapter.unshare(202);
        assert!(!adapter.attempts.is_current(unshared.attempt));

        let cleared = adapter.begin_direct(303, Some(900), true);
        adapter.clear_window(303);
        assert!(!adapter.attempts.is_current(cleared.attempt));

        let leaving = adapter.begin_system_picker(404, Some(1_000), true);
        adapter.leave_room();
        assert!(!adapter.attempts.is_current(leaving.attempt));
    }

    #[test]
    fn superseded_direct_start_cannot_invalidate_pending_system_picker_start() {
        let mut adapter = FocusStartAdapter::default();
        let direct = adapter.begin_direct(101, Some(700), true);
        let picker = adapter.begin_system_picker(202, Some(800), true);

        // A fails only after B has been selected. The failure lifecycle wiring
        // must not erase B's global current token.
        adapter.complete_start(direct, Err(()));
        assert!(adapter.attempts.is_current(picker.attempt));
        adapter.complete_start(picker, Ok(()));
        assert!(adapter.attempts.is_current(picker.attempt));
    }

    #[test]
    fn focus_same_window_delayed_direct_failure_cannot_invalidate_newer_picker_attempt() {
        let mut adapter = FocusStartAdapter::default();
        let direct = adapter.begin_direct(101, Some(700), true);
        let picker = adapter.begin_system_picker(101, Some(700), true);
        assert_ne!(direct.attempt.generation, picker.attempt.generation);
        assert!(adapter.attempts.is_current(picker.attempt));

        // A's media start resolves after the newer picker selection for the
        // same window. Its failure is generation-scoped, so only A could be
        // invalidated; B must remain eligible for its handback and completion.
        adapter.complete_start(direct, Err(()));
        assert!(adapter.attempts.is_current(picker.attempt));
        adapter.complete_start(picker, Ok(()));
        assert!(adapter.attempts.is_current(picker.attempt));
    }

    #[test]
    fn focus_attempt_invalidation_does_not_cancel_a_newer_window_selection() {
        let mut attempts = super::ShareFocusAttempts::new();
        let old = attempts.begin(101, Some(700), Some(700), 1, std::time::Instant::now());
        let current = attempts.begin(202, Some(800), Some(800), 2, std::time::Instant::now());

        attempts.invalidate_window(old.window_id);
        assert!(attempts.is_current(current));
        attempts.invalidate_window(current.window_id);
        assert!(!attempts.is_current(current));
    }

    #[test]
    fn focus_handoff_runs_only_for_the_current_immediate_active_selection() {
        // current, petal_active, owner_matches, cockpit, still_immediate, source_pid
        assert_eq!(
            super::focus_handoff_action(true, true, true, false, true, Some(700)),
            super::FocusHandoffAction::ActivateSource(700)
        );
        assert_eq!(
            super::focus_handoff_action(true, true, true, false, true, None),
            super::FocusHandoffAction::YieldForeground(None)
        );
        // A newer selection, delayed dispatch, cockpit sampling, or Petal not
        // active all suppress the callback instead of stealing focus.
        assert_eq!(
            super::focus_handoff_action(false, true, true, false, true, Some(700)),
            super::FocusHandoffAction::None
        );
        assert_eq!(
            super::focus_handoff_action(true, true, true, false, false, Some(700)),
            super::FocusHandoffAction::None
        );
        assert_eq!(
            super::focus_handoff_action(true, false, true, false, true, Some(700)),
            super::FocusHandoffAction::None
        );
        assert_eq!(
            super::focus_handoff_action(true, true, true, true, true, Some(700)),
            super::FocusHandoffAction::None
        );
    }

    #[test]
    fn focus_handoff_recovers_when_petal_stole_after_selection_snapshot() {
        // #677: selection snapshot was Sublime; Petal later became active.
        // The old frontmost_matches gate returned None here and left Petal
        // in the foreground permanently. petal_active alone is enough.
        assert_eq!(
            super::focus_handoff_action(true, true, true, false, true, Some(700)),
            super::FocusHandoffAction::ActivateSource(700)
        );
    }

    #[cfg(feature = "cockpit-privileged")]
    #[test]
    fn cockpit_visibility_handback_is_off_without_a_registered_source() {
        assert!(!super::keep_cockpit_source_visible(u32::MAX));
    }

    #[test]
    fn optimistic_share_border_is_recorded_before_backend_share_is_active() {
        let mut state = super::ShareState::new();
        let mut show_calls = 0;

        let result = state.ensure_border(42, || {
            show_calls += 1;
            7
        });

        assert_eq!(result, super::EnsureBorderResult::Created(7));
        assert_eq!(show_calls, 1);
        assert_eq!(state.borders.get(&42), Some(&7));
    }

    #[test]
    fn failed_share_start_removes_the_optimistic_border_for_hide() {
        let mut state = super::ShareState::new();
        state.ensure_border(42, || 7);

        let border_to_hide = state.remove_border(42);

        assert_eq!(border_to_hide, Some(7));
        assert!(!state.borders.contains_key(&42));
    }

    #[test]
    fn stop_state_transition_is_explicitly_unshared() {
        let payload = serde_json::to_value(super::ShareStateChanged {
            window_id: 42,
            shared: false,
        })
        .expect("share-state event serializes");

        assert_eq!(payload["windowId"], 42);
        assert_eq!(payload["shared"], false);
    }

    #[test]
    fn repeated_optimistic_share_start_reuses_existing_border_bookkeeping() {
        let mut state = super::ShareState::new();
        assert_eq!(
            state.ensure_border(42, || 7),
            super::EnsureBorderResult::Created(7)
        );

        let mut second_show_called = false;
        let result = state.ensure_border(42, || {
            second_show_called = true;
            8
        });

        assert_eq!(result, super::EnsureBorderResult::Existing(7));
        assert!(!second_show_called);
        assert_eq!(state.borders.get(&42), Some(&7));
    }

    #[test]
    fn system_picker_share_border_waits_for_publish() {
        // issue #249: system-picker shares must not show the same "live"
        // border as direct hover-tab shares until capture+publish succeeds.
        assert_eq!(
            super::share_border_start_timing(super::ShareStartSurface::HoverTab),
            super::ShareBorderStartTiming::Optimistic
        );
        assert_eq!(
            super::share_border_start_timing(super::ShareStartSurface::SystemPicker),
            super::ShareBorderStartTiming::AfterPublish
        );
    }

    fn stack_entry(number: i64, owner_pid: i64) -> HoverStackEntry {
        HoverStackEntry { number, owner_pid }
    }

    #[test]
    fn hover_tab_order_is_satisfied_when_directly_above_target() {
        let self_pid = 42;
        let tab = 700;
        let target = 123;
        let stack = [
            stack_entry(900, 88),
            stack_entry(tab, self_pid),
            stack_entry(target, 99),
            stack_entry(800, 77),
        ];

        assert!(!hover_tab_needs_reorder(&stack, tab, target, self_pid));
    }

    #[test]
    fn hover_tab_order_ignores_other_petal_windows_between_tab_and_target() {
        let self_pid = 42;
        let tab = 700;
        let target = 123;
        let stack = [
            stack_entry(tab, self_pid),
            stack_entry(701, self_pid),
            stack_entry(target, 99),
        ];

        assert!(!hover_tab_needs_reorder(&stack, tab, target, self_pid));
    }

    #[test]
    fn hover_tab_reorders_when_external_window_sits_between_tab_and_target() {
        let self_pid = 42;
        let tab = 700;
        let target = 123;
        let stack = [
            stack_entry(tab, self_pid),
            stack_entry(900, 88),
            stack_entry(target, 99),
        ];

        assert!(hover_tab_needs_reorder(&stack, tab, target, self_pid));
    }

    #[test]
    fn hover_tab_reorders_when_tab_is_missing_from_stack() {
        let self_pid = 42;
        let tab = 700;
        let target = 123;
        let stack = [stack_entry(target, 99)];

        assert!(hover_tab_needs_reorder(&stack, tab, target, self_pid));
    }

    #[test]
    fn hover_tab_does_not_reorder_when_hover_target_is_missing() {
        let self_pid = 42;
        let tab = 700;
        let target = 123;
        let stack = [stack_entry(tab, self_pid), stack_entry(900, 88)];

        assert!(!hover_tab_needs_reorder(&stack, tab, target, self_pid));
    }

    struct RecordingGeometry {
        calls: std::cell::RefCell<Vec<&'static str>>,
    }

    impl RecordingGeometry {
        fn new() -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl super::HoverTabGeometryHandle for RecordingGeometry {
        fn resize(&self, _width: f64, _height: f64) -> Result<(), String> {
            self.calls.borrow_mut().push("resize");
            Ok(())
        }

        fn move_to(&self, _x: f64, _y: f64) -> Result<(), String> {
            self.calls.borrow_mut().push("move");
            Ok(())
        }
    }

    #[test]
    fn native_geometry_seam_resizes_before_positioning() {
        let geometry = RecordingGeometry::new();
        super::apply_geometry(
            &geometry,
            super::HoverTabRect {
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 40.0,
            },
        )
        .unwrap();
        assert_eq!(*geometry.calls.borrow(), vec!["resize", "move"]);
    }

    // -- #680 Rank 3: position_tab's `w.show()` suppression ------------------
    //
    // CLAUDE.md's "Native window-lifecycle changes need a live-exercising
    // test" section is explicit that a unit test on an extracted pure
    // function proves nothing about whether the real event chain actually
    // calls it with the right inputs -- the #497 showstopper shipped green
    // because 810 passing tests only ever exercised isolated helpers, never
    // the real `WindowEvent::Resized` handler that wired them up.
    //
    // `tauri::test`'s `MockRuntime` can't be used to close that gap here:
    // its `MockWindowDispatcher::is_visible` hardcodes `Ok(true)` and its
    // `show`/`hide` are no-ops that track no state at all, so a mock
    // `tauri::WebviewWindow` can never observe or assert a real show() call
    // count. `FakeHoverTabPanel` below is the seam that makes this
    // observable: `show_hover_tab_panel_if_hidden` in production code is the
    // exact function this test drives -- not a reimplemented copy of its
    // `if` -- generically over `HoverTabPanelHandle`, which
    // `tauri::WebviewWindow` implements as a pure one-line passthrough
    // directly beneath it. So this test exercises production's real
    // conditional, with the real call sequence, via the one substitution
    // point genuinely needed to make "was show() called" observable at all.
    struct FakeHoverTabPanel {
        visible: std::cell::Cell<bool>,
        show_calls: std::cell::Cell<u32>,
    }

    impl FakeHoverTabPanel {
        fn new(initially_visible: bool) -> Self {
            Self {
                visible: std::cell::Cell::new(initially_visible),
                show_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl super::HoverTabPanelHandle for FakeHoverTabPanel {
        fn hover_tab_panel_is_visible(&self) -> bool {
            self.visible.get()
        }

        fn hover_tab_panel_show(&self) {
            self.show_calls.set(self.show_calls.get() + 1);
            self.visible.set(true);
        }
    }

    #[test]
    fn position_tab_show_gate_skips_show_when_already_visible() {
        // Case (a): unchanged geometry, already visible -> show() NOT called.
        let panel = FakeHoverTabPanel::new(true);

        super::show_hover_tab_panel_if_hidden(&panel);

        assert_eq!(
            panel.show_calls.get(),
            0,
            "a visible hover-tab panel must not take a redundant show() on a same-hit re-run \
             (#680 Rank 3 -- this is exactly the ~55Hz cursor-driven churn that queued main-thread \
             work ahead of the second-share panel build)"
        );
        assert!(panel.hover_tab_panel_is_visible());
    }

    #[test]
    fn position_tab_show_gate_shows_exactly_once_on_hidden_to_visible_transition() {
        // Case (b): hidden -> visible transition -> show() called exactly once.
        let panel = FakeHoverTabPanel::new(false);

        super::show_hover_tab_panel_if_hidden(&panel);

        assert_eq!(
            panel.show_calls.get(),
            1,
            "a hidden hover-tab panel must still be shown on the real transition that needs it"
        );
        assert!(panel.hover_tab_panel_is_visible());

        // A second call with no intervening hide (the steady-state, same-hit
        // case) must not show() again -- proves the gate re-reads real
        // visibility rather than latching a one-shot flag.
        super::show_hover_tab_panel_if_hidden(&panel);

        assert_eq!(panel.show_calls.get(), 1);
    }
}
