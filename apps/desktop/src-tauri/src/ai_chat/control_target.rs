//! Truthful answers to the questions `control_policy` asks (#658 phase 3).
//!
//! Phase 2 hard-coded `bundle_id: None` and `SecureInput::Unknown`, which was
//! honest — the answers were genuinely unavailable — but meant every action was
//! refused. This module supplies the real ones.
//!
//! Every function here fails toward the denying answer, because that is what
//! the policy does with it:
//! - an unresolvable frontmost application yields `None`, which
//!   `blocklist_reason` refuses as `unknown_target_application`;
//! - an unreadable secure-input state yields [`SecureInput::Unknown`], which the
//!   policy treats exactly like "active".
//!
//! ## Why secure input is loaded by hand
//!
//! `IsSecureEventInputEnabled` lives in HIToolbox (inside Carbon). Linking
//! Carbon at build time would make a missing symbol a *link* failure; resolving
//! it at runtime makes it a **denial** instead. That is the behaviour we want:
//! on a system where the answer cannot be obtained, control simply does not
//! work, rather than working without the check.

use super::control_policy::SecureInput;

/// The application currently in front, as far as the window server is
/// concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmostApp {
    pub pid: i32,
    /// `None` when the app publishes no bundle identifier. The policy refuses
    /// on `None`, so an unidentifiable app is never driven.
    pub bundle_id: Option<String>,
}

#[cfg(target_os = "macos")]
mod native {
    use super::FrontmostApp;
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    /// Frontmost application (pid + bundle id), or `None` if there isn't one.
    ///
    /// A blank bundle identifier is normalized to `None` so it reaches
    /// `blocklist_reason` as "unresolvable" rather than as an empty string that
    /// would match nothing on the blocklist and sail through.
    ///
    /// Called off the main thread by the execution path's frontmost poll, which
    /// matches existing practice here — `platform::appkit::frontmost_app_label`
    /// reads the same two properties the same way and records that this read is
    /// main-thread-independent (unlike creating or closing AppKit windows, the
    /// crash class CLAUDE.md warns about).
    pub fn frontmost_app() -> Option<FrontmostApp> {
        use objc2_app_kit::NSWorkspace;

        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let pid = app.processIdentifier();
        let bundle_id = app
            .bundleIdentifier()
            .map(|id| id.to_string())
            .filter(|id| !id.trim().is_empty());
        Some(FrontmostApp { pid, bundle_id })
    }

    /// Bundle identifier for a specific process, used to blocklist-check the
    /// *target* application rather than only whatever happens to be in front.
    pub fn bundle_id_for_pid(pid: i32) -> Option<String> {
        use objc2_app_kit::NSRunningApplication;

        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        app.bundleIdentifier()
            .map(|id| id.to_string())
            .filter(|id| !id.trim().is_empty())
    }

    const HITOOLBOX: &[u8] =
        b"/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/HIToolbox\0";
    const SYMBOL: &[u8] = b"IsSecureEventInputEnabled\0";
    const RTLD_LAZY: i32 = 0x1;

    extern "C" {
        fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    type IsSecureEventInputEnabled = unsafe extern "C" fn() -> u8;

    /// Resolve the symbol once. A failure is cached as `None`, which keeps the
    /// answer `Unknown` (and therefore denying) rather than retrying on every
    /// action.
    fn secure_input_symbol() -> Option<IsSecureEventInputEnabled> {
        static SYMBOL_CACHE: OnceLock<Option<usize>> = OnceLock::new();
        let address = (*SYMBOL_CACHE.get_or_init(|| {
            // SAFETY: both strings are static and NUL-terminated. The handle is
            // intentionally never closed — the framework stays loaded for the
            // process lifetime, and closing it would invalidate the pointer.
            unsafe {
                let handle = dlopen(HITOOLBOX.as_ptr().cast::<c_char>(), RTLD_LAZY);
                if handle.is_null() {
                    return None;
                }
                let symbol = dlsym(handle, SYMBOL.as_ptr().cast::<c_char>());
                if symbol.is_null() {
                    None
                } else {
                    Some(symbol as usize)
                }
            }
        }))?;
        // SAFETY: `address` came from `dlsym` for a symbol whose real C
        // signature is `Boolean IsSecureEventInputEnabled(void)`.
        Some(unsafe { std::mem::transmute::<usize, IsSecureEventInputEnabled>(address) })
    }

    pub fn secure_input_active() -> Option<bool> {
        let symbol = secure_input_symbol()?;
        // SAFETY: no arguments, returns a Boolean.
        Some(unsafe { symbol() } != 0)
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::FrontmostApp;

    pub fn frontmost_app() -> Option<FrontmostApp> {
        None
    }

    pub fn bundle_id_for_pid(_pid: i32) -> Option<String> {
        None
    }

    pub fn secure_input_active() -> Option<bool> {
        None
    }
}

pub use native::{bundle_id_for_pid, frontmost_app};

/// Tri-state secure-input reading. `Unknown` when the platform cannot answer,
/// which the policy refuses exactly as it refuses `Active`.
pub fn secure_input_state() -> SecureInput {
    secure_input_from_reading(native::secure_input_active())
}

/// Map a raw platform reading to the policy's tri-state. Split out so the
/// "unavailable means Unknown, not Inactive" mapping is testable without a
/// running window server.
pub fn secure_input_from_reading(reading: Option<bool>) -> SecureInput {
    match reading {
        Some(true) => SecureInput::Active,
        Some(false) => SecureInput::Inactive,
        None => SecureInput::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_chat::control_policy::{grant_decision, Action, Decision, GrantContext};

    #[test]
    fn an_unreadable_secure_input_state_is_unknown_not_inactive() {
        assert_eq!(secure_input_from_reading(None), SecureInput::Unknown);
        assert_eq!(secure_input_from_reading(Some(true)), SecureInput::Active);
        assert_eq!(secure_input_from_reading(Some(false)), SecureInput::Inactive);
    }

    #[test]
    fn an_unavailable_reading_still_refuses_through_the_policy() {
        // The mapping only matters because of what the policy then does with
        // it; assert the end-to-end denial rather than just the enum.
        let ctx = GrantContext {
            window_present: true,
            bundle_id: Some("com.apple.TextEdit"),
            secure_input: secure_input_from_reading(None),
            takeover_detection_healthy: true,
            remote_control_allowed: true,
            ai_chat_enabled: true,
        };
        assert_eq!(
            grant_decision(&Action::Type("hi".into()), &ctx),
            Decision::Refuse {
                code: "secure_input_active"
            }
        );
    }

    #[test]
    fn a_blank_bundle_identifier_is_treated_as_absent() {
        // A frontmost app that publishes an empty bundle id must read as
        // `None` so `blocklist_reason` refuses it, not as `Some("")` which
        // would sail past the blocklist.
        let blank: Option<String> = Some("   ".to_string()).filter(|id| !id.trim().is_empty());
        assert!(blank.is_none());
        assert_eq!(
            crate::ai_chat::control_policy::blocklist_reason(blank.as_deref()),
            Some("unknown_target_application")
        );
    }
}
