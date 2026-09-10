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

    /// Mirrors XNU's `vm_region_submap_info_64` (`osfmk/mach/vm_region.h`)
    /// truncated at the V1 revision -- through `pages_reusable`, leaving out
    /// only V2's `object_id_full`, which nothing here reads. Same
    /// truncate-and-request-exactly-that pattern as `TaskVmInfo` above.
    ///
    /// `#[repr(C, packed(4))]` is LOAD-BEARING, not decoration: the header
    /// wraps these structs in `#pragma pack(push, 4)`, so `offset` (a `u64`)
    /// sits at byte 12, not 16. Natural alignment would shift every field
    /// after it by four bytes and read `pages_resident` out of `user_tag`'s
    /// slot -- a silent wrong-answer, not a crash. Verified field-by-field
    /// against the SDK header with `offsetof` (see the test below, which
    /// re-asserts the two sizes the kernel actually contracts on).
    #[repr(C, packed(4))]
    #[derive(Default, Clone, Copy)]
    struct VmRegionSubmapInfo64 {
        protection: i32,
        max_protection: i32,
        inheritance: u32,
        offset: u64,
        user_tag: u32,
        pages_resident: u32,
        pages_shared_now_private: u32,
        pages_swapped_out: u32,
        pages_dirtied: u32,
        ref_count: u32,
        shadow_depth: u16,
        external_pager: u8,
        share_mode: u8,
        is_submap: i32,
        behavior: i32,
        object_id: u32,
        user_wired_count: u16,
        flags: u16,
        pages_reusable: u32,
    }

    /// `VM_REGION_SUBMAP_INFO_V1_COUNT_64` -- 17 `natural_t` on every arch
    /// (the struct is explicitly 4-byte packed, so this does not vary).
    const VM_REGION_SUBMAP_INFO_V1_COUNT_64: u32 =
        (std::mem::size_of::<VmRegionSubmapInfo64>() / std::mem::size_of::<u32>()) as u32;

    extern "C" {
        fn mach_vm_region_recurse(
            target_task: u32,
            address: *mut u64,
            size: *mut u64,
            nesting_depth: *mut u32,
            info: *mut u32,
            info_count: *mut u32,
        ) -> i32;
    }

    /// Bytes per page AS THE KERNEL COUNTS THEM IN `vm_region` RESULTS.
    ///
    /// Trap, measured rather than reasoned about (#106): `pages_resident` /
    /// `pages_dirtied` are in units of the PROCESS's page size, which is what
    /// `sysconf(_SC_PAGESIZE)` reports -- not `vm_kernel_page_size`. The two
    /// differ only under Rosetta, where an x86_64 slice on Apple silicon sees
    /// `vm_page_size=4096` while `vm_kernel_page_size=16384`. Multiplying by
    /// the kernel value there reported 800 MB for a 200 MB allocation, a
    /// silent 4x overstatement in exactly the direction that would make this
    /// diagnostic lie about a memory spike.
    fn page_size_bytes() -> u64 {
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 {
            size as u64
        } else {
            4096
        }
    }

    /// Walk this task's own VM regions once and bucket them by `user_tag`.
    ///
    /// Submaps (the dyld shared region is the only one that matters) are
    /// stepped OVER, not descended into. Descending double-counts: the shared
    /// cache's contents reported ~2.2 GB resident in a hello-world process,
    /// which would swamp exactly the signal this exists to find. Their
    /// contents are shared, clean and never the thing that grew.
    pub fn vm_attribution() -> Option<super::VmAttribution> {
        let page = page_size_bytes();
        let mut totals = [(0u64, 0u64); 14];
        let mut other_by_tag = std::collections::BTreeMap::<u32, u64>::new();
        let mut address: u64 = 0;
        let mut regions: u32 = 0;
        let mut truncated = false;
        let started = std::time::Instant::now();
        loop {
            if regions >= super::VM_REGION_WALK_LIMIT {
                truncated = true;
                break;
            }
            // Cheap: one `Instant::now()` per 256 regions, not per region.
            if regions > 0
                && regions % super::VM_REGION_WALK_DEADLINE_CHECK_INTERVAL == 0
                && started.elapsed() >= super::VM_REGION_WALK_DEADLINE
            {
                truncated = true;
                break;
            }
            let mut info = VmRegionSubmapInfo64::default();
            let mut size: u64 = 0;
            // Reset per call: this is an IN/OUT parameter, and a stale depth
            // from a previous iteration is how the descend-into-submaps
            // variant of this loop spins forever.
            let mut depth: u32 = 0;
            let mut count: u32 = VM_REGION_SUBMAP_INFO_V1_COUNT_64;
            let status = unsafe {
                mach_vm_region_recurse(
                    mach_task_self_,
                    &mut address,
                    &mut size,
                    &mut depth,
                    &mut info as *mut VmRegionSubmapInfo64 as *mut u32,
                    &mut count,
                )
            };
            if status != KERN_SUCCESS {
                // KERN_INVALID_ADDRESS is the normal end of the address space,
                // reached on every successful walk -- not an error worth a log
                // line. Anything else ends the walk with what was gathered so
                // far, which `truncated` does not claim to be complete.
                break;
            }
            if count < VM_REGION_SUBMAP_INFO_V1_COUNT_64 {
                return None;
            }
            // `size` of 0 would make the address cursor stand still; treat it
            // as the end rather than looping to the region cap.
            if size == 0 {
                break;
            }
            let Some(next) = address.checked_add(size) else {
                break;
            };
            if info.is_submap != 0 {
                address = next;
                continue;
            }
            regions += 1;
            let user_tag = info.user_tag;
            let resident = u64::from(info.pages_resident) * page;
            let dirty =
                (u64::from(info.pages_dirtied) + u64::from(info.pages_swapped_out)) * page;
            let owner = super::VmOwner::from_user_tag(user_tag);
            let slot = &mut totals[owner.index()];
            slot.0 += resident;
            slot.1 += dirty;
            if owner == super::VmOwner::Other {
                *other_by_tag.entry(user_tag).or_insert(0) += dirty;
            }
            address = next;
        }
        if regions == 0 {
            return None;
        }
        let other_top_user_tag = other_by_tag
            .into_iter()
            .max_by_key(|(tag, dirty)| (*dirty, std::cmp::Reverse(*tag)))
            .map(|(tag, _)| tag);
        Some(super::build_vm_attribution(
            totals,
            regions,
            truncated,
            other_top_user_tag,
        ))
    }

    #[cfg(test)]
    mod vm_layout_tests {
        use super::*;

        #[test]
        fn submap_info_layout_matches_the_sdk_header() {
            // Both numbers come from a C program compiled against
            // <mach/vm_region.h> on this SDK: VM_REGION_SUBMAP_INFO_V1_SIZE
            // is 68 and VM_REGION_SUBMAP_INFO_V1_COUNT_64 is 17. If a future
            // edit drops `packed(4)` the size becomes 72 and this fails --
            // which is the point, because the kernel would otherwise happily
            // fill 17 words into a differently-shaped struct.
            assert_eq!(std::mem::size_of::<VmRegionSubmapInfo64>(), 68);
            assert_eq!(VM_REGION_SUBMAP_INFO_V1_COUNT_64, 17);
        }
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

/// #106: the VM-region walk, where it exists. macOS reads the real thing;
/// every other platform reports honest absence rather than an empty
/// attribution, which would read as "nothing is allocated" instead of "not
/// measured here" (`platform::mem`'s house rule, same as
/// `live_pixel_buffer_count`).
#[cfg(target_os = "macos")]
pub fn vm_attribution() -> Option<VmAttribution> {
    macos::vm_attribution()
}

#[cfg(not(target_os = "macos"))]
pub fn vm_attribution() -> Option<VmAttribution> {
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

/// Unconditional read that also REFRESHES the throttle cache.
///
/// #106: every `mem=` value in `capture-diag` comes from
/// `process_footprint_bytes_throttled`, so it can be up to
/// `FOOTPRINT_THROTTLE_INTERVAL` (5s) old. Reading a field log naively puts
/// each step of a memory climb up to 5s later than it happened -- in the
/// #106 capture the 1492MB value printed beside `start_share ... first
/// frame` had actually been sampled ~4s BEFORE that share started. Callers
/// marking a specific moment (share start, first frame, publish) must use
/// this instead, and because it writes the cache the next throttled reader
/// within the window reports this fresh value rather than an older one.
pub fn process_footprint_bytes_now() -> Option<u64> {
    forced_read(footprint_cache(), Instant::now(), process_footprint_bytes)
}

/// Read `probe` unconditionally and store it as the throttle cache's newest
/// entry. Split out from the `pub` entry point above for the same reason
/// `throttled_read` is: so tests drive a private cache and a call-counting
/// probe instead of the process-wide static.
fn forced_read(
    cache: &Mutex<Option<(Instant, Option<u64>)>>,
    now: Instant,
    probe: impl FnOnce() -> Option<u64>,
) -> Option<u64> {
    let value = probe();
    let mut guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some((now, value));
    value
}

/// Throttled `process_footprint_bytes()`. `capture-diag` (`session/
/// share.rs`) reads this roughly once per second; the underlying syscall is
/// cheap but there is no reason to pay it more than once per
/// `FOOTPRINT_THROTTLE_INTERVAL` (5s) -- this caches the last reading behind
/// a timestamp check so repeated calls inside the window are a plain load,
/// not a fresh `task_info`/`GetProcessMemoryInfo` call.
///
/// The cost is temporal aliasing: a value read here is anywhere from 0 to 5s
/// old, so it CANNOT date a step in a memory curve. Use
/// `process_footprint_bytes_now` for anything that has to say when (#106).
pub fn process_footprint_bytes_throttled() -> Option<u64> {
    throttled_read(
        footprint_cache(),
        Instant::now(),
        FOOTPRINT_THROTTLE_INTERVAL,
        process_footprint_bytes,
    )
}

/// Bytes one NV12 (`420v`) frame of `width`x`height` occupies: a full-size
/// luma plane plus a half-resolution interleaved chroma plane, each
/// dimension rounded up so an odd dimension still forms whole chroma blocks.
/// Shared by every pool below because the capture, publish-copy and SCK
/// queue paths all hold 4:2:0 frames of the same shape (I420's three planes
/// total the same bytes as NV12's two).
pub fn nv12_frame_bytes(width: u32, height: u32) -> u64 {
    let luma = u64::from(width) * u64::from(height);
    let chroma = u64::from(width.div_ceil(2)) * 2 * u64::from(height.div_ceil(2));
    luma + chroma
}

/// The most bytes Petal's OWN frame buffers can hold for one share at a
/// given source resolution (#106).
///
/// Deliberately a ceiling, not a measurement: each pool is a fixed frame
/// COUNT, so multiplying by the frame size is the largest it can ever be.
/// Logged at share start so a field log carries the number instead of
/// requiring someone to re-derive it from three constants in two modules --
/// and so a spike far above this total is visibly NOT Petal's own pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePoolCeiling {
    /// `capture::CAPTURE_BUFFER_POOL_LIMIT` NV12 plane-copy buffers.
    pub capture_copy_bytes: u64,
    /// `transport::publisher::I420_BUFFER_POOL_LIMIT` I420 publish buffers.
    pub i420_publish_bytes: u64,
    /// `capture::CAPTURE_QUEUE_DEPTH` IOSurfaces SCK may hand us at once.
    pub sck_queue_bytes: u64,
    pub total_bytes: u64,
}

impl FramePoolCeiling {
    pub fn total_mb(&self) -> u64 {
        self.total_bytes / (1024 * 1024)
    }
}

/// Compute [`FramePoolCeiling`]. Frame counts are passed in rather than
/// re-declared here so the constants keep exactly one definition each in the
/// modules that own them (`capture`, `transport::publisher`) -- two notions
/// of "how deep is the pool" is how they drift apart.
pub fn frame_pool_ceiling(
    width: u32,
    height: u32,
    capture_copy_frames: u32,
    i420_publish_frames: u32,
    sck_queue_frames: u32,
) -> FramePoolCeiling {
    let frame = nv12_frame_bytes(width, height);
    let capture_copy_bytes = frame * u64::from(capture_copy_frames);
    let i420_publish_bytes = frame * u64::from(i420_publish_frames);
    let sck_queue_bytes = frame * u64::from(sck_queue_frames);
    FramePoolCeiling {
        capture_copy_bytes,
        i420_publish_bytes,
        sck_queue_bytes,
        total_bytes: capture_copy_bytes + i420_publish_bytes + sck_queue_bytes,
    }
}

// ---------------------------------------------------------------------------
// #106: virtual-memory attribution by allocation owner
// ---------------------------------------------------------------------------

/// Which framework owns a run of pages, decoded from the kernel's per-region
/// `user_tag` (`VM_MEMORY_*` in `<mach/vm_statistics.h>`).
///
/// #106 needs a NAME for the ~1 GB step a display share adds to
/// `phys_footprint`. Petal's own pools are ruled out by arithmetic
/// (`frame_pool_ceiling` above -- ~47 MB at 2560x1440) and the receiver pool
/// by `live_pixel_buffers` reading 0 across the climb, so the owner is
/// downstream of this codebase. These buckets are chosen to separate exactly
/// the candidates that issue names: ScreenCaptureKit and VideoToolbox trade in
/// IOSurfaces (`IoSurface`) backed by IOKit/GPU mappings (`IoKit`,
/// `IoAccelerator`), CoreMedia pools have their own tags (`CoreMedia`), and
/// libwebrtc allocates from the C heap (`Malloc`).
///
/// Deliberately a CLOSED set, not the raw tag: a value from here crosses the
/// Sentry boundary as `memory_top_owner` (see `logging::MemoryOwnerTag`), so
/// it must be a bounded enum and never free text or a path. `proc_regionfilename`
/// would name the mapped file for a region and is NOT used here for that
/// reason -- it returns user paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VmOwner {
    /// `user_tag` 0: anonymous mappings and everything the kernel does not
    /// attribute, including the read-only dyld shared cache. Large and mostly
    /// CLEAN -- see `VmAttribution`'s note on resident vs dirty.
    Untagged,
    /// The C heap (`VM_MEMORY_MALLOC*`, `TCMALLOC`, `DYLD_MALLOC`). Rust's
    /// global allocator lands here too, as does libwebrtc's.
    Malloc,
    Stack,
    /// Mapped code and the shared/unshared pmap regions.
    Dylib,
    /// `VM_MEMORY_IOSURFACE`. The currency of every zero-copy frame on macOS:
    /// ScreenCaptureKit output, VideoToolbox encoder/decoder surfaces,
    /// CoreAnimation-fed display layers.
    IoSurface,
    /// `VM_MEMORY_IOKIT` -- driver mappings other than IOSurface proper.
    IoKit,
    /// `VM_MEMORY_IOACCELERATOR` -- GPU driver allocations.
    IoAccelerator,
    CoreGraphics,
    /// `VM_MEMORY_LAYERKIT` -- CoreAnimation.
    CoreAnimation,
    /// CoreMedia's pools and bitstream buffers -- the VideoToolbox side that
    /// is NOT an IOSurface.
    CoreMedia,
    /// JavaScriptCore / the webview's JIT arenas.
    JavaScript,
    Network,
    Audio,
    /// Any tag with no bucket of its own. `VmAttribution::other_top_user_tag`
    /// carries the raw numeric tag that dominates this bucket so a surprise is
    /// still actionable without shipping free text.
    Other,
}

impl VmOwner {
    /// Every variant, in declaration order. Used to build a fixed-size
    /// accumulator (no allocation during the walk) and to pin the
    /// `logging::MemoryOwnerTag` mirror in tests.
    pub const ALL: [VmOwner; 14] = [
        VmOwner::Untagged,
        VmOwner::Malloc,
        VmOwner::Stack,
        VmOwner::Dylib,
        VmOwner::IoSurface,
        VmOwner::IoKit,
        VmOwner::IoAccelerator,
        VmOwner::CoreGraphics,
        VmOwner::CoreAnimation,
        VmOwner::CoreMedia,
        VmOwner::JavaScript,
        VmOwner::Network,
        VmOwner::Audio,
        VmOwner::Other,
    ];

    /// Stable wire/log spelling. `logging::MemoryOwnerTag` mirrors these
    /// exactly (there is no shared source because `platform::mem` must not
    /// depend on `logging`'s Sentry machinery, the same split
    /// `BrowserUrlExtractionCauseTag` documents); a test asserts the two
    /// agree string-for-string.
    pub const fn tag(self) -> &'static str {
        match self {
            VmOwner::Untagged => "untagged",
            VmOwner::Malloc => "malloc",
            VmOwner::Stack => "stack",
            VmOwner::Dylib => "dylib",
            VmOwner::IoSurface => "iosurface",
            VmOwner::IoKit => "iokit",
            VmOwner::IoAccelerator => "ioaccelerator",
            VmOwner::CoreGraphics => "coregraphics",
            VmOwner::CoreAnimation => "coreanimation",
            VmOwner::CoreMedia => "coremedia",
            VmOwner::JavaScript => "javascript",
            VmOwner::Network => "network",
            VmOwner::Audio => "audio",
            VmOwner::Other => "other",
        }
    }

    // Used by `build_vm_attribution` and by this module's tests on every
    // platform; the walk that feeds it only exists on macOS, so a Windows
    // build sees no non-test caller (#106).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn index(self) -> usize {
        match self {
            VmOwner::Untagged => 0,
            VmOwner::Malloc => 1,
            VmOwner::Stack => 2,
            VmOwner::Dylib => 3,
            VmOwner::IoSurface => 4,
            VmOwner::IoKit => 5,
            VmOwner::IoAccelerator => 6,
            VmOwner::CoreGraphics => 7,
            VmOwner::CoreAnimation => 8,
            VmOwner::CoreMedia => 9,
            VmOwner::JavaScript => 10,
            VmOwner::Network => 11,
            VmOwner::Audio => 12,
            VmOwner::Other => 13,
        }
    }

    /// Decode a kernel `user_tag`. Values are the `VM_MEMORY_*` constants from
    /// `<mach/vm_statistics.h>`; they are ABI, not private -- `vmmap` prints
    /// the same numbers. Kept `const fn` and total (every one of the 256
    /// possible tags maps to a variant) so a tag Apple adds later degrades to
    /// `Other` rather than being dropped.
    pub const fn from_user_tag(user_tag: u32) -> Self {
        match user_tag {
            0 => VmOwner::Untagged,
            // VM_MEMORY_MALLOC(1)..MALLOC_PROB_GUARD(13), TCMALLOC(53),
            // DYLD_MALLOC(61).
            1..=13 | 53 | 61 => VmOwner::Malloc,
            // VM_MEMORY_IOKIT.
            21 => VmOwner::IoKit,
            // VM_MEMORY_STACK(30), VM_MEMORY_GUARD(31).
            30 | 31 => VmOwner::Stack,
            // SHARED_PMAP(32), DYLIB(33), OBJC_DISPATCHERS(34),
            // UNSHARED_PMAP(35), DYLD(60).
            32..=35 | 60 => VmOwner::Dylib,
            // COREGRAPHICS(42), CGIMAGE(52), COREGRAPHICS_DATA(54)..XALLOC(58).
            42 | 52 | 54..=58 => VmOwner::CoreGraphics,
            // VM_MEMORY_LAYERKIT.
            51 => VmOwner::CoreAnimation,
            // JAVASCRIPT_CORE(63), JIT_EXECUTABLE_ALLOCATOR(64),
            // JIT_REGISTER_FILE(65).
            63..=65 => VmOwner::JavaScript,
            // SKYWALK(87), LIBNETWORK(89).
            87 | 89 => VmOwner::Network,
            // VM_MEMORY_IOSURFACE.
            88 => VmOwner::IoSurface,
            // VM_MEMORY_AUDIO.
            90 => VmOwner::Audio,
            // VIDEOBITSTREAM(91), CM_XPC(92), CM_RPC(93), CM_MEMORYPOOL(94),
            // CM_READCACHE(95), CM_CRABS(96), CM_REGWARP(101), CM_HLS(106).
            91..=96 | 101 | 106 => VmOwner::CoreMedia,
            // VM_MEMORY_IOACCELERATOR.
            100 => VmOwner::IoAccelerator,
            _ => VmOwner::Other,
        }
    }
}

/// One owner's share of the address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmOwnerBytes {
    pub owner: VmOwner,
    /// Pages currently in RAM. Includes CLEAN shared mappings (the dyld shared
    /// cache alone is ~110 MB under `Untagged`), so this is NOT comparable to
    /// `phys_footprint` -- use it to see whether a bucket is file-backed.
    pub resident_bytes: u64,
    /// Dirtied + swapped-out pages. THIS is the footprint-relevant number:
    /// measured on this machine, the sum of this column across all owners
    /// equalled `phys_footprint` exactly (284 MB vs 284 MB with 20 x 2560x1440
    /// BGRA IOSurfaces held live). It is what makes the decomposition
    /// answer #106's question rather than merely resemble it.
    pub dirty_bytes: u64,
}

/// Hard cap on regions walked in one sample. A real Petal process maps a few
/// thousand regions; this is an upper bound so a pathological address space
/// cannot turn a diagnostic into a stall. `truncated` says the cap was hit, so
/// a partial reading is never mistaken for a complete one.
pub const VM_REGION_WALK_LIMIT: u32 = 16_384;

/// Wall-clock ceiling on one walk, checked every
/// `VM_REGION_WALK_DEADLINE_CHECK_INTERVAL` regions. The region cap alone is
/// not a time bound: one `mach_vm_region_recurse` measured ~13 us here, so
/// 16,384 of them is a fifth of a second, and `start_begin` sits in the user's
/// click path. Whichever bound trips first ends the walk and sets `truncated`.
pub const VM_REGION_WALK_DEADLINE: Duration = Duration::from_millis(25);
/// Read only by the macOS walker (and by the bounds test everywhere).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const VM_REGION_WALK_DEADLINE_CHECK_INTERVAL: u32 = 256;

/// How many owners the log line names. Four is enough to show the dominant
/// bucket plus context without turning one line into a table.
pub const VM_ATTRIBUTION_TOP_N: usize = 4;

/// A single bounded walk of this process's own VM regions, bucketed by owner
/// (#106).
///
/// ## What it is for
///
/// `phys_footprint` says HOW MUCH; this says WHO. Sampled only at share
/// lifecycle marks (`start_begin`, `first_frame`, `publish_succeeded`,
/// `settle_30s`) and when OS memory pressure transitions -- never per frame
/// and never per second. Measured in this repo's own test binary: 46 regions
/// in 0.6-2.0 ms, i.e. ~13 us per region, and both `VM_REGION_WALK_LIMIT` and
/// `VM_REGION_WALK_DEADLINE` bound the worst case regardless of how many
/// regions a real process maps.
///
/// ## Honest limits
///
/// - It attributes pages mapped into THIS process. A framework that parks
///   bytes in another process (WindowServer, `replayd`) is invisible to it,
///   and a spike that does not move any bucket here is evidence FOR that.
/// - `Untagged` is a genuine bucket, not a failure: anonymous `mmap` and the
///   shared cache both live there. A climb in `Untagged` narrows the owner
///   without naming it.
/// - Region-level `user_tag` is set by whoever mapped the memory. It names the
///   allocating FRAMEWORK, not the feature -- `IoSurface` growing says
///   "IOSurfaces", not "ScreenCaptureKit specifically".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmAttribution {
    /// Owners with any nonzero bytes, sorted by `dirty_bytes` descending
    /// (ties broken by `resident_bytes`, then by owner order for determinism).
    pub owners: Vec<VmOwnerBytes>,
    pub regions_walked: u32,
    /// True when `VM_REGION_WALK_LIMIT` was reached -- the numbers are a floor.
    pub truncated: bool,
    pub total_resident_bytes: u64,
    pub total_dirty_bytes: u64,
    /// The raw `VM_MEMORY_*` tag with the most dirty bytes among regions that
    /// fell into `VmOwner::Other`, when that bucket has any. A number in
    /// `0..=255`, so it is safe to ship, and it turns a surprising `other`
    /// into a one-line lookup in `<mach/vm_statistics.h>`.
    pub other_top_user_tag: Option<u32>,
}

impl VmAttribution {
    /// The owner holding the most dirty bytes, or `None` when nothing is
    /// dirty. This is the value that crosses the Sentry boundary.
    pub fn top_owner(&self) -> Option<VmOwner> {
        self.owners
            .iter()
            .find(|entry| entry.dirty_bytes > 0)
            .map(|entry| entry.owner)
    }
}

/// Fold raw per-owner byte pairs into a sorted [`VmAttribution`]. Split out
/// from the syscall loop so the ordering, filtering and `other_top_user_tag`
/// rules are testable on every platform rather than only where the walk runs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn build_vm_attribution(
    totals: [(u64, u64); 14],
    regions_walked: u32,
    truncated: bool,
    other_top_user_tag: Option<u32>,
) -> VmAttribution {
    let mut owners: Vec<VmOwnerBytes> = VmOwner::ALL
        .iter()
        .map(|owner| VmOwnerBytes {
            owner: *owner,
            resident_bytes: totals[owner.index()].0,
            dirty_bytes: totals[owner.index()].1,
        })
        .filter(|entry| entry.resident_bytes > 0 || entry.dirty_bytes > 0)
        .collect();
    // Dirty first: it is the column that sums to `phys_footprint`, so ranking
    // by resident would put the (clean, shared, never-growing) dyld cache above
    // the bucket that actually moved.
    owners.sort_by(|a, b| {
        b.dirty_bytes
            .cmp(&a.dirty_bytes)
            .then(b.resident_bytes.cmp(&a.resident_bytes))
            .then(a.owner.cmp(&b.owner))
    });
    let total_resident_bytes = owners.iter().map(|entry| entry.resident_bytes).sum();
    let total_dirty_bytes = owners.iter().map(|entry| entry.dirty_bytes).sum();
    let other_top_user_tag = owners
        .iter()
        .any(|entry| entry.owner == VmOwner::Other)
        .then_some(other_top_user_tag)
        .flatten();
    VmAttribution {
        owners,
        regions_walked,
        truncated,
        total_resident_bytes,
        total_dirty_bytes,
        other_top_user_tag,
    }
}

/// Render [`VmAttribution`] as log fields. Pure, so a field log's shape is
/// asserted rather than assumed (#106's own complaint was that the one metric
/// which would have attributed the spike never reached a log).
///
/// `None` renders `vm_walk=unavailable` with no numbers at all -- on Windows,
/// and on a failed read, absence must not read as "nothing is allocated".
pub fn vm_attribution_fields(attribution: Option<&VmAttribution>) -> String {
    let Some(attribution) = attribution else {
        return "vm_walk=unavailable vm_regions=n/a vm_resident_mb=n/a vm_dirty_mb=n/a \
                vm_top=n/a vm_other_tag=n/a"
            .to_string();
    };
    let mb = |bytes: u64| bytes / (1024 * 1024);
    let walk = if attribution.truncated {
        "truncated"
    } else {
        "complete"
    };
    let top = if attribution.owners.is_empty() {
        "none".to_string()
    } else {
        attribution
            .owners
            .iter()
            .take(VM_ATTRIBUTION_TOP_N)
            .map(|entry| {
                format!(
                    "{}:{}/{}",
                    entry.owner.tag(),
                    mb(entry.resident_bytes),
                    mb(entry.dirty_bytes)
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let other_tag = attribution
        .other_top_user_tag
        .map(|tag| tag.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    format!(
        "vm_walk={walk} vm_regions={} vm_resident_mb={} vm_dirty_mb={} vm_top={top} \
         vm_other_tag={other_tag}",
        attribution.regions_walked,
        mb(attribution.total_resident_bytes),
        mb(attribution.total_dirty_bytes)
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
    fn forced_read_reinvokes_the_probe_inside_the_throttle_window_and_refreshes_the_cache() {
        // #106: a lifecycle mark must report NOW, and must leave the cache
        // holding NOW so the next throttled `capture-diag` line does not
        // print a value older than the mark it follows.
        let cache: Mutex<Option<(Instant, Option<u64>)>> = Mutex::new(None);
        let interval = Duration::from_secs(5);
        let t0 = Instant::now();

        assert_eq!(throttled_read(&cache, t0, interval, || Some(100)), Some(100));

        let one_second_later = t0 + Duration::from_secs(1);
        assert_eq!(
            throttled_read(&cache, one_second_later, interval, || Some(900)),
            Some(100),
            "throttled read inside the window must still return the stale value"
        );
        assert_eq!(
            forced_read(&cache, one_second_later, || Some(900)),
            Some(900),
            "a forced read inside the window must re-invoke the probe"
        );
        assert_eq!(
            throttled_read(&cache, t0 + Duration::from_secs(2), interval, || Some(
                7_777
            )),
            Some(900),
            "the forced read must have refreshed the cache with its own value"
        );
    }

    #[test]
    fn nv12_frame_bytes_matches_the_three_halves_shape_and_rounds_odd_dimensions_up() {
        assert_eq!(nv12_frame_bytes(2560, 1440), 5_529_600);
        assert_eq!(nv12_frame_bytes(1920, 1080), 3_110_400);
        // Odd dimensions round each chroma dimension up rather than
        // truncating a partial block away.
        assert_eq!(nv12_frame_bytes(3, 3), 9 + 4 * 2);
        assert_eq!(nv12_frame_bytes(0, 0), 0);
    }

    #[test]
    fn frame_pool_ceiling_scales_with_pixels_and_stays_far_under_the_106_step() {
        // #106's field capture stepped ~1064 MB at once while sharing a
        // 2560x1440 display. Petal's own pools are fixed FRAME COUNTS (3/3/3),
        // so their ceiling at that resolution is ~47 MB -- under 5% of one
        // step. This test is the arithmetic half of that claim; if a pool
        // limit is ever raised, this fails and the claim gets re-checked.
        let ceiling = frame_pool_ceiling(2560, 1440, 3, 3, 3);
        assert_eq!(ceiling.capture_copy_bytes, 3 * 5_529_600);
        assert_eq!(ceiling.i420_publish_bytes, 3 * 5_529_600);
        assert_eq!(ceiling.sck_queue_bytes, 3 * 5_529_600);
        assert_eq!(ceiling.total_bytes, 9 * 5_529_600);
        assert_eq!(ceiling.total_mb(), 47);
        assert!(
            ceiling.total_bytes < 64 * 1024 * 1024,
            "2560x1440 pool ceiling {} bytes is no longer a small fraction of a 1 GB step",
            ceiling.total_bytes
        );

        // 5K, the largest single display Petal can be pointed at today: still
        // bounded, and exactly linear in pixel count rather than growing by
        // some implicit multiple of the source resolution.
        let five_k = frame_pool_ceiling(5120, 2880, 3, 3, 3);
        assert_eq!(five_k.total_bytes, 4 * ceiling.total_bytes);
        assert!(five_k.total_bytes < 256 * 1024 * 1024);

        // Both directions: a deeper pool must report MORE, so the ceiling
        // cannot silently stay flat if someone raises a limit.
        assert!(
            frame_pool_ceiling(2560, 1440, 8, 3, 3).total_bytes > ceiling.total_bytes,
            "raising a pool limit must raise the reported ceiling"
        );
        assert_eq!(frame_pool_ceiling(2560, 1440, 0, 0, 0).total_bytes, 0);
    }

    #[test]
    fn vm_owner_decodes_the_tags_that_separate_the_106_candidates() {
        // The four that matter for #106: SCK/VideoToolbox surfaces, the GPU
        // driver, CoreMedia's own pools, and the C heap libwebrtc allocates
        // from. Values are VM_MEMORY_* from <mach/vm_statistics.h>.
        assert_eq!(VmOwner::from_user_tag(88), VmOwner::IoSurface);
        assert_eq!(VmOwner::from_user_tag(21), VmOwner::IoKit);
        assert_eq!(VmOwner::from_user_tag(100), VmOwner::IoAccelerator);
        assert_eq!(VmOwner::from_user_tag(94), VmOwner::CoreMedia);
        assert_eq!(VmOwner::from_user_tag(91), VmOwner::CoreMedia);
        assert_eq!(VmOwner::from_user_tag(1), VmOwner::Malloc);
        assert_eq!(VmOwner::from_user_tag(3), VmOwner::Malloc);
        assert_eq!(VmOwner::from_user_tag(11), VmOwner::Malloc);
        assert_eq!(VmOwner::from_user_tag(0), VmOwner::Untagged);
        assert_eq!(VmOwner::from_user_tag(30), VmOwner::Stack);
        assert_eq!(VmOwner::from_user_tag(33), VmOwner::Dylib);
        assert_eq!(VmOwner::from_user_tag(42), VmOwner::CoreGraphics);
        assert_eq!(VmOwner::from_user_tag(51), VmOwner::CoreAnimation);
        assert_eq!(VmOwner::from_user_tag(63), VmOwner::JavaScript);
        assert_eq!(VmOwner::from_user_tag(89), VmOwner::Network);
        assert_eq!(VmOwner::from_user_tag(90), VmOwner::Audio);
        // A tag Apple has not assigned yet degrades to `Other`, never panics
        // and is never dropped from the accounting.
        assert_eq!(VmOwner::from_user_tag(200), VmOwner::Other);
        assert_eq!(VmOwner::from_user_tag(255), VmOwner::Other);
    }

    #[test]
    fn vm_owner_mapping_is_total_and_the_index_is_a_bijection() {
        // Every one of the 256 possible user tags maps somewhere -- the walk
        // must never silently discard bytes.
        for tag in 0u32..=255 {
            let owner = VmOwner::from_user_tag(tag);
            assert!(VmOwner::ALL.contains(&owner), "tag {tag} left the set");
        }
        let mut seen = [false; 14];
        for owner in VmOwner::ALL {
            let index = owner.index();
            assert!(!seen[index], "{owner:?} reuses index {index}");
            seen[index] = true;
        }
        assert!(seen.iter().all(|hit| *hit));
        let mut tags: Vec<&str> = VmOwner::ALL.iter().map(|owner| owner.tag()).collect();
        tags.sort_unstable();
        let count = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), count, "owner tag strings must be distinct");
    }

    fn synthetic_totals(pairs: &[(VmOwner, u64, u64)]) -> [(u64, u64); 14] {
        let mut totals = [(0u64, 0u64); 14];
        for (owner, resident, dirty) in pairs {
            totals[owner.index()] = (*resident, *dirty);
        }
        totals
    }

    #[test]
    fn vm_attribution_ranks_by_dirty_so_the_clean_shared_cache_cannot_lead() {
        // The dyld shared cache reads ~110 MB RESIDENT and 0 dirty in every
        // process. Ranking by resident would put it first on every line and
        // bury the bucket that actually grew.
        const MB: u64 = 1024 * 1024;
        let attribution = build_vm_attribution(
            synthetic_totals(&[
                (VmOwner::Untagged, 112 * MB, 0),
                (VmOwner::IoSurface, 281 * MB, 281 * MB),
                (VmOwner::Malloc, 40 * MB, 38 * MB),
            ]),
            78,
            false,
            None,
        );
        assert_eq!(
            attribution
                .owners
                .iter()
                .map(|entry| entry.owner)
                .collect::<Vec<_>>(),
            vec![VmOwner::IoSurface, VmOwner::Malloc, VmOwner::Untagged]
        );
        assert_eq!(attribution.top_owner(), Some(VmOwner::IoSurface));
        assert_eq!(attribution.total_resident_bytes, (112 + 281 + 40) * MB);
        assert_eq!(attribution.total_dirty_bytes, (281 + 38) * MB);
        // Owners with nothing at all are dropped rather than printed as zeros.
        assert!(attribution
            .owners
            .iter()
            .all(|entry| entry.owner != VmOwner::CoreMedia));
    }

    #[test]
    fn vm_attribution_reports_no_top_owner_when_nothing_is_dirty() {
        const MB: u64 = 1024 * 1024;
        let attribution = build_vm_attribution(
            synthetic_totals(&[(VmOwner::Untagged, 112 * MB, 0)]),
            9,
            false,
            None,
        );
        assert_eq!(attribution.top_owner(), None);
    }

    #[test]
    fn vm_attribution_keeps_the_raw_other_tag_only_when_other_has_bytes() {
        const MB: u64 = 1024 * 1024;
        let with_other = build_vm_attribution(
            synthetic_totals(&[(VmOwner::Other, 9 * MB, 9 * MB)]),
            4,
            false,
            Some(107),
        );
        assert_eq!(with_other.other_top_user_tag, Some(107));
        // A raw tag with no `Other` bytes behind it would be a number with no
        // meaning on the line.
        let without_other = build_vm_attribution(
            synthetic_totals(&[(VmOwner::Malloc, 9 * MB, 9 * MB)]),
            4,
            false,
            Some(107),
        );
        assert_eq!(without_other.other_top_user_tag, None);
    }

    #[test]
    fn vm_attribution_fields_render_the_top_owners_and_the_totals() {
        const MB: u64 = 1024 * 1024;
        let attribution = build_vm_attribution(
            synthetic_totals(&[
                (VmOwner::Untagged, 112 * MB, 1 * MB),
                (VmOwner::IoSurface, 281 * MB, 281 * MB),
                (VmOwner::Malloc, 40 * MB, 38 * MB),
                (VmOwner::CoreMedia, 12 * MB, 11 * MB),
                (VmOwner::Dylib, 2 * MB, 0),
            ]),
            78,
            false,
            None,
        );
        let line = vm_attribution_fields(Some(&attribution));
        assert_eq!(
            line,
            "vm_walk=complete vm_regions=78 vm_resident_mb=447 vm_dirty_mb=331 \
             vm_top=iosurface:281/281,malloc:40/38,coremedia:12/11,untagged:112/1 \
             vm_other_tag=n/a"
        );
        // VM_ATTRIBUTION_TOP_N caps the list -- the fifth owner is summarised
        // by the totals, not printed.
        assert!(!line.contains("dylib"));
    }

    #[test]
    fn vm_attribution_fields_say_unavailable_rather_than_inventing_zeros() {
        // Windows, and a failed macOS read, must not render as "0 MB
        // everywhere" -- that is a fabricated all-clear.
        let line = vm_attribution_fields(None);
        assert!(line.contains("vm_walk=unavailable"), "{line}");
        assert!(line.contains("vm_dirty_mb=n/a"), "{line}");
        assert!(line.contains("vm_top=n/a"), "{line}");
        assert!(!line.contains("_mb=0"), "{line}");
    }

    #[test]
    fn vm_attribution_fields_mark_a_capped_walk_as_truncated() {
        let attribution = build_vm_attribution(
            synthetic_totals(&[(VmOwner::Malloc, 1024 * 1024, 1024 * 1024)]),
            VM_REGION_WALK_LIMIT,
            true,
            None,
        );
        let line = vm_attribution_fields(Some(&attribution));
        assert!(line.contains("vm_walk=truncated"), "{line}");
        assert!(line.contains(&format!("vm_regions={VM_REGION_WALK_LIMIT}")), "{line}");
    }

    #[test]
    fn vm_walk_bounds_are_both_real_and_the_deadline_is_checked_often_enough() {
        // Two independent bounds, because the region cap alone is not a time
        // bound: at the ~13 us per region measured here, 16,384 regions is
        // ~213 ms, and `start_begin` runs in the user's click path.
        assert!(VM_REGION_WALK_LIMIT > 0);
        assert!(VM_REGION_WALK_DEADLINE > Duration::ZERO);
        // The deadline check must fire well before the region cap, or it is
        // decoration.
        assert!(VM_REGION_WALK_DEADLINE_CHECK_INTERVAL < VM_REGION_WALK_LIMIT);
        assert!(VM_REGION_WALK_DEADLINE < Duration::from_millis(100));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn vm_attribution_walks_this_process_and_its_dirty_total_tracks_phys_footprint() {
        // The claim this diagnostic rests on: the per-owner DIRTY column sums
        // to `phys_footprint`, the very metric #106 is written in. Measured
        // exactly equal on this machine in a controlled C harness; asserted
        // loosely here because a live test process allocates between the two
        // reads.
        let attribution =
            vm_attribution().expect("a running macOS process must have walkable VM regions");
        assert!(attribution.regions_walked > 0);
        assert!(
            !attribution.truncated,
            "a test process should not need {VM_REGION_WALK_LIMIT} regions"
        );
        assert!(
            attribution.total_dirty_bytes > 1024 * 1024,
            "a running process must have more than a megabyte dirty, got {}",
            attribution.total_dirty_bytes
        );
        assert!(attribution.total_resident_bytes >= attribution.total_dirty_bytes);
        let footprint = process_footprint_bytes().expect("phys_footprint must be readable");
        let ratio = attribution.total_dirty_bytes as f64 / footprint as f64;
        assert!(
            (0.5..=2.0).contains(&ratio),
            "walked dirty {} vs phys_footprint {footprint} (ratio {ratio:.2}) -- the \
             decomposition no longer tracks the number it claims to explain",
            attribution.total_dirty_bytes
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn vm_attribution_attributes_a_deliberate_allocation_to_the_malloc_owner() {
        // Red-then-green in one test: walk, dirty a known number of bytes,
        // walk again, and require the growth to land in the bucket that owns
        // it. Without this, "the walker returns plausible numbers" would be
        // the whole of the evidence -- and a mis-decoded `user_tag` returns
        // plausible numbers too.
        const CHUNK: usize = 128 * 1024 * 1024;
        let dirty_before = vm_attribution()
            .expect("walk must work")
            .owners
            .iter()
            .find(|entry| entry.owner == VmOwner::Malloc)
            .map(|entry| entry.dirty_bytes)
            .unwrap_or(0);
        let mut block = vec![0u8; CHUNK];
        // Touch every page: an untouched allocation is neither resident nor
        // dirty, so a walker could "pass" this test while reading nothing.
        for index in (0..CHUNK).step_by(4096) {
            block[index] = 0xA5;
        }
        let after = vm_attribution().expect("walk must work");
        let dirty_after = after
            .owners
            .iter()
            .find(|entry| entry.owner == VmOwner::Malloc)
            .map(|entry| entry.dirty_bytes)
            .expect("a 128 MiB touched allocation must give the malloc owner bytes");
        let growth = dirty_after.saturating_sub(dirty_before);
        assert!(
            growth >= 100 * 1024 * 1024,
            "malloc dirty grew only {growth} bytes after touching {CHUNK} -- \
             attribution is not tracking the allocation"
        );
        // Keep the block alive across the second walk; dropping it earlier
        // would let the allocator return the pages before they are counted.
        assert_eq!(block[0], 0xA5);
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
