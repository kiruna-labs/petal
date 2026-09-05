//! Click-through sharer-side overlay for remote strokes and telepointers.
//!
//! Receiver compositor windows already host `compositor/pointer.html` above
//! the remote video content. The local sharer did not have an equivalent
//! render surface over their real app window, so owner-targeted draw packets
//! were received and then dropped. This module provides that missing surface:
//! a transparent, non-activating NSPanel that tracks the shared source window
//! using the same hide-retire-reuse lifecycle as `share_border.rs`.
//!
//! The panel is deliberately generic: it hosts the existing pointer route,
//! which already renders both telepointers and draw strokes. That keeps the
//! #196 draw fix and the historical telepointer-on-own-window gap on the same
//! native surface.

use crate::platform::cg::WindowFrame;
use crate::sync_ext::MutexExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::AppHandle;

static OVERLAYS: Mutex<Option<Registry>> = Mutex::new(None);
static NEXT_OVERLAY_ID: AtomicU32 = AtomicU32::new(1);

#[cfg(target_os = "macos")]
const PREWARM_PANEL_FRAME: WindowFrame = WindowFrame {
    x: -10_000,
    y: -10_000,
    width: 1,
    height: 1,
};
pub(crate) const SHARE_OVERLAY_WINDOW_TITLE: &str = "Share Overlay";

#[derive(Debug, Clone, Copy, PartialEq)]
struct ShareOverlayLayout {
    panel: WindowFrame,
    content_width: f64,
    content_height: f64,
}

fn share_overlay_content_frame(source_frame: WindowFrame) -> WindowFrame {
    // The sharer's overlay has no Petal compositor header strip. Size it to
    // the same source rectangle used by local cursor hit-testing and capture
    // metadata so the reused pointer route maps 0..1 over the shared surface.
    WindowFrame {
        x: source_frame.x,
        y: source_frame.y,
        width: source_frame.width.max(1),
        height: source_frame.height.max(1),
    }
}

fn share_overlay_layout(source_frame: WindowFrame) -> ShareOverlayLayout {
    let panel = share_overlay_content_frame(source_frame);
    ShareOverlayLayout {
        panel,
        content_width: panel.width as f64,
        content_height: panel.height as f64,
    }
}

#[cfg(test)]
fn normalized_to_content_point(frame: WindowFrame, x: f64, y: f64) -> (f64, f64) {
    let content = share_overlay_content_frame(frame);
    (
        content.x as f64 + x.clamp(0.0, 1.0) * content.width as f64,
        content.y as f64 + y.clamp(0.0, 1.0) * content.height as f64,
    )
}

#[cfg(test)]
fn content_to_normalized_point(frame: WindowFrame, x: f64, y: f64) -> (f64, f64) {
    let content = share_overlay_content_frame(frame);
    (
        ((x - content.x as f64) / content.width as f64).clamp(0.0, 1.0),
        ((y - content.y as f64) / content.height as f64).clamp(0.0, 1.0),
    )
}

#[cfg(target_os = "macos")]
fn share_overlay_url(window_id: u32, owner_identity: &str, show_draw_toolbar: bool) -> String {
    let owner_identity =
        percent_encoding::utf8_percent_encode(owner_identity, percent_encoding::NON_ALPHANUMERIC);
    let draw_toolbar = if show_draw_toolbar { "1" } else { "0" };
    format!(
        "compositor/pointer.html?windowId={window_id}&surface=sharer&drawToolbar={draw_toolbar}&ownerIdentity={owner_identity}"
    )
}

#[derive(Default)]
struct Registry {
    active: HashMap<u32, OverlayHandle>,
    retired: Vec<OverlayHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegistryCounts {
    live: usize,
    pending: usize,
    retired: usize,
}

impl std::fmt::Display for RegistryCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "live={} pending={} retired={}",
            self.live, self.pending, self.retired
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShowKind {
    UpdateExisting,
    ReuseRetired,
    CreateFresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShowReservation {
    overlay_id: u32,
    kind: ShowKind,
    counts: RegistryCounts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HideKind {
    Hide,
    AlreadyPending,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HideReservation {
    kind: HideKind,
    label: Option<String>,
    counts: RegistryCounts,
}

impl Registry {
    fn counts(&self) -> RegistryCounts {
        RegistryCounts {
            live: self.active.values().filter(|h| h.realized).count(),
            pending: self.active.values().filter(|h| !h.realized).count(),
            retired: self.retired.len(),
        }
    }

    #[cfg(test)]
    fn active_count_for_window(&self, owner_identity: &str, window_id: u32) -> usize {
        self.active
            .values()
            .filter(|h| {
                h.owner_identity == owner_identity && h.window_id == window_id && !h.hide_requested
            })
            .count()
    }

    fn reserve_show(
        &mut self,
        owner_identity: String,
        window_id: u32,
        frame: WindowFrame,
        show_draw_toolbar: bool,
        next_overlay_id: impl FnOnce() -> u32,
    ) -> ShowReservation {
        if let Some(overlay_id) = self.active.iter().find_map(|(&id, h)| {
            (h.owner_identity == owner_identity && h.window_id == window_id).then_some(id)
        }) {
            if let Some(handle) = self.active.get_mut(&overlay_id) {
                handle.frame = frame;
                handle.show_draw_toolbar = show_draw_toolbar;
                handle.tracker_hidden = false;
                handle.hide_requested = false;
            }
            return ShowReservation {
                overlay_id,
                kind: ShowKind::UpdateExisting,
                counts: self.counts(),
            };
        }

        let overlay_id = next_overlay_id();
        if let Some(mut handle) = self.retired.pop() {
            handle.owner_identity = owner_identity;
            handle.window_id = window_id;
            handle.frame = frame;
            handle.show_draw_toolbar = show_draw_toolbar;
            handle.panel_number = 0;
            handle.tracker_hidden = false;
            handle.hide_requested = false;
            handle.draw_active = false;
            handle.realized = true;
            self.active.insert(overlay_id, handle);
            return ShowReservation {
                overlay_id,
                kind: ShowKind::ReuseRetired,
                counts: self.counts(),
            };
        }

        self.active.insert(
            overlay_id,
            OverlayHandle {
                label: overlay_label(overlay_id),
                owner_identity,
                window_id,
                frame,
                panel_number: 0,
                tracker_hidden: false,
                realized: false,
                hide_requested: false,
                draw_active: false,
                show_draw_toolbar,
            },
        );
        ShowReservation {
            overlay_id,
            kind: ShowKind::CreateFresh,
            counts: self.counts(),
        }
    }

    fn request_hide(&mut self, overlay_id: u32) -> HideReservation {
        let Some(handle) = self.active.get_mut(&overlay_id) else {
            return HideReservation {
                kind: HideKind::Unknown,
                label: None,
                counts: self.counts(),
            };
        };

        let label = Some(handle.label.clone());
        let kind = if handle.hide_requested {
            HideKind::AlreadyPending
        } else {
            handle.hide_requested = true;
            HideKind::Hide
        };
        HideReservation {
            kind,
            label,
            counts: self.counts(),
        }
    }

    fn take_hide_requested(&mut self, overlay_id: u32) -> Option<OverlayHandle> {
        if self
            .active
            .get(&overlay_id)
            .is_some_and(|h| h.hide_requested)
        {
            let mut handle = self.active.remove(&overlay_id)?;
            handle.draw_active = false;
            Some(handle)
        } else {
            None
        }
    }

    fn mark_show_dispatch_failed(&mut self, overlay_id: u32) -> RegistryCounts {
        if self.active.get(&overlay_id).is_some_and(|h| !h.realized) {
            self.active.remove(&overlay_id);
        }
        self.counts()
    }

    fn undo_hide_request(&mut self, overlay_id: u32) -> RegistryCounts {
        if let Some(handle) = self.active.get_mut(&overlay_id) {
            handle.hide_requested = false;
        }
        self.counts()
    }
}

#[derive(Clone)]
struct OverlayHandle {
    label: String,
    owner_identity: String,
    window_id: u32,
    frame: WindowFrame,
    panel_number: i64,
    tracker_hidden: bool,
    realized: bool,
    hide_requested: bool,
    draw_active: bool,
    show_draw_toolbar: bool,
}

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = OVERLAYS.lock_unpoisoned();
    let reg = guard.get_or_insert_with(Registry::default);
    f(reg)
}

fn overlay_label(overlay_id: u32) -> String {
    format!("share_overlay_{overlay_id}")
}

pub(crate) fn overlay_label_for_window(window_id: u32, owner_identity: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        // `draw.rs` has already authenticated that this is the local owner;
        // Windows' share overlay registry is keyed by the local share token.
        let _ = owner_identity;
        return crate::windows_share_overlay::labels_for_local_share(window_id)
            .into_iter()
            .next();
    }

    #[cfg(not(target_os = "windows"))]
    with_registry(|reg| {
        reg.active
            .values()
            .find(|h| {
                h.window_id == window_id
                    && h.owner_identity == owner_identity
                    && h.realized
                    && !h.hide_requested
            })
            .map(|h| h.label.clone())
    })
}

pub(crate) fn overlay_labels_for_window(window_id: u32) -> Vec<String> {
    with_registry(|reg| {
        reg.active
            .values()
            .filter(|h| h.window_id == window_id && h.realized && !h.hide_requested)
            .map(|h| h.label.clone())
            .collect()
    })
}

/// Overlay ids currently registered for `window_id`, from this module's own
/// registry. #872: hover-tab bookkeeping can disagree with this authority.
pub(crate) fn overlay_ids_for_window(window_id: u32) -> Vec<u32> {
    with_registry(|reg| {
        reg.active
            .iter()
            .filter_map(|(&id, handle)| (handle.window_id == window_id).then_some(id))
            .collect()
    })
}

/// Hide + retire every overlay registered for `window_id`, independently of
/// hover-tab bookkeeping. Safe when no overlays are registered.
pub(crate) fn retire_overlays_for_window(app: &AppHandle, window_id: u32) {
    #[cfg(target_os = "macos")]
    for overlay_id in overlay_ids_for_window(window_id) {
        hide_share_overlay(app, overlay_id);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, window_id);
}

/// Hide + retire every active overlay. Safe when no overlays are registered.
pub(crate) fn retire_all_overlays(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let overlay_ids = with_registry(|reg| reg.active.keys().copied().collect::<Vec<_>>());
        for overlay_id in overlay_ids {
            hide_share_overlay(app, overlay_id);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickCaptureClearReason {
    NoPublication,
    HidePending,
    Retired,
}

impl ClickCaptureClearReason {
    #[cfg(target_os = "macos")]
    fn as_str(self) -> &'static str {
        match self {
            Self::NoPublication => "no-publication",
            Self::HidePending => "hide-pending",
            Self::Retired => "retired",
        }
    }

    #[cfg(target_os = "macos")]
    fn diagnostic_tag(self) -> crate::logging::OverlayClearReasonTag {
        match self {
            Self::NoPublication => crate::logging::OverlayClearReasonTag::NoPublication,
            Self::HidePending => crate::logging::OverlayClearReasonTag::HidePending,
            Self::Retired => crate::logging::OverlayClearReasonTag::Retired,
        }
    }
}

/// One overlay panel the watchdog must force back to click-through. `label` is
/// what identifies the real panel: a retired handle has no active overlay id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClickCaptureClear {
    pub(crate) overlay_id: Option<u32>,
    pub(crate) label: String,
    pub(crate) window_id: u32,
    pub(crate) reason: ClickCaptureClearReason,
}

/// Pure registry decision for cursor-capturing overlays that must fail safe.
/// An overlay capturing the cursor while the session holds no publication for
/// its window is ALWAYS a bug (#872) -- it swallows clicks over its whole
/// frame, including clicks meant for the user's other applications.
fn plan_click_capture_clear(
    reg: &Registry,
    is_share_active: &dyn Fn(u32) -> bool,
) -> Vec<ClickCaptureClear> {
    let mut planned = reg
        .active
        .iter()
        .filter_map(|(&overlay_id, handle)| {
            if !handle.draw_active {
                return None;
            }
            let reason = if handle.hide_requested {
                ClickCaptureClearReason::HidePending
            } else if !is_share_active(handle.window_id) {
                ClickCaptureClearReason::NoPublication
            } else {
                return None;
            };
            Some(ClickCaptureClear {
                overlay_id: Some(overlay_id),
                label: handle.label.clone(),
                window_id: handle.window_id,
                reason,
            })
        })
        .collect::<Vec<_>>();
    planned.extend(reg.retired.iter().filter(|h| h.draw_active).map(|handle| {
        ClickCaptureClear {
            overlay_id: None,
            label: handle.label.clone(),
            window_id: handle.window_id,
            reason: ClickCaptureClearReason::Retired,
        }
    }));
    planned.sort_by(|a, b| a.label.cmp(&b.label));
    planned
}

/// Panel labels already reported for the current orphan episode, so the 10Hz
/// watchdog logs and reports once rather than every tick. An entry is dropped
/// when `set_draw_active` legitimately re-arms that panel.
#[cfg(target_os = "macos")]
static CLICK_CAPTURE_REPORTED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn click_capture_episode_is_new(label: &str) -> bool {
    CLICK_CAPTURE_REPORTED
        .lock_unpoisoned()
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(label.to_string())
}

#[cfg(target_os = "macos")]
fn end_click_capture_episode(label: &str) {
    if let Some(reported) = CLICK_CAPTURE_REPORTED.lock_unpoisoned().as_mut() {
        reported.remove(label);
    }
}

/// Watchdog: drop cursor capture from every overlay that is capturing without
/// a live publication behind it. Runs on `share_border`'s ~10Hz tracker tick,
/// so it must stay cheap when nothing is drawing -- one registry lock and out.
/// Refs #872.
#[cfg(target_os = "macos")]
pub(crate) fn enforce_click_through_without_publication(app: &AppHandle) {
    use tauri::Manager;

    // Fast path: `draw_active` is false for every handle in the overwhelmingly
    // common case, so a tick normally costs one registry lock and nothing else.
    let capturing_windows = with_registry(|reg| {
        reg.active
            .values()
            .chain(reg.retired.iter())
            .filter(|handle| handle.draw_active)
            .map(|handle| handle.window_id)
            .collect::<std::collections::HashSet<_>>()
    });
    if capturing_windows.is_empty() {
        return;
    }

    // Resolve publication state with the registry lock RELEASED. Holding
    // OVERLAYS while taking SessionState's lock would nest two unrelated
    // mutexes on a background thread; hover_tab already nests the other way
    // round (SHARE_STATE -> OVERLAYS), and a hang here has no tooling
    // safeguard. No session state -> nothing can be legitimately published,
    // so fail safe toward click-through rather than an unclickable desktop.
    let session = app.try_state::<crate::session::SessionState>();
    let published = capturing_windows
        .into_iter()
        .filter(|&window_id| {
            session
                .as_ref()
                .is_some_and(|state| state.is_share_active(window_id))
        })
        .collect::<std::collections::HashSet<_>>();
    drop(session);
    let planned =
        with_registry(|reg| plan_click_capture_clear(reg, &|window_id| published.contains(&window_id)));
    if planned.is_empty() {
        return;
    }

    for clear in &planned {
        if !click_capture_episode_is_new(&clear.label) {
            continue;
        }
        log::warn!(
            "share_overlay: watchdog dropping cursor capture from panel '{}' (window {}, overlay {:?}) -- reason={} (#872)",
            clear.label,
            clear.window_id,
            clear.overlay_id,
            clear.reason.as_str()
        );
        crate::logging::capture_sentry_diagnostic(
            crate::logging::SentryDiagnosticEvent::ShareOverlayCursorCaptureCleared(
                crate::logging::ShareOverlayCursorCaptureDiagnostic {
                    role: crate::logging::DiagnosticRole::Sharer,
                    reason: clear.reason.diagnostic_tag(),
                },
            ),
        );
    }

    with_registry(|reg| {
        for clear in &planned {
            if let Some(overlay_id) = clear.overlay_id {
                if let Some(handle) = reg.active.get_mut(&overlay_id) {
                    handle.draw_active = false;
                }
            }
            for handle in reg.retired.iter_mut() {
                if handle.label == clear.label {
                    handle.draw_active = false;
                }
            }
        }
    });

    let app_main = app.clone();
    let labels = planned
        .into_iter()
        .map(|clear| clear.label)
        .collect::<Vec<_>>();
    if let Err(error) = app.run_on_main_thread(move || {
        for label in labels {
            let Some(window) = app_main.get_webview_window(&label) else {
                continue;
            };
            if let Err(error) = window.set_ignore_cursor_events(true) {
                log::warn!(
                    "share_overlay: watchdog failed to restore click-through for '{label}': {error}"
                );
            }
        }
    }) {
        log::error!("share_overlay: watchdog run_on_main_thread failed: {error}");
    }
}

/// Toggle input on the existing sharer overlay. The panel stays alive for the
/// whole share and only its click-through flag changes, because destroying a
/// tauri-nspanel panel can abort later during deferred AppKit teardown.
#[cfg(target_os = "macos")]
pub(crate) fn set_draw_active(app: &AppHandle, window_id: u32, active: bool) -> Result<(), String> {
    use tauri::Manager;

    let Some((overlay_id, label, previous)) = with_registry(|reg| {
        reg.active.iter_mut().find_map(|(&id, handle)| {
            (handle.window_id == window_id && handle.realized && !handle.hide_requested).then(
                || {
                    let previous = handle.draw_active;
                    handle.draw_active = active;
                    (id, handle.label.clone(), previous)
                },
            )
        })
    }) else {
        return Err(format!("share overlay for window {window_id} is not open"));
    };

    // #872: record the toggle. `draw_active` is the ONLY thing that makes this
    // overlay capture the cursor, and nothing recorded it -- so when a user
    // reported "I cannot click on buttons in my apps", telemetry could not say
    // whether they had drawing on. Emitted on a real state change only.
    if previous != active {
        crate::analytics::annotation_toggled(active);
    }

    let app_main = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        // A real draw-mode change ends whatever watchdog episode this panel was
        // in, so a later orphan is reported again instead of being deduped
        // against a stale one (#872).
        end_click_capture_episode(&label);
        let Some(window) = app_main.get_webview_window(&label) else {
            log::warn!("share_overlay: panel '{label}' missing while changing draw mode");
            if active {
                with_registry(|reg| {
                    if let Some(handle) = reg.active.get_mut(&overlay_id) {
                        handle.draw_active = previous;
                    }
                });
            }
            return;
        };
        if !active {
            // #872: a failed deactivation must never preserve desktop-wide
            // capture, even if retirement wins the following liveness race.
            if let Err(error) = window.set_ignore_cursor_events(true) {
                log::warn!(
                    "share_overlay: failed to restore click-through for window {window_id}: {error}"
                );
            }
        }
        // The overlay can be hidden/retired by a concurrent unshare between
        // the registry write above and this closure actually running on the
        // main thread. Re-check liveness here rather than trusting the
        // pre-dispatch snapshot, so a losing race no-ops instead of
        // re-showing a panel over a window that's no longer shared.
        let still_live = with_registry(|reg| {
            reg.active
                .get(&overlay_id)
                .is_some_and(|handle| handle.realized && !handle.hide_requested)
        });
        if !still_live {
            log::debug!(
                "share_overlay: overlay for window {window_id} hidden/retired before draw-mode dispatch ran; skipping"
            );
            return;
        }
        if active {
            if let Err(error) = window.set_ignore_cursor_events(false) {
                log::warn!(
                    "share_overlay: failed to set click-through=false for window {window_id}: {error}"
                );
                with_registry(|reg| {
                    if let Some(handle) = reg.active.get_mut(&overlay_id) {
                        handle.draw_active = previous;
                    }
                });
                return;
            }
            if let Err(error) = window.show() {
                log::warn!("share_overlay: failed to show draw overlay for window {window_id}: {error}");
            }
            if let Err(error) = window.set_focus() {
                log::warn!("share_overlay: failed to focus draw overlay for window {window_id}: {error}");
            }
        }
        let active_json = if active { "true" } else { "false" };
        if let Err(error) = window.eval(format!(
            "window.__petalDrawSetActive && window.__petalDrawSetActive({active_json});"
        )) {
            log::warn!(
                "share_overlay: failed to update draw mode for window {window_id} overlay '{}': {error}",
                window.label()
            );
            if active {
                let _ = window.set_ignore_cursor_events(true);
                with_registry(|reg| {
                    if let Some(handle) = reg.active.get_mut(&overlay_id) {
                        handle.draw_active = previous;
                    }
                });
            }
        }
    }) {
        if active {
            with_registry(|reg| {
                if let Some(handle) = reg.active.get_mut(&overlay_id) {
                    handle.draw_active = previous;
                }
            });
        }
        return Err(format!("dispatch share overlay draw mode failed: {error}"));
    }
    Ok(())
}

/// Frontend trigger for the sharer's own annotation mode. The session state
/// remains the source of truth so a stale hover-tab item cannot make a hidden
/// or already-unshared overlay interactive.
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn share_overlay_set_draw_active(
    app: AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    window_id: u32,
    active: bool,
) -> Result<(), String> {
    if !state.is_share_active(window_id) {
        return Err(format!("window {window_id} is not actively shared"));
    }
    set_draw_active(&app, window_id, active)
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn share_overlay_draw_active(window_id: u32) -> bool {
    with_registry(|reg| {
        reg.active
            .values()
            .find(|handle| {
                handle.window_id == window_id && handle.realized && !handle.hide_requested
            })
            .is_some_and(|handle| handle.draw_active)
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn show_share_overlay(
    app: &AppHandle,
    owner_identity: &str,
    window_id: u32,
    frame: WindowFrame,
    show_draw_toolbar: bool,
) -> u32 {
    let owner_identity = owner_identity.to_string();
    let reservation = with_registry(|reg| {
        reg.reserve_show(
            owner_identity.clone(),
            window_id,
            frame,
            show_draw_toolbar,
            || NEXT_OVERLAY_ID.fetch_add(1, Ordering::SeqCst),
        )
    });
    let overlay_id = reservation.overlay_id;

    match reservation.kind {
        ShowKind::UpdateExisting => log::info!(
            "share_overlay: show overlay {overlay_id} for owner '{owner_identity}' window {window_id} is idempotent update at ({},{}) {}x{} ({})",
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            reservation.counts
        ),
        ShowKind::ReuseRetired => log::info!(
            "share_overlay: show overlay {overlay_id} for owner '{owner_identity}' window {window_id} reuses retired panel at ({},{}) {}x{} ({})",
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            reservation.counts
        ),
        ShowKind::CreateFresh => log::info!(
            "share_overlay: show overlay {overlay_id} for owner '{owner_identity}' window {window_id} creates fresh panel at ({},{}) {}x{} ({})",
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            reservation.counts
        ),
    }

    let app_main = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        realize_share_overlay(&app_main, overlay_id);
    }) {
        let counts = with_registry(|reg| reg.mark_show_dispatch_failed(overlay_id));
        log::error!(
            "share_overlay: run_on_main_thread failed for window {window_id}: {e} ({counts})"
        );
    }

    overlay_id
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn show_share_overlay(
    _app: &AppHandle,
    _owner_identity: &str,
    _window_id: u32,
    _frame: WindowFrame,
) -> u32 {
    NEXT_OVERLAY_ID.fetch_add(1, Ordering::SeqCst)
}

#[cfg(target_os = "macos")]
pub(crate) async fn prewarm_share_overlays(app: &AppHandle) {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let app_main = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        prewarm_share_overlays_on_main(&app_main);
        let _ = done_tx.send(());
    }) {
        log::error!("share_overlay: failed to schedule post-join panel prewarm: {error}");
        return;
    }
    if done_rx.await.is_err() {
        log::error!("share_overlay: post-join panel prewarm ended without completion");
    }
}

#[cfg(target_os = "macos")]
fn prewarm_share_overlays_on_main(app_main: &AppHandle) {
    let target = crate::session::MAX_CONCURRENT_SHARES;
    let missing = with_registry(|reg| {
        target.saturating_sub(reg.active.len().saturating_add(reg.retired.len()))
    });

    for _ in 0..missing {
        let panel_id = NEXT_OVERLAY_ID.fetch_add(1, Ordering::SeqCst);
        let handle = OverlayHandle {
            label: overlay_label(panel_id),
            owner_identity: String::new(),
            window_id: 0,
            frame: PREWARM_PANEL_FRAME,
            panel_number: 0,
            tracker_hidden: false,
            realized: true,
            hide_requested: false,
            draw_active: false,
            show_draw_toolbar: false,
        };

        match build_share_overlay_panel(app_main, &handle) {
            Ok(_panel) => {
                // Direct manufacture: this panel has never been shown. Put its
                // already-realized handle straight into the normal reuse pool;
                // do not route through hide_share_overlay (issue #680).
                with_registry(|reg| reg.retired.push(handle));
            }
            Err(error) => {
                log::error!(
                    "share_overlay: failed to prewarm panel '{}': {error}",
                    handle.label
                );
            }
        }
    }

    let counts = with_registry(|reg| reg.counts());
    log::info!("share_overlay: post-join prewarm complete at target {target} ({counts})");
}

#[cfg(target_os = "macos")]
fn build_share_overlay_panel(
    app_main: &AppHandle,
    handle: &OverlayHandle,
) -> tauri::Result<std::sync::Arc<dyn tauri_nspanel::Panel>> {
    use tauri::{Manager, WebviewUrl};
    use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

    tauri_panel! {
        panel!(ShareOverlayPanel {
            config: {
                can_become_key_window: false,
                is_floating_panel: true
            }
        })
    }

    let url = share_overlay_url(
        handle.window_id,
        &handle.owner_identity,
        handle.show_draw_toolbar,
    );
    let layout = share_overlay_layout(handle.frame);

    PanelBuilder::<_, ShareOverlayPanel>::new(app_main, &handle.label)
        .url(WebviewUrl::App(url.into()))
        .title(SHARE_OVERLAY_WINDOW_TITLE)
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: layout.panel.x as f64,
            y: layout.panel.y as f64,
        }))
        .level(PanelLevel::Normal)
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: layout.panel.width.max(1) as f64,
            height: layout.panel.height.max(1) as f64,
        }))
        .has_shadow(false)
        .transparent(true)
        .no_activate(true)
        .style_mask(tauri_nspanel::StyleMask::empty().nonactivating_panel())
        .with_window(|w| w.decorations(false).transparent(true).visible(false))
        .collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary(),
        )
        .build()
}

#[cfg(target_os = "macos")]
fn realize_share_overlay(app_main: &AppHandle, overlay_id: u32) {
    use tauri::Manager;

    let Some(handle) = with_registry(|reg| {
        reg.active
            .get(&overlay_id)
            .filter(|h| !h.hide_requested)
            .cloned()
    }) else {
        let counts = with_registry(|reg| reg.counts());
        log::info!(
            "share_overlay: show overlay {overlay_id} skipped; request was hidden or canceled before AppKit work ({counts})"
        );
        return;
    };

    if handle.realized {
        if let Some(window) = app_main.get_webview_window(&handle.label) {
            let panel_number = update_share_overlay_window(app_main, &window, &handle);
            let counts = with_registry(|reg| {
                if let Some(active) = reg.active.get_mut(&overlay_id) {
                    active.panel_number = panel_number;
                    active.realized = true;
                    active.tracker_hidden = false;
                }
                reg.counts()
            });
            log::info!(
                "share_overlay: updated existing panel '{}' as overlay {overlay_id} (window {}{}) ({counts})",
                handle.label,
                handle.window_id,
                cg_window_id_suffix(&window)
            );
            return;
        }
        log::warn!(
            "share_overlay: active/retired panel '{}' missing at show time; rebuilding overlay {overlay_id}",
            handle.label
        );
    }

    match build_share_overlay_panel(app_main, &handle) {
        Ok(panel) => {
            let mut panel_number = 0i64;
            let mut suffix = String::new();
            if let Some(window) = app_main.get_webview_window(&handle.label) {
                let _ = window.set_ignore_cursor_events(!handle.draw_active);
                crate::webview_transparency::apply_or_retry(app_main, &window);
                set_share_overlay_frame(&window, handle.frame);
                // No set_overlay_window_id/set_overlay_owner_identity eval here:
                // `url` above already encodes windowId/ownerIdentity, and
                // compositor/pointer.html's +page.svelte reads both via
                // `page.url.searchParams` into `$derived` state used from first
                // render, so a fresh page is correct without an eval. A
                // still-launching WebContent process made this class of eval's
                // `runJavaScriptInFrameInScriptWorld` call the last main-thread
                // breadcrumb before the #680 AppKit wedge. The reuse path
                // (`update_share_overlay_window`) still needs both evals -- it
                // patches an EXISTING page whose URL is stale. Refs #680.
                clear_overlay_page(&window);
                panel.show();
                panel_number = order_above_shared(&window, handle.window_id);
                suffix = cg_window_id_suffix(&window);
            } else {
                panel.show();
            }
            let counts = with_registry(|reg| {
                if let Some(active) = reg.active.get_mut(&overlay_id) {
                    active.realized = true;
                    active.panel_number = panel_number;
                    active.tracker_hidden = false;
                }
                reg.counts()
            });
            log::info!(
                "share_overlay: created fresh panel '{}' as overlay {overlay_id} (window {}{}) ({counts})",
                handle.label,
                handle.window_id,
                suffix
            );
        }
        Err(e) => {
            let counts = with_registry(|reg| reg.mark_show_dispatch_failed(overlay_id));
            log::error!(
                "share_overlay: failed to create overlay panel for window {}: {e} ({counts})",
                handle.window_id
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn update_share_overlay_window(
    app: &AppHandle,
    window: &tauri::WebviewWindow,
    handle: &OverlayHandle,
) -> i64 {
    let _ = window.set_ignore_cursor_events(!handle.draw_active);
    crate::webview_transparency::apply_or_retry(app, window);
    set_share_overlay_frame(window, handle.frame);
    set_overlay_window_id(window, handle.window_id);
    set_overlay_owner_identity(window, &handle.owner_identity);
    set_overlay_draw_toolbar(window, handle.show_draw_toolbar);
    clear_overlay_page(window);
    let _ = window.show();
    order_above_shared(window, handle.window_id)
}

#[cfg(target_os = "macos")]
fn set_share_overlay_frame(window: &tauri::WebviewWindow, frame: WindowFrame) {
    let layout = share_overlay_layout(frame);
    let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
        x: layout.panel.x as f64,
        y: layout.panel.y as f64,
    }));
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
        width: layout.panel.width.max(1) as f64,
        height: layout.panel.height.max(1) as f64,
    }));
}

#[cfg(target_os = "macos")]
fn set_overlay_window_id(window: &tauri::WebviewWindow, window_id: u32) {
    let js = format!(
        r#"(() => {{
  const windowId = {window_id};
  const apply = () => {{
    if (typeof window.__petalOverlaySetWindowId !== 'function') return false;
    window.__petalOverlaySetWindowId(windowId);
    return true;
  }};
  if (apply()) return;
  let attempts = 0;
  const timer = setInterval(() => {{
    attempts += 1;
    if (apply() || attempts >= 20) clearInterval(timer);
  }}, 50);
}})();"#
    );
    if let Err(e) = window.eval(&js) {
        log::warn!(
            "share_overlay: failed to set overlay window id for '{}': {e}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn set_overlay_owner_identity(window: &tauri::WebviewWindow, owner_identity: &str) {
    let Ok(identity) = serde_json::to_string(owner_identity) else {
        return;
    };
    let js = format!(
        r#"(() => {{
  const ownerIdentity = {identity};
  const apply = () => {{
    if (typeof window.__petalOverlaySetOwnerIdentity !== 'function') return false;
    window.__petalOverlaySetOwnerIdentity(ownerIdentity);
    return true;
  }};
  if (apply()) return;
  let attempts = 0;
  const timer = setInterval(() => {{
    attempts += 1;
    if (apply() || attempts >= 20) clearInterval(timer);
  }}, 50);
}})();"#
    );
    if let Err(error) = window.eval(&js) {
        log::warn!(
            "share_overlay: failed to set overlay owner identity for '{}': {error}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn set_overlay_draw_toolbar(window: &tauri::WebviewWindow, visible: bool) {
    let visible = if visible { "true" } else { "false" };
    let js = format!(
        r#"window.__petalDrawSetToolbarVisible && window.__petalDrawSetToolbarVisible({visible})"#
    );
    if let Err(error) = window.eval(&js) {
        log::warn!(
            "share_overlay: failed to set draw toolbar visibility for '{}': {error}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn clear_overlay_page(window: &tauri::WebviewWindow) {
    let js = r#"window.__petalOverlayClear && window.__petalOverlayClear()"#;
    if let Err(e) = window.eval(js) {
        log::warn!(
            "share_overlay: failed to clear overlay page for '{}': {e}",
            window.label()
        );
    }
}

pub(crate) fn hide_share_overlay(app: &AppHandle, overlay_id: u32) {
    let reservation = with_registry(|reg| reg.request_hide(overlay_id));
    match reservation.kind {
        HideKind::Unknown => {
            log::info!(
                "share_overlay: hide overlay {overlay_id} ignored; no active panel ({})",
                reservation.counts
            );
            return;
        }
        HideKind::AlreadyPending => {
            log::info!(
                "share_overlay: hide overlay {overlay_id} ignored; hide already pending for panel '{}' ({})",
                reservation.label.as_deref().unwrap_or("<unknown>"),
                reservation.counts
            );
            return;
        }
        HideKind::Hide => {}
    }

    #[cfg(target_os = "macos")]
    {
        log::info!(
            "share_overlay: hide overlay {overlay_id} begin (panel '{}', marshalling to main thread) ({})",
            reservation.label.as_deref().unwrap_or("<unknown>"),
            reservation.counts
        );
        let app_main = app.clone();
        if let Err(e) = app.run_on_main_thread(move || {
            complete_hide_share_overlay(&app_main, overlay_id);
        }) {
            let counts = with_registry(|reg| reg.undo_hide_request(overlay_id));
            log::error!("share_overlay: run_on_main_thread (hide) failed: {e} ({counts})");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        with_registry(|reg| {
            reg.take_hide_requested(overlay_id);
        });
    }
}

#[cfg(target_os = "macos")]
fn complete_hide_share_overlay(app_main: &AppHandle, overlay_id: u32) {
    use tauri::Manager;

    let Some(handle) = with_registry(|reg| reg.take_hide_requested(overlay_id)) else {
        let counts = with_registry(|reg| reg.counts());
        log::info!(
            "share_overlay: hide overlay {overlay_id} skipped; panel was re-shown before hide ran ({counts})"
        );
        return;
    };

    if !handle.realized {
        let counts = with_registry(|reg| reg.counts());
        log::info!(
            "share_overlay: pending panel '{}' for overlay {overlay_id} dropped before creation ({counts})",
            handle.label
        );
        return;
    }

    let label = handle.label.clone();
    end_click_capture_episode(&label);
    if let Some(window) = app_main.get_webview_window(&label) {
        let _ = window.set_ignore_cursor_events(true);
        clear_overlay_page(&window);
        let _ = window.hide();
        let suffix = cg_window_id_suffix(&window);
        let counts = with_registry(|reg| {
            reg.retired.push(handle);
            reg.counts()
        });
        log::info!(
            "share_overlay: panel '{label}' hidden{suffix} -- retiring for reuse (never destroyed) ({counts})"
        );
    } else {
        let counts = with_registry(|reg| reg.counts());
        log::warn!(
            "share_overlay: panel '{label}' not found at hide time -- dropping stale handle (never destroyed) ({counts})"
        );
    }
}

#[cfg(target_os = "macos")]
/// #761: window_id -> overlay panel CGWindowID (same drag-nudge cache pattern
/// as share_border's; the overlay covers the shared window's frame exactly).
static DRAG_OVERLAY_WIDS: Mutex<Option<std::collections::HashMap<u32, u32>>> = Mutex::new(None);

#[cfg(target_os = "macos")]
pub(crate) fn drag_nudge_overlay(window_id: u32, x: f64, y: f64) {
    let wid = DRAG_OVERLAY_WIDS
        .lock_unpoisoned()
        .as_ref()
        .and_then(|m| m.get(&window_id).copied());
    if let Some(wid) = wid {
        let _ = crate::platform::sls::move_own_window(wid, x, y);
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn sync_frame_for_window_on_main(app: &AppHandle, window_id: u32, frame: WindowFrame) {
    use tauri::Manager;

    // #761: rigid drag active -> the event nudges own the overlay position.
    #[cfg(target_os = "macos")]
    if crate::platform::gesture_tap::gesture_track_for(window_id, 40).is_some_and(|t| t.rigid) {
        return;
    }

    let overlays = with_registry(|reg| {
        reg.active
            .iter_mut()
            .filter_map(|(&id, h)| {
                if h.window_id == window_id && !h.hide_requested {
                    h.frame = frame;
                    Some((id, h.label.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    });
    for (overlay_id, label) in overlays {
        let Some(window) = app.get_webview_window(&label) else {
            continue;
        };
        #[cfg(target_os = "macos")]
        if let Ok(ns) = window.ns_window() {
            let n: isize =
                unsafe { objc2::msg_send![ns as *mut objc2::runtime::AnyObject, windowNumber] };
            if n > 0 {
                DRAG_OVERLAY_WIDS
                    .lock_unpoisoned()
                    .get_or_insert_with(std::collections::HashMap::new)
                    .insert(window_id, n as u32);
            }
        }
        set_share_overlay_frame(&window, frame);
        let panel_number = order_above_shared(&window, window_id);
        with_registry(|reg| {
            if let Some(handle) = reg.active.get_mut(&overlay_id) {
                handle.panel_number = panel_number;
                handle.tracker_hidden = false;
            }
        });
    }
    crate::ai_chat::panel::update_ai_chat_panel_frame(app, window_id, frame);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn sync_frame_for_window_on_main(
    _app: &AppHandle,
    _window_id: u32,
    _frame: WindowFrame,
) {
}

#[cfg(target_os = "macos")]
pub(crate) fn order_above_shared_for_window_on_main(app: &AppHandle, window_id: u32) {
    use tauri::Manager;

    let overlays = with_registry(|reg| {
        reg.active
            .iter()
            .filter_map(|(&id, h)| {
                (h.window_id == window_id && !h.hide_requested).then(|| (id, h.label.clone()))
            })
            .collect::<Vec<_>>()
    });
    for (overlay_id, label) in overlays {
        let Some(window) = app.get_webview_window(&label) else {
            continue;
        };
        #[cfg(target_os = "macos")]
        if let Ok(ns) = window.ns_window() {
            let n: isize =
                unsafe { objc2::msg_send![ns as *mut objc2::runtime::AnyObject, windowNumber] };
            if n > 0 {
                DRAG_OVERLAY_WIDS
                    .lock_unpoisoned()
                    .get_or_insert_with(std::collections::HashMap::new)
                    .insert(window_id, n as u32);
            }
        }
        {
            let _dedup_scope = ();
        };
        let panel_number = order_above_shared(&window, window_id);
        with_registry(|reg| {
            if let Some(handle) = reg.active.get_mut(&overlay_id) {
                handle.panel_number = panel_number;
                handle.tracker_hidden = false;
            }
        });
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn order_above_shared_for_window_on_main(_app: &AppHandle, _window_id: u32) {}

#[cfg(target_os = "macos")]
pub(crate) fn order_out_for_window_on_main(app: &AppHandle, window_id: u32) {
    use tauri::Manager;

    let overlays = with_registry(|reg| {
        reg.active
            .iter_mut()
            .filter_map(|(&id, h)| {
                (h.window_id == window_id && !h.hide_requested).then(|| {
                    h.draw_active = false;
                    (id, h.label.clone())
                })
            })
            .collect::<Vec<_>>()
    });
    for (overlay_id, label) in overlays {
        let Some(window) = app.get_webview_window(&label) else {
            continue;
        };
        #[cfg(target_os = "macos")]
        if let Ok(ns) = window.ns_window() {
            let n: isize =
                unsafe { objc2::msg_send![ns as *mut objc2::runtime::AnyObject, windowNumber] };
            if n > 0 {
                DRAG_OVERLAY_WIDS
                    .lock_unpoisoned()
                    .get_or_insert_with(std::collections::HashMap::new)
                    .insert(window_id, n as u32);
            }
        }
        {
            let _dedup_scope = ();
        };
        // #872: anything ordered off screen must be click-through even if
        // draw teardown lost a race with the tracker.
        let _ = window.set_ignore_cursor_events(true);
        order_out(&window);
        with_registry(|reg| {
            if let Some(handle) = reg.active.get_mut(&overlay_id) {
                handle.tracker_hidden = true;
            }
        });
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn order_out_for_window_on_main(_app: &AppHandle, _window_id: u32) {}

#[cfg(target_os = "macos")]
fn cg_window_id_suffix(window: &tauri::WebviewWindow) -> String {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    match window.ns_window() {
        Ok(ns_window_ptr) => {
            let window_number: i64 =
                unsafe { msg_send![ns_window_ptr as *mut AnyObject, windowNumber] };
            format!(", CGWindowID={window_number}")
        }
        Err(_) => String::new(),
    }
}

#[cfg(target_os = "macos")]
fn order_above_shared(window: &tauri::WebviewWindow, shared_window_id: u32) -> i64 {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(ns_ptr) = window.ns_window() else {
        log::warn!(
            "share_overlay: ns_window() unavailable for '{}'; cannot order relative to window {shared_window_id}",
            window.label()
        );
        return 0;
    };
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        let _: () = msg_send![ns, setLevel: 0isize];
        let _: () = msg_send![ns, orderWindow: 1isize, relativeTo: shared_window_id as isize];
        let number: i64 = msg_send![ns, windowNumber];
        number
    }
}

#[cfg(target_os = "macos")]
fn order_out(window: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    if let Ok(ns_ptr) = window.ns_window() {
        unsafe {
            let ns = ns_ptr as *mut AnyObject;
            let _: () = msg_send![ns, orderOut: std::ptr::null_mut::<AnyObject>()];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static REGISTRY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn f(x: i32, y: i32, w: i32, h: i32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn handle(label: &str, owner_identity: &str, window_id: u32, realized: bool) -> OverlayHandle {
        OverlayHandle {
            label: label.to_string(),
            owner_identity: owner_identity.to_string(),
            window_id,
            frame: f(10, 10, 300, 200),
            panel_number: if realized { 100 } else { 0 },
            tracker_hidden: false,
            realized,
            hide_requested: false,
            draw_active: false,
            show_draw_toolbar: false,
        }
    }

    #[test]
    fn overlay_label_is_stable() {
        assert_eq!(overlay_label(1), "share_overlay_1");
        assert_eq!(overlay_label(42), "share_overlay_42");
    }

    #[test]
    fn overlay_layout_maps_normalized_points_to_shared_surface() {
        let frame = f(40, 200, 180, 100);
        let layout = share_overlay_layout(frame);
        assert_eq!(layout.panel, frame);
        assert_eq!(layout.content_width, 180.0);
        assert_eq!(layout.content_height, 100.0);
        assert_eq!(normalized_to_content_point(frame, 0.25, 0.5), (85.0, 250.0));
    }

    #[test]
    fn overlay_layout_coordinate_mapping_round_trips_normalized_points() {
        let frame = f(40, 200, 180, 100);

        for (x, y) in [(0.0, 0.0), (0.25, 0.5), (1.0, 1.0)] {
            let content_point = normalized_to_content_point(frame, x, y);
            assert_eq!(content_to_normalized_point(frame, content_point.0, content_point.1), (x, y));
        }
    }

    #[test]
    fn overlay_layout_minimizes_degenerate_source_dimensions() {
        let frame = f(40, 200, 0, -10);
        let layout = share_overlay_layout(frame);
        assert_eq!(layout.panel, f(40, 200, 1, 1));
        assert_eq!(normalized_to_content_point(frame, 2.0, -1.0), (41.0, 200.0));
    }

    #[test]
    fn reserve_show_is_idempotent_per_owner_window() {
        let mut reg = Registry::default();

        let first = reg.reserve_show("owner-a".into(), 77, f(10, 10, 300, 200), false, || 1);
        assert_eq!(first.overlay_id, 1);
        assert_eq!(first.kind, ShowKind::CreateFresh);
        assert_eq!(reg.active_count_for_window("owner-a", 77), 1);

        let updated_frame = f(30, 40, 500, 360);
        let second = reg.reserve_show("owner-a".into(), 77, updated_frame, false, || {
            panic!("idempotent show must not allocate a second overlay id")
        });
        assert_eq!(second.overlay_id, 1);
        assert_eq!(second.kind, ShowKind::UpdateExisting);
        assert_eq!(reg.active_count_for_window("owner-a", 77), 1);
        assert_eq!(reg.active.len(), 1);
        assert_eq!(reg.active.get(&1).unwrap().frame, updated_frame);
    }

    #[test]
    fn reserve_show_keeps_owners_separate_for_same_window_id() {
        let mut reg = Registry::default();

        reg.reserve_show("owner-a".into(), 77, f(10, 10, 300, 200), false, || 1);
        reg.reserve_show("owner-b".into(), 77, f(10, 10, 300, 200), false, || 2);

        assert_eq!(reg.active_count_for_window("owner-a", 77), 1);
        assert_eq!(reg.active_count_for_window("owner-b", 77), 1);
        assert_eq!(reg.active.len(), 2);
    }

    #[test]
    fn reserve_show_reuses_retired_panel() {
        let mut reg = Registry::default();
        reg.retired
            .push(handle("share_overlay_1", "owner-a", 0, true));

        let reservation = reg.reserve_show("owner-b".into(), 88, f(20, 30, 640, 480), false, || 2);
        assert_eq!(reservation.overlay_id, 2);
        assert_eq!(reservation.kind, ShowKind::ReuseRetired);
        assert_eq!(
            reservation.counts,
            RegistryCounts {
                live: 1,
                pending: 0,
                retired: 0,
            }
        );

        let active = reg.active.get(&2).unwrap();
        assert_eq!(active.label, "share_overlay_1");
        assert_eq!(active.owner_identity, "owner-b");
        assert_eq!(active.window_id, 88);
        assert!(active.realized);
    }

    #[test]
    fn reserve_show_cancels_pending_hide_for_same_owner_window() {
        let mut reg = Registry::default();
        reg.active
            .insert(1, handle("share_overlay_1", "owner-a", 77, true));

        let hide = reg.request_hide(1);
        assert_eq!(hide.kind, HideKind::Hide);
        assert!(reg.active.get(&1).unwrap().hide_requested);

        let show = reg.reserve_show("owner-a".into(), 77, f(50, 60, 320, 240), false, || {
            panic!("re-show during pending hide must reuse the same active overlay")
        });
        assert_eq!(show.overlay_id, 1);
        assert_eq!(show.kind, ShowKind::UpdateExisting);
        assert_eq!(reg.active_count_for_window("owner-a", 77), 1);
        assert!(!reg.active.get(&1).unwrap().hide_requested);
        assert!(reg.take_hide_requested(1).is_none());
    }

    #[test]
    fn take_hide_requested_clears_draw_active_before_retirement() {
        let mut reg = Registry::default();
        let mut active = handle("share_overlay_1", "owner-a", 77, true);
        active.draw_active = true;
        active.hide_requested = true;
        reg.active.insert(1, active);

        let retired = reg.take_hide_requested(1).expect("requested hide");
        assert!(!retired.draw_active);
    }

    #[test]
    fn click_capture_clear_planner_covers_every_registry_rule() {
        let active_handle = || {
            let mut value = handle("share_overlay_1", "owner-a", 77, true);
            value.draw_active = true;
            value
        };

        let mut reg = Registry::default();
        reg.active.insert(1, active_handle());
        assert_eq!(
            plan_click_capture_clear(&reg, &|_| false)
                .into_iter()
                .map(|c| (c.overlay_id.unwrap(), c.window_id, c.reason))
                .collect::<Vec<_>>(),
            vec![(1, 77, ClickCaptureClearReason::NoPublication)]
        );

        reg.active.get_mut(&1).unwrap().hide_requested = true;
        assert_eq!(
            plan_click_capture_clear(&reg, &|_| true)
                .into_iter()
                .map(|c| (c.overlay_id.unwrap(), c.window_id, c.reason))
                .collect::<Vec<_>>(),
            vec![(1, 77, ClickCaptureClearReason::HidePending)]
        );

        reg.active.clear();
        reg.retired.push(active_handle());
        assert_eq!(
            plan_click_capture_clear(&reg, &|_| true)
                .into_iter()
                .map(|c| (c.overlay_id, c.window_id, c.reason))
                .collect::<Vec<_>>(),
            // A retired handle lives in a Vec, not the id-keyed map, so it
            // carries no overlay_id -- the clear is driven off its label.
            vec![(None, 77, ClickCaptureClearReason::Retired)]
        );

        reg.retired.clear();
        reg.active.insert(1, active_handle());
        assert!(plan_click_capture_clear(&reg, &|_| true).is_empty());

        reg.active.get_mut(&1).unwrap().draw_active = false;
        reg.retired.push(handle("share_overlay_2", "owner-a", 88, true));
        assert!(plan_click_capture_clear(&reg, &|_| false).is_empty());
    }

    #[test]
    fn overlay_ids_for_window_reads_registry_without_hover_tab_bookkeeping() {
        let _guard = REGISTRY_TEST_LOCK.lock_unpoisoned();
        with_registry(|reg| {
            reg.active.clear();
            reg.retired.clear();
            reg.active
                .insert(41, handle("share_overlay_41", "owner-a", 872, true));
            reg.active
                .insert(42, handle("share_overlay_42", "owner-a", 999, true));
        });

        assert_eq!(overlay_ids_for_window(872), vec![41]);

        with_registry(|reg| {
            reg.active.clear();
            reg.retired.clear();
        });
    }

    #[test]
    fn fresh_panel_build_skips_window_id_and_owner_identity_evals_reuse_path_keeps_them() {
        // Source-level pin (issue #680): a fresh CreateFresh panel must not
        // eval() windowId/ownerIdentity into a still-launching WebContent
        // process -- `share_overlay_url` already encodes both, and
        // compositor/pointer.html's +page.svelte applies them from
        // `page.url.searchParams` on first render. The reuse path patches a
        // stale EXISTING page and must keep both evals unconditionally.
        let source = include_str!("share_overlay.rs");

        let realize_fn = source
            .split_once("fn realize_share_overlay(")
            .and_then(|(_, rest)| rest.split_once("fn update_share_overlay_window("))
            .map(|(fresh_build_body, _)| fresh_build_body)
            .expect("realize_share_overlay must precede update_share_overlay_window");
        assert!(
            !realize_fn.contains("set_overlay_window_id("),
            "fresh-build path must not re-eval windowId already encoded in the URL"
        );
        assert!(
            !realize_fn.contains("set_overlay_owner_identity("),
            "fresh-build path must not re-eval ownerIdentity already encoded in the URL"
        );
        // clear_overlay_page is kept: it resets client-side pointer/stroke
        // state, which has no URL-encoded value to check redundancy against,
        // so it is out of scope for this task's URL-redundancy argument.
        assert!(
            realize_fn.contains("clear_overlay_page("),
            "fresh-build path must still clear stale client-side overlay state"
        );

        let update_fn = source
            .split_once("fn update_share_overlay_window(")
            .and_then(|(_, rest)| rest.split_once("fn set_share_overlay_frame("))
            .map(|(reuse_body, _)| reuse_body)
            .expect("update_share_overlay_window must precede set_share_overlay_frame");
        assert!(
            update_fn.contains("set_overlay_window_id("),
            "reuse path must keep patching windowId on an existing, possibly stale page"
        );
        assert!(
            update_fn.contains("set_overlay_owner_identity("),
            "reuse path must keep patching ownerIdentity on an existing, possibly stale page"
        );
    }

    #[test]
    fn prewarm_directly_manufactures_hidden_realized_retired_panels() {
        let source = include_str!("share_overlay.rs");
        let prewarm = source
            .split_once("fn prewarm_share_overlays_on_main(")
            .and_then(|(_, rest)| rest.split_once("fn build_share_overlay_panel("))
            .map(|(body, _)| body)
            .expect("overlay prewarm must precede the shared panel builder");
        assert!(prewarm.contains("crate::session::MAX_CONCURRENT_SHARES"));
        assert!(prewarm.contains("realized: true"));
        assert!(prewarm.contains("reg.retired.push(handle)"));

        let builder = source
            .split_once("fn build_share_overlay_panel(")
            .and_then(|(_, rest)| rest.split_once("fn realize_share_overlay("))
            .map(|(body, _)| body)
            .expect("shared overlay builder must precede realization");
        assert!(builder.contains(".visible(false)"));
        assert!(!builder.contains(".show();"));
    }
}
