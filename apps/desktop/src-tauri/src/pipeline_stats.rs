//! Cross-peer pipeline stage snapshots for the Network Cockpit.
//!
//! Local diagnostics can measure only one side of a shared window pipeline.
//! topic carries those local measurements to the peer that cannot observe
//! them directly: senders broadcast grab/encode stages, receivers report
//! receive/decode stages back to the owning sender.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

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
}
