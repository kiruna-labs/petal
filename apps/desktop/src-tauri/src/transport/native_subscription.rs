//! The one place the native room decides what it subscribes to.
//!
//! The native LiveKit connection is the owner of audio and of shared-window
//! video (`petal-window-<u32>`); remote cameras are owned by the hidden
//! gallery bridge. `RoomOptions::auto_subscribe` is therefore `false` and
//! every native subscription is an explicit admission here, applied to the
//! post-connect snapshot, to each future `TrackPublished`, and again on
//! `Reconnected` (a reconnect re-issues publication objects, so admitting from
//! the live room snapshot rather than a retained handle is what keeps this
//! correct).
//!
//! Consequences worth stating: a camera track is never subscribed natively, so
//! the compositor needs no compensating `set_subscribed(false)`; and an
//! admitted non-window video in the compositor is an invariant violation, not
//! a routine case.

use livekit::prelude::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use super::publisher::{window_id_from_track_name, CAMERA_TRACK_PREFIX};
use crate::sync_ext::MutexExt;

/// Whether the native room owns `name`. Video is admitted only when the name
/// is an exact canonical `petal-window-<u32>`; cameras, malformed window
/// names, and unknown video stay unsubscribed. Audio is always admitted.
pub(crate) fn native_owns_track(kind: TrackKind, name: &str) -> bool {
    match kind {
        TrackKind::Audio => true,
        TrackKind::Video => window_id_from_track_name(name).is_some(),
    }
}

pub(crate) struct NativeSubscriptionCoordinator {
    room: Arc<Room>,
}

impl NativeSubscriptionCoordinator {
    pub(crate) fn new(room: Arc<Room>) -> Self {
        Self { room }
    }

    /// Admit every currently-published native-owned track. Runs once after
    /// `Room::connect` and again on `Reconnected`.
    pub(crate) fn apply_snapshot(&self) {
        for participant in self.room.remote_participants().values() {
            for publication in participant.track_publications().values() {
                admit(publication);
            }
        }
    }

    /// Admit a publication carried by an ordered room event. Called by the
    /// connect-time fanout BEFORE the event reaches any consumer, so consumers
    /// only ever see tracks whose ownership has already been decided.
    pub(crate) fn observe(&self, event: &RoomEvent) {
        match event {
            RoomEvent::TrackPublished { publication, .. } => admit(publication),
            RoomEvent::Reconnected => self.apply_snapshot(),
            _ => {}
        }
    }
}

/// A camera publication the native room observed on its event stream and
/// declined by policy. Since the coordinator never subscribes cameras, this is
/// the independent native observation that remains for a camera: the
/// publication reached the SFU and this client's room events. The Test
/// Cockpit's CAM scenario reads it (`test_cockpit::camera_native_evidence`),
/// where the pre-#188 assertion waited for a native recv track that can no
/// longer exist. Decode evidence belongs to the gallery bridge, which owns the
/// camera; this record deliberately claims nothing about frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclinedCameraPublication {
    pub(crate) name: String,
    pub(crate) sid: String,
}

/// Bounded so a long meeting with many camera republishes cannot grow it;
/// the CAM scenario matches by exact name, so old entries are inert.
const DECLINED_CAMERA_CAPACITY: usize = 32;

fn declined_cameras() -> &'static Mutex<VecDeque<DeclinedCameraPublication>> {
    static REGISTRY: OnceLock<Mutex<VecDeque<DeclinedCameraPublication>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Record a declined publication if it is camera video. Pure with respect to
/// the SDK so both directions are unit-testable without a `RemoteTrackPublication`.
fn record_declined_if_camera(kind: TrackKind, name: &str, sid: &str) -> bool {
    if kind != TrackKind::Video || !name.starts_with(CAMERA_TRACK_PREFIX) {
        return false;
    }
    let mut registry = declined_cameras().lock_unpoisoned();
    if registry.iter().any(|entry| entry.sid == sid) {
        return true;
    }
    if registry.len() >= DECLINED_CAMERA_CAPACITY {
        registry.pop_front();
    }
    registry.push_back(DeclinedCameraPublication {
        name: name.to_string(),
        sid: sid.to_string(),
    });
    true
}

/// Every camera publication the native room has observed and declined, oldest
/// first. Read by the Test Cockpit; never by media code.
pub(crate) fn declined_camera_publications() -> Vec<DeclinedCameraPublication> {
    declined_cameras().lock_unpoisoned().iter().cloned().collect()
}

fn admit(publication: &RemoteTrackPublication) {
    let name = publication.name();
    if !native_owns_track(publication.kind(), &name) {
        let sid = publication.sid();
        record_declined_if_camera(publication.kind(), &name, &sid.to_string());
        log::debug!(
            "native subscription: declining '{}' (sid={}); the gallery bridge owns remote cameras",
            name,
            sid
        );
        return;
    }
    // At most one call per buffered `TrackPublished` plus one per snapshot
    // (connect, and each `Reconnected`), so a steady-state publication is
    // admitted at most twice. Idempotent -- the SFU ends up subscribed either
    // way -- but NOT free: the vendored SDK spawns a task per call that sends a
    // `proto::UpdateSubscription` signal request, with no coalescing, so a
    // duplicate costs one round-trip.
    //
    // Neither cheap predicate is a safe gate here. `is_subscribed()` is
    // `track().is_some()` (see `reconcile.rs`): "the SDK already holds the
    // decoded track". That is false for BOTH calls in the overlap above, since
    // `TrackSubscribed` has not run yet, so gating on it would skip only calls
    // that are never the duplicate. `is_desired()` (`info.subscribed`) would
    // dedupe the overlap, but on this path it cannot tell "already requested on
    // this connection" from "requested before a full reconnect that dropped the
    // subscription", and a surviving flag is not proof the SFU still holds it.
    // This is the coordinator's re-admission path, so re-requesting is the safe
    // direction: gate here only once a reconnect run measures that the flag is
    // reliably reset.
    publication.set_subscribed(true);
}

/// Log an invariant violation when the native compositor is handed a
/// subscribed video track that is not a window share. The coordinator above
/// never admits one, so seeing this means SDK behavior changed.
pub(crate) fn log_unexpected_native_video(track_name: &str) {
    let scope = if track_name.starts_with(CAMERA_TRACK_PREFIX) {
        "camera"
    } else {
        "unknown"
    };
    log::warn!(
        "native subscription invariant: compositor received subscribed non-window video '{track_name}' (kind={scope}); the gallery bridge owns remote {scope} video"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Cockpit's CAM evidence after #188: a declined camera is recorded
    /// (once per sid), a declined window or audio name is not, and the
    /// registry stays bounded.
    #[test]
    fn declined_camera_publications_are_recorded_once_per_sid_and_bounded() {
        assert!(record_declined_if_camera(TrackKind::Video, "petal-camera-alice", "TR_cam_a"));
        assert!(record_declined_if_camera(TrackKind::Video, "petal-camera-alice", "TR_cam_a"));
        assert!(!record_declined_if_camera(TrackKind::Video, "petal-window-", "TR_bad_window"));
        assert!(!record_declined_if_camera(TrackKind::Audio, "petal-camera-alice", "TR_audio"));
        let recorded = declined_camera_publications();
        assert_eq!(
            recorded.iter().filter(|e| e.sid == "TR_cam_a").count(),
            1,
            "the snapshot and the buffered event both reach admit(); one record"
        );
        assert!(recorded.iter().all(|e| e.name.starts_with(CAMERA_TRACK_PREFIX)));

        for i in 0..(DECLINED_CAMERA_CAPACITY + 8) {
            record_declined_if_camera(
                TrackKind::Video,
                &format!("petal-camera-bulk-{i}"),
                &format!("TR_bulk_{i}"),
            );
        }
        assert!(declined_camera_publications().len() <= DECLINED_CAMERA_CAPACITY);
    }

    #[test]
    fn admission_table_covers_audio_camera_window_and_unknown_video() {
        assert!(native_owns_track(TrackKind::Audio, "petal-camera-alice"));
        assert!(native_owns_track(TrackKind::Audio, "anything"));
        assert!(native_owns_track(TrackKind::Video, "petal-window-7"));
        assert!(native_owns_track(TrackKind::Video, "petal-window-0"));
        assert!(native_owns_track(
            TrackKind::Video,
            "petal-window-4294967295"
        ));

        assert!(!native_owns_track(TrackKind::Video, "petal-camera-alice"));
        assert!(!native_owns_track(TrackKind::Video, "petal-window-"));
        assert!(!native_owns_track(TrackKind::Video, "petal-window-1x"));
        assert!(!native_owns_track(TrackKind::Video, "petal-window--1"));
        assert!(!native_owns_track(TrackKind::Video, "petal-window-1 "));
        assert!(!native_owns_track(
            TrackKind::Video,
            "petal-window-4294967296"
        ));
        assert!(!native_owns_track(TrackKind::Video, "petal-window-capture"));
        assert!(!native_owns_track(TrackKind::Video, "xpetal-window-1"));
        assert!(!native_owns_track(TrackKind::Video, ""));
    }
}
