//! Live-streaming user settings, in `stream-settings.json` in the app data
//! dir (the same per-feature JSON pattern as `ai_chat::settings`):
//!
//! * `alwaysStreamerMode`: mask meeting names and codes even when this
//!   computer isn't streaming (for people who stream with OBS or similar).
//! * `keepOffLiveStreams`: this computer's `petal.stream-optout` attribute,
//!   which keeps its audio off anyone else's live stream.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::sync_ext::MutexExt;

pub const STREAM_SETTINGS_FILE: &str = "stream-settings.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSettings {
    #[serde(default)]
    pub always_streamer_mode: bool,
    #[serde(default)]
    pub keep_off_live_streams: bool,
}

struct Store {
    path: PathBuf,
    settings: StreamSettings,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

fn read_from(path: &Path) -> StreamSettings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn persist(path: &Path, settings: &StreamSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create settings dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| format!("serialize settings: {e}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, json).map_err(|e| format!("write settings: {e}"))?;
    std::fs::rename(&temp, path).map_err(|e| format!("rename settings into place: {e}"))
}

/// Load settings from the app data dir. Called once at startup.
pub fn initialize(app_data_dir: &Path) {
    let path = app_data_dir.join(STREAM_SETTINGS_FILE);
    let settings = read_from(&path);
    *store().lock_unpoisoned() = Some(Store { path, settings });
}

/// Current settings (defaults if never initialized).
pub fn current() -> StreamSettings {
    store().lock_unpoisoned().as_ref().map(|store| store.settings).unwrap_or_default()
}

/// Persist a change; returns the settings before and after.
pub fn update(
    mutate: impl FnOnce(&mut StreamSettings),
) -> Result<(StreamSettings, StreamSettings), String> {
    let mut guard = store().lock_unpoisoned();
    let store = guard.as_mut().ok_or("stream settings not initialized")?;
    let before = store.settings;
    let mut after = before;
    mutate(&mut after);
    persist(&store.path, &after)?;
    store.settings = after;
    Ok((before, after))
}

#[tauri::command]
pub fn stream_settings() -> StreamSettings {
    current()
}

/// Save the Live streaming settings. A change to the forced streamer mode
/// re-emits `streamer-mode-changed`; a change to the opt-out rewrites this
/// computer's `petal.stream-optout` attribute in the joined room.
#[tauri::command]
pub async fn stream_settings_set(
    app: tauri::AppHandle,
    settings: StreamSettings,
) -> Result<StreamSettings, String> {
    let (before, after) = update(|current| *current = settings)?;
    if before.always_streamer_mode != after.always_streamer_mode {
        super::streamer_mode::set_forced(&app, after.always_streamer_mode);
    }
    if before.keep_off_live_streams != after.keep_off_live_streams {
        if let Some(room) = super::live_state::current_room() {
            super::live_state::publish_opt_out(&room, after.keep_off_live_streams).await;
        }
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_defaults_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("petal-stream-settings-{}", std::process::id()));
        let path = dir.join(STREAM_SETTINGS_FILE);
        assert_eq!(read_from(&path), StreamSettings::default());
        let saved = StreamSettings { always_streamer_mode: true, keep_off_live_streams: true };
        persist(&path, &saved).unwrap();
        assert_eq!(read_from(&path), saved);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"alwaysStreamerMode\": true"), "{raw}");
        assert!(raw.contains("\"keepOffLiveStreams\": true"), "{raw}");
        // A missing key keeps its default.
        std::fs::write(&path, r#"{"keepOffLiveStreams":true}"#).unwrap();
        assert_eq!(
            read_from(&path),
            StreamSettings { always_streamer_mode: false, keep_off_live_streams: true }
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
