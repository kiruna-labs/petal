//! Dedicated window-sharing picker window.
//!
//! The picker used to be an in-meeting overlay, which forced the collapsed
//! pill back into gallery mode and clipped inside the tiny pill host. A
//! regular secondary webview keeps the picker independent of the meeting
//! window state and avoids the `tauri_nspanel` close/dealloc crash class.

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const WINDOW_PICKER_LABEL: &str = "window-picker";

/// Logical picker size. Keep in sync with the `WebviewWindowBuilder`
/// `inner_size` below — the centering math uses these.
const PICKER_WIDTH: f64 = 820.0;
const PICKER_HEIGHT: f64 = 700.0;

#[cfg(target_os = "macos")]
fn show_picker_without_focus(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_PICKER_LABEL) else {
        return;
    };
    let _ = window.set_focusable(false);
    let _ = window.show();
    let _ = window.unminimize();
    if let Err(e) = crate::platform::appkit::order_front_without_activating(&window) {
        log::warn!("window-picker: failed to order front without activating: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn show_picker_without_focus(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_PICKER_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
    }
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn open_window_picker_window(
    app: AppHandle,
    state: tauri::State<'_, crate::session::SessionState>,
    color: Option<String>,
) -> Result<(), String> {
    // The system `SCContentSharingPicker` is deliberately not used: activating
    // it engages macOS's window-hover "Share This Window" overlay on every
    // window, and when its UI fails to present (e.g. VMware guests) nothing
    // reliably tears that overlay down -- `isActive = false` does not, only a
    // completed selection does (confirmed live on macOS 26). The custom picker
    // covers windows AND displays with SCK thumbnails, so the system picker
    // adds no capability and only risks the stuck overlay.
    // (Both `state` and `color` are only consumed by the autotest bypass.)
    let _ = (&state, &color);
    #[cfg(any(debug_assertions, feature = "autotest", feature = "cockpit-privileged"))]
    if let Some(target) = autotest_picker_target_from_env() {
        log::warn!(
            "window-picker: PETAL_AUTOTEST_PICKER_TARGET={target:?}; bypassing interactive picker with exact target"
        );
        let shared = start_autotest_picker_target(&app, &state, color, target).await;
        let shared = shared?;
        return if shared {
            Ok(())
        } else {
            Err("autotest system-picker target share failed".to_string())
        };
    }
    open_custom_window_picker_window(&app)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn open_window_picker_window(
    app: AppHandle,
    color: Option<String>,
) -> Result<(), String> {
    let _ = color;
    // MUST be `async`: Tauri runs async commands off the main thread, and
    // `WebviewWindowBuilder::build()` proxies window creation to the main
    // event loop and BLOCKS waiting for it. A sync command runs ON the main
    // thread, so the main loop can never process the proxy message — a
    // self-deadlock that freezes the whole UI (observed live: the picker
    // command never returns and every button stops responding).
    open_custom_window_picker_window(&app)
}

#[tauri::command]
pub async fn toggle_window_picker_window(app: AppHandle) -> Result<bool, String> {
    toggle_custom_window_picker_window(&app)
}

#[cfg(all(
    target_os = "macos",
    any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutotestPickerTarget {
    Window(u32),
    Owner(String),
    Pid(i32),
}

#[cfg(all(
    target_os = "macos",
    any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
))]
fn autotest_picker_target_from_env() -> Option<AutotestPickerTarget> {
    let raw = std::env::var("PETAL_AUTOTEST_PICKER_TARGET").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(value) = raw.strip_prefix("window:") {
        return value.parse().ok().map(AutotestPickerTarget::Window);
    }
    if let Some(value) = raw.strip_prefix("owner:") {
        return (!value.trim().is_empty())
            .then(|| AutotestPickerTarget::Owner(value.trim().to_string()));
    }
    if let Some(value) = raw.strip_prefix("pid:") {
        return value.parse().ok().map(AutotestPickerTarget::Pid);
    }
    raw.parse().ok().map(AutotestPickerTarget::Window)
}

#[cfg(all(
    target_os = "macos",
    any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
))]
fn resolve_autotest_picker_target(
    windows: &[crate::window_source::ShareableWindow],
    target: &AutotestPickerTarget,
) -> Result<u32, String> {
    let mut matches = windows.iter().filter(|window| match target {
        AutotestPickerTarget::Window(id) => window.window_id == *id,
        AutotestPickerTarget::Owner(owner) => window.app_name == *owner,
        AutotestPickerTarget::Pid(pid) => window.app_pid == *pid,
    });
    let Some(window) = matches.next() else {
        return Err(format!(
            "no shareable window matched autotest picker target {target:?}"
        ));
    };
    if matches.next().is_some() {
        return Err(format!(
            "autotest picker target {target:?} matched multiple shareable windows; use window:<CGWindowID>"
        ));
    }
    Ok(window.window_id)
}

#[cfg(all(
    target_os = "macos",
    any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
))]
async fn start_autotest_picker_target(
    app: &AppHandle,
    state: &crate::session::SessionState,
    color: Option<String>,
    target: AutotestPickerTarget,
) -> Result<bool, String> {
    use screencapturekit::shareable_content::SCShareableContent;
    use screencapturekit::stream::content_filter::SCContentFilter;

    let windows = crate::window_source::list().map_err(|e| e.to_string())?;
    let window_id = resolve_autotest_picker_target(&windows, &target)?;

    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| e.to_string())?;
    let window = content
        .windows()
        .into_iter()
        .find(|window| window.window_id() == window_id)
        .ok_or_else(|| format!("autotest picker target window {window_id} disappeared"))?;
    let frame = window.frame();
    let filter = SCContentFilter::create().with_window(&window).build();
    let logical_width = frame.size.width;
    let logical_height = frame.size.height;
    let point_pixel_scale = f64::from(filter.point_pixel_scale()).max(1.0);
    let title = window.title();
    log::info!(
        "window-picker: autotest selected exact window {window_id} ({title:?}) before capture; frontmost={}",
        crate::platform::appkit::frontmost_app_label()
    );
    let shared = crate::hover_tab::start_share_for_system_picker_selection(
        app,
        state,
        window_id,
        crate::hover_tab::WindowFrame {
            x: frame.origin.x.round() as i32,
            y: frame.origin.y.round() as i32,
            width: frame.size.width.round().max(1.0) as i32,
            height: frame.size.height.round().max(1.0) as i32,
        },
        filter,
        logical_width,
        logical_height,
        point_pixel_scale,
        crate::transport::publisher::SharedSourceKind::Window,
        title,
        color,
    )
    .await;
    log::info!(
        "window-picker: autotest exact target {window_id} done shared={shared} frontmost={}",
        crate::platform::appkit::frontmost_app_label()
    );
    Ok(shared)
}

#[cfg(all(
    target_os = "macos",
    not(any(debug_assertions, feature = "autotest", feature = "cockpit-privileged"))
))]
fn autotest_picker_target_from_env() -> Option<()> {
    // Release builds deliberately have no picker autotest branch. Keep this
    // tiny sentinel so the release-path regression test can prove that an
    // injected environment variable is ignored.
    let _ = std::env::var_os("PETAL_AUTOTEST_PICKER_TARGET");
    None
}

fn emit_picker_opened(app: &AppHandle) {
    let _ = app.emit("share-picker-opened", ());
}

fn emit_picker_visibility(app: &AppHandle, open: bool) {
    let _ = app.emit(
        "share-picker-visibility-changed",
        serde_json::json!({ "open": open }),
    );
}

fn open_custom_window_picker_window(app: &AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if objc2::MainThreadMarker::new().is_some() {
            return open_window_picker_window_on_main(app);
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let app_main = app.clone();
        app.run_on_main_thread(move || {
            let _ = tx.send(open_window_picker_window_on_main(&app_main));
        })
        .map_err(|e| format!("window-picker: run_on_main_thread failed: {e}"))?;
        return rx
            .recv()
            .map_err(|e| format!("window-picker: main-thread response failed: {e}"))?;
    }

    #[cfg(not(target_os = "macos"))]
    open_window_picker_window_on_main(app)
}

fn toggle_custom_window_picker_window(app: &AppHandle) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        if objc2::MainThreadMarker::new().is_some() {
            return toggle_window_picker_window_on_main(app);
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let app_main = app.clone();
        app.run_on_main_thread(move || {
            let _ = tx.send(toggle_window_picker_window_on_main(&app_main));
        })
        .map_err(|e| format!("window-picker: run_on_main_thread failed: {e}"))?;
        return rx
            .recv()
            .map_err(|e| format!("window-picker: main-thread response failed: {e}"))?;
    }

    #[cfg(not(target_os = "macos"))]
    toggle_window_picker_window_on_main(app)
}

fn toggle_window_picker_window_on_main(app: &AppHandle) -> Result<bool, String> {
    if let Some(picker) = app.get_webview_window(WINDOW_PICKER_LABEL) {
        if picker.is_visible().unwrap_or(false) {
            picker
                .hide()
                .map_err(|e| format!("window-picker: hide failed: {e}"))?;
            emit_picker_visibility(app, false);
            return Ok(false);
        }
    }
    open_window_picker_window_on_main(app)?;
    Ok(true)
}

/// Centered origin (logical) for a `size` window inside a `work_area`
/// logical rect `(x, y, width, height)`, clamped so the window never starts
/// above or left of the work area's top-left corner (a window larger than
/// the work area just hugs the corner instead of going off-screen).
///
/// `pub(crate)`: also reused by `share_notice` (#679) to horizontally
/// center the top-center share-started pill -- pass `height`/`ah` as `0.0`
/// to get pure horizontal centering out of the same formula (the `y` term
/// collapses to `ay` when both height terms are zero) instead of
/// reimplementing this math a second time.
pub(crate) fn centered_origin_in_work_area(work_area: (f64, f64, f64, f64), size: (f64, f64)) -> (f64, f64) {
    let (ax, ay, aw, ah) = work_area;
    let (w, h) = size;
    ((ax + (aw - w) / 2.0).max(ax), (ay + (ah - h) / 2.0).max(ay))
}

/// Logical top-left that centers a `width` x `height` secondary window on
/// the monitor the main window currently sits on (falling back to the
/// primary monitor). Windows 11 cascades every newly built unpositioned
/// window (CW_USEDEFAULT) — each rebuild after a close lands ~1 inch
/// down-right of the last until the cascade wraps back to the top. Every
/// rebuilt secondary window (the picker, the network cockpit) pins an
/// explicit position through this instead.
pub(crate) fn centered_secondary_window_position(
    app: &AppHandle,
    width: f64,
    height: f64,
) -> Option<(f64, f64)> {
    let monitor = app
        .get_webview_window("main")
        .and_then(|w| w.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor().max(1.0);
    let work_area = monitor.work_area();
    Some(centered_origin_in_work_area(
        (
            work_area.position.x as f64 / scale,
            work_area.position.y as f64 / scale,
            work_area.size.width as f64 / scale,
            work_area.size.height as f64 / scale,
        ),
        (width, height),
    ))
}

/// Start the desktop window-change watcher that drives the picker's
/// auto-refresh. Called whenever the picker becomes visible (rebuild or
/// re-show of the singleton). Windows-only: the watcher is a WinEvent-hook
/// module; other platforms have no watcher and this is a no-op. The watcher
/// self-terminates when the picker is no longer visible (closed/minimized),
/// so it is started here and never stopped explicitly.
#[cfg(target_os = "windows")]
fn start_watcher_for_picker(app: &AppHandle) {
    crate::window_change_watcher::start(app.clone());
}

#[cfg(not(target_os = "windows"))]
fn start_watcher_for_picker(_app: &AppHandle) {}

/// Hide the picker window when the user exits the meeting (user requirement:
/// the picker must not remain on the desktop after leaving). It stays alive
/// as a HIDDEN singleton — a re-open just re-shows it (no rebuild, picker
/// state preserved), and the window-change watcher self-terminates within
/// its visibility cadence once the picker is off screen. No-op when the
/// picker isn't open. Called from every leave path (explicit leave, forced
/// disconnect, quit).
pub(crate) fn hide_picker_on_meeting_exit(app: &AppHandle) {
    if let Some(picker) = app.get_webview_window(WINDOW_PICKER_LABEL) {
        let _ = picker.hide();
        emit_picker_visibility(app, false);
    }
}

fn open_window_picker_window_on_main(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(WINDOW_PICKER_LABEL).is_some() {
        show_picker_without_focus(app);
        start_watcher_for_picker(app);
        emit_picker_opened(app);
        emit_picker_visibility(app, true);
        return Ok(());
    }

    let picker_builder = WebviewWindowBuilder::new(
        app,
        WINDOW_PICKER_LABEL,
        WebviewUrl::App("window-picker.html".into()),
    )
    .title("Petal - Share a Window")
    .inner_size(PICKER_WIDTH, PICKER_HEIGHT)
    .min_inner_size(560.0, 460.0)
    .decorations(false)
    .transparent(true)
    .visible(false)
    .focused(false)
    .focusable(false)
    .accept_first_mouse(true);
    // Windows: force WebView2 GPU acceleration (unsupported on macOS/Linux).
    #[cfg(target_os = "windows")]
    let picker_builder =
        picker_builder.additional_browser_args(crate::webview2_args::WEBVIEW2_ACCEL_ARGS);
    match picker_builder.build()
    {
        Ok(win) => {
            // Windows: opaque window with DWM-native corners (same as the main
            // window); macOS keeps its transparent + CSS-rounded panel.
            #[cfg(target_os = "windows")]
            crate::windows_corner::make_native_rounded(&win);
            // Windows 11 cascades every newly built unpositioned window
            // (CW_USEDEFAULT): each picker rebuild after a close lands ~1
            // inch down-right of the last until the cascade wraps back to
            // the top — the observed open/close drift. Pin an explicit
            // position instead: center on the monitor the main window
            // currently sits on, so the picker always opens in the same
            // spot near the user's meeting window.
            if let Some((x, y)) =
                centered_secondary_window_position(app, PICKER_WIDTH, PICKER_HEIGHT)
            {
                let _ = win.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
            }
            show_picker_without_focus(app);
            start_watcher_for_picker(app);
            emit_picker_opened(app);
            emit_picker_visibility(app, true);
            Ok(())
        }
        Err(e) => {
            if app.get_webview_window(WINDOW_PICKER_LABEL).is_some() {
                show_picker_without_focus(app);
                start_watcher_for_picker(app);
                emit_picker_opened(app);
                emit_picker_visibility(app, true);
                Ok(())
            } else {
                Err(e.to_string())
            }
        }
    }?;

    Ok(())
}

#[cfg(test)]
mod tests {
    // The picker is rebuilt on every open (the frontend destroys it on
    // close), so the position math must be deterministic — Windows 11
    // cascades unpositioned windows on each rebuild, which was the
    // open/close drift.
    #[test]
    fn picker_centers_inside_work_area() {
        // 1920x1080 work area, 820x700 picker -> dead center.
        assert_eq!(
            super::centered_origin_in_work_area((0.0, 0.0, 1920.0, 1080.0), (820.0, 700.0)),
            (550.0, 190.0)
        );
        // A picker larger than the work area clamps to its top-left corner
        // instead of going off-screen.
        assert_eq!(
            super::centered_origin_in_work_area((100.0, 50.0, 800.0, 600.0), (820.0, 700.0)),
            (100.0, 50.0)
        );
        // A secondary monitor's non-zero origin offsets the center.
        assert_eq!(
            super::centered_origin_in_work_area((1920.0, 0.0, 1920.0, 1080.0), (820.0, 700.0)),
            (2470.0, 190.0)
        );
    }
    #[cfg(all(
        target_os = "macos",
        any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
    ))]
    fn test_shareable_window(
        window_id: u32,
        app_name: &str,
        app_pid: i32,
    ) -> crate::window_source::ShareableWindow {
        crate::window_source::ShareableWindow {
            window_id,
            title: None,
            app_name: app_name.to_string(),
            app_bundle_id: format!("test.{app_name}"),
            app_pid,
            app_icon_base64: None,
            kind: None,
        }
    }

    #[cfg(all(
        target_os = "macos",
        any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
    ))]
    #[test]
    fn autotest_picker_target_parser_is_explicit() {
        std::env::set_var("PETAL_AUTOTEST_PICKER_TARGET", "owner:TextEdit");
        assert_eq!(
            super::autotest_picker_target_from_env(),
            Some(super::AutotestPickerTarget::Owner("TextEdit".to_string()))
        );
        std::env::set_var("PETAL_AUTOTEST_PICKER_TARGET", "pid:42");
        assert_eq!(
            super::autotest_picker_target_from_env(),
            Some(super::AutotestPickerTarget::Pid(42))
        );
        std::env::set_var("PETAL_AUTOTEST_PICKER_TARGET", "window:123");
        assert_eq!(
            super::autotest_picker_target_from_env(),
            Some(super::AutotestPickerTarget::Window(123))
        );
        std::env::remove_var("PETAL_AUTOTEST_PICKER_TARGET");
    }

    #[cfg(all(
        target_os = "macos",
        any(debug_assertions, feature = "autotest", feature = "cockpit-privileged")
    ))]
    #[test]
    fn autotest_picker_target_requires_one_exact_shareable_window() {
        let windows = vec![
            test_shareable_window(10, "TextEdit", 42),
            test_shareable_window(11, "TextEdit", 42),
            test_shareable_window(12, "Preview", 99),
        ];

        assert_eq!(
            super::resolve_autotest_picker_target(
                &windows,
                &super::AutotestPickerTarget::Window(11)
            ),
            Ok(11)
        );
        for target in [
            super::AutotestPickerTarget::Owner("TextEdit".to_string()),
            super::AutotestPickerTarget::Pid(42),
        ] {
            assert!(super::resolve_autotest_picker_target(&windows, &target)
                .expect_err("ambiguous broad target must be rejected")
                .contains("matched multiple"));
        }
        assert!(super::resolve_autotest_picker_target(
            &windows,
            &super::AutotestPickerTarget::Window(404)
        )
        .expect_err("missing target must be rejected")
        .contains("no shareable window matched"));
    }

    #[cfg(all(
        target_os = "macos",
        not(any(debug_assertions, feature = "autotest", feature = "cockpit-privileged"))
    ))]
    #[test]
    fn release_ignores_picker_target_env() {
        std::env::set_var("PETAL_AUTOTEST_PICKER_TARGET", "owner:TextEdit");
        assert!(super::autotest_picker_target_from_env().is_none());
        std::env::remove_var("PETAL_AUTOTEST_PICKER_TARGET");
    }
}
