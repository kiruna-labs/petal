//! Process file-descriptor accounting and headroom (#104).
//!
//! ## Why this exists
//!
//! A field log from a 29-hour meeting carried **38,062**
//! `network.cc: Socket creation failed : Too many open files` errors across
//! 19.5 hours. Nothing in Petal measured open descriptors, nothing raised the
//! soft limit, and nothing reacted when allocation started failing -- so a
//! condition that made the app useless for 20 of 29 meeting hours produced no
//! rising signal at all, only a binary cliff.
//!
//! This module is the missing gauge plus the missing headroom. It is
//! deliberately NOT a leak fix: see `descriptor-exhaustion` in #104 for the
//! accumulation evidence.
//!
//! ## macOS ships GUI apps a soft limit of 256
//!
//! `launchctl limit maxfiles` reports `256 unlimited` on a stock machine: an
//! app launched from Finder/Dock inherits soft `RLIMIT_NOFILE` = 256 while the
//! hard limit is effectively `kern.maxfilesperproc` (245,760 here). A Tauri
//! app with several webviews, ScreenCaptureKit streams, two LiveKit
//! PeerConnections and ICE over several interfaces sits close to 256 in
//! ordinary operation. Raising the soft limit toward the hard limit is
//! standard for a media app and costs nothing -- the kernel allocates
//! descriptor slots lazily.
//!
//! ## Counting: two syscalls, cheap enough to run forever
//!
//! The obvious idiom -- `proc_pidinfo(pid, PROC_PIDLISTFDS, 0, NULL, 0)`, which
//! returns a byte count without writing a listing -- reports the **capacity of
//! the process descriptor TABLE, not the number of open descriptors**. It is a
//! sizing hint. This was written that way first and the
//! `open_descriptor_count_tracks_real_descriptors` test caught it immediately:
//! opening 64 real files moved the reading `420 -> 420`, because the table
//! already had room. A gauge that cannot see 64 descriptors appear would have
//! shipped as a plausible-looking constant and told us nothing in the next
//! field log -- which is the exact failure #104 exists to end. Do not
//! "simplify" this back to the one-call form.
//!
//! So: size, then actually fetch the listing and count what the kernel wrote.
//! `proc_pidfdlist` copies out only OPEN entries and returns the bytes actually
//! written, so `written / size_of::<proc_fdinfo>()` is the real count. The cost
//! is one short-lived buffer (bounded by the soft limit: at most ~80 KB at this
//! module's ceiling) once per sample. The in-room sampler runs this once a
//! second for the life of a meeting, so "cheap" is a requirement, not a nicety
//! -- but it is nowhere near the cost of the LiveKit `get_stats` call already
//! on that same tick.
//!
//! Windows has no `RLIMIT_NOFILE`; `GetProcessHandleCount` is the closest
//! analogue and is reported instead (handles, not descriptors -- the log field
//! is the same so a field log from either platform reads the same way).

/// Never raise the soft limit above this. Far above anything Petal legitimately
/// needs (a healthy in-room process sits in the low hundreds), while staying
/// well under `kern.maxfilesperproc` so the `setrlimit` call is accepted, and
/// low enough that a runaway descriptor leak still terminates in a bounded time
/// instead of consuming a system-wide resource. Headroom, not a blank cheque.
// Windows has no `RLIMIT_NOFILE`, so the raise path and everything only it
// constructs are unreachable there. The lint stays fully active on macOS, where
// a genuinely dead helper would matter.
#[cfg_attr(not(unix), allow(dead_code))]
pub const SOFT_LIMIT_CEILING: u64 = 10_240;

/// A `getrlimit(RLIMIT_NOFILE)` reading. `hard == u64::MAX` means "no hard cap"
/// (`RLIM_INFINITY`), normalised here so the pure helpers never have to know
/// the platform's infinity sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorLimits {
    pub soft: u64,
    pub hard: u64,
}

impl DescriptorLimits {
    /// Render the hard limit for a log line without inventing a number for
    /// "unlimited".
    pub fn hard_display(&self) -> String {
        if self.hard == u64::MAX {
            "unlimited".to_string()
        } else {
            self.hard.to_string()
        }
    }
}

/// What `raise_soft_limit` actually did. Every variant is non-fatal: a process
/// that cannot raise its own limit must still start and run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum RaiseOutcome {
    /// The soft limit was raised from `from` to `to`.
    Raised { from: u64, to: u64 },
    /// The soft limit already met or exceeded everything we could ask for --
    /// either it is already at/above the ceiling, or the HARD limit is already
    /// that low and there is nothing to raise it to.
    AlreadyAmple { soft: u64 },
    /// Every `setrlimit` attempt was rejected. `attempted` is the highest value
    /// tried. The process keeps its existing soft limit.
    Failed { soft: u64, attempted: u64 },
    /// The reading itself failed, or this platform has no `RLIMIT_NOFILE`.
    Unsupported,
}

/// The ordered list of soft-limit values to try, highest first.
///
/// Pure so both awkward paths are testable without touching the process:
/// * **the hard limit is already low** -- the target is clamped to `hard`, and
///   when `soft == hard` the list is EMPTY (nothing to ask for), which the
///   caller reports as `AlreadyAmple` rather than as a failure;
/// * **`setrlimit` fails** -- macOS rejects a soft limit above
///   `kern.maxfilesperproc` with `EINVAL` even when the hard limit reads
///   `RLIM_INFINITY`, so a single attempt is not enough. Successive halvings
///   give a bounded retry that always terminates and never proposes a value at
///   or below the limit already in force (which would be a silent DOWNGRADE).
///
/// `hard == u64::MAX` means `RLIM_INFINITY`; the ceiling applies either way.
// Windows has no `RLIMIT_NOFILE`, so the raise path and everything only it
// constructs are unreachable there. The lint stays fully active on macOS, where
// a genuinely dead helper would matter.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn soft_limit_candidates(soft: u64, hard: u64) -> Vec<u64> {
    let target = hard.min(SOFT_LIMIT_CEILING);
    if soft >= target {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    let mut value = target;
    // Bounded by construction: `value` at least halves each pass and the loop
    // stops as soon as it would not beat the limit already in force.
    while value > soft && candidates.len() < 6 {
        candidates.push(value);
        value /= 2;
    }
    candidates
}

/// `true` when `open` has reached the high-water fraction of `soft`.
///
/// Integer math on purpose: this runs on every sample forever, and a float
/// comparison here would be the only float in the path. `soft == 0` is a
/// nonsense limit (never observed, but a `getrlimit` that returns one must not
/// make this divide or fire) and reads as "not elevated".
pub fn at_high_water(open: u64, soft: u64) -> bool {
    soft != 0 && open.saturating_mul(100) >= soft.saturating_mul(HIGH_WATER_PCT)
}

/// `true` when a live pressure episode has recovered far enough to end.
///
/// Deliberately LOWER than the high-water mark: without hysteresis a reading
/// oscillating either side of 80% would re-open an episode on every sample, and
/// the sustained 20-hour condition in #104 is exactly the shape that would do
/// it.
pub fn below_clear_water(open: u64, soft: u64) -> bool {
    soft == 0 || open.saturating_mul(100) < soft.saturating_mul(CLEAR_WATER_PCT)
}

const HIGH_WATER_PCT: u64 = 80;
const CLEAR_WATER_PCT: u64 = 70;

/// `true` when an `io::Error` is the descriptor table refusing to allocate.
///
/// `std::io::ErrorKind` has no stable variant for either condition, so this
/// matches the raw OS error: `EMFILE` (this process is at its own
/// `RLIMIT_NOFILE`) and `ENFILE` (the whole system table is full). The #104
/// field log shows Petal's own writes failing with `os error 24` = `EMFILE`.
pub fn is_descriptor_exhaustion(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(error.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE))
    }
    #[cfg(windows)]
    {
        // ERROR_TOO_MANY_OPEN_FILES.
        matches!(error.raw_os_error(), Some(4))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::{DescriptorLimits, RaiseOutcome};

    /// Normalise the platform's `RLIM_INFINITY` sentinel to `u64::MAX` so the
    /// pure helpers never see a magic number.
    fn normalise(raw: libc::rlim_t) -> u64 {
        if raw == libc::RLIM_INFINITY {
            u64::MAX
        } else {
            raw as u64
        }
    }

    pub fn descriptor_limits() -> Option<DescriptorLimits> {
        // SAFETY: `getrlimit` writes into a caller-owned `rlimit` we fully own
        // and have zero-initialised; no ownership is transferred either way.
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let status = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) };
        if status != 0 {
            log::warn!(
                "platform::fd: getrlimit(RLIMIT_NOFILE) failed: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(DescriptorLimits {
            soft: normalise(limits.rlim_cur),
            hard: normalise(limits.rlim_max),
        })
    }

    fn try_set_soft(value: u64, hard: libc::rlim_t) -> bool {
        let limits = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: hard,
        };
        // SAFETY: `setrlimit` reads a caller-owned, fully-initialised `rlimit`.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) == 0 }
    }

    pub fn raise_soft_limit() -> RaiseOutcome {
        let Some(before) = descriptor_limits() else {
            return RaiseOutcome::Unsupported;
        };
        let candidates = super::soft_limit_candidates(before.soft, before.hard);
        let Some(&highest) = candidates.first() else {
            return RaiseOutcome::AlreadyAmple { soft: before.soft };
        };
        // Preserve the existing hard limit verbatim: an unprivileged process
        // may lower it but never raise it, so echoing back what we read is the
        // only value guaranteed to be accepted.
        let raw_hard = if before.hard == u64::MAX {
            libc::RLIM_INFINITY
        } else {
            before.hard as libc::rlim_t
        };
        for candidate in &candidates {
            if try_set_soft(*candidate, raw_hard) {
                return RaiseOutcome::Raised {
                    from: before.soft,
                    to: *candidate,
                };
            }
        }
        RaiseOutcome::Failed {
            soft: before.soft,
            attempted: highest,
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_count {
    /// Slack entries added to the buffer beyond the kernel's sizing hint, so a
    /// descriptor opened between the two calls does not silently truncate the
    /// listing (and undercount).
    const SIZING_SLACK_ENTRIES: usize = 64;

    /// Open descriptors for this process, or `None` if the kernel refused to
    /// answer.
    pub fn open_descriptor_count() -> Option<u64> {
        let pid = std::process::id() as libc::c_int;
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        // Step 1: how big is the descriptor table? This is a CAPACITY, not the
        // open count -- see the module doc comment. It is only used to size the
        // buffer for step 2.
        //
        // SAFETY: a null buffer with a zero size is the documented sizing call;
        // the kernel writes nothing.
        let capacity_bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if capacity_bytes <= 0 {
            return None;
        }
        let slots = (capacity_bytes as usize / entry) + SIZING_SLACK_ENTRIES;
        let mut buffer: Vec<libc::proc_fdinfo> = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            slots
        ];
        let buffer_bytes = slots.saturating_mul(entry);
        let Ok(buffer_bytes) = libc::c_int::try_from(buffer_bytes) else {
            return None;
        };
        // Step 2: the real listing. `proc_pidfdlist` copies out only OPEN
        // entries and returns the bytes it actually wrote.
        //
        // SAFETY: `buffer` is a live, fully-initialised allocation of exactly
        // `buffer_bytes` bytes, and `buffer_bytes` is what the kernel is told
        // it may write -- never more than the allocation.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer_bytes,
            )
        };
        if written <= 0 {
            // Zero is not a plausible answer for a running process (stdin/
            // stdout/stderr alone are three), so it is treated as failure
            // rather than reported as a fabricated 0 -- same "honest absence,
            // never a made-up number" rule as `platform::mem`.
            return None;
        }
        Some(written as u64 / entry as u64)
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{DescriptorLimits, RaiseOutcome};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    /// Windows reports HANDLES, not descriptors, and imposes no per-process
    /// handle rlimit worth reporting (the practical ceiling is ~16 million).
    /// The field name in the log line is shared with macOS on purpose so one
    /// grep reads both platforms.
    pub fn open_descriptor_count() -> Option<u64> {
        let mut count: u32 = 0;
        // SAFETY: both arguments are caller-owned; `GetCurrentProcess` returns
        // a pseudo-handle that needs no release.
        let result = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
        match result {
            Ok(()) => Some(count as u64),
            Err(e) => {
                log::warn!("platform::fd: GetProcessHandleCount failed: {e}");
                None
            }
        }
    }

    pub fn descriptor_limits() -> Option<DescriptorLimits> {
        None
    }

    pub fn raise_soft_limit() -> RaiseOutcome {
        RaiseOutcome::Unsupported
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod stub_count {
    pub fn open_descriptor_count() -> Option<u64> {
        // `/dev/fd` (or `/proc/self/fd`) is the portable fallback; counting its
        // entries costs a directory walk, which is why it is not the macOS
        // path.
        std::fs::read_dir("/dev/fd")
            .ok()
            .map(|entries| entries.filter_map(Result::ok).count() as u64)
    }
}

#[cfg(all(not(unix), not(target_os = "windows")))]
mod non_unix_limits {
    use super::{DescriptorLimits, RaiseOutcome};
    pub fn descriptor_limits() -> Option<DescriptorLimits> {
        None
    }
    pub fn raise_soft_limit() -> RaiseOutcome {
        RaiseOutcome::Unsupported
    }
}

#[cfg(target_os = "macos")]
pub use macos_count::open_descriptor_count;
#[cfg(target_os = "windows")]
pub use windows_impl::open_descriptor_count;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use stub_count::open_descriptor_count;

#[cfg(unix)]
pub use unix_impl::{descriptor_limits, raise_soft_limit};
#[cfg(target_os = "windows")]
pub use windows_impl::{descriptor_limits, raise_soft_limit};
#[cfg(all(not(unix), not(target_os = "windows")))]
pub use non_unix_limits::{descriptor_limits, raise_soft_limit};

/// Raise the soft descriptor limit and record before/after in the log, as one
/// `startup:` line so every field log carries the value in force.
///
/// Called from `run()` immediately after logging is initialised and well before
/// the Tauri builder, any webview, or the LiveKit runtime -- all of which
/// allocate descriptors against whatever limit is in force when they start.
pub fn log_startup_descriptor_limits() {
    let before = descriptor_limits();
    let outcome = raise_soft_limit();
    let after = descriptor_limits();
    let open = open_descriptor_count()
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unknown".into());
    let (before_soft, hard) = match before {
        Some(limits) => (limits.soft.to_string(), limits.hard_display()),
        None => ("unknown".into(), "unknown".into()),
    };
    let after_soft = after
        .map(|limits| limits.soft.to_string())
        .unwrap_or_else(|| "unknown".into());
    match outcome {
        RaiseOutcome::Raised { from, to } => log::info!(
            "startup: file descriptors soft_limit={from} -> {to} hard_limit={hard} fd_open={open} (#104)"
        ),
        RaiseOutcome::AlreadyAmple { soft } => log::info!(
            "startup: file descriptors soft_limit={soft} hard_limit={hard} fd_open={open} \
             -- already at the ceiling, not raised (#104)"
        ),
        RaiseOutcome::Failed { soft, attempted } => log::warn!(
            "startup: file descriptors soft_limit={soft} hard_limit={hard} fd_open={open} \
             -- every setrlimit up to {attempted} was refused; continuing at the inherited \
             limit (#104)"
        ),
        RaiseOutcome::Unsupported => log::info!(
            "startup: file descriptors soft_limit={before_soft} -> {after_soft} \
             hard_limit={hard} fd_open={open} -- no RLIMIT_NOFILE on this platform (#104)"
        ),
    }
}

/// The always-openable null device, by platform. `/dev/null` does not exist
/// on Windows and made two descriptor-gauge tests panic there while the
/// gauge itself was fine (#104).
#[cfg(test)]
pub(crate) const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_target_the_ceiling_when_the_hard_limit_is_unlimited() {
        // The real macOS GUI-app shape: soft 256, hard RLIM_INFINITY.
        let candidates = soft_limit_candidates(256, u64::MAX);
        assert_eq!(candidates.first(), Some(&SOFT_LIMIT_CEILING));
        assert!(
            candidates.windows(2).all(|pair| pair[0] > pair[1]),
            "candidates must descend so a retry never asks for MORE: {candidates:?}"
        );
        assert!(
            candidates.iter().all(|value| *value > 256),
            "a candidate at or below the limit in force would be a silent downgrade: {candidates:?}"
        );
    }

    #[test]
    fn candidates_clamp_to_a_low_hard_limit() {
        // "The hard limit is already low": we may not exceed it, so the target
        // is the hard limit itself, not the ceiling.
        let candidates = soft_limit_candidates(64, 512);
        assert_eq!(candidates.first(), Some(&512));
        assert!(candidates.iter().all(|value| *value <= 512));
    }

    #[test]
    fn candidates_are_empty_when_there_is_nothing_to_raise_to() {
        // soft == hard, below the ceiling: nothing to ask for. Reported as
        // AlreadyAmple, never as a failure.
        assert!(soft_limit_candidates(512, 512).is_empty());
        // Already at the ceiling.
        assert!(soft_limit_candidates(SOFT_LIMIT_CEILING, u64::MAX).is_empty());
        // Already above it -- must never propose a DOWNGRADE to the ceiling.
        assert!(soft_limit_candidates(65_536, u64::MAX).is_empty());
    }

    #[test]
    fn candidates_terminate_and_stay_bounded() {
        let candidates = soft_limit_candidates(1, u64::MAX);
        assert!(!candidates.is_empty());
        assert!(
            candidates.len() <= 6,
            "a bounded retry, not a walk down to 1: {candidates:?}"
        );
    }

    #[test]
    fn high_water_and_clear_water_are_tested_in_both_directions() {
        // 80% of 256 is 204.8 -> 205 crosses, 204 does not.
        assert!(at_high_water(205, 256));
        assert!(!at_high_water(204, 256));
        assert!(at_high_water(256, 256));
        assert!(!at_high_water(0, 256));
        // Hysteresis: 70% of 256 is 179.2. 180 is still inside the episode,
        // 179 clears it. Crucially, a value between the two marks does NOT
        // clear -- that is the whole point of the gap.
        assert!(below_clear_water(179, 256));
        assert!(!below_clear_water(180, 256));
        assert!(!below_clear_water(200, 256));
        assert!(
            !below_clear_water(204, 256) && !at_high_water(204, 256),
            "between the marks a reading must neither open nor close an episode"
        );
        // A nonsense limit must not divide, fire, or trap.
        assert!(!at_high_water(1_000, 0));
        assert!(below_clear_water(1_000, 0));
    }

    #[test]
    fn exhaustion_errors_are_recognised_and_others_are_not() {
        #[cfg(unix)]
        {
            assert!(is_descriptor_exhaustion(&std::io::Error::from_raw_os_error(
                libc::EMFILE
            )));
            assert!(is_descriptor_exhaustion(&std::io::Error::from_raw_os_error(
                libc::ENFILE
            )));
            // The negative direction matters: a check whose "no" carries no
            // information is worth nothing (CLAUDE.md's gate rule).
            assert!(!is_descriptor_exhaustion(
                &std::io::Error::from_raw_os_error(libc::ENOENT)
            ));
            assert!(!is_descriptor_exhaustion(
                &std::io::Error::from_raw_os_error(libc::ENOSPC)
            ));
        }
        assert!(!is_descriptor_exhaustion(&std::io::Error::other("not an os error")));
    }

    /// The gauge itself, measured against real descriptors -- an assertion on
    /// the arithmetic alone would not prove `proc_pidinfo` is being asked the
    /// right question. Opens files, watches the count RISE, drops them, watches
    /// it FALL back.
    #[test]
    fn open_descriptor_count_tracks_real_descriptors() {
        let Some(baseline) = open_descriptor_count() else {
            // Non-macOS/Windows without /dev/fd: nothing to assert.
            return;
        };
        // The count is process-wide and `cargo test` runs tests concurrently in
        // this same process, so both assertions carry half of EXTRA as slack --
        // enough to absorb other threads opening or closing files, far too
        // little to pass if this gauge were not tracking real descriptors.
        const EXTRA: u64 = 64;
        const SLACK: u64 = EXTRA / 2;
        let mut held = Vec::with_capacity(EXTRA as usize);
        for _ in 0..EXTRA {
            held.push(std::fs::File::open(NULL_DEVICE).expect("open the null device"));
        }
        let raised = open_descriptor_count().expect("count while holding descriptors");
        assert!(
            raised >= baseline + SLACK,
            "holding {EXTRA} extra descriptors must be visible: {baseline} -> {raised}"
        );
        drop(held);
        let settled = open_descriptor_count().expect("count after release");
        assert!(
            settled + SLACK <= raised,
            "releasing {EXTRA} descriptors must be visible too: {raised} -> {settled}"
        );
    }

    #[test]
    fn descriptor_limits_are_readable_and_the_raise_never_lowers_them() {
        let Some(before) = descriptor_limits() else {
            return;
        };
        assert!(before.soft > 0, "a zero soft limit would be nonsense");
        assert!(before.hard >= before.soft);
        let outcome = raise_soft_limit();
        let after = descriptor_limits().expect("limits readable after the raise");
        assert!(
            after.soft >= before.soft,
            "raising must never lower the limit in force: {before:?} -> {after:?} ({outcome:?})"
        );
        match outcome {
            RaiseOutcome::Raised { from, to } => {
                assert_eq!(from, before.soft);
                assert_eq!(to, after.soft);
            }
            RaiseOutcome::AlreadyAmple { soft } => assert_eq!(soft, before.soft),
            // Non-fatal by contract; the process keeps running either way.
            RaiseOutcome::Failed { .. } | RaiseOutcome::Unsupported => {}
        }
    }

    #[test]
    fn hard_display_never_invents_a_number_for_unlimited() {
        assert_eq!(
            DescriptorLimits {
                soft: 256,
                hard: u64::MAX
            }
            .hard_display(),
            "unlimited"
        );
        assert_eq!(
            DescriptorLimits {
                soft: 256,
                hard: 4096
            }
            .hard_display(),
            "4096"
        );
    }
}
