//! Cross-peer pipeline stage snapshots for the Network Cockpit.
//!
//! Local diagnostics can measure only one side of a shared window pipeline.
//! topic carries those local measurements to the peer that cannot observe
//! them directly: senders broadcast grab/encode stages, receivers report
//! receive/decode stages back to the owning sender.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use livekit::prelude::*;
use serde::{Deserialize, Serialize};

use crate::diagnostics::{
    CaptureStateReport, DiagnosticsState, PipelineStageKind, PipelineStageMetrics,
    ReceiverFreezeMetrics, TrackHealth,
};
use crate::session::RoomGeneration;
use crate::time_util::now_ms;

pub const TOPIC: &str = "petal.pipeline-stats";
const VERSION: u8 = 1;
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
static EPOCHS_BY_PUBLICATION: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStatsRole {
    Sender,
    Receiver,
}

/// A fact observed by the reporting peer. This stays deliberately coarse and
/// privacy-safe: no title, pixels, input, URL, or wall-clock ordering is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PipelineLifecycle {
    CaptureReady,
    Published,
    Subscribed,
    FirstDecoded,
    FirstPresented,
    Unsubscribed,
    Unpublished,
    TerminalFailure,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineStatsMessage {
    pub v: u8,
    pub role: PipelineStatsRole,
    pub reporter_id: String,
    pub owner_identity: String,
    pub window_id: u32,
    pub seq: u64,
    pub sent_at_ms: u64,
    pub grabbed: Option<PipelineStageMetrics>,
    pub encoded_sent: Option<PipelineStageMetrics>,
    pub received: Option<PipelineStageMetrics>,
    pub decoded: Option<PipelineStageMetrics>,
    pub capture_state: Option<CaptureStateReport>,
    pub receiver_freeze: Option<ReceiverFreezeMetrics>,
    /// Additive v1 correlation fields. `None` preserves older v1 packets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_sid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<PipelineLifecycle>,
    /// #408: the sender's own view of its non-top simulcast rungs, read from
    /// the same outbound-rtp poll that drives the publisher's #107 starvation
    /// guard. Additive v1 field: senders only. `None` (absent) means "unknown"
    /// -- an older sender, a single-layer publish, or no poll yet -- and the
    /// receiver then keeps its pre-#408 behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower_rungs: Option<LowerRungsReport>,
}

/// #408: whether a sender's lower simulcast rungs are worth falling back to.
/// `funded`: the allocator grants some lower rung a nonzero target bitrate.
/// `delivering`: some lower rung is encoding frames (nonzero fps). A receiver
/// whose HIGH layer stalls must not downgrade to LOW when `delivering` is
/// false: LOW is then a second dead layer, and the downgrade turns a ~5 s
/// HIGH hiccup into a ~35 s freeze (the #408 field incident).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LowerRungsReport {
    pub funded: bool,
    pub delivering: bool,
}

/// A sender report older than this is treated as unknown. The publisher polls
/// every 5 s and pipeline stats go out every diagnostics tick, so two missed
/// polls plus slack.
pub const LOWER_RUNGS_FRESH_FOR: Duration = Duration::from_secs(12);

/// Latest local lower-rung activity per published track SID (sender side).
static LOCAL_LOWER_RUNGS: LazyLock<Mutex<HashMap<String, (LowerRungsReport, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Latest REMOTE sender report per (owner identity, window id) (receiver side).
static REMOTE_LOWER_RUNGS: LazyLock<Mutex<HashMap<(String, u32), (LowerRungsReport, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bound for both maps; entries are tiny, this only stops unbounded growth in
/// a long room with many republishes.
const MAX_LOWER_RUNGS_ENTRIES: usize = 200;

fn insert_bounded<K: std::hash::Hash + Eq + Clone>(
    map: &mut HashMap<K, (LowerRungsReport, Instant)>,
    key: K,
    report: LowerRungsReport,
    now: Instant,
) {
    if !map.contains_key(&key) && map.len() >= MAX_LOWER_RUNGS_ENTRIES {
        map.retain(|_, (_, at)| now.duration_since(*at) < LOWER_RUNGS_FRESH_FOR);
        if map.len() >= MAX_LOWER_RUNGS_ENTRIES {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
            {
                map.remove(&oldest);
            }
        }
    }
    map.insert(key, (report, now));
}

fn fresh(entry: Option<&(LowerRungsReport, Instant)>, now: Instant) -> Option<LowerRungsReport> {
    entry
        .filter(|(_, at)| now.duration_since(*at) < LOWER_RUNGS_FRESH_FOR)
        .map(|(report, _)| *report)
}

/// Sender side: the publisher's encoder-stats poll records what it saw.
pub fn record_local_lower_rungs(publication_sid: &str, report: LowerRungsReport) {
    if publication_sid.trim().is_empty() {
        return;
    }
    let mut map = LOCAL_LOWER_RUNGS.lock().unwrap_or_else(|p| p.into_inner());
    insert_bounded(&mut map, publication_sid.to_string(), report, Instant::now());
}

/// Sender side: the publication's stats poll ended (unpublish/republish).
pub fn clear_local_lower_rungs(publication_sid: &str) {
    LOCAL_LOWER_RUNGS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(publication_sid);
}

fn local_lower_rungs(publication_sid: &str) -> Option<LowerRungsReport> {
    let map = LOCAL_LOWER_RUNGS.lock().unwrap_or_else(|p| p.into_inner());
    fresh(map.get(publication_sid), Instant::now())
}

fn record_remote_lower_rungs(owner_identity: &str, window_id: u32, report: LowerRungsReport) {
    let mut map = REMOTE_LOWER_RUNGS.lock().unwrap_or_else(|p| p.into_inner());
    insert_bounded(
        &mut map,
        (owner_identity.to_string(), window_id),
        report,
        Instant::now(),
    );
}

fn forget_remote_lower_rungs(owner_identity: &str, window_id: u32) {
    REMOTE_LOWER_RUNGS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&(owner_identity.to_string(), window_id));
}

/// Receiver side (#408): the owner's latest, still-fresh lower-rung report for
/// this shared window, or `None` when unknown/stale.
pub fn remote_lower_rungs(owner_identity: &str, window_id: u32) -> Option<LowerRungsReport> {
    let map = REMOTE_LOWER_RUNGS.lock().unwrap_or_else(|p| p.into_inner());
    fresh(
        map.get(&(owner_identity.to_string(), window_id)),
        Instant::now(),
    )
}

fn opaque_epoch(publication_sid: &str) -> Option<String> {
    if publication_sid.trim().is_empty() {
        return None;
    }
    let mut epochs = EPOCHS_BY_PUBLICATION
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(epoch) = epochs.get(publication_sid) {
        return Some(epoch.clone());
    }
    let epoch = format!("e{:x}", NEXT_EPOCH.fetch_add(1, Ordering::Relaxed));
    // Bounded only to prevent diagnostics state becoming an unbounded map in a
    // long room. This identity is opaque and local; a later SID gets a new one.
    if epochs.len() >= 200 {
        if let Some(oldest) = epochs.keys().next().cloned() {
            epochs.remove(&oldest);
        }
    }
    epochs.insert(publication_sid.to_string(), epoch.clone());
    Some(epoch)
}

pub fn start_receiver_for_room(
    _app: &tauri::AppHandle,
    room: Arc<Room>,
    local_identity: String,
    generation: RoomGeneration,
    diagnostics: DiagnosticsState,
) {
    let mut events = room.subscribe();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("pipeline-stats: receiver exiting for stale room generation");
                break;
            }
            let RoomEvent::DataReceived {
                payload,
                topic,
                participant,
                ..
            } = event
            else {
                continue;
            };
            if topic.as_deref() != Some(TOPIC) {
                continue;
            }
            let Ok(mut message) = serde_json::from_slice::<PipelineStatsMessage>(&payload) else {
                log::debug!("pipeline-stats: dropping invalid JSON payload");
                continue;
            };
            let Some(sender_identity) = participant.as_ref().map(|p| p.identity().to_string())
            else {
                log::debug!("pipeline-stats: dropping anonymous packet");
                continue;
            };
            if sender_identity == local_identity || message.v != VERSION || message.window_id == 0 {
                continue;
            }
            if message.reporter_id != sender_identity {
                log::warn!(
                    "pipeline-stats: reporterId '{}' did not match packet sender '{}'; using trusted sender",
                    message.reporter_id,
                    sender_identity
                );
                message.reporter_id = sender_identity.clone();
            }
            if !role_is_authoritative(&message, &sender_identity, &local_identity) {
                log::debug!("pipeline-stats: dropping role/route-invalid packet");
                continue;
            }
            record_message(&diagnostics, message);
        }
    });
}

/// Sender facts are broadcast only by the owner; receiver facts are only
/// accepted at their direct owner destination. This prevents a third peer from
/// asserting another side completed a lifecycle stage.
fn role_is_authoritative(
    message: &PipelineStatsMessage,
    trusted_sender: &str,
    local_identity: &str,
) -> bool {
    match message.role {
        PipelineStatsRole::Sender => message.owner_identity == trusted_sender,
        PipelineStatsRole::Receiver => {
            message.owner_identity == local_identity && message.owner_identity != trusted_sender
        }
    }
}

pub async fn publish_for_tracks(
    room: Arc<Room>,
    local_identity: &str,
    tracks: &[TrackHealth],
    diagnostics: &DiagnosticsState,
) {
    if local_identity.trim().is_empty() {
        return;
    }
    let sent_at_ms = now_ms();
    let messages = tracks
        .iter()
        .filter_map(|track| message_for_track(local_identity, track, sent_at_ms))
        .collect::<Vec<_>>();

    for message in messages {
        if matches!(message.role, PipelineStatsRole::Sender) {
            diagnostics.record_canonical_owner_epoch(
                &message.owner_identity,
                message.window_id,
                message.publication_sid.as_deref(),
                message.share_epoch.as_deref().unwrap_or_default(),
                message.seq,
            );
        }
        if let Err(e) = publish_message(room.clone(), message).await {
            log::debug!("pipeline-stats: publish_data failed: {e}");
        }
    }
}

fn message_for_track(
    local_identity: &str,
    track: &TrackHealth,
    sent_at_ms: u64,
) -> Option<PipelineStatsMessage> {
    if track.kind != "video" {
        return None;
    }
    let window_id = track.window_id?;
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let reporter_id = local_identity.to_string();

    let message = match track.direction.as_str() {
        "send" => PipelineStatsMessage {
            v: VERSION,
            role: PipelineStatsRole::Sender,
            reporter_id: reporter_id.clone(),
            owner_identity: reporter_id,
            window_id,
            seq,
            sent_at_ms,
            grabbed: track.grabbed.clone(),
            encoded_sent: track.encoded_sent.clone(),
            received: None,
            decoded: None,
            capture_state: track.capture_state.clone(),
            receiver_freeze: None,
            publication_sid: Some(track.sid.clone()),
            share_epoch: opaque_epoch(&track.sid),
            // Presence in `local_participant().track_publications()` is the
            // evidence for this lifecycle fact. `grabbed` is a sampled metric,
            // not a one-time capture-ready transition, so never relabel every
            // 1Hz observation as captureReady.
            lifecycle: Some(PipelineLifecycle::Published),
            lower_rungs: local_lower_rungs(&track.sid),
        },
        "recv" => PipelineStatsMessage {
            v: VERSION,
            role: PipelineStatsRole::Receiver,
            reporter_id,
            owner_identity: track.owner_identity.clone()?,
            window_id,
            seq,
            sent_at_ms,
            grabbed: None,
            encoded_sent: None,
            received: track.received.clone(),
            decoded: track.decoded.clone(),
            capture_state: None,
            receiver_freeze: track.receiver_freeze.clone(),
            publication_sid: Some(track.sid.clone()),
            // The owner is the epoch authority. A receiver has the shared
            // publication SID but must not mint a conflicting epoch before it
            // observes the owner's sender report.
            share_epoch: None,
            lifecycle: None,
            lower_rungs: None,
        },
        _ => return None,
    };

    message_has_stage(&message).then_some(message)
}

fn message_has_stage(message: &PipelineStatsMessage) -> bool {
    message.grabbed.is_some()
        || message.encoded_sent.is_some()
        || message.received.is_some()
        || message.decoded.is_some()
        || message.capture_state.is_some()
        || message.receiver_freeze.is_some()
        || message.lifecycle.is_some()
        || message.lower_rungs.is_some()
}

fn record_message(diagnostics: &DiagnosticsState, message: PipelineStatsMessage) {
    let PipelineStatsMessage {
        role,
        reporter_id,
        owner_identity,
        window_id,
        sent_at_ms,
        grabbed,
        encoded_sent,
        received,
        decoded,
        capture_state,
        receiver_freeze,
        publication_sid,
        share_epoch,
        lifecycle,
        lower_rungs,
        seq,
        ..
    } = message;

    let declared_epoch = share_epoch.unwrap_or_default();
    let epoch = diagnostics.canonical_or_provisional_epoch(
        &owner_identity,
        window_id,
        publication_sid.as_deref(),
        &declared_epoch,
    );
    if !diagnostics.accept_remote_pipeline_observation(
        &owner_identity,
        window_id,
        &reporter_id,
        publication_sid.as_deref(),
        &epoch,
        seq,
    ) {
        return;
    }
    if matches!(role, PipelineStatsRole::Sender) {
        // #408: only the owner's own sender report speaks for its rungs
        // (`role_is_authoritative` already pinned owner == trusted sender).
        // A sender report WITHOUT the field (older build, single-layer
        // republish, no poll yet) means "unknown" now: forget the old verdict
        // instead of letting it outlive the publication it described.
        match lower_rungs {
            Some(report) => record_remote_lower_rungs(&owner_identity, window_id, report),
            None => forget_remote_lower_rungs(&owner_identity, window_id),
        }
        diagnostics.record_canonical_owner_epoch(
            &owner_identity,
            window_id,
            publication_sid.as_deref(),
            &declared_epoch,
            seq,
        );
    }
    if let Some(lifecycle) = lifecycle {
        diagnostics.record_remote_pipeline_lifecycle(
            owner_identity.clone(),
            window_id,
            reporter_id.clone(),
            epoch.clone(),
            publication_sid.clone(),
            lifecycle,
            seq,
        );
    }
    if let Some(metrics) = grabbed {
        diagnostics.record_remote_pipeline_stage(
            owner_identity.clone(),
            window_id,
            reporter_id.clone(),
            publication_sid.clone(),
            epoch.clone(),
            PipelineStageKind::Grabbed,
            metrics,
            sent_at_ms,
        );
    }
    if let Some(metrics) = encoded_sent {
        diagnostics.record_remote_pipeline_stage(
            owner_identity.clone(),
            window_id,
            reporter_id.clone(),
            publication_sid.clone(),
            epoch.clone(),
            PipelineStageKind::EncodedSent,
            metrics,
            sent_at_ms,
        );
    }
    if let Some(metrics) = received {
        diagnostics.record_remote_pipeline_stage(
            owner_identity.clone(),
            window_id,
            reporter_id.clone(),
            publication_sid.clone(),
            epoch.clone(),
            PipelineStageKind::Received,
            metrics,
            sent_at_ms,
        );
    }
    if let Some(metrics) = decoded {
        diagnostics.record_remote_pipeline_stage(
            owner_identity.clone(),
            window_id,
            reporter_id.clone(),
            publication_sid.clone(),
            epoch.clone(),
            PipelineStageKind::Decoded,
            metrics,
            sent_at_ms,
        );
    }
    if let Some(state) = capture_state {
        diagnostics.record_remote_capture_state(
            owner_identity.clone(),
            window_id,
            reporter_id.clone(),
            publication_sid.clone(),
            epoch.clone(),
            state,
            sent_at_ms,
        );
    }
    if let Some(metrics) = receiver_freeze {
        diagnostics.record_remote_receiver_freeze(
            owner_identity,
            window_id,
            reporter_id,
            publication_sid,
            epoch,
            metrics,
            sent_at_ms,
        );
    }
}

async fn publish_message(room: Arc<Room>, message: PipelineStatsMessage) -> Result<(), String> {
    let destination_identities = match message.role {
        PipelineStatsRole::Receiver if message.owner_identity != message.reporter_id => {
            vec![ParticipantIdentity(message.owner_identity.clone())]
        }
        _ => Vec::new(),
    };
    let payload = serde_json::to_vec(&message)
        .map_err(|e| format!("serialize pipeline stats message: {e}"))?;
    let packet = livekit::DataPacket {
        payload,
        topic: Some(TOPIC.to_string()),
        reliable: true,
        destination_identities,
    };
    room.local_participant()
        .publish_data(packet)
        .await
        .map_err(|e| format!("publish pipeline stats data: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct ContractFixture {
        topics: ContractTopics,
        #[serde(rename = "pipelineStatsMessages")]
        pipeline_stats_messages: Vec<PipelineStatsFixture>,
    }

    #[derive(Deserialize)]
    struct ContractTopics {
        #[serde(rename = "pipelineStats")]
        pipeline_stats: String,
    }

    #[derive(Deserialize)]
    struct PipelineStatsFixture {
        name: String,
        reliable: bool,
        message: PipelineStatsMessage,
        fields: Vec<String>,
        #[serde(rename = "stageFields")]
        stage_fields: Vec<String>,
        #[serde(rename = "captureStateFields")]
        capture_state_fields: Vec<String>,
        #[serde(rename = "captureCpuFields")]
        capture_cpu_fields: Vec<String>,
        #[serde(rename = "receiverFreezeFields")]
        receiver_freeze_fields: Vec<String>,
        /// #408: present on vectors whose message carries `lowerRungs`.
        #[serde(rename = "lowerRungsFields", default)]
        lower_rungs_fields: Option<Vec<String>>,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn topic_matches_shared_contract() {
        assert_eq!(TOPIC, contract_fixture().topics.pipeline_stats);
    }

    #[test]
    fn sender_and_receiver_wire_shapes_match_contract() {
        let fixture = contract_fixture();
        assert_eq!(
            fixture
                .pipeline_stats_messages
                .iter()
                .map(|message| message.name.as_str())
                .collect::<Vec<_>>(),
            vec!["sender", "receiver"]
        );

        for vector in fixture.pipeline_stats_messages {
            assert!(vector.reliable, "{} must be reliable", vector.name);
            let value = serde_json::to_value(&vector.message).unwrap();
            let mut fields = value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            fields.sort();
            assert_eq!(fields, vector.fields, "{}", vector.name);

            for stage in [
                &vector.message.grabbed,
                &vector.message.encoded_sent,
                &vector.message.received,
                &vector.message.decoded,
            ]
            .into_iter()
            .flatten()
            {
                let mut stage_fields = serde_json::to_value(stage)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                stage_fields.sort();
                assert_eq!(stage_fields, vector.stage_fields, "{}", vector.name);
            }
            if let Some(capture_state) = &vector.message.capture_state {
                let mut capture_fields = serde_json::to_value(capture_state)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                capture_fields.sort();
                assert_eq!(
                    capture_fields, vector.capture_state_fields,
                    "{}",
                    vector.name
                );

                let mut cpu_fields = serde_json::to_value(&capture_state.cpu)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                cpu_fields.sort();
                assert_eq!(cpu_fields, vector.capture_cpu_fields, "{}", vector.name);
            }
            if let Some(receiver_freeze) = &vector.message.receiver_freeze {
                let mut freeze_fields = serde_json::to_value(receiver_freeze)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                freeze_fields.sort();
                assert_eq!(
                    freeze_fields, vector.receiver_freeze_fields,
                    "{}",
                    vector.name
                );
            }

            match (&vector.message.lower_rungs, &vector.lower_rungs_fields) {
                (Some(lower_rungs), Some(expected)) => {
                    let mut lower_fields = serde_json::to_value(lower_rungs)
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>();
                    lower_fields.sort();
                    assert_eq!(&lower_fields, expected, "{}", vector.name);
                }
                (None, None) => {}
                (message, fields) => panic!(
                    "{}: lowerRungs {:?} and lowerRungsFields {:?} must be pinned together",
                    vector.name, message, fields
                ),
            }

            let json = serde_json::to_string(&vector.message).unwrap();
            let round_tripped: PipelineStatsMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, vector.message);
        }
    }

    #[test]
    fn messages_include_only_locally_measured_side() {
        let track = TrackHealth {
            kind: "video".into(),
            direction: "send".into(),
            window_id: Some(42),
            grabbed: Some(PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: Some(30.0),
                kbps: None,
            }),
            encoded_sent: Some(PipelineStageMetrics {
                width: Some(1280),
                height: Some(720),
                fps: Some(29.0),
                kbps: Some(1800.0),
            }),
            ..Default::default()
        };

        let message = message_for_track("native-1", &track, 1000).unwrap();

        assert_eq!(message.role, PipelineStatsRole::Sender);
        assert_eq!(message.owner_identity, "native-1");
        assert!(message.grabbed.is_some());
        assert!(message.encoded_sent.is_some());
        assert!(message.received.is_none());
        assert!(message.decoded.is_none());
    }

    #[test]
    fn role_authority_requires_owner_sender_and_direct_receiver_route() {
        let sender = PipelineStatsMessage {
            v: VERSION,
            role: PipelineStatsRole::Sender,
            reporter_id: "owner".into(),
            owner_identity: "owner".into(),
            window_id: 1,
            seq: 1,
            sent_at_ms: 0,
            grabbed: None,
            encoded_sent: None,
            received: None,
            decoded: None,
            capture_state: None,
            receiver_freeze: None,
            publication_sid: None,
            share_epoch: None,
            lifecycle: None,
            lower_rungs: None,
        };
        assert!(role_is_authoritative(&sender, "owner", "viewer"));
        assert!(!role_is_authoritative(&sender, "other", "viewer"));
        let receiver = PipelineStatsMessage {
            role: PipelineStatsRole::Receiver,
            reporter_id: "viewer".into(),
            owner_identity: "owner".into(),
            ..sender
        };
        assert!(role_is_authoritative(&receiver, "viewer", "owner"));
        assert!(!role_is_authoritative(&receiver, "viewer", "other"));
    }

    fn sender_track(sid: &str, window_id: u32) -> TrackHealth {
        TrackHealth {
            kind: "video".into(),
            direction: "send".into(),
            sid: sid.into(),
            window_id: Some(window_id),
            encoded_sent: Some(PipelineStageMetrics {
                width: Some(2560),
                height: Some(1600),
                fps: Some(29.0),
                kbps: Some(2000.0),
            }),
            ..Default::default()
        }
    }

    /// #408: the sender publishes its lower-rung verdict in the per-window
    /// sender message, under the pinned `lowerRungs` wire name, and it
    /// survives a JSON round trip unchanged.
    #[test]
    fn sender_message_carries_recorded_lower_rungs_and_round_trips() {
        let sid = "TR_408_sender_round_trip";
        let dark = LowerRungsReport {
            funded: true,
            delivering: false,
        };
        record_local_lower_rungs(sid, dark);
        let message = message_for_track("native-1", &sender_track(sid, 7), 1000).unwrap();
        assert_eq!(message.lower_rungs, Some(dark));

        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(
            json["lowerRungs"],
            serde_json::json!({ "funded": true, "delivering": false })
        );
        let round_tripped: PipelineStatsMessage =
            serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, message);

        clear_local_lower_rungs(sid);
        let cleared = message_for_track("native-1", &sender_track(sid, 7), 1000).unwrap();
        assert_eq!(cleared.lower_rungs, None);
        assert!(
            !serde_json::to_value(&cleared)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("lowerRungs"),
            "unknown must be omitted, not serialized as null/false"
        );
    }

    /// #408: receivers only ever report their own side; they never carry the
    /// sender-only lower-rung field even if one was recorded for that SID.
    #[test]
    fn receiver_messages_never_carry_lower_rungs() {
        let sid = "TR_408_receiver_side";
        record_local_lower_rungs(
            sid,
            LowerRungsReport {
                funded: false,
                delivering: false,
            },
        );
        let track = TrackHealth {
            kind: "video".into(),
            direction: "recv".into(),
            sid: sid.into(),
            window_id: Some(9),
            owner_identity: Some("owner".into()),
            received: Some(PipelineStageMetrics::default()),
            ..Default::default()
        };
        let message = message_for_track("viewer", &track, 1000).unwrap();
        assert_eq!(message.lower_rungs, None);
        clear_local_lower_rungs(sid);
    }

    /// An older sender (no `lowerRungs` key at all) still parses as v1.
    #[test]
    fn older_sender_without_lower_rungs_still_parses() {
        let mut json = serde_json::to_value(
            message_for_track("native-1", &sender_track("TR_408_legacy", 3), 1).unwrap(),
        )
        .unwrap();
        json.as_object_mut().unwrap().remove("lowerRungs");
        let parsed: PipelineStatsMessage = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.lower_rungs, None);
    }

    /// #408 receiver side: an accepted owner report is what
    /// `remote_lower_rungs` (read by the subscriber's starvation policy)
    /// returns; a later owner report WITHOUT the field forgets it.
    #[test]
    fn record_message_stores_and_forgets_remote_lower_rungs() {
        let diagnostics = DiagnosticsState::default();
        let owner = "owner-408-remote";
        let window_id = 4080;
        let mut message = message_for_track(owner, &sender_track("TR_408_remote", window_id), 1)
            .unwrap();
        message.lower_rungs = Some(LowerRungsReport {
            funded: false,
            delivering: false,
        });
        record_message(&diagnostics, message.clone());
        assert_eq!(
            remote_lower_rungs(owner, window_id),
            Some(LowerRungsReport {
                funded: false,
                delivering: false,
            })
        );
        assert_eq!(remote_lower_rungs(owner, window_id + 1), None);
        assert_eq!(remote_lower_rungs("someone-else", window_id), None);

        message.seq += 1;
        message.lower_rungs = None;
        record_message(&diagnostics, message);
        assert_eq!(remote_lower_rungs(owner, window_id), None);
    }

    #[test]
    fn stale_lower_rungs_report_reads_as_unknown() {
        let now = Instant::now();
        let report = LowerRungsReport {
            funded: true,
            delivering: true,
        };
        assert_eq!(fresh(Some(&(report, now)), now), Some(report));
        assert_eq!(
            fresh(Some(&(report, now)), now + LOWER_RUNGS_FRESH_FOR),
            None
        );
        assert_eq!(fresh(None, now), None);
    }
}
