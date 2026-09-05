//! Process memory-footprint reads (#683).
//!
//! ## macOS: `phys_footprint`, not `resident_size`
//!
//! Activity Monitor's "Memory" column is `phys_footprint`, NOT
//! `resident_size` -- `resident_size` includes shared/mapped pages this
//! process doesn't uniquely own (framework text/data mapped copy-on-write
//! across every process on the system), so it reads much higher than what
//! this app's own leak-hunting actually cares about. This is the single
//! fact most likely to get "simplified away" by whoever next touches this
//! file, swapping in the more obviously-named field. Don't.
//!
//! ## Why raw FFI instead of `mach2`
//!
//! `mach2` 0.4.3 is already resolved transitively (via `cpal`, see
//! `Cargo.lock`), but it doesn't define `task_vm_info_data_t` -- using it
//! here would still mean hand-declaring this struct AND adding a direct
//! `Cargo.toml` dependency for a crate that buys nothing beyond a couple of
//! constants this file declares itself anyway. Follows the same raw-FFI
//! house pattern as `platform::power` (see that file's own "why raw FFI"
//! section) -- no new dependency, no new `Cargo.toml` line for this
//! platform's half of the split.
//!
//! ## `task_for_pid`/entitlements do not apply here
//!
//! `task_info(mach_task_self(), ...)` targets THIS process's own task port.
//! The hardened-runtime/sandbox restriction that requires a
//! `com.apple.security.get-task-allow`-style entitlement applies to
//! `task_for_pid` targeting an *other* process -- not to a task reading its
//! own `mach_task_self()` info, which needs no entitlement.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos {
    /// Mirrors XNU's `task_vm_info` (`osfmk/mach/task_info.h`), truncated to
    /// exactly the fields through `phys_footprint` (the `TASK_VM_INFO_REV1`
    /// cutoff). Later revisions (REV2 adds an address range, REV3 adds a
    /// long tail of per-tag ledger counters) are irrelevant to this file and
    /// deliberately not declared, so the buffer handed to the kernel is
    /// sized to match exactly what is requested (see `TASK_VM_INFO_COUNT`
    /// below) -- never larger than what the kernel is told it may write
    /// into.
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        /// Added in `TASK_VM_INFO_REV1` -- the one field this whole module
        /// exists to read. See the module doc comment for why this (not
        /// `resident_size` above) is Activity Monitor's "Memory".
        phys_footprint: u64,
    }

    const TASK_VM_INFO: u32 = 22;

    /// NOT Apple's own `TASK_VM_INFO_COUNT` macro (which sizes against the
    /// current SDK's full, longer struct) -- this is sized against OUR
    /// truncated `TaskVmInfo` above, so the count requested and the buffer
    /// offered agree exactly. XNU's `task_info()` fills fields only up to
    /// whatever revision threshold the requested count clears, and never
    /// writes past that count, so a smaller-than-canonical request here is
    /// safe, not a truncation bug.
    const TASK_VM_INFO_COUNT: u32 =
        (std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<u32>()) as u32;

    const KERN_SUCCESS: i32 = 0;

    extern "C" {
        /// The process's cached task port, exported by libSystem (linked
        /// into every Rust binary by default -- no `#[link(...)]` framework
        /// needed here, unlike the IOKit/CoreFoundation calls elsewhere in
        /// `platform/`). `mach_task_self()` in C is a macro expanding to
        /// this global, not a real function symbol.
        static mach_task_self_: u32;

        fn task_info(
            target_task: u32,
            flavor: u32,
            task_info_out: *mut u32,
            task_info_count: *mut u32,
        ) -> i32;
    }

    pub fn process_footprint_bytes() -> Option<u64> {
        let mut info = TaskVmInfo::default();
        // IN: the capacity we're offering, in `natural_t` (u32) units. OUT:
        // the kernel overwrites this with however much it actually wrote.
        let mut count: u32 = TASK_VM_INFO_COUNT;
        let status = unsafe {
            task_info(
                mach_task_self_,
                TASK_VM_INFO,
                &mut info as *mut TaskVmInfo as *mut u32,
                &mut count,
            )
        };
        if status != KERN_SUCCESS {
            log::warn!("platform::mem: task_info(TASK_VM_INFO) failed with status {status}");
            return None;
        }
        // Gotcha #2 (the one that's easy to skip): a `KERN_SUCCESS` status
        // does not by itself prove `phys_footprint` was written. The kernel
        // clamps its OUTGOING count to whatever revision threshold it
        // actually filled -- an undersized returned count means this field
        // was never touched, and reading it anyway would report a
        // plausible-looking zero/garbage value instead of the honest "not
        // available" this returns.
        if count < TASK_VM_INFO_COUNT {
            log::warn!(
                "platform::mem: task_info(TASK_VM_INFO) returned a truncated count \
                 ({count} < {TASK_VM_INFO_COUNT}) -- phys_footprint not available"
            );
            return None;
        }
        Some(info.phys_footprint)
    }

    /// System memory-pressure level via `kern.memorystatus_vm_pressure_level`
    /// (#884): 1 = normal, 2 = warn, 4 = critical. `None` when the sysctl is
    /// unreadable -- never report a fabricated "normal".
    pub fn memory_pressure_level() -> Option<u32> {
        let mut level: u32 = 0;
        let mut len = std::mem::size_of::<u32>();
        let name = c"kern.memorystatus_vm_pressure_level";
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut level as *mut u32 as *mut std::ffi::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(level)
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    pub fn process_footprint_bytes() -> Option<u64> {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
        // Gotcha: `cb` MUST be set before the call -- `GetProcessMemoryInfo`
        // uses it to know how large the buffer actually is (the Win32 API
        // accepts either the smaller `PROCESS_MEMORY_COUNTERS` or this `_EX`
        // variant through the same pointer type). An unset/zero `cb` still
        // "succeeds" but leaves `PrivateUsage` reading uninitialized stack
        // memory, not an error -- a silent wrong-answer bug, not a crash.
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let result = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
                counters.cb,
            )
        };
        match result {
            Ok(()) => Some(counters.PrivateUsage as u64),
            Err(e) => {
                log::warn!("platform::mem: GetProcessMemoryInfo failed: {e}");
                None
            }
        }
    }
}

/// Non-macOS/Windows stub -- matches the existing `#[cfg(not(target_os =
/// "macos"))]` no-op pattern already used elsewhere in `platform/` (e.g.
/// `power::DisplaySleepAssertion`) so this module stays buildable
/// everywhere even though this app only ships for macOS and Windows.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod stub {
    pub fn process_footprint_bytes() -> Option<u64> {
        None
    }
}

#[cfg(target_os = "macos")]
pub use macos::process_footprint_bytes;
#[cfg(target_os = "windows")]
pub use windows_impl::process_footprint_bytes;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use stub::process_footprint_bytes;

#[cfg(target_os = "macos")]
pub use macos::memory_pressure_level;
/// Non-macOS: memory-pressure sysctl not available; report honest absence
/// (#884), same rationale as `live_pixel_buffer_count`'s platform gating.
#[cfg(not(target_os = "macos"))]
pub fn memory_pressure_level() -> Option<u32> {
    None
}

const FOOTPRINT_THROTTLE_INTERVAL: Duration = Duration::from_secs(5);

fn footprint_cache() -> &'static Mutex<Option<(Instant, Option<u64>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<u64>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Cache/throttle wrapper: re-reads `probe` only once `interval` has elapsed
/// since the last real read, otherwise returns the cached value. Kept
/// generic over the probe and split out from the `pub` entry point below so
/// tests can exercise it against a private, per-test cache and a
/// call-counting probe instead of racing the process-wide static (and
/// instead of racing the real syscall's actual wall-clock timing).
fn throttled_read(
    cache: &Mutex<Option<(Instant, Option<u64>)>>,
    now: Instant,
    interval: Duration,
    probe: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    let mut guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((last_at, last_value)) = *guard {
        if now.duration_since(last_at) < interval {
            return last_value;
        }
    }
    let value = probe();
    *guard = Some((now, value));
    value
}

/// Throttled `process_footprint_bytes()`. `capture-diag` (`session/
/// share.rs`) reads this roughly once per second; the underlying syscall is
/// cheap but there is no reason to pay it more than once per
/// `FOOTPRINT_THROTTLE_INTERVAL` (5s) -- this caches the last reading behind
/// a timestamp check so repeated calls inside the window are a plain load,
/// not a fresh `task_info`/`GetProcessMemoryInfo` call.
pub fn process_footprint_bytes_throttled() -> Option<u64> {
    throttled_read(
        footprint_cache(),
        Instant::now(),
        FOOTPRINT_THROTTLE_INTERVAL,
        process_footprint_bytes,
    )
}

/// Global counter of this app's own live (constructed, not yet dropped)
/// `native_display::OwnedCVPixelBuffer` instances -- incremented at
/// construction, decremented in `Drop` (see that type). Declared here
/// (rather than in `native_display.rs`, which is `#![cfg(target_os =
/// "macos")]`-gated for the whole file) so cross-platform code --
/// `transport::subscriber`'s receiver frame-health formatter -- can read it
/// unconditionally without a further cfg split; on any platform other than
/// macOS it simply stays at zero forever since nothing increments it there.
///
/// Blind spot, stated explicitly per #683: this counts only THIS app's own
/// decode-output buffers. It cannot see framework-internal ScreenCaptureKit
/// or libwebrtc buffers, so a clean reading rules out one specific leak
/// class, not "no leak anywhere."
pub static LIVE_PIXEL_BUFFERS: AtomicI64 = AtomicI64::new(0);

/// Snapshot of [`LIVE_PIXEL_BUFFERS`]. `Some` only on macOS -- reporting a
/// static zero on a platform where the counter is never wired up would look
/// like "definitely no live buffers" rather than the truth ("not tracked on
/// this platform"), which is exactly the plausible-looking-fake-data shape
/// CLAUDE.md's data-honesty rule forbids.
#[cfg(target_os = "macos")]
pub fn live_pixel_buffer_count() -> Option<u32> {
    Some(LIVE_PIXEL_BUFFERS.load(Ordering::Relaxed).max(0) as u32)
}

#[cfg(not(target_os = "macos"))]
pub fn live_pixel_buffer_count() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn process_footprint_bytes_is_nonzero_and_not_absurd_for_the_test_process() {
        let bytes = process_footprint_bytes().expect(
            "task_info(TASK_VM_INFO) must succeed for this process's own mach_task_self()",
        );
        assert!(bytes > 0, "a running process must have nonzero footprint");
        // Sanity ceiling, not a real limit -- catches a garbage/uninitialized
        // read (e.g. an offset bug) without pretending to know the real
        // upper bound of a healthy test process's memory use.
        assert!(
            bytes < 50 * 1024 * 1024 * 1024,
            "phys_footprint={bytes} bytes is implausibly large for a test process"
        );
    }

    #[test]
    fn throttle_returns_cached_value_and_does_not_reinvoke_the_probe_within_the_window() {
        let cache: Mutex<Option<(Instant, Option<u64>)>> = Mutex::new(None);
        let calls = std::cell::Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            Some(42)
        };
        let interval = Duration::from_secs(5);
        let t0 = Instant::now();

        let first = throttled_read(&cache, t0, interval, probe);
        assert_eq!(first, Some(42));
        assert_eq!(calls.get(), 1, "first call must invoke the probe");

        let still_inside = t0 + Duration::from_secs(1);
        let second = throttled_read(&cache, still_inside, interval, probe);
        assert_eq!(second, Some(42), "cached value must be returned unchanged");
        assert_eq!(
            calls.get(),
            1,
            "a call within the throttle window must NOT re-invoke the probe"
        );
    }

    #[test]
    fn throttle_reinvokes_the_probe_once_the_interval_has_elapsed() {
        let cache: Mutex<Option<(Instant, Option<u64>)>> = Mutex::new(None);
        let calls = std::cell::Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            Some(calls.get() as u64)
        };
        let interval = Duration::from_secs(5);
        let t0 = Instant::now();

        let first = throttled_read(&cache, t0, interval, probe);
        assert_eq!(first, Some(1));

        let after_window = t0 + Duration::from_secs(6);
        let second = throttled_read(&cache, after_window, interval, probe);
        assert_eq!(
            second,
            Some(2),
            "once the interval elapses the probe must run again"
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn live_pixel_buffer_count_platform_gate() {
        // No FFI needed here -- just documents/enforces the platform gate:
        // Some(_) only where the counter is actually wired up.
        #[cfg(target_os = "macos")]
        assert!(live_pixel_buffer_count().is_some());
        #[cfg(not(target_os = "macos"))]
        assert!(live_pixel_buffer_count().is_none());
    }
}
