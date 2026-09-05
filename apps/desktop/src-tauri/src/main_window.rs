//! Navigation helpers for secondary native webviews that need to bring the
//! primary Petal window forward without reimplementing Svelte routing.

use tauri::{AppHandle, Manager};

#[tauri::command]
pub async fn open_main_route(app: AppHandle, route: String) -> Result<(), String> {
    let route = allowed_route(&route).ok_or_else(|| format!("route not allowed: {route}"))?;
    open_route(&app, &route).await
}

/// Bring the main window back WITHOUT navigating it.
///
/// The menubar "Open Petal" row must not reuse `open_main_route`: navigating a
/// webview that is currently on `/meeting/<room>` runs that route's `onDestroy`
/// (stops the local camera, restores the home window) while the user is still
/// in the room -- `leave_room` is never called, so they stay joined with their
/// camera cut. "Open Petal" promises to show Petal, so it shows it.
///
/// Same reveal gate as `open_route`: during a #636 cold start the reveal gate
/// owns the first show, and this is a no-op until then.
#[tauri::command]
pub async fn show_main_window(app: AppHandle) -> Result<(), String> {
    if !crate::main_window_revealed() {
        return Ok(());
    }
    crate::show_and_activate_main_window(&app, "menubar-open-petal");
    Ok(())
}

fn allowed_route(route: &str) -> Option<String> {
    let route = route.trim();
    if route == "/main" || route == "/settings" {
        Some(route.to_string())
    } else if let Some(room) = route.strip_prefix("/meeting/") {
        if room.is_empty() || room.contains('?') || room.contains('#') {
            None
        } else {
            Some(route.to_string())
        }
    } else {
        None
    }
}

const NAVIGATE_JS_TEMPLATE: &str = r#"(() => {
  const route = __PETAL_ROUTE__;
  try {
    const navigate = window.__petalNavigate;
    if (typeof navigate === 'function') {
      navigate(route);
      return;
    }
  } catch (_) {}
  window.location.assign(route);
})();"#;

/// #782: `location.assign` is a FULL document navigation -- it reloads the SPA, which
/// tears down a live meeting route and restarts the camera at a different resolution than
/// the published track. Prefer the SvelteKit client-side router; fall back only when the
/// SPA has not booted.
fn navigate_js(route: &str) -> String {
    let route_json = serde_json::to_string(route).unwrap_or_else(|_| "\"/main\"".into());
    NAVIGATE_JS_TEMPLATE.replace("__PETAL_ROUTE__", &route_json)
}

async fn open_route(app: &AppHandle, route: &str) -> Result<(), String> {
    for _attempt in 0..25 {
        if let Some(window) = app.get_webview_window("main") {
            let js = navigate_js(route);
            window
                .eval(js.as_str())
                .map_err(|e| format!("failed to navigate main webview: {e}"))?;
            // No unconditional `window.show()` -- see deep_link.rs's note
            // (#636): during a cold launch the reveal gate owns the first show,
            // and showing here races first paint. But once the reveal HAS
            // happened, a window that is not on screen was hidden by the user
            // (red traffic dot), and this route is one of the ways back.
            if crate::main_window_revealed() {
                crate::show_and_activate_main_window(app, "open-main-route");
            }
            let _ = window.unminimize();
            let _ = window.set_focus();
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    Err(format!("main window never appeared for route {route}"))
}

#[cfg(test)]
mod tests {
    use super::{allowed_route, navigate_js};

    #[test]
    fn allows_expected_app_routes() {
        assert_eq!(allowed_route("/main"), Some("/main".to_string()));
        assert_eq!(allowed_route(" /main "), Some("/main".to_string()));
        assert_eq!(allowed_route("/settings"), Some("/settings".to_string()));
        assert_eq!(
            allowed_route("/meeting/webtest"),
            Some("/meeting/webtest".to_string())
        );
    }

    #[test]
    fn rejects_external_or_unexpected_routes() {
        assert_eq!(allowed_route("https://example.com"), None);
        assert_eq!(allowed_route("/meeting/"), None);
        assert_eq!(allowed_route("/meeting/webtest?x=1"), None);
        assert_eq!(allowed_route("/meeting/webtest#hash"), None);
        assert_eq!(allowed_route("/network-cockpit"), None);
        assert_eq!(allowed_route("javascript:alert(1)"), None);
    }

    #[test]
    fn navigation_prefers_spa_hook_and_keeps_cold_start_fallback() {
        let js = navigate_js("/settings");
        assert!(js.contains("__petalNavigate"));
        assert!(js.contains("location.assign"));
        assert!(js.contains("const route = \"/settings\";"));
        assert!(!js.contains("const route = /settings;"));
    }

    #[test]
    fn navigation_json_escapes_allowed_meeting_routes() {
        let route = "/meeting/room\"quoted";
        assert_eq!(allowed_route(route), Some(route.to_string()));
        let js = navigate_js(route);
        assert!(js.contains(r#"const route = "/meeting/room\"quoted";"#));
    }
}
