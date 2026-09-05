//! Live room presence (SPEC.md §4.6: "Presence via heartbeat + graceful/
//! ungraceful leave") — who is actually in a room right now.
//!
//! ## Why this is NOT a second heartbeat mechanism
//!
//! SPEC.md's own phrasing ("Presence via the room service (heartbeat +
//! graceful/ungraceful leave)") describes what a room-service backend would
//! need to build from scratch. This app doesn't need to build that from
//! scratch: LiveKit's SFU already runs a real heartbeat between the server
//! and every connected participant to detect graceful leaves (an explicit
//! `Room::close`/disconnect) vs. ungraceful ones (the connection just dies) —
//! that's exactly what `RoomEvent::ParticipantConnected`/
//! `ParticipantDisconnected` report, and telepointers/resilience already read
//! `RoomEvent`s from this same room connection (`telepointer.rs`,
//! `resilience.rs`) via the identical `room.subscribe()` pattern. Building a
//! second, app-level heartbeat on top would duplicate a mechanism the
//! managed platform already provides, for no benefit — checked before
//! writing a line of this module, per the task's own instruction not to
//! build a second heartbeat if LiveKit's own events are sufficient.
//!
//! What this module adds on top of raw `RoomEvent`s: a per-room-connection
//! cached snapshot (`current_snapshot`) so a late Tauri command
//! (`room_presence`) can answer "who's here right now" without waiting for
//! the next event, plus a `presence-update` event so the frontend's
//! `RoomRow`/`meeting` views can react live instead of polling.

use crate::sync_ext::MutexExt;
use std::collections::HashSet;
use std::sync::Mutex;

use crate::session::RoomGeneration;
use tauri::AppHandle;

/// One participant currently present in the room, as seen by this process's
/// own LiveKit connection. `identity` is the durable LiveKit participant
/// identity (SPEC.md's real per-user identity, threaded through from
/// onboarding — see `session::join_room`); `name` is the display name set at
/// token-mint time.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentParticipant {
    pub identity: String,
    pub name: String,
    pub is_local: bool,
    pub speaking: bool,
    pub mic_muted: bool,
}

/// Payload for the `presence-update` event — the full current roster (not a
/// delta), so a listener that missed an intermediate event still ends up
/// consistent on the next one. Keyed by the durable local room name (not the
/// derived LiveKit room name) so the frontend can match it against
/// `RoomRecord.name` without knowing the LiveKit-side naming scheme.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceUpdate {
    pub room_name: String,
    pub participants: Vec<PresentParticipant>,
}

/// Snapshot of who's currently in the room this process is connected to, if
/// any. `None` room_name/participants pairing lives in `session::SessionState`
/// (see `SessionState::current_room_name`/`presence_snapshot`) — this struct
/// is just the cached roster value itself, guarded by its own mutex so
/// reading it doesn't require locking the rest of `SessionState`.
#[derive(Default)]
pub struct PresenceState {
    inner: Mutex<Vec<PresentParticipant>>,
}

impl PresenceState {
    pub fn snapshot(&self) -> Vec<PresentParticipant> {
        self.inner.lock_unpoisoned().clone()
    }

    fn set(&self, participants: Vec<PresentParticipant>) {
        *self.inner.lock_unpoisoned() = participants;
    }
}
fn apply_speaking(
    participants: &mut [PresentParticipant],
    speaking_identities: &HashSet<String>,
) -> bool {
    let mut changed = false;
    for participant in participants {
        // #659: LiveKit's `ActiveSpeakersChanged` is per-PARTICIPANT, computed
        // from aggregate energy across every track that identity publishes --
        // including, during an AI chat session, the assistant's voice track
        // (published under the sharer's own identity, since it's their
        // session, but deliberately never muted by the room mic-mute button:
        // "the sharer muting their mic must NOT mute the AI's voice"). So
        // while a participant's mic is muted, LiveKit sends ZERO energy from
        // it -- any "speaking" attributed to that identity while muted cannot
        // be their own voice, full stop, whether or not AI chat is involved.
        // This is a strictly correct suppression, not an AI-chat-specific
        // hack: a muted identity has no way to genuinely register as
        // speaking. (An unmuted sharer who is silent while the assistant
        // answers still shows as speaking -- that residual case needs real
        // per-track audio-level detection to close, which this does not
        // attempt.)
        let speaking = speaking_identities.contains(&participant.identity) && !participant.mic_muted;
        changed |= participant.speaking != speaking;
        participant.speaking = speaking;
    }
    changed
}

fn apply_mic_muted(
    participants: &mut [PresentParticipant],
    identity: &str,
    mic_muted: bool,
) -> bool {
    let Some(participant) = participants
        .iter_mut()
        .find(|participant| participant.identity == identity)
    else {
        return false;
    };
    if participant.mic_muted == mic_muted {
        return false;
    }
    participant.mic_muted = mic_muted;
    true
}

fn remote_mic_muted(participant: &livekit::participant::RemoteParticipant) -> bool {
    participant
        .track_publications()
        .values()
        .find(|publication| publication.source() == livekit::track::TrackSource::Microphone)
        .map_or(true, |publication| publication.is_muted())
}

/// The local participant's own equivalent of [`remote_mic_muted`] — same
/// query, same default (no mic publication yet reads as muted), just against
/// `LocalParticipant`'s own publication map instead of a remote one.
///
/// Exists because the initial roster seed (`start_for_room`) used to hardcode
/// `mic_muted: true` for the local entry regardless of actual state. That was
/// harmless before #659 (nothing consulted `mic_muted` when deciding
/// `speaking`), but #659 made `apply_speaking` skip a muted identity
/// entirely — so a user who joined a SECOND room in the same app run, having
/// already unmuted in the first, would never fire `LocalTrackPublished` (the
/// mic publishes pre-unmuted) and their own speaking indicator would stay
/// permanently suppressed for the whole meeting. Reading the real state at
/// seed time closes that.
fn local_mic_muted(participant: &livekit::participant::LocalParticipant) -> bool {
    participant
        .track_publications()
        .values()
        .find(|publication| publication.source() == livekit::track::TrackSource::Microphone)
        .map_or(true, |publication| publication.is_muted())
}

/// Start watching `room`'s participant-connect/disconnect events and keep
/// `presence` (this room connection's cached roster) up to date, emitting a
/// `presence-update` event to the main webview on every change. Called once
/// per room connection from `session::join_room`, the same seam
/// `telepointer::start_receiver_for_room`/`resilience::start_for_room`
/// already use for their own once-per-connection watchers.
///
/// `local_identity`/`local_name` seed the roster with this process's own
/// participant before any `RoomEvent` fires (there's no
/// `ParticipantConnected` event for yourself — only for others already in
/// the room when you join `Room::remote_participants()` covers those, also
/// folded in below).
pub fn start_for_room(
    app: &AppHandle,
    room: std::sync::Arc<livekit::Room>,
    presence: std::sync::Arc<PresenceState>,
    room_display_name: String,
    local_identity: String,
    local_name: String,
    generation: RoomGeneration,
) {
    // Seed with whoever's already in the room (this process joined an
    // already-populated room) plus ourselves — `ParticipantConnected` only
    // fires for participants who connect AFTER this process has already
    // subscribed, so a room with existing occupants needs an explicit
    // initial read of `remote_participants()`.
    let mut initial: Vec<PresentParticipant> = room
        .remote_participants()
        .values()
        .map(|p| PresentParticipant {
            identity: p.identity().to_string(),
            name: p.name(),
            is_local: false,
            speaking: p.is_speaking(),
            mic_muted: remote_mic_muted(p),
        })
        .collect();
    initial.push(PresentParticipant {
        identity: local_identity,
        name: local_name,
        is_local: true,
        speaking: room.local_participant().is_speaking(),
        mic_muted: local_mic_muted(&room.local_participant()),
    });
    presence.set(initial.clone());
    emit_presence(app, &room_display_name, initial);

    let mut events = room.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("presence: room watcher exiting for stale room generation");
                break;
            }
            match event {
                livekit::RoomEvent::ParticipantConnected(p) => {
                    let mut roster = presence.snapshot();
                    let identity = p.identity().to_string();
                    let name = p.name();
                    let log_name = crate::logging::log_safe_quoted(&name);
                    let log_identity = crate::logging::log_safe_quoted(&identity);
                    let log_room = crate::logging::log_safe_quoted(&room_display_name);
                    if !roster.iter().any(|r| r.identity == identity) {
                        log::info!("presence: '{log_name}' ({log_identity}) joined '{log_room}'");
                        roster.push(PresentParticipant {
                            identity,
                            name,
                            is_local: false,
                            speaking: p.is_speaking(),
                            mic_muted: remote_mic_muted(&p),
                        });
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    } else {
                        log::warn!(
                            "presence: ParticipantConnected for '{log_name}' ({log_identity}) in \
                             '{log_room}' but that identity is already in the roster \
                             -- duplicate identity join? (roster unchanged)"
                        );
                    }
                }
                livekit::RoomEvent::ActiveSpeakersChanged { speakers } => {
                    let speaking_identities = speakers
                        .iter()
                        .map(|speaker| speaker.identity().to_string())
                        .collect();
                    let mut roster = presence.snapshot();
                    if apply_speaking(&mut roster, &speaking_identities) {
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    }
                }
                livekit::RoomEvent::TrackPublished {
                    participant,
                    publication,
                } if publication.source() == livekit::track::TrackSource::Microphone => {
                    let mut roster = presence.snapshot();
                    if apply_mic_muted(
                        &mut roster,
                        &participant.identity().to_string(),
                        publication.is_muted(),
                    ) {
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    }
                }
                livekit::RoomEvent::TrackUnpublished {
                    participant,
                    publication,
                } if publication.source() == livekit::track::TrackSource::Microphone => {
                    let mut roster = presence.snapshot();
                    if apply_mic_muted(
                        &mut roster,
                        &participant.identity().to_string(),
                        remote_mic_muted(&participant),
                    ) {
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    }
                }
                livekit::RoomEvent::TrackMuted {
                    participant,
                    publication,
                } if publication.source() == livekit::track::TrackSource::Microphone => {
                    let mut roster = presence.snapshot();
                    if apply_mic_muted(&mut roster, &participant.identity().to_string(), true) {
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    }
                }
                livekit::RoomEvent::TrackUnmuted {
                    participant,
                    publication,
                } if publication.source() == livekit::track::TrackSource::Microphone => {
                    let mut roster = presence.snapshot();
                    if apply_mic_muted(&mut roster, &participant.identity().to_string(), false) {
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    }
                }
                livekit::RoomEvent::ParticipantDisconnected(p) => {
                    let identity = p.identity().to_string();
                    let name = p.name();
                    let log_name = crate::logging::log_safe_quoted(&name);
                    let log_identity = crate::logging::log_safe_quoted(&identity);
                    let log_room = crate::logging::log_safe_quoted(&room_display_name);
                    let mut roster = presence.snapshot();
                    let before = roster.len();
                    roster.retain(|r| r.identity != identity);
                    if roster.len() != before {
                        log::warn!(
                            "presence: '{log_name}' ({log_identity}) disconnected from \
                             '{log_room}' -- roster {before} -> {}",
                            roster.len()
                        );
                        presence.set(roster.clone());
                        emit_presence(&app, &room_display_name, roster);
                    } else {
                        log::warn!(
                            "presence: ParticipantDisconnected for '{log_name}' ({log_identity}) in \
                             '{log_room}' but that identity was not in the roster \
                             (already removed, or never added -- roster unchanged)"
                        );
                    }
                }
                livekit::RoomEvent::Disconnected { reason } => {
                    // This process left (gracefully or not) -- clear the
                    // roster; there is no "current room" for it anymore.
                    let log_room = crate::logging::log_safe_quoted(&room_display_name);
                    log::warn!(
                        "presence: local room connection disconnected ({reason:?}) -- clearing \
                         roster for '{log_room}'"
                    );
                    presence.set(Vec::new());
                    emit_presence(&app, &room_display_name, Vec::new());
                    break;
                }
                _ => {}
            }
        }
    });
}

fn emit_presence(app: &AppHandle, room_name: &str, participants: Vec<PresentParticipant>) {
    // Feed the menubar pill (issue #4): in-meeting = non-empty roster (the
    // roster always includes the local participant while joined, and is
    // cleared on RoomEvent::Disconnected above), count = real roster size.
    // Safe from this tokio event-loop thread -- update_meeting_state
    // marshals its AppKit redraw onto the main thread itself.
    #[cfg(target_os = "macos")]
    crate::menubar::update_meeting_state(app, !participants.is_empty(), participants.len() as u32);

    let _ = tauri::Emitter::emit(
        app,
        "presence-update",
        PresenceUpdate {
            room_name: room_name.to_string(),
            participants,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_empty() {
        let state = PresenceState::default();
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn set_then_snapshot_round_trips() {
        let state = PresenceState::default();
        let participants = vec![PresentParticipant {
            identity: "u1".to_string(),
            name: "Jordan".to_string(),
            is_local: true,
            speaking: false,
            mic_muted: true,
        }];
        state.set(participants.clone());
        assert_eq!(state.snapshot(), participants);
    }

    #[test]
    fn active_speaker_updates_roster_truth() {
        let mut participants = vec![
            PresentParticipant {
                identity: "local".to_string(),
                name: "Local".to_string(),
                is_local: true,
                speaking: false,
                mic_muted: false,
            },
            PresentParticipant {
                identity: "remote".to_string(),
                name: "Remote".to_string(),
                is_local: false,
                speaking: false,
                mic_muted: false,
            },
        ];
        let speakers = std::collections::HashSet::from(["local".to_string()]);

        assert!(apply_speaking(&mut participants, &speakers));
        assert!(participants[0].speaking);
        assert!(!participants[1].speaking);
        assert!(!apply_speaking(&mut participants, &speakers));
    }

    /// #659 regression: the assistant's voice is published under the
    /// sharer's own identity (it's their session), so LiveKit's aggregate,
    /// per-participant `ActiveSpeakersChanged` lights up the sharer as
    /// "speaking" purely from the AI answering -- even though the sharer's
    /// own microphone, muted while they listen, is producing zero energy.
    /// Reverting `apply_speaking`'s muted check makes the first assertion
    /// below fail (a muted identity would still show as speaking).
    #[test]
    fn a_muted_participant_never_shows_as_speaking_even_if_livekit_reports_it() {
        let mut participants = vec![PresentParticipant {
            identity: "sharer".to_string(),
            name: "Sharer".to_string(),
            is_local: false,
            speaking: false,
            mic_muted: true,
        }];
        let speakers = std::collections::HashSet::from(["sharer".to_string()]);

        assert!(
            !apply_speaking(&mut participants, &speakers),
            "a muted participant's speaking flag must not change to true"
        );
        assert!(
            !participants[0].speaking,
            "the assistant's voice (published under the sharer's identity) must not \
             make a MUTED sharer appear to be speaking"
        );

        // The other direction: a genuinely unmuted, genuinely-reported speaker
        // must still show as speaking -- this is the exact failure mode #659's
        // own DoD calls out for any fix here.
        participants[0].mic_muted = false;
        assert!(apply_speaking(&mut participants, &speakers));
        assert!(
            participants[0].speaking,
            "an unmuted participant LiveKit reports as speaking must still show as speaking"
        );
    }

    #[test]
    fn microphone_state_updates_roster_truth() {
        let mut participants = vec![PresentParticipant {
            identity: "remote".to_string(),
            name: "Remote".to_string(),
            is_local: false,
            speaking: false,
            mic_muted: false,
        }];

        assert!(apply_mic_muted(&mut participants, "remote", true));
        assert!(participants[0].mic_muted);
        assert!(!apply_mic_muted(&mut participants, "remote", true));
        assert!(!apply_mic_muted(&mut participants, "missing", false));
    }
}
