//! Noticing that the human took back control (#658 phase 3).
//!
//! A listen-only `CGEventTap` counts input events that do NOT carry Petal's
//! synthetic-source marker — i.e. the sharer physically typing, clicking or
//! scrolling. `control_policy` refuses the whole click/key/scroll tier unless
//! this detector is healthy, because driving those without being able to notice
//! the human taking over is the failure mode the tier exists to prevent.
//!
//! Ported from `examples/event_tap_probe.rs` (#654 Q6), which proved the
//! mechanism: a synthetic event tagged via `kCGEventSourceUserData` came back
//! through the tap with its marker intact, while 59 real keystrokes came back
//! unmarked. The probe's own caveat is honoured here — it ran under a process
//! holding Input Monitoring, and Petal may not hold it, so **a tap that cannot
//! be created or whose provenance round-trip fails reports UNHEALTHY.** There is
//! no path in this module that assumes health.
//!
//! ## Health means two things, not one
//!
//! [`healthy`] is true only when BOTH hold:
//! 1. the tap exists and is enabled, and
//! 2. a marked synthetic event posted by us was observed back **with its marker
//!    intact**.
//!
//! (2) is not ceremony. Without it the tap could be delivering events while
//! provenance is unreadable, in which case our own injected input would be
//! counted as the human's — and every action would look like a takeover. The
//! self-test is a `mouseMoved` to the cursor's current position: observable by
//! the tap, invisible on screen, and it disturbs nothing.
//!
//! ## A known conservative bias
//!
//! `remote_control.rs`'s replay creates its CGEvents from a NULL event source
//! and therefore posts them UNMARKED. Where a replay falls back from the
//! accessibility route to a CGEvent post, this detector counts that post as
//! physical activity. That errs toward believing the human is present, which
//! revokes agent control rather than extending it — the safe direction. Fixing
//! it means stamping the marker inside remote_control's own event sink, which
//! is shared with human remote control and out of scope here.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Petal's synthetic-event provenance marker: the ASCII bytes "PETLSYNT".
/// Mirrors the value the #654 probe round-tripped successfully.
pub const SYNTHETIC_EVENT_MARKER: i64 = 0x5045_544C_5359_4E54;

/// `kCGEventSourceUserData` — the CGEvent integer field that carries the
/// marker. 42 is Apple's constant; naming it here keeps the magic number in one
/// place with the marker it pairs with.
pub const EVENT_SOURCE_USER_DATA_FIELD: u32 = 42;

/// The (field, value) pair every synthetic event must carry so this detector
/// does not mistake Petal's own input for the human's.
///
/// Pure, so the contract can be asserted without a window server: the field id
/// must be `kCGEventSourceUserData` and the value must be the exact eight bytes
/// the tap compares against.
pub fn provenance_stamp() -> (u32, i64) {
    (EVENT_SOURCE_USER_DATA_FIELD, SYNTHETIC_EVENT_MARKER)
}

/// Physical (unmarked) deliberate input observed since the process started.
/// Movement is excluded — a cursor drifting across the screen is not the human
/// taking over, and counting it would make every action look contested.
static PHYSICAL_INPUT: AtomicU64 = AtomicU64::new(0);
/// Events observed carrying our marker. Used only by the provenance self-test.
static MARKED_INPUT: AtomicU64 = AtomicU64::new(0);
/// Set only once the tap is enabled AND the provenance round-trip succeeded.
static HEALTHY: AtomicBool = AtomicBool::new(false);
/// Whether a detector thread is currently alive.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Asks the detector thread to wind down.
static STOP: AtomicBool = AtomicBool::new(false);

/// Deliberate physical input observed so far. A monotonically increasing
/// baseline token, not a rate.
pub fn physical_count() -> u64 {
    PHYSICAL_INPUT.load(Ordering::Relaxed)
}

/// Has the human physically acted since `baseline` was taken?
///
/// Used to auto-revoke after an action: if the sharer typed or clicked while we
/// were driving their window, the session's standing authorization is dropped
/// and the next action needs a fresh yes.
pub fn physical_activity_since(baseline: u64) -> bool {
    physical_count() > baseline
}

/// Can we currently notice the human taking over?
///
/// False until the tap is up AND provenance has been proven. Never optimistic:
/// the whole click/key/scroll tier is gated on this.
pub fn healthy() -> bool {
    HEALTHY.load(Ordering::SeqCst)
}

/// Start the detector if it is not already running. Idempotent and cheap, so
/// callers may invoke it on every tool call as a self-heal.
pub fn ensure_started() {
    STOP.store(false, Ordering::SeqCst);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return; // already alive
    }
    platform::spawn();
}

/// How long [`stop`] will wait for the detector thread to actually exit
/// before giving up and returning anyway. Comfortably above the run loop's
/// own 0.25s poll interval in `platform::run`, so a healthy exit is always
/// observed; bounded so a stuck platform hook can never hang a teardown
/// forever.
const STOP_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Ask the detector to wind down, and WAIT for it to actually finish exiting
/// before returning. Health drops immediately (an action racing a teardown
/// fails closed rather than trusting a dying tap), but that used to be the
/// only synchronous part of this call — #661 item 10: `platform::run`'s loop
/// only notices `STOP` on its next ~0.25s wake, so `RUNNING` stayed `true`
/// for up to that long after `stop()` returned. A session restarting inside
/// that window called [`ensure_started`], saw `RUNNING` already `true`, and
/// returned without spawning anything — while the old thread, now racing
/// `ensure_started`'s own `STOP.store(false, ..)`, could keep looping with
/// `HEALTHY` stuck `false` forever, since nothing re-runs the provenance
/// check outside of a fresh thread start. Waiting here for `RUNNING` to
/// actually clear makes the next `ensure_started` call unconditionally spawn
/// a fresh thread with an honest re-check, closing the race instead of
/// hoping the timing works out.
pub fn stop() {
    STOP.store(true, Ordering::SeqCst);
    HEALTHY.store(false, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + STOP_JOIN_TIMEOUT;
    while RUNNING.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
mod platform {
    // Apple's `kCGEvent*` names are kept verbatim so this stays greppable
    // against the CoreGraphics headers and the #654 probe.
    #![allow(non_upper_case_globals)]

    use super::{provenance_stamp, HEALTHY, MARKED_INPUT, PHYSICAL_INPUT, RUNNING, STOP};
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    type CFMachPortRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopRef = *mut c_void;
    type CFAllocatorRef = *const c_void;
    type CFStringRef = *const c_void;
    type CGEventTapProxy = *const c_void;
    type CGEventRef = *mut c_void;
    type CGEventSourceRef = *mut c_void;
    type CGEventTapCallBack =
        extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

    const kCGSessionEventTap: u32 = 1;
    const kCGHeadInsertEventTap: u32 = 0;
    const kCGEventTapOptionListenOnly: u32 = 1;
    const kCGHIDEventTap: u32 = 0;
    /// `kCGEventSourceStateHIDSystemState`.
    const kCGEventSourceStateHIDSystemState: u32 = 1;

    const kCGEventLeftMouseDown: u32 = 1;
    const kCGEventRightMouseDown: u32 = 3;
    const kCGEventMouseMoved: u32 = 5;
    const kCGEventKeyDown: u32 = 10;
    const kCGEventScrollWheel: u32 = 22;
    const kCGEventOtherMouseDown: u32 = 25;
    /// The OS disables a tap that is too slow, and tells it so out-of-band.
    const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
    const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventTapIsEnabled(tap: CFMachPortRef) -> bool;
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGEventSourceCreate(state_id: u32) -> CGEventSourceRef;
        fn CGEventSourceSetUserData(source: CGEventSourceRef, user_data: i64);
        fn CGEventCreateMouseEvent(
            source: CGEventSourceRef,
            mouse_type: u32,
            point: CGPoint,
            button: u32,
        ) -> CGEventRef;
        fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
        fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
        fn CGEventPost(tap: u32, event: CGEventRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFMachPortCreateRunLoopSource(
            allocator: CFAllocatorRef,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRunLoopRunInMode(
            mode: CFStringRef,
            seconds: f64,
            return_after_source_handled: bool,
        ) -> i32;
        static kCFRunLoopDefaultMode: CFStringRef;
    }

    extern "C" fn tap_callback(
        _proxy: CGEventTapProxy,
        event_type: u32,
        event: CGEventRef,
        _user: *mut c_void,
    ) -> CGEventRef {
        if event_type == kCGEventTapDisabledByTimeout
            || event_type == kCGEventTapDisabledByUserInput
        {
            // The tap stopped delivering. Until the supervisor re-enables it we
            // cannot see the human, so we must not claim we can.
            HEALTHY.store(false, Ordering::SeqCst);
            return event;
        }
        let (field, ours) = provenance_stamp();
        // SAFETY: `event` is valid for the duration of the callback.
        let marker = unsafe { CGEventGetIntegerValueField(event, field) };
        if marker == ours {
            MARKED_INPUT.fetch_add(1, Ordering::Relaxed);
            return event;
        }
        // Unreadable provenance counts as physical: fail toward revocation.
        if matches!(
            event_type,
            kCGEventLeftMouseDown
                | kCGEventRightMouseDown
                | kCGEventOtherMouseDown
                | kCGEventKeyDown
                | kCGEventScrollWheel
        ) {
            PHYSICAL_INPUT.fetch_add(1, Ordering::Relaxed);
        }
        event
    }

    /// Post one marked, invisible synthetic event and report whether the tap saw
    /// it back WITH the marker. This is the provenance half of health.
    fn provenance_round_trips() -> bool {
        let before = MARKED_INPUT.load(Ordering::Relaxed);
        // SAFETY: each created CF object is released exactly once below.
        unsafe {
            let probe = CGEventCreate(std::ptr::null_mut());
            let here = if probe.is_null() {
                CGPoint { x: 0.0, y: 0.0 }
            } else {
                let point = CGEventGetLocation(probe);
                CFRelease(probe.cast_const());
                point
            };
            let source = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
            if source.is_null() {
                return false;
            }
            // The one place a marked event is BUILT. `provenance_stamp` is the
            // single definition of what "ours" means, shared with the callback
            // above that compares against it.
            CGEventSourceSetUserData(source, provenance_stamp().1);
            let event = CGEventCreateMouseEvent(source, kCGEventMouseMoved, here, 0);
            if !event.is_null() {
                CGEventPost(kCGHIDEventTap, event);
                CFRelease(event.cast_const());
            }
            CFRelease(source.cast_const());
            // Let the tap drain it.
            for _ in 0..12 {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, false);
            }
        }
        MARKED_INPUT.load(Ordering::Relaxed) > before
    }

    pub fn spawn() {
        std::thread::spawn(|| {
            run();
            HEALTHY.store(false, Ordering::SeqCst);
            RUNNING.store(false, Ordering::SeqCst);
        });
    }

    fn run() {
        let mask: u64 = (1 << kCGEventLeftMouseDown)
            | (1 << kCGEventRightMouseDown)
            | (1 << kCGEventOtherMouseDown)
            | (1 << kCGEventMouseMoved)
            | (1 << kCGEventKeyDown)
            | (1 << kCGEventScrollWheel);

        // SAFETY: standard tap creation. NULL means the OS refused — typically a
        // missing TCC grant. That is reported as unhealthy, never assumed away.
        let tap = unsafe {
            CGEventTapCreate(
                kCGSessionEventTap,
                kCGHeadInsertEventTap,
                kCGEventTapOptionListenOnly,
                mask,
                tap_callback,
                std::ptr::null_mut(),
            )
        };
        if tap.is_null() {
            log::warn!(
                "ai_chat: takeover detector unavailable (CGEventTapCreate refused) -- agent click/key/scroll stays refused"
            );
            return;
        }

        // SAFETY: `tap` is a live CFMachPort for the lifetime of this function.
        let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
        if source.is_null() {
            log::warn!("ai_chat: takeover detector could not attach to a run loop");
            unsafe { CFRelease(tap.cast_const()) };
            return;
        }
        // SAFETY: both handles are live; the source is removed before release.
        unsafe {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CGEventTapEnable(tap, true);
        }

        if provenance_round_trips() {
            HEALTHY.store(true, Ordering::SeqCst);
            log::info!("ai_chat: takeover detector live (provenance round-trip confirmed)");
        } else {
            log::warn!(
                "ai_chat: takeover detector could not prove event provenance -- agent click/key/scroll stays refused"
            );
        }

        while !STOP.load(Ordering::SeqCst) {
            // SAFETY: run-loop pumping on this thread's own run loop.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, false) };
            // SAFETY: `tap` is still live.
            if !unsafe { CGEventTapIsEnabled(tap) } {
                HEALTHY.store(false, Ordering::SeqCst);
                // SAFETY: re-enabling a live tap.
                unsafe { CGEventTapEnable(tap, true) };
                if unsafe { CGEventTapIsEnabled(tap) } && provenance_round_trips() {
                    HEALTHY.store(true, Ordering::SeqCst);
                }
            }
        }

        // SAFETY: teardown in the reverse order of setup; each handle released
        // exactly once.
        unsafe {
            CGEventTapEnable(tap, false);
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFRelease(source.cast_const());
            CFRelease(tap.cast_const());
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    /// No event tap off macOS, so the detector is permanently unhealthy — which
    /// keeps the click/key/scroll tier refused rather than silently available.
    pub fn spawn() {
        super::RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_ext::MutexExt;

    /// Serializes every test that touches this module's process-global flags,
    /// and clears them on the way OUT. #780: `stopping_drops_health_immediately`
    /// stores `HEALTHY = true` before `stop()` clears it, and cargo runs tests
    /// as threads in ONE process -- so a parallel
    /// `a_detector_that_never_started_is_not_healthy` read that transient
    /// `true` (~1 red in 10 full-suite runs). Drop does the clearing rather
    /// than each test, because a panic or an early return would skip
    /// hand-written cleanup and silently re-open the flake.
    struct TakeoverTestGuard {
        // Dropped AFTER the Drop body below, so the flags are already clean
        // before any waiting test can acquire the lock.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for TakeoverTestGuard {
        fn drop(&mut self) {
            HEALTHY.store(false, Ordering::SeqCst);
            RUNNING.store(false, Ordering::SeqCst);
            STOP.store(false, Ordering::SeqCst);
        }
    }

    fn takeover_test_lock() -> TakeoverTestGuard {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        TakeoverTestGuard {
            _lock: LOCK.get_or_init(|| std::sync::Mutex::new(())).lock_unpoisoned(),
        }
    }

    #[test]
    fn the_marker_is_the_bytes_the_probe_round_tripped() {
        let (field, value) = provenance_stamp();
        assert_eq!(field, EVENT_SOURCE_USER_DATA_FIELD);
        assert_eq!(field, 42, "kCGEventSourceUserData");
        assert_eq!(value, SYNTHETIC_EVENT_MARKER);
        // Spelling it out as bytes catches a transposed nibble that a hex
        // literal would hide.
        assert_eq!(value.to_be_bytes(), *b"PETLSYNT");
    }

    #[test]
    fn a_detector_that_never_started_is_not_healthy() {
        let _guard = takeover_test_lock();
        // The unit-test process never brings the tap up -- nothing here can:
        // HEALTHY is only set inside `platform::run`, reachable solely via
        // `ensure_started()`, whose one crate caller is the env-gated AI-chat
        // websocket connect path. So this reads the real fail-closed default,
        // ambiently, rather than one the test just wrote itself. The guard is
        // what makes that deterministic: it clears the flags when the previous
        // test released it, not when this one acquires it (#780).
        assert!(
            !healthy(),
            "HEALTHY must default false -- the whole click/key/scroll tier is gated on it"
        );
    }

    #[test]
    fn activity_is_measured_against_a_baseline_not_an_absolute() {
        let baseline = physical_count();
        assert!(!physical_activity_since(baseline));
        // A later baseline can never look like activity has gone backwards.
        assert!(!physical_activity_since(physical_count()));
    }

    #[test]
    fn stopping_drops_health_immediately() {
        let _guard = takeover_test_lock();
        // A teardown racing an in-flight action must fail closed. The guard
        // clears HEALTHY again on drop, so this `true` can never escape to
        // the sibling test that asserts the fail-closed default.
        HEALTHY.store(true, Ordering::SeqCst);
        stop();
        assert!(!healthy());
    }

    /// #661 item 10 regression. `platform::run`'s loop only notices `STOP` on
    /// its own ~0.25s poll wake, so the real detector thread can stay
    /// `RUNNING` for a while after `stop()` sets the flag. If `stop()`
    /// returned immediately, a session restarting inside that window would
    /// call `ensure_started`, see `RUNNING` still true, and return without
    /// spawning a fresh thread -- permanently leaving `HEALTHY` false with no
    /// thread left alive to ever set it true again.
    ///
    /// This test stands in for that real timing without touching a real
    /// `CGEventTap`: a background thread holds `RUNNING` true for 50ms (the
    /// role `platform::run`'s wind-down plays), and `stop()` must not return
    /// before that clears. Reverting `stop()` to not wait makes the timing
    /// assertion fail (it returns near-instantly instead).
    #[test]
    fn stop_waits_for_running_to_actually_clear_before_returning() {
        let _guard = takeover_test_lock();
        RUNNING.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let clearer = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            RUNNING.store(false, Ordering::SeqCst);
        });

        stop();
        let elapsed = started.elapsed();
        clearer.join().unwrap();

        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "stop() returned after {elapsed:?} -- it must wait for RUNNING to \
             actually clear, not just for STOP to be set, or a restart racing \
             the old thread's wind-down sees RUNNING still true and does nothing"
        );
        assert!(!RUNNING.load(Ordering::SeqCst));
    }
}
