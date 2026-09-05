//! Persisted screen-share priority and hover-tab placement preferences.
//!
//! Both preferences are app-wide and live in the same native JSON file so the
//! hover tab, system picker, and non-hover share entry points observe one
//! durable configuration. Previewed hover-tab positions only update memory;
//! callers explicitly commit on pointer-up or a native-menu preset.

use crate::hover_core::{normalize_hover_tab_vertical_offset, DEFAULT_HOVER_TAB_VERTICAL_OFFSET};
use crate::sync_ext::MutexExt;
use crate::transport::publisher::CaptureResolution;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const PREFERENCES_FILE: &str = "share-preferences.json";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SharePriority {
    #[default]
    Automatic,
    Responsive,
    SharpText,
    DataSaver,
}

impl SharePriority {
    /// Minimum capture cadence allowed during share startup. Startup must not
    /// begin with a tier-specific low-FPS cap while the receiver waits for its
    /// first visible frame (#299).
    pub const fn startup_cadence_floor(self) -> u32 {
        match self {
            Self::Automatic | Self::Responsive | Self::SharpText => 30,
            Self::DataSaver => 15,
        }
    }

    pub const fn capture_fps(self) -> u32 {
        match self {
            Self::DataSaver => 15,
            Self::Automatic | Self::Responsive | Self::SharpText => 30,
        }
    }

    pub const fn capture_resolution(self) -> CaptureResolution {
        match self {
            Self::Responsive | Self::DataSaver => CaptureResolution::P1080,
            Self::Automatic | Self::SharpText => CaptureResolution::Auto,
        }
    }

    pub const fn meets_interactive_latency_slo(self) -> bool {
        !matches!(self, Self::DataSaver)
    }
}

fn default_hover_tab_vertical_offset() -> f64 {
    DEFAULT_HOVER_TAB_VERTICAL_OFFSET
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharePreferencesFile {
    priority: SharePriority,
    #[serde(default = "default_hover_tab_vertical_offset")]
    hover_tab_vertical_offset: f64,
}

struct SharePriorityStore {
    path: PathBuf,
    priority: SharePriority,
    /// Current in-memory value, including an uncommitted drag preview.
    hover_tab_vertical_offset: f64,
    /// Last value durably written to disk. Other preference mutations must
    /// use this field so a preview cannot be persisted accidentally.
    committed_hover_tab_vertical_offset: f64,
}

impl SharePriorityStore {
    fn load(app_data_dir: &Path) -> Self {
        let path = app_data_dir.join(PREFERENCES_FILE);
        let (priority, hover_tab_vertical_offset) = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str::<SharePreferencesFile>(&contents)
                .map(|file| {
                    (
                        file.priority,
                        normalize_hover_tab_vertical_offset(file.hover_tab_vertical_offset),
                    )
                })
                .unwrap_or_else(|error| {
                    log::warn!(
                        "share-priority: could not parse {} ({error}); using defaults",
                        path.display()
                    );
                    (SharePriority::Automatic, DEFAULT_HOVER_TAB_VERTICAL_OFFSET)
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (SharePriority::Automatic, DEFAULT_HOVER_TAB_VERTICAL_OFFSET)
            }
            Err(error) => {
                log::warn!(
                    "share-priority: could not read {} ({error}); using defaults",
                    path.display()
                );
                (SharePriority::Automatic, DEFAULT_HOVER_TAB_VERTICAL_OFFSET)
            }
        };
        log::info!(
            "share-priority: loaded {priority:?}, hover-tab offset={hover_tab_vertical_offset:.3} from {}",
            path.display()
        );
        Self {
            path,
            priority,
            hover_tab_vertical_offset,
            committed_hover_tab_vertical_offset: hover_tab_vertical_offset,
        }
    }

    fn persist(&mut self, priority: SharePriority) -> Result<(), String> {
        persist_preferences_to_path(
            &self.path,
            priority,
            self.committed_hover_tab_vertical_offset,
        )?;
        self.priority = priority;
        Ok(())
    }

    fn preview_hover_tab_vertical_offset(&mut self, offset: f64) -> f64 {
        let offset = normalize_hover_tab_vertical_offset(offset);
        self.hover_tab_vertical_offset = offset;
        offset
    }

    fn persist_hover_tab_vertical_offset(&mut self, offset: f64) -> Result<f64, String> {
        let offset = normalize_hover_tab_vertical_offset(offset);
        persist_preferences_to_path(&self.path, self.priority, offset)?;
        self.hover_tab_vertical_offset = offset;
        self.committed_hover_tab_vertical_offset = offset;
        Ok(offset)
    }
}

static STORE: OnceLock<Mutex<SharePriorityStore>> = OnceLock::new();

pub fn initialize(app_data_dir: PathBuf) {
    if STORE
        .set(Mutex::new(SharePriorityStore::load(&app_data_dir)))
        .is_err()
    {
        log::debug!("share-priority: persistence already initialized");
    }
}

pub fn current() -> SharePriority {
    STORE
        .get()
        .map(|store| store.lock_unpoisoned().priority)
        .unwrap_or_default()
}

pub(crate) fn current_hover_tab_vertical_offset() -> f64 {
    STORE
        .get()
        .map(|store| store.lock_unpoisoned().hover_tab_vertical_offset)
        .unwrap_or(DEFAULT_HOVER_TAB_VERTICAL_OFFSET)
}

fn set_current(priority: SharePriority) -> Result<(), String> {
    let Some(store) = STORE.get() else {
        return Err("screen-share preference storage is not initialized".to_string());
    };
    store.lock_unpoisoned().persist(priority)
}

/// Update the in-memory preview without touching disk. The drag bridge calls
/// this for pointer moves; only `commit_hover_tab_vertical_offset` persists.
pub(crate) fn preview_hover_tab_vertical_offset(offset: f64) -> Result<f64, String> {
    let Some(store) = STORE.get() else {
        return Err("screen-share preference storage is not initialized".to_string());
    };
    Ok(store
        .lock_unpoisoned()
        .preview_hover_tab_vertical_offset(offset))
}

/// Persist one normalized hover-tab position while preserving the selected
/// screen-share priority.
pub(crate) fn commit_hover_tab_vertical_offset(offset: f64) -> Result<f64, String> {
    let Some(store) = STORE.get() else {
        return Err("screen-share preference storage is not initialized".to_string());
    };
    store
        .lock_unpoisoned()
        .persist_hover_tab_vertical_offset(offset)
}

fn persist_preferences_to_path(
    path: &Path,
    priority: SharePriority,
    hover_tab_vertical_offset: f64,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&SharePreferencesFile {
        priority,
        hover_tab_vertical_offset: normalize_hover_tab_vertical_offset(hover_tab_vertical_offset),
    })
    .map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)
        .map_err(|error| format!("writing {}: {error}", temporary.display()))?;
    // Rename is the atomic commit point on the supported filesystems. A
    // reader sees either the previous complete JSON or the new complete JSON,
    // never the partially written temporary file.
    std::fs::rename(&temporary, path).map_err(|error| {
        format!(
            "renaming {} to {}: {error}",
            temporary.display(),
            path.display()
        )
    })
}

#[tauri::command]
pub fn get_share_priority() -> SharePriority {
    current()
}

#[tauri::command]
pub async fn set_share_priority(
    app: tauri::AppHandle,
    priority: SharePriority,
    window_id: Option<u32>,
) -> Result<SharePriority, String> {
    set_current(priority)?;
    log::info!(
        "share-priority: saved {priority:?} as the default for future shares (interactive_slo={})",
        priority.meets_interactive_latency_slo()
    );

    #[cfg(target_os = "macos")]
    if let Some(window_id) = window_id {
        use tauri::Manager;
        if let Some(state) = app.try_state::<crate::session::SessionState>() {
            if state.is_share_active(window_id) {
                if let Err(error) =
                    crate::session::set_share_priority(state.inner(), window_id, priority).await
                {
                    // The durable selection succeeded and must remain the
                    // default. A live republish failure is recoverable and is
                    // retried naturally by the next share/reconcile cycle.
                    log::warn!(
                        "share-priority: saved {priority:?}, but could not apply it live to window {window_id}: {error}"
                    );
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = (app, window_id);

    Ok(priority)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "petal-share-priority-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn defaults_to_automatic_and_center_when_file_is_missing() {
        let dir = scratch_dir("missing");
        let store = SharePriorityStore::load(&dir);
        assert_eq!(store.priority, SharePriority::Automatic);
        assert_eq!(store.hover_tab_vertical_offset, 0.5);
    }

    #[test]
    fn legacy_file_without_position_loads_center() {
        let dir = scratch_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PREFERENCES_FILE), r#"{"priority":"sharpText"}"#).unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(store.priority, SharePriority::SharpText);
        assert_eq!(store.hover_tab_vertical_offset, 0.5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persisted_priority_and_position_survive_reload() {
        let dir = scratch_dir("reload");
        let mut store = SharePriorityStore::load(&dir);
        store.persist(SharePriority::SharpText).unwrap();
        store.persist_hover_tab_vertical_offset(0.75).unwrap();

        let reloaded = SharePriorityStore::load(&dir);
        assert_eq!(reloaded.priority, SharePriority::SharpText);
        assert_eq!(reloaded.hover_tab_vertical_offset, 0.75);
        assert!(dir.join(PREFERENCES_FILE).is_file());
        assert!(!dir.join("share-preferences.json.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preview_changes_memory_but_commit_is_the_disk_boundary() {
        let dir = scratch_dir("preview");
        let mut store = SharePriorityStore::load(&dir);
        store.persist_hover_tab_vertical_offset(0.5).unwrap();
        let before = std::fs::read_to_string(dir.join(PREFERENCES_FILE)).unwrap();

        assert_eq!(store.preview_hover_tab_vertical_offset(0.2), 0.2);
        assert_eq!(store.hover_tab_vertical_offset, 0.2);
        assert_eq!(
            std::fs::read_to_string(dir.join(PREFERENCES_FILE)).unwrap(),
            before
        );

        // A concurrent quality change must not turn an in-memory drag preview
        // into a durable position change.
        store.persist(SharePriority::SharpText).unwrap();
        let after_priority = SharePriorityStore::load(&dir);
        assert_eq!(after_priority.priority, SharePriority::SharpText);
        assert_eq!(after_priority.hover_tab_vertical_offset, 0.5);
        assert_eq!(store.hover_tab_vertical_offset, 0.2);

        store.persist_hover_tab_vertical_offset(0.2).unwrap();
        let reloaded = SharePriorityStore::load(&dir);
        assert_eq!(reloaded.hover_tab_vertical_offset, 0.2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_and_non_finite_positions_are_safe() {
        assert_eq!(normalize_hover_tab_vertical_offset(-2.0), 0.0);
        assert_eq!(normalize_hover_tab_vertical_offset(2.0), 1.0);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::NAN), 0.5);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::NEG_INFINITY), 0.5);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::INFINITY), 0.5);

        let dir = scratch_dir("clamp");
        let mut store = SharePriorityStore::load(&dir);
        assert_eq!(store.preview_hover_tab_vertical_offset(-1.0), 0.0);
        store.persist_hover_tab_vertical_offset(4.0).unwrap();
        assert_eq!(
            SharePriorityStore::load(&dir).hover_tab_vertical_offset,
            1.0
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_file_falls_back_to_safe_defaults() {
        let dir = scratch_dir("malformed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PREFERENCES_FILE), "not json").unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(store.priority, SharePriority::Automatic);
        assert_eq!(store.hover_tab_vertical_offset, 0.5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_data_saver_relaxes_interactive_latency_promise() {
        assert!(SharePriority::Automatic.meets_interactive_latency_slo());
        assert!(SharePriority::Responsive.meets_interactive_latency_slo());
        assert!(SharePriority::SharpText.meets_interactive_latency_slo());
        assert!(!SharePriority::DataSaver.meets_interactive_latency_slo());
        assert_eq!(SharePriority::DataSaver.capture_fps(), 15);
    }

    #[test]
    fn startup_cadence_floor_is_explicit_for_each_priority() {
        assert_eq!(SharePriority::Automatic.startup_cadence_floor(), 30);
        assert_eq!(SharePriority::Responsive.startup_cadence_floor(), 30);
        assert_eq!(SharePriority::SharpText.startup_cadence_floor(), 30);
        assert_eq!(SharePriority::DataSaver.startup_cadence_floor(), 15);
        for priority in [
            SharePriority::Automatic,
            SharePriority::Responsive,
            SharePriority::SharpText,
            SharePriority::DataSaver,
        ] {
            assert!(priority.capture_fps() >= priority.startup_cadence_floor());
        }
    }
}
