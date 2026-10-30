//! Whether the room is LIVE: watches participant attributes (`petal.live`) and
//! emits `stream-live-changed` whenever the derived state changes. Drives every
//! LIVE indicator: the meeting top bar, the floating pill, the macOS menubar
//! pill, and the live share's remote header.
//!
//! Also keeps a handle on the joined room so this computer's
//! `petal.stream-optout` attribute follows the "Keep me off live streams"
//! setting: set right after join and whenever the setting changes.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use livekit::prelude::{Room, RoomEvent};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::wire::{
    derive_live_state, opt_out_value, LiveStreamState, STREAM_LIVE_ATTRIBUTE, STREAM_OPTOUT_ATTRIBUTE,
};
use crate::room_generation::RoomGeneration;
use crate::sync_ext::MutexExt;

pub const STREAM_LIVE_CHANGED_EVENT: &str = "stream-live-changed";

/// One live share in the room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamLiveEntry {
    pub owner_identity: String,
    pub window_id: u32,
    pub paused: bool,
    pub started_at: u64,
}

/// The room's live state. The top-level fields describe the earliest stream
/// (the one that stays when two start together); `streams` lists every live
/// share, earliest first, so each one's surface can carry the badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamLivePayload {
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u32>,
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    pub streams: Vec<StreamLiveEntry>,
}

impl From<Vec<LiveStreamState>> for StreamLivePayload {
    fn from(states: Vec<LiveStreamState>) -> Self {
        let first = states.first().cloned();
        let streams: Vec<StreamLiveEntry> = states
            .into_iter()
            .map(|state| StreamLiveEntry {
                owner_identity: state.share.owner_identity,
                window_id: state.share.window_id,
                paused: state.paused,
                started_at: state.started_at,
            })
            .collect();
        match first {
            Some(state) => Self {
                live: true,
                owner_identity: Some(state.share.owner_identity),
                window_id: Some(state.share.window_id),
                paused: state.paused,
                started_at: Some(state.started_at),
                streams,
            },
            None => Self {
                live: false,
                owner_identity: None,
                window_id: None,
                paused: false,
                started_at: None,
                streams,
            },
        }
    }
}

fn current_slot() -> &'static Mutex<StreamLivePayload> {
    static SLOT: OnceLock<Mutex<StreamLivePayload>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(StreamLivePayload::from(Vec::new())))
}

fn room_slot() -> &'static Mutex<Option<Weak<Room>>> {
    static ROOM: OnceLock<Mutex<Option<Weak<Room>>>> = OnceLock::new();
    ROOM.get_or_init(|| Mutex::new(None))
}

/// The joined room, while there is one.
pub fn current_room() -> Option<Arc<Room>> {
    room_slot().lock_unpoisoned().as_ref().and_then(Weak::upgrade)
}

/// Write this computer's `petal.stream-optout` attribute: `"1"` to keep its
/// audio off live streams, empty (deleting the key) to be included again.
pub async fn publish_opt_out(room: &Room, keep_off: bool) {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert(STREAM_OPTOUT_ATTRIBUTE.to_string(), opt_out_value(keep_off).to_string());
    if let Err(error) = room.local_participant().set_attributes(attributes).await {
        log::warn!("streaming: couldn't update the {STREAM_OPTOUT_ATTRIBUTE} attribute: {error}");
    }
}

/// The last derived state, for a surface that mounts mid-meeting.
pub fn current() -> StreamLivePayload {
    current_slot().lock_unpoisoned().clone()
}

/// Every live stream in the room as it is now, our own attribute included,
/// earliest first.
pub fn streams_in_room(room: &Room) -> Vec<LiveStreamState> {
    let local = room.local_participant();
    let mut pairs: Vec<(String, Option<String>)> = vec![(
        local.identity().to_string(),
        local.attributes().get(STREAM_LIVE_ATTRIBUTE).cloned(),
    )];
    for participant in room.remote_participants().values() {
        pairs.push((
            participant.identity().to_string(),
            participant.attributes().get(STREAM_LIVE_ATTRIBUTE).cloned(),
        ));
    }
    derive_live_state(pairs.iter().map(|(identity, value)| (identity.as_str(), value.as_deref())))
}

/// Derive from the room as it is now, including our own attributes.
pub fn derive_from_room(room: &Room) -> StreamLivePayload {
    StreamLivePayload::from(streams_in_room(room))
}

/// Recompute and emit if changed.
pub fn refresh(app: &AppHandle, room: &Room) {
    let next = derive_from_room(room);
    let changed = {
        let mut slot = current_slot().lock_unpoisoned();
        let changed = *slot != next;
        *slot = next.clone();
        changed
    };
    if changed {
        crate::menubar::update_live_state(app, next.live);
        let _ = app.emit(STREAM_LIVE_CHANGED_EVENT, next);
    }
}

pub(crate) fn start_for_room(app: &AppHandle, room: Arc<Room>, generation: RoomGeneration) {
    let mut events = room.subscribe();
    let app = app.clone();
    *room_slot().lock_unpoisoned() = Some(Arc::downgrade(&room));
    // Absent means included, so only an opt-out needs writing on join.
    if super::settings::current().keep_off_live_streams {
        let room = room.clone();
        tauri::async_runtime::spawn(async move { publish_opt_out(&room, true).await });
    }
    refresh(&app, &room);
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                break;
            }
            match event {
                RoomEvent::ParticipantAttributesChanged { .. }
                | RoomEvent::ParticipantConnected(_)
                | RoomEvent::ParticipantDisconnected(_)
                | RoomEvent::Connected { .. }
                | RoomEvent::Reconnected => refresh(&app, &room),
                RoomEvent::Disconnected { .. } => break,
                _ => {}
            }
        }
        // The room is gone: nothing is live from here.
        {
            let mut slot = room_slot().lock_unpoisoned();
            if slot.as_ref().is_some_and(|weak| weak.ptr_eq(&Arc::downgrade(&room))) {
                *slot = None;
            }
        }
        let cleared = StreamLivePayload::from(Vec::new());
        *current_slot().lock_unpoisoned() = cleared.clone();
        crate::menubar::update_live_state(&app, false);
        let _ = app.emit(STREAM_LIVE_CHANGED_EVENT, cleared);
    });
}

#[tauri::command]
pub fn stream_live_state() -> StreamLivePayload {
    current()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::wire::StreamShareBinding;

    #[test]
    fn payload_shapes() {
        let idle = serde_json::to_value(StreamLivePayload::from(Vec::new())).unwrap();
        assert_eq!(idle, serde_json::json!({ "live": false, "paused": false, "streams": [] }));
        let live = StreamLivePayload::from(vec![
            LiveStreamState {
                share: StreamShareBinding { owner_identity: "alice".into(), window_id: 42 },
                started_at: 1_700_000_000_000,
                paused: true,
            },
            LiveStreamState {
                share: StreamShareBinding { owner_identity: "bob".into(), window_id: 7 },
                started_at: 1_700_000_000_500,
                paused: false,
            },
        ]);
        assert_eq!(
            serde_json::to_value(live).unwrap(),
            serde_json::json!({
                "live": true, "ownerIdentity": "alice", "windowId": 42,
                "paused": true, "startedAt": 1_700_000_000_000u64,
                "streams": [
                    { "ownerIdentity": "alice", "windowId": 42, "paused": true, "startedAt": 1_700_000_000_000u64 },
                    { "ownerIdentity": "bob", "windowId": 7, "paused": false, "startedAt": 1_700_000_000_500u64 }
                ]
            })
        );
    }
}
