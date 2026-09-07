//! Plugin data bus: `plugin/<id>[/<sub>]` topics over the LiveKit data channel.
//!
//! LOCKSTEP with `shared/plugin-host/topics.ts` and
//! `contracts/petal-contracts.json` (`topics.pluginPrefix`,
//! `pluginTopicVectors`, `pluginDataEvent`, `pluginLimits`) -- docs/CONTRACTS.md
//! "Plugin bus". The sender of every inbound packet is the authenticated
//! LiveKit participant; payload contents are never trusted for identity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::session::{RoomGeneration, SessionState};

pub const TOPIC_PREFIX: &str = "plugin/";
pub const EVENT_NAME: &str = "plugin-data";

/// `pluginLimits` in the contract fixture.
pub const MAX_PAYLOAD_BYTES: usize = 16_384;
pub const LOSSY_PER_SECOND: f64 = 30.0;
pub const RELIABLE_PER_SECOND: f64 = 10.0;
pub const INBOUND_PER_SENDER_PER_SECOND: f64 = 60.0;
const MAX_PLUGIN_ID_LEN: usize = 64;
const MAX_SUB_LEN: usize = 32;
const MAX_DESTINATIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTopic {
    pub plugin_id: String,
    pub sub: Option<String>,
}

/// Same rule as `shared/plugin-host/manifest.ts` `PLUGIN_ID_RE`:
/// `^[a-z0-9]+(\.[a-z0-9-]+)+$`, at most 64 chars.
pub fn is_plugin_id(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_PLUGIN_ID_LEN {
        return false;
    }
    let mut segments = value.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
        return false;
    }
    let mut rest = 0;
    for segment in segments {
        rest += 1;
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return false;
        }
    }
    rest >= 1
}

/// Same rule as `shared/plugin-host/topics.ts` `SUB_RE`: `^[a-z0-9][a-z0-9-]{0,31}$`.
pub fn is_sub(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_SUB_LEN {
        return false;
    }
    (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

pub fn parse_topic(topic: &str) -> Option<PluginTopic> {
    let rest = topic.strip_prefix(TOPIC_PREFIX)?;
    let (plugin_id, sub) = match rest.find('/') {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    if !is_plugin_id(plugin_id) {
        return None;
    }
    if let Some(sub) = sub {
        if !is_sub(sub) {
            return None;
        }
    }
    Some(PluginTopic {
        plugin_id: plugin_id.to_string(),
        sub: sub.map(str::to_string),
    })
}

pub fn topic_for(plugin_id: &str, sub: Option<&str>) -> Result<String, String> {
    if !is_plugin_id(plugin_id) {
        return Err(format!("invalid plugin id: {plugin_id}"));
    }
    match sub {
        None => Ok(format!("{TOPIC_PREFIX}{plugin_id}")),
        Some(sub) if is_sub(sub) => Ok(format!("{TOPIC_PREFIX}{plugin_id}/{sub}")),
        Some(sub) => Err(format!("invalid topic sub: {sub}")),
    }
}

/// Token bucket keyed by an arbitrary string. Mirrors
/// `shared/plugin-host/rateLimit.ts` so both ends enforce the same numbers.
pub struct RateLimiter {
    per_second: f64,
    capacity: f64,
    buckets: HashMap<String, (f64, Instant)>,
}

impl RateLimiter {
    pub fn new(per_second: f64) -> Self {
        Self {
            per_second,
            capacity: per_second.ceil().max(1.0),
            buckets: HashMap::new(),
        }
    }

    pub fn try_take(&mut self, key: &str) -> bool {
        self.try_take_at(key, Instant::now())
    }

    pub fn try_take_at(&mut self, key: &str, now: Instant) -> bool {
        let entry = self
            .buckets
            .entry(key.to_string())
            .or_insert((self.capacity, now));
        if now > entry.1 {
            let refill = now.duration_since(entry.1).as_secs_f64() * self.per_second;
            entry.0 = (entry.0 + refill).min(self.capacity);
            entry.1 = now;
        }
        if entry.0 < 1.0 {
            return false;
        }
        entry.0 -= 1.0;
        true
    }

    /// Drop idle keys so a long meeting with many senders does not grow unbounded.
    pub fn prune(&mut self, now: Instant, idle_secs: f64) {
        self.buckets
            .retain(|_, (_, at)| now.duration_since(*at).as_secs_f64() < idle_secs);
    }
}

pub const STATE_EVENT_NAME: &str = "plugin-state-changed";
pub const PLUGINS_METADATA_KEY: &str = "plugins";
/// `pluginStateMetadata.limits` in the contract fixture.
pub const PER_PLUGIN_STATE_BYTES: usize = 2048;
pub const PLUGINS_TOTAL_BYTES: usize = 8192;
const STATE_WRITES_PER_SECOND: f64 = 4.0;

/// Strict `major.minor.patch` (mirrors `isReleaseVersion` in manifest.ts).
pub fn is_release_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

fn is_source(value: &str) -> bool {
    matches!(value, "builtin" | "registry" | "dev")
}

/// Validate one `plugins[<id>]` entry: `{ v: release version, src: builtin|registry|dev, state?: json <= 2 KB }`.
/// Returns the cleaned entry (unknown keys dropped, null state removed).
pub fn clean_plugin_entry(entry: &serde_json::Value) -> Option<serde_json::Value> {
    let obj = entry.as_object()?;
    let v = obj.get("v")?.as_str()?;
    let src = obj.get("src")?.as_str()?;
    if !is_release_version(v) || !is_source(src) {
        return None;
    }
    let mut out = serde_json::Map::new();
    out.insert("v".into(), serde_json::Value::String(v.into()));
    out.insert("src".into(), serde_json::Value::String(src.into()));
    if let Some(state) = obj.get("state") {
        if !state.is_null() && state.to_string().len() <= PER_PLUGIN_STATE_BYTES {
            out.insert("state".into(), state.clone());
        }
    }
    Some(serde_json::Value::Object(out))
}

/// Read and validate the `plugins` key of a participant-metadata blob.
/// Malformed entries are dropped individually (LOCKSTEP with
/// `pluginsFromMetadata` in shared/plugin-host/metadata.ts).
pub fn plugins_from_metadata(metadata: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return out;
    };
    let Some(plugins) = root.get(PLUGINS_METADATA_KEY).and_then(|v| v.as_object()) else {
        return out;
    };
    for (id, entry) in plugins {
        if !is_plugin_id(id) {
            continue;
        }
        if let Some(clean) = clean_plugin_entry(entry) {
            out.insert(id.clone(), clean);
        }
    }
    out
}

/// Payload of the global `plugin-state-changed` event (`pluginStateChangedEvent`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginStateChangedEvent {
    pub identity: String,
    pub plugins: serde_json::Map<String, serde_json::Value>,
}

fn emit_state(app: &AppHandle, identity: String, metadata: &str) {
    let event = PluginStateChangedEvent {
        identity,
        plugins: plugins_from_metadata(metadata),
    };
    if let Err(e) = app.emit(STATE_EVENT_NAME, &event) {
        log::debug!("plugins::bus: state emit failed: {e}");
    }
}

/// Payload of the global `plugin-data` Tauri event (`pluginDataEvent` in the contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginDataEvent {
    pub topic: String,
    pub plugin_id: String,
    pub sub: Option<String>,
    pub sender_identity: String,
    pub sender_name: Option<String>,
    pub payload_base64: String,
}

pub fn event_for(
    topic: &str,
    parsed: &PluginTopic,
    sender_identity: &str,
    sender_name: Option<&str>,
    payload: &[u8],
) -> PluginDataEvent {
    PluginDataEvent {
        topic: topic.to_string(),
        plugin_id: parsed.plugin_id.clone(),
        sub: parsed.sub.clone(),
        sender_identity: sender_identity.to_string(),
        sender_name: sender_name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string),
        payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
    }
}

/// Start the catch-all receiver for `plugin/*` topics on this room connection.
/// Same one-room-connection seam as `telepointer::start_receiver_for_room`.
pub fn start_receiver_for_room(app: &AppHandle, room: Arc<livekit::Room>, generation: RoomGeneration) {
    let mut events = room.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Late joiner: everyone already in the room advertised before we
        // subscribed, so seed the frontend's view from current metadata.
        for (identity, participant) in room.remote_participants() {
            let metadata = participant.metadata();
            if !plugins_from_metadata(&metadata).is_empty() {
                emit_state(&app, identity.to_string(), &metadata);
            }
        }
        let mut inbound = RateLimiter::new(INBOUND_PER_SENDER_PER_SECOND);
        let mut last_prune = Instant::now();
        while let Some(event) = events.recv().await {
            if !generation.is_current() {
                log::debug!("plugins::bus: receiver exiting for stale room generation");
                break;
            }
            let (payload, topic, participant) = match event {
                livekit::RoomEvent::DataReceived {
                    payload,
                    topic,
                    participant,
                    ..
                } => (payload, topic, participant),
                livekit::RoomEvent::ParticipantMetadataChanged {
                    participant,
                    old_metadata,
                    metadata,
                } => {
                    if matches!(participant, livekit::prelude::Participant::Local(_)) {
                        continue;
                    }
                    if plugins_from_metadata(&old_metadata) != plugins_from_metadata(&metadata) {
                        emit_state(&app, participant.identity().to_string(), &metadata);
                    }
                    continue;
                }
                livekit::RoomEvent::ParticipantConnected(participant) => {
                    let metadata = participant.metadata();
                    if !plugins_from_metadata(&metadata).is_empty() {
                        emit_state(&app, participant.identity().to_string(), &metadata);
                    }
                    continue;
                }
                _ => continue,
            };
            let Some(topic) = topic.as_deref() else {
                continue;
            };
            if !topic.starts_with(TOPIC_PREFIX) {
                continue;
            }
            let Some(parsed) = parse_topic(topic) else {
                log::debug!("plugins::bus: malformed plugin topic {topic:?} dropped");
                continue;
            };
            if payload.len() > MAX_PAYLOAD_BYTES {
                log::debug!(
                    "plugins::bus: {} byte payload for {} exceeds {MAX_PAYLOAD_BYTES}; dropped",
                    payload.len(),
                    parsed.plugin_id
                );
                continue;
            }
            // No authenticated sender = nothing a plugin may trust; drop.
            let Some(participant) = participant.as_ref() else {
                continue;
            };
            let sender_identity = participant.identity().to_string();
            let sender_name = participant.name();
            let now = Instant::now();
            if !inbound.try_take_at(&format!("{sender_identity}\u{0}{}", parsed.plugin_id), now) {
                continue;
            }
            if now.duration_since(last_prune).as_secs() > 60 {
                inbound.prune(now, 120.0);
                last_prune = now;
            }
            let event = event_for(topic, &parsed, &sender_identity, Some(sender_name.as_str()), &payload);
            // Global emit (never emit_to): plain `listen()` in the main webview
            // only receives global events.
            if let Err(e) = app.emit(EVENT_NAME, &event) {
                log::debug!("plugins::bus: emit failed: {e}");
            }
        }
    });
}

fn outbound_limiters() -> &'static Mutex<(RateLimiter, RateLimiter)> {
    static LIMITERS: OnceLock<Mutex<(RateLimiter, RateLimiter)>> = OnceLock::new();
    LIMITERS.get_or_init(|| {
        Mutex::new((
            RateLimiter::new(LOSSY_PER_SECOND),
            RateLimiter::new(RELIABLE_PER_SECOND),
        ))
    })
}

/// Validate one outbound publish (pure; shared by the command and its tests).
pub fn validate_publish(
    plugin_id: &str,
    sub: Option<&str>,
    payload_base64: &str,
    destination_identities: &[String],
) -> Result<(String, Vec<u8>), String> {
    let topic = topic_for(plugin_id, sub)?;
    let payload = base64::engine::general_purpose::STANDARD
        .decode(payload_base64)
        .map_err(|e| format!("payload is not valid base64: {e}"))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(format!(
            "payload is {} bytes; the limit is {MAX_PAYLOAD_BYTES}",
            payload.len()
        ));
    }
    if destination_identities.len() > MAX_DESTINATIONS {
        return Err(format!("at most {MAX_DESTINATIONS} destination identities"));
    }
    if destination_identities
        .iter()
        .any(|d| d.trim().is_empty() || d.len() > 256)
    {
        return Err("destination identities must be non-empty".to_string());
    }
    Ok((topic, payload))
}

/// Publish a plugin data packet from the trusted main webview. The topic is
/// always derived from `plugin_id` here, so a plugin can never publish under
/// another plugin's namespace even if the frontend broker were bypassed.
#[tauri::command]
pub async fn plugin_publish_data(
    app: AppHandle,
    plugin_id: String,
    sub: Option<String>,
    payload_base64: String,
    reliable: bool,
    destination_identities: Option<Vec<String>>,
) -> Result<(), String> {
    let destinations = destination_identities.unwrap_or_default();
    let (topic, payload) = validate_publish(&plugin_id, sub.as_deref(), &payload_base64, &destinations)?;
    {
        let mut limiters = outbound_limiters().lock().map_err(|_| "limiter poisoned".to_string())?;
        let limiter = if reliable { &mut limiters.1 } else { &mut limiters.0 };
        if !limiter.try_take(&plugin_id) {
            return Err(format!(
                "plugin {plugin_id} exceeded its {} publish quota",
                if reliable { "reliable" } else { "lossy" }
            ));
        }
    }
    let state = app
        .try_state::<SessionState>()
        .ok_or_else(|| "session state is not available".to_string())?;
    let (room_connection, _identity) = state
        .inner()
        .control_channel_snapshot()
        .ok_or_else(|| "join a room before publishing plugin data".to_string())?;
    let packet = livekit::DataPacket {
        payload,
        topic: Some(topic),
        reliable,
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

fn state_limiter() -> &'static Mutex<RateLimiter> {
    static LIMITER: OnceLock<Mutex<RateLimiter>> = OnceLock::new();
    LIMITER.get_or_init(|| Mutex::new(RateLimiter::new(STATE_WRITES_PER_SECOND)))
}

/// Set or remove this participant's `plugins[<plugin_id>]` advertisement
/// (plugins/README.md §2.5). The frontend host owns the entry's content; this
/// re-validates its shape and the per-plugin state budget before merging it
/// into `ShareMetadata` (the total budget is enforced there).
#[tauri::command]
pub async fn plugin_set_state(
    app: AppHandle,
    plugin_id: String,
    entry: Option<serde_json::Value>,
) -> Result<(), String> {
    if !is_plugin_id(&plugin_id) {
        return Err(format!("invalid plugin id: {plugin_id}"));
    }
    let clean = match entry {
        None => None,
        Some(value) => Some(
            clean_plugin_entry(&value)
                .ok_or_else(|| "entry must be { v: <release version>, src: builtin|registry|dev, state?: <json <= 2 KB> }".to_string())?,
        ),
    };
    if !state_limiter()
        .lock()
        .map_err(|_| "limiter poisoned".to_string())?
        .try_take(&plugin_id)
    {
        return Err(format!("plugin {plugin_id} exceeded its state publish quota"));
    }
    let state = app
        .try_state::<SessionState>()
        .ok_or_else(|| "session state is not available".to_string())?;
    let (room_connection, _identity) = state
        .inner()
        .control_channel_snapshot()
        .ok_or_else(|| "join a room before publishing plugin state".to_string())?;
    room_connection.set_plugin_metadata_entry(&plugin_id, clean).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::time::Duration;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TopicVector {
        topic: String,
        plugin_id: Option<String>,
        sub: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Limits {
        max_payload_bytes: usize,
        lossy_per_second: f64,
        reliable_per_second: f64,
        inbound_per_sender_per_second: f64,
    }

    #[derive(Deserialize)]
    struct Topics {
        #[serde(rename = "pluginPrefix")]
        plugin_prefix: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DataEventFixture {
        name: String,
        fields: Vec<String>,
        example: serde_json::Map<String, serde_json::Value>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StateMetadataFixture {
        key: String,
        metadata: String,
        entries: serde_json::Map<String, serde_json::Value>,
        limits: serde_json::Map<String, serde_json::Value>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StateEventFixture {
        name: String,
        fields: Vec<String>,
        example: serde_json::Map<String, serde_json::Value>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        topics: Topics,
        plugin_topic_vectors: Vec<TopicVector>,
        plugin_limits: Limits,
        plugin_data_event: DataEventFixture,
        plugin_state_metadata: StateMetadataFixture,
        plugin_state_changed_event: StateEventFixture,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../../../../../contracts/petal-contracts.json")).unwrap()
    }

    #[test]
    fn topic_vectors_match_the_contract() {
        let f = fixture();
        assert_eq!(TOPIC_PREFIX, f.topics.plugin_prefix);
        for v in f.plugin_topic_vectors {
            let parsed = parse_topic(&v.topic);
            match v.plugin_id {
                None => assert!(parsed.is_none(), "should reject {}", v.topic),
                Some(id) => assert_eq!(
                    parsed,
                    Some(PluginTopic { plugin_id: id, sub: v.sub }),
                    "{}",
                    v.topic
                ),
            }
        }
    }

    #[test]
    fn limits_match_the_contract() {
        let l = fixture().plugin_limits;
        assert_eq!(MAX_PAYLOAD_BYTES, l.max_payload_bytes);
        assert_eq!(LOSSY_PER_SECOND, l.lossy_per_second);
        assert_eq!(RELIABLE_PER_SECOND, l.reliable_per_second);
        assert_eq!(INBOUND_PER_SENDER_PER_SECOND, l.inbound_per_sender_per_second);
    }

    #[test]
    fn event_matches_the_contract_example() {
        let f = fixture().plugin_data_event;
        assert_eq!(f.name, EVENT_NAME);
        let parsed = parse_topic("plugin/petal.reactions/emoji").unwrap();
        let payload = r#"{"e":"👍","t":1788640000000}"#.as_bytes();
        let event = event_for("plugin/petal.reactions/emoji", &parsed, "alex-1a2b", Some("Alex"), payload);
        let json = serde_json::to_value(&event).unwrap();
        let mut keys: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, f.fields);
        assert_eq!(json, serde_json::Value::Object(f.example));
    }

    #[test]
    fn sender_name_is_trimmed_and_optional() {
        let parsed = parse_topic("plugin/petal.chat").unwrap();
        assert_eq!(event_for("plugin/petal.chat", &parsed, "x", Some("  "), b"").sender_name, None);
        assert_eq!(
            event_for("plugin/petal.chat", &parsed, "x", Some(" Alex "), b"").sender_name,
            Some("Alex".into())
        );
    }

    #[test]
    fn topic_for_round_trips_and_rejects_bad_parts() {
        assert_eq!(topic_for("petal.reactions", None).unwrap(), "plugin/petal.reactions");
        assert_eq!(
            topic_for("petal.reactions", Some("emoji")).unwrap(),
            "plugin/petal.reactions/emoji"
        );
        assert!(topic_for("reactions", None).is_err());
        assert!(topic_for("petal.reactions", Some("Emoji")).is_err());
        assert!(topic_for("petal.reactions", Some("a/b")).is_err());
        assert!(!is_plugin_id(&"a.".repeat(40)));
    }

    #[test]
    fn validate_publish_enforces_size_shape_and_destinations() {
        let ok = validate_publish("petal.reactions", Some("emoji"), "AQID", &[]).unwrap();
        assert_eq!(ok, ("plugin/petal.reactions/emoji".to_string(), vec![1, 2, 3]));
        assert!(validate_publish("petal.reactions", None, "not base64!!", &[]).is_err());
        let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_PAYLOAD_BYTES + 1]);
        assert!(validate_publish("petal.reactions", None, &big, &[]).is_err());
        let exact = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_PAYLOAD_BYTES]);
        assert!(validate_publish("petal.reactions", None, &exact, &[]).is_ok());
        let many: Vec<String> = (0..MAX_DESTINATIONS + 1).map(|i| format!("p{i}")).collect();
        assert!(validate_publish("petal.reactions", None, "AQ==", &many).is_err());
        assert!(validate_publish("petal.reactions", None, "AQ==", &["".to_string()]).is_err());
    }

    #[test]
    fn plugins_metadata_matches_the_contract_example() {
        let f = fixture().plugin_state_metadata;
        assert_eq!(PLUGINS_METADATA_KEY, f.key);
        assert_eq!(plugins_from_metadata(&f.metadata), f.entries);
        assert_eq!(f.limits["perPluginStateBytes"], PER_PLUGIN_STATE_BYTES);
        assert_eq!(f.limits["totalBytes"], PLUGINS_TOTAL_BYTES);
        assert!(plugins_from_metadata("not json").is_empty());
        assert!(plugins_from_metadata(r#"{"plugins":[]}"#).is_empty());
    }

    #[test]
    fn state_changed_event_matches_the_contract_example() {
        let f = fixture();
        assert_eq!(f.plugin_state_changed_event.name, STATE_EVENT_NAME);
        let event = PluginStateChangedEvent {
            identity: "alex-1a2b".into(),
            plugins: plugins_from_metadata(&f.plugin_state_metadata.metadata),
        };
        let json = serde_json::to_value(&event).unwrap();
        let mut keys: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, f.plugin_state_changed_event.fields);
        assert_eq!(json, serde_json::Value::Object(f.plugin_state_changed_event.example));
    }

    #[test]
    fn clean_plugin_entry_enforces_shape_and_state_budget() {
        assert!(is_release_version("1.0.0"));
        assert!(!is_release_version("1.0"));
        assert!(!is_release_version("01.0.0"));
        assert!(!is_release_version("1.0.0-beta"));
        let ok = clean_plugin_entry(&serde_json::json!({"v":"1.0.0","src":"dev","state":{"a":1},"junk":true})).unwrap();
        assert_eq!(ok, serde_json::json!({"v":"1.0.0","src":"dev","state":{"a":1}}));
        let null_state = clean_plugin_entry(&serde_json::json!({"v":"1.0.0","src":"builtin","state":null})).unwrap();
        assert_eq!(null_state, serde_json::json!({"v":"1.0.0","src":"builtin"}));
        let big = "x".repeat(PER_PLUGIN_STATE_BYTES);
        let too_big = clean_plugin_entry(&serde_json::json!({"v":"1.0.0","src":"builtin","state":big})).unwrap();
        assert!(too_big.get("state").is_none(), "oversized state is dropped, not the whole entry");
        assert!(clean_plugin_entry(&serde_json::json!({"v":"1.0","src":"builtin"})).is_none());
        assert!(clean_plugin_entry(&serde_json::json!({"v":"1.0.0","src":"store"})).is_none());
        assert!(clean_plugin_entry(&serde_json::json!("nope")).is_none());
    }

    #[test]
    fn rate_limiter_bursts_then_refills_per_key() {
        let mut l = RateLimiter::new(10.0);
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(l.try_take_at("a", t0));
        }
        assert!(!l.try_take_at("a", t0));
        assert!(l.try_take_at("b", t0), "keys are independent");
        assert!(l.try_take_at("a", t0 + Duration::from_millis(100)));
        assert!(!l.try_take_at("a", t0 + Duration::from_millis(100)));
        for _ in 0..10 {
            assert!(l.try_take_at("a", t0 + Duration::from_secs(5)), "never above capacity");
        }
        assert!(!l.try_take_at("a", t0 + Duration::from_secs(5)));
        l.prune(t0 + Duration::from_secs(500), 120.0);
        assert!(l.buckets.is_empty());
    }
}
