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
use std::sync::Arc;

use super::publisher::{window_id_from_track_name, CAMERA_TRACK_PREFIX};

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

fn admit(publication: &RemoteTrackPublication) {
    let name = publication.name();
    if !native_owns_track(publication.kind(), &name) {
        log::debug!(
            "native subscription: declining '{}' (sid={}); the gallery bridge owns remote cameras",
            name,
            publication.sid()
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
