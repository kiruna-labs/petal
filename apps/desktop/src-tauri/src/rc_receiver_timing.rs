//! #374: native controller receive-path timing for the window it controls.
//!
//! Fed only while this controller holds a grant on that host window
//! (`rc_input_timing::controls_window`), from the real receive path:
//! - per-frame subscribe timing events (`subscribe_timing_events`):
//!   `capture_to_receive_ms` = WebrtcReceive - (host capture stamp + offset),
//!   and `decode_ms` = DecoderOutput - DecoderUpload joined by `frame_id`;
//! - a 1 s receiver stats poll: the inbound-rtp interval deltas shared with
//!   the web controller (`shared/logic/inboundRtpDeltas.ts`, pinned by the
//!   contract's `inboundRtpDeltaVectors`) and `sub_rtt_ms`;
//! - `receive_to_present_ms` at the compositor hand-off (enqueue / Present),
//!   estimated: no present callback exists (`present_timing=estimated`).
//!
//! Per-frame values go into the histograms only; nothing here logs per frame.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use futures::{Stream, StreamExt};
use livekit::prelude::{RemoteVideoTrack, SubscribeTimingEvent, SubscribeTimingStage};
use livekit::webrtc::stats::RtcStats;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::rc_input_timing::{self, ClockEstimate};
use crate::rc_latency_telemetry::{self, Aggregator, Counter, Dimension, MaxCounter, RcRole, Stage};
use crate::time_util::now_ms;

/// The receiver stats cadence, matching the web controller's 1 s poll.
pub(crate) const STATS_INTERVAL: Duration = Duration::from_secs(1);
const STATS_TIMEOUT: Duration = Duration::from_secs(2);
/// Uploads awaiting their decoder output; a frame the decoder drops ages out.
const MAX_PENDING_UPLOADS: usize = 64;

/// The cumulative inbound-rtp counters one interval delta is built from
/// (W3C names, the contract's `previous` / `current` objects).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct InboundRtpCounters {
    pub(crate) jitter_buffer_delay: Option<f64>,
    pub(crate) jitter_buffer_target_delay: Option<f64>,
    pub(crate) jitter_buffer_minimum_delay: Option<f64>,
    pub(crate) jitter_buffer_emitted_count: Option<f64>,
    pub(crate) total_decode_time: Option<f64>,
    pub(crate) total_processing_delay: Option<f64>,
    pub(crate) frames_decoded: Option<f64>,
    pub(crate) frames_dropped: Option<f64>,
    pub(crate) freeze_count: Option<f64>,
    pub(crate) total_freezes_duration: Option<f64>,
    pub(crate) nack_count: Option<f64>,
    pub(crate) pli_count: Option<f64>,
    pub(crate) packets_lost: Option<f64>,
    pub(crate) packets_received: Option<f64>,
}

impl InboundRtpCounters {
    /// The video inbound-rtp counters in a receiver's stats (most bytes
    /// wins). The native parser defaults an absent field to 0, so every
    /// field is present here; only the web side can see a missing one.
    pub(crate) fn from_stats(stats: &[RtcStats]) -> Option<Self> {
        let best = stats
            .iter()
            .filter_map(|s| match s {
                RtcStats::InboundRtp(inbound) if inbound.stream.kind == "video" => Some(inbound),
                _ => None,
            })
            .max_by_key(|inbound| inbound.inbound.bytes_received)?;
        let i = &best.inbound;
        Some(Self {
            jitter_buffer_delay: Some(i.jitter_buffer_delay),
            jitter_buffer_target_delay: Some(i.jitter_buffer_target_delay),
            jitter_buffer_minimum_delay: Some(i.jitter_buffer_minimum_delay),
            jitter_buffer_emitted_count: Some(i.jitter_buffer_emitted_count as f64),
            total_decode_time: Some(i.total_decode_time),
            total_processing_delay: Some(i.total_processing_delay),
            frames_decoded: Some(f64::from(i.frames_decoded)),
            frames_dropped: Some(f64::from(i.frames_dropped)),
            freeze_count: Some(f64::from(i.freeze_count)),
            total_freezes_duration: Some(i.total_freeze_duration),
            nack_count: Some(f64::from(i.nack_count)),
            pli_count: Some(f64::from(i.pli_count)),
            packets_lost: Some(best.received.packets_lost as f64),
            packets_received: Some(best.received.packets_received as f64),
        })
    }
}

/// One interval's values (the contract's `expected` objects). `None` is
/// missing data: never sent as 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct InboundRtpDeltas {
    pub(crate) jb_delay_ms: Option<f64>,
    pub(crate) jb_target_ms: Option<f64>,
    pub(crate) jb_min_ms: Option<f64>,
    pub(crate) decode_ms: Option<f64>,
    pub(crate) processing_ms: Option<f64>,
    pub(crate) frames_dropped: Option<f64>,
    pub(crate) freeze_count: Option<f64>,
    pub(crate) freeze_duration_ms: Option<f64>,
    pub(crate) nack_count: Option<f64>,
    pub(crate) pli_count: Option<f64>,
    pub(crate) packets_lost_pct_x100: Option<f64>,
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}

/// `current - previous`, or `None` when either is missing or it went backwards.
fn delta(previous: Option<f64>, current: Option<f64>) -> Option<f64> {
    let (before, after) = (finite(previous)?, finite(current)?);
    (after >= before).then_some(after - before)
}

/// Δseconds / Δcount × 1000, `None` without a positive count.
fn average_ms(seconds: Option<f64>, count: Option<f64>) -> Option<f64> {
    let (total, units) = (seconds?, count?);
    (units > 0.0).then(|| total / units * 1000.0)
}

/// The Rust mirror of `inboundRtpDeltas` in `shared/logic/inboundRtpDeltas.ts`.
pub(crate) fn inbound_rtp_deltas(p: &InboundRtpCounters, c: &InboundRtpCounters) -> InboundRtpDeltas {
    let emitted = delta(p.jitter_buffer_emitted_count, c.jitter_buffer_emitted_count);
    let decoded = delta(p.frames_decoded, c.frames_decoded);
    let received = delta(p.packets_received, c.packets_received);
    // packetsLost may fall when a late packet arrives; that interval lost nothing.
    let lost = finite(p.packets_lost)
        .zip(finite(c.packets_lost))
        .map(|(before, after)| (after - before).max(0.0));
    let expected = received.zip(lost).map(|(r, l)| r + l);
    InboundRtpDeltas {
        jb_delay_ms: average_ms(delta(p.jitter_buffer_delay, c.jitter_buffer_delay), emitted),
        jb_target_ms: average_ms(
            delta(p.jitter_buffer_target_delay, c.jitter_buffer_target_delay),
            emitted,
        ),
        jb_min_ms: average_ms(
            delta(p.jitter_buffer_minimum_delay, c.jitter_buffer_minimum_delay),
            emitted,
        ),
        decode_ms: average_ms(delta(p.total_decode_time, c.total_decode_time), decoded),
        processing_ms: average_ms(
            delta(p.total_processing_delay, c.total_processing_delay),
            decoded,
        ),
        frames_dropped: delta(p.frames_dropped, c.frames_dropped),
        freeze_count: delta(p.freeze_count, c.freeze_count),
        freeze_duration_ms: delta(p.total_freezes_duration, c.total_freezes_duration)
            .map(|seconds| seconds * 1000.0),
        nack_count: delta(p.nack_count, c.nack_count),
        pli_count: delta(p.pli_count, c.pli_count),
        packets_lost_pct_x100: expected
            .zip(lost)
            .filter(|(expected, _)| *expected > 0.0)
            .map(|(expected, lost)| (lost / expected * 10_000.0).round()),
    }
}

/// The `jb_min_ms` dimension bucket (contract `jbMinMsBucketVectors`).
pub(crate) fn jb_min_ms_bucket(ms: f64) -> &'static str {
    if !(ms >= 0.5) {
        "0"
    } else if ms < 20.0 {
        "1_19"
    } else if ms < 50.0 {
        "20_49"
    } else if ms < 100.0 {
        "50_99"
    } else if ms < 200.0 {
        "100_199"
    } else {
        "200_plus"
    }
}

/// The selected pair's `currentRoundTripTime` in ms. The native parser reads
/// an absent RTT as 0, so 0 is no sample rather than a 0 ms round trip.
pub(crate) fn selected_candidate_pair_rtt_ms(stats: &[RtcStats]) -> Option<f64> {
    let seconds = crate::sfu_edge::selected_candidate_pair(stats)?.current_round_trip_time;
    (seconds.is_finite() && seconds > 0.0).then(|| seconds * 1000.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TimingSample {
    CaptureToReceive { ms: f64, offset_err_ms: u64 },
    Decode { ms: f64 },
}

/// Joins one track's subscribe timing events by `frame_id`. Bounded, and a
/// drop or out-of-order event yields no sample rather than a wrong one.
#[derive(Debug, Default)]
pub(crate) struct TimingJoiner {
    uploads: VecDeque<(u32, u64)>,
}

impl TimingJoiner {
    pub(crate) fn observe(
        &mut self,
        event: &SubscribeTimingEvent,
        clock: impl FnOnce() -> Option<ClockEstimate>,
    ) -> Option<TimingSample> {
        match event.stage {
            SubscribeTimingStage::WebrtcReceive => {
                if event.capture_timestamp_us == 0 {
                    return None;
                }
                let clock = clock()?;
                let ms = rc_input_timing::offset_corrected_ms(
                    event.timestamp_us as f64 / 1000.0,
                    event.capture_timestamp_us as f64 / 1000.0,
                    clock,
                )?;
                Some(TimingSample::CaptureToReceive {
                    ms,
                    offset_err_ms: clock.err_ms.ceil() as u64,
                })
            }
            SubscribeTimingStage::DecoderUpload => {
                let id = event.frame_id?;
                self.uploads.retain(|(frame, _)| *frame != id);
                if self.uploads.len() >= MAX_PENDING_UPLOADS {
                    self.uploads.pop_front();
                }
                self.uploads.push_back((id, event.timestamp_us));
                None
            }
            SubscribeTimingStage::DecoderOutput => {
                let id = event.frame_id?;
                let at = self.uploads.iter().position(|(frame, _)| *frame == id)?;
                let (_, uploaded_us) = self.uploads.remove(at)?;
                let us = event.timestamp_us.checked_sub(uploaded_us)?;
                Some(TimingSample::Decode {
                    ms: us as f64 / 1000.0,
                })
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.uploads.clear();
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.uploads.len()
    }
}

const DELTA_COUNTERS: [(fn(&InboundRtpDeltas) -> Option<f64>, Counter); 5] = [
    (|d| d.frames_dropped, Counter::FramesDropped),
    (|d| d.freeze_count, Counter::FreezeCount),
    (|d| d.freeze_duration_ms, Counter::FreezeDurationMs),
    (|d| d.nack_count, Counter::NackCount),
    (|d| d.pli_count, Counter::PliCount),
];

/// One window track's receive-timing state. Everything resets when control
/// of that window ends, so nothing straddles two control sessions.
#[derive(Debug, Default)]
pub(crate) struct ReceiverTiming {
    joiner: TimingJoiner,
    previous: Option<InboundRtpCounters>,
    /// A capture stamp arrived on this track while controlled.
    metadata_seen: bool,
    /// Per-frame decode samples flow, so the 1 s stats average is not also
    /// recorded into `decode_ms` (one source per stage).
    timing_decode: bool,
}

impl ReceiverTiming {
    pub(crate) fn on_event(
        &mut self,
        event: &SubscribeTimingEvent,
        controlled: bool,
        clock: impl FnOnce() -> Option<ClockEstimate>,
        aggregator: &Aggregator,
    ) {
        if !controlled {
            self.reset();
            return;
        }
        if event.stage == SubscribeTimingStage::WebrtcReceive && event.capture_timestamp_us > 0 {
            self.metadata_seen = true;
        }
        match self.joiner.observe(event, clock) {
            Some(TimingSample::CaptureToReceive { ms, offset_err_ms }) => aggregator
                .record_stage_with_max(
                    Stage::CaptureToReceiveMs,
                    ms,
                    MaxCounter::OffsetErrMs,
                    offset_err_ms,
                ),
            Some(TimingSample::Decode { ms }) => {
                self.timing_decode = true;
                aggregator.record_stage(Stage::DecodeMs, ms);
            }
            None => {}
        }
    }

    /// One 1 s receiver stats sample for the controlled window.
    pub(crate) fn on_stats(&mut self, stats: Option<&[RtcStats]>, aggregator: &Aggregator) {
        aggregator.set_dimension(
            Dimension::FrameMetadata,
            if self.metadata_seen { "available" } else { "unavailable" },
        );
        let Some(stats) = stats else {
            return;
        };
        let counters = InboundRtpCounters::from_stats(stats);
        if let (Some(previous), Some(current)) = (self.previous.as_ref(), counters.as_ref()) {
            self.record_deltas(&inbound_rtp_deltas(previous, current), aggregator);
        }
        self.previous = counters;
        if let Some(rtt) = selected_candidate_pair_rtt_ms(stats) {
            aggregator.record_stage(Stage::SubRttMs, rtt);
        }
    }

    fn record_deltas(&self, deltas: &InboundRtpDeltas, aggregator: &Aggregator) {
        let stages = [
            (deltas.jb_delay_ms, Stage::JbDelayMs),
            (deltas.jb_target_ms, Stage::JbTargetMs),
            (deltas.processing_ms, Stage::ProcessingMs),
            (deltas.decode_ms.filter(|_| !self.timing_decode), Stage::DecodeMs),
        ];
        for (value, stage) in stages {
            if let Some(ms) = value {
                aggregator.record_stage(stage, ms);
            }
        }
        if let Some(ms) = deltas.jb_min_ms {
            aggregator.set_dimension(Dimension::JbMinMs, jb_min_ms_bucket(ms));
        }
        for (read, counter) in DELTA_COUNTERS {
            if let Some(value) = read(deltas) {
                aggregator.add_counter(counter, value.round() as u64);
            }
        }
        if let Some(pct) = deltas.packets_lost_pct_x100 {
            aggregator.note_max(MaxCounter::PacketsLostPctX100, pct as u64);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.joiner.clear();
        self.previous = None;
        self.metadata_seen = false;
        self.timing_decode = false;
    }
}

async fn next_event<E>(events: &mut Option<E>) -> Option<SubscribeTimingEvent>
where
    E: Stream<Item = SubscribeTimingEvent> + Unpin,
{
    match events.as_mut() {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// The per-track loop. `controlled` and `clock` are read live: control can
/// start long after subscribe and end at any time.
pub(crate) async fn run<E, F, Fut>(
    mut events: Option<E>,
    mut poll_stats: F,
    controlled: impl Fn() -> bool,
    clock: impl Fn(u64) -> Option<ClockEstimate>,
    aggregator: &Aggregator,
    cancel: CancellationToken,
) where
    E: Stream<Item = SubscribeTimingEvent> + Unpin,
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<RtcStats>>>,
{
    let mut state = ReceiverTiming::default();
    let mut tick = tokio::time::interval(STATS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                if !controlled() {
                    state.reset();
                    continue;
                }
                let stats = poll_stats().await;
                // A release during the await must not land in the session.
                if controlled() {
                    state.on_stats(stats.as_deref(), aggregator);
                } else {
                    state.reset();
                }
            }
            event = next_event(&mut events) => match event {
                Some(event) => state.on_event(&event, controlled(), || clock(now_ms()), aggregator),
                None => events = None,
            },
        }
    }
}

/// Start receive timing for one subscribed window track. Call it BEFORE
/// `NativeVideoStream::new` on that track: decoder-output events are wired
/// into the stream only if the subscription exists first (#374).
pub(crate) fn spawn_for_window_track(
    video_track: &RemoteVideoTrack,
    has_frame_metadata: bool,
    host_identity: String,
    window_id: u32,
    cancel: CancellationToken,
) {
    // #374: subscribe only when the publication carries the frame-metadata
    // trailer (its receiver handler already exists); on any other track it
    // would install a new frame transformer on the receive path.
    let events = has_frame_metadata.then(|| Box::pin(video_track.subscribe_timing_events()));
    let track = video_track.clone();
    let clock_host = host_identity.clone();
    tauri::async_runtime::spawn(async move {
        run(
            events,
            move || {
                let track = track.clone();
                async move {
                    tokio::time::timeout(STATS_TIMEOUT, track.get_stats())
                        .await
                        .ok()?
                        .ok()
                }
            },
            move || rc_input_timing::controls_window(&host_identity, window_id),
            move |now| rc_input_timing::host_clock(&clock_host, now),
            rc_latency_telemetry::aggregator(RcRole::Controller),
            cancel,
        )
        .await;
    });
}

/// The decode-side stamp a controlled window's frame carries to the
/// compositor hand-off; `None` (no work later) for every other window.
pub(crate) fn controlled_frame_stamp(host_identity: &str, window_id: u32, decoded_at_us: u64) -> Option<u64> {
    rc_input_timing::controls_window(host_identity, window_id).then_some(decoded_at_us)
}

/// `hand-off - decoded + one refresh`: an estimate, since neither
/// `AVSampleBufferDisplayLayer` nor a windowed swap chain reports when the
/// frame actually lit (#374). `None` without a usable refresh interval.
pub(crate) fn present_sample_ms(decoded_at_us: u64, handed_off_us: u64, refresh_ms: f64) -> Option<f64> {
    if !(refresh_ms.is_finite() && refresh_ms > 0.0) {
        return None;
    }
    let waited_us = handed_off_us.checked_sub(decoded_at_us)?;
    Some(waited_us as f64 / 1000.0 + refresh_ms)
}

fn record_present_into(aggregator: &Aggregator, decoded_at_us: u64, handed_off_us: u64, refresh_ms: f64) {
    if let Some(ms) = present_sample_ms(decoded_at_us, handed_off_us, refresh_ms) {
        aggregator.record_stage(Stage::ReceiveToPresentMs, ms);
        aggregator.set_dimension(Dimension::PresentTiming, "estimated");
    }
}

/// Compositor hand-off of a frame stamped by [`controlled_frame_stamp`].
pub(crate) fn record_present(decoded_at_us: u64, handed_off_us: u64, refresh_ms: f64) {
    record_present_into(
        rc_latency_telemetry::aggregator(RcRole::Controller),
        decoded_at_us,
        handed_off_us,
        refresh_ms,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const RAW_CONTRACT: &str = include_str!("../../../../contracts/petal-contracts.json");

    #[derive(Deserialize)]
    struct Contract {
        #[serde(rename = "inboundRtpDeltaVectors")]
        vectors: Vec<DeltaVector>,
        #[serde(rename = "jbMinMsBucketVectors")]
        buckets: Vec<BucketVector>,
    }

    #[derive(Deserialize)]
    struct DeltaVector {
        name: String,
        previous: InboundRtpCounters,
        current: InboundRtpCounters,
        expected: serde_json::Map<String, serde_json::Value>,
    }

    #[derive(Deserialize)]
    struct BucketVector {
        ms: f64,
        bucket: String,
    }

    fn contract() -> Contract {
        serde_json::from_str(RAW_CONTRACT).expect("contract parses")
    }

    #[test]
    fn interval_deltas_pass_the_shared_contract_vectors() {
        let vectors = contract().vectors;
        for required in ["counter-reset", "zero-emitted", "missing-and-non-finite"] {
            assert!(vectors.iter().any(|v| v.name == required), "contract lacks {required}");
        }
        for vector in vectors {
            let actual = serde_json::to_value(inbound_rtp_deltas(&vector.previous, &vector.current))
                .expect("serializes");
            let actual = actual.as_object().unwrap();
            let mut want_keys: Vec<_> = vector.expected.keys().collect();
            let mut got_keys: Vec<_> = actual.keys().collect();
            want_keys.sort();
            got_keys.sort();
            assert_eq!(got_keys, want_keys, "{} keys", vector.name);
            for (key, want) in &vector.expected {
                let got = &actual[key];
                match want.as_f64() {
                    None => assert!(got.is_null(), "{} {key}: {got} != null", vector.name),
                    Some(want) => {
                        let got = got.as_f64().unwrap_or(f64::NAN);
                        assert!((got - want).abs() < 1e-6, "{} {key}: {got} != {want}", vector.name);
                    }
                }
            }
        }
    }

    #[test]
    fn jb_min_buckets_pass_the_shared_contract_vectors() {
        for vector in contract().buckets {
            assert_eq!(jb_min_ms_bucket(vector.ms), vector.bucket, "{}", vector.ms);
            assert!(Dimension::JbMinMs.value_allowed(jb_min_ms_bucket(vector.ms)));
        }
        assert_eq!(jb_min_ms_bucket(f64::NAN), "0");
    }

    fn event(stage: SubscribeTimingStage, timestamp_us: u64, capture_us: u64, frame_id: Option<u32>) -> SubscribeTimingEvent {
        SubscribeTimingEvent {
            stage,
            timestamp_us,
            capture_timestamp_us: capture_us,
            frame_id,
        }
    }

    /// Local clock 25 ms ahead of the host, ±2 ms.
    fn clock() -> Option<ClockEstimate> {
        Some(ClockEstimate {
            offset_ms: 25.0,
            err_ms: 2.0,
        })
    }

    #[test]
    fn capture_to_receive_applies_the_offset_and_a_stale_offset_gives_nothing() {
        let mut joiner = TimingJoiner::default();
        let receive = event(SubscribeTimingStage::WebrtcReceive, 1_040_000, 1_000_000, Some(7));
        assert_eq!(
            joiner.observe(&receive, clock),
            Some(TimingSample::CaptureToReceive {
                ms: 15.0,
                offset_err_ms: 2
            }),
            "1040 - (1000 + 25)"
        );
        assert_eq!(joiner.observe(&receive, || None), None, "stale offset: no sample");
        let no_stamp = event(SubscribeTimingStage::WebrtcReceive, 1_040_000, 0, Some(7));
        assert_eq!(joiner.observe(&no_stamp, clock), None, "no capture stamp");
        // Beyond the offset error is a clock step, not a 0 ms leg.
        let stepped = event(SubscribeTimingStage::WebrtcReceive, 1_020_000, 1_000_000, Some(8));
        assert_eq!(joiner.observe(&stepped, clock), None);
        let within = event(SubscribeTimingStage::WebrtcReceive, 1_024_000, 1_000_000, Some(9));
        assert_eq!(
            joiner.observe(&within, clock),
            Some(TimingSample::CaptureToReceive {
                ms: 0.0,
                offset_err_ms: 2
            })
        );
    }

    #[test]
    fn decode_joins_by_frame_id_through_drops_and_out_of_order_events() {
        let mut joiner = TimingJoiner::default();
        let upload = |id, at| event(SubscribeTimingStage::DecoderUpload, at, 1, Some(id));
        let output = |id, at| event(SubscribeTimingStage::DecoderOutput, at, 1, Some(id));
        assert_eq!(joiner.observe(&output(1, 5_000), clock), None, "output before its upload");
        assert_eq!(joiner.observe(&upload(1, 6_000), clock), None);
        assert_eq!(joiner.observe(&upload(2, 7_000), clock), None, "frame 2 is dropped by the decoder");
        assert_eq!(joiner.observe(&upload(3, 8_000), clock), None);
        assert_eq!(joiner.observe(&output(3, 11_500), clock), Some(TimingSample::Decode { ms: 3.5 }));
        assert_eq!(joiner.observe(&output(1, 9_000), clock), Some(TimingSample::Decode { ms: 3.0 }), "out of order");
        assert_eq!(joiner.observe(&output(1, 9_500), clock), None, "a repeated output never pairs twice");
        assert_eq!(
            joiner.observe(&event(SubscribeTimingStage::DecoderOutput, 9_000, 1, None), clock),
            None,
            "no frame id"
        );
        for id in 100..(100 + MAX_PENDING_UPLOADS as u32 * 2) {
            joiner.observe(&upload(id, 10_000), clock);
        }
        assert_eq!(joiner.pending(), MAX_PENDING_UPLOADS, "dropped frames age out");
        assert_eq!(joiner.observe(&output(2, 12_000), clock), None, "evicted");
    }

    /// `RTCStatsReport::ToJson()` shape, keys spelled as the linked
    /// libwebrtc.a's attribute table spells them (#374 freeze-key proof).
    fn report_json(frames_decoded: u32, emitted: u32, jb_delay: f64, freezes_s: f64, rtt_s: f64) -> String {
        serde_json::json!([
            {
                "type": "inbound-rtp", "id": "IT01V", "timestamp": 1_700_000_000_000_000i64,
                "ssrc": 1234, "kind": "video", "transportId": "T01", "codecId": "C01",
                "packetsReceived": 10 * frames_decoded, "packetsLost": 0, "jitter": 0.001,
                "framesDecoded": frames_decoded, "framesDropped": 1, "bytesReceived": 99_000,
                "totalDecodeTime": 0.003 * f64::from(frames_decoded),
                "totalProcessingDelay": 0.040 * f64::from(frames_decoded),
                "jitterBufferDelay": jb_delay, "jitterBufferTargetDelay": 0.0,
                "jitterBufferMinimumDelay": 0.0, "jitterBufferEmittedCount": emitted,
                "pauseCount": 1, "totalPausesDuration": 0.2,
                "freezeCount": 2, "totalFreezesDuration": freezes_s,
                "nackCount": 3, "pliCount": 1
            },
            { "type": "transport", "id": "T01", "timestamp": 1_700_000_000_000_000i64, "selectedCandidatePairId": "CP01" },
            {
                "type": "candidate-pair", "id": "CP01", "timestamp": 1_700_000_000_000_000i64, "transportId": "T01",
                "localCandidateId": "L1", "remoteCandidateId": "R1", "state": "succeeded",
                "nominated": true, "currentRoundTripTime": rtt_s
            },
            { "type": "future-stats-type", "id": "X1", "timestamp": 1_700_000_000_000_000i64, "someField": [1, 2] }
        ])
        .to_string()
    }

    #[test]
    fn stats_parse_reads_the_real_freeze_keys_and_skips_an_unknown_type() {
        let stats = livekit::webrtc::stats::parse_stats_report(&report_json(30, 30, 0.6, 0.75, 0.004))
            .expect("an unknown stats type must not fail the report");
        assert_eq!(stats.len(), 4);
        assert!(matches!(stats[3], RtcStats::Unknown));
        let RtcStats::InboundRtp(inbound) = &stats[0] else {
            panic!("inbound-rtp first");
        };
        assert_eq!(inbound.inbound.total_freeze_duration, 0.75, "totalFreezesDuration");
        assert_eq!(inbound.inbound.total_pause_duration, 0.2, "totalPausesDuration");
        let counters = InboundRtpCounters::from_stats(&stats).expect("video inbound-rtp");
        assert_eq!(counters.total_freezes_duration, Some(0.75));
        assert_eq!(counters.frames_decoded, Some(30.0));
        assert_eq!(selected_candidate_pair_rtt_ms(&stats), Some(4.0));
        assert!(livekit::webrtc::stats::parse_stats_report("").expect("empty").is_empty());
        assert!(livekit::webrtc::stats::parse_stats_report("[{\"type\":").is_err(), "malformed is an error, not a panic");
    }

    #[test]
    fn present_is_the_hand_off_plus_one_refresh_and_marks_itself_estimated() {
        assert_eq!(present_sample_ms(1_000_000, 1_004_000, 16.0), Some(20.0));
        assert_eq!(present_sample_ms(1_004_000, 1_000_000, 16.0), None, "clock went backwards");
        assert_eq!(present_sample_ms(1_000_000, 1_004_000, 0.0), None, "no refresh rate");
        let aggregator = Aggregator::new(RcRole::Controller);
        let now = Instant::now();
        aggregator.sync_at(true, now);
        record_present_into(&aggregator, 1_000_000, 1_004_000, 8.0);
        let summary = aggregator.sync_at(true, now + rc_latency_telemetry::EMIT_INTERVAL).unwrap();
        assert_eq!(summary.stages[0].0, Stage::ReceiveToPresentMs);
        assert_eq!(summary.stages[0].1.max, 12);
        assert_eq!(summary.dimensions, [(Dimension::PresentTiming, "estimated".to_string())]);
    }

    /// Drives the real per-track loop: timing events and 1 s stats while the
    /// window is controlled, nothing before control or after release, and a
    /// counter reset (resubscribe) contributes no negative delta.
    #[tokio::test(start_paused = true)]
    async fn the_track_loop_records_only_while_the_window_is_controlled() {
        let aggregator: &'static Aggregator = Box::leak(Box::new(Aggregator::new(RcRole::Controller)));
        let started = Instant::now();
        aggregator.sync_at(true, started);
        let controlled = Arc::new(AtomicBool::new(false));
        let polls = Arc::new(AtomicU64::new(0));
        // Each poll answers the next snapshot; snapshot 3 is a counter reset.
        let snapshots = Arc::new(Mutex::new(VecDeque::from([
            report_json(30, 30, 0.6, 0.0, 0.004),
            report_json(60, 60, 1.2, 0.25, 0.006),
            report_json(5, 5, 0.1, 0.0, 0.006),
            report_json(35, 35, 0.7, 0.0, 0.006),
        ])));
        let (tx, rx) = futures::channel::mpsc::unbounded::<SubscribeTimingEvent>();
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let controlled = controlled.clone();
            let polls = polls.clone();
            let snapshots = snapshots.clone();
            let cancel = cancel.clone();
            async move {
                run(
                    Some(rx),
                    move || {
                        polls.fetch_add(1, Ordering::SeqCst);
                        let next = snapshots.lock().unwrap().pop_front();
                        async move { next.and_then(|json| livekit::webrtc::stats::parse_stats_report(&json).ok()) }
                    },
                    move || controlled.load(Ordering::SeqCst),
                    |_| clock(),
                    aggregator,
                    cancel,
                )
                .await;
            }
        });
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };
        // Before control: events and ticks record nothing, and stats are not polled.
        tx.unbounded_send(event(SubscribeTimingStage::WebrtcReceive, 1_040_000, 1_000_000, Some(1))).unwrap();
        settle().await;
        tokio::time::advance(STATS_INTERVAL * 2).await;
        settle().await;
        assert_eq!(polls.load(Ordering::SeqCst), 0, "no stats poll without control");

        controlled.store(true, Ordering::SeqCst);
        for (id, receive, upload, output) in [(2u32, 1_040_000u64, 1_041_000u64, 1_044_000u64), (3, 1_073_000, 1_074_000, 1_076_000)] {
            tx.unbounded_send(event(SubscribeTimingStage::WebrtcReceive, receive, receive - 55_000, Some(id))).unwrap();
            tx.unbounded_send(event(SubscribeTimingStage::DecoderUpload, upload, 1, Some(id))).unwrap();
            tx.unbounded_send(event(SubscribeTimingStage::DecoderOutput, output, 1, Some(id))).unwrap();
        }
        settle().await;
        for _ in 0..4 {
            tokio::time::advance(STATS_INTERVAL).await;
            settle().await;
        }
        assert_eq!(polls.load(Ordering::SeqCst), 4);

        controlled.store(false, Ordering::SeqCst);
        tx.unbounded_send(event(SubscribeTimingStage::WebrtcReceive, 1_090_000, 1_050_000, Some(9))).unwrap();
        settle().await;
        tokio::time::advance(STATS_INTERVAL * 2).await;
        settle().await;
        assert_eq!(polls.load(Ordering::SeqCst), 4, "release stops the poll");
        cancel.cancel();
        task.await.unwrap();

        let summary = aggregator
            .sync_at(true, started + rc_latency_telemetry::EMIT_INTERVAL * 2)
            .expect("an interval with samples");
        let stage = |wanted: Stage| summary.stages.iter().find(|(s, _)| *s == wanted).map(|(_, h)| *h);
        let capture = stage(Stage::CaptureToReceiveMs).expect("capture_to_receive_ms");
        assert_eq!((capture.n, capture.p50, capture.max), (2, 30, 30), "only the two controlled frames");
        let decode = stage(Stage::DecodeMs).expect("per-frame decode_ms");
        assert_eq!((decode.n, decode.max), (2, 3), "joined decode only, no stats average once joined");
        // Snapshots 1->2 and 3->4 give deltas; 2->3 went backwards and gives none.
        let jb = stage(Stage::JbDelayMs).expect("jb_delay_ms");
        assert_eq!((jb.n, jb.p50), (2, 20));
        assert_eq!(stage(Stage::ProcessingMs).unwrap().p50, 40);
        assert_eq!(stage(Stage::SubRttMs).map(|h| h.n), Some(4));
        assert_eq!(summary.counters, [(Counter::FreezeDurationMs, 250)], "unchanged counters are 0 and omitted");
        assert_eq!(summary.max_counters, [(MaxCounter::OffsetErrMs, 2), (MaxCounter::PacketsLostPctX100, 0)]);
        assert!(summary.dimensions.contains(&(Dimension::FrameMetadata, "available".to_string())));
        assert!(summary.dimensions.contains(&(Dimension::JbMinMs, "0".to_string())));
    }

    fn body_after<'a>(source: &'a str, anchor: &str) -> &'a str {
        let at = source.find(anchor).unwrap_or_else(|| panic!("missing {anchor}"));
        &source[at..]
    }

    /// Wiring guard: a live subscription needs a LiveKit room (no test
    /// harness), so this pins the order the SDK requires in both receive
    /// loops, and that each compositor records the stamp at its hand-off.
    #[test]
    fn both_receive_loops_subscribe_before_the_stream_and_both_compositors_record() {
        let subscriber = include_str!("transport/subscriber.rs");
        let subscriber = subscriber.split_once("\n#[cfg(test)]\nmod tests {").unwrap().0;
        for anchor in ["let video_track_for_stats = video_track.clone();", "fn spawn_windows_decode_loop("] {
            let body = body_after(subscriber, anchor);
            let spawn = body.find("rc_receiver_timing::spawn_for_window_track(").expect(anchor);
            let stream = body.find("NativeVideoStream::new(").expect(anchor);
            assert!(spawn < stream, "{anchor}: timing must subscribe before the stream exists");
        }
        assert_eq!(subscriber.matches("rc_receiver_timing::controlled_frame_stamp(").count(), 2);
        let mac = include_str!("compositor.rs");
        let drain = body_after(mac, "fn drain_pending_display_samples_on_main(");
        let drain = &drain[..drain.find("\n}\n").unwrap()];
        assert!(drain.contains("pending.present_stamp"));
        assert!(drain.contains("rc_receiver_timing::record_present("));
        let windows = include_str!("windows_compositor.rs");
        let present = body_after(windows, "fn present_frame(");
        let present = &present[..present.find("\n}\n").unwrap()];
        let presented = present.rfind("swap_chain.Present(").unwrap();
        let recorded = present.find("rc_receiver_timing::record_present(").unwrap();
        assert!(presented < recorded, "Windows records only after a successful Present");
        assert!(present.contains("frame.present_stamp.take()"), "a re-present never records twice");
    }
}
