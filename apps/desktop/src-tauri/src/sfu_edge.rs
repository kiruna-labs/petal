//! Which LiveKit edge and ICE path this client is on (#372, tracking #369).
//!
//! Supplies the `lk_region`, `self_hosted_sfu`, `same_edge`,
//! `ice_candidate_type` and `ice_protocol` dimensions of
//! `remote_control_latency_summary`, and `lk_region` on `meeting_joined`.
//! Pure classifiers are pinned by `contracts/petal-contracts.json`
//! (`lkEdgeTelemetry`); the TS mirror is `shared/logic/lkNodeHash.ts`.
//! The raw LiveKit node id never leaves this module: peers exchange only
//! `lkNodeHash` (8 hex chars of a salted SHA-256) on `request` / `active`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use livekit::webrtc::stats::{IceCandidateType, IceServerTransportProtocol, RtcStats};
use sha2::{Digest, Sha256};

use crate::rc_latency_telemetry::{self, Dimension, RcRole};
use crate::sync_ext::MutexExt;

const NODE_HASH_SALT: &str = "petal-lk-node:";
const NODE_HASH_HEX_LEN: usize = 8;
const UNKNOWN: &str = "unknown";

/// `lkNodeHash`: the first 8 hex chars of SHA-256(`"petal-lk-node:" + node_id`).
/// `None` for an empty node id (the server sent none).
pub(crate) fn lk_node_hash(node_id: &str) -> Option<String> {
    if node_id.is_empty() {
        return None;
    }
    let digest = Sha256::digest(format!("{NODE_HASH_SALT}{node_id}").as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(hex[..NODE_HASH_HEX_LEN].to_string())
}

/// A peer-supplied hash is attacker-controlled: only 8 lowercase hex chars count.
pub(crate) fn is_lk_node_hash(value: &str) -> bool {
    value.len() == NODE_HASH_HEX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `same_edge`: `unknown` unless both sides have a well-formed hash.
pub(crate) fn same_edge(local: Option<&str>, peer: Option<&str>) -> &'static str {
    match (
        local.filter(|v| is_lk_node_hash(v)),
        peer.filter(|v| is_lk_node_hash(v)),
    ) {
        (Some(local), Some(peer)) if local == peer => "true",
        (Some(_), Some(_)) => "false",
        _ => UNKNOWN,
    }
}

/// `self_hosted_sfu`: `false` only for a `*.livekit.cloud` host, `unknown`
/// when the URL has no parseable host.
pub(crate) fn self_hosted_sfu(url: &str) -> &'static str {
    let Some((_, rest)) = url.split_once("://") else {
        return UNKNOWN;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        UNKNOWN
    } else if host == "livekit.cloud" || host.ends_with(".livekit.cloud") {
        "false"
    } else {
        "true"
    }
}

/// `lk_region` token. Only a LiveKit Cloud edge reports a region worth
/// sending; a self-hosted region is operator config text, so it is `unknown`.
pub(crate) fn lk_region_token(region: &str, self_hosted: &str) -> String {
    if self_hosted != "false" {
        return UNKNOWN.to_string();
    }
    let token: String = region
        .trim()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    if token.bytes().any(|b| b.is_ascii_alphanumeric()) {
        token
    } else {
        UNKNOWN.to_string()
    }
}

/// This client's edge. `lk_region` is `None` when the server sent no server
/// info (omitted, never guessed); a self-hosted SFU reports `unknown`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalEdge {
    pub(crate) lk_region: Option<String>,
    pub(crate) self_hosted_sfu: &'static str,
    pub(crate) node_hash: Option<String>,
}

impl LocalEdge {
    pub(crate) fn new(info: Option<&livekit::ServerInfoSummary>, url: &str) -> Self {
        let self_hosted_sfu = self_hosted_sfu(url);
        Self {
            lk_region: info.map(|i| lk_region_token(&i.region, self_hosted_sfu)),
            self_hosted_sfu,
            node_hash: info.and_then(|i| lk_node_hash(&i.node_id)),
        }
    }
}

/// `same_edge` across every peer of a role's session (the host may hold
/// several concurrent grants, #374): `false` if any known peer is on another
/// node, else `unknown` if any hash is missing, else `true`.
pub(crate) fn same_edge_all<'a>(
    local: Option<&str>,
    peers: impl IntoIterator<Item = Option<&'a str>>,
) -> &'static str {
    let mut verdict = None;
    for peer in peers {
        match same_edge(local, peer) {
            "false" => return "false",
            "unknown" => verdict = Some(UNKNOWN),
            _ => verdict = verdict.or(Some("true")),
        }
    }
    verdict.unwrap_or(UNKNOWN)
}

/// Worst observed local ICE path, each dimension independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IcePath {
    pub(crate) candidate_type: &'static str,
    pub(crate) protocol: &'static str,
}

fn candidate_type_rank(value: &str) -> u8 {
    match value {
        "host" => 1,
        "srflx" => 2,
        "prflx" => 3,
        "relay" => 4,
        _ => 0,
    }
}

fn protocol_rank(value: &str) -> u8 {
    match value {
        "udp" => 1,
        "tcp" => 2,
        _ => 0,
    }
}

/// The selected pair's local candidate on one peer connection: the
/// transport's `selectedCandidatePairId`, else a nominated succeeded pair.
/// A relay's latency-relevant protocol is the client-to-TURN leg.
pub(crate) fn selected_ice_path(stats: &[RtcStats]) -> Option<IcePath> {
    use livekit::webrtc::stats::IceCandidatePairState;
    let pairs: HashMap<&str, &livekit::webrtc::stats::dictionaries::CandidatePairStats> = stats
        .iter()
        .filter_map(|s| match s {
            RtcStats::CandidatePair(p) => Some((p.rtc.id.as_str(), &p.candidate_pair)),
            _ => None,
        })
        .collect();
    let selected = stats
        .iter()
        .find_map(|s| match s {
            RtcStats::Transport(t) => pairs
                .get(t.transport.selected_candidate_pair_id.as_str())
                .copied(),
            _ => None,
        })
        .or_else(|| {
            pairs.values().copied().find(|p| {
                p.nominated && p.state == Some(IceCandidatePairState::Succeeded)
            })
        })?;
    let local = stats.iter().find_map(|s| match s {
        RtcStats::LocalCandidate(c) if c.rtc.id == selected.local_candidate_id => {
            Some(&c.local_candidate)
        }
        _ => None,
    })?;
    let candidate_type = match local.candidate_type {
        Some(IceCandidateType::Host) => "host",
        Some(IceCandidateType::Srflx) => "srflx",
        Some(IceCandidateType::Prflx) => "prflx",
        Some(IceCandidateType::Relay) => "relay",
        None => UNKNOWN,
    };
    let protocol = match (local.candidate_type, local.relay_protocol) {
        (Some(IceCandidateType::Relay), Some(IceServerTransportProtocol::Udp)) => "udp",
        (Some(IceCandidateType::Relay), Some(_)) => "tcp",
        _ => match local.protocol.to_ascii_lowercase().as_str() {
            "udp" => "udp",
            "tcp" => "tcp",
            _ => UNKNOWN,
        },
    };
    Some(IcePath {
        candidate_type,
        protocol,
    })
}

/// Worst of the publisher and subscriber paths, per dimension: a relay on
/// either PC, or TCP on either, is what adds latency. `None` = no data.
pub(crate) fn worst_ice_path(paths: impl IntoIterator<Item = Option<IcePath>>) -> Option<IcePath> {
    paths.into_iter().flatten().reduce(|a, b| IcePath {
        candidate_type: if candidate_type_rank(b.candidate_type) > candidate_type_rank(a.candidate_type) {
            b.candidate_type
        } else {
            a.candidate_type
        },
        protocol: if protocol_rank(b.protocol) > protocol_rank(a.protocol) {
            b.protocol
        } else {
            a.protocol
        },
    })
}

/// `NotSampled` omits the ICE dimensions; `Unknown` (no selected pair, or a
/// reconnect awaiting its first sample) replaces a stale path with `unknown`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum IceState {
    #[default]
    NotSampled,
    Unknown,
    Known(IcePath),
}

/// The room this state describes: `key` is its address (identity only,
/// never dereferenced), `weak` reads its live server info.
struct RoomRef {
    key: usize,
    weak: std::sync::Weak<livekit::Room>,
    url: String,
}

fn room_key(room: &livekit::Room) -> usize {
    room as *const livekit::Room as usize
}

#[derive(Default)]
struct EdgeState {
    /// The connected room, weak so this module never extends its life
    /// (#883). Re-read on every use: a full reconnect lands on a new signal
    /// session whose join response may name another node.
    room: Option<RoomRef>,
    local: Option<LocalEdge>,
    ice: IceState,
    /// Bumped on every connect, reconnect and leave, so an ICE sample begun
    /// on an older connection is discarded (`note_ice_path_for`).
    generation: u64,
    /// Latest `lkNodeHash` seen from each remote identity (`request` on the
    /// host, `active` on the controller). Only validated hashes are stored.
    peer_hashes: HashMap<String, String>,
    /// Every peer of each role's current control session.
    session_peers: [Vec<String>; 2],
}

impl EdgeState {
    /// Whether `room` is the connection this state describes; a task from
    /// an older connection must never write into a newer one's state.
    fn is_current(&self, key: usize) -> bool {
        self.room.as_ref().is_some_and(|room| room.key == key)
    }

    fn refresh_local(&mut self) {
        if let Some(room) = &self.room {
            if let Some(live) = room.weak.upgrade() {
                self.local = Some(LocalEdge::new(live.server_info().as_ref(), &room.url));
            }
        }
    }

    /// A (re)connect of room `key`: a new generation, and a path stamped on
    /// the old peer connections reads `unknown` until resampled.
    fn begin_connection(&mut self, room: RoomRef) {
        self.room = Some(room);
        self.generation += 1;
        if self.ice != IceState::NotSampled {
            self.ice = IceState::Unknown;
        }
    }

    /// Room `key` left. False (and untouched) for an older room's late leave.
    fn leave(&mut self, key: usize) -> bool {
        if !self.is_current(key) {
            return false;
        }
        *self = EdgeState {
            generation: self.generation + 1,
            ..EdgeState::default()
        };
        true
    }

    /// The generation an ICE sample of room `key` must carry, `None` for an
    /// older room's poller.
    fn sample_generation(&self, key: usize) -> Option<u64> {
        self.is_current(key).then_some(self.generation)
    }

    /// Accept a sample begun at `generation`; false for a stale one.
    fn accept_ice(&mut self, generation: u64, path: Option<IcePath>) -> bool {
        if self.generation != generation {
            return false;
        }
        let next = path.map_or(IceState::Unknown, IceState::Known);
        if self.ice != next {
            log::info!(
                "sfu-edge: ice_candidate_type={} ice_protocol={}",
                path.map_or(UNKNOWN, |p| p.candidate_type),
                path.map_or(UNKNOWN, |p| p.protocol)
            );
            self.ice = next;
        }
        true
    }
}

fn state() -> &'static Mutex<EdgeState> {
    static STATE: OnceLock<Mutex<EdgeState>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn role_slot(role: RcRole) -> usize {
    match role {
        RcRole::Host => 0,
        RcRole::Controller => 1,
    }
}

/// Every edge dimension for one role's session: `Some` to set, `None` to
/// clear (missing data is omitted, and a value from an earlier connection
/// must not outlive it). `same_edge` says `unknown` once the local edge is known.
fn dimensions_for(state: &EdgeState, role: RcRole) -> Vec<(Dimension, Option<String>)> {
    let local = state.local.as_ref();
    let peers = state.session_peers[role_slot(role)]
        .iter()
        .map(|p| state.peer_hashes.get(p).map(String::as_str));
    let ice = match state.ice {
        IceState::NotSampled => None,
        IceState::Unknown => Some((UNKNOWN, UNKNOWN)),
        IceState::Known(path) => Some((path.candidate_type, path.protocol)),
    };
    vec![
        (Dimension::LkRegion, local.and_then(|l| l.lk_region.clone())),
        (Dimension::SelfHostedSfu, local.map(|l| l.self_hosted_sfu.to_string())),
        (
            Dimension::SameEdge,
            local.map(|l| same_edge_all(l.node_hash.as_deref(), peers).to_string()),
        ),
        (Dimension::IceCandidateType, ice.map(|(t, _)| t.to_string())),
        (Dimension::IceProtocol, ice.map(|(_, p)| p.to_string())),
    ]
}

fn apply_with(state: &EdgeState, role: RcRole, mut write: impl FnMut(Dimension, Option<&str>)) {
    if state.session_peers[role_slot(role)].is_empty() {
        return;
    }
    for (dimension, value) in dimensions_for(state, role) {
        write(dimension, value.as_deref());
    }
}

fn apply_locked(state: &EdgeState, role: RcRole) {
    apply_with(state, role, |dimension, value| match value {
        Some(value) => {
            rc_latency_telemetry::set_dimension(role, dimension, value);
        }
        None => rc_latency_telemetry::clear_dimension(role, dimension),
    });
}

fn apply_all_locked(state: &EdgeState) {
    apply_locked(state, RcRole::Host);
    apply_locked(state, RcRole::Controller);
}

/// Record a remote identity's `lkNodeHash` (malformed or absent clears it).
pub(crate) fn note_peer_node_hash(identity: &str, hash: Option<&str>) {
    let mut state = state().lock_unpoisoned();
    match hash.filter(|h| is_lk_node_hash(h)) {
        Some(hash) => {
            state.peer_hashes.insert(identity.to_string(), hash.to_string());
        }
        None => {
            state.peer_hashes.remove(identity);
        }
    }
}

/// The set of peers `role` currently controls or is controlled by changed
/// (grant, revoke, activation, stop). Re-stamps the session's dimensions;
/// call after `sync(role, true)`. An empty set only forgets the peers.
pub(crate) fn set_session_peers(role: RcRole, mut peers: Vec<String>) {
    peers.sort();
    peers.dedup();
    let mut state = state().lock_unpoisoned();
    state.refresh_local();
    state.session_peers[role_slot(role)] = peers;
    apply_locked(&state, role);
}

/// The local `lkNodeHash` for an outgoing `request` / `active` status, read
/// from the live room so a status re-sent right after a reconnect carries
/// the new node, not the one cached before it.
pub(crate) fn local_node_hash() -> Option<String> {
    let mut state = state().lock_unpoisoned();
    state.refresh_local();
    state.local.as_ref()?.node_hash.clone()
}

/// `lk_region` for `meeting_joined`; `None` when no server info was read.
pub(crate) fn local_lk_region() -> Option<String> {
    let mut state = state().lock_unpoisoned();
    state.refresh_local();
    state.local.as_ref()?.lk_region.clone()
}

/// Connect or reconnect: re-read the server info and re-stamp any active
/// session. A reconnect may have rebuilt the peer connections, so the ICE
/// path is dropped until the next sample.
pub(crate) fn note_room_connected(room: &std::sync::Arc<livekit::Room>, url: &str) {
    let mut state = state().lock_unpoisoned();
    state.begin_connection(RoomRef {
        key: room_key(room),
        weak: std::sync::Arc::downgrade(room),
        url: url.to_string(),
    });
    state.refresh_local();
    if let Some(local) = &state.local {
        log::info!(
            "sfu-edge: lk_region={} self_hosted_sfu={} lk_node_hash={}",
            local.lk_region.as_deref().unwrap_or("none"),
            local.self_hosted_sfu,
            local.node_hash.as_deref().unwrap_or("none")
        );
    }
    apply_all_locked(&state);
}

/// `room` left: nothing from it may label the next connection. A late
/// `Disconnected` from an older room leaves a newer room's state alone.
pub(crate) fn note_room_left(room: &livekit::Room) {
    state().lock_unpoisoned().leave(room_key(room));
}

/// An ICE sample taken on connection `generation`. Re-stamps active
/// sessions on every sample, not only on change: `set_dimension` never
/// blocks and can drop a write under contention, so the next sample repairs
/// it. `None` (no selected pair) records `unknown`.
fn note_ice_path_for(generation: u64, path: Option<IcePath>) {
    let mut state = state().lock_unpoisoned();
    if state.accept_ice(generation, path) {
        apply_all_locked(&state);
    }
}

/// Sample both peer connections' selected candidate pairs. Bounded: the
/// caller is the diagnostics poller, which must not stall on a wedged PC.
pub(crate) async fn sample_ice_path(room: &livekit::Room) {
    let Some(generation) = state().lock_unpoisoned().sample_generation(room_key(room)) else {
        return;
    };
    let stats = tokio::time::timeout(std::time::Duration::from_secs(2), room.get_stats()).await;
    let Ok(Ok(stats)) = stats else {
        return;
    };
    note_ice_path_for(
        generation,
        worst_ice_path([
            selected_ice_path(&stats.publisher_stats),
            selected_ice_path(&stats.subscriber_stats),
        ]),
    );
}

#[cfg(test)]
pub(crate) fn set_local_for_test(local: Option<LocalEdge>, ice: Option<IcePath>) {
    let mut state = state().lock_unpoisoned();
    state.local = local;
    state.ice = ice.map_or(IceState::NotSampled, IceState::Known);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const RAW_CONTRACT: &str = include_str!("../../../../contracts/petal-contracts.json");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Contract {
        lk_edge_telemetry: EdgeContract,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct EdgeContract {
        node_hash_salt: String,
        node_hash_vectors: Vec<HashVector>,
        same_edge: Vec<SameEdgeVector>,
        self_hosted_sfu: Vec<SelfHostedVector>,
        region_tokens: Vec<RegionVector>,
        ice_path: Vec<IceVector>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct HashVector {
        node_id: String,
        lk_node_hash: Option<String>,
    }

    #[derive(Deserialize)]
    struct SameEdgeVector {
        local: Option<String>,
        peer: Option<String>,
        expected: String,
    }

    #[derive(Deserialize)]
    struct SelfHostedVector {
        url: String,
        expected: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RegionVector {
        region: String,
        self_hosted_sfu: String,
        expected: String,
    }

    #[derive(Deserialize)]
    struct IceVector {
        name: String,
        publisher: Vec<serde_json::Value>,
        subscriber: Vec<serde_json::Value>,
        expected: Option<IceExpected>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IceExpected {
        candidate_type: String,
        protocol: String,
    }

    fn contract() -> EdgeContract {
        serde_json::from_str::<Contract>(RAW_CONTRACT)
            .expect("contract parses")
            .lk_edge_telemetry
    }

    fn rtc_stats(values: &[serde_json::Value]) -> Vec<RtcStats> {
        values
            .iter()
            .map(|v| serde_json::from_value(v.clone()).expect("fixture stat parses as RtcStats"))
            .collect()
    }

    #[test]
    fn node_hash_matches_the_shared_vectors() {
        let contract = contract();
        assert_eq!(contract.node_hash_salt, NODE_HASH_SALT);
        assert!(contract.node_hash_vectors.len() >= 4);
        for vector in contract.node_hash_vectors {
            assert_eq!(lk_node_hash(&vector.node_id), vector.lk_node_hash, "{}", vector.node_id);
            if let Some(hash) = lk_node_hash(&vector.node_id) {
                assert!(is_lk_node_hash(&hash));
                assert!(!hash.contains(&vector.node_id));
            }
        }
    }

    #[test]
    fn same_edge_truth_table_matches_the_contract() {
        let vectors = contract().same_edge;
        assert!(vectors.iter().any(|v| v.expected == "true"));
        assert!(vectors.iter().any(|v| v.expected == "false"));
        assert!(vectors.iter().any(|v| v.expected == "unknown" && v.local.is_none()));
        assert!(vectors.iter().any(|v| v.expected == "unknown" && v.peer.is_none()));
        for v in vectors {
            assert_eq!(
                same_edge(v.local.as_deref(), v.peer.as_deref()),
                v.expected,
                "{:?} vs {:?}",
                v.local,
                v.peer
            );
        }
    }

    /// Concurrent grants (#374): one known cross-edge peer makes it `false`.
    #[test]
    fn same_edge_across_concurrent_peers() {
        let a = Some("69e94d0a");
        let b = Some("e4c80fa5");
        assert_eq!(same_edge_all(a, [a, a]), "true");
        assert_eq!(same_edge_all(a, [a, b]), "false");
        assert_eq!(same_edge_all(a, [None, b]), "false");
        assert_eq!(same_edge_all(a, [a, None]), "unknown");
        assert_eq!(same_edge_all(a, []), "unknown");
        assert_eq!(same_edge_all(None, [a]), "unknown");
    }

    #[test]
    fn self_hosted_sfu_classification_matches_the_contract() {
        for v in contract().self_hosted_sfu {
            assert_eq!(self_hosted_sfu(&v.url), v.expected, "{}", v.url);
        }
    }

    #[test]
    fn region_tokens_match_the_contract_and_the_dimension_rule() {
        for v in contract().region_tokens {
            let token = lk_region_token(&v.region, &v.self_hosted_sfu);
            assert_eq!(token, v.expected, "{:?}/{}", v.region, v.self_hosted_sfu);
            assert!(Dimension::LkRegion.value_allowed(&token), "{token}");
        }
    }

    #[test]
    fn ice_path_classification_matches_the_canned_stats() {
        let vectors = contract().ice_path;
        assert!(vectors.iter().any(|v| v.name == "relay-tcp-beats-srflx-udp"));
        for v in vectors {
            let path = worst_ice_path([
                selected_ice_path(&rtc_stats(&v.publisher)),
                selected_ice_path(&rtc_stats(&v.subscriber)),
            ]);
            let expected = v.expected.as_ref().map(|e| (e.candidate_type.as_str(), e.protocol.as_str()));
            assert_eq!(path.map(|p| (p.candidate_type, p.protocol)), expected, "{}", v.name);
            if let Some(path) = path {
                assert!(Dimension::IceCandidateType.value_allowed(path.candidate_type));
                assert!(Dimension::IceProtocol.value_allowed(path.protocol));
            }
        }
    }

    fn info(region: &str, node: &str) -> livekit::ServerInfoSummary {
        livekit::ServerInfoSummary {
            region: region.into(),
            node_id: node.into(),
        }
    }

    fn room_ref(key: usize) -> RoomRef {
        RoomRef {
            key,
            weak: std::sync::Weak::new(),
            url: "wss://petal-x.livekit.cloud".into(),
        }
    }

    fn dim(state: &EdgeState, wanted: Dimension) -> Option<String> {
        dimensions_for(state, RcRole::Host)
            .into_iter()
            .find(|(d, _)| *d == wanted)
            .and_then(|(_, v)| v)
    }

    /// A no-pair sample says `unknown`; a never-sampled connection omits ICE.
    #[test]
    fn ice_dimensions_follow_the_sample_state() {
        let mut state = EdgeState::default();
        assert_eq!(dim(&state, Dimension::IceCandidateType), None);
        state.ice = IceState::Unknown;
        assert_eq!(dim(&state, Dimension::IceCandidateType).as_deref(), Some("unknown"));
        assert_eq!(dim(&state, Dimension::IceProtocol).as_deref(), Some("unknown"));
        state.ice = IceState::Known(IcePath {
            candidate_type: "relay",
            protocol: "tcp",
        });
        assert_eq!(dim(&state, Dimension::IceCandidateType).as_deref(), Some("relay"));
    }

    /// A reconnect whose server sends no info clears the old edge's region
    /// from the running session instead of leaving it stale.
    #[test]
    fn reconnect_without_server_info_clears_the_session_region() {
        let aggregator = rc_latency_telemetry::Aggregator::new(RcRole::Host);
        let now = std::time::Instant::now();
        aggregator.sync_at(true, now);
        let write = |dimension: Dimension, value: Option<&str>| match value {
            Some(value) => {
                aggregator.set_dimension(dimension, value);
            }
            None => aggregator.clear_dimension(dimension),
        };
        let url = "wss://petal-x.livekit.cloud";
        let mut state = EdgeState {
            local: Some(LocalEdge::new(Some(&info("us-east-1", "ND_7xKqM2pLz9Rb")), url)),
            ..EdgeState::default()
        };
        state.session_peers[0] = vec!["controller".into()];
        apply_with(&state, RcRole::Host, write);
        state.local = Some(LocalEdge::new(None, url));
        apply_with(&state, RcRole::Host, write);
        aggregator.record_stage(rc_latency_telemetry::Stage::ReplayInjectMs, 5.0);
        let summary = aggregator
            .sync_at(true, now + rc_latency_telemetry::EMIT_INTERVAL)
            .expect("interval with a sample emits");
        let line = summary.log_line();
        assert!(!line.contains("lk_region="), "{line}");
        assert!(line.contains("self_hosted_sfu=false") && line.contains("same_edge=unknown"), "{line}");
    }

    /// Round-3 races: an older room's ICE sample and its late `Disconnected`
    /// never write into the newer room's state.
    #[test]
    fn an_older_room_never_writes_into_the_newer_rooms_state() {
        let relay = Some(IcePath {
            candidate_type: "relay",
            protocol: "tcp",
        });
        let mut state = EdgeState::default();
        state.begin_connection(room_ref(1));
        let stale = state.sample_generation(1).expect("room 1 is current");
        state.begin_connection(room_ref(2));
        // Room 1's poller can no longer start a sample, and one it started
        // before the switch is discarded.
        assert_eq!(state.sample_generation(1), None);
        assert!(!state.accept_ice(stale, relay));
        assert_eq!(state.ice, IceState::NotSampled);
        // A sample begun before room 2's own reconnect is stale too.
        let before = state.sample_generation(2).unwrap();
        state.begin_connection(room_ref(2));
        assert!(!state.accept_ice(before, relay));
        let current = state.sample_generation(2).unwrap();
        assert!(state.accept_ice(current, relay));
        assert_eq!(state.ice, IceState::Known(relay.unwrap()));
        // Room 1's late leave leaves room 2 alone; room 2's own leave clears.
        state.peer_hashes.insert("peer".into(), "69e94d0a".into());
        assert!(!state.leave(1));
        assert!(state.is_current(2) && !state.peer_hashes.is_empty());
        assert!(state.leave(2));
        assert!(state.room.is_none() && state.peer_hashes.is_empty());
        assert!(!state.accept_ice(current, relay), "a sample from before the leave");
    }

    #[test]
    fn local_edge_never_carries_the_raw_node_id() {
        let info = info("us-east-1", "ND_7xKqM2pLz9Rb");
        let edge = LocalEdge::new(Some(&info), "wss://petal-x.livekit.cloud");
        assert_eq!(edge.lk_region.as_deref(), Some("us-east-1"));
        assert_eq!(edge.self_hosted_sfu, "false");
        assert_eq!(edge.node_hash.as_deref(), Some("69e94d0a"));
        assert!(!format!("{edge:?}").contains("ND_7xKqM2pLz9Rb"));

        let local = LocalEdge::new(Some(&info), "ws://localhost:7880");
        assert_eq!((local.lk_region.as_deref(), local.self_hosted_sfu), (Some("unknown"), "true"));
        // No server info: region omitted, not guessed.
        let none = LocalEdge::new(None, "wss://petal-x.livekit.cloud");
        assert_eq!((none.lk_region, none.node_hash), (None, None));
    }
}
