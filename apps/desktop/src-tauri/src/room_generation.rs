//! Shared room-generation token (deduplicated from the two platform session
//! modules). A per-room watcher loop holds a snapshot token so stale loops
//! from an older room cannot mutate UI/native state after a fast rejoin.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::sync_ext::MutexExt;

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

    /// The generation number, for tagging events with the room they belong to.
    pub(crate) fn value(&self) -> u64 {
        self.value
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

/// The one owner of a camera turned on while no room is committed (PR #503).
///
/// A meeting is pending from the moment a join of its room begins until it
/// commits or the meeting ends. A camera turned on while it is pending is
/// kept here, keyed to the join's room generation, and published by the
/// commit. A failed attempt does not drop it: connection recovery and "Try
/// again" rejoin the same room, and the next attempt adopts it. It is dropped
/// only when the meeting ends: leave, meeting ended, camera off, a join of a
/// different room, or the meeting route abandoning a failed join. Room heals
/// never touch it.
#[derive(Default)]
pub(crate) struct DeferredCamera(Mutex<DeferredCameraState>);

#[derive(Default)]
struct DeferredCameraState {
    /// The pending meeting's room; `None` when no meeting is pending.
    room: Option<String>,
    /// The name its meeting route asked to join (`abandon` is scoped to it).
    room_name: Option<String>,
    /// Generation of the join connecting it right now; `None` between attempts.
    connecting: Option<u64>,
    /// True once the room committed (nothing to defer to any more).
    joined: bool,
    /// A camera turned on while the meeting was pending.
    camera: bool,
}

impl DeferredCameraState {
    /// A join is pending that a camera turned on now can be kept for.
    fn pending(&self) -> bool {
        self.room.is_some() && !self.joined
    }
}

/// What a camera refused as "not in room" does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CameraDeferral {
    /// The room committed after the refusal: publish now.
    RetryNow,
    /// Kept for the pending meeting; its commit publishes it.
    Deferred,
    /// No meeting is pending: a real refusal.
    Refused,
}

impl DeferredCamera {
    /// A join of `room_id` begins. Its generation is bumped under this lock,
    /// so a camera turned on mid-join is always keyed to this join. Returns
    /// the attempt and whether a camera deferred to another room was dropped.
    pub(crate) fn begin_join(
        &self,
        room_id: &str,
        room_name: &str,
        begin_generation: impl FnOnce() -> RoomGeneration,
    ) -> (JoinAttempt<'_>, bool) {
        let mut state = self.0.lock_unpoisoned();
        let generation = begin_generation();
        let dropped = state.camera && state.room.as_deref() != Some(room_id);
        if dropped {
            state.camera = false;
        }
        state.room = Some(room_id.to_string());
        state.room_name = Some(room_name.to_string());
        state.connecting = Some(generation.value());
        state.joined = false;
        drop(state);
        (
            JoinAttempt {
                owner: self,
                generation,
                committed: false,
            },
            dropped,
        )
    }

    /// A camera turned on was refused as "not in room". Checked against the
    /// room slot (`in_room`) under this lock, so a commit can't slip between.
    pub(crate) fn defer(&self, in_room: impl FnOnce() -> bool) -> CameraDeferral {
        let mut state = self.0.lock_unpoisoned();
        if in_room() {
            CameraDeferral::RetryNow
        } else if state.pending() {
            state.camera = true;
            CameraDeferral::Deferred
        } else {
            CameraDeferral::Refused
        }
    }

    /// Defer a camera turned on while a join is pending but not yet
    /// committed, before anything looks at the room slot (Windows exposes it
    /// before its late commit). False when nothing is pending.
    pub(crate) fn defer_if_pending(&self) -> bool {
        let mut state = self.0.lock_unpoisoned();
        if state.pending() {
            state.camera = true;
            return true;
        }
        false
    }

    /// The commit's hand-off: if the committed room at `generation` is still
    /// current and joined, the deferred camera becomes the room's intent
    /// (`set_intent`), in one step under this lock. A connection loss runs
    /// `connection_lost` under the same lock, so it either sees the intent
    /// set (and records it as carryover) or keeps the camera here.
    pub(crate) fn take_camera(
        &self,
        generation: &RoomGeneration,
        set_intent: impl FnOnce(),
    ) -> bool {
        let mut state = self.0.lock_unpoisoned();
        if !generation.is_current() || !state.joined || !state.camera {
            return false;
        }
        state.camera = false;
        set_intent();
        true
    }

    /// The camera was turned off: nothing is waiting any more.
    pub(crate) fn cancel_camera(&self) {
        self.0.lock_unpoisoned().camera = false;
    }

    /// The joined room's connection was lost and recovery will rejoin it:
    /// the meeting is pending again.
    pub(crate) fn connection_lost(&self) {
        let mut state = self.0.lock_unpoisoned();
        state.joined = false;
        state.connecting = None;
    }

    /// The meeting route for `room_name` is gone, so no "Try again" or
    /// recovery will follow a failed attempt: drops the deferred camera and
    /// ends the pending meeting. A joined room, another room's meeting, or a
    /// join in flight (it may be a replacement route's; the route re-sends
    /// this when its own in-flight join settles) is untouched. Returns
    /// whether a deferred camera was dropped.
    pub(crate) fn abandon(&self, room_name: &str) -> bool {
        let mut state = self.0.lock_unpoisoned();
        if state.room.is_none()
            || state.joined
            || state.connecting.is_some()
            || state.room_name.as_deref() != Some(room_name)
        {
            return false;
        }
        let dropped = state.camera;
        *state = DeferredCameraState::default();
        dropped
    }

    /// The meeting ended. Returns whether a deferred camera was dropped.
    pub(crate) fn end(&self) -> bool {
        let mut state = self.0.lock_unpoisoned();
        let dropped = state.camera;
        *state = DeferredCameraState::default();
        dropped
    }

    #[cfg(test)]
    pub(crate) fn camera_deferred(&self) -> bool {
        self.0.lock_unpoisoned().camera
    }
}

/// One join attempt. Dropped without `commit` (every failure path of the
/// join), it leaves the meeting pending and the camera kept for the next
/// attempt: a failed attempt is not a terminal failure (the meeting route
/// ends the meeting with `DeferredCamera::abandon` when it gives up).
pub(crate) struct JoinAttempt<'a> {
    owner: &'a DeferredCamera,
    generation: RoomGeneration,
    committed: bool,
}

impl JoinAttempt<'_> {
    pub(crate) fn generation(&self) -> &RoomGeneration {
        &self.generation
    }

    /// The room committed at this attempt's generation. Returns whether a
    /// camera is waiting to be published into it.
    pub(crate) fn commit(&mut self) -> bool {
        self.committed = true;
        let mut state = self.owner.0.lock_unpoisoned();
        if state.connecting != Some(self.generation.value()) {
            return false;
        }
        state.connecting = None;
        state.joined = true;
        state.camera
    }
}

impl Drop for JoinAttempt<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self.owner.0.lock_unpoisoned();
        if state.connecting == Some(self.generation.value()) {
            state.connecting = None;
        }
    }
}
