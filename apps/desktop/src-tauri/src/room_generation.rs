//! Shared room-generation token (deduplicated from the two platform session
//! modules). A per-room watcher loop holds a snapshot token so stale loops
//! from an older room cannot mutate UI/native state after a fast rejoin.

use std::sync::atomic::{AtomicU64, Ordering};
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
