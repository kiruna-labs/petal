//! #399: redundant lossy transport for remote-control input.
//!
//! Discrete input used to ride LiveKit's reliable, ordered data channel. One
//! lost SCTP packet there holds back every input queued behind it until the
//! retransmit timer fires (measured: six keystrokes all landing 449ms late at
//! 1% loss). The SFU's SCTP timers cannot be tuned from here, so a capable
//! controller instead publishes every non-hover input in an `inputBundle` on
//! the lossy (unordered, no-retransmit) channel. Each bundle carries the new
//! input plus the few before it, so one lost packet is recovered by the next
//! one; a trailing copy covers the last input of a burst.
//!
//! The host dedupes by the bundle's own sequence (`rseq`) and replays in
//! strict order. A gap is held for at most [`GAP_HOLD`]; an unrecoverable gap
//! is skipped and the caller releases held input, because the lost packet may
//! have been an Up.
//!
//! Capability-gated: a host advertises `supportsRedundantInput` on its
//! `active` status. A controller that never saw it keeps the legacy reliable
//! ordered path, and an old controller never sends a bundle.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::remote_control_core::{RemoteControlAction, RemoteControlMessage, RemoteControlType};

/// JSON `kind` of a bundle packet on the remote-control topic.
pub(crate) const BUNDLE_KIND: &str = "inputBundle";
pub(crate) const BUNDLE_VERSION: u8 = 1;
/// Already-published inputs resent in each bundle after the new ones.
pub(crate) const MAX_REDUNDANT_COPIES: usize = 3;
/// New inputs in one bundle; reaching it flushes without waiting for pacing.
pub(crate) const MAX_BUNDLE_INPUTS: usize = 48;
/// Redundant copies are added only while the bundle stays under this, so a
/// typical bundle fits one SCTP chunk. New inputs are always sent.
pub(crate) const MAX_BUNDLE_PAYLOAD_BYTES: usize = 1_200;
/// Older inputs past this age are no longer worth resending: the host has
/// either delivered them or skipped the gap by then.
pub(crate) const MAX_REDUNDANT_AGE: Duration = Duration::from_millis(500);
/// At most one bundle per interval; inputs arriving sooner ride the next one.
/// A same-millisecond burst of one-bundle-per-input overflowed the lossy
/// channel's send buffer and LiveKit silently dropped the tail (#399 VM run
/// 39912633182: gaps on the CLEAN pass).
pub(crate) const MIN_BUNDLE_INTERVAL: Duration = Duration::from_millis(8);
/// Each bundle is published this many times back to back, so one loss does
/// not depend on a timer (background-tab timers can be throttled).
pub(crate) const IMMEDIATE_COPIES: usize = 2;
/// After the last bundle of a burst it is resent at these offsets, so a lost
/// final keystroke or Up is still recovered.
pub(crate) const TRAILING_COPY_DELAYS: [Duration; 2] =
    [Duration::from_millis(30), Duration::from_millis(100)];
/// Longest the host holds later inputs waiting for a missing one.
pub(crate) const GAP_HOLD: Duration = Duration::from_millis(60);
/// Out-of-order inputs the host buffers before it force-skips the gap.
pub(crate) const MAX_BUFFERED_INPUTS: usize = 64;
const RETIRED_STREAMS_KEPT: usize = 8;

/// Wire packet. `inputs[i]` has rseq `first + i`; rseq starts at 1 per stream
/// and a controller opens a new `stream` for every grant it is given.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputBundle {
    pub v: u8,
    pub kind: String,
    pub window_id: u32,
    pub stream: String,
    pub first: u64,
    pub inputs: Vec<RemoteControlMessage>,
}

/// Every controller input except a hover move. Hover moves stay lossy and
/// latest-wins with no redundancy: a newer move replaces a lost one.
pub(crate) fn rides_redundant_stream(message: &RemoteControlMessage) -> bool {
    match message.message_type {
        RemoteControlType::Pointer => !(message.action == Some(RemoteControlAction::Move)
            && message.buttons.unwrap_or(0) == 0),
        RemoteControlType::Wheel | RemoteControlType::Key | RemoteControlType::Text => true,
        _ => false,
    }
}

/// Parses a bundle; `None` for any other remote-control packet.
pub(crate) fn parse_input_bundle(payload: &[u8]) -> Option<InputBundle> {
    // Cheap pre-check so ordinary packets skip a second parse.
    let needle = BUNDLE_KIND.as_bytes();
    if !payload.windows(needle.len()).any(|window| window == needle) {
        return None;
    }
    let bundle: InputBundle = serde_json::from_slice(payload).ok()?;
    let valid = bundle.v == BUNDLE_VERSION
        && bundle.kind == BUNDLE_KIND
        && !bundle.stream.is_empty()
        && bundle.first >= 1
        && !bundle.inputs.is_empty()
        && bundle.inputs.len() <= MAX_BUNDLE_INPUTS + MAX_REDUNDANT_COPIES
        && bundle
            .inputs
            .iter()
            .all(|input| input.window_id == bundle.window_id && rides_redundant_stream(input));
    valid.then_some(bundle)
}

#[derive(Debug, Clone)]
struct SentInput {
    rseq: u64,
    queued_at: Instant,
    encoded_len: usize,
    message: RemoteControlMessage,
}

/// What a controller does with a pushed input.
#[derive(Debug)]
pub(crate) enum PushOutcome {
    /// Publish this bundle now (flush number for its trailing copies).
    Publish(InputBundle, u64),
    /// Paced: call `flush` after this long.
    FlushAfter(Duration),
    /// A flush is already scheduled and will carry this input.
    Queued,
}

/// Controller side: one per (window, host) grant.
#[derive(Debug)]
pub(crate) struct RedundantInputSender {
    window_id: u32,
    stream: String,
    next_rseq: u64,
    /// Published inputs kept for redundancy, then `unsent` new ones.
    recent: VecDeque<SentInput>,
    unsent: usize,
    last_flush_at: Option<Instant>,
    flush_scheduled: bool,
    flushes: u64,
    last_flush_len: usize,
}

impl RedundantInputSender {
    pub(crate) fn new(window_id: u32, stream: String) -> Self {
        Self {
            window_id,
            stream,
            next_rseq: 1,
            recent: VecDeque::new(),
            unsent: 0,
            last_flush_at: None,
            flush_scheduled: false,
            flushes: 0,
            last_flush_len: 0,
        }
    }

    pub(crate) fn stream(&self) -> &str {
        &self.stream
    }

    /// Assigns the next rseq and paces publication.
    pub(crate) fn push(&mut self, message: RemoteControlMessage, now: Instant) -> PushOutcome {
        let encoded_len = serde_json::to_vec(&message).map_or(0, |bytes| bytes.len());
        let rseq = self.next_rseq;
        self.next_rseq = self.next_rseq.saturating_add(1);
        self.recent.push_back(SentInput {
            rseq,
            queued_at: now,
            encoded_len,
            message,
        });
        self.unsent += 1;
        let wait = self.last_flush_at.map_or(Duration::ZERO, |last| {
            MIN_BUNDLE_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        });
        if wait.is_zero() || self.unsent >= MAX_BUNDLE_INPUTS {
            let bundle = self.flush(now).expect("an input was just queued");
            return PushOutcome::Publish(bundle, self.flushes);
        }
        if self.flush_scheduled {
            return PushOutcome::Queued;
        }
        self.flush_scheduled = true;
        PushOutcome::FlushAfter(wait)
    }

    /// Builds the bundle for every queued input; `None` when nothing is queued
    /// (a forced flush already sent it).
    pub(crate) fn flush(&mut self, now: Instant) -> Option<InputBundle> {
        self.flush_scheduled = false;
        if self.unsent == 0 {
            return None;
        }
        let bundle = self.build(now);
        self.last_flush_len = self.unsent;
        self.unsent = 0;
        self.flushes += 1;
        self.last_flush_at = Some(now);
        while self.recent.len() > self.last_flush_len + MAX_REDUNDANT_COPIES {
            self.recent.pop_front();
        }
        Some(bundle)
    }

    pub(crate) fn flushes(&self) -> u64 {
        self.flushes
    }

    /// The trailing copy of flush `flush`, unless a newer input superseded it.
    pub(crate) fn trailing(&self, flush: u64, now: Instant) -> Option<InputBundle> {
        (self.flushes == flush && self.unsent == 0 && !self.recent.is_empty())
            .then(|| self.build_with_newest(now, self.last_flush_len))
    }

    fn build(&self, now: Instant) -> InputBundle {
        self.build_with_newest(now, self.unsent)
    }

    /// The `fresh` newest inputs plus as many contiguous older ones as fit
    /// the copy, age, and size bounds.
    fn build_with_newest(&self, now: Instant, fresh: usize) -> InputBundle {
        let fresh = fresh.clamp(1, self.recent.len());
        let mut taken = fresh;
        let mut bytes: usize = self
            .recent
            .iter()
            .rev()
            .take(fresh)
            .map(|input| input.encoded_len)
            .sum();
        for older in self.recent.iter().rev().skip(fresh) {
            if taken - fresh >= MAX_REDUNDANT_COPIES
                || now.saturating_duration_since(older.queued_at) > MAX_REDUNDANT_AGE
                || bytes + older.encoded_len > MAX_BUNDLE_PAYLOAD_BYTES
            {
                break;
            }
            taken += 1;
            bytes += older.encoded_len;
        }
        let inputs: Vec<_> = self.recent.iter().skip(self.recent.len() - taken).collect();
        InputBundle {
            v: BUNDLE_VERSION,
            kind: BUNDLE_KIND.to_string(),
            window_id: self.window_id,
            stream: self.stream.clone(),
            first: inputs[0].rseq,
            inputs: inputs.into_iter().map(|input| input.message.clone()).collect(),
        }
    }
}

/// What the host should do after one sequencer step.
#[derive(Debug, Default)]
pub(crate) struct SequencerOutput {
    /// Inputs to hand to the ordinary per-message path, in order.
    pub deliver: Vec<RemoteControlMessage>,
    /// Delivered inputs that were not the newest in the bundle that carried
    /// them (a redundant copy or a paced batch).
    pub recovered: u64,
    pub duplicates: u64,
    /// Inputs given up on. Non-zero means held input must be released first.
    pub skipped: u64,
    /// A gap is open; flush again after [`GAP_HOLD`].
    pub arm_gap_timer: bool,
    pub stale_stream: bool,
}

/// Host side: one per (trusted sender, window).
#[derive(Debug, Default)]
pub(crate) struct InputSequencer {
    stream: Option<String>,
    next: u64,
    buffered: BTreeMap<u64, RemoteControlMessage>,
    gap_since: Option<Instant>,
    retired: VecDeque<String>,
}

impl InputSequencer {
    pub(crate) fn accept(&mut self, bundle: InputBundle, now: Instant) -> SequencerOutput {
        let mut out = SequencerOutput::default();
        if self.stream.as_deref() != Some(bundle.stream.as_str()) {
            if self.retired.iter().any(|retired| *retired == bundle.stream) {
                out.stale_stream = true;
                return out;
            }
            // A new grant: whatever the old stream still buffered is
            // delivered (skipping its gap) before the new stream starts.
            self.skip_gap(&mut out);
            if let Some(old) = self.stream.take() {
                self.retired.push_back(old);
                while self.retired.len() > RETIRED_STREAMS_KEPT {
                    self.retired.pop_front();
                }
            }
            self.stream = Some(bundle.stream.clone());
            self.next = 1;
            self.buffered.clear();
            self.gap_since = None;
        }
        let newest = bundle.first + bundle.inputs.len() as u64 - 1;
        for (offset, input) in bundle.inputs.into_iter().enumerate() {
            let rseq = bundle.first + offset as u64;
            if rseq < self.next || self.buffered.contains_key(&rseq) {
                out.duplicates += 1;
                continue;
            }
            if rseq != newest {
                out.recovered += 1;
            }
            self.buffered.insert(rseq, input);
        }
        let next_before = self.next;
        self.drain_contiguous(&mut out);
        if self.buffered.len() > MAX_BUFFERED_INPUTS {
            self.skip_gap(&mut out);
        }
        self.update_gap(now, next_before, &mut out);
        out
    }

    /// Skips the gap once it has been held for [`GAP_HOLD`].
    pub(crate) fn flush_expired(&mut self, now: Instant) -> SequencerOutput {
        let mut out = SequencerOutput::default();
        let Some(since) = self.gap_since else {
            return out;
        };
        if now.saturating_duration_since(since) < GAP_HOLD {
            out.arm_gap_timer = true;
            return out;
        }
        let next_before = self.next;
        self.skip_gap(&mut out);
        self.update_gap(now, next_before, &mut out);
        out
    }

    fn drain_contiguous(&mut self, out: &mut SequencerOutput) {
        while let Some(input) = self.buffered.remove(&self.next) {
            out.deliver.push(input);
            self.next = self.next.saturating_add(1);
        }
    }

    fn skip_gap(&mut self, out: &mut SequencerOutput) {
        let Some(&lowest) = self.buffered.keys().next() else {
            return;
        };
        out.skipped += lowest.saturating_sub(self.next);
        self.next = lowest;
        self.drain_contiguous(out);
    }

    fn update_gap(&mut self, now: Instant, next_before: u64, out: &mut SequencerOutput) {
        if self.buffered.is_empty() {
            self.gap_since = None;
        } else if self.gap_since.is_none() || self.next != next_before {
            // A fresh hole gets its own full hold.
            self.gap_since = Some(now);
            out.arm_gap_timer = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seq: u64, action: &str, key: &str) -> RemoteControlMessage {
        serde_json::from_value(serde_json::json!({
            "v": 1, "kind": "key", "action": action, "targetUserId": "host",
            "controllerId": "ctl", "windowId": 7, "seq": seq, "key": key,
            "code": format!("Key{}", key.to_uppercase()), "grantToken": "tok",
            "modifiers": {"alt": false, "ctrl": false, "meta": false, "shift": false}
        }))
        .unwrap()
    }

    fn json(bundle: &InputBundle) -> serde_json::Value {
        serde_json::to_value(bundle).unwrap()
    }

    fn seqs(messages: &[RemoteControlMessage]) -> Vec<u64> {
        messages.iter().map(|message| message.seq).collect()
    }

    fn published(outcome: PushOutcome) -> (InputBundle, u64) {
        match outcome {
            PushOutcome::Publish(bundle, flush) => (bundle, flush),
            other => panic!("expected an immediate publish, got {other:?}"),
        }
    }

    /// Keydowns 10ms apart (slower than the pacing interval): one bundle each.
    fn send_burst(sender: &mut RedundantInputSender, count: u64, t0: Instant) -> Vec<InputBundle> {
        (1..=count)
            .map(|seq| {
                published(sender.push(key(seq, "down", "a"), t0 + Duration::from_millis(seq * 10))).0
            })
            .collect()
    }

    #[test]
    fn dropped_packet_is_recovered_from_the_next_bundle() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        let bundles = send_burst(&mut sender, 3, t0);
        let mut host = InputSequencer::default();
        assert_eq!(seqs(&host.accept(bundles[0].clone(), t0).deliver), vec![1]);
        // bundles[1] is lost on the wire.
        let out = host.accept(bundles[2].clone(), t0 + Duration::from_millis(30));
        assert_eq!(seqs(&out.deliver), vec![2, 3]);
        assert_eq!(out.recovered, 1);
        assert_eq!(out.skipped, 0);
        assert!(!out.arm_gap_timer);
    }

    #[test]
    fn lost_final_input_is_recovered_by_the_trailing_copy() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        let first = published(sender.push(key(1, "down", "a"), t0)).0;
        let (last, flush) = published(sender.push(key(2, "up", "a"), t0 + Duration::from_millis(20)));
        let mut host = InputSequencer::default();
        assert_eq!(seqs(&host.accept(first, t0).deliver), vec![1]);
        // `last` (the burst's final input) and its immediate copy are lost; the
        // trailing copy sent TRAILING_COPY_DELAYS[0] later carries the same inputs.
        let trailing = sender
            .trailing(flush, t0 + Duration::from_millis(20) + TRAILING_COPY_DELAYS[0])
            .unwrap();
        assert_eq!(json(&trailing), json(&last));
        assert_eq!(seqs(&host.accept(trailing.clone(), t0).deliver), vec![2]);
        let out = host.accept(trailing, t0);
        assert!(out.deliver.is_empty());
        assert_eq!(out.duplicates, 2);
        // A newer input supersedes the old trailing copy.
        published(sender.push(key(3, "down", "b"), t0 + Duration::from_millis(40)));
        assert!(sender.trailing(flush, t0 + Duration::from_millis(140)).is_none());
    }

    #[test]
    fn a_same_instant_burst_is_paced_into_one_bundle_carrying_every_input() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        let (first, _) = published(sender.push(key(1, "down", "a"), t0));
        assert!(matches!(
            sender.push(key(2, "down", "a"), t0),
            PushOutcome::FlushAfter(wait) if wait == MIN_BUNDLE_INTERVAL
        ));
        for seq in 3..=12 {
            assert!(matches!(sender.push(key(seq, "down", "a"), t0), PushOutcome::Queued));
        }
        let flushed = sender.flush(t0 + MIN_BUNDLE_INTERVAL).unwrap();
        // All 11 queued inputs ride one bundle; at this size the byte budget
        // leaves no room for an older copy (it is sent twice instead).
        assert_eq!(seqs(&flushed.inputs), (2..=12).collect::<Vec<_>>());
        assert!(sender.flush(t0 + MIN_BUNDLE_INTERVAL).is_none());
        let mut host = InputSequencer::default();
        let mut delivered = seqs(&host.accept(first, t0).deliver);
        delivered.extend(seqs(&host.accept(flushed, t0).deliver));
        assert_eq!(delivered, (1..=12).collect::<Vec<_>>());
        // A runaway burst flushes at MAX_BUNDLE_INPUTS without waiting.
        let mut big = RedundantInputSender::new(7, "s2".into());
        published(big.push(key(1, "down", "a"), t0));
        let mut forced = None;
        for seq in 2..=(MAX_BUNDLE_INPUTS as u64 + 1) {
            if let PushOutcome::Publish(bundle, _) = big.push(key(seq, "down", "a"), t0) {
                forced = Some(bundle);
            }
        }
        let forced = forced.expect("a full queue publishes immediately");
        assert!(forced.inputs.len() <= MAX_BUNDLE_INPUTS + MAX_REDUNDANT_COPIES);
        assert!(parse_input_bundle(&serde_json::to_vec(&forced).unwrap()).is_some());
    }

    #[test]
    fn reordered_bundles_replay_in_sequence_order() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        // Big text inputs leave no room for redundancy, so reorder opens a
        // real gap the host must hold rather than paper over.
        let mut text = key(1, "down", "a");
        text.message_type = RemoteControlType::Text;
        text.action = None;
        text.text = Some("x".repeat(MAX_BUNDLE_PAYLOAD_BYTES));
        let first = published(sender.push(text.clone(), t0)).0;
        text.seq = 2;
        let second = published(sender.push(text, t0 + MIN_BUNDLE_INTERVAL)).0;
        assert_eq!(second.inputs.len(), 1, "size bound keeps the big input alone");
        let mut host = InputSequencer::default();
        let out = host.accept(second, t0);
        assert!(out.deliver.is_empty());
        assert!(out.arm_gap_timer);
        let out = host.accept(first, t0 + Duration::from_millis(5));
        assert_eq!(seqs(&out.deliver), vec![1, 2]);
        assert!(!out.arm_gap_timer);
        assert_eq!(host.flush_expired(t0 + GAP_HOLD * 2).skipped, 0);
    }

    #[test]
    fn duplicate_deliveries_are_ignored() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        let bundles = send_burst(&mut sender, 4, t0);
        let mut host = InputSequencer::default();
        let mut delivered = Vec::new();
        for bundle in bundles.iter().chain(bundles.iter()) {
            delivered.extend(seqs(&host.accept(bundle.clone(), t0).deliver));
        }
        assert_eq!(delivered, vec![1, 2, 3, 4]);
    }

    #[test]
    fn unrecoverable_gap_is_skipped_after_the_hold_and_reports_the_loss() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        let (down_a, _) = published(sender.push(key(1, "down", "a"), t0));
        // The Up and every bundle carrying it are lost; the next input comes
        // after the redundancy window has aged it out.
        published(sender.push(key(2, "up", "a"), t0 + MIN_BUNDLE_INTERVAL));
        let later = t0 + MAX_REDUNDANT_AGE + Duration::from_millis(20);
        let (down_b, _) = published(sender.push(key(3, "down", "b"), later));
        assert_eq!(down_b.first, 3, "the aged-out Up is no longer resent");
        let mut host = InputSequencer::default();
        assert_eq!(seqs(&host.accept(down_a, t0).deliver), vec![1]);
        let out = host.accept(down_b, later);
        assert!(out.deliver.is_empty(), "held while the gap may still fill");
        assert!(out.arm_gap_timer);
        let early = host.flush_expired(later + GAP_HOLD / 2);
        assert!(early.deliver.is_empty());
        assert_eq!(early.skipped, 0);
        let out = host.flush_expired(later + GAP_HOLD);
        assert_eq!(out.skipped, 1, "caller must release held input for the lost Up");
        assert_eq!(seqs(&out.deliver), vec![3]);
        assert!(!out.arm_gap_timer);
    }

    fn bundle_for(stream: &str, first: u64, message: RemoteControlMessage) -> InputBundle {
        InputBundle {
            v: BUNDLE_VERSION,
            kind: BUNDLE_KIND.into(),
            window_id: 7,
            stream: stream.into(),
            first,
            inputs: vec![message],
        }
    }

    #[test]
    fn new_stream_restarts_sequence_and_stale_stream_is_dropped() {
        let t0 = Instant::now();
        let mut host = InputSequencer::default();
        assert_eq!(host.accept(bundle_for("s1", 1, key(1, "down", "a")), t0).deliver.len(), 1);
        assert_eq!(seqs(&host.accept(bundle_for("s2", 1, key(9, "down", "b")), t0).deliver), vec![9]);
        let out = host.accept(bundle_for("s1", 2, key(2, "up", "a")), t0);
        assert!(out.stale_stream);
        assert!(out.deliver.is_empty());
    }

    #[test]
    fn hover_moves_never_ride_the_redundant_stream() {
        let mut hover = key(1, "down", "a");
        hover.message_type = RemoteControlType::Pointer;
        hover.action = Some(RemoteControlAction::Move);
        hover.buttons = Some(0);
        assert!(!rides_redundant_stream(&hover));
        hover.buttons = Some(1);
        assert!(rides_redundant_stream(&hover), "a drag move is ordered input");
        let mut wheel = key(1, "down", "a");
        wheel.message_type = RemoteControlType::Wheel;
        assert!(rides_redundant_stream(&wheel));
    }

    #[test]
    fn bundle_round_trips_and_rejects_foreign_shapes() {
        let t0 = Instant::now();
        let mut sender = RedundantInputSender::new(7, "s1".into());
        let bundle = send_burst(&mut sender, 2, t0).pop().unwrap();
        let bytes = serde_json::to_vec(&bundle).unwrap();
        assert_eq!(parse_input_bundle(&bytes).map(|parsed| json(&parsed)), Some(json(&bundle)));
        let mut wrong_window = bundle.clone();
        wrong_window.inputs[0].window_id = 8;
        assert!(parse_input_bundle(&serde_json::to_vec(&wrong_window).unwrap()).is_none());
        let plain = serde_json::to_vec(&key(1, "down", "a")).unwrap();
        assert!(parse_input_bundle(&plain).is_none());
    }
}
