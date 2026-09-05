use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::fmt;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicU64;
#[cfg(target_os = "macos")]
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use serde::Serialize;
use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::UpdaterExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CpuArch {
    Arm64,
    X86_64,
}

impl CpuArch {
    fn running() -> Option<Self> {
        match std::env::consts::ARCH {
            "aarch64" => Some(Self::Arm64),
            "x86_64" => Some(Self::X86_64),
            _ => None,
        }
    }

    fn from_mach_cpu_type(cpu_type: u32) -> Option<Self> {
        match cpu_type {
            0x0100_000c => Some(Self::Arm64),
            0x0100_0007 => Some(Self::X86_64),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X86_64 => "x86_64",
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardedUpdateResult {
    pub status: GuardedUpdateStatus,
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuardedUpdateStatus {
    UpToDate,
    Available,
    Installed,
}

/// Once-per-process latch for the passive launch check. Every webview that
/// mounts the root layout used to fire the launch check again -- the window
/// picker, the network cockpit, and any hard navigation of the main webview
/// (a deep-link meeting join does `window.location.assign`, remounting the
/// layout) each hit the update endpoint a second time. The latch guarantees
/// exactly one launch check per process; later calls return `None` with no
/// network I/O. A fresh process starts with a fresh `AtomicBool`.
static LAUNCH_CHECK_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim the once-per-process launch check. `true` for the first caller in
/// this process, `false` for every later caller.
fn claim_launch_check() -> bool {
    !LAUNCH_CHECK_CLAIMED.swap(true, Ordering::SeqCst)
}

/// The committed `tauri.conf.json` ships NO updater endpoint: a build from a
/// plain clone must never poll the maintainers' update feed (open-source rule:
/// no phoning home by default). The official pipeline layers
/// `tauri.release.conf.json` on top (`tauri build --config`), which is the
/// only place the production endpoint + minisign pubkey live. Returns `true`
/// when at least one non-empty endpoint is configured.
fn updater_endpoints_configured(config: &tauri::Config) -> bool {
    updater_endpoints_configured_in(config.plugins.0.get("updater"))
}

fn updater_endpoints_configured_in(updater: Option<&serde_json::Value>) -> bool {
    updater
        .and_then(|section| section.get("endpoints"))
        .and_then(serde_json::Value::as_array)
        .map(|endpoints| {
            endpoints
                .iter()
                .any(|endpoint| endpoint.as_str().is_some_and(|url| !url.trim().is_empty()))
        })
        .unwrap_or(false)
}

static UPDATER_DISABLED_LOGGED: AtomicBool = AtomicBool::new(false);

/// `Some(result)` when this build has no update endpoint: report up-to-date
/// with zero network I/O and log the reason exactly once per process.
fn disabled_updater_result<R: Runtime>(app: &AppHandle<R>) -> Option<GuardedUpdateResult> {
    if updater_endpoints_configured(&app.config()) {
        return None;
    }
    if !UPDATER_DISABLED_LOGGED.swap(true, Ordering::SeqCst) {
        log::info!(
            "updater: no update endpoint configured in this build (plugins.updater.endpoints is \
             empty) -- auto-update disabled; official releases set it via tauri.release.conf.json"
        );
    }
    Some(GuardedUpdateResult {
        status: GuardedUpdateStatus::UpToDate,
        version: None,
    })
}

/// The passive launch check. The frontend calls this from the main window's
/// root layout on mount. Runs the real check exactly once per process;
/// returns `None` for any later call (a secondary window mounting the same
/// layout, or a hard navigation of the main webview), so the update endpoint
/// is never hit more than once per launch. Main-menu and manual checks keep
/// calling `check_compatible_update_available` directly.
#[tauri::command]
pub async fn run_launch_update_check<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<GuardedUpdateResult>, String> {
    if !claim_launch_check() {
        return Ok(None);
    }
    check_compatible_update_available(app).await.map(Some)
}

#[tauri::command]
pub async fn check_compatible_update_available<R: Runtime>(
    app: AppHandle<R>,
) -> Result<GuardedUpdateResult, String> {
    if let Some(disabled) = disabled_updater_result(&app) {
        return Ok(disabled);
    }
    let update = match app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?
        .check()
        .await
    {
        Ok(update) => update,
        // A manifest with no entry for this platform (e.g. only `darwin-aarch64`
        // published while this process runs on Windows) means "no update for
        // you" — report up-to-date silently, mirroring the backend's 204
        // contract, instead of surfacing a fake update failure every launch.
        Err(tauri_plugin_updater::Error::TargetsNotFound(_)) => {
            log::info!("updater: no manifest entry for this platform; treating as up-to-date");
            return Ok(GuardedUpdateResult {
                status: GuardedUpdateStatus::UpToDate,
                version: None,
            });
        }
        Err(e) => return Err(format!("update check failed: {e}")),
    };

    let Some(update) = update else {
        return Ok(GuardedUpdateResult {
            status: GuardedUpdateStatus::UpToDate,
            version: None,
        });
    };

    let version = update.version.clone();
    log::info!(
        "updater: available {version} (current {}) -- waiting for explicit install",
        update.current_version
    );
    Ok(GuardedUpdateResult {
        status: GuardedUpdateStatus::Available,
        version: Some(version),
    })
}

#[tauri::command]
pub async fn download_and_install_compatible_update<R: Runtime>(
    app: AppHandle<R>,
) -> Result<GuardedUpdateResult, String> {
    if let Some(disabled) = disabled_updater_result(&app) {
        return Ok(disabled);
    }
    let update = match app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?
        .check()
        .await
    {
        Ok(update) => update,
        // A manifest with no entry for this platform (e.g. only `darwin-aarch64`
        // published while this process runs on Windows) means "no update for
        // you" — report up-to-date silently, mirroring the backend's 204
        // contract, instead of surfacing a fake update failure every launch.
        Err(tauri_plugin_updater::Error::TargetsNotFound(_)) => {
            log::info!("updater: no manifest entry for this platform; treating as up-to-date");
            return Ok(GuardedUpdateResult {
                status: GuardedUpdateStatus::UpToDate,
                version: None,
            });
        }
        Err(e) => return Err(format!("update check failed: {e}")),
    };

    let Some(update) = update else {
        return Ok(GuardedUpdateResult {
            status: GuardedUpdateStatus::UpToDate,
            version: None,
        });
    };

    let version = update.version.clone();
    log::info!(
        "updater: available {version} (current {})",
        update.current_version
    );

    let mut first_chunk = true;
    let bytes = update
        .download(
            |chunk_length, content_length| {
                if first_chunk {
                    first_chunk = false;
                    log::info!(
                        "updater: downloading {} bytes",
                        content_length
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    );
                }
                log::debug!("updater: downloaded chunk {chunk_length} bytes");
            },
            || {
                log::info!("updater: download finished");
            },
        )
        .await
        .map_err(|e| format!("update download failed: {e}"))?;

    verify_update_archive_architecture(&bytes).map_err(|e| {
        log::error!("updater: architecture guard rejected update {version}: {e}");
        format!("update is incompatible with this Mac: {e}")
    })?;

    #[cfg(target_os = "macos")]
    {
        let executable = std::env::current_exe()
            .map_err(|e| format!("Petal could not locate the installed app: {e}"))?;
        let destination = tauri_plugin_updater::extract_path_from_executable(&executable)
            .map_err(|e| format!("Petal could not locate the installed app: {e}"))?;
        install_macos_app_bundle(&bytes, &destination)
            .map_err(|error| mac_install_user_message(&error, &destination))?;
        // #902: re-register the bundle we just swapped in, or the user can
        // lose their Dock icon permanently (it does not heal on relaunch).
        // Never fatal: the update itself is already installed and correct,
        // and `run()`'s startup repair is the second line of defense.
        if let Err(error) = crate::platform::launch_services::register_bundle(&destination) {
            log::error!(
                "updater: installed the update but could not re-register '{}' with LaunchServices: {error} -- the Dock icon may be missing until the app is relaunched (#902)",
                destination.display()
            );
        }
    }
    #[cfg(not(target_os = "macos"))]
    update
        .install(&bytes)
        .map_err(|e| format!("update install failed: {e}"))?;
    log::info!("updater: installed compatible update {version}");

    Ok(GuardedUpdateResult {
        status: GuardedUpdateStatus::Installed,
        version: Some(version),
    })
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacInstallStage {
    Resolve,
    Stage,
    Extract,
    Backup,
    Promote,
    Rollback,
    Privileged,
}

#[cfg(target_os = "macos")]
impl MacInstallStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Stage => "stage",
            Self::Extract => "extract",
            Self::Backup => "backup",
            Self::Promote => "promote",
            Self::Rollback => "rollback",
            Self::Privileged => "privileged",
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
enum MacInstallCause {
    Io(std::io::Error),
    ExitStatus(std::process::ExitStatus),
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MacInstallError {
    stage: MacInstallStage,
    cause: MacInstallCause,
    kind: std::io::ErrorKind,
    raw_os_error: Option<i32>,
    source: PathBuf,
    destination: PathBuf,
}

#[cfg(target_os = "macos")]
impl MacInstallError {
    fn io(
        stage: MacInstallStage,
        source: impl Into<PathBuf>,
        destination: impl Into<PathBuf>,
        error: std::io::Error,
    ) -> Self {
        Self {
            stage,
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            cause: MacInstallCause::Io(error),
            source: source.into(),
            destination: destination.into(),
        }
    }

    fn exit_status(
        stage: MacInstallStage,
        source: impl Into<PathBuf>,
        destination: impl Into<PathBuf>,
        status: std::process::ExitStatus,
    ) -> Self {
        Self {
            stage,
            cause: MacInstallCause::ExitStatus(status),
            kind: std::io::ErrorKind::PermissionDenied,
            raw_os_error: None,
            source: source.into(),
            destination: destination.into(),
        }
    }

    fn is_read_only(&self) -> bool {
        self.kind == std::io::ErrorKind::ReadOnlyFilesystem || self.raw_os_error == Some(30)
    }
}

#[cfg(target_os = "macos")]
impl fmt::Display for MacInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed from {} to {}: ",
            self.stage.as_str(),
            self.source.display(),
            self.destination.display()
        )?;
        match &self.cause {
            MacInstallCause::Io(error) => error.fmt(formatter),
            MacInstallCause::ExitStatus(status) => {
                write!(formatter, "osascript exited with {status}")
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl std::error::Error for MacInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            MacInstallCause::Io(error) => Some(error),
            MacInstallCause::ExitStatus(_) => None,
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagingLocation {
    DestinationParent,
    SystemTemp,
}

#[cfg(target_os = "macos")]
struct StagingGuard {
    path: PathBuf,
    preserve: bool,
}

#[cfg(target_os = "macos")]
impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            preserve: false,
        }
    }

    fn preserve_for_recovery(&mut self) {
        self.preserve = true;
    }
}

#[cfg(target_os = "macos")]
impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(target_os = "macos")]
static MAC_STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
fn create_unique_staging_directory(parent: &Path) -> std::io::Result<PathBuf> {
    for _ in 0..32 {
        let sequence = MAC_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = parent.join(format!(
            ".petal-update-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique update staging directory",
    ))
}

#[cfg(target_os = "macos")]
fn install_macos_app_bundle(bytes: &[u8], destination: &Path) -> Result<(), MacInstallError> {
    let result = install_macos_app_bundle_inner(bytes, destination);
    if let Err(error) = &result {
        report_macos_install_failure(error, destination);
    }
    result
}

#[cfg(target_os = "macos")]
fn install_macos_app_bundle_inner(bytes: &[u8], destination: &Path) -> Result<(), MacInstallError> {
    let parent = destination.parent().ok_or_else(|| {
        MacInstallError::io(
            MacInstallStage::Resolve,
            destination,
            destination,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "installed app path has no parent directory",
            ),
        )
    })?;

    let (staging_path, staging_location) = match create_unique_staging_directory(parent) {
        Ok(path) => (path, StagingLocation::DestinationParent),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let system_temp = std::env::temp_dir();
            let path = create_unique_staging_directory(&system_temp).map_err(|fallback_error| {
                MacInstallError::io(
                    MacInstallStage::Stage,
                    &system_temp,
                    system_temp.join(".petal-update-*"),
                    fallback_error,
                )
            })?;
            (path, StagingLocation::SystemTemp)
        }
        Err(error) => {
            return Err(MacInstallError::io(
                MacInstallStage::Stage,
                parent,
                parent.join(".petal-update-*"),
                error,
            ));
        }
    };
    let mut staging = StagingGuard::new(staging_path);
    log::debug!(
        "updater: staging macOS app at {} ({staging_location:?})",
        staging.path.display()
    );

    let new_bundle = staging.path.join("new");
    std::fs::create_dir(&new_bundle).map_err(|error| {
        MacInstallError::io(MacInstallStage::Extract, &staging.path, &new_bundle, error)
    })?;

    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|error| {
        MacInstallError::io(MacInstallStage::Extract, &staging.path, &new_bundle, error)
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            MacInstallError::io(MacInstallStage::Extract, &staging.path, &new_bundle, error)
        })?;
        let collected_path: PathBuf = entry
            .path()
            .map_err(|error| {
                MacInstallError::io(MacInstallStage::Extract, &staging.path, &new_bundle, error)
            })?
            .iter()
            .skip(1)
            .collect();
        let extraction_path = new_bundle.join(&collected_path);

        if let Some(entry_parent) = extraction_path.parent() {
            std::fs::create_dir_all(entry_parent).map_err(|error| {
                MacInstallError::io(MacInstallStage::Extract, &new_bundle, entry_parent, error)
            })?;
        }
        entry.unpack(&extraction_path).map_err(|error| {
            MacInstallError::io(
                MacInstallStage::Extract,
                &new_bundle,
                &extraction_path,
                error,
            )
        })?;
    }

    let old_bundle = staging.path.join("old");
    match std::fs::rename(destination, &old_bundle) {
        Ok(()) => {
            if let Err(promote_error) = std::fs::rename(&new_bundle, destination) {
                if let Err(rollback_error) = std::fs::rename(&old_bundle, destination) {
                    staging.preserve_for_recovery();
                    let combined = std::io::Error::new(
                        rollback_error.kind(),
                        format!(
                            "promote failed: {promote_error}; rollback failed: {rollback_error}; old app preserved at {}",
                            old_bundle.display()
                        ),
                    );
                    return Err(MacInstallError::io(
                        MacInstallStage::Rollback,
                        &old_bundle,
                        destination,
                        combined,
                    ));
                }
                return Err(MacInstallError::io(
                    MacInstallStage::Promote,
                    &new_bundle,
                    destination,
                    promote_error,
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            log::debug!("updater: app installation needs administrator privileges");
            // This is the updater plugin's pre-existing non-atomic fallback. It
            // only runs when a co-located staging swap is not permitted.
            // Two defects inherited from the plugin's version of this, both
            // fixed here because this is Petal's code now (#871 review):
            //
            // 1. It ran `rm -rf <destination> && mv <new> <destination>`. If
            //    the rm succeeded and the mv failed, the user's app was GONE
            //    -- with the staging copy deleted too. Move the old bundle
            //    aside first, promote, and only then discard it; a failed
            //    promote leaves the original recoverable at <dest>.petal-old.
            // 2. It interpolated paths into single quotes inside an AppleScript
            //    `do shell script`, so a path containing a quote (e.g.
            //    /Volumes/Tim's SSD/Petal.app) broke the quoting -- arbitrary
            //    ROOT execution from a user-writable location. Paths now go in
            //    as escaped AppleScript string literals and are shell-quoted by
            //    AppleScript's own `quoted form of`.
            let old_aside = format!("{}.petal-old", destination.display());
            let script = format!(
                "set d to {dest}\n\
                 set n to {new}\n\
                 set o to {aside}\n\
                 do shell script \"rm -rf \" & quoted form of o & \
                 \" && mv -f \" & quoted form of d & \" \" & quoted form of o & \
                 \" && mv -f \" & quoted form of n & \" \" & quoted form of d & \
                 \" && rm -rf \" & quoted form of o with administrator privileges",
                dest = applescript_string_literal(&destination.display().to_string()),
                new = applescript_string_literal(&new_bundle.display().to_string()),
                aside = applescript_string_literal(&old_aside),
            );
            let status = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .status()
                .map_err(|command_error| {
                    MacInstallError::io(
                        MacInstallStage::Privileged,
                        &new_bundle,
                        destination,
                        command_error,
                    )
                })?;
            if !status.success() {
                return Err(MacInstallError::exit_status(
                    MacInstallStage::Privileged,
                    &new_bundle,
                    destination,
                    status,
                ));
            }
        }
        Err(error) => {
            return Err(MacInstallError::io(
                MacInstallStage::Backup,
                destination,
                &old_bundle,
                error,
            ));
        }
    }

    let _ = std::process::Command::new("touch")
        .arg(destination)
        .status();
    Ok(())
}

/// Escape a path for embedding as an AppleScript string literal. AppleScript
/// string literals are double-quoted and escape with backslash, so `\` and `"`
/// are the only characters that can terminate or extend the literal. Shell
/// quoting is then AppleScript's own `quoted form of`, which is what makes a
/// path containing `'` safe -- interpolating one into single quotes inside a
/// `do shell script` was arbitrary ROOT execution (#871 review).
#[cfg(target_os = "macos")]
fn applescript_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(target_os = "macos")]
fn mac_install_user_message(error: &MacInstallError, destination: &Path) -> String {
    // The one branch where the user's app is no longer where they left it: the
    // backup rename succeeded, the promote failed, and the rollback failed too.
    // `install_macos_app_bundle_inner` deliberately keeps the staging directory
    // in that case, so the message must name it -- a generic "try again" would
    // strand the user with no app and no idea where it went (#871).
    if error.stage == MacInstallStage::Rollback {
        return format!(
            "The update could not be completed. Your previous Petal is safe at {} -- move it back to {}.",
            error.source.display(),
            destination.display()
        );
    }
    if error.is_read_only() {
        return "Petal is running from a read-only disk image. Drag Petal into Applications, then try again."
            .to_string();
    }
    if error.kind == std::io::ErrorKind::CrossesDevices {
        let parent = destination.parent().unwrap_or(destination);
        return format!(
            "Petal could not install the update because {} and the staging folder are on different disks.",
            parent.display()
        );
    }
    if error.stage == MacInstallStage::Privileged
        && error.kind == std::io::ErrorKind::PermissionDenied
    {
        return "This update needs an administrator password. Moving Petal to Applications avoids this."
            .to_string();
    }
    "Petal could not install the update. Move Petal to Applications, then try again.".to_string()
}

#[cfg(target_os = "macos")]
fn report_macos_install_failure(error: &MacInstallError, destination: &Path) {
    let source_dev = device_id_for_path(&error.source)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let destination_dev = device_id_for_path(&error.destination)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    log::error!(
        "updater: install failed stage={} kind={:?} source={} destination={} source_dev={} destination_dev={} error={}",
        error.stage.as_str(),
        error.kind,
        error.source.display(),
        error.destination.display(),
        source_dev,
        destination_dev,
        error
    );

    crate::logging::capture_sentry_diagnostic(
        crate::logging::SentryDiagnosticEvent::UpdateInstallFailed(
            crate::logging::UpdateInstallFailedDiagnostic {
                stage: install_failure_stage_tag(error.stage),
                kind: install_failure_kind_tag(error),
                boundary: install_volume_boundary(destination),
                destination: install_destination_class(destination),
            },
        ),
    );
}

#[cfg(target_os = "macos")]
fn device_id_for_path(path: &Path) -> Option<u64> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if let Ok(metadata) = std::fs::metadata(current) {
            return Some(metadata.dev());
        }
        candidate = current.parent();
    }
    None
}

#[cfg(target_os = "macos")]
fn install_failure_stage_tag(stage: MacInstallStage) -> crate::logging::InstallFailureStageTag {
    use crate::logging::InstallFailureStageTag;
    match stage {
        MacInstallStage::Resolve => InstallFailureStageTag::Resolve,
        MacInstallStage::Stage => InstallFailureStageTag::Stage,
        MacInstallStage::Extract => InstallFailureStageTag::Extract,
        MacInstallStage::Backup => InstallFailureStageTag::Backup,
        MacInstallStage::Promote => InstallFailureStageTag::Promote,
        MacInstallStage::Rollback => InstallFailureStageTag::Rollback,
        MacInstallStage::Privileged => InstallFailureStageTag::Privileged,
    }
}

#[cfg(target_os = "macos")]
fn install_failure_kind_tag(error: &MacInstallError) -> crate::logging::InstallFailureKindTag {
    use crate::logging::InstallFailureKindTag;
    if error.is_read_only() {
        return InstallFailureKindTag::ReadOnly;
    }
    match error.kind {
        std::io::ErrorKind::CrossesDevices => InstallFailureKindTag::CrossDevice,
        std::io::ErrorKind::PermissionDenied => InstallFailureKindTag::PermissionDenied,
        std::io::ErrorKind::NotFound => InstallFailureKindTag::NotFound,
        _ if error.raw_os_error == Some(28) => InstallFailureKindTag::NoSpace,
        _ => InstallFailureKindTag::Other,
    }
}

#[cfg(target_os = "macos")]
fn install_volume_boundary(destination: &Path) -> crate::logging::InstallVolumeBoundaryTag {
    use crate::logging::InstallVolumeBoundaryTag;
    let destination_dev = destination
        .parent()
        .and_then(|parent| std::fs::metadata(parent).ok())
        .map(|metadata| metadata.dev());
    let temp_dev = std::fs::metadata(std::env::temp_dir())
        .ok()
        .map(|metadata| metadata.dev());
    match (destination_dev, temp_dev) {
        (Some(destination), Some(temp)) if destination == temp => {
            InstallVolumeBoundaryTag::SameVolume
        }
        (Some(_), Some(_)) => InstallVolumeBoundaryTag::CrossVolume,
        _ => InstallVolumeBoundaryTag::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn install_destination_class(destination: &Path) -> crate::logging::InstallDestinationClassTag {
    use crate::logging::InstallDestinationClassTag;
    if destination.strip_prefix("/Applications").is_ok() {
        return InstallDestinationClassTag::Applications;
    }
    if destination.starts_with("/Users")
        && destination
            .components()
            .any(|component| component.as_os_str() == "Applications")
    {
        return InstallDestinationClassTag::UserApplications;
    }
    if destination.strip_prefix("/Volumes").is_ok() {
        return if filesystem_is_read_only(destination) {
            InstallDestinationClassTag::DiskImage
        } else {
            InstallDestinationClassTag::RemovableVolume
        };
    }
    InstallDestinationClassTag::Other
}

#[cfg(target_os = "macos")]
fn filesystem_is_read_only(path: &Path) -> bool {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if let Ok(path) = CString::new(current.as_os_str().as_bytes()) {
            let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } == 0 {
                let stats = unsafe { stats.assume_init() };
                return stats.f_flags & libc::MNT_RDONLY as u32 != 0;
            }
        }
        candidate = current.parent();
    }
    false
}

fn verify_update_archive_architecture(bytes: &[u8]) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // Windows updates are NSIS installers, not macOS .app bundles, so
        // verify the staged executable's PE machine type instead of the Mach-O
        // path below. tauri-plugin-updater already picks the manifest entry by
        // target (`windows-x86_64` etc.); this mirrors the macOS guard as
        // defense in depth against a wrong-arch installer.
        return verify_windows_installer_architecture(bytes);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let running = CpuArch::running().ok_or_else(|| {
            format!(
                "unsupported running architecture {}",
                std::env::consts::ARCH
            )
        })?;
        let found = app_bundle_macho_architectures(bytes)?;
        let found_labels = arch_labels(&found);
        if found.contains(&running) {
            log::info!(
                "updater: architecture guard accepted update; running={} bundle={}",
                running.as_str(),
                found_labels
            );
            Ok(())
        } else {
            Err(format!(
                "running architecture {} not present in staged app bundle ({})",
                running.as_str(),
                found_labels
            ))
        }
    }
}

fn app_bundle_macho_architectures(bytes: &[u8]) -> Result<BTreeSet<CpuArch>, String> {
    let cursor = Cursor::new(bytes);
    let decoder = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);
    let mut bundle_executable: Option<String> = None;
    let mut macos_files: Vec<(String, Vec<u8>)> = Vec::new();

    for entry in archive
        .entries()
        .map_err(|e| format!("could not read update archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("could not read update archive entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("could not read update archive entry path: {e}"))?
            .to_path_buf();
        let path_string = path.to_string_lossy().into_owned();

        if path_string.ends_with(".app/Contents/Info.plist") {
            let mut plist_bytes = Vec::new();
            std::io::copy(&mut entry, &mut plist_bytes)
                .map_err(|e| format!("could not read {path_string}: {e}"))?;
            bundle_executable = Some(parse_cf_bundle_executable(&plist_bytes)?);
            continue;
        }

        if !path_string.contains(".app/Contents/MacOS/") {
            continue;
        }

        let mut file_bytes = Vec::new();
        std::io::copy(&mut entry, &mut file_bytes)
            .map_err(|e| format!("could not read {path_string}: {e}"))?;
        macos_files.push((path_string, file_bytes));
    }

    let bundle_executable = bundle_executable
        .ok_or_else(|| "no CFBundleExecutable found in app Info.plist".to_string())?;
    let expected_suffix = format!(".app/Contents/MacOS/{bundle_executable}");
    for (path, file_bytes) in macos_files {
        if path.ends_with(&expected_suffix) {
            let arches = macho_architectures(&file_bytes)
                .map_err(|e| format!("could not inspect {path}: {e}"))?;
            if arches.is_empty() {
                return Err(format!(
                    "no supported Mach-O CPU architecture found in {path}"
                ));
            }
            return Ok(arches);
        }
    }

    Err(format!(
        "declared app executable {bundle_executable} was not found in update archive"
    ))
}

/// Windows: verify a staged update archive is a PE executable for the running
/// architecture. The archive is the NSIS installer; its machine type comes
/// from the PE header (DOS header `e_lfanew` -> "PE\0\0" -> 2-byte machine).
#[cfg(target_os = "windows")]
fn verify_windows_installer_architecture(bytes: &[u8]) -> Result<(), String> {
    let machine =
        pe_machine_type(bytes).ok_or_else(|| "update archive is not a PE executable".to_string())?;
    let expected = match std::env::consts::ARCH {
        "x86_64" => 0x8664,
        "x86" => 0x014c,
        "aarch64" => 0xaa64,
        other => return Err(format!("unsupported running architecture {other}")),
    };
    if machine == expected {
        log::info!(
            "updater: architecture guard accepted update; running={} pe_machine=0x{machine:04x}",
            std::env::consts::ARCH
        );
        Ok(())
    } else {
        Err(format!(
            "update archive machine 0x{machine:04x} does not match running architecture {}",
            std::env::consts::ARCH
        ))
    }
}

#[cfg(target_os = "windows")]
fn pe_machine_type(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().ok()?) as usize;
    if e_lfanew + 6 > bytes.len() || &bytes[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(
        bytes[e_lfanew + 4..e_lfanew + 6].try_into().ok()?,
    ))
}

fn parse_cf_bundle_executable(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| format!("Info.plist is not valid UTF-8 XML: {e}"))?;
    let key_index = text
        .find("<key>CFBundleExecutable</key>")
        .ok_or_else(|| "Info.plist is missing CFBundleExecutable".to_string())?;
    let after_key = &text[key_index..];
    let string_start = after_key
        .find("<string>")
        .ok_or_else(|| "Info.plist has CFBundleExecutable without string value".to_string())?
        + "<string>".len();
    let after_string_start = &after_key[string_start..];
    let string_end = after_string_start
        .find("</string>")
        .ok_or_else(|| "Info.plist has unterminated CFBundleExecutable string".to_string())?;
    let executable = after_string_start[..string_end].trim();
    if executable.is_empty() || executable.contains('/') {
        return Err("Info.plist CFBundleExecutable value is invalid".to_string());
    }
    Ok(executable.to_string())
}

fn arch_labels(arches: &BTreeSet<CpuArch>) -> String {
    if arches.is_empty() {
        return "none".to_string();
    }
    arches
        .iter()
        .map(|arch| arch.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn macho_architectures(bytes: &[u8]) -> Result<BTreeSet<CpuArch>, String> {
    if bytes.len() < 8 {
        return Err("file too small for Mach-O header".to_string());
    }

    const MH_MAGIC: u32 = 0xfeed_face;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const FAT_MAGIC: u32 = 0xcafe_babe;
    const FAT_MAGIC_64: u32 = 0xcafe_babf;

    let magic_be = read_u32_be(bytes, 0)?;
    let magic_le = read_u32_le(bytes, 0)?;

    if magic_be == FAT_MAGIC || magic_be == FAT_MAGIC_64 {
        return parse_fat_macho(bytes, magic_be == FAT_MAGIC_64, Endian::Big);
    }
    if magic_le == FAT_MAGIC || magic_le == FAT_MAGIC_64 {
        return parse_fat_macho(bytes, magic_le == FAT_MAGIC_64, Endian::Little);
    }

    if magic_be == MH_MAGIC || magic_be == MH_MAGIC_64 {
        return parse_thin_macho(bytes, Endian::Big);
    }
    if magic_le == MH_MAGIC || magic_le == MH_MAGIC_64 {
        return parse_thin_macho(bytes, Endian::Little);
    }

    Err(format!("unrecognized Mach-O magic 0x{magic_be:08x}"))
}

#[derive(Debug, Clone, Copy)]
enum Endian {
    Big,
    Little,
}

fn parse_thin_macho(bytes: &[u8], endian: Endian) -> Result<BTreeSet<CpuArch>, String> {
    let cpu_type = read_u32(bytes, 4, endian)?;
    Ok(CpuArch::from_mach_cpu_type(cpu_type).into_iter().collect())
}

fn parse_fat_macho(
    bytes: &[u8],
    is_64_bit: bool,
    endian: Endian,
) -> Result<BTreeSet<CpuArch>, String> {
    let nfat_arch = read_u32(bytes, 4, endian)? as usize;
    let entry_size = if is_64_bit { 32 } else { 20 };
    let required = 8usize
        .checked_add(
            nfat_arch
                .checked_mul(entry_size)
                .ok_or_else(|| "fat Mach-O architecture table is too large".to_string())?,
        )
        .ok_or_else(|| "fat Mach-O architecture table is too large".to_string())?;
    if bytes.len() < required {
        return Err(format!(
            "fat Mach-O header truncated: need {required} bytes, got {}",
            bytes.len()
        ));
    }

    let mut arches = BTreeSet::new();
    for index in 0..nfat_arch {
        let offset = 8 + index * entry_size;
        let cpu_type = read_u32(bytes, offset, endian)?;
        if let Some(arch) = CpuArch::from_mach_cpu_type(cpu_type) {
            arches.insert(arch);
        }
    }
    Ok(arches)
}

fn read_u32(bytes: &[u8], offset: usize, endian: Endian) -> Result<u32, String> {
    match endian {
        Endian::Big => read_u32_be(bytes, offset),
        Endian::Little => read_u32_le(bytes, offset),
    }
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("truncated u32 at offset {offset}"))?;
    Ok(u32::from_be_bytes(
        slice.try_into().expect("slice length checked"),
    ))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("truncated u32 at offset {offset}"))?;
    Ok(u32::from_le_bytes(
        slice.try_into().expect("slice length checked"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMITTED_TAURI_CONF: &str = include_str!("../tauri.conf.json");
    const RELEASE_TAURI_CONF: &str = include_str!("../tauri.release.conf.json");

    fn updater_section(conf: &str) -> serde_json::Value {
        let parsed: serde_json::Value = serde_json::from_str(conf).expect("valid JSON");
        parsed["plugins"]["updater"].clone()
    }

    /// Open-source default: the committed config must never carry an update
    /// endpoint, so a plain-clone build cannot poll the maintainers' feed.
    #[test]
    fn committed_tauri_conf_has_no_updater_endpoint() {
        let updater = updater_section(COMMITTED_TAURI_CONF);
        assert!(
            !updater_endpoints_configured_in(Some(&updater)),
            "tauri.conf.json must ship with empty plugins.updater.endpoints; got {updater}"
        );
        assert_eq!(updater["pubkey"].as_str(), Some(""));
        let parsed: serde_json::Value = serde_json::from_str(COMMITTED_TAURI_CONF).unwrap();
        assert!(
            parsed["bundle"]["createUpdaterArtifacts"].is_null(),
            "createUpdaterArtifacts belongs in tauri.release.conf.json only"
        );
    }

    /// The release overlay is where the official endpoint + pubkey live.
    #[test]
    fn release_overlay_carries_official_updater_anchors() {
        let updater = updater_section(RELEASE_TAURI_CONF);
        assert!(updater_endpoints_configured_in(Some(&updater)));
        assert_eq!(
            updater["endpoints"],
            serde_json::json!(["https://app.petal.live/api/updater"])
        );
        assert!(updater["pubkey"]
            .as_str()
            .is_some_and(|key| key.len() > 40));
    }

    #[test]
    fn endpoint_detection_rejects_missing_empty_and_blank() {
        assert!(!updater_endpoints_configured_in(None));
        assert!(!updater_endpoints_configured_in(Some(&serde_json::json!({}))));
        assert!(!updater_endpoints_configured_in(Some(&serde_json::json!({"endpoints": []}))));
        assert!(!updater_endpoints_configured_in(Some(&serde_json::json!({"endpoints": [" "]}))));
        assert!(updater_endpoints_configured_in(Some(
            &serde_json::json!({"endpoints": ["https://example.com/api/updater"]})
        )));
    }

    /// Telemetry is opt-in at build time: a DSN/key is only ever baked when
    /// the build explicitly supplied it. With neither env var set during the
    /// build, nothing may be compiled in.
    #[test]
    fn telemetry_is_not_baked_without_explicit_build_env() {
        let explicit = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
        if !explicit("PETAL_SENTRY_DSN") {
            assert!(
                option_env!("PETAL_SENTRY_DSN").is_none(),
                "a Sentry DSN was baked into a build that never set PETAL_SENTRY_DSN"
            );
        }
        if !explicit("PETAL_POSTHOG_KEY") {
            assert!(
                option_env!("PETAL_POSTHOG_KEY").is_none(),
                "a PostHog key was baked into a build that never set PETAL_POSTHOG_KEY"
            );
        }
    }

    #[cfg(target_os = "macos")]
    use std::ffi::OsString;
    #[cfg(target_os = "macos")]
    use std::sync::Mutex;

    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const FAT_MAGIC: u32 = 0xcafe_babe;
    const CPU_TYPE_X86_64: u32 = 0x0100_0007;
    const CPU_TYPE_ARM64: u32 = 0x0100_000c;

    #[cfg(target_os = "macos")]
    static TMPDIR_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "macos")]
    struct TestDirectory(PathBuf);

    #[cfg(target_os = "macos")]
    impl TestDirectory {
        fn new() -> Self {
            for _ in 0..32 {
                let sequence = MAC_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = Path::new("/private/tmp").join(format!(
                    "petal-updater-test-{}-{sequence}",
                    std::process::id()
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
            panic!("could not allocate a unique updater test directory");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "macos")]
    struct EnvironmentVariableGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    #[cfg(target_os = "macos")]
    impl EnvironmentVariableGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for EnvironmentVariableGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn thin_le(cpu_type: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        bytes.extend_from_slice(&cpu_type.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes
    }

    fn fat_be(cpu_types: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        bytes.extend_from_slice(&(cpu_types.len() as u32).to_be_bytes());
        for cpu_type in cpu_types {
            bytes.extend_from_slice(&cpu_type.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
        }
        bytes
    }

    fn test_update_archive(executable_bytes: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::{Builder, Header};

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut archive = Builder::new(&mut encoder);

            let plist = br#"
                <plist>
                  <dict>
                    <key>CFBundleExecutable</key>
                    <string>Petal</string>
                  </dict>
                </plist>
            "#;
            let mut plist_header = Header::new_gnu();
            plist_header.set_size(plist.len() as u64);
            plist_header.set_cksum();
            archive
                .append_data(
                    &mut plist_header,
                    "Petal.app/Contents/Info.plist",
                    &plist[..],
                )
                .unwrap();

            let mut exe_header = Header::new_gnu();
            exe_header.set_size(executable_bytes.len() as u64);
            exe_header.set_cksum();
            archive
                .append_data(
                    &mut exe_header,
                    "Petal.app/Contents/MacOS/Petal",
                    executable_bytes,
                )
                .unwrap();
            archive.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[cfg(target_os = "macos")]
    fn test_app_bundle_archive(marker: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::{Builder, Header};

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut archive = Builder::new(&mut encoder);
            let plist = format!(
                "<plist><dict><key>CFBundleExecutable</key><string>desktop</string><key>TestMarker</key><string>{}</string></dict></plist>",
                String::from_utf8_lossy(marker)
            );
            let mut plist_header = Header::new_gnu();
            plist_header.set_size(plist.len() as u64);
            plist_header.set_mode(0o644);
            plist_header.set_cksum();
            archive
                .append_data(
                    &mut plist_header,
                    "Petal.app/Contents/Info.plist",
                    plist.as_bytes(),
                )
                .unwrap();

            let mut executable_header = Header::new_gnu();
            executable_header.set_size(marker.len() as u64);
            executable_header.set_mode(0o755);
            executable_header.set_cksum();
            archive
                .append_data(
                    &mut executable_header,
                    "Petal.app/Contents/MacOS/desktop",
                    marker,
                )
                .unwrap();
            archive.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[cfg(target_os = "macos")]
    fn write_existing_bundle(destination: &Path, marker: &[u8]) {
        std::fs::create_dir_all(destination.join("Contents/MacOS")).unwrap();
        std::fs::write(destination.join("Contents/Info.plist"), b"old plist").unwrap();
        std::fs::write(destination.join("Contents/MacOS/desktop"), marker).unwrap();
    }

    #[cfg(target_os = "macos")]
    fn assert_installed_marker(destination: &Path, marker: &[u8]) {
        assert_eq!(
            std::fs::read(destination.join("Contents/MacOS/desktop")).unwrap(),
            marker
        );
        assert!(
            std::fs::read_to_string(destination.join("Contents/Info.plist"))
                .unwrap()
                .contains(&String::from_utf8_lossy(marker).to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_stages_beside_the_destination_not_in_tmpdir() {
        let test_directory = TestDirectory::new();
        let fake_tmpdir = test_directory.path().join("fake-tmp");
        std::fs::create_dir(&fake_tmpdir).unwrap();
        let _lock = TMPDIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _tmpdir = EnvironmentVariableGuard::set("TMPDIR", &fake_tmpdir);

        let destination = test_directory.path().join("Applications").join("Petal.app");
        write_existing_bundle(&destination, b"old desktop");

        install_macos_app_bundle(&test_app_bundle_archive(b"new desktop"), &destination)
            .expect("the real macOS installer should replace the bundle");

        assert_installed_marker(&destination, b"new desktop");
        assert_eq!(
            std::fs::read_dir(&fake_tmpdir).unwrap().count(),
            0,
            "destination-volume staging must never touch TMPDIR"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_replaces_the_bundle_atomically_and_leaves_no_staging_directory() {
        let test_directory = TestDirectory::new();
        let applications = test_directory.path().join("Applications");
        let destination = applications.join("Petal.app");
        write_existing_bundle(&destination, b"original bundle");

        install_macos_app_bundle(
            &test_app_bundle_archive(b"replacement bundle"),
            &destination,
        )
        .expect("the real macOS installer should replace the bundle");

        assert_installed_marker(&destination, b"replacement bundle");
        let staging_entries = std::fs::read_dir(&applications)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".petal-update-")
            })
            .collect::<Vec<_>>();
        assert!(
            staging_entries.is_empty(),
            "successful install left staging entries behind: {staging_entries:?}"
        );
    }

    /// A download that does not unpack must leave the user with the app they
    /// already had. The extract runs entirely inside the staging directory and
    /// finishes before the backup rename, so nothing has moved yet when it
    /// fails -- this pins that ordering rather than the extract itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_extract_leaves_the_installed_bundle_untouched() {
        let test_directory = TestDirectory::new();
        let applications = test_directory.path().join("Applications");
        let destination = applications.join("Petal.app");
        write_existing_bundle(&destination, b"still the old bundle");

        let error = install_macos_app_bundle(b"not a gzip archive at all", &destination)
            .expect_err("a corrupt archive must not install");

        assert_eq!(error.stage, MacInstallStage::Extract);
        assert_eq!(
            std::fs::read(destination.join("Contents/MacOS/desktop")).unwrap(),
            b"still the old bundle"
        );
        assert!(
            std::fs::read_dir(&applications)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".petal-update-")),
            "a failed extract left its staging directory behind"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_across_a_real_volume_boundary() {
        let Some(cross_volume_dir) = std::env::var_os("PETAL_TEST_CROSS_VOLUME_DIR") else {
            eprintln!("PETAL_TEST_CROSS_VOLUME_DIR is unset; skipping cross-volume updater test");
            return;
        };
        // Reads TMPDIR, so it must not run while
        // `install_stages_beside_the_destination_not_in_tmpdir` is rewriting
        // it -- a sibling test's global write is exactly the kind of race that
        // makes a mutation check read as green for the wrong reason.
        let _lock = TMPDIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cross_volume_dir = PathBuf::from(cross_volume_dir);
        assert_ne!(
            std::fs::metadata(&cross_volume_dir).unwrap().dev(),
            std::fs::metadata(std::env::temp_dir()).unwrap().dev(),
            "PETAL_TEST_CROSS_VOLUME_DIR must be on a different volume"
        );
        let applications = cross_volume_dir.join("Applications");
        std::fs::create_dir_all(&applications).unwrap();
        let destination = applications.join("Petal.app");
        assert!(
            !destination.exists(),
            "cross-volume test refuses to replace an existing {}",
            destination.display()
        );
        write_existing_bundle(&destination, b"old cross-volume bundle");

        let result = install_macos_app_bundle(
            &test_app_bundle_archive(b"new cross-volume bundle"),
            &destination,
        );
        if result.is_ok() {
            assert_installed_marker(&destination, b"new cross-volume bundle");
        }
        let _ = std::fs::remove_dir_all(&destination);
        result.expect("destination-volume staging must avoid EXDEV across a real volume");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_destination_and_volume_classification_are_closed() {
        assert_eq!(
            install_destination_class(Path::new("/Applications/Petal.app")),
            crate::logging::InstallDestinationClassTag::Applications
        );
        assert_eq!(
            install_destination_class(Path::new("/Users/test/Applications/Petal.app")),
            crate::logging::InstallDestinationClassTag::UserApplications
        );
        assert_eq!(
            install_destination_class(Path::new("/Volumes/PETAL/Petal.app")),
            crate::logging::InstallDestinationClassTag::RemovableVolume
        );

        let test_directory = TestDirectory::new();
        let destination = test_directory.path().join("Applications/Petal.app");
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        assert_eq!(
            install_volume_boundary(&destination),
            crate::logging::InstallVolumeBoundaryTag::SameVolume
        );
    }

    #[test]
    fn launch_check_is_claimed_exactly_once_per_process() {
        assert!(claim_launch_check());
        assert!(!claim_launch_check());
        assert!(!claim_launch_check());
    }

    #[test]
    fn parses_little_endian_thin_arm64_header() {
        let arches = macho_architectures(&thin_le(CPU_TYPE_ARM64)).unwrap();
        assert_eq!(arches, BTreeSet::from([CpuArch::Arm64]));
    }

    #[test]
    fn parses_little_endian_thin_x86_64_header() {
        let arches = macho_architectures(&thin_le(CPU_TYPE_X86_64)).unwrap();
        assert_eq!(arches, BTreeSet::from([CpuArch::X86_64]));
    }

    #[test]
    fn parses_big_endian_fat_header_with_both_supported_arches() {
        let arches = macho_architectures(&fat_be(&[CPU_TYPE_X86_64, CPU_TYPE_ARM64])).unwrap();
        assert_eq!(arches, BTreeSet::from([CpuArch::Arm64, CpuArch::X86_64]));
    }

    #[test]
    fn rejects_truncated_fat_architecture_table() {
        let mut bytes = fat_be(&[CPU_TYPE_ARM64]);
        bytes.truncate(bytes.len() - 1);
        let err = macho_architectures(&bytes).unwrap_err();
        assert!(err.contains("truncated"));
    }

    #[test]
    fn rejects_unknown_magic() {
        let err = macho_architectures(&[0, 1, 2, 3, 4, 5, 6, 7]).unwrap_err();
        assert!(err.contains("unrecognized Mach-O magic"));
    }

    #[test]
    fn parses_cf_bundle_executable_from_xml_plist() {
        let plist = br#"
            <plist>
              <dict>
                <key>CFBundleExecutable</key>
                <string>Petal</string>
              </dict>
            </plist>
        "#;
        assert_eq!(parse_cf_bundle_executable(plist).unwrap(), "Petal");
    }

    #[test]
    fn extracts_declared_app_executable_architectures_from_update_archive() {
        let archive = test_update_archive(&fat_be(&[CPU_TYPE_X86_64, CPU_TYPE_ARM64]));
        let arches = app_bundle_macho_architectures(&archive).unwrap();
        assert_eq!(arches, BTreeSet::from([CpuArch::Arm64, CpuArch::X86_64]));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pe_machine_type_reads_amd64_header() {
        let mut bytes = vec![0u8; 0x60];
        bytes[0..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        assert_eq!(pe_machine_type(&bytes), Some(0x8664));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pe_machine_type_rejects_non_pe() {
        assert_eq!(pe_machine_type(b"not an executable"), None);
        // MZ magic but no PE header at e_lfanew.
        let mut truncated = vec![0u8; 0x40];
        truncated[0..2].copy_from_slice(b"MZ");
        assert_eq!(pe_machine_type(&truncated), None);
    }
}
