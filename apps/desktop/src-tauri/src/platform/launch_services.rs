//! LaunchServices registration repair (#902).
//!
//! **Why this module exists.** The macOS updater replaces the installed
//! `.app` bundle in place while the current process is still running
//! (`updater::install_macos_app_bundle_inner`). Doing that can leave
//! LaunchServices with no usable record of the bundle. On an affected machine
//! the app ran with a working menu bar and NO DOCK ICON, and it did **not**
//! self-heal: quitting, relaunching and rebooting all left it broken, and only
//! an explicit `lsregister -f /Applications/Petal.app` recovered it. So a user
//! who takes an in-place update can lose their Dock icon permanently, with no
//! way to discover why.
//!
//! The two halves come apart because they have different owners: the **menu
//! bar** is drawn by the in-process `NSApplication` and does not consult
//! LaunchServices, so the app looks fine, while the **Dock tile** depends on
//! the LaunchServices record, which is gone.
//!
//! Not an activation-policy problem: an Accessory-policy app has neither a
//! Dock tile nor a menu bar, and the affected app had a menu bar. #823/#824/
//! #705 are all ruled out -- see #902.
//!
//! **Do not detect this with `NSRunningApplication`.** Its `bundleIdentifier`
//! describes the process check-in `NSApplicationMain` performs, not the
//! on-disk record: measured against a bundle toggled with `lsregister -f`/`-u`
//! it reads the SAME in both states, so a gate built on it silently never
//! fires. Only `LSCopyApplicationURLsForBundleIdentifier` discriminates
//! (registered -> the bundle's own path; unregistered -> empty, -10814).

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::path::{Path, PathBuf};

/// `lsregister`, the LaunchServices maintenance tool. Used only as the
/// fallback when `LSRegisterURL` cannot be resolved; it lives inside a
/// framework's Support directory and is therefore less stable than the API,
/// so it is deliberately second choice.
const LSREGISTER_PATH: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const std::os::raw::c_char) -> *mut c_void;
}
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// The bundle identifier from the running app's `Info.plist`.
///
/// Deliberately `NSBundle`, not `NSRunningApplication`: `NSBundle` reads the
/// bundle on disk, so it still reports the identifier in the broken state we
/// need to detect. Returns `None` for an unbundled dev binary.
pub fn main_bundle_identifier() -> Option<String> {
    let bundle = objc2_foundation::NSBundle::mainBundle();
    let identifier = unsafe { bundle.bundleIdentifier() }?;
    let identifier = identifier.to_string();
    (!identifier.is_empty()).then_some(identifier)
}

/// The `.app` bundle containing the running executable, if there is one.
/// `None` for an unbundled dev binary.
pub fn running_bundle_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    bundle_path_for_executable(&executable)
}

/// Pure: walk `<bundle>.app/Contents/MacOS/<exe>` back to `<bundle>.app`.
/// Returns `None` for any path that is not inside a `.app` bundle, so a dev
/// binary at `target/debug/desktop` is correctly rejected.
pub fn bundle_path_for_executable(executable: &Path) -> Option<PathBuf> {
    let macos_dir = executable.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents_dir.parent()?;
    if bundle.extension()? != "app" {
        return None;
    }
    Some(bundle.to_path_buf())
}

/// Every bundle path LaunchServices currently has registered for `identifier`.
///
/// `Some(vec![])` means the database was queried and genuinely holds nothing
/// -- the #902 signature. `None` means the question could not be answered
/// (the symbol is missing on this macOS), and the caller MUST NOT read that
/// as "unregistered": treating unknown as broken would re-register on every
/// launch and churn the database.
pub fn registered_bundle_paths(identifier: &str) -> Option<Vec<PathBuf>> {
    type LsCopyUrlsFn = unsafe extern "C" fn(*const c_void, *mut *const c_void) -> *const c_void;

    let symbol = unsafe { dlsym(RTLD_DEFAULT, c"LSCopyApplicationURLsForBundleIdentifier".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: the resolved symbol is LaunchServices'
    // `CFArrayRef LSCopyApplicationURLsForBundleIdentifier(CFStringRef, CFErrorRef*)`.
    let ls_copy_urls: LsCopyUrlsFn = unsafe { std::mem::transmute(symbol) };

    let identifier = objc2_foundation::NSString::from_str(identifier);
    // `NSString` is toll-free bridged to `CFStringRef`.
    let identifier_ptr: *const c_void = objc2::rc::Retained::as_ptr(&identifier) as *const c_void;
    let mut error: *const c_void = std::ptr::null();
    // SAFETY: `identifier_ptr` is a live bridged CFStringRef for this call.
    let array_ptr = unsafe { ls_copy_urls(identifier_ptr, &mut error) };
    if array_ptr.is_null() {
        // Not an error path: this is exactly what an unregistered identifier
        // returns (with kLSApplicationNotFoundErr in `error`).
        return Some(Vec::new());
    }

    // The Copy rule: the returned CFArrayRef is +1 and ours to release.
    // `CFArrayRef` of `CFURLRef` is toll-free bridged to `NSArray<NSURL>`.
    let array: objc2::rc::Retained<objc2_foundation::NSArray<objc2_foundation::NSURL>> =
        match unsafe {
            objc2::rc::Retained::from_raw(
                array_ptr as *mut objc2_foundation::NSArray<objc2_foundation::NSURL>,
            )
        } {
            Some(array) => array,
            None => return Some(Vec::new()),
        };

    let mut paths = Vec::new();
    for url in array.iter() {
        if let Some(path) = unsafe { url.path() } {
            paths.push(PathBuf::from(path.to_string()));
        }
    }
    Some(paths)
}

/// Pure (unit-tested): does the registration already point at `bundle`?
///
/// Compared after canonicalization so `/tmp` vs `/private/tmp` and symlinked
/// paths do not read as a mismatch and trigger a pointless re-register.
pub fn registration_points_at(bundle: &Path, registered: &[PathBuf]) -> bool {
    let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let target = canonical(bundle);
    registered.iter().any(|path| canonical(path) == target)
}

/// Pure decision (#902, unit-tested): should startup repair the registration?
///
/// Repair only when this is a real bundled app (identifier and bundle path
/// both present, so a dev binary is never touched), the database was
/// successfully queried, and it does not point at us. `registered == None`
/// means "could not tell" and must never trigger a repair.
pub fn startup_repair_needed(
    bundle: Option<&Path>,
    identifier: Option<&str>,
    registered: Option<&[PathBuf]>,
) -> bool {
    let (Some(bundle), Some(_identifier), Some(registered)) = (bundle, identifier, registered)
    else {
        return false;
    };
    !registration_points_at(bundle, registered)
}

/// Register (or refresh) `bundle` with LaunchServices.
///
/// Prefers the public `LSRegisterURL` API, resolved with `dlsym` so a future
/// macOS that drops the symbol degrades to the fallback instead of failing to
/// link (same stance as `platform::sls`). Falls back to `lsregister -f`.
pub fn register_bundle(bundle: &Path) -> Result<(), String> {
    match register_via_ls_register_url(bundle) {
        Ok(()) => {
            log::info!(
                "launch_services: re-registered '{}' via LSRegisterURL (#902)",
                bundle.display()
            );
            return Ok(());
        }
        Err(error) => {
            log::warn!(
                "launch_services: LSRegisterURL unavailable or failed for '{}' ({error}); falling back to lsregister (#902)",
                bundle.display()
            );
        }
    }

    let output = std::process::Command::new(LSREGISTER_PATH)
        .arg("-f")
        .arg(bundle)
        .output()
        .map_err(|error| format!("could not run {LSREGISTER_PATH}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "lsregister -f exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    log::info!(
        "launch_services: re-registered '{}' via lsregister -f (#902)",
        bundle.display()
    );
    Ok(())
}

fn register_via_ls_register_url(bundle: &Path) -> Result<(), String> {
    type LsRegisterUrlFn = unsafe extern "C" fn(*const c_void, bool) -> i32;

    let symbol = unsafe { dlsym(RTLD_DEFAULT, c"LSRegisterURL".as_ptr()) };
    if symbol.is_null() {
        return Err("LSRegisterURL not present in this process".to_string());
    }
    // SAFETY: the symbol resolved above is LaunchServices' `LSRegisterURL`,
    // whose C signature is `OSStatus LSRegisterURL(CFURLRef, Boolean)`.
    let ls_register_url: LsRegisterUrlFn = unsafe { std::mem::transmute(symbol) };

    let url = objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str(
        &bundle.to_string_lossy(),
    ));
    // `NSURL` is toll-free bridged to `CFURLRef`.
    let url_ptr: *const c_void = objc2::rc::Retained::as_ptr(&url) as *const c_void;
    // SAFETY: `url_ptr` is a live toll-free-bridged CFURLRef for the duration
    // of this call; `true` means "update an existing registration".
    let status = unsafe { ls_register_url(url_ptr, true) };
    if status != 0 {
        return Err(format!("LSRegisterURL returned OSStatus {status}"));
    }
    Ok(())
}

/// Startup self-heal. Repairs users who were ALREADY stranded by an earlier
/// in-place update -- the updater-side fix only protects future updates, and a
/// stranded user will never run `lsregister` by hand.
///
/// Cheap and silent in the healthy case: one `Info.plist` read and one
/// LaunchServices lookup, then nothing.
pub fn repair_registration_if_missing() {
    let bundle = running_bundle_path();
    let identifier = main_bundle_identifier();
    let registered = identifier
        .as_deref()
        .and_then(registered_bundle_paths);

    if !startup_repair_needed(bundle.as_deref(), identifier.as_deref(), registered.as_deref()) {
        log::debug!(
            "launch_services: registration OK (bundle={:?}, identifier={:?}, registered={:?})",
            bundle.as_ref().map(|path| path.display().to_string()),
            identifier,
            registered
        );
        return;
    }
    let Some(bundle) = bundle else { return };
    log::warn!(
        "launch_services: this app is NOT registered with LaunchServices -- no Dock icon. \
         Repairing '{}' (identifier={:?}, registered={:?}) (#902; usually caused by an \
         in-place update replacing the bundle)",
        bundle.display(),
        identifier,
        registered
    );
    match register_bundle(&bundle) {
        Ok(()) => log::info!("launch_services: startup registration repair succeeded (#902)"),
        Err(error) => log::error!(
            "launch_services: startup registration repair FAILED for '{}': {error} (#902)",
            bundle.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_path_resolves_from_a_real_bundle_layout() {
        let executable = Path::new("/Applications/Petal.app/Contents/MacOS/desktop");
        assert_eq!(
            bundle_path_for_executable(executable),
            Some(PathBuf::from("/Applications/Petal.app"))
        );
    }

    #[test]
    fn bundle_path_rejects_an_unbundled_dev_binary() {
        // The dev binary must never be "repaired": it legitimately has no
        // bundle and no LaunchServices identity.
        for path in [
            "/Users/dev/petal/apps/desktop/src-tauri/target/debug/desktop",
            "/usr/local/bin/desktop",
            "/Applications/Petal.app/Contents/Resources/desktop",
            "/Applications/NotAnApp/Contents/MacOS/desktop",
        ] {
            assert_eq!(
                bundle_path_for_executable(Path::new(path)),
                None,
                "{path} is not inside a .app bundle and must not resolve"
            );
        }
    }

    #[test]
    fn registration_matches_only_our_own_bundle_path() {
        let bundle = Path::new("/Applications/Petal.app");
        assert!(registration_points_at(
            bundle,
            &[PathBuf::from("/Applications/Petal.app")]
        ));
        // A record pointing somewhere else is as broken as no record: the
        // Dock tile follows the registered path, not ours.
        assert!(!registration_points_at(
            bundle,
            &[PathBuf::from("/Users/dev/Downloads/Petal.app")]
        ));
        assert!(!registration_points_at(bundle, &[]));
        // One good entry among several still counts.
        assert!(registration_points_at(
            bundle,
            &[
                PathBuf::from("/Users/dev/Downloads/Petal.app"),
                PathBuf::from("/Applications/Petal.app"),
            ]
        ));
    }

    #[test]
    fn startup_repair_only_when_bundled_and_unregistered() {
        let bundle = PathBuf::from("/Applications/Petal.app");
        let ours = [bundle.clone()];
        let elsewhere = [PathBuf::from("/Users/dev/Downloads/Petal.app")];
        let none: [PathBuf; 0] = [];

        // The #902 signature: bundled, but LaunchServices holds no record.
        assert!(startup_repair_needed(
            Some(&bundle),
            Some("com.petal.app"),
            Some(&none)
        ));
        // A record pointing at a different copy is equally broken.
        assert!(startup_repair_needed(
            Some(&bundle),
            Some("com.petal.app"),
            Some(&elsewhere)
        ));
        // Healthy bundled app -- must not re-register on every launch.
        assert!(!startup_repair_needed(
            Some(&bundle),
            Some("com.petal.app"),
            Some(&ours)
        ));
        // Could not query LaunchServices: unknown is NOT broken. Treating it
        // as broken would re-register on every launch forever.
        assert!(!startup_repair_needed(
            Some(&bundle),
            Some("com.petal.app"),
            None
        ));
        // Unbundled dev binary -- nothing to repair, in any state.
        assert!(!startup_repair_needed(None, None, Some(&none)));
        assert!(!startup_repair_needed(None, Some("com.petal.app"), Some(&none)));
        assert!(!startup_repair_needed(Some(&bundle), None, Some(&none)));
    }
}
