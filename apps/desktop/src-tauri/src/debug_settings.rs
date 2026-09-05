//! Debug-mode user setting (#669): the master switch that gates the remote-
//! window header's Debug button. Off by default -- the button is a
//! diagnostic affordance (frame counters, glass-to-glass estimates, packet
//! loss) most users never need cluttering their header.
//!
//! Same persistence shape as `ai_chat/settings.rs` (see that module's docs
//! for the full rationale): a JSON file in the app data dir behind a
//! `OnceLock<Mutex<...>>`, initialized at startup, rather than `localStorage`
//! -- each Tauri webview is its own JS realm, so a Settings-window toggle
//! stored in one webview's localStorage would never reach an already-open
//! compositor surface webview. Unlike AI chat's store this one carries no
//! secret, so it skips the 0600/atomic-rename machinery that exists there
//! specifically to protect an API key -- a plain write is enough here.
//!
//! Unlike `ai_chat::settings` (macOS-only: AI chat needs the accessibility
//! tree), this setting is initialized on EVERY platform -- the Debug button
//! exists on both the macOS (`compositor.rs`) and Windows
//! (`windows_compositor.rs`) remote-window headers.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "debug-settings.json";

/// Persisted shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DebugSettings {
    /// Master switch for the remote-window header's Debug button. **Off by
    /// default.**
    #[serde(default)]
    pub enabled: bool,
}

struct Store {
    path: PathBuf,
    settings: DebugSettings,
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

fn read_from(path: &Path) -> Option<DebugSettings> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Current settings (defaults -- off -- if never initialized or the file is
/// missing/corrupt: a read that fails must fail CLOSED).
pub fn current() -> DebugSettings {
    store()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.settings.clone()))
        .unwrap_or_default()
}

/// Whether debug mode is switched on.
pub fn is_enabled() -> bool {
    current().enabled
}

/// Persist a mutation. Returns the new settings for the caller to relay
/// (e.g. as an event payload) without a second `current()` round trip.
pub fn update(mutate: impl FnOnce(&mut DebugSettings)) -> Result<DebugSettings, String> {
    let mut guard = store().lock().map_err(|_| "settings lock poisoned")?;
    let store = guard.as_mut().ok_or("settings not initialized")?;
    mutate(&mut store.settings);
    persist(&store.path, &store.settings)?;
    Ok(store.settings.clone())
}

fn persist(path: &Path, settings: &DebugSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create settings dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(settings).map_err(|e| format!("serialize settings: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write settings: {e}"))
}

// ---- Tauri command surface --------------------------------------------------

/// `debug-mode-changed`'s event name. Emitted by [`set_debug_mode`] so an
/// ALREADY-OPEN remote-window surface webview picks up a Settings-window
/// toggle live, without needing to be reopened or re-navigated -- each Tauri
/// webview is its own JS realm, so nothing short of an explicit event or a
/// fresh `invoke` ever crosses that boundary. `ai_chat_set_enabled` never grew
/// this (a known, documented gap, #669); this setting does not repeat it.
pub const DEBUG_MODE_CHANGED_EVENT: &str = "debug-mode-changed";

/// Read the current settings for the frontend.
#[tauri::command]
pub fn debug_mode_settings() -> DebugSettings {
    current()
}

/// Toggle debug mode. Emits [`DEBUG_MODE_CHANGED_EVENT`] so every open
/// remote-window header updates live -- see that constant's docs for why.
#[tauri::command]
pub fn set_debug_mode(app: tauri::AppHandle, enabled: bool) -> Result<DebugSettings, String> {
    use tauri::Emitter;
    let settings = update(|s| s.enabled = enabled)?;
    let _ = app.emit(DEBUG_MODE_CHANGED_EVENT, settings.clone());
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off() {
        assert!(
            !DebugSettings::default().enabled,
            "debug mode must be OFF by default"
        );
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "petal-debug-settings-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE);

        persist(&path, &DebugSettings { enabled: true }).unwrap();
        let loaded = read_from(&path).expect("settings should read back");
        assert!(loaded.enabled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_file_falls_back_to_off() {
        let dir = std::env::temp_dir().join(format!(
            "petal-debug-settings-corrupt-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE);
        std::fs::write(&path, "{not json").unwrap();
        assert!(read_from(&path).is_none());
        assert!(!read_from(&path).unwrap_or_default().enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full lifecycle a real session goes through: `initialize()` at
    /// startup (empty dir, so defaults), `update()` from a Settings toggle,
    /// then a second `initialize()` simulating an app restart -- the
    /// persisted value must survive it.
    #[test]
    fn initialize_then_update_persists_across_a_simulated_restart() {
        let dir = std::env::temp_dir().join(format!(
            "petal-debug-settings-e2e-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        initialize(&dir);
        assert!(!is_enabled(), "fresh app data dir must start disabled");

        let updated = update(|s| s.enabled = true).unwrap();
        assert!(updated.enabled);
        assert!(is_enabled());

        // Simulate an app restart: re-initialize from the same dir into a
        // fresh in-memory store.
        initialize(&dir);
        assert!(is_enabled(), "debug mode must persist across restart");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
