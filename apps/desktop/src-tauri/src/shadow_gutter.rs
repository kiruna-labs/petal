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
//! WKWebView panel hit-tests its whole frame), so while a gutter panel is
//! visible a light main-thread poll makes the window ignore cursor events
//! whenever the cursor is outside the card, and take them again inside it.
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

#[cfg(target_os = "macos")]
mod passthrough {
    use super::ShadowGutter;
    use crate::sync_ext::MutexExt;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;
    use tauri::{AppHandle, Manager};

    struct Tracked {
        gutter: ShadowGutter,
        /// Last value handed to `set_ignore_cursor_events`; `None` until the
        /// poll has decided once for the current showing.
        ignoring: Option<bool>,
    }

    fn registry() -> &'static Mutex<HashMap<String, Tracked>> {
        static REGISTRY: OnceLock<Mutex<HashMap<String, Tracked>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    static POLL_STARTED: AtomicBool = AtomicBool::new(false);
    static ANY_VISIBLE: AtomicBool = AtomicBool::new(false);

    /// Poll period while a gutter panel is on screen: fast enough that the
    /// card takes the click before the user's press lands on it.
    const ACTIVE_PERIOD: Duration = Duration::from_millis(30);
    /// Poll period while every gutter panel is hidden.
    const IDLE_PERIOD: Duration = Duration::from_millis(100);

    /// Make the gutter of the window `label` click-through while it is
    /// visible. Idempotent; call once after creating the panel.
    pub fn track(app: &AppHandle, label: &str, gutter: ShadowGutter) {
        registry().lock_unpoisoned().insert(
            label.to_string(),
            Tracked {
                gutter,
                ignoring: None,
            },
        );
        if POLL_STARTED.swap(true, Ordering::AcqRel) {
            return;
        }
        let app = app.clone();
        let spawned = std::thread::Builder::new()
            .name("shadow-gutter".into())
            .spawn(move || loop {
                let period = if ANY_VISIBLE.load(Ordering::Acquire) {
                    ACTIVE_PERIOD
                } else {
                    IDLE_PERIOD
                };
                std::thread::sleep(period);
                let app_main = app.clone();
                let _ = app.run_on_main_thread(move || tick(&app_main));
            });
        if let Err(e) = spawned {
            POLL_STARTED.store(false, Ordering::Release);
            log::warn!("shadow_gutter: failed to start pass-through poll: {e}");
        }
    }

    /// Main thread: flip each visible gutter panel's cursor pass-through to
    /// match whether the cursor is over its card.
    fn tick(app: &AppHandle) {
        let cursor = crate::platform::cg::cursor_position();
        let mut any_visible = false;
        let mut registry = registry().lock_unpoisoned();
        for (label, tracked) in registry.iter_mut() {
            let Some(window) = app.get_webview_window(label) else {
                continue;
            };
            if !window.is_visible().unwrap_or(false) {
                // Next showing decides afresh (its present path may have
                // reset pass-through while hidden).
                tracked.ignoring = None;
                continue;
            }
            any_visible = true;
            let (Some(cursor), Ok(position), Ok(size), Ok(scale)) = (
                cursor,
                window.outer_position(),
                window.outer_size(),
                window.scale_factor(),
            ) else {
                continue;
            };
            let scale = scale.max(1.0);
            let frame = (
                position.x as f64 / scale,
                position.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
            );
            let ignore = !tracked.gutter.cursor_over_card(cursor, frame);
            if tracked.ignoring != Some(ignore) {
                let _ = window.set_ignore_cursor_events(ignore);
                tracked.ignoring = Some(ignore);
            }
        }
        ANY_VISIBLE.store(any_visible, Ordering::Release);
    }
}

#[cfg(target_os = "macos")]
pub use passthrough::track;

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
        assert!(!FLOAT.cursor_over_card((300.0, 120.0), frame), "right gutter");
        assert!(!FLOAT.cursor_over_card((150.0, 95.0), frame), "top gutter");
        assert!(!FLOAT.cursor_over_card((150.0, 200.0), frame), "bottom gutter");
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
