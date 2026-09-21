//! Meeting chat transport (plugins/README.md §2.7, decision 2 as amended
//! 2026-09-14: chat is a HOST surface). Native carries the bytes and stamps
//! the sender; the wire model, validation, history, and rendering live in
//! `shared/logic/chat.ts` on the trusted main webview, exactly like the
//! plugin bus. Pinned by contracts/petal-contracts.json (`topics.chat`,
//! `chatLimits`, `chatDataEvent`).
//!
//! Inbound: every `petal.chat` packet from an authenticated participant is
//! forwarded as one global `chat-data` event (`senderIdentity`, `senderName`,
//! `payloadBase64`), after a size cap and a per-sender rate limit. Outbound:
//! `chat_publish` validates size and destinations and publishes reliably.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::plugins::bus::RateLimiter;
use crate::session::{RoomGeneration, SessionState};

pub const TOPIC: &str = "petal.chat";
pub const EVENT_NAME: &str = "chat-data";
pub const MAX_PAYLOAD_BYTES: usize = 8192;
pub const INBOUND_PER_SENDER_PER_SECOND: f64 = 10.0;
/// Outbound backstop for the whole webview; the drawer itself sends far less.
const OUTBOUND_PER_SECOND: f64 = 10.0;
const MAX_DESTINATIONS: usize = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatDataEvent {
    pub sender_identity: String,
    pub sender_name: Option<String>,
    pub payload_base64: String,
}

pub fn event_for(
    sender_identity: &str,
    sender_name: Option<&str>,
    payload: &[u8],
) -> ChatDataEvent {
    let sender_name = sender_name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    ChatDataEvent {
        sender_identity: sender_identity.to_string(),
        sender_name,
        payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
    }
}

pub fn start_receiver_for_room(
    app: &AppHandle,
    room: Arc<livekit::Room>,
    generation: RoomGeneration,
) {
    let mut events = room.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut inbound = RateLimiter::new(INBOUND_PER_SENDER_PER_SECOND);
        let mut last_prune = Instant::now();
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("chat: receiver exiting for stale room generation");
                break;
            }
            let livekit::RoomEvent::DataReceived {
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
            if payload.len() > MAX_PAYLOAD_BYTES {
                log::debug!(
                    "chat: {} byte payload exceeds {MAX_PAYLOAD_BYTES}; dropped",
                    payload.len()
                );
                continue;
            }
            // No authenticated sender = nothing the drawer may attribute; drop.
            let Some(participant) = participant.as_ref() else {
                continue;
            };
            let sender_identity = participant.identity().to_string();
            let sender_name = participant.name();
            let now = Instant::now();
            if !inbound.try_take_at(&sender_identity, now) {
                continue;
            }
            if now.duration_since(last_prune).as_secs() > 60 {
                inbound.prune(now, 120.0);
                last_prune = now;
            }
            let event = event_for(&sender_identity, Some(sender_name.as_str()), &payload);
            // Global emit (never emit_to): plain `listen()` in the main webview
            // only receives global events.
            if let Err(e) = app.emit(EVENT_NAME, &event) {
                log::debug!("chat: emit failed: {e}");
            }
        }
    });
}

fn outbound_limiter() -> &'static Mutex<RateLimiter> {
    static LIMITER: OnceLock<Mutex<RateLimiter>> = OnceLock::new();
    LIMITER.get_or_init(|| Mutex::new(RateLimiter::new(OUTBOUND_PER_SECOND)))
}

pub fn validate_publish(
    payload_base64: &str,
    destination_identities: &[String],
) -> Result<Vec<u8>, String> {
    let payload = base64::engine::general_purpose::STANDARD
        .decode(payload_base64)
        .map_err(|e| format!("payload is not valid base64: {e}"))?;
    if payload.is_empty() {
        return Err("payload is empty".to_string());
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(format!(
            "payload is {} bytes; the limit is {MAX_PAYLOAD_BYTES}",
            payload.len()
        ));
    }
    if destination_identities.len() > MAX_DESTINATIONS {
        return Err(format!("at most {MAX_DESTINATIONS} destination identity (a history reply goes to one requester)"));
    }
    if destination_identities
        .iter()
        .any(|d| d.trim().is_empty() || d.len() > 256)
    {
        return Err("destination identities must be non-empty".to_string());
    }
    Ok(payload)
}

/// Publish one chat packet from the trusted main webview. Always reliable;
/// the topic is fixed here so nothing else can be published through it.
#[tauri::command]
pub async fn chat_publish(
    app: AppHandle,
    payload_base64: String,
    destination_identities: Option<Vec<String>>,
) -> Result<(), String> {
    let destinations = destination_identities.unwrap_or_default();
    let payload = validate_publish(&payload_base64, &destinations)?;
    if !outbound_limiter()
        .lock()
        .map_err(|_| "limiter poisoned".to_string())?
        .try_take("chat")
    {
        return Err("chat is sending too fast; try again in a moment".to_string());
    }
    let state = app
        .try_state::<SessionState>()
        .ok_or_else(|| "session state is not available".to_string())?;
    let (room_connection, _identity) = state
        .inner()
        .control_channel_snapshot()
        .ok_or_else(|| "join a room before sending chat".to_string())?;
    let packet = livekit::DataPacket {
        payload,
        topic: Some(TOPIC.to_string()),
        reliable: true,
        destination_identities: destinations
            .into_iter()
            .map(livekit::prelude::ParticipantIdentity)
            .collect(),
    };
    room_connection
        .room()
        .local_participant()
        .publish_data(packet)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contracts() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn topic_and_limits_match_the_contract() {
        let c = contracts();
        assert_eq!(c["topics"]["chat"], TOPIC);
        assert_eq!(c["chatLimits"]["maxPayloadBytes"], MAX_PAYLOAD_BYTES);
        assert_eq!(
            c["chatLimits"]["inboundPerSenderPerSecond"]
                .as_f64()
                .unwrap(),
            INBOUND_PER_SENDER_PER_SECOND
        );
        assert_eq!(c["chatDataEvent"]["name"], EVENT_NAME);
    }

    #[test]
    fn event_matches_the_contract_example_and_stamps_the_sender_from_livekit() {
        let c = contracts();
        let example = &c["chatDataEvent"]["example"];
        let payload = base64::engine::general_purpose::STANDARD
            .decode(example["payloadBase64"].as_str().unwrap())
            .unwrap();
        // The payload is the contract's `msg` vector, byte for byte.
        let wire: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(wire, c["chatMessages"][0]["message"]);
        let event = event_for("alex-1a2b", Some("Alex"), &payload);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json, *example);
        let mut fields: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort();
        let pinned: Vec<&str> = c["chatDataEvent"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(fields, pinned);
        // A blank LiveKit name is None, never an empty string the drawer would render.
        assert_eq!(event_for("x", Some("   "), b"{}").sender_name, None);
        assert_eq!(event_for("x", None, b"{}").sender_name, None);
    }

    #[test]
    fn validate_publish_enforces_size_shape_and_destinations() {
        let ok =
            base64::engine::general_purpose::STANDARD.encode(br#"{"v":1,"type":"history-req"}"#);
        assert!(validate_publish(&ok, &[]).is_ok());
        assert!(validate_publish(&ok, &["joiner-1".to_string()]).is_ok());
        assert!(validate_publish("!!not base64!!", &[]).is_err());
        assert!(validate_publish("", &[]).unwrap_err().contains("empty"));
        let big =
            base64::engine::general_purpose::STANDARD.encode(vec![b'x'; MAX_PAYLOAD_BYTES + 1]);
        assert!(validate_publish(&big, &[]).unwrap_err().contains("limit"));
        let exact = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; MAX_PAYLOAD_BYTES]);
        assert!(validate_publish(&exact, &[]).is_ok());
        assert!(validate_publish(&ok, &["a".to_string(), "b".to_string()])
            .unwrap_err()
            .contains("at most"));
        assert!(validate_publish(&ok, &["  ".to_string()]).is_err());
    }

    #[test]
    fn inbound_limiter_is_per_sender() {
        let mut limiter = RateLimiter::new(INBOUND_PER_SENDER_PER_SECOND);
        let now = Instant::now();
        for _ in 0..(INBOUND_PER_SENDER_PER_SECOND as usize) {
            assert!(limiter.try_take_at("alex", now));
        }
        assert!(!limiter.try_take_at("alex", now), "alex is over the window");
        assert!(limiter.try_take_at("mira", now), "mira is unaffected");
    }
}
