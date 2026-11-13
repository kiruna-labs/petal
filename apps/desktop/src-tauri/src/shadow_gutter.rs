//! Transparent shadow gutter for macOS card panels (#406).
//!
//! A transparent `NSPanel` with a native window shadow gets an outline drawn
//! around its alpha: AppKit derives a non-opaque window's shadow from every
//! pixel with alpha > 0, including a CSS `box-shadow`'s faint footprint. On
//! macOS 26 that shadow reads as a black hairline "bubble". So these panels
//! turn the native shadow off and let the page's CSS shadow be the only
//! elevation. For that CSS shadow not to be clipped flat at the window edge,
//! the window is larger than the card by the shadow's extent on each side
//! (the "gutter"), and the page pads its card by the same amount.
//!
//! The gutter is transparent but would still swallow clicks (a transparent
//! WKWebView panel hit-tests its whole frame), so the window ignores cursor
//! events whenever the cursor is outside the card and takes them again inside
//! it. That flip is driven by pointer `NSEvent` monitors (installed when a
//! gutter panel is shown, removed on the first pointer event that finds none
//! on screen; no events while the cursor is still), plus re-evaluation on
//! [`panel_shown`] and on a tracked panel's move/resize; see [`Passthrough`].
//!
//! The extents mirror `shared/ui/tokens.css` (`--shadow-float`,
//! `--shadow-panel`); `tests/shadowGutter.test.ts` fails if they drift.

/// Space reserved around a card for its CSS shadow, in logical points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowGutter {
    pub x: f64,
    pub top: f64,
    pub bottom: f64,
}

/// `--shadow-float: 0 18px 48px -22px`: blur 48 - spread 22 = 26 at the
/// sides, 26 - 18 = 8 above, 26 + 18 = 44 below.
pub const FLOAT: ShadowGutter = ShadowGutter {
    x: 26.0,
    top: 8.0,
    bottom: 44.0,
};

/// `--shadow-panel: 0 30px 80px -28px`: 52 at the sides, 22 above, 82 below.
pub const PANEL: ShadowGutter = ShadowGutter {
    x: 52.0,
    top: 22.0,
    bottom: 82.0,
};

impl ShadowGutter {
    /// Query-string value the page reads to pad its card by this gutter.
    pub fn query_value(self) -> &'static str {
        if self == PANEL {
            "panel"
        } else {
            "float"
        }
    }

    /// Window size for a card of `width` x `height`.
    pub fn window_size(self, width: f64, height: f64) -> (f64, f64) {
        (width + 2.0 * self.x, height + self.top + self.bottom)
    }

    /// Window top-left origin for a card whose top-left is at `(x, y)`.
    pub fn window_origin(self, x: f64, y: f64) -> (f64, f64) {
        (x - self.x, y - self.top)
    }

    /// Whether `cursor` (global logical points) is over the card of a window
    /// whose frame is `(x, y, width, height)` (global logical points).
    pub fn cursor_over_card(self, cursor: (f64, f64), window: (f64, f64, f64, f64)) -> bool {
        let (cx, cy) = cursor;
        let (x, y, width, height) = window;
        let left = x + self.x;
        let right = x + width - self.x;
        let top = y + self.top;
        let bottom = y + height - self.bottom;
        cx >= left && cx < right && cy >= top && cy < bottom
    }
}

/// Append the gutter query parameter to a panel route URL.
pub fn route_with_gutter(route: &str, gutter: ShadowGutter) -> String {
    let separator = if route.contains('?') { '&' } else { '?' };
    format!("{route}{separator}shadowGutter={}", gutter.query_value())
}

/// What the caller must do with the pointer-motion monitors after a
/// [`Passthrough`] transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorAction {
    None,
    /// A gutter panel was shown: start observing pointer motion.
    Install,
    /// No gutter panel is on screen: stop observing.
    Remove,
}

/// A tracked panel window, as the pass-through state machine sees it. The
/// app implements it for `tauri::WebviewWindow`; tests use a fake.
pub trait GutterWindow {
    fn is_visible(&self) -> bool;
    /// Frame in global top-left logical points: `(x, y, width, height)`.
    fn frame(&self) -> Option<(f64, f64, f64, f64)>;
    fn set_ignore_cursor_events(&self, ignore: bool);
}

struct Tracked {
    gutter: ShadowGutter,
    /// Last value handed to `set_ignore_cursor_events`; `None` = unknown.
    ignoring: Option<bool>,
    /// Shown and not yet seen hidden. Only these are re-evaluated, so a
    /// hidden (e.g. retired) panel costs no window lookups per event.
    on_screen: bool,
}

/// Which gutter panels are click-through, re-evaluated on pointer events and
/// on a tracked panel's move/resize rather than on a timer: a 30 ms poll left
/// the card deaf to a fast move-and-click and never parked while an overlay
/// stayed open (#415). `ignoresMouseEvents` is per window, and `hitTest:`
/// returning nil drops a click instead of passing it to the window behind, so
/// the whole window is flipped as soon as the cursor crosses the card edge.
#[derive(Default)]
pub struct Passthrough {
    tracked: std::collections::HashMap<String, Tracked>,
    monitors_installed: bool,
}

impl Passthrough {
    pub fn track(&mut self, label: &str, gutter: ShadowGutter) {
        self.tracked.insert(
            label.to_string(),
            Tracked {
                gutter,
                ignoring: None,
                on_screen: false,
            },
        );
    }

    /// The panel `label` was just shown. Its pass-through is set for the
    /// cursor's position NOW, unconditionally (it may have hidden while
    /// click-through); with an unknown cursor or frame it takes clicks.
    pub fn shown(
        &mut self,
        label: &str,
        window: &impl GutterWindow,
        cursor: Option<(f64, f64)>,
    ) -> MonitorAction {
        let Some(tracked) = self.tracked.get_mut(label) else {
            return MonitorAction::None;
        };
        let ignore = match (cursor, window.frame()) {
            (Some(cursor), Some(frame)) => !tracked.gutter.cursor_over_card(cursor, frame),
            _ => false,
        };
        window.set_ignore_cursor_events(ignore);
        tracked.ignoring = Some(ignore);
        tracked.on_screen = true;
        if self.monitors_installed {
            MonitorAction::None
        } else {
            self.monitors_installed = true;
            MonitorAction::Install
        }
    }

    /// Installing the monitors failed after showing `label`. Nothing would
    /// flip it back, so it takes clicks (a swallowed gutter click beats a
    /// click-through card) and is left out of re-evaluation until the next
    /// show, which retries.
    pub fn install_failed(&mut self, label: &str, window: &impl GutterWindow) {
        self.monitors_installed = false;
        if let Some(tracked) = self.tracked.get_mut(label) {
            window.set_ignore_cursor_events(false);
            tracked.ignoring = Some(false);
            tracked.on_screen = false;
        }
    }

    /// Re-evaluate every on-screen panel for the cursor at `cursor` (it
    /// moved, or a panel moved/resized under it); flip each one whose card
    /// edge it is now on the other side of. Untracks panels whose window is
    /// gone; once no tracked panel is on screen, stop observing.
    pub fn reevaluate<W: GutterWindow>(
        &mut self,
        cursor: Option<(f64, f64)>,
        mut window: impl FnMut(&str) -> Option<W>,
    ) -> MonitorAction {
        let mut any_visible = false;
        self.tracked.retain(|label, tracked| {
            if !tracked.on_screen {
                return true;
            }
            let Some(win) = window(label) else {
                return false;
            };
            if !win.is_visible() {
                tracked.on_screen = false;
                return true;
            }
            any_visible = true;
            let (Some(cursor), Some(frame)) = (cursor, win.frame()) else {
                return true;
            };
            let ignore = !tracked.gutter.cursor_over_card(cursor, frame);
            if tracked.ignoring != Some(ignore) {
                win.set_ignore_cursor_events(ignore);
                tracked.ignoring = Some(ignore);
            }
            true
        });
        if !any_visible && self.monitors_installed {
            self.monitors_installed = false;
            MonitorAction::Remove
        } else {
            MonitorAction::None
        }
    }
}

mod passthrough {
    use super::{GutterWindow, MonitorAction, Passthrough, ShadowGutter};
    use crate::sync_ext::MutexExt;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSEvent, NSEventMask};
    use std::cell::RefCell;
    use std::ptr::NonNull;
    use std::sync::{Mutex, OnceLock};
    use tauri::{AppHandle, Manager, WebviewWindow, WindowEvent};

    fn state() -> &'static Mutex<Passthrough> {
        static STATE: OnceLock<Mutex<Passthrough>> = OnceLock::new();
        STATE.get_or_init(|| Mutex::new(Passthrough::default()))
    }

    thread_local! {
        /// Main thread only: the global + local monitor tokens while installed.
        static MONITORS: RefCell<Vec<Retained<AnyObject>>> = const { RefCell::new(Vec::new()) };
    }

    impl GutterWindow for WebviewWindow {
        fn is_visible(&self) -> bool {
            WebviewWindow::is_visible(self).unwrap_or(false)
        }

        fn frame(&self) -> Option<(f64, f64, f64, f64)> {
            let position = self.outer_position().ok()?;
            let size = self.outer_size().ok()?;
            let scale = self.scale_factor().ok()?.max(1.0);
            Some((
                position.x as f64 / scale,
                position.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
            ))
        }

        fn set_ignore_cursor_events(&self, ignore: bool) {
            let _ = WebviewWindow::set_ignore_cursor_events(self, ignore);
        }
    }

    /// Make the gutter of the window `label` click-through while it is
    /// visible. Call once after creating the panel. A move or resize of the
    /// panel re-evaluates it, so a card that slides under a still cursor
    /// takes the click.
    pub fn track(app: &AppHandle, label: &str, gutter: ShadowGutter) {
        state().lock_unpoisoned().track(label, gutter);
        let Some(window) = app.get_webview_window(label) else {
            return;
        };
        let app = app.clone();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
                reevaluate(&app);
            }
        });
    }

    /// Main thread, right after a tracked panel is shown and positioned: set
    /// its pass-through for the current cursor before it can take a click,
    /// and observe pointer events while it is on screen. No-op if untracked.
    pub fn panel_shown(window: &WebviewWindow) {
        let cursor = crate::platform::cg::cursor_position();
        let action = state()
            .lock_unpoisoned()
            .shown(window.label(), window, cursor);
        if action == MonitorAction::Install && !install_monitors(window.app_handle()) {
            state()
                .lock_unpoisoned()
                .install_failed(window.label(), window);
        }
    }

    /// Main thread: from either monitor, or a tracked panel's move/resize.
    fn reevaluate(app: &AppHandle) {
        if objc2::MainThreadMarker::new().is_none() {
            return;
        }
        let cursor = crate::platform::cg::cursor_position();
        let action = state()
            .lock_unpoisoned()
            .reevaluate(cursor, |label| app.get_webview_window(label));
        if action == MonitorAction::Remove {
            remove_monitors();
        }
    }

    /// Motion, drags and button releases (a release that reaches a monitor
    /// settles a drag whose motion a tracking loop consumed).
    fn pointer_mask() -> NSEventMask {
        NSEventMask::MouseMoved
            | NSEventMask::LeftMouseDragged
            | NSEventMask::RightMouseDragged
            | NSEventMask::OtherMouseDragged
            | NSEventMask::LeftMouseUp
            | NSEventMask::RightMouseUp
            | NSEventMask::OtherMouseUp
    }

    /// Global monitor: events delivered to other apps (the cursor is over a
    /// click-through gutter or outside our windows). Local monitor: events
    /// delivered to us. Both handlers run on the main thread. Installs both
    /// or neither; returns whether they are installed.
    fn install_monitors(app: &AppHandle) -> bool {
        if objc2::MainThreadMarker::new().is_none() {
            log::warn!("shadow_gutter: pointer monitors must be installed on the main thread");
            return false;
        }
        MONITORS.with(|cell| {
            let mut monitors = cell.borrow_mut();
            if !monitors.is_empty() {
                return true;
            }
            let global_app = app.clone();
            let global = RcBlock::new(move |_event: NonNull<NSEvent>| {
                reevaluate(&global_app);
            });
            let local_app = app.clone();
            let local = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
                reevaluate(&local_app);
                event.as_ptr()
            });
            if let Some(token) =
                NSEvent::addGlobalMonitorForEventsMatchingMask_handler(pointer_mask(), &global)
            {
                monitors.push(token);
            }
            // SAFETY: the handler returns the event it was given, unchanged.
            if let Some(token) = unsafe {
                NSEvent::addLocalMonitorForEventsMatchingMask_handler(pointer_mask(), &local)
            } {
                monitors.push(token);
            }
            if monitors.len() == 2 {
                return true;
            }
            log::warn!(
                "shadow_gutter: installed {} of 2 pointer monitors; retrying on next show",
                monitors.len()
            );
            for token in monitors.drain(..) {
                // SAFETY: the token came from NSEvent's add*Monitor methods.
                unsafe { NSEvent::removeMonitor(&token) };
            }
            false
        })
    }

    fn remove_monitors() {
        MONITORS.with(|cell| {
            for token in cell.borrow_mut().drain(..) {
                // SAFETY: every token came from NSEvent's add*Monitor methods.
                unsafe { NSEvent::removeMonitor(&token) };
            }
        });
    }
}

pub use passthrough::{panel_shown, track};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_wraps_the_card_with_the_gutter_on_every_side() {
        assert_eq!(FLOAT.window_size(420.0, 120.0), (472.0, 172.0));
        assert_eq!(FLOAT.window_origin(100.0, 96.0), (74.0, 88.0));
        assert_eq!(PANEL.window_size(280.0, 300.0), (384.0, 404.0));
    }

    #[test]
    fn only_the_card_takes_the_cursor_not_the_gutter() {
        let (x, y) = FLOAT.window_origin(100.0, 100.0);
        let (w, h) = FLOAT.window_size(200.0, 80.0);
        let frame = (x, y, w, h);
        assert!(FLOAT.cursor_over_card((100.0, 100.0), frame));
        assert!(FLOAT.cursor_over_card((299.0, 179.0), frame));
        assert!(!FLOAT.cursor_over_card((99.0, 120.0), frame), "left gutter");
        assert!(
            !FLOAT.cursor_over_card((300.0, 120.0), frame),
            "right gutter"
        );
        assert!(!FLOAT.cursor_over_card((150.0, 95.0), frame), "top gutter");
        assert!(
            !FLOAT.cursor_over_card((150.0, 200.0), frame),
            "bottom gutter"
        );
    }

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// A panel window: frame + visibility + every `set_ignore_cursor_events`
    /// call it received, in order.
    #[derive(Clone)]
    struct FakeWindow {
        visible: Rc<Cell<bool>>,
        frame: (f64, f64, f64, f64),
        calls: Rc<RefCell<Vec<bool>>>,
    }

    impl FakeWindow {
        /// A FLOAT-gutter panel whose 200x80 card's top-left is (100, 100).
        fn card_at_100() -> Self {
            let (x, y) = FLOAT.window_origin(100.0, 100.0);
            let (w, h) = FLOAT.window_size(200.0, 80.0);
            Self {
                visible: Rc::new(Cell::new(true)),
                frame: (x, y, w, h),
                calls: Rc::default(),
            }
        }

        fn ignoring(&self) -> Option<bool> {
            self.calls.borrow().last().copied()
        }
    }

    impl GutterWindow for FakeWindow {
        fn is_visible(&self) -> bool {
            self.visible.get()
        }
        fn frame(&self) -> Option<(f64, f64, f64, f64)> {
            Some(self.frame)
        }
        fn set_ignore_cursor_events(&self, ignore: bool) {
            self.calls.borrow_mut().push(ignore);
        }
    }

    const ON_CARD: (f64, f64) = (150.0, 140.0);
    const IN_GUTTER: (f64, f64) = (90.0, 140.0);

    fn moved(p: &mut Passthrough, w: &FakeWindow, cursor: (f64, f64)) -> MonitorAction {
        p.reevaluate(Some(cursor), |label| {
            (label == "consent").then(|| w.clone())
        })
    }

    #[test]
    fn show_sets_pass_through_for_the_cursor_now_and_starts_observing() {
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let w = FakeWindow::card_at_100();
        assert_eq!(
            p.shown("consent", &w, Some(ON_CARD)),
            MonitorAction::Install
        );
        assert_eq!(
            w.ignoring(),
            Some(false),
            "card under the cursor takes the first click"
        );
        let w2 = FakeWindow::card_at_100();
        assert_eq!(
            p.shown("consent", &w2, Some(IN_GUTTER)),
            MonitorAction::None,
            "already observing"
        );
        assert_eq!(
            w2.ignoring(),
            Some(true),
            "gutter under the cursor is click-through at once"
        );
        assert_eq!(p.shown("consent", &w2, None), MonitorAction::None);
        assert_eq!(w2.ignoring(), Some(false), "unknown cursor: take clicks");
        assert_eq!(
            p.shown("untracked", &w2, Some(IN_GUTTER)),
            MonitorAction::None
        );
    }

    #[test]
    fn crossing_back_onto_the_card_takes_clicks_on_that_motion_event() {
        // Edge lag (#415): the card must take clicks on the very motion event
        // that crosses its edge, not on a later timer tick.
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let w = FakeWindow::card_at_100();
        p.shown("consent", &w, Some(ON_CARD));
        moved(&mut p, &w, IN_GUTTER);
        assert_eq!(w.ignoring(), Some(true));
        moved(&mut p, &w, ON_CARD);
        assert_eq!(w.ignoring(), Some(false));
        moved(&mut p, &w, (151.0, 141.0));
        assert_eq!(
            *w.calls.borrow(),
            vec![false, true, false],
            "flips only on a crossing"
        );
    }

    #[test]
    fn re_show_after_hiding_in_the_gutter_takes_clicks_immediately() {
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let w = FakeWindow::card_at_100();
        p.shown("consent", &w, Some(ON_CARD));
        moved(&mut p, &w, IN_GUTTER);
        w.visible.set(false);
        // Re-shown under a cursor that is now on the card (the card moved or
        // the cursor did, with no motion event while hidden).
        w.visible.set(true);
        p.shown("consent", &w, Some(ON_CARD));
        assert_eq!(w.ignoring(), Some(false));
    }

    #[test]
    fn observing_stops_once_no_panel_is_visible_and_resumes_on_show() {
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let w = FakeWindow::card_at_100();
        assert_eq!(
            p.shown("consent", &w, Some(ON_CARD)),
            MonitorAction::Install
        );
        assert_eq!(
            moved(&mut p, &w, IN_GUTTER),
            MonitorAction::None,
            "visible: keep observing"
        );
        w.visible.set(false);
        assert_eq!(moved(&mut p, &w, ON_CARD), MonitorAction::Remove);
        assert_eq!(
            moved(&mut p, &w, ON_CARD),
            MonitorAction::None,
            "removed once"
        );
        assert_eq!(
            *w.calls.borrow(),
            vec![false, true],
            "a hidden panel is never flipped"
        );
        w.visible.set(true);
        assert_eq!(
            p.shown("consent", &w, Some(ON_CARD)),
            MonitorAction::Install
        );
    }

    #[test]
    fn a_card_that_moves_under_a_still_cursor_takes_the_click() {
        // A panel move/resize re-evaluates with the same cursor position.
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let mut w = FakeWindow::card_at_100();
        p.shown("consent", &w, Some(IN_GUTTER));
        assert_eq!(w.ignoring(), Some(true));
        w.frame.0 -= 40.0; // card now spans x 60..260, under the cursor at x 90
        moved(&mut p, &w, IN_GUTTER);
        assert_eq!(w.ignoring(), Some(false));
    }

    #[test]
    fn a_failed_monitor_install_is_retried_on_the_next_show() {
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let w = FakeWindow::card_at_100();
        assert_eq!(
            p.shown("consent", &w, Some(IN_GUTTER)),
            MonitorAction::Install
        );
        assert_eq!(w.ignoring(), Some(true));
        p.install_failed("consent", &w);
        assert_eq!(
            w.ignoring(),
            Some(false),
            "nothing would flip it back: take clicks"
        );
        moved(&mut p, &w, IN_GUTTER); // e.g. a queued Moved from the show sequence
        assert_eq!(
            w.ignoring(),
            Some(false),
            "unobserved: must not turn click-through again"
        );
        assert_eq!(
            p.shown("consent", &w, Some(ON_CARD)),
            MonitorAction::Install
        );
    }

    #[test]
    fn a_destroyed_window_is_untracked() {
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        let w = FakeWindow::card_at_100();
        p.shown("consent", &w, Some(ON_CARD));
        assert_eq!(
            p.reevaluate(Some(ON_CARD), |_| None::<FakeWindow>),
            MonitorAction::Remove
        );
        assert_eq!(
            p.shown("consent", &w, Some(ON_CARD)),
            MonitorAction::None,
            "no longer tracked"
        );
    }

    #[test]
    fn a_hidden_panel_is_not_queried_per_pointer_event() {
        let mut p = Passthrough::default();
        p.track("consent", FLOAT);
        p.track("retired-overlay", FLOAT);
        let w = FakeWindow::card_at_100();
        p.shown("consent", &w, Some(ON_CARD));
        let mut asked = Vec::new();
        p.reevaluate(Some(ON_CARD), |label| {
            asked.push(label.to_string());
            Some(w.clone())
        });
        assert_eq!(asked, vec!["consent"], "never-shown panel is not looked up");
        w.visible.set(false);
        moved(&mut p, &w, ON_CARD);
        let mut asked = 0;
        p.reevaluate(Some(ON_CARD), |_| {
            asked += 1;
            Some(w.clone())
        });
        assert_eq!(
            asked, 0,
            "seen hidden once: no longer looked up until shown"
        );
    }

    #[test]
    fn route_query_names_the_gutter() {
        assert_eq!(
            route_with_gutter("control-consent.html", FLOAT),
            "control-consent.html?shadowGutter=float"
        );
        assert_eq!(
            route_with_gutter("compositor/ai-chat.html?windowId=7", FLOAT),
            "compositor/ai-chat.html?windowId=7&shadowGutter=float"
        );
        assert_eq!(
            route_with_gutter("menubar-popover.html", PANEL),
            "menubar-popover.html?shadowGutter=panel"
        );
    }
}
