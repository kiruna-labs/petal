//! Installed-plugin state on disk (plugins/README.md §2.2):
//!
//!   <app_data_dir>/plugins/plugins.json                  installed state
//!   <app_data_dir>/plugins/<id>/<version>/bundle.json    verified bundle
//!
//! Same store shape as `ai_chat/settings.rs`: one `OnceLock<Mutex<…>>`
//! initialized at startup, atomic write (temp file + rename). Ids and
//! versions are validated before any path join so a hostile index cannot
//! escape the plugins directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::bus::{is_plugin_id, is_release_version};

pub const PLUGINS_DIR: &str = "plugins";
pub const STATE_FILE: &str = "plugins.json";
pub const BUNDLE_FILE: &str = "bundle.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledRecord {
    pub version: String,
    pub enabled: bool,
    /// `registry` today; `dev` arrives with developer mode (I-10).
    pub source: String,
    pub granted_permissions: Vec<String>,
    pub installed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// The newest registry index this install has accepted (anti-rollback, review #3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySeen {
    pub url: String,
    /// `generatedAt` of the index, as unix milliseconds.
    pub generated_at_ms: u64,
    /// `timestamp:` from the index signature's trusted comment, unix seconds.
    pub signed_at_s: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledState {
    #[serde(default)]
    pub plugins: BTreeMap<String, InstalledRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_seen: Option<RegistrySeen>,
}

struct Store {
    dir: PathBuf,
    state: InstalledState,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

/// Called once from `setup()`; loads `plugins.json` (missing/corrupt = empty).
pub fn initialize(app_data_dir: &Path) {
    let dir = app_data_dir.join(PLUGINS_DIR);
    let state = read_state(&dir.join(STATE_FILE));
    log::info!(
        "plugins::store: loaded {} installed plugin record(s) from {}",
        state.plugins.len(),
        dir.display()
    );
    *store().lock().unwrap_or_else(|p| p.into_inner()) = Some(Store { dir, state });
}

fn read_state(path: &Path) -> InstalledState {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!(
                "plugins::store: {} unreadable ({e}); starting empty",
                path.display()
            );
            InstalledState::default()
        }),
        Err(_) => InstalledState::default(),
    }
}

fn with_store<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    let store = guard
        .as_mut()
        .ok_or_else(|| "plugin store is not initialized".to_string())?;
    f(store)
}

fn persist(store: &Store) -> Result<(), String> {
    std::fs::create_dir_all(&store.dir).map_err(|e| format!("create plugins dir: {e}"))?;
    let json = serde_json::to_vec_pretty(&store.state).map_err(|e| e.to_string())?;
    write_atomic(&store.dir.join(STATE_FILE), &json)
}

/// Temp file + rename so a crash mid-write can never leave a truncated file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut file =
            owner_only_file(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        file.write_all(contents)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

fn owner_only_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
}

/// `<plugins dir>/<id>/<version>` -- only for validated ids/versions.
pub fn version_dir(base: &Path, id: &str, version: &str) -> Result<PathBuf, String> {
    if !is_plugin_id(id) {
        return Err(format!("invalid plugin id: {id}"));
    }
    if !is_release_version(version) {
        return Err(format!("invalid version: {version}"));
    }
    Ok(base.join(id).join(version))
}

pub fn list() -> Result<InstalledState, String> {
    with_store(|s| Ok(s.state.clone()))
}

pub fn get(id: &str) -> Result<Option<InstalledRecord>, String> {
    with_store(|s| Ok(s.state.plugins.get(id).cloned()))
}

/// Write the verified bundle and record the install. Replaces any previous version's record;
/// older version directories are removed best-effort.
pub fn install(id: &str, record: InstalledRecord, bundle_bytes: &[u8]) -> Result<(), String> {
    with_store(|s| {
        let dir = version_dir(&s.dir, id, &record.version)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        write_atomic(&dir.join(BUNDLE_FILE), bundle_bytes)?;
        if let Some(previous) = s.state.plugins.get(id) {
            if previous.version != record.version {
                if let Ok(old) = version_dir(&s.dir, id, &previous.version) {
                    let _ = std::fs::remove_dir_all(old);
                }
            }
        }
        s.state.plugins.insert(id.to_string(), record);
        persist(s)
    })
}

pub fn set_enabled(id: &str, enabled: bool) -> Result<(), String> {
    with_store(|s| {
        let record = s
            .state
            .plugins
            .get_mut(id)
            .ok_or_else(|| format!("plugin {id} is not installed"))?;
        record.enabled = enabled;
        persist(s)
    })
}

pub fn uninstall(id: &str) -> Result<(), String> {
    with_store(|s| {
        if !is_plugin_id(id) {
            return Err(format!("invalid plugin id: {id}"));
        }
        s.state.plugins.remove(id);
        let _ = std::fs::remove_dir_all(s.dir.join(id));
        persist(s)
    })
}

/// The stored bundle text for an installed plugin, re-hashed against the sha256 recorded at
/// install (review #5: anything that can write one file under the plugins directory must not
/// get code execution). The frontend additionally re-validates the manifest before booting it.
pub fn read_bundle(id: &str) -> Result<String, String> {
    with_store(|s| {
        let record = s
            .state
            .plugins
            .get(id)
            .ok_or_else(|| format!("plugin {id} is not installed"))?;
        let path = version_dir(&s.dir, id, &record.version)?.join(BUNDLE_FILE);
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        match &record.sha256 {
            Some(expected) if super::sha256_hex(&bytes) != *expected => {
                return Err(format!(
                    "stored bundle for {id} does not match the sha256 recorded at install; refusing to load it"
                ));
            }
            None if record.source != "dev" => {
                return Err(format!(
                    "installed record for {id} has no sha256; refusing to load it"
                ));
            }
            _ => {}
        }
        String::from_utf8(bytes).map_err(|_| format!("stored bundle for {id} is not UTF-8"))
    })
}

pub fn registry_seen() -> Result<Option<RegistrySeen>, String> {
    with_store(|s| Ok(s.state.registry_seen.clone()))
}

pub fn record_registry_seen(seen: RegistrySeen) -> Result<(), String> {
    with_store(|s| {
        s.state.registry_seen = Some(seen);
        persist(s)
    })
}

/// Every test that touches the process-global store (here and in `registry`) holds this.
#[cfg(test)]
pub(crate) fn test_lock() -> &'static Mutex<()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    &TEST_LOCK
}

#[cfg(test)]
pub(crate) fn initialize_for_tests(dir: &Path) {
    *store().lock().unwrap_or_else(|p| p.into_inner()) = Some(Store {
        dir: dir.to_path_buf(),
        state: read_state(&dir.join(STATE_FILE)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "petal-plugins-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    fn record_for(version: &str, bundle: &[u8]) -> InstalledRecord {
        InstalledRecord {
            version: version.into(),
            enabled: true,
            source: "registry".into(),
            granted_permissions: vec!["meeting:read".into()],
            installed_at_ms: 1,
            sha256: Some(super::super::sha256_hex(bundle)),
        }
    }

    #[test]
    fn version_dir_refuses_path_games() {
        let base = Path::new("/tmp/base");
        assert!(version_dir(base, "../x", "1.0.0").is_err());
        assert!(version_dir(base, "petal.x", "1.0.0/../2.0.0").is_err());
        assert_eq!(
            version_dir(base, "petal.x", "1.0.0").unwrap(),
            base.join("petal.x").join("1.0.0")
        );
    }

    #[test]
    fn install_persists_and_survives_reload_and_upgrade_removes_old_dir() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir();
        initialize_for_tests(&dir);
        install("petal.x", record_for("1.0.0", b"{\"v\":1}"), b"{\"v\":1}").unwrap();
        assert_eq!(read_bundle("petal.x").unwrap(), "{\"v\":1}");
        assert!(dir.join("petal.x/1.0.0/bundle.json").exists());

        // Reload from disk.
        initialize_for_tests(&dir);
        assert_eq!(list().unwrap().plugins["petal.x"].version, "1.0.0");

        install("petal.x", record_for("1.1.0", b"{\"v\":2}"), b"{\"v\":2}").unwrap();
        assert!(
            !dir.join("petal.x/1.0.0").exists(),
            "old version dir removed"
        );
        assert_eq!(read_bundle("petal.x").unwrap(), "{\"v\":2}");

        set_enabled("petal.x", false).unwrap();
        assert!(!get("petal.x").unwrap().unwrap().enabled);
        assert!(set_enabled("petal.nope", true).is_err());

        uninstall("petal.x").unwrap();
        assert!(get("petal.x").unwrap().is_none());
        assert!(!dir.join("petal.x").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_bundle_refuses_a_tampered_or_unhashed_bundle() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir();
        initialize_for_tests(&dir);
        install("petal.t", record_for("1.0.0", b"{\"ok\":1}"), b"{\"ok\":1}").unwrap();
        assert!(read_bundle("petal.t").is_ok());
        // Someone rewrites the file on disk: the recorded sha256 no longer matches.
        std::fs::write(dir.join("petal.t/1.0.0/bundle.json"), b"{\"evil\":1}").unwrap();
        let err = read_bundle("petal.t").unwrap_err();
        assert!(err.contains("does not match the sha256"), "{err}");
        // A registry record without a hash is never trusted.
        let mut unhashed = record_for("1.0.0", b"x");
        unhashed.sha256 = None;
        install("petal.u", unhashed, b"x").unwrap();
        assert!(read_bundle("petal.u").unwrap_err().contains("no sha256"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registry_seen_round_trips() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir();
        initialize_for_tests(&dir);
        assert_eq!(registry_seen().unwrap(), None);
        let seen = RegistrySeen {
            url: "https://r.test".into(),
            generated_at_ms: 5,
            signed_at_s: 6,
        };
        record_registry_seen(seen.clone()).unwrap();
        initialize_for_tests(&dir);
        assert_eq!(registry_seen().unwrap(), Some(seen));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_state_file_starts_empty() {
        let _guard = test_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STATE_FILE), b"not json").unwrap();
        initialize_for_tests(&dir);
        assert!(list().unwrap().plugins.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
