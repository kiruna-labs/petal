//! AI chat user settings: the master switch and the optional bring-your-own
//! Gemini key.
//!
//! Follows `share_priority.rs`'s pattern — a JSON file in the app data dir
//! behind a `OnceLock<Mutex<…>>`, initialized at startup — rather than
//! `localStorage`, because the key must never be readable from a webview and
//! must survive the frontend's factory-reset sweep.
//!
//! ## What the key file does and does not protect
//!
//! The key is written to a file with `0600` permissions (owner read/write
//! only). That stops another *user* on the machine from reading it, and keeps
//! it out of the webview, logs, and Sentry. It does **not** protect against
//! anything running as this user — such code can read the file directly.
//!
//! The takt reference encrypts this file with AES-256-GCM, but stores the key
//! *next to* the ciphertext, so against a file-read attacker it is obfuscation
//! rather than protection; its real benefit is avoiding accidental plaintext
//! exposure (a backup grep, a shared screen). `0600` plus never rendering the
//! key back to the UI covers the same ground without a crypto dependency whose
//! guarantee would be overstated. Recorded as a deliberate deviation on #656.
//! Keychain was rejected for the same reason takt rejected it: ad-hoc-signed
//! dev builds get a fresh code identity on every rebuild, so it would prompt
//! constantly during development.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "ai-chat-settings.json";

/// Persisted shape. The key lives in the same file; see the module docs for
/// exactly what that does and doesn't buy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatSettings {
    /// Master switch. **Off by default** — enabling it is the sharer's consent
    /// for room peers to start AI chat on windows they share (#657).
    #[serde(default)]
    pub enabled: bool,
    /// Optional user-supplied Gemini API key (bring-your-own-key mode). Never
    /// sent to the frontend; see [`Redacted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl AiChatSettings {
    /// Which credential path a session should take. Hosted (backend-minted
    /// ephemeral token) is preferred when available; a user key is the
    /// fallback and the only option for third-party OSS builds, which have no
    /// baked backend.
    pub fn has_own_key(&self) -> bool {
        self.api_key.as_ref().is_some_and(|k| !k.trim().is_empty())
    }
}

/// What the frontend is allowed to see: never the key itself, only whether one
/// is set. Prevents a compromised or merely curious webview from reading it
/// back out.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Redacted {
    pub enabled: bool,
    pub has_api_key: bool,
}

impl From<&AiChatSettings> for Redacted {
    fn from(s: &AiChatSettings) -> Self {
        Redacted {
            enabled: s.enabled,
            has_api_key: s.has_own_key(),
        }
    }
}

struct Store {
    path: PathBuf,
    settings: AiChatSettings,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

/// Load settings from the app data dir. Called once at startup.
pub fn initialize(app_data_dir: &Path) {
    let path = app_data_dir.join(SETTINGS_FILE);
    let settings = read_from(&path).unwrap_or_default();
    if let Ok(mut guard) = store().lock() {
        *guard = Some(Store { path, settings });
    }
}

fn read_from(path: &Path) -> Option<AiChatSettings> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Current settings (defaults if never initialized).
pub fn current() -> AiChatSettings {
    store()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.settings.clone()))
        .unwrap_or_default()
}

/// Whether AI chat is switched on. Every entry point consults this; when false
/// the feature must be entirely invisible and no session can exist.
pub fn is_enabled() -> bool {
    current().enabled
}

/// Persist a mutation. Returns the redacted view for the UI.
pub fn update(mutate: impl FnOnce(&mut AiChatSettings)) -> Result<Redacted, String> {
    let mut guard = store().lock().map_err(|_| "settings lock poisoned")?;
    let store = guard.as_mut().ok_or("settings not initialized")?;
    mutate(&mut store.settings);
    persist(&store.path, &store.settings)?;
    Ok(Redacted::from(&store.settings))
}

fn persist(path: &Path, settings: &AiChatSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create settings dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(settings).map_err(|e| format!("serialize settings: {e}"))?;

    // Owner-only (0600) from the moment the file exists, and an atomic
    // rename into place -- not write-then-chmod. `std::fs::write` creates
    // the file at the process's default (umask-derived) mode and only
    // narrows it afterward, so a settings write briefly leaves an API key
    // world/group-readable, and a crash or concurrent read between the two
    // calls can observe that window (or, on `write`'s non-atomic partial
    // write, a torn file). Same pattern the takt reference uses for its own
    // credential file — mode fixed at creation, then a rename that can only
    // ever be seen as "hasn't happened yet" or "already complete."
    let tmp_path = tmp_path_for(path);
    let _ = std::fs::remove_file(&tmp_path); // stale leftover from a prior crash, if any
    write_owner_only(&tmp_path, json.as_bytes()).map_err(|e| format!("write settings: {e}"))?;
    std::fs::rename(&tmp_path, path).map_err(|e| format!("rename settings into place: {e}"))?;
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Create `path` with owner-only (0600) permissions set at creation time —
/// not applied after the fact — and write `contents` before returning.
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_keyless() {
        let s = AiChatSettings::default();
        assert!(!s.enabled, "AI chat must be OFF by default");
        assert!(!s.has_own_key());
    }

    #[test]
    fn blank_key_does_not_count_as_configured() {
        for blank in ["", "   ", "\t\n"] {
            let s = AiChatSettings {
                enabled: true,
                api_key: Some(blank.to_string()),
            };
            assert!(!s.has_own_key(), "{blank:?} should not count as a key");
        }
    }

    #[test]
    fn redacted_view_never_carries_the_key() {
        let s = AiChatSettings {
            enabled: true,
            api_key: Some("AIza-super-secret".into()),
        };
        let redacted = Redacted::from(&s);
        assert!(redacted.enabled);
        assert!(redacted.has_api_key);
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("AIza-super-secret"),
            "the key leaked into the frontend payload: {json}"
        );
        assert!(!json.to_lowercase().contains("apikey\":\""), "{json}");
    }

    #[test]
    fn roundtrips_through_disk_and_stays_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "petal-ai-chat-settings-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE);

        let settings = AiChatSettings {
            enabled: true,
            api_key: Some("k".into()),
        };
        persist(&path, &settings).unwrap();

        let loaded = read_from(&path).expect("settings should read back");
        assert!(loaded.enabled);
        assert!(loaded.has_own_key());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression test for the write-then-chmod bug: `std::fs::write(path,
    /// json)` truncates the EXISTING inode at `path` in place, so a
    /// concurrent reader (or a crash mid-write) could observe a torn file at
    /// its own final path, and the mode narrowing that followed was a
    /// separate, non-atomic step. The final on-disk *mode* ends up 0600
    /// either way (that's what `roundtrips_through_disk_and_stays_owner_only`
    /// already checks) — the property that specifically distinguishes
    /// "create fresh + rename over" from "open existing + truncate + write"
    /// is the file's *identity*: a rename-based replace always leaves a
    /// brand new inode at `path`, never the one that was there before.
    /// Reverting `persist` back to `std::fs::write(path, json);
    /// restrict_permissions(path)` makes the inode-changes assertion below
    /// fail (same inode reused both times) even though the final mode still
    /// happens to read 0600.
    #[test]
    #[cfg(unix)]
    fn key_file_persist_replaces_the_file_rather_than_truncating_it_in_place() {
        use std::os::unix::fs::MetadataExt;

        let dir = std::env::temp_dir().join(format!(
            "petal-ai-chat-settings-atomicity-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SETTINGS_FILE);

        let settings = AiChatSettings {
            enabled: true,
            api_key: Some("k".into()),
        };
        persist(&path, &settings).unwrap();
        assert!(
            !tmp_path_for(&path).exists(),
            "persist() left a temp file behind: {:?}",
            tmp_path_for(&path)
        );
        let first_inode = std::fs::metadata(&path).unwrap().ino();

        persist(&path, &settings).unwrap();
        assert!(!tmp_path_for(&path).exists());
        let second_inode = std::fs::metadata(&path).unwrap().ino();

        assert_ne!(
            first_inode, second_inode,
            "a second persist() reused the first write's inode -- it truncated \
             the existing file in place instead of replacing it via rename"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_file_falls_back_to_safe_defaults() {
        // A corrupt settings file must not enable the feature.
        let dir = std::env::temp_dir().join(format!(
            "petal-ai-chat-corrupt-test-{}",
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
}
