//! Persistent colored border overlays around actively-shared windows.
//!
//! Ported from takt's `capture_highlight.rs`, generalized to support MULTIPLE
//! simultaneous borders (takt has exactly one global highlight panel, reused
//! for whichever window is hovered; Petal needs one border PER actively-shared
//! window, each in the sharing user's identity color, since several windows
//! can be shared at once).
//!
//! Each border is a transparent, click-through, non-activating NSPanel sized to
//! the shared source itself, hosting the `share-border` SvelteKit route
//! (rendering only the colored CSS border), with the color passed as a
//! `?color=` query param.
//!
//! **Z-order (issue #23):** the border must visually hug the shared
//! window at the shared window's own place in the stack — NOT float above
//! everything. `PanelLevel::Status` (level 25) painted the border over any
//! OTHER window stacked on top of the shared window. So the panel now lives
//! at the normal window level and is ordered DIRECTLY above the shared
//! window via `-[NSWindow orderWindow:NSWindowAbove relativeTo:<CGWindowID>]`
//! (the `relativeTo:` window number is global — cross-app ordering works).
//! Full-display shares are the exception: a display is not a `CGWindow`, so
//! display borders use status-level front ordering and are never hidden just
//! because their synthetic source id is absent from `CGWindowList` (#199).
//! There is no public notification for another app's window changing
//! z-position, so a light background poll ([`start_tracker`], ~100ms)
//! compares the front-to-back order from `CGWindowListCopyWindowInfo` (same
//! FFI pattern as `hover_tab.rs`/`window_diag.rs`'s private `cg` submodules)
//! and re-asserts the ordering only when the border is no longer directly in
//! front of its shared window. The same poll also repositions the border
//! when the shared window moves/resizes (closing the long-standing
//! `update_share_border_frame` TODO) and orders the border out entirely
//! while the shared window is not on-screen (minimized / other Space).
//!
//! Panels are created dynamically per-share (not a single pre-created
//! singleton like takt's). **Hidden panels are NEVER destroyed — they are
//! hidden and retired for reuse.** `window.close()` on one of these NSPanels
//! reproducibly aborts the whole app a few seconds later: an ObjC exception
//! during deferred dealloc unwinds through tao's run-loop observer as a Rust
//! foreign exception ("Rust cannot catch foreign exceptions, aborting") —
//! the EXACT crash class `compositor.rs` already hit and fixed the same way
//! (see `CompositorState::retired`'s doc comment; live-confirmed here
//! 2026-07-02 when the first real hover-pill unshare click killed the app).
//! So `hide_share_border` hides + parks the panel, and `show_share_border`
//! reuses a parked panel when one exists — share/unshare cycles reuse one
//! window instead of leaking or destroying.
//!
//! macOS-only; no-op stubs elsewhere.

use crate::platform::cg::WindowFrame;
use crate::sync_ext::MutexExt;
use crate::transport::publisher::SharedSourceKind;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use tauri::AppHandle;

/// Registry of border panels, keyed by an incrementing `border_id` (NOT the
/// window_id). `show_share_border` is idempotent per `window_id`: asking to
/// show a border for a window that already has one updates the existing panel
/// and returns the existing id instead of creating an overlapping panel.
/// `retired` holds hidden-but-alive panels available for reuse (see module doc
/// for why they are never destroyed).
static BORDERS: Mutex<Option<Registry>> = Mutex::new(None);
static NEXT_BORDER_ID: AtomicU32 = AtomicU32::new(1);
static TRACKER_WAKE: OnceLock<Arc<(Mutex<bool>, Condvar)>> = OnceLock::new();

/// Visual treatment for the active screenshare outline. The Svelte
/// `share-border` route had a 3px square border; issue #65 makes the same
/// identity-colored stroke 1px thicker and rounds the corners.
const SCREENSHARE_BORDER_STROKE_PX: f64 = 4.0;
const SCREENSHARE_BORDER_RADIUS_PX: f64 = 10.0;
const DISPLAY_SOURCE_TAG: u32 = 0x4000_0000;
const DISPLAY_SOURCE_MASK: u32 = 0x3fff_ffff;
#[cfg(target_os = "macos")]
const PREWARM_PANEL_FRAME: WindowFrame = WindowFrame {
    x: -10_000,
    y: -10_000,
    width: 1,
    height: 1,
};
#[cfg(target_os = "macos")]
const PREWARM_BORDER_COLOR: &str = "#000000";
pub(crate) const SHARE_BORDER_WINDOW_TITLE: &str = "Share Border";
#[cfg(target_os = "macos")]
const SHARE_BORDER_REVEAL_EVENT: &str = "petal-share-border-reveal";

#[derive(Debug, Clone, Copy, PartialEq)]
struct ShareBorderLayout {
    panel: WindowFrame,
    border_top: f64,
    border_width: f64,
    border_height: f64,
}

fn share_border_layout(frame: WindowFrame) -> ShareBorderLayout {
    let border_width = frame.width.max(1) as f64;
    let border_height = frame.height.max(1) as f64;
    ShareBorderLayout {
        panel: WindowFrame {
            x: frame.x,
            y: frame.y,
            width: frame.width.max(1),
            height: frame.height.max(1),
        },
        border_top: 0.0,
        border_width,
        border_height,
    }
}

fn share_border_panel_frame(frame: WindowFrame) -> WindowFrame {
    share_border_layout(frame).panel
}

/// Decode the display id embedded by `window_picker::display_source_id`.
/// Display source ids deliberately live outside the CGWindow id space, so the
/// tracker can identify them without ever treating the id as a real window.
fn display_id_from_source_id(source_id: u32) -> Option<u32> {
    (source_id & 0x8000_0000 == 0 && source_id & DISPLAY_SOURCE_TAG == DISPLAY_SOURCE_TAG)
        .then_some(source_id & DISPLAY_SOURCE_MASK)
}

#[cfg(target_os = "macos")]
fn display_frame_for_source_id(source_id: u32, fallback: WindowFrame) -> WindowFrame {
    use core_graphics::geometry::CGRect;

    let Some(display_id) = display_id_from_source_id(source_id) else {
        return fallback;
    };

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayBounds(display: u32) -> CGRect;
    }

    // CGDisplayBounds is in the same global display coordinate space as the
    // picker frame. Keep the picker frame as a safe fallback while a display
    // is disappearing or WindowServer has not published its bounds yet.
    let bounds = unsafe { CGDisplayBounds(display_id) };
    if bounds.size.width.is_finite()
        && bounds.size.height.is_finite()
        && bounds.size.width > 0.0
        && bounds.size.height > 0.0
    {
        WindowFrame {
            x: bounds.origin.x.round() as i32,
            y: bounds.origin.y.round() as i32,
            width: bounds.size.width.round() as i32,
            height: bounds.size.height.round() as i32,
        }
    } else {
        fallback
    }
}

#[cfg(target_os = "macos")]
fn share_border_url(color: &str, frame: WindowFrame, animate: bool) -> String {
    let encoded_color = urlencoding_encode(color);
    let layout = share_border_layout(frame);
    let mut url = format!(
        "share-border.html?color={encoded_color}&borderTop={}&windowWidth={}&windowHeight={}",
        layout.border_top, layout.border_width, layout.border_height
    );
    if animate {
        url.push_str("&animate=1");
    }
    url
}

#[derive(Default)]
struct Registry {
    active: HashMap<u32, BorderHandle>,
    retired: Vec<BorderHandle>,
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

#[cfg(target_os = "macos")]
fn should_reveal_via_url(
    source_kind: SharedSourceKind,
    show_kind: ShowKind,
    rebuilding_missing_panel: bool,
) -> bool {
    source_kind == SharedSourceKind::Window
        && (show_kind == ShowKind::CreateFresh || rebuilding_missing_panel)
}

#[cfg(target_os = "macos")]
fn should_reveal_via_eval(source_kind: SharedSourceKind, show_kind: ShowKind) -> bool {
    source_kind == SharedSourceKind::Window && show_kind == ShowKind::ReuseRetired
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShowReservation {
    border_id: u32,
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
    fn active_count_for_window(&self, window_id: u32) -> usize {
        self.active
            .values()
            .filter(|h| h.window_id == window_id && !h.hide_requested)
            .count()
    }

    fn reserve_show(
        &mut self,
        source_kind: SharedSourceKind,
        window_id: u32,
        frame: WindowFrame,
        color: String,
        next_border_id: impl FnOnce() -> u32,
    ) -> ShowReservation {
        if let Some(border_id) = self
            .active
            .iter()
            .find_map(|(&id, h)| (h.window_id == window_id).then_some(id))
        {
            if let Some(handle) = self.active.get_mut(&border_id) {
                handle.source_kind = source_kind;
                handle.frame = frame;
                handle.color = color;
                handle.tracker_hidden = false;
                handle.hide_requested = false;
                handle.last_shared_state = None;
                handle.suppressed_misorder_ticks = 0;
            }
            return ShowReservation {
                border_id,
                kind: ShowKind::UpdateExisting,
                counts: self.counts(),
            };
        }

        let border_id = next_border_id();
        if let Some(mut handle) = self.retired.pop() {
            handle.color = color;
            handle.source_kind = source_kind;
            handle.window_id = window_id;
            handle.frame = frame;
            handle.panel_number = 0;
            handle.tracker_hidden = false;
            handle.hide_requested = false;
            handle.last_shared_state = None;
            handle.suppressed_misorder_ticks = 0;
            // Retired handles always represent already-created panels. If a
            // later main-thread lookup proves the window disappeared anyway,
            // the realize path rebuilds or drops the stale handle explicitly.
            handle.realized = true;
            self.active.insert(border_id, handle);
            return ShowReservation {
                border_id,
                kind: ShowKind::ReuseRetired,
                counts: self.counts(),
            };
        }

        self.active.insert(
            border_id,
            BorderHandle {
                label: border_label(border_id),
                color,
                source_kind,
                window_id,
                frame,
                panel_number: 0,
                tracker_hidden: false,
                realized: false,
                hide_requested: false,
                last_shared_state: None,
                suppressed_misorder_ticks: 0,
            },
        );
        ShowReservation {
            border_id,
            kind: ShowKind::CreateFresh,
            counts: self.counts(),
        }
    }

    fn request_hide(&mut self, border_id: u32) -> HideReservation {
        let Some(handle) = self.active.get_mut(&border_id) else {
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

    fn take_hide_requested(&mut self, border_id: u32) -> Option<BorderHandle> {
        if self
            .active
            .get(&border_id)
            .is_some_and(|h| h.hide_requested)
        {
            self.active.remove(&border_id)
        } else {
            None
        }
    }

    fn mark_show_dispatch_failed(&mut self, border_id: u32) -> RegistryCounts {
        if self.active.get(&border_id).is_some_and(|h| !h.realized) {
            self.active.remove(&border_id);
        }
        self.counts()
    }

    fn undo_hide_request(&mut self, border_id: u32) -> RegistryCounts {
        if let Some(handle) = self.active.get_mut(&border_id) {
            handle.hide_requested = false;
        }
        self.counts()
    }
}

#[derive(Clone)]
struct BorderHandle {
    /// The webview window label, so we can look it up via `get_webview_window`
    /// and hide it / re-show it.
    label: String,
    /// The desired border color. Newly-created panels receive this via the URL
    /// query param; reused panels are recolored by JS injection.
    color: String,
    /// Whether this border tracks a real `CGWindow` or a full display selected
    /// through `SCContentSharingPicker`.
    source_kind: SharedSourceKind,
    /// The shared source id. For window shares this is a real `CGWindowID` and
    /// the `relativeTo:` target for z-ordering. For display shares it is the
    /// synthetic display source id from `window_picker.rs` (#199). 0 while
    /// retired.
    window_id: u32,
    /// Last requested shared-source frame. Stored in the registry so repeated
    /// `show_share_border` calls before the main-thread AppKit closure runs
    /// collapse into one panel created at the latest frame.
    frame: WindowFrame,
    /// The border panel's own `NSWindow.windowNumber` (== its CGWindowID),
    /// read on the main thread at creation/reuse. 0 until known.
    panel_number: i64,
    /// True while the tracker has ordered the panel out because its shared
    /// window is not currently on-screen (minimized / other Space).
    tracker_hidden: bool,
    /// False only for a freshly reserved show request whose AppKit panel has
    /// not been built yet.
    realized: bool,
    /// Set by `hide_share_border` until the main-thread hide closure consumes
    /// the handle. A racing `show_share_border` for the same window clears this
    /// flag, causing the pending hide closure to skip instead of hiding a
    /// newly re-shown border.
    hide_requested: bool,
    /// Last observed frame/order for the shared source window, ignoring
    /// Petal-owned overlay/app windows. Used to distinguish source movement
    /// from Petal activation reshuffling only the border panel.
    last_shared_state: Option<SharedWindowState>,
    /// Consecutive tracker ticks where the border was too far forward, but the
    /// #19 source-change debounce intentionally suppressed the reorder. A
    /// short run is treated as Petal activation noise; a persistent run is
    /// self-healed so the border cannot float over unrelated windows forever.
    suppressed_misorder_ticks: u8,
}

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = BORDERS.lock_unpoisoned();
    let reg = guard.get_or_insert_with(Registry::default);
    f(reg)
}

fn border_label(border_id: u32) -> String {
    format!("share_border_{border_id}")
}

/// Show a persistent colored border around `frame` for `window_id`, in
/// `color` (a hex string like `#f06cc9`). Returns the `border_id` used to hide
/// it later via [`hide_share_border`]. Idempotent per `window_id`: if a border
/// for this window is already active, this updates/reorders/recolors that
/// existing panel and returns its existing id.
#[cfg(target_os = "macos")]
pub fn show_share_border(app: &AppHandle, window_id: u32, frame: WindowFrame, color: &str) -> u32 {
    show_share_border_for_source(app, SharedSourceKind::Window, window_id, frame, color)
}

/// Show a persistent colored border for a shared source. Window sources keep
/// the historical relative-to-CGWindow z-order lifecycle; display sources use a
/// display-specific lifecycle because a full display is not a `CGWindow`.
#[cfg(target_os = "macos")]
pub fn show_share_border_for_source(
    app: &AppHandle,
    source_kind: SharedSourceKind,
    window_id: u32,
    frame: WindowFrame,
    color: &str,
) -> u32 {
    let color = color.to_string();
    let reservation = with_registry(|reg| {
        reg.reserve_show(source_kind, window_id, frame, color.clone(), || {
            NEXT_BORDER_ID.fetch_add(1, Ordering::SeqCst)
        })
    });
    let border_id = reservation.border_id;
    let source_label = share_source_label(source_kind);
    let show_kind = reservation.kind;

    // Info (not debug) level: native-panel lifecycle steps must be visible in
    // a default-level petal.log (issue #13 -- panel show/hide/retire/reuse is
    // exactly the crash-adjacent activity a post-mortem needs to see).
    match reservation.kind {
        ShowKind::UpdateExisting => log::info!(
            "share_border: show border {border_id} for {source_label} {window_id} is idempotent update at ({},{}) {}x{} color={} ({})",
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            color,
            reservation.counts
        ),
        ShowKind::ReuseRetired => log::info!(
            "share_border: show border {border_id} for {source_label} {window_id} reuses retired panel at ({},{}) {}x{} color={} ({})",
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            color,
            reservation.counts
        ),
        ShowKind::CreateFresh => log::info!(
            "share_border: show border {border_id} for {source_label} {window_id} creates fresh panel at ({},{}) {}x{} color={} ({})",
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            color,
            reservation.counts
        ),
    }

    // AppKit window/panel creation MUST happen on the main thread. This
    // function is reached from `toggle_share_for_window`, which runs inside an
    // ASYNC Tauri command (i.e. on a tokio worker thread, NOT the main/UI
    // thread) — building the NSPanel there traps with EXC_BREAKPOINT / "Must
    // only be used from the main thread" (seen crashing the app on the very
    // first real GUI-triggered share). So we allocate the `border_id`
    // synchronously (the caller needs it immediately for its optimistic
    // bookkeeping/rollback) and marshal the whole panel build to the main
    // thread via `run_on_main_thread`; the border simply appears a beat later.
    let app_main = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        realize_share_border(&app_main, border_id, show_kind);
    }) {
        let counts = with_registry(|reg| reg.mark_show_dispatch_failed(border_id));
        log::error!(
            "share_border: run_on_main_thread failed for {source_label} {window_id}: {e} ({counts})"
        );
    }

    border_id
}

fn share_source_label(source_kind: SharedSourceKind) -> &'static str {
    match source_kind {
        SharedSourceKind::Window => "window",
        SharedSourceKind::Display => "display",
        SharedSourceKind::DisplayRegion => "display-region",
    }
}

#[cfg(target_os = "macos")]
pub(crate) async fn prewarm_share_borders(app: &AppHandle) {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let app_main = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        prewarm_share_borders_on_main(&app_main);
        let _ = done_tx.send(());
    }) {
        log::error!("share_border: failed to schedule post-join panel prewarm: {error}");
        return;
    }
    if done_rx.await.is_err() {
        log::error!("share_border: post-join panel prewarm ended without completion");
    }
}

#[cfg(target_os = "macos")]
fn prewarm_share_borders_on_main(app_main: &AppHandle) {
    let target = crate::session::MAX_CONCURRENT_SHARES;
    let missing = with_registry(|reg| {
        target.saturating_sub(reg.active.len().saturating_add(reg.retired.len()))
    });

    for _ in 0..missing {
        let panel_id = NEXT_BORDER_ID.fetch_add(1, Ordering::SeqCst);
        let handle = BorderHandle {
            label: border_label(panel_id),
            color: PREWARM_BORDER_COLOR.to_string(),
            source_kind: SharedSourceKind::Window,
            window_id: 0,
            frame: PREWARM_PANEL_FRAME,
            panel_number: 0,
            tracker_hidden: false,
            realized: true,
            hide_requested: false,
            last_shared_state: None,
            suppressed_misorder_ticks: 0,
        };

        match build_share_border_panel(app_main, &handle, false) {
            Ok(_panel) => {
                // Direct manufacture: this panel has never been shown. Put its
                // already-realized handle straight into the normal reuse pool;
                // do not route through hide_share_border (issue #680).
                with_registry(|reg| reg.retired.push(handle));
            }
            Err(error) => {
                log::error!(
                    "share_border: failed to prewarm panel '{}': {error}",
                    handle.label
                );
            }
        }
    }

    let counts = with_registry(|reg| reg.counts());
    log::info!("share_border: post-join prewarm complete at target {target} ({counts})");
}

#[cfg(target_os = "macos")]
fn build_share_border_panel(
    app_main: &AppHandle,
    handle: &BorderHandle,
    animate_reveal: bool,
) -> tauri::Result<Arc<dyn tauri_nspanel::Panel>> {
    use tauri::{Manager, WebviewUrl};
    use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel};

    tauri_panel! {
        panel!(ShareBorderPanel {
            config: {
                can_become_key_window: false,
                is_floating_panel: true
            }
        })
    }

    let url = share_border_url(&handle.color, handle.frame, animate_reveal);
    let layout = share_border_layout(handle.frame);

    PanelBuilder::<_, ShareBorderPanel>::new(app_main, &handle.label)
        .url(WebviewUrl::App(url.into()))
        .title(SHARE_BORDER_WINDOW_TITLE)
        .position(tauri::Position::Logical(tauri::LogicalPosition {
            x: layout.panel.x as f64,
            y: layout.panel.y as f64,
        }))
        .level(match handle.source_kind {
            // Normal (level 0), NOT Status: the border must sit at the shared
            // window's own tier so windows stacked above the shared window also
            // cover the border -- see the module doc (issue #23). The exact
            // position within the tier is set right after `show()` via
            // `order_above_shared`.
            SharedSourceKind::Window => PanelLevel::Normal,
            // Displays and regions are not CGWindows, so there is no relative
            // window tier to join.
            SharedSourceKind::Display | SharedSourceKind::DisplayRegion => PanelLevel::Status,
        })
        .size(tauri::Size::Logical(tauri::LogicalSize {
            width: layout.panel.width.max(1) as f64,
            height: layout.panel.height.max(1) as f64,
        }))
        .has_shadow(false)
        .transparent(true)
        .no_activate(true)
        .style_mask(tauri_nspanel::StyleMask::empty().nonactivating_panel())
        .corner_radius(SCREENSHARE_BORDER_RADIUS_PX)
        .with_window(|w| w.decorations(false).transparent(true).visible(false))
        .collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary(),
        )
        .build()
}

#[cfg(target_os = "macos")]
fn realize_share_border(app_main: &AppHandle, border_id: u32, show_kind: ShowKind) {
    use tauri::Manager;

    let Some(handle) = with_registry(|reg| {
        reg.active
            .get(&border_id)
            .filter(|h| !h.hide_requested)
            .cloned()
    }) else {
        let counts = with_registry(|reg| reg.counts());
        log::info!(
            "share_border: show border {border_id} skipped; request was hidden or canceled before AppKit work ({counts})"
        );
        return;
    };

    let rebuilding_missing_panel = handle.realized;
    if handle.realized {
        if let Some(window) = app_main.get_webview_window(&handle.label) {
            let panel_number = update_share_border_window(app_main, &window, &handle, show_kind);
            let counts = with_registry(|reg| {
                if let Some(active) = reg.active.get_mut(&border_id) {
                    active.panel_number = panel_number;
                    active.realized = true;
                    active.tracker_hidden = false;
                }
                reg.counts()
            });
            log::info!(
                "share_border: updated existing panel '{}' as border {border_id} ({} {}{}) ({counts})",
                handle.label,
                share_source_label(handle.source_kind),
                handle.window_id,
                cg_window_id_suffix(&window)
            );
            return;
        }
        log::warn!(
            "share_border: active/retired panel '{}' missing at show time; rebuilding border {border_id}",
            handle.label
        );
    }

    let animate_reveal =
        should_reveal_via_url(handle.source_kind, show_kind, rebuilding_missing_panel);
    match build_share_border_panel(app_main, &handle, animate_reveal) {
        Ok(panel) => {
            let mut panel_number = 0i64;
            let mut suffix = String::new();
            if let Some(window) = app_main.get_webview_window(&handle.label) {
                // Click-through: never blocks interaction with the real window
                // underneath.
                let _ = window.set_ignore_cursor_events(true);
                // Without this the panel composites an opaque black rect over
                // the shared window despite `.transparent(true)` -- see
                // webview_transparency.rs's doc for why.
                crate::webview_transparency::apply_or_retry(app_main, &window);
                apply_share_border_treatment(&window);
                // No apply_share_border_layout/apply_share_border_color eval here:
                // `url` above already encodes borderTop/windowWidth/windowHeight/
                // color, and share-border/+page.svelte reads them via
                // `page.url.searchParams` into its initial `style:` bindings, so a
                // fresh page renders correctly on first paint with no eval. A
                // still-launching WebContent process made this eval's
                // `runJavaScriptInFrameInScriptWorld` call the last main-thread
                // breadcrumb before the #680 AppKit wedge. The reuse path
                // (`update_share_border_window`) still needs both evals -- it
                // patches an EXISTING page whose URL is stale. Refs #680.
                panel.show();
                panel_number = order_share_border(&window, handle.source_kind, handle.window_id);
                suffix = cg_window_id_suffix(&window);
            } else {
                panel.show();
            }
            let counts = with_registry(|reg| {
                if let Some(active) = reg.active.get_mut(&border_id) {
                    active.realized = true;
                    active.panel_number = panel_number;
                    active.tracker_hidden = false;
                }
                reg.counts()
            });
            log::info!(
                "share_border: created fresh panel '{}' as border {border_id} ({} {}{}) ({counts})",
                handle.label,
                share_source_label(handle.source_kind),
                handle.window_id,
                suffix
            );
        }
        Err(e) => {
            let counts = with_registry(|reg| reg.mark_show_dispatch_failed(border_id));
            log::error!(
                "share_border: failed to create border panel for {} {}: {e} ({counts})",
                share_source_label(handle.source_kind),
                handle.window_id
            );
        }
    }
}

#[cfg(target_os = "macos")]
/// #761: window_id -> the border panel's CGWindowID, cached from main-thread
/// apply paths so the gesture tap can SLS-move the border in lockstep with a
/// dragged shared window (same treatment as the hover pill; the panel is
/// Petal-owned). Stale entries are harmless -- a dead wid makes SLSMoveWindow
/// error and the next apply rewrites the entry.
static DRAG_PANEL_WIDS: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn cache_drag_panel_wid(window_id: u32, window: &tauri::WebviewWindow) {
    if let Ok(ns) = window.ns_window() {
        let n: isize =
            unsafe { objc2::msg_send![ns as *mut objc2::runtime::AnyObject, windowNumber] };
        if n > 0 {
            DRAG_PANEL_WIDS
                .lock_unpoisoned()
                .get_or_insert_with(HashMap::new)
                .insert(window_id, n as u32);
        }
    }
}

/// #761 event-driven border nudge (tap thread): position-only SLS move -- a
/// drag never resizes, so the CSS layout is untouched. Returns quietly when
/// this window has no live border.
#[cfg(target_os = "macos")]
pub(crate) fn drag_nudge_border(window_id: u32, x: f64, y: f64) {
    let wid = DRAG_PANEL_WIDS
        .lock_unpoisoned()
        .as_ref()
        .and_then(|m| m.get(&window_id).copied());
    if let Some(wid) = wid {
        let _ = crate::platform::sls::move_own_window(wid, x, y);
    }
}

#[cfg(target_os = "macos")]
fn update_share_border_window(
    app: &AppHandle,
    window: &tauri::WebviewWindow,
    handle: &BorderHandle,
    show_kind: ShowKind,
) -> i64 {
    let _ = window.set_ignore_cursor_events(true);
    crate::webview_transparency::apply_or_retry(app, window);
    set_share_border_frame(window, handle.frame);
    #[cfg(target_os = "macos")]
    cache_drag_panel_wid(handle.window_id, window);
    apply_share_border_treatment(window);
    apply_share_border_color(window, &handle.color);
    if should_reveal_via_eval(handle.source_kind, show_kind) {
        apply_share_border_reveal(window);
    }
    let _ = window.show();
    // Re-assert level + relative z-order on every idempotent show/reuse: a
    // reused panel may carry an older order, and a tracker-hidden panel needs
    // ordering-in to become visible again.
    order_share_border(window, handle.source_kind, handle.window_id)
}

#[cfg(target_os = "macos")]
fn set_share_border_frame(window: &tauri::WebviewWindow, frame: WindowFrame) {
    let layout = share_border_layout(frame);
    let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
        x: layout.panel.x as f64,
        y: layout.panel.y as f64,
    }));
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
        width: layout.panel.width.max(1) as f64,
        height: layout.panel.height.max(1) as f64,
    }));
    apply_share_border_layout(window, frame);
}

#[cfg(target_os = "macos")]
fn share_border_treatment_css() -> String {
    format!(
        ":root{{--share-border-stroke:{SCREENSHARE_BORDER_STROKE_PX}px;--share-border-radius:{SCREENSHARE_BORDER_RADIUS_PX}px;}}"
    )
}

/// The share-border page is intentionally tiny static Svelte. Keep this
/// issue's native ownership by injecting the treatment from the native owner
/// that creates/reuses the overlay panel.
#[cfg(target_os = "macos")]
fn apply_share_border_treatment(window: &tauri::WebviewWindow) {
    let css = share_border_treatment_css();
    let js = format!(
        r#"(() => {{
  const css = {css:?};
  const install = () => {{
    if (!document.head) return false;
    let style = document.getElementById('petal-share-border-treatment');
    if (!style) {{
      style = document.createElement('style');
      style.id = 'petal-share-border-treatment';
      document.head.appendChild(style);
    }}
    style.textContent = css;
    return true;
  }};
  if (install()) return;
  let attempts = 0;
  const timer = setInterval(() => {{
    attempts += 1;
    if (install() || attempts >= 20) clearInterval(timer);
  }}, 50);
}})();"#
    );
    if let Err(e) = window.eval(&js) {
        log::warn!(
            "share_border: failed to inject border treatment into '{}': {e}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn apply_share_border_color(window: &tauri::WebviewWindow, color: &str) {
    let js = format!(
        r#"(() => {{
  const color = {color:?};
  const apply = () => {{
    const root = document.documentElement;
    const shell = document.querySelector('.share-border-shell');
    if (!root || !shell) return false;
    root.style.setProperty('--share-color', color);
    shell.style.setProperty('--share-color', color);
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
            "share_border: failed to update border color for '{}': {e}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn apply_share_border_layout(window: &tauri::WebviewWindow, frame: WindowFrame) {
    let layout = share_border_layout(frame);
    let js = format!(
        r#"(() => {{
  const vars = {{
    '--share-border-top': '{}px',
    '--share-border-width': '{}px',
    '--share-border-height': '{}px'
  }};
  const apply = () => {{
    const root = document.documentElement;
    const shell = document.querySelector('.share-border-shell');
    if (!root || !shell) return false;
    for (const [name, value] of Object.entries(vars)) {{
      root.style.setProperty(name, value);
      shell.style.setProperty(name, value);
    }}
    return true;
  }};
  if (apply()) return;
  let attempts = 0;
  const timer = setInterval(() => {{
    attempts += 1;
    if (apply() || attempts >= 20) clearInterval(timer);
  }}, 50);
}})();"#,
        layout.border_top, layout.border_width, layout.border_height
    );
    if let Err(e) = window.eval(&js) {
        log::warn!(
            "share_border: failed to update border layout for '{}': {e}",
            window.label()
        );
    }
}

#[cfg(target_os = "macos")]
fn share_border_reveal_js() -> String {
    format!(
        r#"(() => {{
  window.dispatchEvent(new CustomEvent({:?}));
}})();"#,
        SHARE_BORDER_REVEAL_EVENT
    )
}

#[cfg(target_os = "macos")]
fn apply_share_border_reveal(window: &tauri::WebviewWindow) {
    let js = share_border_reveal_js();
    if let Err(e) = window.eval(&js) {
        log::warn!(
            "share_border: failed to replay border reveal for '{}': {e}",
            window.label()
        );
    }
}

#[cfg(not(target_os = "macos"))]
pub fn show_share_border(
    _app: &AppHandle,
    _window_id: u32,
    _frame: WindowFrame,
    _color: &str,
) -> u32 {
    NEXT_BORDER_ID.fetch_add(1, Ordering::SeqCst)
}

#[cfg(not(target_os = "macos"))]
pub fn show_share_border_for_source(
    app: &AppHandle,
    _source_kind: SharedSourceKind,
    window_id: u32,
    frame: WindowFrame,
    color: &str,
) -> u32 {
    show_share_border(app, window_id, frame, color)
}

/// Hide the border panel for `border_id` and retire it for reuse. Safe to
/// call if already hidden / unknown.
///
/// NEVER closes/destroys the panel — `window.close()` on one of these panels
/// aborts the app seconds later via a deferred-dealloc ObjC exception (see
/// the module doc; same crash class + same hide-and-retire fix as
/// `compositor::remove_window`).
pub fn hide_share_border(app: &AppHandle, border_id: u32) {
    let reservation = with_registry(|reg| reg.request_hide(border_id));
    match reservation.kind {
        HideKind::Unknown => {
            log::info!(
                "share_border: hide border {border_id} ignored; no active panel ({})",
                reservation.counts
            );
            return;
        }
        HideKind::AlreadyPending => {
            log::info!(
                "share_border: hide border {border_id} ignored; hide already pending for panel '{}' ({})",
                reservation.label.as_deref().unwrap_or("<unknown>"),
                reservation.counts
            );
            return;
        }
        HideKind::Hide => {}
    }

    #[cfg(target_os = "macos")]
    {
        // Hiding the NSWindow is AppKit work — must run on the main thread,
        // same as creation (see `show_share_border`). Unshare reaches here from
        // the async command's worker thread, so marshal the hide; the handle is
        // only retired after the hide actually ran (keeps a just-hidden panel
        // from being reused before it left the screen).
        log::info!(
            "share_border: hide border {border_id} begin (panel '{}', marshalling to main thread) ({})",
            reservation.label.as_deref().unwrap_or("<unknown>"),
            reservation.counts
        );
        let app_main = app.clone();
        if let Err(e) = app.run_on_main_thread(move || {
            complete_hide_share_border(&app_main, border_id);
        }) {
            let counts = with_registry(|reg| reg.undo_hide_request(border_id));
            log::error!("share_border: run_on_main_thread (hide) failed: {e} ({counts})");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        with_registry(|reg| {
            reg.take_hide_requested(border_id);
        });
    }
}

#[cfg(target_os = "macos")]
fn complete_hide_share_border(app_main: &AppHandle, border_id: u32) {
    use tauri::Manager;

    let Some(handle) = with_registry(|reg| reg.take_hide_requested(border_id)) else {
        let counts = with_registry(|reg| reg.counts());
        log::info!(
            "share_border: hide border {border_id} skipped; panel was re-shown before hide ran ({counts})"
        );
        return;
    };

    if !handle.realized {
        let counts = with_registry(|reg| reg.counts());
        log::info!(
            "share_border: pending panel '{}' for border {border_id} dropped before creation ({counts})",
            handle.label
        );
        return;
    }

    let label = handle.label.clone();
    if let Some(window) = app_main.get_webview_window(&label) {
        let _ = window.hide();
        let suffix = cg_window_id_suffix(&window);
        let counts = with_registry(|reg| {
            reg.retired.push(handle);
            reg.counts()
        });
        log::info!(
            "share_border: panel '{label}' hidden{suffix} -- retiring for reuse (never destroyed) ({counts})"
        );
    } else {
        let counts = with_registry(|reg| reg.counts());
        log::warn!(
            "share_border: panel '{label}' not found at hide time -- dropping stale handle (never destroyed) ({counts})"
        );
    }
}

/// Reposition an already-shown border to `frame` (e.g. if the shared window
/// moves or is resized).
///
/// Automatic move-tracking is handled by [`start_tracker`]'s poll (which
/// applies frame changes directly); this command remains for manual/frontend
/// use.
#[tauri::command]
pub fn update_share_border_frame(app: AppHandle, border_id: u32, frame: WindowFrame) {
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        let handle = with_registry(|reg| {
            reg.active.get_mut(&border_id).map(|h| {
                h.frame = frame;
                (h.label.clone(), h.window_id)
            })
        });
        if let Some((label, window_id)) = handle {
            // #761: while a rigid drag's event nudges own this window's border
            // position, a tracker write here is an OLDER sample (backwards
            // jump). Size/CSS never change mid-drag; the post-drag pass
            // re-syncs everything.
            #[cfg(target_os = "macos")]
            if crate::platform::gesture_tap::gesture_track_for(window_id, 40)
                .is_some_and(|t| t.rigid)
            {
                return;
            }
            if let Some(window) = app.get_webview_window(&label) {
                set_share_border_frame(&window, frame);
                #[cfg(target_os = "macos")]
                cache_drag_panel_wid(window_id, &window);
                crate::share_overlay::sync_frame_for_window_on_main(&app, window_id, frame);
                crate::remote_control::invalidate_control_frame(window_id);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, border_id, frame);
    }
}

/// Format a `, CGWindowID=<n>` suffix for panel-lifecycle log lines (issue
/// #13: include the CGWindowID where available, so a crash report's window
/// list / `screencapture -l<id>` can be correlated back to a specific panel).
/// Reads `NSWindow.windowNumber` -- the same `msg_send!` pattern
/// `compositor.rs`'s own CGWindowID log line already uses. MUST be called on
/// the main thread (all call sites above are inside `run_on_main_thread`
/// closures). Returns an empty string if the native handle isn't available,
/// so log lines degrade gracefully instead of failing.
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

/// Order `window` (a border panel) DIRECTLY above the shared window with
/// CGWindowID `shared_window_id` in the global z-stack, and return the
/// panel's own window number (== its CGWindowID) for the tracker's
/// stack-order checks. Returns 0 if the NSWindow handle is unavailable.
///
/// MUST be called on the main thread (AppKit — same rule as every other
/// NSWindow touch in this module). `-orderWindow:relativeTo:` takes a global
/// window number, so cross-app relative ordering works; it is a one-shot
/// placement (AppKit does not maintain the relationship), which is exactly
/// why [`start_tracker`] re-asserts it when stacking changes.
#[cfg(target_os = "macos")]
fn order_above_shared(window: &tauri::WebviewWindow, shared_window_id: u32) -> i64 {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(ns_ptr) = window.ns_window() else {
        log::warn!(
            "share_border: ns_window() unavailable for '{}'; cannot order relative to window {shared_window_id}",
            window.label()
        );
        return 0;
    };
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        // Re-assert NSNormalWindowLevel (0) — a reused panel may carry an
        // older level, and relative ordering only holds within a level tier.
        let _: () = msg_send![ns, setLevel: 0isize];
        // NSWindowAbove = 1.
        let _: () = msg_send![ns, orderWindow: 1isize, relativeTo: shared_window_id as isize];
        let number: i64 = msg_send![ns, windowNumber];
        number
    }
}

/// Order a full-display share border. The source id is synthetic, so using
/// `orderWindow:relativeTo:` would target no real window and the tracker would
/// later hide the panel (#199).
#[cfg(target_os = "macos")]
fn order_display_border(window: &tauri::WebviewWindow, display_source_id: u32) -> i64 {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(ns_ptr) = window.ns_window() else {
        log::warn!(
            "share_border: ns_window() unavailable for '{}'; cannot order display border {display_source_id}",
            window.label()
        );
        return 0;
    };
    unsafe {
        let ns = ns_ptr as *mut AnyObject;
        // NSStatusWindowLevel = 25. Display shares do not have a source window
        // z-tier, so the outline must stay visible at the display edge.
        let _: () = msg_send![ns, setLevel: 25isize];
        let _: () = msg_send![ns, orderFrontRegardless];
        let number: i64 = msg_send![ns, windowNumber];
        number
    }
}

#[cfg(target_os = "macos")]
fn order_share_border(
    window: &tauri::WebviewWindow,
    source_kind: SharedSourceKind,
    source_id: u32,
) -> i64 {
    match source_kind {
        SharedSourceKind::Window | SharedSourceKind::DisplayRegion => {
            order_above_shared(window, source_id)
        }
        SharedSourceKind::Display => order_display_border(window, source_id),
    }
}

/// Order a border panel out of the on-screen stack without touching tauri's
/// visibility bookkeeping (raw AppKit `orderOut:`). Main thread only.
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

/// What the tracker decided a border needs this tick. Pure data so the
/// decision logic ([`plan_border`]) is unit-testable without AppKit.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BorderAction {
    /// Re-assert `orderWindow:Above:relativeTo:` — the border is no longer
    /// directly in front of its shared window. For display shares this reorders
    /// the status-level display outline to the front. Also used to bring back a
    /// tracker-hidden border, since ordering a window in shows it.
    Reorder,
    /// The shared window moved/resized — move the border to this frame.
    SetFrame(WindowFrame),
    /// The shared window is not on-screen (minimized / other Space) — order
    /// the border out until it comes back.
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BorderStackEntry {
    number: i64,
    owner_pid: i64,
    frame: WindowFrame,
}

/// Project a CoreGraphics on-screen snapshot into the tracker's stack view.
///
/// Extracted as a seam (#742) so the projection can be driven from fixtures
/// instead of only from a live WindowServer. Two behaviours are deliberate and
/// PINNED by `border_stack_projection_*` tests -- a replacement window source
/// must reproduce them or change them knowingly:
/// - `layer` and `alpha` are DISCARDED; unlike hover_tab, this tracker treats
///   every on-screen entry as stack-relevant and relies on `plan_border` to
///   decide.
/// - bounds are TRUNCATED f64 -> i32 (`as`), not rounded, unlike
///   `cg::frame_for_window_id`, which rounds.
#[cfg(target_os = "macos")]
fn border_stack_from_entries(entries: Vec<cg::WindowEntry>) -> Vec<BorderStackEntry> {
    entries
        .into_iter()
        .map(|entry| BorderStackEntry {
            number: entry.number,
            owner_pid: entry.owner_pid,
            frame: WindowFrame {
                x: entry.x as i32,
                y: entry.y as i32,
                width: entry.w as i32,
                height: entry.h as i32,
            },
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QaWindowStackPosition {
    number: i64,
    stack_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QaShareBorderStackReport {
    window_id: u32,
    source: QaWindowStackPosition,
    border: Option<QaWindowStackPosition>,
    overlays: Vec<QaWindowStackPosition>,
}

fn qa_stack_position(stack: &[i64], number: i64) -> QaWindowStackPosition {
    QaWindowStackPosition {
        number,
        stack_index: stack.iter().position(|candidate| *candidate == number),
    }
}

/// QA/autotest-only WindowServer readback. The production tracker never calls
/// this path; it exists so #300 live validation can prove all three surfaces
/// from one front-to-back snapshot.
#[cfg(target_os = "macos")]
pub(crate) fn qa_share_border_stack_report(
    app: &AppHandle,
    window_id: u32,
) -> Result<QaShareBorderStackReport, String> {
    use tauri::Manager;

    let border_number = with_registry(|reg| {
        reg.active
            .values()
            .find(|handle| handle.window_id == window_id && !handle.hide_requested)
            .map(|handle| handle.panel_number)
            .filter(|number| *number > 0)
    });
    let overlay_labels = crate::share_overlay::overlay_labels_for_window(window_id);
    let app_main = app.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let numbers = overlay_labels
            .iter()
            .filter_map(|label| app_main.get_webview_window(label))
            .filter_map(|window| crate::platform::appkit::window_number(&window).ok())
            .map(i64::from)
            .collect::<Vec<_>>();
        let _ = tx.send(numbers);
    })
    .map_err(|error| format!("scheduling overlay window-number readback: {error}"))?;
    let overlay_numbers = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .map_err(|error| format!("waiting for overlay window-number readback: {error}"))?;
    let stack = cg::onscreen_windows_lean()
        .ok_or_else(|| "CGWindowListCopyWindowInfo returned no stack".to_string())?
        .into_iter()
        .map(|entry| entry.number)
        .collect::<Vec<_>>();

    let report = QaShareBorderStackReport {
        window_id,
        source: qa_stack_position(&stack, i64::from(window_id)),
        border: border_number.map(|number| qa_stack_position(&stack, number)),
        overlays: overlay_numbers
            .into_iter()
            .map(|number| qa_stack_position(&stack, number))
            .collect(),
    };
    log::info!("share-border-qa: WindowServer stack snapshot {report:?}");
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SharedWindowState {
    frame: WindowFrame,
    external_stack_index: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct BorderPlan {
    actions: Vec<BorderAction>,
    shared_state: Option<SharedWindowState>,
    suppressed_misorder_ticks: u8,
}

/// Four 100ms tracker ticks keeps the #19 flicker debounce but bounds the
/// #300 persistent-misorder failure to roughly 400ms.
const PERSISTENT_MISORDER_TICKS: u8 = 4;

/// Decide what a single border needs, given the current front-to-back
/// on-screen stack (`stack[0]` = frontmost).
///
/// Rules:
/// - window source absent from the stack → `Hide` (once; no-op if already
///   tracker-hidden).
/// - display source absent from the stack → no hide; display ids are
///   synthetic, not `CGWindow`s (#199).
/// - window source present but border hidden/absent → `Reorder` (ordering in
///   shows it directly above the shared window) + `SetFrame` (it may have
///   moved while the border was ordered out).
/// - both window source and border present: `Reorder` unless the border is
///   DIRECTLY in front of its source; `SetFrame` if the overlay panel frame
///   no longer matches the current source frame.
#[cfg(test)]
fn plan_border_actions(
    stack: &[BorderStackEntry],
    panel_number: i64,
    shared_number: i64,
    tracker_hidden: bool,
    self_pid: i64,
    previous_shared_state: Option<SharedWindowState>,
) -> Vec<BorderAction> {
    plan_border(
        stack,
        panel_number,
        shared_number,
        SharedSourceKind::Window,
        WindowFrame {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
        tracker_hidden,
        self_pid,
        previous_shared_state,
        0,
    )
    .actions
}

fn plan_border(
    stack: &[BorderStackEntry],
    panel_number: i64,
    shared_number: i64,
    source_kind: SharedSourceKind,
    source_frame: WindowFrame,
    tracker_hidden: bool,
    self_pid: i64,
    previous_shared_state: Option<SharedWindowState>,
    previous_suppressed_misorder_ticks: u8,
) -> BorderPlan {
    if matches!(
        source_kind,
        SharedSourceKind::Display | SharedSourceKind::DisplayRegion
    ) {
        return plan_display_border(stack, panel_number, source_frame, tracker_hidden, self_pid);
    }

    let relevant_stack: Vec<&BorderStackEntry> = stack
        .iter()
        .filter(|entry| {
            entry.number == panel_number
                || entry.number == shared_number
                || entry.owner_pid != self_pid
        })
        .collect();

    let shared = relevant_stack
        .iter()
        .position(|entry| entry.number == shared_number);
    let Some(shared_idx) = shared else {
        return BorderPlan {
            actions: if tracker_hidden {
                vec![]
            } else {
                vec![BorderAction::Hide]
            },
            shared_state: None,
            suppressed_misorder_ticks: 0,
        };
    };
    let external_shared_idx = stack
        .iter()
        .filter(|entry| entry.owner_pid != self_pid || entry.number == shared_number)
        .position(|entry| entry.number == shared_number)
        .unwrap_or(0);
    let shared_state = SharedWindowState {
        frame: relevant_stack[shared_idx].frame,
        external_stack_index: external_shared_idx,
    };
    let source_changed = previous_shared_state.is_none_or(|previous| previous != shared_state);

    let shared_frame = shared_state.frame;
    let expected_panel_frame = share_border_panel_frame(shared_frame);

    let panel = relevant_stack
        .iter()
        .position(|entry| entry.number == panel_number);
    let Some(panel_idx) = panel else {
        // Border hidden (by the tracker or otherwise) while its shared window
        // is on-screen: bring it back directly above the shared window.
        return BorderPlan {
            actions: vec![BorderAction::Reorder, BorderAction::SetFrame(shared_frame)],
            shared_state: Some(shared_state),
            suppressed_misorder_ticks: 0,
        };
    };

    let mut actions = Vec::new();
    // issue #19: activating Petal can raise the border panel without
    // moving/raising the shared source window. Debounce that panel-only
    // reshuffle by reordering too-front panels only after the source
    // frame/order changes. A panel behind its source is always wrong.
    // Front-to-back order: the border must sit at exactly shared_idx - 1.
    let mut suppressed_misorder_ticks = 0;
    if panel_idx + 1 != shared_idx {
        if source_changed || panel_idx > shared_idx {
            actions.push(BorderAction::Reorder);
        } else {
            suppressed_misorder_ticks = previous_suppressed_misorder_ticks.saturating_add(1);
            if suppressed_misorder_ticks >= PERSISTENT_MISORDER_TICKS {
                actions.push(BorderAction::Reorder);
                suppressed_misorder_ticks = 0;
            }
        }
    }
    if relevant_stack[panel_idx].frame != expected_panel_frame {
        actions.push(BorderAction::SetFrame(shared_frame));
    }
    BorderPlan {
        actions,
        shared_state: Some(shared_state),
        suppressed_misorder_ticks,
    }
}

fn plan_display_border(
    stack: &[BorderStackEntry],
    panel_number: i64,
    source_frame: WindowFrame,
    tracker_hidden: bool,
    self_pid: i64,
) -> BorderPlan {
    let panel = stack
        .iter()
        .find(|entry| entry.number == panel_number && entry.owner_pid == self_pid);
    let expected_panel_frame = share_border_panel_frame(source_frame);
    // A display has no CGWindow entry whose position can anchor this panel.
    // Only restore ordering when the panel is absent/ordered out; a correctly
    // placed display border is static and should not churn the WindowServer
    // stack every 100ms (#199).
    let mut actions = Vec::new();

    match panel {
        Some(panel) => {
            if tracker_hidden {
                actions.push(BorderAction::Reorder);
            }
            if panel.frame != expected_panel_frame {
                actions.push(BorderAction::SetFrame(source_frame));
            }
        }
        None => {
            actions.push(BorderAction::Reorder);
            actions.push(BorderAction::SetFrame(source_frame));
        }
    }

    BorderPlan {
        actions,
        shared_state: None,
        suppressed_misorder_ticks: 0,
    }
}

/// Start the border z-order/move tracker: a light background poll (~10Hz)
/// that snapshots `CGWindowListCopyWindowInfo` (front-to-back order IS the
/// z-order) and, per active border, re-asserts relative ordering / frame /
/// visibility via [`plan_border`]. Standalone loop, deliberately NOT
/// piggybacked on `hover_tab`'s tracker (that loop is owned by a different
/// concern and runs at a different cadence for a different purpose). All
/// AppKit work is marshaled to the main thread; the CGWindowList snapshot
/// itself is thread-safe and stays on the poll thread. Cheap when no borders
/// are active (registry check only, no window-list enumeration).
#[cfg(target_os = "macos")]
pub fn start_tracker(app: &AppHandle) {
    const POLL_MS: u64 = 100;
    let app = app.clone();
    let wake = Arc::new((Mutex::new(false), Condvar::new()));
    let _ = TRACKER_WAKE.set(wake.clone());
    let observer_app = app.clone();
    crate::platform::on_main(
        &app,
        "share-border: register activation observer",
        move || register_activation_observer(observer_app),
    );
    std::thread::spawn(move || loop {
        let (lock, condvar) = &*wake;
        let mut signalled = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*signalled {
            signalled = condvar
                .wait_timeout(signalled, std::time::Duration::from_millis(POLL_MS))
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
        *signalled = false;
        drop(signalled);

        // #872: an orphaned sharer overlay usually has no border left, so this
        // MUST run before the `borders.is_empty()` early-out below -- otherwise
        // the watchdog misses exactly the case it exists for (an overlay eating
        // clicks across the user's whole desktop after its share ended).
        crate::share_overlay::enforce_click_through_without_publication(&app);

        // (border_id, panel_number, source id, source kind, source frame,
        // tracker_hidden, last shared state, suppressed misorder ticks)
        let borders: Vec<(
            u32,
            i64,
            i64,
            SharedSourceKind,
            WindowFrame,
            bool,
            Option<SharedWindowState>,
            u8,
        )> = with_registry(|reg| {
            reg.active
                .iter()
                .filter(|(_, h)| h.panel_number != 0 && !h.hide_requested)
                .map(|(&id, h)| {
                    (
                        id,
                        h.panel_number,
                        h.window_id as i64,
                        h.source_kind,
                        h.frame,
                        h.tracker_hidden,
                        h.last_shared_state,
                        h.suppressed_misorder_ticks,
                    )
                })
                .collect()
        });
        if borders.is_empty() {
            continue;
        }

        // #743: the tracker reads only number/pid/frame, never names.
        // #744: read the shared registry snapshot. Records are already the raw
        // window rows this projection needs (number, owner_pid, truncated
        // frame); the border_stack_from_entries characterization + the
        // border_stack_from_registry_matches_projection test pin the parity.
        // Negative "window numbers" (parse-failure fallbacks, never real
        // windows) are absent from the registry by construction, which is
        // strictly more correct. Fall back to a direct enumeration only before
        // the registry global is set.
        let Some(stack) = crate::window_registry::global()
            .map(|reg| {
                reg.snapshot()
                    .records_front_to_back()
                    .map(|r| BorderStackEntry {
                        number: r.wid as i64,
                        owner_pid: r.owner_pid as i64,
                        frame: r.frame,
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|s| !s.is_empty())
            .or_else(|| cg::onscreen_windows_lean().map(border_stack_from_entries))
        else {
            continue;
        };
        let self_pid = std::process::id() as i64;

        let mut plans: Vec<(u32, Vec<BorderAction>)> = Vec::new();
        let mut tracker_state_updates: Vec<(u32, Option<SharedWindowState>, u8)> = Vec::new();
        for (
            border_id,
            panel_number,
            shared_number,
            source_kind,
            source_frame,
            tracker_hidden,
            last_shared_state,
            suppressed_misorder_ticks,
        ) in borders
        {
            let source_frame = if source_kind == SharedSourceKind::Display {
                display_frame_for_source_id(shared_number as u32, source_frame)
            } else {
                source_frame
            };
            let plan = plan_border(
                &stack,
                panel_number,
                shared_number,
                source_kind,
                source_frame,
                tracker_hidden,
                self_pid,
                last_shared_state,
                suppressed_misorder_ticks,
            );
            if plan.shared_state != last_shared_state
                || plan.suppressed_misorder_ticks != suppressed_misorder_ticks
            {
                tracker_state_updates.push((
                    border_id,
                    plan.shared_state,
                    plan.suppressed_misorder_ticks,
                ));
            }
            if !plan.actions.is_empty() {
                plans.push((border_id, plan.actions));
            }
        }
        if !tracker_state_updates.is_empty() {
            with_registry(|reg| {
                for (border_id, shared_state, suppressed_misorder_ticks) in tracker_state_updates {
                    if let Some(handle) = reg.active.get_mut(&border_id) {
                        handle.last_shared_state = shared_state;
                        handle.suppressed_misorder_ticks = suppressed_misorder_ticks;
                    }
                }
            });
        }
        if plans.is_empty() {
            continue;
        }

        let app_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            use tauri::Manager;
            for (border_id, actions) in plans {
                // Re-check under the lock: the border may have been retired
                // (unshare) between the snapshot and this main-thread hop.
                let handle = with_registry(|reg| {
                    reg.active.get(&border_id).and_then(|h| {
                        (!h.hide_requested).then(|| (h.label.clone(), h.window_id, h.source_kind))
                    })
                });
                let Some((label, window_id, source_kind)) = handle else {
                    continue;
                };
                let Some(window) = app_main.get_webview_window(&label) else {
                    continue;
                };
                for action in actions {
                    match action {
                        BorderAction::Reorder => {
                            let qa_logging = std::env::var_os("PETAL_AUTOTEST_SOCK").is_some();
                            let overlay_labels = qa_logging
                                .then(|| crate::share_overlay::overlay_labels_for_window(window_id))
                                .unwrap_or_default();
                            let panel_number = order_share_border(&window, source_kind, window_id);
                            crate::share_overlay::order_above_shared_for_window_on_main(
                                &app_main, window_id,
                            );
                            crate::ai_chat::panel::raise_ai_chat_panel_if_active_on_main(
                                &app_main, window_id,
                            );
                            if qa_logging {
                                log::info!(
                                    "share-border-qa: reorder executed border_id={border_id} source_window={window_id} border_panel={panel_number} overlay_panels_invoked={} overlay_labels={overlay_labels:?} result={}",
                                    overlay_labels.len(),
                                    if panel_number > 0 {
                                        "border-ordered"
                                    } else {
                                        "border-window-unavailable"
                                    }
                                );
                            }
                            with_registry(|reg| {
                                if let Some(h) = reg.active.get_mut(&border_id) {
                                    h.panel_number = panel_number;
                                    h.tracker_hidden = false;
                                }
                            });
                        }
                        BorderAction::SetFrame(frame) => {
                            set_share_border_frame(&window, frame);
                            crate::share_overlay::sync_frame_for_window_on_main(
                                &app_main, window_id, frame,
                            );
                            crate::remote_control::invalidate_control_frame(window_id);
                            with_registry(|reg| {
                                if let Some(h) = reg.active.get_mut(&border_id) {
                                    h.frame = frame;
                                }
                            });
                        }
                        BorderAction::Hide => {
                            crate::remote_control::invalidate_control_frame(window_id);
                            order_out(&window);
                            crate::share_overlay::order_out_for_window_on_main(
                                &app_main, window_id,
                            );
                            with_registry(|reg| {
                                if let Some(h) = reg.active.get_mut(&border_id) {
                                    h.tracker_hidden = true;
                                }
                            });
                        }
                    }
                }
            }
        });
    });
}

/// The activation notification is only a latency optimization. The existing
/// tracker remains authoritative; waking it here makes the notification run
/// the exact same snapshot/plan/action path as the normal 100ms poll.
#[cfg(target_os = "macos")]
pub(crate) fn reassert_for_activation() -> bool {
    if !activation_reassert_enabled(has_active_borders()) {
        return false;
    }
    if let Some(wake) = TRACKER_WAKE.get() {
        let (lock, condvar) = &**wake;
        let mut signalled = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *signalled = true;
        condvar.notify_one();
    }
    true
}

#[cfg(target_os = "macos")]
pub(crate) fn has_active_share() -> bool {
    has_active_borders()
}

fn activation_reassert_enabled(has_active_share: bool) -> bool {
    has_active_share
}

#[cfg(target_os = "macos")]
fn has_active_borders() -> bool {
    with_registry(|reg| reg.active.values().any(|handle| !handle.hide_requested))
}

#[cfg(target_os = "macos")]
fn register_activation_observer(app: AppHandle) {
    use objc2_app_kit::{NSWorkspace, NSWorkspaceDidActivateApplicationNotification};
    use objc2_foundation::NSNotification;

    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let observer = block2::RcBlock::new(move |_note: std::ptr::NonNull<NSNotification>| {
        // These checks are deliberately before any window-list work: app
        // activation must be a no-op when no share is active (#465).
        let local_share_active = has_active_share();
        let remote_share_active = crate::compositor::has_active_remote_windows();
        if !local_share_active && !remote_share_active {
            return;
        }
        if local_share_active {
            let _ = reassert_for_activation();
        }
        if !remote_share_active {
            return;
        }
        let app_for_main = app.clone();
        let app_for_closure = app_for_main.clone();
        let _ = app_for_main.run_on_main_thread(move || {
            crate::compositor::reassert_active_chrome_on_main(&app_for_closure);
        });
    });
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidActivateApplicationNotification),
            None,
            None,
            &observer,
        )
    };
    std::mem::forget(token);
}

#[cfg(not(target_os = "macos"))]
pub fn start_tracker(_app: &AppHandle) {}

#[cfg(target_os = "macos")]
use crate::platform::cg;

/// Percent-encode a color string for a URL query param, via the shared
/// `percent-encoding` crate (same as compositor.rs).
#[cfg(target_os = "macos")]
fn urlencoding_encode(s: &str) -> String {
    // Shared `percent-encoding` crate (already a dep, used by compositor.rs) --
    // no hand-rolled `#`-only encoder (#143). NON_ALPHANUMERIC over-encodes
    // safely; the webview's URLSearchParams decodes it back verbatim.
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn cg_entry(number: i64, pid: i64, x: f64, y: f64, w: f64, h: f64) -> cg::WindowEntry {
        cg::WindowEntry {
            number,
            owner_pid: pid,
            owner_name: "Owner".to_string(),
            name: "Title".to_string(),
            layer: 0,
            alpha: 1.0,
            x,
            y,
            w,
            h,
        }
    }

    /// CHARACTERIZATION (#742). Pins today's projection so the window-registry
    /// migration cannot change it silently. Both behaviours below are real and
    /// currently intentional-by-accident; if either should change, that is a
    /// deliberate, documented change, not a drive-by.
    #[cfg(target_os = "macos")]
    #[test]
    fn border_stack_projection_preserves_order_number_and_pid() {
        let stack = border_stack_from_entries(vec![
            cg_entry(1, 100, 0.0, 0.0, 800.0, 600.0),
            cg_entry(2, 200, 10.0, 20.0, 300.0, 400.0),
        ]);
        assert_eq!(stack.len(), 2);
        // front-to-back order is the CG array order and must be preserved:
        // plan_border compares stack INDEXES to decide z-order reasserts.
        assert_eq!(stack[0].number, 1);
        assert_eq!(stack[1].number, 2);
        assert_eq!(stack[1].owner_pid, 200);
        assert_eq!(
            stack[1].frame,
            WindowFrame {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            }
        );
    }

    /// The border tracker keeps NON-layer-0 and transparent windows, unlike
    /// hover_tab's hit-test which filters them out. A registry that applies one
    /// global "is this a real window" filter would silently change border
    /// z-order behaviour.
    #[cfg(target_os = "macos")]
    #[test]
    fn border_stack_projection_keeps_overlay_and_transparent_windows() {
        let mut overlay = cg_entry(9, 300, 0.0, 0.0, 100.0, 100.0);
        overlay.layer = 25;
        overlay.alpha = 0.0;
        let stack = border_stack_from_entries(vec![overlay]);
        assert_eq!(
            stack.len(),
            1,
            "layer/alpha are discarded by this projection; filtering happens in plan_border"
        );
        assert_eq!(stack[0].number, 9);
    }

    /// Bounds are TRUNCATED (`as i32`), not rounded -- unlike
    /// `cg::frame_for_window_id`, which rounds. A registry that normalises on
    /// rounding would shift borders by a pixel on fractional-scale displays.
    #[cfg(target_os = "macos")]
    #[test]
    fn border_stack_projection_truncates_fractional_bounds_it_does_not_round() {
        let stack = border_stack_from_entries(vec![cg_entry(1, 100, 10.9, -3.9, 100.7, 200.99)]);
        assert_eq!(
            stack[0].frame,
            WindowFrame {
                x: 10,
                y: -3,
                width: 100,
                height: 200
            },
            "truncation toward zero, not rounding"
        );
    }

    /// GOLDEN REPLAY (#742, plan §7.1): drive the real projection with a
    /// recorded live-session fixture and pin the full decision sequence.
    /// The registry (#744) must pass this SAME test via fixture ingest.
    /// A compact digest per frame keeps the golden reviewable while still
    /// pinning every field of every entry.
    #[cfg(target_os = "macos")]
    #[test]
    fn border_stack_projection_matches_golden_over_recorded_session() {
        for fixture in crate::window_fixtures::REPLAY_FIXTURES {
            border_stack_golden_one(fixture);
        }
    }

    /// GOLDEN TRANSFER (#744): the registry snapshot must reproduce the border
    /// stack that `border_stack_from_entries` produces, so migrating the tracker
    /// to read the registry changes nothing for real windows. Negative window
    /// numbers (parse-failure fallbacks) are absent from the registry by
    /// construction; the direct stack is filtered to match, since those are not
    /// real windows.
    #[cfg(target_os = "macos")]
    #[test]
    fn border_stack_from_registry_matches_projection() {
        use crate::window_registry::{OwnChromeOracle, WindowRegistry};
        struct Foreign;
        impl OwnChromeOracle for Foreign {
            fn is_decorative(&self, _: &str) -> bool {
                false
            }
        }
        for fixture_name in crate::window_fixtures::REPLAY_FIXTURES {
            let fixture = crate::window_fixtures::load(
                &crate::window_fixtures::fixtures_dir().join(format!("{fixture_name}.jsonl")),
            );
            for f in &fixture {
                let entries: Vec<cg::WindowEntry> =
                    f.windows.iter().map(|w| w.to_entry()).collect();
                let direct: Vec<BorderStackEntry> = border_stack_from_entries(entries)
                    .into_iter()
                    .filter(|e| e.number >= 0)
                    .collect();
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
                reg.ingest_rows(&rows, 999, &Foreign);
                let via_registry: Vec<BorderStackEntry> = reg
                    .snapshot()
                    .records_front_to_back()
                    .map(|r| BorderStackEntry {
                        number: r.wid as i64,
                        owner_pid: r.owner_pid as i64,
                        frame: r.frame,
                    })
                    .collect();
                assert_eq!(
                    via_registry, direct,
                    "registry border stack diverges from projection for {fixture_name}"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn border_stack_golden_one(fixture: &str) {
        let frames = crate::window_fixtures::load(
            &crate::window_fixtures::fixtures_dir().join(format!("{fixture}.jsonl")),
        );
        assert!(frames.len() >= 10, "fixture {fixture} too short");
        #[derive(serde::Serialize)]
        struct FrameDigest {
            t_ms: u64,
            len: usize,
            front3: Vec<(i64, i64, i32, i32, i32, i32)>,
            fnv: u64,
        }
        let digests = frames
            .iter()
            .map(|f| {
                let entries = f.windows.iter().map(|w| w.to_entry()).collect::<Vec<_>>();
                let stack = border_stack_from_entries(entries);
                let mut fnv: u64 = 0xcbf29ce484222325;
                for e in &stack {
                    for v in [
                        e.number,
                        e.owner_pid,
                        e.frame.x as i64,
                        e.frame.y as i64,
                        e.frame.width as i64,
                        e.frame.height as i64,
                    ] {
                        fnv ^= v as u64;
                        fnv = fnv.wrapping_mul(0x100000001b3);
                    }
                }
                FrameDigest {
                    t_ms: f.t_ms,
                    len: stack.len(),
                    front3: stack
                        .iter()
                        .take(3)
                        .map(|e| {
                            (
                                e.number,
                                e.owner_pid,
                                e.frame.x,
                                e.frame.y,
                                e.frame.width,
                                e.frame.height,
                            )
                        })
                        .collect(),
                    fnv,
                }
            })
            .collect::<Vec<_>>();
        crate::window_fixtures::assert_golden(&format!("border-stack.{fixture}"), &digests);
    }

    #[test]
    fn activation_reassert_is_a_noop_without_an_active_share() {
        assert!(!activation_reassert_enabled(false));
    }

    #[test]
    fn activation_reassert_is_enabled_for_an_active_share() {
        assert!(activation_reassert_enabled(true));
    }

    #[test]
    fn border_label_is_stable_and_unique_per_id() {
        assert_eq!(border_label(1), "share_border_1");
        assert_eq!(border_label(42), "share_border_42");
        assert_ne!(border_label(1), border_label(2));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn share_border_treatment_is_one_px_thicker_and_rounded() {
        assert_eq!(SCREENSHARE_BORDER_STROKE_PX, 4.0);
        assert_eq!(SCREENSHARE_BORDER_RADIUS_PX, 10.0);
        let css = share_border_treatment_css();
        assert!(css.contains("--share-border-stroke:4px"));
        assert!(css.contains("--share-border-radius:10px"));
        assert!(!css.contains("--share-tab"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn share_border_url_has_no_legacy_top_tab_anchor() {
        let url = share_border_url("#f06cc9", f(40, 200, 180, 100), true);

        assert!(url.starts_with(
            "share-border.html?color=%23f06cc9&borderTop=0&windowWidth=180&windowHeight=100"
        ));
        assert!(!url.contains("tabAnchorX="));
        assert!(url.ends_with("&animate=1"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn share_border_url_omits_reveal_params_when_not_requested() {
        let url = share_border_url("#84d2ff", f(0, 0, 1512, 982), false);

        assert!(url.contains("color=%2384d2ff"));
        assert!(!url.contains("animate=1"));
        assert!(!url.contains("tabAnchorX="));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn share_border_reveal_routing_tracks_show_kind_and_source_kind() {
        assert!(should_reveal_via_url(
            SharedSourceKind::Window,
            ShowKind::CreateFresh,
            false
        ));
        assert!(!should_reveal_via_eval(
            SharedSourceKind::Window,
            ShowKind::CreateFresh
        ));

        assert!(!should_reveal_via_url(
            SharedSourceKind::Window,
            ShowKind::ReuseRetired,
            false
        ));
        assert!(should_reveal_via_eval(
            SharedSourceKind::Window,
            ShowKind::ReuseRetired
        ));

        assert!(!should_reveal_via_url(
            SharedSourceKind::Window,
            ShowKind::UpdateExisting,
            false
        ));
        assert!(!should_reveal_via_eval(
            SharedSourceKind::Window,
            ShowKind::UpdateExisting
        ));

        assert!(should_reveal_via_url(
            SharedSourceKind::Window,
            ShowKind::UpdateExisting,
            true
        ));
        assert!(!should_reveal_via_url(
            SharedSourceKind::Display,
            ShowKind::CreateFresh,
            true
        ));
        assert!(!should_reveal_via_eval(
            SharedSourceKind::Display,
            ShowKind::ReuseRetired
        ));
    }

    #[test]
    fn fresh_panel_build_skips_layout_and_color_evals_reuse_path_keeps_them() {
        // Source-level pin (issue #680): a fresh CreateFresh panel must not
        // eval() layout/color into a still-launching WebContent process --
        // `share_border_url` already encodes both, and share-border's
        // +page.svelte applies them from `page.url.searchParams` on first
        // render. The reuse path patches a stale EXISTING page and must keep
        // both evals unconditionally.
        let source = include_str!("share_border.rs");

        let realize_fn = source
            .split_once("fn realize_share_border(")
            .and_then(|(_, rest)| rest.split_once("fn update_share_border_window("))
            .map(|(fresh_build_body, _)| fresh_build_body)
            .expect("realize_share_border must precede update_share_border_window");
        assert!(
            !realize_fn.contains("apply_share_border_layout("),
            "fresh-build path must not re-eval layout already encoded in the URL"
        );
        assert!(
            !realize_fn.contains("apply_share_border_color("),
            "fresh-build path must not re-eval color already encoded in the URL"
        );
        // The treatment eval (border stroke/radius CSS custom props) is kept:
        // those values are compile-time constants, never URL-encoded, so this
        // task's URL-redundancy argument doesn't cover it.
        assert!(
            realize_fn.contains("apply_share_border_treatment("),
            "fresh-build path must still install the border treatment stylesheet"
        );

        let update_fn = source
            .split_once("fn update_share_border_window(")
            .and_then(|(_, rest)| rest.split_once("fn set_share_border_frame("))
            .map(|(reuse_body, _)| reuse_body)
            .expect("update_share_border_window must precede set_share_border_frame");
        assert!(
            // update_share_border_window calls apply_share_border_layout
            // indirectly via set_share_border_frame, not inline -- check for
            // that call site rather than the eval function's own name.
            update_fn.contains("set_share_border_frame("),
            "reuse path must keep patching layout on an existing, possibly stale page"
        );
        assert!(
            update_fn.contains("apply_share_border_color("),
            "reuse path must keep patching color on an existing, possibly stale page"
        );
    }

    #[test]
    fn prewarm_directly_manufactures_hidden_realized_retired_panels() {
        let source = include_str!("share_border.rs");
        let prewarm = source
            .split_once("fn prewarm_share_borders_on_main(")
            .and_then(|(_, rest)| rest.split_once("fn build_share_border_panel("))
            .map(|(body, _)| body)
            .expect("border prewarm must precede the shared panel builder");
        assert!(prewarm.contains("crate::session::MAX_CONCURRENT_SHARES"));
        assert!(prewarm.contains("realized: true"));
        assert!(prewarm.contains("reg.retired.push(handle)"));

        let builder = source
            .split_once("fn build_share_border_panel(")
            .and_then(|(_, rest)| rest.split_once("fn realize_share_border("))
            .map(|(body, _)| body)
            .expect("shared border builder must precede realization");
        assert!(builder.contains(".visible(false)"));
        assert!(!builder.contains(".show();"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn share_border_reveal_eval_dispatches_custom_event() {
        let js = share_border_reveal_js();

        assert!(js.contains("CustomEvent(\"petal-share-border-reveal\")"));
        assert!(js.contains("window.dispatchEvent"));
    }

    fn f(x: i32, y: i32, w: i32, h: i32) -> WindowFrame {
        WindowFrame {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn pf(frame: WindowFrame) -> WindowFrame {
        share_border_panel_frame(frame)
    }

    #[test]
    fn qa_stack_position_uses_front_to_back_index_and_marks_absent_windows() {
        let stack = [700, 42, 900, 12];
        assert_eq!(
            qa_stack_position(&stack, 42),
            QaWindowStackPosition {
                number: 42,
                stack_index: Some(1),
            }
        );
        assert_eq!(qa_stack_position(&stack, 77).stack_index, None);
    }

    #[test]
    fn share_border_layout_frames_exact_source_window_without_tab_band() {
        let layout = share_border_layout(f(40, 200, 180, 100));
        assert_eq!(layout.panel, f(40, 200, 180, 100));
        assert_eq!(layout.border_top, 0.0);
        assert_eq!(layout.border_width, 180.0);
        assert_eq!(layout.border_height, 100.0);
    }

    #[test]
    fn share_border_layout_minimizes_only_degenerate_source_dimensions() {
        let layout = share_border_layout(f(40, 200, 0, -10));
        assert_eq!(layout.panel, f(40, 200, 1, 1));
        assert_eq!(layout.border_width, 1.0);
        assert_eq!(layout.border_height, 1.0);
    }

    fn handle(label: &str, color: &str, window_id: u32, realized: bool) -> BorderHandle {
        BorderHandle {
            label: label.to_string(),
            color: color.to_string(),
            source_kind: SharedSourceKind::Window,
            window_id,
            frame: f(10, 10, 300, 200),
            panel_number: if realized { 100 } else { 0 },
            tracker_hidden: false,
            realized,
            hide_requested: false,
            last_shared_state: None,
            suppressed_misorder_ticks: 0,
        }
    }

    #[test]
    fn reserve_show_is_idempotent_per_window() {
        let mut reg = Registry::default();

        let first = reg.reserve_show(
            SharedSourceKind::Window,
            77,
            f(10, 10, 300, 200),
            "#f06cc9".to_string(),
            || 1,
        );
        assert_eq!(first.border_id, 1);
        assert_eq!(first.kind, ShowKind::CreateFresh);
        assert_eq!(reg.active_count_for_window(77), 1);

        let updated_frame = f(30, 40, 500, 360);
        let second = reg.reserve_show(
            SharedSourceKind::Window,
            77,
            updated_frame,
            "#84d2ff".to_string(),
            || panic!("idempotent show must not allocate a second border id"),
        );
        assert_eq!(second.border_id, 1);
        assert_eq!(second.kind, ShowKind::UpdateExisting);
        assert_eq!(reg.active_count_for_window(77), 1);
        assert_eq!(reg.active.len(), 1);

        let active = reg.active.get(&1).unwrap();
        assert_eq!(active.frame, updated_frame);
        assert_eq!(active.color, "#84d2ff");
        assert!(!active.hide_requested);
    }

    #[test]
    fn reserve_show_reuses_retired_panel_and_recolors_it() {
        let mut reg = Registry::default();
        reg.retired
            .push(handle("share_border_1", "#f06cc9", 0, true));

        let reservation = reg.reserve_show(
            SharedSourceKind::Display,
            88,
            f(20, 30, 640, 480),
            "#84d2ff".to_string(),
            || 2,
        );
        assert_eq!(reservation.border_id, 2);
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
        assert_eq!(active.label, "share_border_1");
        assert_eq!(active.window_id, 88);
        assert_eq!(active.source_kind, SharedSourceKind::Display);
        assert_eq!(active.color, "#84d2ff");
        assert!(active.realized);
    }

    #[test]
    fn reserve_show_cancels_pending_hide_for_same_window() {
        let mut reg = Registry::default();
        reg.active
            .insert(1, handle("share_border_1", "#f06cc9", 77, true));

        let hide = reg.request_hide(1);
        assert_eq!(hide.kind, HideKind::Hide);
        assert!(reg.active.get(&1).unwrap().hide_requested);

        let show = reg.reserve_show(
            SharedSourceKind::Window,
            77,
            f(50, 60, 320, 240),
            "#f06cc9".to_string(),
            || panic!("re-show during pending hide must reuse the same active border"),
        );
        assert_eq!(show.border_id, 1);
        assert_eq!(show.kind, ShowKind::UpdateExisting);
        assert_eq!(reg.active_count_for_window(77), 1);
        assert!(!reg.active.get(&1).unwrap().hide_requested);
        assert!(reg.take_hide_requested(1).is_none());
    }

    #[test]
    fn pending_show_hidden_before_realization_is_dropped_not_retired() {
        let mut reg = Registry::default();
        reg.reserve_show(
            SharedSourceKind::Window,
            77,
            f(10, 10, 300, 200),
            "#f06cc9".to_string(),
            || 1,
        );
        assert_eq!(
            reg.counts(),
            RegistryCounts {
                live: 0,
                pending: 1,
                retired: 0,
            }
        );

        reg.request_hide(1);
        let pending = reg.take_hide_requested(1).unwrap();
        assert!(!pending.realized);
        assert_eq!(
            reg.counts(),
            RegistryCounts {
                live: 0,
                pending: 0,
                retired: 0,
            }
        );
    }

    const PANEL: i64 = 100;
    const SHARED: i64 = 200;
    const OTHER: i64 = 300;
    const PETAL_WINDOW: i64 = 400;
    const SELF_PID: i64 = 10;
    const OTHER_PID: i64 = 20;

    fn s(number: i64, owner_pid: i64, frame: WindowFrame) -> BorderStackEntry {
        BorderStackEntry {
            number,
            owner_pid,
            frame,
        }
    }

    fn source_state(frame: WindowFrame, external_stack_index: usize) -> SharedWindowState {
        SharedWindowState {
            frame,
            external_stack_index,
        }
    }

    #[test]
    fn no_actions_when_border_directly_above_shared_with_matching_frame() {
        // Front-to-back: other app frontmost, then border, then shared.
        let shared = f(10, 10, 300, 200);
        let stack = vec![
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(PANEL, SELF_PID, pf(shared)),
            s(SHARED, OTHER_PID, shared),
        ];
        assert!(plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None).is_empty());
    }

    #[test]
    fn ignores_petals_own_windows_between_border_and_shared() {
        let shared = f(10, 10, 300, 200);
        let stack = vec![
            s(PANEL, SELF_PID, pf(shared)),
            s(PETAL_WINDOW, SELF_PID, f(60, 60, 360, 260)),
            s(SHARED, OTHER_PID, shared),
        ];
        assert!(plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None).is_empty());
    }

    #[test]
    fn reorders_when_another_window_sits_between_border_and_shared() {
        // The failure mode from issue #23: window B stacked over the
        // shared window, but the border still painting above B.
        let shared = f(10, 10, 300, 200);
        let stack = vec![
            s(PANEL, SELF_PID, pf(shared)),
            s(PETAL_WINDOW, SELF_PID, f(60, 60, 360, 260)),
            s(OTHER, OTHER_PID, f(50, 50, 400, 400)), // B, covering the shared window
            s(SHARED, OTHER_PID, shared),
        ];
        assert_eq!(
            plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None),
            vec![BorderAction::Reorder]
        );
    }

    #[test]
    fn records_shared_source_state_for_later_debounce_ticks() {
        let shared = f(10, 10, 300, 200);
        let stack = vec![
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(PANEL, SELF_PID, pf(shared)),
            s(SHARED, OTHER_PID, shared),
        ];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Window,
            shared,
            false,
            SELF_PID,
            None,
            0,
        );
        assert!(plan.actions.is_empty());
        assert_eq!(plan.shared_state, Some(source_state(shared, 1)));
    }

    #[test]
    fn debounces_panel_only_activation_reshuffle_when_source_state_unchanged() {
        let shared = f(10, 10, 300, 200);
        let previous = source_state(shared, 1);
        // Petal activation can raise only the border panel to the front. The
        // non-Petal stack still has OTHER above SHARED, so the source itself
        // did not move/raise and the tracker should avoid a visible reorder.
        let stack = vec![
            s(PANEL, SELF_PID, pf(shared)),
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(SHARED, OTHER_PID, shared),
        ];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Window,
            shared,
            false,
            SELF_PID,
            Some(previous),
            1,
        );
        assert!(plan.actions.is_empty());
        assert_eq!(plan.shared_state, Some(previous));
        assert_eq!(plan.suppressed_misorder_ticks, 2);
    }

    #[test]
    fn reorders_after_persistent_panel_only_activation_reshuffle() {
        let shared = f(10, 10, 300, 200);
        let previous = source_state(shared, 1);
        let stack = vec![
            s(PANEL, SELF_PID, pf(shared)),
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(SHARED, OTHER_PID, shared),
        ];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Window,
            shared,
            false,
            SELF_PID,
            Some(previous),
            PERSISTENT_MISORDER_TICKS - 1,
        );
        assert_eq!(plan.actions, vec![BorderAction::Reorder]);
        assert_eq!(plan.shared_state, Some(previous));
        assert_eq!(plan.suppressed_misorder_ticks, 0);
    }

    /// #465 measurement, driving the tracker's REAL decision function over a
    /// sequence of woken ticks rather than a pure boolean helper.
    ///
    /// #465 made the tracker wake immediately on
    /// `NSWorkspaceDidActivateApplicationNotification` instead of waiting for
    /// the next 100ms poll. This asserts what that early wake actually buys on
    /// the activation path it was aimed at -- and it is NOT an immediate
    /// reorder: `PERSISTENT_MISORDER_TICKS` is counted in TICKS, not elapsed
    /// time, so a woken tick just burns one of the four.
    ///
    /// Arm B is the positive control: the same harness and stack helpers, with
    /// the panel BEHIND its source, must reorder on the very first tick.
    /// Without it, arm A's "no Reorder" readings would be uninterpretable.
    #[test]
    fn woken_activation_ticks_do_not_bypass_the_persistent_misorder_debounce() {
        let shared = f(10, 10, 300, 200);
        let previous = source_state(shared, 1);

        // Arm A: Petal activation raised only the border panel. The non-Petal
        // stack is unchanged, so `source_changed` stays false tick after tick.
        let too_front = vec![
            s(PANEL, SELF_PID, pf(shared)),
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(SHARED, OTHER_PID, shared),
        ];
        let mut ticks = 0u8;
        let mut first_reorder_tick = None;
        for tick in 1..=6u32 {
            let plan = plan_border(
                &too_front,
                PANEL,
                SHARED,
                SharedSourceKind::Window,
                shared,
                false,
                SELF_PID,
                Some(previous),
                ticks,
            );
            ticks = plan.suppressed_misorder_ticks;
            if plan.actions.contains(&BorderAction::Reorder) && first_reorder_tick.is_none() {
                first_reorder_tick = Some(tick);
            }
        }
        assert_eq!(
            first_reorder_tick,
            Some(u32::from(PERSISTENT_MISORDER_TICKS)),
            "an activation-woken tick must still traverse the full \
             PERSISTENT_MISORDER_TICKS debounce; the wake shortens the WAIT, not \
             the tick COUNT"
        );

        // Arm B (positive control): panel fell BEHIND its source. That branch
        // is exempt from the debounce and must fire on tick 1 -- proving the
        // loop above can observe an immediate Reorder when one is warranted.
        let behind = vec![
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(SHARED, OTHER_PID, shared),
            s(PANEL, SELF_PID, pf(shared)),
        ];
        let control = plan_border(
            &behind,
            PANEL,
            SHARED,
            SharedSourceKind::Window,
            shared,
            false,
            SELF_PID,
            Some(previous),
            0,
        );
        assert!(
            control.actions.contains(&BorderAction::Reorder),
            "positive control failed: a panel behind its source must reorder \
             on the first tick, so the harness can see an immediate Reorder"
        );
        assert_eq!(control.suppressed_misorder_ticks, 0);
    }

    #[test]
    fn reorders_when_shared_source_stack_position_changes_after_debounce() {
        let shared = f(10, 10, 300, 200);
        let previous = source_state(shared, 1);
        // The shared source raised above OTHER while the border stayed behind.
        let stack = vec![
            s(SHARED, OTHER_PID, shared),
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(PANEL, SELF_PID, pf(shared)),
        ];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Window,
            shared,
            false,
            SELF_PID,
            Some(previous),
            0,
        );
        assert_eq!(plan.actions, vec![BorderAction::Reorder]);
        assert_eq!(plan.shared_state, Some(source_state(shared, 0)));
    }

    #[test]
    fn reorders_when_border_fell_behind_shared() {
        let shared = f(10, 10, 300, 200);
        let stack = vec![
            s(SHARED, OTHER_PID, shared),
            s(PETAL_WINDOW, SELF_PID, f(60, 60, 360, 260)),
            s(PANEL, SELF_PID, pf(shared)),
        ];
        assert_eq!(
            plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None),
            vec![BorderAction::Reorder]
        );
    }

    #[test]
    fn reorders_when_border_fell_behind_unchanged_shared_source() {
        let shared = f(10, 10, 300, 200);
        let previous = source_state(shared, 0);
        let stack = vec![s(SHARED, OTHER_PID, shared), s(PANEL, SELF_PID, pf(shared))];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Window,
            shared,
            false,
            SELF_PID,
            Some(previous),
            0,
        );
        assert_eq!(plan.actions, vec![BorderAction::Reorder]);
        assert_eq!(plan.shared_state, Some(previous));
    }

    #[test]
    fn sets_frame_when_shared_window_moved() {
        let old = f(10, 10, 300, 200);
        let moved = f(120, 90, 300, 200);
        let stack = vec![
            s(PANEL, SELF_PID, pf(old)),
            s(PETAL_WINDOW, SELF_PID, f(60, 60, 360, 260)),
            s(SHARED, OTHER_PID, moved),
        ];
        assert_eq!(
            plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None),
            vec![BorderAction::SetFrame(moved)]
        );
    }

    #[test]
    fn reorders_and_sets_frame_together_when_both_wrong() {
        let old = f(10, 10, 300, 200);
        let moved = f(120, 90, 320, 240);
        let stack = vec![
            s(PANEL, SELF_PID, pf(old)),
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(SHARED, OTHER_PID, moved),
        ];
        assert_eq!(
            plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None),
            vec![BorderAction::Reorder, BorderAction::SetFrame(moved)]
        );
    }

    #[test]
    fn hides_once_when_shared_window_leaves_screen() {
        let shared = f(10, 10, 300, 200);
        let stack = vec![
            s(PANEL, SELF_PID, pf(shared)),
            s(PETAL_WINDOW, SELF_PID, f(60, 60, 360, 260)),
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
        ];
        assert_eq!(
            plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None),
            vec![BorderAction::Hide]
        );
        // Already hidden: no repeated Hide spam.
        assert!(plan_border_actions(&stack, PANEL, SHARED, true, SELF_PID, None).is_empty());
    }

    #[test]
    fn display_border_does_not_hide_when_source_id_is_not_a_cgwindow() {
        let display = f(0, 0, 1512, 982);
        let stack = vec![
            s(PANEL, SELF_PID, pf(display)),
            s(OTHER, OTHER_PID, f(140, 120, 600, 400)),
        ];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Display,
            display,
            false,
            SELF_PID,
            None,
            0,
        );

        assert!(plan.actions.is_empty());
        assert_eq!(plan.shared_state, None);
    }

    #[test]
    fn display_border_is_reordered_when_cgwindow_list_does_not_report_panel() {
        let display = f(0, 0, 1512, 982);
        let plan = plan_border(
            &[],
            PANEL,
            SHARED,
            SharedSourceKind::Display,
            display,
            false,
            SELF_PID,
            None,
            0,
        );

        assert_eq!(
            plan.actions,
            vec![BorderAction::Reorder, BorderAction::SetFrame(display)]
        );
        assert_eq!(plan.shared_state, None);
    }

    #[test]
    fn display_border_hugs_frame_without_window_stack_or_hysteresis_state() {
        let display = f(-1512, 0, 1512, 982);
        let stack = vec![
            s(PANEL, SELF_PID, pf(display)),
            s(OTHER, OTHER_PID, f(0, 0, 800, 600)),
        ];

        let plan = plan_border(
            &stack,
            PANEL,
            (DISPLAY_SOURCE_TAG | 7) as i64,
            SharedSourceKind::Display,
            display,
            false,
            SELF_PID,
            Some(source_state(f(1, 1, 2, 2), 99)),
            PERSISTENT_MISORDER_TICKS - 1,
        );

        assert!(plan.actions.is_empty());
        assert_eq!(plan.shared_state, None);
        assert_eq!(plan.suppressed_misorder_ticks, 0);
    }

    #[test]
    fn display_source_id_decodes_only_tagged_ids() {
        assert_eq!(display_id_from_source_id(DISPLAY_SOURCE_TAG | 42), Some(42));
        assert_eq!(display_id_from_source_id(DISPLAY_SOURCE_TAG), Some(0));
        assert_eq!(display_id_from_source_id(42), None);
        assert_eq!(display_id_from_source_id(u32::MAX), None);
    }

    #[test]
    fn display_border_reorders_and_reframes_when_tracker_hidden() {
        let old = f(0, 0, 1280, 720);
        let display = f(0, 0, 1512, 982);
        let stack = vec![s(PANEL, SELF_PID, pf(old))];

        let plan = plan_border(
            &stack,
            PANEL,
            SHARED,
            SharedSourceKind::Display,
            display,
            true,
            SELF_PID,
            None,
            0,
        );

        assert_eq!(
            plan.actions,
            vec![BorderAction::Reorder, BorderAction::SetFrame(display)]
        );
        assert_eq!(plan.shared_state, None);
    }

    #[test]
    fn reorders_and_reframes_when_shared_window_returns_while_border_hidden() {
        let back = f(40, 60, 300, 200);
        let stack = vec![
            s(OTHER, OTHER_PID, f(0, 0, 500, 500)),
            s(SHARED, OTHER_PID, back),
        ];
        assert_eq!(
            plan_border_actions(&stack, PANEL, SHARED, true, SELF_PID, None),
            vec![BorderAction::Reorder, BorderAction::SetFrame(back)]
        );
    }

    #[test]
    fn multiple_borders_each_track_their_own_shared_window() {
        // borderA above sharedA, borderB above sharedB, interleaved — both fine.
        let (panel_b, shared_b) = (101i64, 201i64);
        let shared_a_frame = f(0, 0, 100, 100);
        let shared_b_frame = f(200, 0, 100, 100);
        let stack = vec![
            s(PANEL, SELF_PID, pf(shared_a_frame)),
            s(SHARED, OTHER_PID, shared_a_frame),
            s(panel_b, SELF_PID, pf(shared_b_frame)),
            s(shared_b, OTHER_PID, shared_b_frame),
        ];
        assert!(plan_border_actions(&stack, PANEL, SHARED, false, SELF_PID, None).is_empty());
        assert!(plan_border_actions(&stack, panel_b, shared_b, false, SELF_PID, None).is_empty());
    }
}
