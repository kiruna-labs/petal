//! #373: host video-send timing for a shared window under remote control.
//!
//! Fed only while a controller holds a grant on that window
//! (`remote_control::host_window_grant`), from the window track's real
//! publish path, on macOS and Windows alike (one shared publisher):
//! - publish timing events (`LocalVideoTrack::publish_timing_events`), joined
//!   by `frame_id`: `capture_to_encoded_ms` = EncoderOutput - capture stamp,
//!   `encoded_to_packetized_ms` = WebrtcPacketize - EncoderOutput. Both
//!   stamps are host wall clock, so no offset is involved. With simulcast a
//!   frame is encoded once per layer; a frame records its slowest layer;
//! - a 1 s publisher stats poll: `pub_rtt_ms`, the selected candidate pair's
//!   RTT (same rule as the controller's `sub_rtt_ms`).
//!
//! The event stream is subscribed when control of the window starts and
//! dropped when it ends. Per-frame values go into the histograms only;
//! nothing here logs per frame.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::Stream;
use livekit::prelude::{LocalVideoTrack, PublishTimingEvent, PublishTimingStage};
use livekit::webrtc::stats::RtcStats;
use tokio_util::sync::CancellationToken;

use crate::rc_latency_telemetry::{self, Aggregator, RcRole, Stage};
use crate::rc_receiver_timing::{finish_poll, next_event, selected_candidate_pair_rtt_ms, STATS_INTERVAL};
use crate::time_util::now_us;

const STATS_TIMEOUT: Duration = Duration::from_secs(2);
/// Frames whose layers may still be arriving. Beyond this the oldest is
/// recorded with the layers it has.
const MAX_PENDING_FRAMES: usize = 4;
/// A pending frame with no event for this long is complete (the source went
/// idle, so no newer frame will push it out).
const FRAME_SETTLE_US: u64 = 250_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SendSample {
    CaptureToEncoded { ms: f64 },
    EncodedToPacketized { ms: f64 },
}

#[derive(Debug)]
struct PendingFrame {
    frame_id: u32,
    last_event_us: u64,
    /// The latest layer's EncoderOutput, which its packetize pairs with.
    last_output_us: u64,
    encoded_ms: f64,
    packetized_ms: Option<f64>,
}

impl PendingFrame {
    fn finish(self, emit: &mut impl FnMut(SendSample)) {
        emit(SendSample::CaptureToEncoded { ms: self.encoded_ms });
        if let Some(ms) = self.packetized_ms {
            emit(SendSample::EncodedToPacketized { ms });
        }
    }
}

/// Joins one track's publish timing events by `frame_id`. Bounded, and a
/// missing stage or a frame without a capture stamp yields no sample.
#[derive(Debug, Default)]
pub(crate) struct SendTimingJoiner {
    frames: VecDeque<PendingFrame>,
}

impl SendTimingJoiner {
    pub(crate) fn observe(&mut self, event: &PublishTimingEvent, emit: &mut impl FnMut(SendSample)) {
        let Some(id) = event.frame_id else {
            return;
        };
        match event.stage {
            PublishTimingStage::EncoderUpload => {}
            PublishTimingStage::EncoderOutput => {
                if event.capture_timestamp_us == 0 {
                    return;
                }
                let Some(us) = event.timestamp_us.checked_sub(event.capture_timestamp_us) else {
                    return;
                };
                let ms = us as f64 / 1000.0;
                if let Some(frame) = self.frames.iter_mut().find(|f| f.frame_id == id) {
                    frame.encoded_ms = frame.encoded_ms.max(ms);
                    frame.last_output_us = event.timestamp_us;
                    frame.last_event_us = event.timestamp_us;
                    return;
                }
                if self.frames.len() >= MAX_PENDING_FRAMES {
                    if let Some(oldest) = self.frames.pop_front() {
                        oldest.finish(emit);
                    }
                }
                self.frames.push_back(PendingFrame {
                    frame_id: id,
                    last_event_us: event.timestamp_us,
                    last_output_us: event.timestamp_us,
                    encoded_ms: ms,
                    packetized_ms: None,
                });
            }
            PublishTimingStage::WebrtcPacketize => {
                let Some(frame) = self.frames.iter_mut().find(|f| f.frame_id == id) else {
                    return;
                };
                let Some(us) = event.timestamp_us.checked_sub(frame.last_output_us) else {
                    return;
                };
                let ms = us as f64 / 1000.0;
                frame.packetized_ms = Some(frame.packetized_ms.map_or(ms, |m| m.max(ms)));
                frame.last_event_us = event.timestamp_us;
            }
        }
    }

    /// Record every frame that has had no event for [`FRAME_SETTLE_US`].
    pub(crate) fn flush_settled(&mut self, now_us: u64, emit: &mut impl FnMut(SendSample)) {
        while self
            .frames
            .front()
            .is_some_and(|f| now_us.saturating_sub(f.last_event_us) >= FRAME_SETTLE_US)
        {
            if let Some(frame) = self.frames.pop_front() {
                frame.finish(emit);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.frames.len()
    }
}

fn record(aggregator: &Aggregator) -> impl FnMut(SendSample) + '_ {
    move |sample| match sample {
        SendSample::CaptureToEncoded { ms } => aggregator.record_stage(Stage::CaptureToEncodedMs, ms),
        SendSample::EncodedToPacketized { ms } => aggregator.record_stage(Stage::EncodedToPacketizedMs, ms),
    }
}

/// One window track's send-timing state, bound to one grant identity.
#[derive(Debug, Default)]
pub(crate) struct HostSendTiming {
    grant: Option<u64>,
    joiner: SendTimingJoiner,
}

impl HostSendTiming {
    pub(crate) fn on_event(&mut self, event: &PublishTimingEvent, grant: u64, aggregator: &Aggregator) {
        self.adopt(grant);
        self.joiner.observe(event, &mut record(aggregator));
    }

    pub(crate) fn on_tick(&mut self, grant: u64, now_us: u64, aggregator: &Aggregator) {
        self.adopt(grant);
        self.joiner.flush_settled(now_us, &mut record(aggregator));
    }

    pub(crate) fn on_stats(&self, stats: Option<&[RtcStats]>, aggregator: &Aggregator) {
        if let Some(rtt) = stats.and_then(selected_candidate_pair_rtt_ms) {
            aggregator.record_stage(Stage::PubRttMs, rtt);
        }
    }

    /// Bind to `grant`, dropping frames gathered under any other one.
    fn adopt(&mut self, grant: u64) {
        if self.grant != Some(grant) {
            self.joiner.clear();
            self.grant = Some(grant);
        }
    }

    pub(crate) fn reset(&mut self) {
        self.grant = None;
        self.joiner.clear();
    }
}

/// The per-track loop. `subscribe` is called when control of the window
/// starts and its stream dropped when control ends; `grant` is read live.
/// The stats poll runs beside the event drain and lands only if the same
/// grant is still live when it returns.
pub(crate) async fn run<S, E, F, Fut>(
    mut subscribe: S,
    mut poll_stats: F,
    grant: impl Fn() -> Option<u64>,
    now_us: impl Fn() -> u64,
    aggregator: &Aggregator,
    cancel: CancellationToken,
) where
    S: FnMut() -> E,
    E: Stream<Item = PublishTimingEvent> + Unpin,
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<RtcStats>>>,
{
    let mut state = HostSendTiming::default();
    let mut events: Option<E> = None;
    let mut in_flight: Option<(u64, Pin<Box<Fut>>)> = None;
    let mut tick = tokio::time::interval(STATS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            stats = finish_poll(&mut in_flight) => {
                let polled_under = in_flight.take().map(|(g, _)| g);
                if grant().is_some_and(|live| polled_under == Some(live) && state.grant == Some(live)) {
                    state.on_stats(stats.as_deref(), aggregator);
                }
            }
            _ = tick.tick() => match grant() {
                None => {
                    state.reset();
                    events = None;
                }
                Some(live) => {
                    if events.is_none() {
                        events = Some(subscribe());
                    }
                    state.on_tick(live, now_us(), aggregator);
                    if in_flight.is_none() {
                        in_flight = Some((live, Box::pin(poll_stats())));
                    }
                }
            },
            event = next_event(&mut events) => match (event, grant()) {
                (Some(event), Some(live)) => state.on_event(&event, live, aggregator),
                (Some(_), None) => {
                    state.reset();
                    events = None;
                }
                (None, _) => events = None,
            },
        }
    }
}

/// Start send timing for one published window track (macOS and Windows).
/// The track is published with frame metadata, so its sender transformer
/// already exists; subscribing only attaches an observer to it.
pub(crate) fn spawn_for_window_track(track: &LocalVideoTrack, window_id: u32, cancel: CancellationToken) {
    let events_track = track.clone();
    let stats_track = track.clone();
    tokio::spawn(async move {
        run(
            move || Box::pin(events_track.publish_timing_events()),
            move || {
                let track = stats_track.clone();
                async move {
                    tokio::time::timeout(STATS_TIMEOUT, track.get_stats())
                        .await
                        .ok()?
                        .ok()
                }
            },
            move || crate::remote_control::host_window_grant(window_id),
            now_us,
            rc_latency_telemetry::aggregator(RcRole::Host),
            cancel,
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn event(stage: PublishTimingStage, timestamp_us: u64, capture_us: u64, frame_id: Option<u32>) -> PublishTimingEvent {
        PublishTimingEvent {
            stage,
            timestamp_us,
            capture_timestamp_us: capture_us,
            frame_id,
        }
    }

    fn output(id: u32, at: u64, capture: u64) -> PublishTimingEvent {
        event(PublishTimingStage::EncoderOutput, at, capture, Some(id))
    }

    fn packetize(id: u32, at: u64) -> PublishTimingEvent {
        event(PublishTimingStage::WebrtcPacketize, at, 1, Some(id))
    }

    fn collect(joiner: &mut SendTimingJoiner, events: &[PublishTimingEvent]) -> Vec<SendSample> {
        let mut out = Vec::new();
        for e in events {
            joiner.observe(e, &mut |s| out.push(s));
        }
        out
    }

    #[test]
    fn a_frame_records_its_slowest_layer_once_it_retires() {
        let mut joiner = SendTimingJoiner::default();
        let samples = collect(
            &mut joiner,
            &[
                event(PublishTimingStage::EncoderUpload, 1_001_000, 1_000_000, Some(1)),
                output(1, 1_006_000, 1_000_000), // low layer
                packetize(1, 1_006_500),
                output(1, 1_012_000, 1_000_000), // top layer
                packetize(1, 1_012_200),
            ],
        );
        assert!(samples.is_empty(), "nothing until the frame retires");
        let mut out = Vec::new();
        joiner.flush_settled(1_012_200 + FRAME_SETTLE_US - 1, &mut |s| out.push(s));
        assert!(out.is_empty(), "not settled yet");
        joiner.flush_settled(1_012_200 + FRAME_SETTLE_US, &mut |s| out.push(s));
        assert_eq!(
            out,
            [SendSample::CaptureToEncoded { ms: 12.0 }, SendSample::EncodedToPacketized { ms: 0.5 }]
        );
        assert_eq!(joiner.pending(), 0);
    }

    #[test]
    fn out_of_order_layers_join_by_frame_id_and_a_missing_stage_gives_no_sample() {
        let mut joiner = SendTimingJoiner::default();
        let mut samples = collect(
            &mut joiner,
            &[
                packetize(9, 900_000),           // packetize before any output: dropped
                output(2, 1_040_000, 1_033_000), // frame 2 low layer
                output(3, 1_072_000, 1_066_000), // frame 3 before frame 2's top layer
                output(2, 1_045_000, 1_033_000), // frame 2 top layer, late
                packetize(2, 1_046_000),
                output(4, 1_100_000, 1_099_000), // no capture stamp below
                event(PublishTimingStage::EncoderOutput, 1_101_000, 0, Some(5)),
                event(PublishTimingStage::EncoderOutput, 1_101_000, 1_100_000, None),
                output(6, 1_000_000, 1_100_000), // clock step: capture after output
            ],
        );
        joiner.flush_settled(u64::MAX, &mut |s| samples.push(s));
        assert_eq!(
            samples,
            [
                SendSample::CaptureToEncoded { ms: 12.0 },
                SendSample::EncodedToPacketized { ms: 1.0 },
                SendSample::CaptureToEncoded { ms: 6.0 },
                SendSample::CaptureToEncoded { ms: 1.0 },
            ],
            "frame 3 never packetized: encode sample only"
        );
    }

    #[test]
    fn pending_frames_are_bounded_and_the_oldest_retires_first() {
        let mut joiner = SendTimingJoiner::default();
        let frames: Vec<_> = (1..=(MAX_PENDING_FRAMES as u32 + 2))
            .map(|id| output(id, 2_000_000 + u64::from(id) * 1_000, 2_000_000))
            .collect();
        let samples = collect(&mut joiner, &frames);
        assert_eq!(joiner.pending(), MAX_PENDING_FRAMES);
        assert_eq!(
            samples,
            [SendSample::CaptureToEncoded { ms: 1.0 }, SendSample::CaptureToEncoded { ms: 2.0 }]
        );
        assert!(collect(&mut joiner, &[packetize(1, 2_010_000)]).is_empty(), "retired frames never pair");
    }

    fn report_json(rtt_s: f64) -> String {
        serde_json::json!([
            {
                "type": "outbound-rtp", "id": "OT01V", "timestamp": 1_700_000_000_000_000i64,
                "ssrc": 4321, "kind": "video", "transportId": "T01", "codecId": "C01",
                "packetsSent": 300, "bytesSent": 300_000, "framesEncoded": 30
            },
            { "type": "transport", "id": "T01", "timestamp": 1_700_000_000_000_000i64, "selectedCandidatePairId": "CP01" },
            {
                "type": "candidate-pair", "id": "CP01", "timestamp": 1_700_000_000_000_000i64, "transportId": "T01",
                "localCandidateId": "L1", "remoteCandidateId": "R1", "state": "succeeded",
                "nominated": true, "currentRoundTripTime": rtt_s
            }
        ])
        .to_string()
    }

    #[test]
    fn publisher_rtt_comes_from_the_selected_candidate_pair() {
        let stats = livekit::webrtc::stats::parse_stats_report(&report_json(0.018)).expect("parses");
        let aggregator = Aggregator::new(RcRole::Host);
        let now = Instant::now();
        aggregator.sync_at(true, now);
        HostSendTiming::default().on_stats(Some(&stats), &aggregator);
        HostSendTiming::default().on_stats(None, &aggregator);
        let zero = livekit::webrtc::stats::parse_stats_report(&report_json(0.0)).unwrap();
        HostSendTiming::default().on_stats(Some(&zero), &aggregator);
        let summary = aggregator.sync_at(true, now + rc_latency_telemetry::EMIT_INTERVAL).unwrap();
        assert_eq!(summary.stages.len(), 1);
        let (stage, stats) = summary.stages[0];
        assert_eq!(stage, Stage::PubRttMs);
        assert_eq!((stats.n, stats.p50), (1, 18), "an absent (0) RTT is no sample");
    }

    /// Drives the real per-track loop: no subscription or poll before
    /// control, events and 1 s stats while controlled, a regrant drops the
    /// old grant's pending frames, and release unsubscribes.
    #[tokio::test(start_paused = true)]
    async fn the_track_loop_subscribes_and_records_only_while_the_window_is_controlled() {
        let aggregator: &'static Aggregator = Box::leak(Box::new(Aggregator::new(RcRole::Host)));
        let started = Instant::now();
        aggregator.sync_at(true, started);
        // 0 = no grant.
        let grant = Arc::new(AtomicU64::new(0));
        let polls = Arc::new(AtomicU64::new(0));
        let clock_us = Arc::new(AtomicU64::new(5_000_000));
        let senders: Arc<Mutex<Vec<futures::channel::mpsc::UnboundedSender<PublishTimingEvent>>>> =
            Arc::default();
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let grant = grant.clone();
            let polls = polls.clone();
            let clock_us = clock_us.clone();
            let senders = senders.clone();
            let cancel = cancel.clone();
            async move {
                run(
                    move || {
                        let (tx, rx) = futures::channel::mpsc::unbounded();
                        senders.lock().unwrap().push(tx);
                        rx
                    },
                    move || {
                        polls.fetch_add(1, Ordering::SeqCst);
                        async { livekit::webrtc::stats::parse_stats_report(&report_json(0.012)).ok() }
                    },
                    move || Some(grant.load(Ordering::SeqCst)).filter(|g| *g != 0),
                    move || clock_us.load(Ordering::SeqCst),
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
        let send = |e: PublishTimingEvent| {
            senders.lock().unwrap().last().unwrap().unbounded_send(e).is_ok()
        };
        settle().await;
        tokio::time::advance(STATS_INTERVAL * 2).await;
        settle().await;
        assert!(senders.lock().unwrap().is_empty(), "no subscription without control");
        assert_eq!(polls.load(Ordering::SeqCst), 0, "no stats poll without control");

        grant.store(7, Ordering::SeqCst);
        tokio::time::advance(STATS_INTERVAL).await;
        settle().await;
        assert_eq!(senders.lock().unwrap().len(), 1, "control subscribes");
        for (id, capture) in [(1u32, 4_900_000u64), (2, 4_933_000)] {
            assert!(send(output(id, capture + 8_000, capture)));
            assert!(send(packetize(id, capture + 8_300)));
        }
        settle().await;
        clock_us.store(5_300_000, Ordering::SeqCst);
        tokio::time::advance(STATS_INTERVAL).await;
        settle().await; // frames 1 and 2 settle under grant 7
        assert!(send(output(3, 5_310_000, 5_302_000)));
        settle().await;
        grant.store(8, Ordering::SeqCst);
        // The regrant drops frame 3, still pending under grant 7.
        assert!(send(output(4, 5_320_000, 5_310_000)));
        settle().await;
        clock_us.store(5_600_000, Ordering::SeqCst);
        tokio::time::advance(STATS_INTERVAL).await;
        settle().await; // frame 4 settles under grant 8
        let polled_while_controlled = polls.load(Ordering::SeqCst);
        assert!(polled_while_controlled >= 3, "polled {polled_while_controlled}");

        grant.store(0, Ordering::SeqCst);
        tokio::time::advance(STATS_INTERVAL).await;
        settle().await;
        assert!(!send(output(5, 5_700_000, 5_690_000)), "release dropped the stream");
        tokio::time::advance(STATS_INTERVAL * 2).await;
        settle().await;
        assert_eq!(polls.load(Ordering::SeqCst), polled_while_controlled, "release stops the poll");
        cancel.cancel();
        task.await.unwrap();

        let summary = aggregator
            .sync_at(true, started + rc_latency_telemetry::EMIT_INTERVAL * 2)
            .expect("an interval with samples");
        let stage = |wanted: Stage| summary.stages.iter().find(|(s, _)| *s == wanted).map(|(_, h)| *h);
        let encoded = stage(Stage::CaptureToEncodedMs).expect("capture_to_encoded_ms");
        assert_eq!((encoded.n, encoded.p50, encoded.max), (3, 8, 10), "frames 1, 2 and 4");
        let packetized = stage(Stage::EncodedToPacketizedMs).expect("encoded_to_packetized_ms");
        assert_eq!(packetized.n, 2, "only frames 1 and 2 were packetized");
        let rtt = stage(Stage::PubRttMs).expect("pub_rtt_ms");
        assert_eq!((rtt.n as u64, rtt.p50), (polled_while_controlled, 12));
    }

    /// Wiring guard: a live publication needs a LiveKit room, so this pins
    /// that every window publication (macOS and Windows share one path)
    /// starts send timing under its own cancel token.
    #[test]
    fn every_window_publication_starts_send_timing() {
        let publisher = include_str!("transport/publisher.rs");
        let publisher = publisher.split_once("\n#[cfg(test)]\nmod tests {").map_or(publisher, |(body, _)| body);
        assert_eq!(publisher.matches("rc_host_send_timing::spawn_for_window_track(").count(), 1);
        let body = &publisher[publisher.find("async fn publish_window_at_with_encoder_context(").unwrap()..];
        let body = &body[..body.find("\n    }\n").unwrap()];
        let published = body.find(".publish_track(").unwrap();
        let spawned = body.find("rc_host_send_timing::spawn_for_window_track(").expect("spawned");
        assert!(published < spawned, "subscribe after publish: the transformer exists then");
    }
}
