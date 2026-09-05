//! Real macOS permission checks + requests for Screen Recording, Microphone,
//! Camera, and Accessibility (SPEC.md §4.1's onboarding permission flow).
//!
//! Before this module, the only real permission code in the app was a Screen
//! Recording *preflight* (`window_source::has_screen_recording_access()`,
//! wrapping `CGPreflightScreenCaptureAccess()`) — there was no *request* path
//! for any of the three, and the onboarding UI's "grant" buttons were pure
//! frontend mocks. This module fills that gap with the real OS-prompt-
//! triggering calls, exposed as Tauri commands (registered in `lib.rs`).
//!
//! ## FFI strategy — why raw framework links + `objc2`'s own machinery, and
//! NOT new binding crates
//!
//! This crate deliberately does NOT add `objc2-av-foundation` /
//! `objc2-core-media` / any new `objc2-*` binding crate for the mic/camera
//! path. This codebase already fought — and won — a hard `-ObjC`
//! duplicate-Swift/ObjC-symbol linker battle (see the M0 writeup in
//! CLAUDE.md and `vendor/screencapturekit/PETAL_PATCH.md`): once `-ObjC` is
//! on the link line (required transitively by `livekit`/`webrtc-sys` on
//! macOS), it force-loads whole archives, and every crate that ships its own
//! Objective-C/Swift class metadata becomes a new surface for a
//! duplicate-symbol collision. So:
//!
//! - **Screen Recording** uses raw C `CGRequestScreenCaptureAccess()` from
//!   CoreGraphics.framework (same `#[link(name = "CoreGraphics", kind =
//!   "framework")]` pattern already in `window_source.rs`). Pure C, no class
//!   metadata.
//! - **Mic/Camera** go through `AVCaptureDevice`'s *class methods*
//!   (`authorizationStatusForMediaType:` and
//!   `requestAccessForMediaType:completionHandler:`) via the already-present
//!   `objc2` crate's `class!`/`msg_send!` machinery — which adds no class
//!   metadata of its own — plus `block2::RcBlock` (already a dependency, used
//!   the same way in `menubar.rs`) for the completion handler. AVFoundation
//!   itself is linked with `#[link(name = "AVFoundation", kind =
//!   "framework")]` only to resolve the two global `AVMediaType*` NSString
//!   constants; no Objective-C classes are imported from a binding crate.
//!
//! ## The Screen Recording "needs relaunch" quirk (IMPORTANT)
//!
//! macOS only re-reads a process's Screen Recording TCC grant at PROCESS
//! START. After the user flips Petal on in System Settings (or grants it via
//! the `CGRequestScreenCaptureAccess()` prompt), the *current* process still
//! cannot actually capture — `CGPreflightScreenCaptureAccess()` may even keep
//! returning the pre-grant value until relaunch. So `request_screen_recording`
//! returns the immediate post-prompt result, but the frontend must tell the
//! user to RELAUNCH for capture to take effect (the onboarding/`PermissionRow`
//! design already has a "Relaunch now" affordance for exactly this).
//!
//! ## The Info.plist requirement (MANDATORY — app hard-crashes without it)
//!
//! macOS hard-crashes any process that calls `requestAccessForMediaType:` for
//! audio/video without the matching usage-description key in its bundle's
//! Info.plist. `src-tauri/Info.plist` therefore declares
//! `NSMicrophoneUsageDescription` and `NSCameraUsageDescription` (alongside
//! the pre-existing `NSScreenCaptureUsageDescription`); Tauri 2 merges that
//! file into the built `.app`'s `Contents/Info.plist` by default.

// The pure status-int -> string mapping and relaunch recommendation are
// platform-independent and unit-tested (see `#[cfg(test)]` below), so they
// live outside the macOS module.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestOutcome {
    pub granted: bool,
    pub was_granted: bool,
    pub auto_relaunch_recommended: bool,
}

impl PermissionRequestOutcome {
    pub fn new(was_granted: bool, granted: bool) -> Self {
        Self {
            granted,
            was_granted,
            auto_relaunch_recommended: granted && !was_granted,
        }
    }
}

/// The four `AVAuthorizationStatus` values, as the lowercase strings the
/// frontend maps onto Onboarding's own status enums. Kept as a pure function
/// (no FFI) so it can be unit-tested without a live `AVCaptureDevice`.
///
/// Apple's `AVAuthorizationStatus`: 0=notDetermined, 1=restricted,
/// 2=denied, 3=authorized. Any other value is treated as "denied" (the safe,
/// least-privileged default) rather than panicking.
pub fn auth_status_string(raw: i64) -> &'static str {
    match raw {
        0 => "not-determined",
        1 => "restricted",
        2 => "denied",
        3 => "authorized",
        _ => "denied",
    }
}

pub fn privacy_settings_url(which: &str) -> Option<&'static str> {
    match which {
        "screenRecording" | "screen-recording" | "screen_recording" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        }
        "microphone" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        }
        "camera" => Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"),
        "accessibility" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        }
        _ => None,
    }
}

// =============================================================================
// macOS implementation
// =============================================================================

#[cfg(target_os = "macos")]
mod macos {
    use super::{auth_status_string, PermissionRequestOutcome};
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    use std::os::raw::c_void;
    use std::sync::mpsc;
    use std::time::Duration;

    // Give the user a generous window to actually read + click the OS
    // permission dialog. The Tauri command runs on a worker thread, so
    // blocking it this long is fine (it does not block the UI thread).
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // Triggers the OS Screen Recording prompt on first call, and returns
        // whether access is (now) granted. NOTE: even a `true` here does not
        // make the CURRENT process able to capture — Screen Recording is only
        // re-read at process start (see module doc comment). The frontend must
        // prompt for a relaunch.
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        static kAXTrustedCheckOptionPrompt: *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        // Global `NSString *` constants exported by AVFoundation. They resolve
        // to the FourCC media-type strings ("soun"/"vide") internally, but we
        // link the real exported symbols rather than hardcoding the literals,
        // so this can't silently break if Apple ever changes the underlying
        // representation.
        static AVMediaTypeAudio: *const NSString;
        static AVMediaTypeVideo: *const NSString;
    }

    /// Screen Recording — preflight (no prompt). Delegates to the existing
    /// `window_source` implementation rather than duplicating the
    /// `CGPreflightScreenCaptureAccess()` FFI, so there's exactly one source
    /// of truth for "do we have screen access right now."
    pub fn check_screen_recording() -> bool {
        crate::window_source::has_screen_recording_access()
    }

    /// Screen Recording — request (prompts on first call). See the module doc
    /// comment: a fresh grant still needs one relaunch before capture works.
    pub fn request_screen_recording() -> PermissionRequestOutcome {
        let was_granted = check_screen_recording();
        let granted = unsafe { CGRequestScreenCaptureAccess() };
        let outcome = PermissionRequestOutcome::new(was_granted, granted);
        log::info!(
            "permissions: request_screen_recording() -> granted={}, was_granted={}, auto_relaunch_recommended={} (capture grant is process-start scoped)",
            outcome.granted,
            outcome.was_granted,
            outcome.auto_relaunch_recommended
        );
        outcome
    }

    /// The two AV media types we care about, resolved from the real exported
    /// AVFoundation NSString constants.
    #[derive(Clone, Copy)]
    enum MediaType {
        Audio,
        Video,
    }

    impl MediaType {
        /// The `AVMediaType` NSString for this kind. Safe to call — the
        /// constants are always present once AVFoundation is linked.
        fn ns_string(self) -> &'static NSString {
            unsafe {
                let ptr = match self {
                    MediaType::Audio => AVMediaTypeAudio,
                    MediaType::Video => AVMediaTypeVideo,
                };
                // The constant is a non-null global for the lifetime of the
                // process; reborrow it as a `'static` reference.
                &*ptr
            }
        }

        fn label(self) -> &'static str {
            match self {
                MediaType::Audio => "microphone",
                MediaType::Video => "camera",
            }
        }
    }

    /// `+[AVCaptureDevice authorizationStatusForMediaType:]` — a CLASS method
    /// returning `NSInteger` (`AVAuthorizationStatus`). Does not prompt.
    fn authorization_status(media: MediaType) -> i64 {
        let cls = class!(AVCaptureDevice);
        let media_ns: &NSString = media.ns_string();
        // `authorizationStatusForMediaType:` returns NSInteger == isize on
        // 64-bit; read it as i64 for the pure mapping fn.
        let raw: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: media_ns] };
        raw as i64
    }

    /// `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]` —
    /// triggers the OS prompt (if status is not-determined) and invokes the
    /// completion handler on an arbitrary queue with the resulting BOOL. We
    /// block the (worker-thread) command until the handler fires or a timeout
    /// elapses, then re-read and return the fresh authorization status.
    fn request_access(media: MediaType) -> i64 {
        let current = authorization_status(media);
        // If the decision is already made (authorized/denied/restricted),
        // `requestAccess...` won't prompt and calls back immediately with the
        // existing decision — but there's no reason to spin up a block +
        // channel for that; just return what we already have.
        if current != 0 {
            log::info!(
                "permissions: request_{}() -- already decided ({}), not prompting",
                media.label(),
                auth_status_string(current)
            );
            return current;
        }

        let (tx, rx) = mpsc::channel::<bool>();
        // `RcBlock::new` (block2 0.6.2 API, same call shape menubar.rs uses)
        // builds a heap block from a Rust closure. The completion handler's
        // ObjC signature is `void (^)(BOOL granted)`, so the closure takes a
        // single `Bool` and returns `()`.
        let handler = block2::RcBlock::new(move |granted: Bool| {
            // The receiver may already be gone if we timed out; ignore a send
            // error rather than panicking on the AV callback thread.
            let _ = tx.send(granted.as_bool());
        });

        let cls = class!(AVCaptureDevice);
        let media_ns: &NSString = media.ns_string();
        unsafe {
            let _: () = msg_send![
                cls,
                requestAccessForMediaType: media_ns,
                completionHandler: &*handler
            ];
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(granted) => log::info!(
                "permissions: request_{}() completion handler fired, granted={}",
                media.label(),
                granted
            ),
            Err(_) => log::warn!(
                "permissions: request_{}() timed out after {:?} waiting for the OS dialog -- returning current status",
                media.label(),
                REQUEST_TIMEOUT
            ),
        }

        // Always re-read the authoritative status rather than trusting the
        // one-shot BOOL: it's the same value the frontend would get from a
        // subsequent `check_*`, and it's correct even on the timeout path.
        authorization_status(media)
    }

    pub fn check_microphone() -> String {
        auth_status_string(authorization_status(MediaType::Audio)).to_string()
    }

    pub fn check_camera() -> String {
        auth_status_string(authorization_status(MediaType::Video)).to_string()
    }

    pub fn request_microphone() -> String {
        auth_status_string(request_access(MediaType::Audio)).to_string()
    }

    pub fn request_camera() -> String {
        auth_status_string(request_access(MediaType::Video)).to_string()
    }

    pub fn check_accessibility() -> bool {
        unsafe { AXIsProcessTrusted() }
    }

    pub fn request_accessibility() -> PermissionRequestOutcome {
        let was_trusted = check_accessibility();
        let trusted = unsafe {
            let keys: [*const c_void; 1] = [kAXTrustedCheckOptionPrompt];
            let values: [*const c_void; 1] = [kCFBooleanTrue];
            let options = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                std::ptr::null(),
                std::ptr::null(),
            );
            let trusted = AXIsProcessTrustedWithOptions(options);
            if !options.is_null() {
                CFRelease(options);
            }
            trusted
        };
        let outcome = PermissionRequestOutcome::new(was_trusted, trusted);
        log::info!(
            "permissions: request_accessibility() -> trusted={}, was_trusted={}, auto_relaunch_recommended={}",
            outcome.granted,
            outcome.was_granted,
            outcome.auto_relaunch_recommended
        );
        outcome
    }

    pub fn open_privacy_settings(which: &str) -> bool {
        let Some(url) = super::privacy_settings_url(which) else {
            log::warn!("permissions: open_privacy_settings({which}) refused -- unknown pane");
            return false;
        };
        log::info!("permissions: opening System Settings privacy pane '{which}' via {url}");
        match std::process::Command::new("open").arg(url).status() {
            Ok(status) if status.success() => true,
            Ok(status) => {
                log::warn!(
                    "permissions: open_privacy_settings({which}) failed -- open exited with {status}"
                );
                false
            }
            Err(e) => {
                log::warn!(
                    "permissions: open_privacy_settings({which}) failed to launch open: {e}"
                );
                false
            }
        }
    }

    // Silence an unused-import warning for AnyObject on some objc2 feature
    // combinations while keeping the import available for msg_send inference.
    #[allow(dead_code)]
    fn _touch(_: *const AnyObject) {}
}

// =============================================================================
// Non-macOS adapters. These commands model macOS TCC gates, which do not
// exist on Windows or Linux. Report them as ready so those platforms are not
// trapped behind macOS-only System Settings links. Native capture/input
// permissions and unsupported fallbacks remain independent per platform.
// =============================================================================

#[cfg(not(target_os = "macos"))]
mod macos {
    use super::PermissionRequestOutcome;

    pub fn check_screen_recording() -> bool {
        true
    }
    pub fn request_screen_recording() -> PermissionRequestOutcome {
        PermissionRequestOutcome::new(true, true)
    }
    pub fn check_microphone() -> String {
        "authorized".to_string()
    }
    pub fn check_camera() -> String {
        "authorized".to_string()
    }
    pub fn request_microphone() -> String {
        "authorized".to_string()
    }
    pub fn request_camera() -> String {
        "authorized".to_string()
    }
    pub fn check_accessibility() -> bool {
        true
    }
    pub fn request_accessibility() -> PermissionRequestOutcome {
        PermissionRequestOutcome::new(true, true)
    }
    pub fn open_privacy_settings(_which: &str) -> bool {
        false
    }
}

// =============================================================================
// Tauri commands (registered in lib.rs's invoke_handler)
// =============================================================================

/// Screen Recording — preflight check, no prompt. Reuses
/// `window_source::has_screen_recording_access()` (one source of truth).
#[tauri::command]
pub fn check_screen_recording() -> bool {
    macos::check_screen_recording()
}

/// Screen Recording — triggers the OS prompt on first call. Returns the
/// immediate result plus whether this request freshly crossed from denied to
/// granted. macOS only re-reads this grant at process start (see module doc
/// comment), so a fresh grant should be followed by one bounded app relaunch.
#[tauri::command]
pub fn request_screen_recording() -> PermissionRequestOutcome {
    let outcome = macos::request_screen_recording();
    if !outcome.granted {
        crate::analytics::permission_denied(crate::analytics::PermissionKind::Screen);
    }
    outcome
}

/// Microphone — auth status as one of
/// "not-determined" | "restricted" | "denied" | "authorized". No prompt.
#[tauri::command]
pub fn check_microphone() -> String {
    macos::check_microphone()
}

/// Camera — auth status (same string set as `check_microphone`). No prompt.
#[tauri::command]
pub fn check_camera() -> String {
    macos::check_camera()
}

/// Microphone — triggers the OS prompt (if not yet decided), blocks until the
/// user answers (or a 60s timeout), then returns the resulting auth status
/// string.
#[tauri::command]
pub fn request_microphone() -> String {
    let status = macos::request_microphone();
    if status == "denied" || status == "restricted" {
        crate::analytics::permission_denied(crate::analytics::PermissionKind::Mic);
    }
    status
}

/// Camera — triggers the OS prompt (if not yet decided), blocks until the user
/// answers (or a 60s timeout), then returns the resulting auth status string.
#[tauri::command]
pub fn request_camera() -> String {
    let status = macos::request_camera();
    if status == "denied" || status == "restricted" {
        crate::analytics::permission_denied(crate::analytics::PermissionKind::Camera);
    }
    status
}

/// Accessibility — preflight check, no prompt. Required for replaying remote
/// control input through CGEvent.
#[tauri::command]
pub fn check_accessibility() -> bool {
    macos::check_accessibility()
}

/// Accessibility — registers Petal in the macOS Accessibility list and shows
/// the system prompt. Returns whether this request freshly crossed to trusted,
/// which the onboarding UI uses to do one bounded relaunch.
#[tauri::command]
pub fn request_accessibility() -> PermissionRequestOutcome {
    macos::request_accessibility()
}

#[tauri::command]
pub fn open_privacy_settings(which: String) -> bool {
    macos::open_privacy_settings(&which)
}

#[cfg(test)]
mod tests {
    use super::{auth_status_string, privacy_settings_url, PermissionRequestOutcome};

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_adapters_do_not_gate_on_macos_tcc() {
        assert!(super::check_screen_recording());
        assert_eq!(
            super::request_screen_recording(),
            PermissionRequestOutcome::new(true, true)
        );
        assert_eq!(super::check_microphone(), "authorized");
        assert_eq!(super::request_microphone(), "authorized");
        assert_eq!(super::check_camera(), "authorized");
        assert_eq!(super::request_camera(), "authorized");
        assert!(super::check_accessibility());
        assert_eq!(
            super::request_accessibility(),
            PermissionRequestOutcome::new(true, true)
        );
    }

    #[test]
    fn maps_known_av_authorization_statuses() {
        assert_eq!(auth_status_string(0), "not-determined");
        assert_eq!(auth_status_string(1), "restricted");
        assert_eq!(auth_status_string(2), "denied");
        assert_eq!(auth_status_string(3), "authorized");
    }

    #[test]
    fn maps_unknown_status_to_denied_default() {
        // Any out-of-range value is treated as the least-privileged "denied"
        // rather than panicking or silently claiming access.
        assert_eq!(auth_status_string(4), "denied");
        assert_eq!(auth_status_string(-1), "denied");
        assert_eq!(auth_status_string(999), "denied");
    }

    #[test]
    fn recommends_relaunch_only_for_fresh_grants() {
        assert_eq!(
            PermissionRequestOutcome::new(false, true),
            PermissionRequestOutcome {
                granted: true,
                was_granted: false,
                auto_relaunch_recommended: true,
            }
        );
        assert_eq!(
            PermissionRequestOutcome::new(true, true),
            PermissionRequestOutcome {
                granted: true,
                was_granted: true,
                auto_relaunch_recommended: false,
            }
        );
        assert_eq!(
            PermissionRequestOutcome::new(false, false),
            PermissionRequestOutcome {
                granted: false,
                was_granted: false,
                auto_relaunch_recommended: false,
            }
        );
    }

    #[test]
    fn maps_privacy_settings_panes_to_system_settings_urls() {
        assert_eq!(
            privacy_settings_url("camera"),
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Camera")
        );
        assert_eq!(
            privacy_settings_url("screenRecording"),
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        );
        assert_eq!(privacy_settings_url("bogus"), None);
    }
}
