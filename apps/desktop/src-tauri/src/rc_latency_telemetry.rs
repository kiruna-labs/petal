//! Per-control-session remote-control latency aggregator (#370, tracking #369).
//!
//! Stage/dimension keys, histogram vectors and the summary log line are
//! pinned by `contracts/petal-contracts.json` (`latencyHistogramVectors`,
//! `remoteControlLatencySummary`). What lands with its first producer, not
//! here: the TS mirror (#375), the Sentry degraded diagnostic (#371/#376).
//!
//! Producers call only [`record_stage`], [`add_counter`], [`raise_max`] and
//! [`set_dimension`]. The owner
//! calls [`sync`] on a timer with the current "is control active" truth.
//! Each interval with samples becomes one `remote_control_latency_summary`
//! PostHog event and one `remote-control-latency-summary:` log line.
//! Integer-ms aggregates only: no identities, titles, coordinates or key
//! data ever enter this module.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

pub(crate) const HISTOGRAM_MAX_MS: f64 = 5000.0;
const HISTOGRAM_GROWTH: f64 = 1.05;
pub(crate) const EMIT_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const FINAL_MIN_SAMPLES: u32 = 5;
pub(crate) const MAX_EVENTS_PER_SESSION: u32 = 120;
pub(crate) const LOG_PREFIX: &str = "remote-control-latency-summary:";

/// Upper edges of every bucket; bucket `j` is `[edges[j-1], edges[j])` with
/// `edges[-1] = 0`. Built by repeated multiplication (correctly rounded IEEE
/// ops), so a TS mirror can reproduce the layout bit-for-bit.
fn bucket_edges() -> &'static [f64] {
    static EDGES: OnceLock<Vec<f64>> = OnceLock::new();
    EDGES.get_or_init(|| {
        let mut edges = Vec::new();
        let mut edge = 1.0_f64;
        while edge < HISTOGRAM_MAX_MS {
            edges.push(edge);
            edge *= HISTOGRAM_GROWTH;
        }
        edges.push(HISTOGRAM_MAX_MS);
        edges
    })
}

fn bucket_index(ms: f64) -> usize {
    let edges = bucket_edges();
    edges.partition_point(|edge| *edge <= ms).min(edges.len() - 1)
}

fn bucket_representative(index: usize) -> f64 {
    let edges = bucket_edges();
    if index == 0 {
        0.5
    } else {
        (edges[index - 1] * edges[index]).sqrt()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistogramStats {
    pub(crate) p50: u32,
    pub(crate) p95: u32,
    pub(crate) max: u32,
    pub(crate) n: u32,
}

/// Fixed log-spaced histogram over 0-5000 ms (~5% resolution). Recording
/// never allocates: the bucket array is sized once at construction.
#[derive(Debug, Clone)]
pub(crate) struct LatencyHistogram {
    counts: Vec<u32>,
    n: u32,
    rejected: u32,
    min: f64,
    max: f64,
}

impl LatencyHistogram {
    pub(crate) fn new() -> Self {
        Self {
            counts: vec![0; bucket_edges().len()],
            n: 0,
            rejected: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// False (counted as rejected) for a negative or non-finite value.
    /// Values above 5000 ms clamp to 5000.
    pub(crate) fn record(&mut self, ms: f64) -> bool {
        if !ms.is_finite() || ms < 0.0 {
            self.rejected = self.rejected.saturating_add(1);
            return false;
        }
        let clamped = ms.min(HISTOGRAM_MAX_MS);
        let index = bucket_index(clamped);
        self.counts[index] = self.counts[index].saturating_add(1);
        self.n = self.n.saturating_add(1);
        self.min = self.min.min(clamped);
        self.max = self.max.max(clamped);
        true
    }

    pub(crate) fn n(&self) -> u32 {
        self.n
    }

    #[cfg(test)]
    fn rejected(&self) -> u32 {
        self.rejected
    }

    /// Nearest-rank `ceil(n*p) - 1`, matching `remote-control-latency-verdict.mjs`.
    pub(crate) fn percentile(&self, fraction: f64) -> Option<u32> {
        if self.n == 0 {
            return None;
        }
        let rank = (((self.n as f64) * fraction).ceil() as i64 - 1).max(0) as u64;
        let mut cumulative = 0u64;
        for (index, count) in self.counts.iter().enumerate() {
            cumulative += u64::from(*count);
            if cumulative > rank {
                let value = bucket_representative(index).clamp(self.min, self.max);
                return Some(value.round() as u32);
            }
        }
        Some(self.max.round() as u32)
    }

    pub(crate) fn max(&self) -> Option<u32> {
        (self.n > 0).then(|| self.max.round() as u32)
    }

    pub(crate) fn stats(&self) -> Option<HistogramStats> {
        Some(HistogramStats {
            p50: self.percentile(0.5)?,
            p95: self.percentile(0.95)?,
            max: self.max()?,
            n: self.n,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.counts.iter_mut().for_each(|count| *count = 0);
        self.n = 0;
        self.rejected = 0;
        self.min = f64::INFINITY;
        self.max = f64::NEG_INFINITY;
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RcRole {
    Host,
    /// #371: the controlling side; today only `input_rtt_ms`.
    Controller,
}

impl RcRole {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Controller => "controller",
        }
    }
}

macro_rules! closed_keys {
    ($name:ident { $($variant:ident => $key:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(crate) enum $name { $($variant),+ }
        impl $name {
            pub(crate) const ALL: &'static [$name] = &[$(Self::$variant),+];
            pub(crate) fn key(self) -> &'static str {
                match self { $(Self::$variant => $key),+ }
            }
            fn index(self) -> usize {
                self as usize
            }
        }
    };
}

closed_keys!(Stage {
    InputLegMs => "input_leg_ms",
    InputLegLossyMs => "input_leg_lossy_ms",
    InputRttMs => "input_rtt_ms",
    InputExcessDelayMs => "input_excess_delay_ms",
    ReplayInjectMs => "replay_inject_ms",
    ReplayCompleteMs => "replay_complete_ms",
    InjectToCaptureMs => "inject_to_capture_ms",
    CaptureIntervalMs => "capture_interval_ms",
    CaptureToEncodedMs => "capture_to_encoded_ms",
    EncodedToPacketizedMs => "encoded_to_packetized_ms",
    PubRttMs => "pub_rtt_ms",
    SubRttMs => "sub_rtt_ms",
    VideoNetMs => "video_net_ms",
    JbDelayMs => "jb_delay_ms",
    JbTargetMs => "jb_target_ms",
    DecodeMs => "decode_ms",
    ProcessingMs => "processing_ms",
    ReceiveToPresentMs => "receive_to_present_ms",
    PhotonMs => "photon_ms",
});

closed_keys!(Dimension {
    LkRegion => "lk_region",
    SameEdge => "same_edge",
    SelfHostedSfu => "self_hosted_sfu",
    IceCandidateType => "ice_candidate_type",
    IceProtocol => "ice_protocol",
    WindowCodec => "window_codec",
    ContentChange => "content_change",
});

const DIMENSION_COUNT: usize = 7;

// Sum-per-interval counters. A zero counter is never emitted.
closed_keys!(Counter {
    InputStallCount => "input_stall_count",
});

// Max-per-interval values that qualify the stage samples recorded with them.
// Emitted whenever set, 0 included, but never make an interval emit alone.
closed_keys!(MaxCounter {
    OffsetErrMs => "offset_err_ms",
});

impl Dimension {
    /// `None` = free-form token (lowercase `[a-z0-9_-]`, at most 32 chars).
    pub(crate) fn allowed_values(self) -> Option<&'static [&'static str]> {
        match self {
            Self::LkRegion => None,
            Self::SameEdge | Self::SelfHostedSfu => Some(&["true", "false", "unknown"]),
            Self::IceCandidateType => Some(&["host", "srflx", "prflx", "relay", "unknown"]),
            Self::IceProtocol => Some(&["udp", "tcp", "unknown"]),
            Self::WindowCodec => Some(&["h264", "h265", "vp8", "vp9", "av1", "unknown"]),
            Self::ContentChange => Some(&["detected", "unknown"]),
        }
    }

    pub(crate) fn value_allowed(self, value: &str) -> bool {
        match self.allowed_values() {
            Some(allowed) => allowed.contains(&value),
            None => {
                (1..=32).contains(&value.len())
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            }
        }
    }
}

/// One emitted interval. `stages` holds only stages with n > 0, `counters`
/// only non-zero counters, `dimensions` only what a producer actually set
/// this session (missing data is omitted, never sent as 0 or a guess).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) role: RcRole,
    pub(crate) rc_session_id: String,
    pub(crate) interval_s: u64,
    pub(crate) is_final: bool,
    pub(crate) stages: Vec<(Stage, HistogramStats)>,
    pub(crate) counters: Vec<(Counter, u64)>,
    pub(crate) max_counters: Vec<(MaxCounter, u64)>,
    pub(crate) dimensions: Vec<(Dimension, String)>,
}

impl Summary {
    /// PostHog properties for `remote_control_latency_summary`, in the same
    /// order as the log line. Common properties are added by `analytics.rs`.
    pub(crate) fn properties(&self) -> Vec<(String, serde_json::Value)> {
        let mut out = vec![
            ("role".to_string(), self.role.key().into()),
            ("rc_session_id".to_string(), self.rc_session_id.clone().into()),
            ("interval_s".to_string(), self.interval_s.into()),
            ("final".to_string(), self.is_final.into()),
        ];
        for (stage, stats) in &self.stages {
            if stats.n == 0 {
                continue;
            }
            let key = stage.key();
            out.push((format!("{key}_p50"), stats.p50.into()));
            out.push((format!("{key}_p95"), stats.p95.into()));
            out.push((format!("{key}_max"), stats.max.into()));
            out.push((format!("{key}_n"), stats.n.into()));
        }
        for (counter, value) in &self.counters {
            if *value > 0 {
                out.push((counter.key().to_string(), (*value).into()));
            }
        }
        for (counter, value) in &self.max_counters {
            out.push((counter.key().to_string(), (*value).into()));
        }
        for (dimension, value) in &self.dimensions {
            out.push((dimension.key().to_string(), value.clone().into()));
        }
        out
    }

    pub(crate) fn log_line(&self) -> String {
        let mut line = LOG_PREFIX.to_string();
        for (key, value) in self.properties() {
            line.push(' ');
            line.push_str(&key);
            line.push('=');
            match value {
                serde_json::Value::String(text) => line.push_str(&text),
                other => line.push_str(&other.to_string()),
            }
        }
        line
    }
}

struct Session {
    rc_session_id: String,
    interval_started_at: Instant,
    events_emitted: u32,
    stages: Vec<LatencyHistogram>,
    counters: Vec<u64>,
    max_counters: Vec<Option<u64>>,
    dimensions: [Option<String>; DIMENSION_COUNT],
}

impl Session {
    fn new(now: Instant) -> Self {
        Self {
            rc_session_id: new_rc_session_id(),
            interval_started_at: now,
            events_emitted: 0,
            stages: Stage::ALL.iter().map(|_| LatencyHistogram::new()).collect(),
            counters: vec![0; Counter::ALL.len()],
            max_counters: vec![None; MaxCounter::ALL.len()],
            dimensions: Default::default(),
        }
    }

    fn has_counts(&self) -> bool {
        self.counters.iter().any(|value| *value > 0)
    }

    /// Drain the current interval. `None` when it held no samples and no
    /// non-zero counter (an empty interval never emits or spends the cap) or
    /// the per-session cap is spent. Counters reset to 0, never "present 0":
    /// that reset kept empty intervals emitting all-zero events (#371).
    fn take_interval(&mut self, role: RcRole, now: Instant, is_final: bool) -> Option<Summary> {
        let stages: Vec<(Stage, HistogramStats)> = Stage::ALL
            .iter()
            .filter_map(|stage| Some((*stage, self.stages[stage.index()].stats()?)))
            .collect();
        let counters: Vec<(Counter, u64)> = Counter::ALL
            .iter()
            .map(|counter| (*counter, self.counters[counter.index()]))
            .filter(|(_, value)| *value > 0)
            .collect();
        let max_counters: Vec<(MaxCounter, u64)> = MaxCounter::ALL
            .iter()
            .filter_map(|counter| Some((*counter, self.max_counters[counter.index()]?)))
            .collect();
        let elapsed_ms = now.saturating_duration_since(self.interval_started_at).as_millis() as u64;
        self.interval_started_at = now;
        self.stages.iter_mut().for_each(LatencyHistogram::reset);
        self.counters.iter_mut().for_each(|value| *value = 0);
        self.max_counters.iter_mut().for_each(|value| *value = None);
        if (stages.is_empty() && counters.is_empty())
            || self.events_emitted >= MAX_EVENTS_PER_SESSION
        {
            return None;
        }
        self.events_emitted += 1;
        Some(Summary {
            role,
            rc_session_id: self.rc_session_id.clone(),
            interval_s: (elapsed_ms + 500) / 1000,
            is_final,
            stages,
            counters,
            max_counters,
            dimensions: Dimension::ALL
                .iter()
                .filter_map(|d| Some((*d, self.dimensions[d.index()].clone()?)))
                .collect(),
        })
    }
}

fn new_rc_session_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes.copy_from_slice(&nanos.to_le_bytes());
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One role's aggregator. The hot path (`record_stage`) only `try_lock`s:
/// on contention the sample is dropped and counted, never waited for.
pub(crate) struct Aggregator {
    role: RcRole,
    session: Mutex<Option<Session>>,
    contended_drops: AtomicU64,
}

impl Aggregator {
    pub(crate) const fn new(role: RcRole) -> Self {
        Self {
            role,
            session: Mutex::new(None),
            contended_drops: AtomicU64::new(0),
        }
    }

    fn with_session_nonblocking(&self, apply: impl FnOnce(&mut Session)) {
        let mut guard = match self.session.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.contended_drops.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        if let Some(session) = guard.as_mut() {
            apply(session);
        }
    }

    pub(crate) fn record_stage(&self, stage: Stage, ms: f64) {
        self.with_session_nonblocking(|session| {
            session.stages[stage.index()].record(ms);
        });
    }

    pub(crate) fn add_counter(&self, counter: Counter, delta: u64) {
        self.with_session_nonblocking(|session| {
            let slot = &mut session.counters[counter.index()];
            *slot = slot.saturating_add(delta);
        });
    }

    pub(crate) fn raise_max(&self, counter: MaxCounter, value: u64) {
        self.with_session_nonblocking(|session| {
            let slot = &mut session.max_counters[counter.index()];
            *slot = Some(slot.map_or(value, |current| current.max(value)));
        });
    }

    /// False for a value outside the dimension's closed set (old value kept)
    /// or when no session is active.
    pub(crate) fn set_dimension(&self, dimension: Dimension, value: &str) -> bool {
        if !dimension.value_allowed(value) {
            return false;
        }
        let mut set = false;
        self.with_session_nonblocking(|session| {
            session.dimensions[dimension.index()] = Some(value.to_string());
            set = true;
        });
        set
    }

    /// Reconcile with the current control state. Returns the summary that
    /// became due, if any; emission happens outside the lock.
    pub(crate) fn sync_at(&self, active: bool, now: Instant) -> Option<Summary> {
        let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        match (active, guard.as_mut()) {
            (true, None) => {
                *guard = Some(Session::new(now));
                None
            }
            (false, Some(session)) => {
                // A counter-only final partial still emits: the n >= 5 rule
                // is about percentile quality, not about counts (#371).
                let enough = session.has_counts()
                    || session.stages.iter().any(|h| h.n() >= FINAL_MIN_SAMPLES);
                let summary = enough.then(|| session.take_interval(self.role, now, true)).flatten();
                *guard = None;
                summary
            }
            (true, Some(session))
                if now.saturating_duration_since(session.interval_started_at) >= EMIT_INTERVAL =>
            {
                session.take_interval(self.role, now, false)
            }
            _ => None,
        }
    }

    #[cfg(test)]
    fn contended_drops(&self) -> u64 {
        self.contended_drops.load(Ordering::Relaxed)
    }
}

static HOST: Aggregator = Aggregator::new(RcRole::Host);
static CONTROLLER: Aggregator = Aggregator::new(RcRole::Controller);

fn aggregator(role: RcRole) -> &'static Aggregator {
    match role {
        RcRole::Host => &HOST,
        RcRole::Controller => &CONTROLLER,
    }
}

/// Record one latency sample for `role`'s active control session. Never
/// blocks and never allocates; dropped when no session is active.
pub(crate) fn record_stage(role: RcRole, stage: Stage, ms: f64) {
    aggregator(role).record_stage(stage, ms);
}

/// Add to a sum counter for `role`'s active session. Same non-blocking,
/// dropped-without-a-session rules as [`record_stage`].
pub(crate) fn add_counter(role: RcRole, counter: Counter, delta: u64) {
    aggregator(role).add_counter(counter, delta);
}

/// Raise a max-per-interval value for `role`'s active session. Same
/// non-blocking, dropped-without-a-session rules as [`record_stage`].
pub(crate) fn raise_max(role: RcRole, counter: MaxCounter, value: u64) {
    aggregator(role).raise_max(counter, value);
}

/// Set a closed-value dimension for the rest of `role`'s session.
pub(crate) fn set_dimension(role: RcRole, dimension: Dimension, value: &str) -> bool {
    aggregator(role).set_dimension(dimension, value)
}

/// Reconcile `role` with whether control is active right now, and emit any
/// interval that became due. Called from the remote-control sweeper tick.
pub(crate) fn sync(role: RcRole, active: bool) {
    if let Some(summary) = aggregator(role).sync_at(active, Instant::now()) {
        log::info!("{}", summary.log_line());
        crate::analytics::remote_control_latency_summary(&summary);
    }
}

/// Test hook: force the host session's current interval out now.
#[cfg(test)]
pub(crate) fn take_host_interval_for_test() -> Option<Summary> {
    // Drains at the real now: a future instant would leave the next interval
    // starting in the future and hold back every later forced take.
    let mut guard = HOST.session.lock().unwrap_or_else(|p| p.into_inner());
    guard.as_mut()?.take_interval(RcRole::Host, Instant::now(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixture {
        #[serde(rename = "latencyHistogramVectors")]
        vectors: Vec<Vector>,
        #[serde(rename = "remoteControlLatencySummary")]
        summary: SummaryContract,
    }

    #[derive(Deserialize)]
    struct Vector {
        name: String,
        samples: Vec<Option<f64>>,
        expected: Expected,
    }

    #[derive(Deserialize)]
    struct Expected {
        n: u32,
        rejected: u32,
        p50: Option<u32>,
        p95: Option<u32>,
        max: Option<u32>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SummaryContract {
        stages: Vec<String>,
        counters: Vec<String>,
        max_counters: Vec<String>,
        dimensions: serde_json::Map<String, serde_json::Value>,
        roles: Vec<String>,
        interval_ms: u64,
        final_min_samples: u32,
        max_events_per_session: u32,
        log_prefix: String,
        example: Example,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Example {
        summary: ExampleSummary,
        properties: serde_json::Map<String, serde_json::Value>,
        log_line: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExampleSummary {
        role: String,
        rc_session_id: String,
        interval_s: u64,
        #[serde(rename = "final")]
        is_final: bool,
        stages: serde_json::Map<String, serde_json::Value>,
        #[serde(default)]
        counters: serde_json::Map<String, serde_json::Value>,
        dimensions: serde_json::Map<String, serde_json::Value>,
    }

    const RAW_CONTRACT: &str = include_str!("../../../../contracts/petal-contracts.json");

    fn fixture() -> Fixture {
        serde_json::from_str(RAW_CONTRACT).expect("contract fixture parses")
    }

    fn summary_with(stages: Vec<(Stage, HistogramStats)>) -> Summary {
        Summary {
            role: RcRole::Host,
            rc_session_id: "0".repeat(32),
            interval_s: 60,
            is_final: false,
            stages,
            counters: vec![],
            max_counters: vec![],
            dimensions: vec![],
        }
    }

    #[test]
    fn histogram_matches_the_shared_contract_vectors() {
        let vectors = fixture().vectors;
        assert!(vectors.len() >= 5);
        for vector in vectors {
            let mut histogram = LatencyHistogram::new();
            for sample in &vector.samples {
                histogram.record(sample.unwrap_or(f64::NAN));
            }
            assert_eq!(histogram.n(), vector.expected.n, "{} n", vector.name);
            assert_eq!(histogram.rejected(), vector.expected.rejected, "{} rejected", vector.name);
            assert_eq!(histogram.percentile(0.5), vector.expected.p50, "{} p50", vector.name);
            assert_eq!(histogram.percentile(0.95), vector.expected.p95, "{} p95", vector.name);
            assert_eq!(histogram.max(), vector.expected.max, "{} max", vector.name);
        }
    }

    #[test]
    fn histogram_resolution_is_about_five_percent_and_reset_clears() {
        assert_eq!(bucket_edges().len(), 176);
        let mut histogram = LatencyHistogram::new();
        for ms in [100.0, 250.0, 1000.0, 4000.0] {
            histogram.reset();
            histogram.record(ms);
            histogram.record(ms * 1.001);
            let p50 = f64::from(histogram.percentile(0.5).unwrap());
            assert!((p50 - ms).abs() / ms <= 0.05, "{ms} -> {p50}");
        }
        histogram.reset();
        assert_eq!(histogram.stats(), None);
        assert_eq!(histogram.rejected(), 0);
    }

    #[test]
    fn keys_match_the_shared_contract_schema() {
        let contract = fixture().summary;
        let stages: Vec<_> = Stage::ALL.iter().map(|s| s.key()).collect();
        assert_eq!(stages, contract.stages);
        let mut dimensions: Vec<_> = Dimension::ALL.iter().map(|d| d.key()).collect();
        assert_eq!(dimensions.len(), DIMENSION_COUNT);
        // serde_json's Map is sorted; the declared order (which the log line
        // follows) is checked against the raw contract text instead.
        let block = &RAW_CONTRACT[RAW_CONTRACT.find("\"remoteControlLatencySummary\"").unwrap()..];
        let block = &block[block.find("\"dimensions\"").unwrap()..];
        let positions: Vec<_> = dimensions
            .iter()
            .map(|key| block.find(&format!("\"{key}\":")).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "dimension order drifted");
        dimensions.sort_unstable();
        let contract_dimensions: Vec<_> = contract.dimensions.keys().map(String::as_str).collect();
        assert_eq!(dimensions, contract_dimensions);
        for dimension in Dimension::ALL {
            let expected = &contract.dimensions[dimension.key()];
            match dimension.allowed_values() {
                None => assert!(expected.is_null(), "{}", dimension.key()),
                Some(values) => {
                    let expected: Vec<_> = expected
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!(values, expected.as_slice(), "{}", dimension.key());
                }
            }
        }
        let counters: Vec<_> = Counter::ALL.iter().map(|c| c.key()).collect();
        assert_eq!(counters, contract.counters);
        let max_counters: Vec<_> = MaxCounter::ALL.iter().map(|c| c.key()).collect();
        assert_eq!(max_counters, contract.max_counters);
        let roles: Vec<_> = [RcRole::Host, RcRole::Controller].iter().map(|r| r.key()).collect();
        assert_eq!(roles, contract.roles);
        assert_eq!(contract.interval_ms, EMIT_INTERVAL.as_millis() as u64);
        assert_eq!(contract.final_min_samples, FINAL_MIN_SAMPLES);
        assert_eq!(contract.max_events_per_session, MAX_EVENTS_PER_SESSION);
        assert_eq!(contract.log_prefix, LOG_PREFIX);
    }

    #[test]
    fn example_summary_renders_the_contract_properties_and_log_line() {
        let example = fixture().summary.example;
        let input = example.summary;
        assert_eq!(input.role, RcRole::Host.key());
        let stats = |value: &serde_json::Value| HistogramStats {
            p50: value["p50"].as_u64().unwrap() as u32,
            p95: value["p95"].as_u64().unwrap() as u32,
            max: value["max"].as_u64().unwrap() as u32,
            n: value["n"].as_u64().unwrap() as u32,
        };
        let summary = Summary {
            role: RcRole::Host,
            rc_session_id: input.rc_session_id,
            interval_s: input.interval_s,
            is_final: input.is_final,
            // Summaries hold stages in schema order (serde_json's Map is sorted).
            stages: Stage::ALL
                .iter()
                .filter_map(|stage| Some((*stage, stats(input.stages.get(stage.key())?))))
                .collect(),
            counters: Counter::ALL
                .iter()
                .filter_map(|c| Some((*c, input.counters.get(c.key())?.as_u64()?)))
                .collect(),
            max_counters: vec![],
            dimensions: Dimension::ALL
                .iter()
                .filter_map(|d| {
                    Some((*d, input.dimensions.get(d.key())?.as_str()?.to_string()))
                })
                .collect(),
        };
        let properties: serde_json::Map<String, serde_json::Value> =
            summary.properties().into_iter().collect();
        assert_eq!(properties, example.properties);
        // The log line pins key order as well as values.
        assert_eq!(summary.log_line(), example.log_line);
        let empty = summary_with(vec![(Stage::PhotonMs, HistogramStats { p50: 0, p95: 0, max: 0, n: 0 })]);
        assert!(!empty.log_line().contains("photon_ms"), "an n = 0 stage is never rendered");
    }

    #[test]
    fn lifecycle_grant_tick_emit_then_final_partial() {
        let aggregator = Aggregator::new(RcRole::Host);
        let t0 = Instant::now();
        aggregator.record_stage(Stage::ReplayInjectMs, 4.0);
        assert!(aggregator.sync_at(false, t0).is_none(), "no session before grant");
        assert!(aggregator.sync_at(true, t0).is_none(), "grant starts a session");
        for ms in [2.0, 3.0, 4.0, 5.0, 6.0, 7.0] {
            aggregator.record_stage(Stage::ReplayInjectMs, ms);
        }
        assert!(aggregator.sync_at(true, t0 + Duration::from_secs(59)).is_none());
        let first = aggregator
            .sync_at(true, t0 + Duration::from_secs(60))
            .expect("60 s of active control emits");
        assert!(!first.is_final);
        assert_eq!(first.interval_s, 60);
        assert_eq!(first.stages.len(), 1);
        assert_eq!(first.stages[0].0, Stage::ReplayInjectMs);
        assert_eq!(first.stages[0].1.n, 6);
        assert_eq!(first.rc_session_id.len(), 32);
        for ms in [10.0, 11.0, 12.0, 13.0, 14.0] {
            aggregator.record_stage(Stage::ReplayCompleteMs, ms);
        }
        let last = aggregator
            .sync_at(false, t0 + Duration::from_secs(75))
            .expect("release with n >= 5 emits a final partial");
        assert!(last.is_final);
        assert_eq!(last.interval_s, 15);
        assert_eq!(last.rc_session_id, first.rc_session_id);
        assert_eq!(last.stages.len(), 1, "a stage with n = 0 is omitted");
        aggregator.record_stage(Stage::ReplayCompleteMs, 1.0);
        assert!(aggregator.sync_at(false, t0 + Duration::from_secs(80)).is_none());
    }

    #[test]
    fn final_partial_needs_five_samples_and_regrant_mints_a_new_session() {
        let aggregator = Aggregator::new(RcRole::Host);
        let t0 = Instant::now();
        aggregator.sync_at(true, t0);
        for ms in [1.0, 2.0, 3.0, 4.0] {
            aggregator.record_stage(Stage::ReplayCompleteMs, ms);
        }
        assert!(aggregator.sync_at(false, t0 + Duration::from_secs(5)).is_none(), "n < 5");
        aggregator.sync_at(true, t0 + Duration::from_secs(6));
        for _ in 0..5 {
            aggregator.record_stage(Stage::ReplayCompleteMs, 9.0);
        }
        let summary = aggregator.sync_at(false, t0 + Duration::from_secs(7)).unwrap();
        assert_eq!(summary.stages[0].1.n, 5, "the earlier session's samples are gone");
    }

    /// Empty intervals must never emit or spend the 120-event cap; an
    /// interval with samples after a run of empty ones still emits.
    #[test]
    fn empty_intervals_never_emit_or_spend_the_cap() {
        let aggregator = Aggregator::new(RcRole::Host);
        let mut now = Instant::now();
        aggregator.sync_at(true, now);
        assert!(aggregator.set_dimension(Dimension::WindowCodec, "h264"));
        for _ in 0..200 {
            now += EMIT_INTERVAL;
            assert!(aggregator.sync_at(true, now).is_none(), "a dimension alone is not data");
        }
        aggregator.record_stage(Stage::ReplayInjectMs, 3.0);
        now += EMIT_INTERVAL;
        let summary = aggregator.sync_at(true, now).expect("200 empty intervals spent nothing");
        assert_eq!(summary.dimensions, [(Dimension::WindowCodec, "h264".to_string())]);
        now += EMIT_INTERVAL;
        assert!(aggregator.sync_at(true, now).is_none(), "the next empty interval is silent again");
    }

    /// #371, direction 1: counters reset to 0 (not "present 0"), so an
    /// interval with no samples and no non-zero counter stays silent.
    #[test]
    fn counter_reset_never_turns_empty_intervals_into_events() {
        let aggregator = Aggregator::new(RcRole::Host);
        let mut now = Instant::now();
        aggregator.sync_at(true, now);
        aggregator.add_counter(Counter::InputStallCount, 2);
        now += EMIT_INTERVAL;
        let summary = aggregator.sync_at(true, now).expect("a counter alone is data");
        assert!(summary.stages.is_empty());
        assert_eq!(summary.counters, [(Counter::InputStallCount, 2)]);
        assert!(summary.log_line().ends_with(" input_stall_count=2"));
        for _ in 0..200 {
            now += EMIT_INTERVAL;
            assert!(aggregator.sync_at(true, now).is_none(), "touched counters reset to absent");
        }
        aggregator.add_counter(Counter::InputStallCount, 0);
        now += EMIT_INTERVAL;
        assert!(aggregator.sync_at(true, now).is_none(), "a zero delta is not data");
        assert!(aggregator.sync_at(false, now).is_none(), "nor is an empty final");
    }

    /// #371, direction 2: a final partial carrying only counters still emits;
    /// the stage-only n >= 5 rule must not drop it.
    #[test]
    fn counter_only_final_partial_still_emits() {
        let aggregator = Aggregator::new(RcRole::Host);
        let t0 = Instant::now();
        aggregator.sync_at(true, t0);
        aggregator.record_stage(Stage::InputExcessDelayMs, 80.0);
        aggregator.add_counter(Counter::InputStallCount, 1);
        let last = aggregator
            .sync_at(false, t0 + Duration::from_secs(10))
            .expect("a counter-only final partial emits");
        assert!(last.is_final);
        assert_eq!(last.counters, [(Counter::InputStallCount, 1)]);
        assert_eq!(last.stages.len(), 1, "the n = 1 stage rides along");
    }

    /// #371: `offset_err_ms` keeps the interval max, renders 0, resets per
    /// interval, and never makes an interval or a final partial emit alone.
    #[test]
    fn max_counters_keep_the_interval_max_but_never_emit_alone() {
        let aggregator = Aggregator::new(RcRole::Host);
        let mut now = Instant::now();
        aggregator.sync_at(true, now);
        aggregator.raise_max(MaxCounter::OffsetErrMs, 0);
        now += EMIT_INTERVAL;
        assert!(aggregator.sync_at(true, now).is_none(), "a max counter alone is not data");
        aggregator.record_stage(Stage::InputLegMs, 20.0);
        for value in [3, 9, 4] {
            aggregator.raise_max(MaxCounter::OffsetErrMs, value);
        }
        now += EMIT_INTERVAL;
        let summary = aggregator.sync_at(true, now).unwrap();
        assert_eq!(summary.max_counters, [(MaxCounter::OffsetErrMs, 9)]);
        assert!(summary.log_line().contains(" input_leg_ms_n=1 offset_err_ms=9"));
        aggregator.record_stage(Stage::InputLegLossyMs, 20.0);
        aggregator.raise_max(MaxCounter::OffsetErrMs, 0);
        now += EMIT_INTERVAL;
        let next = aggregator.sync_at(true, now).unwrap();
        assert_eq!(next.max_counters, [(MaxCounter::OffsetErrMs, 0)], "reset, then 0 is sent");
        aggregator.raise_max(MaxCounter::OffsetErrMs, 7);
        assert!(aggregator.sync_at(false, now).is_none(), "nor is a max-only final");
    }

    #[test]
    fn controller_role_has_its_own_session() {
        assert_eq!(RcRole::Controller.key(), "controller");
        let aggregator = Aggregator::new(RcRole::Controller);
        let t0 = Instant::now();
        aggregator.sync_at(true, t0);
        for ms in [20.0, 21.0, 22.0, 23.0, 24.0] {
            aggregator.record_stage(Stage::InputRttMs, ms);
        }
        let summary = aggregator.sync_at(false, t0 + Duration::from_secs(3)).unwrap();
        assert_eq!(summary.role, RcRole::Controller);
        assert!(summary.log_line().contains("role=controller"));
        assert_eq!(summary.stages[0].0, Stage::InputRttMs);
    }

    #[test]
    fn a_session_caps_at_120_events_including_the_final() {
        let aggregator = Aggregator::new(RcRole::Host);
        let mut now = Instant::now();
        aggregator.sync_at(true, now);
        let mut emitted = 0;
        for _ in 0..130 {
            aggregator.record_stage(Stage::ReplayInjectMs, 3.0);
            now += EMIT_INTERVAL;
            if aggregator.sync_at(true, now).is_some() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, MAX_EVENTS_PER_SESSION);
        for _ in 0..5 {
            aggregator.record_stage(Stage::ReplayInjectMs, 3.0);
        }
        assert!(aggregator.sync_at(false, now).is_none(), "the final partial is capped too");
    }

    #[test]
    fn dimensions_are_closed() {
        let aggregator = Aggregator::new(RcRole::Host);
        assert!(!aggregator.set_dimension(Dimension::WindowCodec, "h264"), "no session yet");
        aggregator.sync_at(true, Instant::now());
        assert!(aggregator.set_dimension(Dimension::WindowCodec, "h264"));
        assert!(!aggregator.set_dimension(Dimension::WindowCodec, "My Codec"));
        assert!(!aggregator.set_dimension(Dimension::LkRegion, "Room Name With Spaces"));
        assert!(!aggregator.set_dimension(Dimension::LkRegion, &"a".repeat(33)));
        assert!(aggregator.set_dimension(Dimension::LkRegion, "us-east-1"));
    }

    #[test]
    fn record_stage_never_blocks_on_a_held_lock() {
        let aggregator = std::sync::Arc::new(Aggregator::new(RcRole::Host));
        aggregator.sync_at(true, Instant::now());
        let guard = aggregator.session.lock().unwrap();
        let worker = {
            let aggregator = aggregator.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                for _ in 0..1000 {
                    aggregator.record_stage(Stage::ReplayInjectMs, 1.0);
                }
                started.elapsed()
            })
        };
        let elapsed = worker.join().unwrap();
        drop(guard);
        assert!(elapsed < Duration::from_secs(1), "record_stage waited on the lock");
        assert_eq!(aggregator.contended_drops(), 1000);
    }
}
