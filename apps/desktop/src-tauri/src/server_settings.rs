//! Token-server and invite-origin user setting (#310): pick a backend
//! address without rebuilding the client. Same persistence shape as
//! `debug_settings.rs` (JSON in the app data dir behind a
//! `OnceLock<Mutex<...>>`) rather than `localStorage` — Settings lives in
//! its own Tauri webview, and the token client that reads the URL is Rust.
//!
//! Resolution order for the token server is owned by
//! `transport::token::backend_base_url`: runtime `PETAL_BACKEND_URL`, then
//! this override, then the URL baked at compile time. An empty runtime env
//! still means "no backend" (debug local-mint opt-out) and does not fall
//! through to the saved value.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "server-settings.json";
const MAX_ORIGIN_CHARS: usize = 2048;

/// Persisted overrides. `None` / missing field = use the build or env.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_origin: Option<String>,
}

/// What Settings and invite-copy surfaces show. Includes the resolved
/// values so the UI can say which server this launch will actually call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettingsView {
    pub backend_url: Option<String>,
    pub invite_origin: Option<String>,
    pub effective_backend_url: Option<String>,
    pub effective_invite_origin: String,
    /// `env` | `settings` | `build` | `none`
    pub backend_source: String,
}

struct Store {
    path: PathBuf,
    settings: ServerSettings,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

/// Load settings from the app data dir. Called once at startup, on every
/// platform.
pub fn initialize(app_data_dir: &Path) {
    let path = app_data_dir.join(SETTINGS_FILE);
    let settings = read_from(&path).unwrap_or_default();
    if let Ok(mut guard) = store().lock() {
        *guard = Some(Store { path, settings });
    }
}

fn read_from(path: &Path) -> Option<ServerSettings> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: ServerSettings = serde_json::from_str(&raw).ok()?;
    Some(ServerSettings {
        backend_url: parsed
            .backend_url
            .as_deref()
            .and_then(normalize_server_origin)
            .map(|origin| origin.to_string()),
        invite_origin: parsed
            .invite_origin
            .as_deref()
            .and_then(normalize_server_origin)
            .map(|origin| origin.to_string()),
    })
}

pub fn current() -> ServerSettings {
    store()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.settings.clone()))
        .unwrap_or_default()
}

/// Saved token-server override, if any. `backend_base_url` consults this
/// after the runtime env and before the compile-time bake.
pub fn backend_url() -> Option<String> {
    current().backend_url
}

pub fn invite_origin() -> Option<String> {
    current().invite_origin
}

pub fn update(mutate: impl FnOnce(&mut ServerSettings)) -> Result<ServerSettings, String> {
    let mut guard = store().lock().map_err(|_| "settings lock poisoned")?;
    let store = guard.as_mut().ok_or("settings not initialized")?;
    mutate(&mut store.settings);
    persist(&store.path, &store.settings)?;
    Ok(store.settings.clone())
}

fn persist(path: &Path, settings: &ServerSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create settings dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(settings).map_err(|e| format!("serialize settings: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write settings: {e}"))
}

/// Mirror of `shared/logic/serverEndpoints.ts` `normalizeServerOrigin`.
/// Keep the two in lockstep: http(s) origin only, `/api` and `/api/token`
/// stripped as a courtesy, everything else refused.
pub fn normalize_server_origin(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ORIGIN_CHARS {
        return None;
    }
    let parsed = reqwest::Url::parse(trimmed).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_string();
    if host.is_empty() {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let path = parsed.path().trim_end_matches('/');
    if path != "" && path != "/api" && path != "/api/token" {
        return None;
    }
    Some(match parsed.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

fn parse_optional_origin(raw: Option<String>) -> Result<Option<String>, String> {
    match raw {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => normalize_server_origin(&value)
            .map(Some)
            .ok_or_else(|| {
                "Enter an http:// or https:// address, origin only — no path.".to_string()
            }),
    }
}

fn default_invite_origin() -> String {
    option_env!("VITE_PETAL_INVITE_ORIGIN")
        .and_then(|value| normalize_server_origin(value))
        .unwrap_or_else(|| "https://meet.petal.live".to_string())
}

pub fn view() -> ServerSettingsView {
    let settings = current();
    let (effective_backend_url, backend_source) =
        crate::transport::token::backend_base_url_source();
    ServerSettingsView {
        backend_url: settings.backend_url,
        invite_origin: settings.invite_origin.clone(),
        effective_backend_url,
        effective_invite_origin: settings
            .invite_origin
            .unwrap_or_else(default_invite_origin),
        backend_source,
    }
}

pub const SERVER_SETTINGS_CHANGED_EVENT: &str = "server-settings-changed";

#[tauri::command]
pub fn server_settings() -> ServerSettingsView {
    view()
}

#[tauri::command]
pub fn set_server_settings(
    app: tauri::AppHandle,
    backend_url: Option<String>,
    invite_origin: Option<String>,
) -> Result<ServerSettingsView, String> {
    use tauri::Emitter;
    let backend_url = parse_optional_origin(backend_url)?;
    let invite_origin = parse_optional_origin(invite_origin)?;
    update(|s| {
        s.backend_url = backend_url;
        s.invite_origin = invite_origin;
    })?;
    let settings = view();
    let _ = app.emit(SERVER_SETTINGS_CHANGED_EVENT, settings.clone());
    Ok(settings)
}

#[cfg(test)]
pub fn reset_store_for_tests() {
    if let Ok(mut guard) = store().lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "petal-server-settings-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn normalize_accepts_http_s_origins_and_strips_api_paths() {
        assert_eq!(
            normalize_server_origin(" https://petal.example.test/// "),
            Some("https://petal.example.test".to_string())
        );
        assert_eq!(
            normalize_server_origin("https://petal.example.test/api/token"),
            Some("https://petal.example.test".to_string())
        );
        assert_eq!(
            normalize_server_origin("http://127.0.0.1:8787"),
            Some("http://127.0.0.1:8787".to_string())
        );
        assert_eq!(normalize_server_origin("https://petal.example.test/extra"), None);
        assert_eq!(normalize_server_origin("file:///tmp"), None);
        assert_eq!(
            normalize_server_origin("https://user:pass@petal.example.test"),
            None
        );
        assert_eq!(normalize_server_origin(""), None);
    }

    #[test]
    fn defaults_are_empty() {
        assert_eq!(ServerSettings::default(), ServerSettings::default());
        assert!(ServerSettings::default().backend_url.is_none());
        assert!(ServerSettings::default().invite_origin.is_none());
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = unique_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE);
        persist(
            &path,
            &ServerSettings {
                backend_url: Some("https://petal.example.test".to_string()),
                invite_origin: Some("https://meet.example.test".to_string()),
            },
        )
        .unwrap();
        let loaded = read_from(&path).expect("settings should read back");
        assert_eq!(
            loaded.backend_url.as_deref(),
            Some("https://petal.example.test")
        );
        assert_eq!(
            loaded.invite_origin.as_deref(),
            Some("https://meet.example.test")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_file_falls_back_to_empty() {
        let dir = unique_dir("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE);
        std::fs::write(&path, "{not json").unwrap();
        assert!(read_from(&path).is_none());
        assert!(read_from(&path).unwrap_or_default().backend_url.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn initialize_then_update_persists_across_a_simulated_restart() {
        let dir = unique_dir("e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        reset_store_for_tests();
        initialize(&dir);
        assert!(backend_url().is_none(), "fresh app data dir must start empty");

        update(|s| {
            s.backend_url = Some("https://petal.example.test".to_string());
        })
        .unwrap();
        assert_eq!(backend_url().as_deref(), Some("https://petal.example.test"));

        initialize(&dir);
        assert_eq!(
            backend_url().as_deref(),
            Some("https://petal.example.test"),
            "server URL must persist across restart"
        );

        reset_store_for_tests();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
