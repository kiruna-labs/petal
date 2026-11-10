//! Shared room-generation token (deduplicated from the two platform session
//! modules). A per-room watcher loop holds a snapshot token so stale loops
//! from an older room cannot mutate UI/native state after a fast rejoin.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct RoomGeneration {
    counter: Arc<AtomicU64>,
    value: u64,
}

impl RoomGeneration {
    /// Snapshot the counter at `value` — used by the platform session
    /// modules' `begin_room_generation`/`current_room_generation` helpers.
    pub(crate) fn new(counter: Arc<AtomicU64>, value: u64) -> Self {
        Self { counter, value }
    }

    pub(crate) fn is_current(&self) -> bool {
        self.counter.load(Ordering::SeqCst) == self.value
    }

    /// Invalidate THIS generation if it is still the current one. Used by
    /// the Windows forced-disconnect path to stop watchers of the generation
    /// that just died; macOS never calls it (kept on the shared type so the
    /// Windows production call site survives the dedup).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn invalidate_if_current(&self) -> bool {
        self.counter
            .compare_exchange(
                self.value,
                self.value.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }
}

/// Counts `join_room` attempts that have started connecting but not yet
/// returned. A camera turned on in that window has no room to publish into
/// yet; it is kept as intent and published once the join commits, instead
/// of being refused as "not in room" (the join-time camera toast).
#[derive(Default)]
pub(crate) struct JoinAttempts {
    in_flight: AtomicUsize,
}

impl JoinAttempts {
    /// Mark a join as in flight until the returned guard drops (every exit
    /// path of the join, success or failure).
    pub(crate) fn begin(&self) -> JoinAttemptGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        JoinAttemptGuard(&self.in_flight)
    }

    pub(crate) fn in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }
}

pub(crate) struct JoinAttemptGuard<'a>(&'a AtomicUsize);

impl Drop for JoinAttemptGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_attempt_is_in_flight_only_while_its_guard_lives() {
        let attempts = JoinAttempts::default();
        assert!(!attempts.in_flight());
        let first = attempts.begin();
        assert!(attempts.in_flight());
        let second = attempts.begin();
        drop(first);
        assert!(attempts.in_flight(), "an overlapping join is still in flight");
        drop(second);
        assert!(!attempts.in_flight());
    }
}
