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
/// Inputs per bundle, newest included.
pub(crate) const MAX_BUNDLE_INPUTS: usize = 4;
/// Older inputs are added only while the encoded bundle stays under this, so
/// a typical bundle fits one SCTP chunk. The newest input is always sent.
pub(crate) const MAX_BUNDLE_PAYLOAD_BYTES: usize = 1_200;
/// Older inputs past this age are no longer worth resending: the host has
/// either delivered them or skipped the gap by then.
pub(crate) const MAX_REDUNDANT_AGE: Duration = Duration::from_millis(500);
/// After the last input of a burst the same bundle is resent at these offsets,
/// so a lost final keystroke or Up is still recovered.
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
        && bundle.inputs.len() <= MAX_BUNDLE_INPUTS * 4
        && bundle
            .inputs
            .iter()
            .all(|input| input.window_id == bundle.window_id && rides_redundant_stream(input));
    valid.then_some(bundle)
}

#[derive(Debug, Clone)]
struct SentInput {
    rseq: u64,
    sent_at: Instant,
    encoded_len: usize,
    message: RemoteControlMessage,
}

/// Controller side: one per (window, host) grant.
#[derive(Debug)]
pub(crate) struct RedundantInputSender {
    window_id: u32,
    stream: String,
    next_rseq: u64,
    recent: VecDeque<SentInput>,
}

impl RedundantInputSender {
    pub(crate) fn new(window_id: u32, stream: String) -> Self {
        Self {
            window_id,
            stream,
            next_rseq: 1,
            recent: VecDeque::new(),
        }
    }

    pub(crate) fn stream(&self) -> &str {
        &self.stream
    }

    /// rseq of the newest input, used by trailing-copy timers to tell whether
    /// a newer input has superseded them.
    pub(crate) fn latest_rseq(&self) -> Option<u64> {
        self.recent.back().map(|input| input.rseq)
    }

    /// Assigns the next rseq and returns the bundle to publish now.
    pub(crate) fn push(&mut self, message: RemoteControlMessage, now: Instant) -> InputBundle {
        let encoded_len = serde_json::to_vec(&message).map_or(0, |bytes| bytes.len());
        let rseq = self.next_rseq;
        self.next_rseq = self.next_rseq.saturating_add(1);
        self.recent.push_back(SentInput {
            rseq,
            sent_at: now,
            encoded_len,
            message,
        });
        while self.recent.len() > MAX_BUNDLE_INPUTS {
            self.recent.pop_front();
        }
        self.bundle(now).expect("an input was just pushed")
    }

    /// The newest input plus as many contiguous older ones as fit the count,
    /// age, and size bounds.
    pub(crate) fn bundle(&self, now: Instant) -> Option<InputBundle> {
        let newest = self.recent.back()?;
        let mut taken = 1;
        let mut bytes = newest.encoded_len;
        for older in self.recent.iter().rev().skip(1) {
            if taken >= MAX_BUNDLE_INPUTS
                || now.saturating_duration_since(older.sent_at) > MAX_REDUNDANT_AGE
                || bytes + older.encoded_len > MAX_BUNDLE_PAYLOAD_BYTES
            {
                break;
            }
            taken += 1;
            bytes += older.encoded_len;
        }
        let inputs: Vec<_> = self
            .recent
            .iter()
            .skip(self.recent.len() - taken)
            .collect();
        Some(InputBundle {
            v: BUNDLE_VERSION,
            kind: BUNDLE_KIND.to_string(),
            window_id: self.window_id,
            stream: self.stream.clone(),
            first: inputs[0].rseq,
            inputs: inputs.into_iter().map(|input| input.message.clone()).collect(),
        })
    }
}

/// What the host should do after one sequencer step.
#[derive(Debug, Default)]
pub(crate) struct SequencerOutput {
    /// Inputs to hand to the ordinary per-message path, in order.
    pub deliver: Vec<RemoteControlMessage>,
    /// Delivered inputs whose own packet never arrived (recovered from a
    /// later bundle's redundancy).
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

    /// Types `count` keydowns through a sender and returns every bundle sent.
    fn send_burst(sender: &mut RedundantInputSender, count: u64, t0: Instant) -> Vec<InputBundle> {
        (1..=count)
            .map(|seq| sender.push(key(seq, "down", "a"), t0 + Duration::from_millis(seq * 10)))
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
        let bundles = send_burst(&mut sender, 2, t0);
        let mut host = InputSequencer::default();
        assert_eq!(seqs(&host.accept(bundles[0].clone(), t0).deliver), vec![1]);
        // bundles[1] (the last input of the burst) is lost; the trailing copy
        // the controller sends TRAILING_COPY_DELAYS[0] later is the same bundle.
        let trailing = sender
            .bundle(t0 + Duration::from_millis(20) + TRAILING_COPY_DELAYS[0])
            .unwrap();
        assert_eq!(json(&trailing), json(&bundles[1]));
        assert_eq!(seqs(&host.accept(trailing.clone(), t0).deliver), vec![2]);
        // The second trailing copy is pure duplicate.
        let out = host.accept(trailing, t0);
        assert!(out.deliver.is_empty());
        assert_eq!(out.duplicates, 2);
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
        let first = sender.push(text.clone(), t0);
        text.seq = 2;
        let second = sender.push(text, t0);
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
        sender.push(key(1, "down", "a"), t0);
        // The Up and every bundle carrying it are lost; later inputs arrive
        // after the redundancy window has aged it out.
        let _lost_up = sender.push(key(2, "up", "a"), t0);
        let later = t0 + MAX_REDUNDANT_AGE + Duration::from_millis(1);
        let down_b = sender.push(key(3, "down", "b"), later);
        assert_eq!(down_b.first, 3, "the aged-out Up is no longer resent");
        let mut host = InputSequencer::default();
        assert_eq!(
            seqs(&host.accept(sender_bundle_for(1, key(1, "down", "a")), t0).deliver),
            vec![1]
        );
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

    fn sender_bundle_for(first: u64, message: RemoteControlMessage) -> InputBundle {
        InputBundle {
            v: BUNDLE_VERSION,
            kind: BUNDLE_KIND.into(),
            window_id: 7,
            stream: "s1".into(),
            first,
            inputs: vec![message],
        }
    }

    #[test]
    fn new_stream_restarts_sequence_and_stale_stream_is_dropped() {
        let t0 = Instant::now();
        let mut host = InputSequencer::default();
        assert_eq!(host.accept(sender_bundle_for(1, key(1, "down", "a")), t0).deliver.len(), 1);
        let mut fresh = sender_bundle_for(1, key(9, "down", "b"));
        fresh.stream = "s2".into();
        assert_eq!(seqs(&host.accept(fresh, t0).deliver), vec![9]);
        let out = host.accept(sender_bundle_for(2, key(2, "up", "a")), t0);
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
