//! Where this launch is running FROM, decided once at startup (#172).
//!
//! A Petal opened straight from the mounted `.dmg`, or App-Translocated
//! after a quarantined download, cannot update itself: the updater's install
//! step fails on the read-only or ephemeral location, and until now that
//! failure was the only place the situation was detected -- so those
//! installs never updated and the user was never told. The class is probed
//! before `tauri::Builder` (so LaunchServices registration can be skipped for
//! an ephemeral bundle path) and exposed to the webview as a closed string,
//! never a raw path; the root route diverts to `/relocate` for a class that
//! needs the user to move the app.
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchLocationClass {
    /// `/Applications/...`
    Applications,
    /// `/Users/<user>/Applications/...`
    UserApplications,
    /// A read-only volume under `/Volumes` -- the mounted disk image.
    DiskImage,
    /// Gatekeeper's App Translocation: a randomized read-only mount under
    /// `.../AppTranslocation/...`. The original location is not knowable
    /// without private API, so the notice can only give instructions.
    Translocated,
    /// Read-only somewhere else (a read-only network share, say).
    OtherReadOnly,
    /// Writable, somewhere other than an Applications folder. Not a problem
    /// for the updater.
    Other,
    /// Not inside a `.app` bundle at all (a dev binary).
    Unbundled,
}

impl LaunchLocationClass {
    /// Closed wire value for the webview and for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applications => "applications",
            Self::UserApplications => "user_applications",
            Self::DiskImage => "disk_image",
            Self::Translocated => "translocated",
            Self::OtherReadOnly => "other_read_only",
            Self::Other => "other",
            Self::Unbundled => "unbundled",
        }
    }

    /// The updater cannot replace this bundle in place; the user has to move
    /// it (the standard Finder drag) before updates work.
    pub fn needs_relocation(self) -> bool {
        matches!(
            self,
            Self::DiskImage | Self::Translocated | Self::OtherReadOnly
        )
    }

    /// Registering an ephemeral path with LaunchServices would make the Dock
    /// and `open -b` point at a mount that disappears on eject.
    pub fn skip_launch_services_registration(self) -> bool {
        self.needs_relocation()
    }
}

/// Pure classification. `bundle` is the running `.app` (None for an
/// unbundled binary); `read_only` is whether its filesystem is mounted
/// read-only.
pub fn classify(bundle: Option<&Path>, read_only: bool) -> LaunchLocationClass {
    let Some(bundle) = bundle else {
        return LaunchLocationClass::Unbundled;
    };
    if bundle
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
    {
        return LaunchLocationClass::Translocated;
    }
    if bundle.strip_prefix("/Volumes").is_ok() && read_only {
        return LaunchLocationClass::DiskImage;
    }
    if bundle.strip_prefix("/Applications").is_ok() {
        return LaunchLocationClass::Applications;
    }
    if bundle.starts_with("/Users")
        && bundle
            .components()
            .any(|component| component.as_os_str() == "Applications")
    {
        return LaunchLocationClass::UserApplications;
    }
    if read_only {
        return LaunchLocationClass::OtherReadOnly;
    }
    LaunchLocationClass::Other
}

static CLASS: OnceLock<LaunchLocationClass> = OnceLock::new();

/// Decide the class for this process. Called once from `run()` BEFORE
/// LaunchServices registration; idempotent afterwards.
#[cfg(target_os = "macos")]
pub fn probe_at_startup() -> LaunchLocationClass {
    *CLASS.get_or_init(|| {
        let bundle = crate::platform::launch_services::running_bundle_path();
        let read_only = bundle
            .as_deref()
            .is_some_and(crate::updater::filesystem_is_read_only);
        let class = classify(bundle.as_deref(), read_only);
        if class.needs_relocation() {
            log::warn!(
                "launch_location: running from {} ({}) -- the updater cannot install here; the app will ask the user to move it to Applications (#172)",
                class.as_str(),
                bundle
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            );
        } else {
            log::info!("launch_location: {}", class.as_str());
        }
        class
    })
}

#[cfg(not(target_os = "macos"))]
pub fn probe_at_startup() -> LaunchLocationClass {
    *CLASS.get_or_init(|| LaunchLocationClass::Other)
}

pub fn current() -> LaunchLocationClass {
    probe_at_startup()
}

/// The closed class string for the webview's launch router. Never a path.
#[tauri::command]
pub fn launch_location_class() -> String {
    current().as_str().to_string()
}

/// "Open Applications": the user performs the standard Finder move. This
/// deliberately does NOT reuse the updater's privileged replacement script
/// for a one-click move -- different collision, signature, quarantine,
/// ownership and rollback semantics.
#[tauri::command]
pub fn open_applications_folder() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("/Applications")
            .status()
            .map_err(|error| error.to_string())
            .and_then(|status| {
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("open exited with {status}"))
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("not applicable on this platform".to_string())
    }
}

/// "Show Petal in Finder": reveal the running bundle so the user can drag it.
/// Only meaningful for a disk-image run (a translocated bundle lives in a
/// hidden temporary mount); the webview offers it only for that class.
#[tauri::command]
pub fn reveal_running_bundle() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let bundle = crate::platform::launch_services::running_bundle_path()
            .ok_or_else(|| "not running from an app bundle".to_string())?;
        std::process::Command::new("open")
            .arg("-R")
            .arg(&bundle)
            .status()
            .map_err(|error| error.to_string())
            .and_then(|status| {
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("open -R exited with {status}"))
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("not applicable on this platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_closed_and_ordered() {
        let p = Path::new;
        assert_eq!(
            classify(Some(p("/Volumes/Petal/Petal.app")), true),
            LaunchLocationClass::DiskImage
        );
        // A writable external volume is not a disk image.
        assert_eq!(
            classify(Some(p("/Volumes/Tim's SSD/Petal.app")), false),
            LaunchLocationClass::Other
        );
        assert_eq!(
            classify(
                Some(p("/private/var/folders/d8/hvxv/T/AppTranslocation/6B1C-4F2A/d/Petal.app")),
                true
            ),
            LaunchLocationClass::Translocated
        );
        // Translocation wins over any other reading of the path.
        assert_eq!(
            classify(Some(p("/Volumes/x/AppTranslocation/y/Petal.app")), true),
            LaunchLocationClass::Translocated
        );
        assert_eq!(
            classify(Some(p("/Applications/Petal.app")), false),
            LaunchLocationClass::Applications
        );
        assert_eq!(
            classify(Some(p("/Users/alice/Applications/Petal.app")), false),
            LaunchLocationClass::UserApplications
        );
        assert_eq!(
            classify(Some(p("/Users/alice/Downloads/Petal.app")), false),
            LaunchLocationClass::Other
        );
        assert_eq!(
            classify(Some(p("/mnt/readonly-share/Petal.app")), true),
            LaunchLocationClass::OtherReadOnly
        );
        assert_eq!(classify(None, false), LaunchLocationClass::Unbundled);
    }

    #[test]
    fn only_ephemeral_or_read_only_classes_need_relocation_and_skip_registration() {
        for class in [
            LaunchLocationClass::DiskImage,
            LaunchLocationClass::Translocated,
            LaunchLocationClass::OtherReadOnly,
        ] {
            assert!(class.needs_relocation(), "{class:?}");
            assert!(class.skip_launch_services_registration(), "{class:?}");
        }
        for class in [
            LaunchLocationClass::Applications,
            LaunchLocationClass::UserApplications,
            LaunchLocationClass::Other,
            LaunchLocationClass::Unbundled,
        ] {
            assert!(!class.needs_relocation(), "{class:?}");
            assert!(!class.skip_launch_services_registration(), "{class:?}");
        }
    }

    /// The wire value is what the webview router switches on; keep it stable.
    #[test]
    fn wire_values_are_stable_snake_case() {
        let all = [
            (LaunchLocationClass::Applications, "applications"),
            (LaunchLocationClass::UserApplications, "user_applications"),
            (LaunchLocationClass::DiskImage, "disk_image"),
            (LaunchLocationClass::Translocated, "translocated"),
            (LaunchLocationClass::OtherReadOnly, "other_read_only"),
            (LaunchLocationClass::Other, "other"),
            (LaunchLocationClass::Unbundled, "unbundled"),
        ];
        for (class, wire) in all {
            assert_eq!(class.as_str(), wire);
        }
    }
}
