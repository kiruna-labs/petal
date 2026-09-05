//! Display-sleep prevention (#259/#264): a thin RAII wrapper around IOKit's
//! `IOPMAssertionCreateWithName`/`IOPMAssertionRelease`, held for the
//! lifetime of an active meeting (`session::room::join_room`/`leave_room`)
//! so macOS doesn't idle the display to sleep while Petal is actively
//! rendering ~30fps remote shares (or publishing one).
//!
//! ## Why this alone is not the fix
//!
//! `kIOPMAssertionTypePreventUserIdleDisplaySleep` only prevents *idle*
//! display sleep (the OS's own inactivity timer). It does NOT prevent a
//! user forcing sleep -- lid close, `pmset displaysleepnow`, a Control
//! Center "Lock" -- which is exactly the scenario in the real crash this
//! issue fixes (see CLAUDE.md's display-sleep crash class). The real safety
//! net is `resilience.rs`'s `screensDidSleep`/`screensDidWake` observers,
//! which pause/resume `compositor.rs`'s `AVSampleBufferDisplayLayer` enqueue
//! regardless of whether this assertion is held. This assertion is a
//! best-effort improvement to the common case (idle timeout), not a
//! substitute for the pause/resume path.
//!
//! ## Why raw FFI instead of a crate
//!
//! No `IOPMLib.h` Rust binding crate exists in this workspace's dependency
//! graph (checked directly). This follows the same house pattern
//! `platform::cg`/`capture.rs::color_profile_for_display_id` already use for
//! a couple of framework calls: `#[link(name = ..., kind = "framework")]
//! extern "C"` against the stable C ABI, rather than pulling in a new crate
//! for two functions.

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    type IoReturn = i32;
    type IoPmAssertionId = u32;
    type IoPmAssertionLevel = u32;

    const K_IOPM_ASSERTION_LEVEL_ON: IoPmAssertionLevel = 255;
    const K_IO_RETURN_SUCCESS: IoReturn = 0;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: IoPmAssertionLevel,
            assertion_name: CFStringRef,
            assertion_id: *mut IoPmAssertionId,
        ) -> IoReturn;
        fn IOPMAssertionRelease(assertion_id: IoPmAssertionId) -> IoReturn;
    }

    /// RAII handle for one held `PreventUserIdleDisplaySleep` assertion.
    /// Dropping it releases the assertion via `IOPMAssertionRelease`. Not
    /// `Clone`/`Copy` -- exactly one assertion is held per active meeting
    /// (owned by `session::room::RoomJoinInfo`), so ownership maps directly
    /// onto the meeting lifecycle: created in `join_room`, dropped whenever
    /// `SessionInner.joined` is cleared (`leave_room` or a forced
    /// disconnect's cleanup), with no separate teardown call needed.
    pub struct DisplaySleepAssertion {
        id: IoPmAssertionId,
    }

    impl DisplaySleepAssertion {
        /// Acquire the assertion. `reason` is a short human-readable label
        /// IOKit surfaces in `pmset -g assertions` -- e.g. `"Petal meeting:
        /// <room name>"` -- for live verification (DoD: "verify with `pmset
        /// -g assertions` while a meeting is active vs. not"). Returns
        /// `None` (logged as a warning) if IOKit refuses; this must never be
        /// a hard failure that blocks joining a meeting.
        pub fn acquire(reason: &str) -> Option<Self> {
            // `kIOPMAssertionTypePreventUserIdleDisplaySleep`'s real value is
            // literally the string "PreventUserIdleDisplaySleep" (Apple's
            // IOPMLib.h: `#define kIOPMAssertionTypePreventUserIdleDisplaySleep
            // CFSTR("PreventUserIdleDisplaySleep")`) -- built directly rather
            // than linked as an extern CFStringRef symbol, avoiding one more
            // FFI symbol to resolve for a value that's a stable, documented
            // constant.
            let assertion_type = CFString::new("PreventUserIdleDisplaySleep");
            let assertion_name = CFString::new(reason);
            let mut id: IoPmAssertionId = 0;
            let status = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type.as_concrete_TypeRef(),
                    K_IOPM_ASSERTION_LEVEL_ON,
                    assertion_name.as_concrete_TypeRef(),
                    &mut id,
                )
            };
            if status == K_IO_RETURN_SUCCESS {
                log::info!(
                    "platform::power: acquired PreventUserIdleDisplaySleep assertion (id={id}, reason='{reason}')"
                );
                Some(Self { id })
            } else {
                log::warn!(
                    "platform::power: IOPMAssertionCreateWithName failed with status {status} -- \
                     display may idle-sleep during this meeting (screensDidSleep/Wake pause/resume \
                     still applies regardless)"
                );
                None
            }
        }
    }

    impl Drop for DisplaySleepAssertion {
        fn drop(&mut self) {
            let status = unsafe { IOPMAssertionRelease(self.id) };
            if status == K_IO_RETURN_SUCCESS {
                log::info!(
                    "platform::power: released display-sleep assertion (id={})",
                    self.id
                );
            } else {
                log::warn!(
                    "platform::power: IOPMAssertionRelease failed with status {status} for id={}",
                    self.id
                );
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::DisplaySleepAssertion;

/// Non-macOS stub -- this app is macOS-only (see CLAUDE.md), but keeping the
/// type buildable everywhere matches this codebase's existing pattern of
/// `#[cfg(not(target_os = "macos"))]` no-op stubs for platform FFI (e.g.
/// `capture.rs::color_profile_for_display_id`).
#[cfg(not(target_os = "macos"))]
pub struct DisplaySleepAssertion;

#[cfg(not(target_os = "macos"))]
impl DisplaySleepAssertion {
    pub fn acquire(_reason: &str) -> Option<Self> {
        None
    }
}
