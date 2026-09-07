//! Persisted screen-share priority and hover-tab placement preferences.
//!
//! Both preferences are app-wide and live in the same native JSON file so the
//! hover tab, system picker, and non-hover share entry points observe one
//! durable configuration. Previewed hover-tab positions only update memory;
//! callers explicitly commit on pointer-up or a native-menu preset.

use crate::hover_core::{
    hover_tab_position_for_side_offset, hover_tab_side_offset, normalize_hover_tab_position,
    normalize_hover_tab_vertical_offset, HoverTabPosition, HoverTabSide,
    DEFAULT_HOVER_TAB_POSITION, DEFAULT_HOVER_TAB_SIDE, DEFAULT_HOVER_TAB_VERTICAL_OFFSET,
};
use crate::sync_ext::MutexExt;
use crate::transport::publisher::CaptureResolution;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const PREFERENCES_FILE: &str = "share-preferences.json";
const HOVER_TAB_POSITION_VERSION: u8 = 2;

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

fn default_hover_tab_side() -> HoverTabSide {
    DEFAULT_HOVER_TAB_SIDE
}

fn deserialize_hover_tab_side<'de, D>(deserializer: D) -> Result<HoverTabSide, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str() {
        Some("top") => HoverTabSide::Top,
        Some("right") => HoverTabSide::Right,
        Some("bottom") => HoverTabSide::Bottom,
        Some("left") => HoverTabSide::Left,
        _ => DEFAULT_HOVER_TAB_SIDE,
    })
}

fn json_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    value.and_then(serde_json::Value::as_f64)
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharePreferencesFile {
    priority: SharePriority,
    #[serde(
        default = "default_hover_tab_side",
        deserialize_with = "deserialize_hover_tab_side"
    )]
    /// Kept as a migration source for files written by the interim four-side
    /// implementation. New writes include `hover_tab_perimeter_position`.
    hover_tab_side: HoverTabSide,
    #[serde(default)]
    hover_tab_vertical_offset: Option<serde_json::Value>,
    #[serde(default)]
    hover_tab_perimeter_position: Option<serde_json::Value>,
    #[serde(default)]
    hover_tab_perimeter_version: Option<u8>,
}

struct SharePriorityStore {
    path: PathBuf,
    priority: SharePriority,
    /// Current in-memory value, including an uncommitted drag preview.
    hover_tab_position: HoverTabPosition,
    /// Last value durably written to disk. Other preference mutations use this
    /// field so a preview cannot be persisted accidentally.
    committed_hover_tab_position: HoverTabPosition,
}

impl SharePriorityStore {
    fn load(app_data_dir: &Path) -> Self {
        let path = app_data_dir.join(PREFERENCES_FILE);
        let (priority, hover_tab_position) = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str::<SharePreferencesFile>(&contents)
                .map(|file| {
                    let legacy_offset = json_f64(file.hover_tab_vertical_offset.as_ref())
                        .map(normalize_hover_tab_vertical_offset)
                        .unwrap_or(DEFAULT_HOVER_TAB_VERTICAL_OFFSET);
                    // Only a perimeter position stamped with the current
                    // version is trusted. No shipped build ever wrote the
                    // interim eight-segment scalar (main persisted only
                    // side + vertical offset), so an unversioned value can
                    // only come from a dev build of unknown vintage -- fall
                    // back to the side/offset pair every shipped build wrote
                    // instead of guessing a model and relocating the tab.
                    let position = json_f64(file.hover_tab_perimeter_position.as_ref())
                        .filter(|_| file.hover_tab_perimeter_version == Some(HOVER_TAB_POSITION_VERSION))
                        .map(normalize_hover_tab_position)
                        .unwrap_or_else(|| {
                            hover_tab_position_for_side_offset(file.hover_tab_side, legacy_offset)
                        });
                    (file.priority, position)
                })
                .unwrap_or_else(|error| {
                    log::warn!(
                        "share-priority: could not parse {} ({error}); using defaults",
                        path.display()
                    );
                    (SharePriority::Automatic, DEFAULT_HOVER_TAB_POSITION)
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (SharePriority::Automatic, DEFAULT_HOVER_TAB_POSITION)
            }
            Err(error) => {
                log::warn!(
                    "share-priority: could not read {} ({error}); using defaults",
                    path.display()
                );
                (SharePriority::Automatic, DEFAULT_HOVER_TAB_POSITION)
            }
        };
        let (side, offset) = hover_tab_side_offset(hover_tab_position);
        log::info!(
            "share-priority: loaded {priority:?}, hover-tab position={:.4} side={side:?} offset={offset:.3} from {}",
            hover_tab_position.value(),
            path.display()
        );
        Self {
            path,
            priority,
            hover_tab_position,
            committed_hover_tab_position: hover_tab_position,
        }
    }

    fn persist(&mut self, priority: SharePriority) -> Result<(), String> {
        persist_preferences_to_path(&self.path, priority, self.committed_hover_tab_position)?;
        self.priority = priority;
        Ok(())
    }

    fn preview_hover_tab_position(&mut self, position: HoverTabPosition) -> HoverTabPosition {
        let position = position.normalized();
        self.hover_tab_position = position;
        position
    }

    fn persist_hover_tab_position(
        &mut self,
        position: HoverTabPosition,
    ) -> Result<HoverTabPosition, String> {
        let position = position.normalized();
        if let Err(error) = persist_preferences_to_path(&self.path, self.priority, position) {
            // A failed commit must not leave a preview-only position looking
            // current to native followers. The caller can then restore the
            // native frame from the same durable value.
            self.hover_tab_position = self.committed_hover_tab_position;
            return Err(error);
        }
        self.hover_tab_position = position;
        self.committed_hover_tab_position = position;
        Ok(position)
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

pub(crate) fn current_hover_tab_position() -> HoverTabPosition {
    STORE
        .get()
        .map(|store| store.lock_unpoisoned().hover_tab_position)
        .unwrap_or(DEFAULT_HOVER_TAB_POSITION)
}

fn set_current(priority: SharePriority) -> Result<(), String> {
    let Some(store) = STORE.get() else {
        return Err("screen-share preference storage is not initialized".to_string());
    };
    store.lock_unpoisoned().persist(priority)
}

/// Update the in-memory preview without touching disk. The drag bridge calls
/// this for pointer moves; only `commit_hover_tab_position` persists.
pub(crate) fn preview_hover_tab_position(
    position: HoverTabPosition,
) -> Result<HoverTabPosition, String> {
    let Some(store) = STORE.get() else {
        return Err("screen-share preference storage is not initialized".to_string());
    };
    Ok(store.lock_unpoisoned().preview_hover_tab_position(position))
}

/// Compatibility preview for callers that still express a right-edge offset.
/// Persist one complete normalized perimeter position while preserving the
/// selected screen-share priority.
pub(crate) fn commit_hover_tab_position(
    position: HoverTabPosition,
) -> Result<HoverTabPosition, String> {
    let Some(store) = STORE.get() else {
        return Err("screen-share preference storage is not initialized".to_string());
    };
    store.lock_unpoisoned().persist_hover_tab_position(position)
}

/// Compatibility commit for callers that still express a right-edge offset.
fn atomically_replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

        let temporary: Vec<u16> = temporary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            MoveFileExW(
                PCWSTR(temporary.as_ptr()),
                PCWSTR(path_wide.as_ptr()),
                MOVEFILE_REPLACE_EXISTING,
            )
        }
        .map_err(|error| format!("replacing {}: {error}", path.display()))
    }

    #[cfg(not(target_os = "windows"))]
    {
        std::fs::rename(temporary, path).map_err(|error| {
            format!(
                "renaming {} to {}: {error}",
                temporary.display(),
                path.display()
            )
        })
    }
}

fn persist_preferences_to_path(
    path: &Path,
    priority: SharePriority,
    position: HoverTabPosition,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let position = position.normalized();
    let (hover_tab_side, hover_tab_vertical_offset) = hover_tab_side_offset(position);
    let json = serde_json::to_string_pretty(&SharePreferencesFile {
        priority,
        hover_tab_side,
        hover_tab_vertical_offset: Some(serde_json::json!(hover_tab_vertical_offset)),
        hover_tab_perimeter_position: Some(serde_json::json!(position.value())),
        hover_tab_perimeter_version: Some(HOVER_TAB_POSITION_VERSION),
    })
    .map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)
        .map_err(|error| format!("writing {}: {error}", temporary.display()))?;
    // Replacing the destination is the atomic commit point on the supported
    // filesystems. A reader sees either the previous complete JSON or the new
    // complete JSON, never the partially written temporary file. Windows
    // needs MoveFileExW because std::fs::rename does not replace an existing
    // destination there.
    atomically_replace_file(&temporary, path)
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
        assert_eq!(store.hover_tab_position, DEFAULT_HOVER_TAB_POSITION);
        assert_eq!(
            store.committed_hover_tab_position,
            DEFAULT_HOVER_TAB_POSITION
        );
    }

    #[test]
    fn legacy_file_without_position_loads_center() {
        let dir = scratch_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PREFERENCES_FILE), r#"{"priority":"sharpText"}"#).unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(store.priority, SharePriority::SharpText);
        assert_eq!(store.hover_tab_position, DEFAULT_HOVER_TAB_POSITION);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persisted_priority_and_position_survive_reload() {
        let dir = scratch_dir("reload");
        let mut store = SharePriorityStore::load(&dir);
        store.persist(SharePriority::SharpText).unwrap();
        let position = hover_tab_position_for_side_offset(HoverTabSide::Left, 0.75);
        store.persist_hover_tab_position(position).unwrap();

        let reloaded = SharePriorityStore::load(&dir);
        assert_eq!(reloaded.priority, SharePriority::SharpText);
        assert_eq!(reloaded.hover_tab_position, position);
        assert!(dir.join(PREFERENCES_FILE).is_file());
        assert!(!dir.join("share-preferences.json.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preview_changes_memory_but_commit_is_the_disk_boundary() {
        let dir = scratch_dir("preview");
        let mut store = SharePriorityStore::load(&dir);
        let committed = hover_tab_position_for_side_offset(HoverTabSide::Right, 0.5);
        store.persist_hover_tab_position(committed).unwrap();
        let before = std::fs::read_to_string(dir.join(PREFERENCES_FILE)).unwrap();
        let preview = hover_tab_position_for_side_offset(HoverTabSide::Bottom, 0.2);

        assert_eq!(store.preview_hover_tab_position(preview), preview);
        assert_eq!(store.hover_tab_position, preview);
        assert_eq!(store.committed_hover_tab_position, committed);
        assert_eq!(
            std::fs::read_to_string(dir.join(PREFERENCES_FILE)).unwrap(),
            before
        );

        // A concurrent quality change must not turn an in-memory drag preview
        // into a durable position change.
        store.persist(SharePriority::SharpText).unwrap();
        let after_priority = SharePriorityStore::load(&dir);
        assert_eq!(after_priority.priority, SharePriority::SharpText);
        assert_eq!(after_priority.hover_tab_position, committed);
        assert_eq!(store.hover_tab_position, preview);

        store.persist_hover_tab_position(preview).unwrap();
        let reloaded = SharePriorityStore::load(&dir);
        assert_eq!(reloaded.hover_tab_position, preview);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn new_perimeter_value_takes_precedence_over_legacy_side_fields() {
        let dir = scratch_dir("new-position");
        std::fs::create_dir_all(&dir).unwrap();
        let position = 0.73;
        std::fs::write(
            dir.join(PREFERENCES_FILE),
            format!(
                r#"{{"priority":"automatic","hoverTabSide":"left","hoverTabVerticalOffset":0.1,"hoverTabPerimeterPosition":{position},"hoverTabPerimeterVersion":2}}"#
            ),
        )
        .unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(store.hover_tab_position, HoverTabPosition(position));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_position_commit_restores_the_last_committed_memory_value() {
        let blocker = scratch_dir("persist-failure");
        std::fs::write(&blocker, "not a directory").unwrap();
        let committed = DEFAULT_HOVER_TAB_POSITION;
        let preview = hover_tab_position_for_side_offset(HoverTabSide::Bottom, 0.2);
        let mut store = SharePriorityStore {
            path: blocker.join(PREFERENCES_FILE),
            priority: SharePriority::Automatic,
            hover_tab_position: preview,
            committed_hover_tab_position: committed,
        };

        assert!(store
            .persist_hover_tab_position(HoverTabPosition(0.9))
            .is_err());
        assert_eq!(store.hover_tab_position, committed);
        assert_eq!(store.committed_hover_tab_position, committed);
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn an_unversioned_perimeter_position_falls_back_to_the_shipped_side_and_offset() {
        // No shipped build ever wrote `hoverTabPerimeterPosition` without
        // `hoverTabPerimeterVersion: 2`; main wrote side + vertical offset
        // only. A bare scalar is therefore of unknown model and must not be
        // interpreted (the old path relocated 4-segment values into corners).
        let dir = scratch_dir("unversioned-position");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PREFERENCES_FILE),
            r#"{"priority":"automatic","hoverTabSide":"left","hoverTabVerticalOffset":0.25,"hoverTabPerimeterPosition":0.375}"#,
        )
        .unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(
            store.hover_tab_position,
            hover_tab_position_for_side_offset(HoverTabSide::Left, 0.25)
        );
        // The same scalar WITH the version stamp is trusted as-is.
        std::fs::write(
            dir.join(PREFERENCES_FILE),
            r#"{"priority":"automatic","hoverTabSide":"left","hoverTabVerticalOffset":0.25,"hoverTabPerimeterPosition":0.375,"hoverTabPerimeterVersion":2}"#,
        )
        .unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(store.hover_tab_position, HoverTabPosition(0.375));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_and_non_finite_positions_are_safe() {
        assert_eq!(normalize_hover_tab_vertical_offset(-2.0), 0.0);
        assert_eq!(normalize_hover_tab_vertical_offset(2.0), 1.0);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::NAN), 0.5);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::NEG_INFINITY), 0.5);
        assert_eq!(normalize_hover_tab_vertical_offset(f64::INFINITY), 0.5);
        assert_eq!(normalize_hover_tab_position(-0.25), HoverTabPosition(0.75));
        assert_eq!(
            normalize_hover_tab_position(f64::NAN),
            DEFAULT_HOVER_TAB_POSITION
        );

        let dir = scratch_dir("clamp");
        let mut store = SharePriorityStore::load(&dir);
        assert_eq!(
            store.preview_hover_tab_position(HoverTabPosition(-1.0)),
            HoverTabPosition(0.0)
        );
        store
            .persist_hover_tab_position(HoverTabPosition(4.0))
            .unwrap();
        assert_eq!(
            SharePriorityStore::load(&dir).hover_tab_position,
            HoverTabPosition(0.0)
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
        assert_eq!(store.hover_tab_position, DEFAULT_HOVER_TAB_POSITION);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_saved_side_defaults_right_without_discarding_the_offset() {
        let dir = scratch_dir("invalid-side");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PREFERENCES_FILE),
            r#"{"priority":"automatic","hoverTabSide":"diagonal","hoverTabVerticalOffset":0.25}"#,
        )
        .unwrap();
        let store = SharePriorityStore::load(&dir);
        assert_eq!(
            store.hover_tab_position,
            hover_tab_position_for_side_offset(HoverTabSide::Right, 0.25)
        );
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
