//! Data-channel RTT probe for the network cockpit.
//!
//! This is intentionally a peer-to-peer LiveKit data-channel round-trip
//! measurement, not glass-to-glass video latency. The sender stamps a ping
//! with its own wall clock, the receiver echoes a pong with the same
//! `probeId`/`sendTimeMs` plus receiver-side timestamps, and the original
//! sender computes RTT plus an NTP-style peer clock-offset estimate on its
//! own clock when the pong returns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use livekit::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::diagnostics::DiagnosticsState;
use crate::session::RoomGeneration;
use crate::sync_ext::MutexExt;
use crate::time_util::now_ms;

pub const TOPIC: &str = "petal.latency-probe";
const VERSION: u8 = 1;
const PING_INTERVAL: Duration = Duration::from_secs(2);
const PROBE_EXPIRY_MS: u64 = 30_000;
const MAX_OUTSTANDING_PROBES: usize = 64;
static NEXT_PROBE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatencyProbeKind {
    Ping,
    Pong,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyProbeMessage {
    pub v: u8,
    pub kind: LatencyProbeKind,
    pub probe_id: u64,
    pub sender_id: String,
    pub send_time_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_receive_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_send_time_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct OutstandingProbes {
    sent_at_ms: HashMap<(String, u64), u64>,
}

impl OutstandingProbes {
    fn remember(
        &mut self,
        peer_identity: impl Into<String>,
        probe_id: u64,
        send_time_ms: u64,
        now_ms: u64,
    ) {
        self.prune(now_ms);
        self.sent_at_ms
            .insert((peer_identity.into(), probe_id), send_time_ms);
        while self.sent_at_ms.len() > MAX_OUTSTANDING_PROBES {
            let Some(oldest_key) = self
                .sent_at_ms
                .iter()
                .min_by_key(|entry| *entry.1)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.sent_at_ms.remove(&oldest_key);
        }
    }

    fn take(&mut self, peer_identity: &str, probe_id: u64) -> Option<u64> {
        self.sent_at_ms
            .remove(&(peer_identity.to_string(), probe_id))
    }

    fn prune(&mut self, now_ms: u64) {
        self.sent_at_ms
            .retain(|_, sent_at| now_ms.saturating_sub(*sent_at) <= PROBE_EXPIRY_MS);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ReceiverAction {
    PublishPong {
        message: LatencyProbeMessage,
        destination_identity: String,
    },
    RecordRtt {
        rtt_ms: f64,
        peer_to_local_clock_offset_us: Option<i64>,
        peer_identity: String,
    },
}

fn next_probe_id(send_time_ms: u64) -> u64 {
    const JS_SAFE_TIME_MASK: u64 = (1u64 << 41) - 1;
    const COUNTER_MASK: u64 = (1u64 << 12) - 1;
    let n = NEXT_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    ((send_time_ms & JS_SAFE_TIME_MASK) << 12) | (n & COUNTER_MASK)
}

fn estimate_peer_to_local_clock_offset_us(
    local_send_time_ms: u64,
    peer_receive_time_ms: u64,
    peer_send_time_ms: u64,
    local_receive_time_ms: u64,
) -> Option<i64> {
    if local_receive_time_ms < local_send_time_ms || peer_send_time_ms < peer_receive_time_ms {
        return None;
    }
    let peer_minus_local_ms = (i128::from(peer_receive_time_ms) - i128::from(local_send_time_ms)
        + i128::from(peer_send_time_ms)
        - i128::from(local_receive_time_ms)) as f64
        / 2.0;
    Some((-peer_minus_local_ms * 1000.0).round() as i64)
}

fn handle_inbound_probe(
    message: LatencyProbeMessage,
    authenticated_sender: Option<String>,
    local_identity: &str,
    generation_current: bool,
    outstanding: &mut OutstandingProbes,
    now_ms: u64,
) -> Option<ReceiverAction> {
    if !generation_current || message.v != VERSION {
        return None;
    }
    let authenticated_sender = authenticated_sender?;
    let authenticated_sender = authenticated_sender.trim();
    if authenticated_sender.is_empty() || authenticated_sender == local_identity {
        return None;
    }

    match message.kind {
        LatencyProbeKind::Ping => Some(ReceiverAction::PublishPong {
            message: LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Pong,
                probe_id: message.probe_id,
                sender_id: local_identity.to_string(),
                send_time_ms: message.send_time_ms,
                receiver_receive_time_ms: Some(now_ms),
                receiver_send_time_ms: None,
            },
            destination_identity: authenticated_sender.to_string(),
        }),
        LatencyProbeKind::Pong => {
            let send_time_ms = outstanding.take(authenticated_sender, message.probe_id)?;
            let rtt_ms = now_ms.saturating_sub(send_time_ms) as f64;
            let peer_to_local_clock_offset_us = message
                .receiver_receive_time_ms
                .zip(message.receiver_send_time_ms)
                .and_then(|(receiver_receive_time_ms, receiver_send_time_ms)| {
                    estimate_peer_to_local_clock_offset_us(
                        send_time_ms,
                        receiver_receive_time_ms,
                        receiver_send_time_ms,
                        now_ms,
                    )
                });
            Some(ReceiverAction::RecordRtt {
                rtt_ms,
                peer_to_local_clock_offset_us,
                peer_identity: authenticated_sender.to_string(),
            })
        }
    }
}

pub fn start_receiver_for_room(
    _app: &AppHandle,
    room: Arc<Room>,
    identity: String,
    generation: RoomGeneration,
    diagnostics: DiagnosticsState,
) {
    let outstanding = Arc::new(Mutex::new(OutstandingProbes::default()));
    start_ping_sender(
        room.clone(),
        identity.clone(),
        generation.clone(),
        diagnostics.clone(),
        outstanding.clone(),
    );

    let mut events = room.subscribe();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("latency-probe: receiver exiting for stale room generation");
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
            let Ok(message) = serde_json::from_slice::<LatencyProbeMessage>(&payload) else {
                log::debug!("latency-probe: dropping invalid JSON payload");
                continue;
            };
            let sender_identity = participant.as_ref().map(|p| p.identity().to_string());
            let action = {
                let mut outstanding = outstanding.lock_unpoisoned();
                handle_inbound_probe(
                    message,
                    sender_identity,
                    &identity,
                    generation.is_current(),
                    &mut outstanding,
                    now_ms(),
                )
            };
            match action {
                Some(ReceiverAction::PublishPong {
                    mut message,
                    destination_identity,
                }) => {
                    message.receiver_send_time_ms = Some(now_ms());
                    if let Err(e) =
                        publish_probe(room.clone(), message, vec![destination_identity]).await
                    {
                        log::debug!("latency-probe: publish pong failed: {e}");
                    }
                }
                Some(ReceiverAction::RecordRtt {
                    rtt_ms,
                    peer_to_local_clock_offset_us,
                    peer_identity,
                }) => {
                    if let Some(offset_us) = peer_to_local_clock_offset_us {
                        diagnostics.record_peer_clock_offset(
                            peer_identity.clone(),
                            offset_us,
                            rtt_ms,
                        );
                    } else {
                        diagnostics.record_peer_rtt(rtt_ms);
                    }
                    log::debug!(
                        "latency-probe: peer RTT to '{peer_identity}' {:.1} ms",
                        rtt_ms
                    );
                }
                None => {}
            }
        }
    });
}

fn start_ping_sender(
    room: Arc<Room>,
    identity: String,
    generation: RoomGeneration,
    diagnostics: DiagnosticsState,
    outstanding: Arc<Mutex<OutstandingProbes>>,
) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(PING_INTERVAL);
        loop {
            interval.tick().await;
            if !generation.is_current() {
                log::debug!("latency-probe: ping sender exiting for stale room generation");
                break;
            }
            if !diagnostics.is_cockpit_open() {
                continue;
            }
            let peer_identities = room
                .remote_participants()
                .keys()
                .map(|identity| identity.to_string())
                .collect::<Vec<_>>();
            for peer_identity in peer_identities {
                let send_time_ms = now_ms();
                let probe_id = next_probe_id(send_time_ms);
                {
                    let mut outstanding = outstanding.lock_unpoisoned();
                    outstanding.remember(
                        peer_identity.clone(),
                        probe_id,
                        send_time_ms,
                        send_time_ms,
                    );
                }
                let message = LatencyProbeMessage {
                    v: VERSION,
                    kind: LatencyProbeKind::Ping,
                    probe_id,
                    sender_id: identity.clone(),
                    send_time_ms,
                    receiver_receive_time_ms: None,
                    receiver_send_time_ms: None,
                };
                if let Err(e) =
                    publish_probe(room.clone(), message, vec![peer_identity.clone()]).await
                {
                    log::debug!("latency-probe: publish ping to '{peer_identity}' failed: {e}");
                }
            }
        }
    });
}

async fn publish_probe(
    room: Arc<Room>,
    message: LatencyProbeMessage,
    destination_identities: Vec<String>,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&message)
        .map_err(|e| format!("serialize latency probe message: {e}"))?;
    let packet = livekit::DataPacket {
        payload,
        topic: Some(TOPIC.to_string()),
        reliable: true,
        destination_identities: destination_identities
            .into_iter()
            .map(ParticipantIdentity)
            .collect(),
    };
    room.local_participant()
        .publish_data(packet)
        .await
        .map_err(|e| format!("publish latency probe data: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct ContractFixture {
        topics: ContractTopics,
        #[serde(rename = "latencyProbeMessages")]
        latency_probe_messages: Vec<LatencyProbeFixture>,
    }

    #[derive(Deserialize)]
    struct ContractTopics {
        #[serde(rename = "latencyProbe")]
        latency_probe: String,
    }

    #[derive(Deserialize)]
    struct LatencyProbeFixture {
        name: String,
        reliable: bool,
        message: LatencyProbeMessage,
        fields: Vec<String>,
    }

    fn contract_fixture() -> ContractFixture {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn topic_matches_shared_contract() {
        assert_eq!(TOPIC, contract_fixture().topics.latency_probe);
    }

    #[test]
    fn ping_and_pong_wire_shapes_match_contract() {
        let fixture = contract_fixture();
        assert_eq!(
            fixture
                .latency_probe_messages
                .iter()
                .map(|message| message.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ping", "pong"]
        );

        for vector in fixture.latency_probe_messages {
            assert!(
                vector.reliable,
                "latency probe {} must be reliable",
                vector.name
            );
            let value = serde_json::to_value(&vector.message).unwrap();
            let mut fields = value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            fields.sort();
            assert_eq!(fields, vector.fields, "{}", vector.name);
            let json = serde_json::to_string(&vector.message).unwrap();
            let round_tripped: LatencyProbeMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, vector.message);
        }
    }

    #[test]
    fn receiver_echoes_ping_as_targeted_pong() {
        let mut outstanding = OutstandingProbes::default();
        let action = handle_inbound_probe(
            LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Ping,
                probe_id: 99,
                sender_id: "peer-a".to_string(),
                send_time_ms: 1_000,
                receiver_receive_time_ms: None,
                receiver_send_time_ms: None,
            },
            Some("peer-a".to_string()),
            "local-a",
            true,
            &mut outstanding,
            1_010,
        );

        assert_eq!(
            action,
            Some(ReceiverAction::PublishPong {
                message: LatencyProbeMessage {
                    v: VERSION,
                    kind: LatencyProbeKind::Pong,
                    probe_id: 99,
                    sender_id: "local-a".to_string(),
                    send_time_ms: 1_000,
                    receiver_receive_time_ms: Some(1_010),
                    receiver_send_time_ms: None,
                },
                destination_identity: "peer-a".to_string(),
            })
        );
    }

    #[test]
    fn receiver_ignores_messages_from_stale_room_generation() {
        let mut outstanding = OutstandingProbes::default();
        let action = handle_inbound_probe(
            LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Ping,
                probe_id: 99,
                sender_id: "peer-a".to_string(),
                send_time_ms: 1_000,
                receiver_receive_time_ms: None,
                receiver_send_time_ms: None,
            },
            Some("peer-a".to_string()),
            "local-a",
            false,
            &mut outstanding,
            1_010,
        );

        assert_eq!(action, None);
    }

    #[test]
    fn receiver_records_rtt_only_for_outstanding_local_probe() {
        let mut outstanding = OutstandingProbes::default();
        outstanding.remember("peer-a", 42, 1_000, 1_000);

        let action = handle_inbound_probe(
            LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Pong,
                probe_id: 42,
                sender_id: "peer-a".to_string(),
                send_time_ms: 1_000,
                receiver_receive_time_ms: None,
                receiver_send_time_ms: None,
            },
            Some("peer-a".to_string()),
            "local-a",
            true,
            &mut outstanding,
            1_037,
        );

        assert_eq!(
            action,
            Some(ReceiverAction::RecordRtt {
                rtt_ms: 37.0,
                peer_to_local_clock_offset_us: None,
                peer_identity: "peer-a".to_string(),
            })
        );

        let duplicate = handle_inbound_probe(
            LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Pong,
                probe_id: 42,
                sender_id: "peer-a".to_string(),
                send_time_ms: 1_000,
                receiver_receive_time_ms: None,
                receiver_send_time_ms: None,
            },
            Some("peer-a".to_string()),
            "local-a",
            true,
            &mut outstanding,
            1_038,
        );
        assert_eq!(duplicate, None);
    }

    #[test]
    fn receiver_records_clock_offset_from_timestamped_pong() {
        let mut outstanding = OutstandingProbes::default();
        outstanding.remember("peer-a", 42, 1_000, 1_000);

        // Peer clock is 25ms ahead of local. The ping takes 10ms out, peer
        // spends 5ms before sending the pong, then the pong takes 10ms back.
        let action = handle_inbound_probe(
            LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Pong,
                probe_id: 42,
                sender_id: "peer-a".to_string(),
                send_time_ms: 1_000,
                receiver_receive_time_ms: Some(1_035),
                receiver_send_time_ms: Some(1_040),
            },
            Some("peer-a".to_string()),
            "local-a",
            true,
            &mut outstanding,
            1_025,
        );

        assert_eq!(
            action,
            Some(ReceiverAction::RecordRtt {
                rtt_ms: 25.0,
                peer_to_local_clock_offset_us: Some(-25_000),
                peer_identity: "peer-a".to_string(),
            })
        );
    }

    #[test]
    fn receiver_records_clock_offsets_for_three_peers_sharing_a_probe_id() {
        let mut outstanding = OutstandingProbes::default();
        for peer_identity in ["peer-a", "peer-b", "peer-c"] {
            outstanding.remember(peer_identity, 42, 1_000, 1_000);
        }

        let actions = ["peer-a", "peer-b", "peer-c"]
            .into_iter()
            .map(|peer_identity| {
                handle_inbound_probe(
                    LatencyProbeMessage {
                        v: VERSION,
                        kind: LatencyProbeKind::Pong,
                        probe_id: 42,
                        sender_id: peer_identity.to_string(),
                        send_time_ms: 1_000,
                        receiver_receive_time_ms: Some(1_010),
                        receiver_send_time_ms: Some(1_015),
                    },
                    Some(peer_identity.to_string()),
                    "local-a",
                    true,
                    &mut outstanding,
                    1_025,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actions,
            vec![
                Some(ReceiverAction::RecordRtt {
                    rtt_ms: 25.0,
                    peer_to_local_clock_offset_us: Some(0),
                    peer_identity: "peer-a".to_string(),
                }),
                Some(ReceiverAction::RecordRtt {
                    rtt_ms: 25.0,
                    peer_to_local_clock_offset_us: Some(0),
                    peer_identity: "peer-b".to_string(),
                }),
                Some(ReceiverAction::RecordRtt {
                    rtt_ms: 25.0,
                    peer_to_local_clock_offset_us: Some(0),
                    peer_identity: "peer-c".to_string(),
                }),
            ]
        );

        let duplicate_peer_a = handle_inbound_probe(
            LatencyProbeMessage {
                v: VERSION,
                kind: LatencyProbeKind::Pong,
                probe_id: 42,
                sender_id: "peer-a".to_string(),
                send_time_ms: 1_000,
                receiver_receive_time_ms: Some(1_010),
                receiver_send_time_ms: Some(1_015),
            },
            Some("peer-a".to_string()),
            "local-a",
            true,
            &mut outstanding,
            1_026,
        );
        assert_eq!(duplicate_peer_a, None);
    }

    #[test]
    fn generated_probe_ids_stay_json_safe_for_web_harness() {
        let probe_id = next_probe_id(1_720_000_000_123);
        assert!(probe_id <= 9_007_199_254_740_991);
    }

    #[test]
    fn outstanding_probes_are_expired_and_bounded() {
        let mut outstanding = OutstandingProbes::default();
        outstanding.remember("peer-a", 1, 1_000, 1_000);
        outstanding.remember(
            "peer-a",
            2,
            1_005 + PROBE_EXPIRY_MS,
            1_005 + PROBE_EXPIRY_MS,
        );
        assert!(outstanding.take("peer-a", 1).is_none());
        assert_eq!(outstanding.take("peer-a", 2), Some(1_005 + PROBE_EXPIRY_MS));

        for i in 0..(MAX_OUTSTANDING_PROBES as u64 + 5) {
            outstanding.remember("peer-a", 10_000 + i, 20_000 + i, 20_000 + i);
        }
        assert!(outstanding.sent_at_ms.len() <= MAX_OUTSTANDING_PROBES);
        assert!(outstanding.take("peer-a", 10_000).is_none());
        assert_eq!(
            outstanding.take("peer-a", 10_000 + MAX_OUTSTANDING_PROBES as u64 + 4),
            Some(20_000 + MAX_OUTSTANDING_PROBES as u64 + 4)
        );
    }
}
