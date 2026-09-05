//! Native main-window resize helpers.
//!
//! AppKit window frame animation must run on the main thread. The Tauri command
//! below only validates input on the command boundary, then marshals the
//! `NSWindow setFrame:display:animate:` call through `AppHandle::run_on_main_thread`.

#![cfg(target_os = "macos")]

use serde::Serialize;
use tauri::{AppHandle, Manager};

const MAIN_WINDOW_LABEL: &str = "main";
const MIN_WIDTH: f64 = 320.0;
const MIN_HEIGHT: f64 = 240.0;
const MAX_WIDTH: f64 = 4096.0;
const MAX_HEIGHT: f64 = 4096.0;
const MAIN_THREAD_TIMEOUT_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LogicalResize {
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnimatedResizeOutcome {
    /// True when the command found the main window and applied either the
    /// native AppKit animation or a non-animated Tauri fallback resize.
    pub applied: bool,
    /// True when the AppKit `setFrame:display:animate:` path was used.
    pub animated: bool,
    /// The validated/clamped logical width used for the resize.
    pub width: f64,
    /// The validated/clamped logical height used for the resize.
    pub height: f64,
    /// Human-readable reason when the command could not animate or apply.
    pub reason: Option<String>,
}

/// Smoothly resize the primary Petal window using AppKit's native window-frame
/// animation. Input sizes are logical points, not physical pixels.
#[tauri::command]
pub async fn animate_main_window_resize(
    app: AppHandle,
    width: f64,
    height: f64,
) -> Result<AnimatedResizeOutcome, String> {
    let target = validate_logical_resize(width, height)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let app_for_thread = app.clone();

    app.run_on_main_thread(move || {
        let outcome = apply_main_window_resize(&app_for_thread, target);
        let _ = tx.send(outcome);
    })
    .map_err(|e| format!("failed to schedule main-window resize on main thread: {e}"))?;

    match tokio::time::timeout(std::time::Duration::from_millis(MAIN_THREAD_TIMEOUT_MS), rx).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(_closed)) => Err("main-window resize result channel closed".to_string()),
        Err(_elapsed) => Err("timed out waiting for main-window resize".to_string()),
    }
}

pub(crate) fn validate_logical_resize(width: f64, height: f64) -> Result<LogicalResize, String> {
    if !width.is_finite() || !height.is_finite() {
        return Err("window size must be finite".to_string());
    }
    if width <= 0.0 || height <= 0.0 {
        return Err("window size must be positive".to_string());
    }

    Ok(LogicalResize {
        width: width.clamp(MIN_WIDTH, MAX_WIDTH),
        height: height.clamp(MIN_HEIGHT, MAX_HEIGHT),
    })
}

fn apply_main_window_resize(app: &AppHandle, target: LogicalResize) -> AnimatedResizeOutcome {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        log::warn!("window_resize: main window missing; cannot resize");
        return outcome(
            target,
            false,
            false,
            Some("main window not found".to_string()),
        );
    };

    let Ok(ns_window_ptr) = window.ns_window() else {
        log::warn!(
            "window_resize: main window native handle unavailable; using non-animated fallback"
        );
        return match window.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: target.width,
            height: target.height,
        })) {
            Ok(()) => outcome(
                target,
                true,
                false,
                Some("native window handle unavailable; used non-animated fallback".to_string()),
            ),
            Err(e) => outcome(
                target,
                false,
                false,
                Some(format!(
                    "native window handle unavailable and fallback resize failed: {e}"
                )),
            ),
        };
    };

    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_foundation::{NSPoint, NSRect, NSSize};

        let ns_window = ns_window_ptr as *mut AnyObject;
        let current: NSRect = msg_send![ns_window, frame];
        let center_x = current.origin.x + current.size.width / 2.0;
        let center_y = current.origin.y + current.size.height / 2.0;
        let next = NSRect {
            origin: NSPoint::new(
                center_x - target.width / 2.0,
                center_y - target.height / 2.0,
            ),
            size: NSSize::new(target.width, target.height),
        };

        let _: () = msg_send![ns_window, setFrame: next, display: true, animate: true];
    }

    log::info!(
        "window_resize: animated main window to {:.0}x{:.0} logical points",
        target.width,
        target.height
    );
    outcome(target, true, true, None)
}

fn outcome(
    target: LogicalResize,
    applied: bool,
    animated: bool,
    reason: Option<String>,
) -> AnimatedResizeOutcome {
    AnimatedResizeOutcome {
        applied,
        animated,
        width: target.width,
        height: target.height,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_logical_resize, MAX_HEIGHT, MAX_WIDTH, MIN_HEIGHT, MIN_WIDTH};

    #[test]
    fn accepts_normal_positive_sizes() {
        let size = validate_logical_resize(900.0, 640.0).unwrap();
        assert_eq!(size.width, 900.0);
        assert_eq!(size.height, 640.0);
    }

    #[test]
    fn clamps_to_sane_logical_bounds() {
        let min = validate_logical_resize(1.0, 1.0).unwrap();
        assert_eq!(min.width, MIN_WIDTH);
        assert_eq!(min.height, MIN_HEIGHT);

        let max = validate_logical_resize(10_000.0, 10_000.0).unwrap();
        assert_eq!(max.width, MAX_WIDTH);
        assert_eq!(max.height, MAX_HEIGHT);
    }

    #[test]
    fn rejects_non_finite_or_non_positive_sizes() {
        assert!(validate_logical_resize(0.0, 600.0).is_err());
        assert!(validate_logical_resize(800.0, -1.0).is_err());
        assert!(validate_logical_resize(f64::NAN, 600.0).is_err());
        assert!(validate_logical_resize(800.0, f64::INFINITY).is_err());
    }
}
