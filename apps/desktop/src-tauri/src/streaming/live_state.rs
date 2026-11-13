//! Whether the room is LIVE: watches participant attributes (`petal.live`) and
//! emits `stream-live-changed` whenever the derived state changes. Drives every
//! LIVE indicator: the meeting top bar, the floating pill, the macOS menubar
//! pill, and the live share's remote header.
//!
//! Also owns the joined room's [`AttributeKeeper`], which writes this
//! computer's own `petal.live` and `petal.stream-optout` until they stick and
//! again after every reconnect. The opt-out follows the "Keep me off live
//! streams" setting: set right after join (before the mic publishes, see
//! [`opt_out_settled`]) and whenever the setting changes.

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use livekit::prelude::{Participant, Room, RoomEvent};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

use super::attributes::{AttributeKeeper, RoomSink};
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

/// The joined room and the writer of our own attributes in it.
struct JoinedRoom {
    room: Weak<Room>,
    keeper: Arc<AttributeKeeper>,
}

fn room_slot() -> &'static Mutex<Option<JoinedRoom>> {
    static ROOM: OnceLock<Mutex<Option<JoinedRoom>>> = OnceLock::new();
    ROOM.get_or_init(|| Mutex::new(None))
}

/// How long joining waits for the opt-out to reach the server. An opted-out
/// mic that still isn't covered by then is not published at all.
pub const OPT_OUT_BEFORE_MIC_TIMEOUT: Duration = Duration::from_secs(5);

/// The writer of our own attributes in `room`, while it is the joined room.
pub fn keeper_for(room: &Room) -> Option<Arc<AttributeKeeper>> {
    let slot = room_slot().lock_unpoisoned();
    let joined = slot.as_ref()?;
    let current = joined.room.upgrade()?;
    std::ptr::eq(Arc::as_ptr(&current), room).then(|| joined.keeper.clone())
}

/// Bring this computer's `petal.stream-optout` in line with the "Keep me off
/// live streams" setting: `"1"` keeps its audio off live streams, `"0"`
/// includes it again. Written until it sticks, and again
/// after every reconnect. The setting is read under the room slot's lock, the
/// same lock a join installs its writer under, so a change racing a join
/// can't leave the new room with the old value.
pub fn publish_opt_out() {
    if let Some(joined) = room_slot().lock_unpoisoned().as_ref() {
        joined.keeper.set(STREAM_OPTOUT_ATTRIBUTE, opt_out_value(super::settings::current().keep_off_live_streams));
    }
}

/// Join: wait (up to `timeout`) until the server has our opt-out, so the
/// mic never publishes before a streamer can see it. True when it is there
/// (or there is nothing to write).
pub async fn opt_out_settled(room: &Room, timeout: Duration) -> bool {
    let Some(keeper) = keeper_for(room) else { return true };
    settle_opt_out(&keeper, super::settings::current().keep_off_live_streams, timeout).await
}

async fn settle_opt_out(keeper: &AttributeKeeper, keep_off: bool, timeout: Duration) -> bool {
    // Only an opt-out has to be there first; "0" goes out in the background.
    if !keep_off {
        return true;
    }
    let settled = keeper.wait_confirmed(STREAM_OPTOUT_ATTRIBUTE, opt_out_value(keep_off), timeout).await;
    if !settled {
        log::warn!("streaming: {STREAM_OPTOUT_ATTRIBUTE} not written within {timeout:?}");
    }
    settled
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
    let keeper = AttributeKeeper::new();
    let writer = CancellationToken::new();
    tauri::async_runtime::spawn(keeper.clone().run(RoomSink(Arc::downgrade(&room)), writer.clone()));
    *room_slot().lock_unpoisoned() = Some(JoinedRoom { room: Arc::downgrade(&room), keeper: keeper.clone() });
    // "1" or "0" (see wire::STREAM_OPTIN_VALUE): written on every join.
    publish_opt_out();
    refresh(&app, &room);
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                break;
            }
            if let Some(action) = keeper_action(&event) {
                match action {
                    KeeperAction::ReassertAll => keeper.reassert_all(),
                    KeeperAction::ObserveLocal => keeper.observe_local(&room.local_participant().attributes()),
                }
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
        writer.cancel();
        // The room is gone: nothing is live from here.
        {
            let mut slot = room_slot().lock_unpoisoned();
            if slot.as_ref().is_some_and(|joined| joined.room.ptr_eq(&Arc::downgrade(&room))) {
                *slot = None;
            }
        }
        let cleared = StreamLivePayload::from(Vec::new());
        *current_slot().lock_unpoisoned() = cleared.clone();
        crate::menubar::update_live_state(&app, false);
        let _ = app.emit(STREAM_LIVE_CHANGED_EVENT, cleared);
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeeperAction {
    /// Any reconnect, a full one included: nothing written counts as
    /// confirmed from its start, and everything is written again.
    ReassertAll,
    /// Our own attributes changed: put back anything we wrote that is gone.
    ObserveLocal,
}

/// What a room event means for our own attributes.
fn keeper_action(event: &RoomEvent) -> Option<KeeperAction> {
    match event {
        RoomEvent::Reconnecting | RoomEvent::Reconnected => Some(KeeperAction::ReassertAll),
        RoomEvent::ParticipantAttributesChanged { participant: Participant::Local(_), .. } => {
            Some(KeeperAction::ObserveLocal)
        }
        _ => None,
    }
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
    fn every_reconnect_writes_our_attributes_again() {
        // A resume and a full reconnect both end in `Reconnected` (the SDK's
        // `handle_restarted` dispatches it once tracks are republished).
        assert_eq!(keeper_action(&RoomEvent::Reconnected), Some(KeeperAction::ReassertAll));
        // From the moment the room starts reconnecting, nothing written
        // counts as confirmed (the remote-audio gate closes on that).
        assert_eq!(keeper_action(&RoomEvent::Reconnecting), Some(KeeperAction::ReassertAll));
        assert_eq!(keeper_action(&RoomEvent::Connected { participants_with_tracks: vec![] }), None);
    }

    #[tokio::test(start_paused = true)]
    async fn joining_holds_the_mic_until_the_opt_out_is_on_the_server() {
        use super::super::attributes::{AttributeSink, WriteFuture};
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};

        /// A server that takes 800 ms to accept a write.
        struct SlowRoom(Arc<AtomicBool>);
        impl AttributeSink for SlowRoom {
            fn write(&self, _attributes: HashMap<String, String>) -> WriteFuture<'_> {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    self.0.store(true, Ordering::SeqCst);
                    Ok(())
                })
            }
        }

        let on_server = Arc::new(AtomicBool::new(false));
        let keeper = AttributeKeeper::new();
        keeper.set(STREAM_OPTOUT_ATTRIBUTE, opt_out_value(true));
        tokio::spawn(keeper.clone().run(SlowRoom(on_server.clone()), CancellationToken::new()));
        assert!(settle_opt_out(&keeper, true, OPT_OUT_BEFORE_MIC_TIMEOUT).await);
        assert!(on_server.load(Ordering::SeqCst), "the mic may publish only after the server has the opt-out");

        // Not opted out: nothing to wait for.
        let keeper = AttributeKeeper::new();
        keeper.set(STREAM_OPTOUT_ATTRIBUTE, opt_out_value(false));
        let started = tokio::time::Instant::now();
        assert!(settle_opt_out(&keeper, false, OPT_OUT_BEFORE_MIC_TIMEOUT).await);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

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
