//! #371: controller->host input timing for `remote_control_latency_summary`.
//!
//! Three producers, all fed by the optional `sentAtMs` envelope field (the
//! controller's wall clock when the input was produced) and the optional
//! `hostHoldMs` on `kind:"result"`:
//! - controller `input_rtt_ms = (result_recv - sentAtMs) - hostHoldMs` for v2
//!   discrete ops ([`PendingRtt`]); same clock on both ends, no offset needed.
//! - host `input_excess_delay_ms` / `input_stall_count` over consecutive
//!   ordered inputs ([`StallDetector`]); clock-free, compares gaps only.
//! - host `input_leg_ms` / `input_leg_lossy_ms` = socket receive minus
//!   offset-corrected `sentAtMs`, with the offset from the grant-gated
//!   latency probe ([`ClockWindows`]); `offset_err_ms` qualifies them.
//! - the controller session lifecycle ([`ControllerTargets`]) that decides
//!   when the controller aggregator has an active session.
//!
//! Identities live only in these in-memory keys; nothing here is emitted.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::diagnostics::CLOCK_OFFSET_STALE_MS;
use crate::rc_latency_telemetry::{self, Counter, MaxCounter, RcRole, Stage};
use crate::remote_control_core::{
    rides_lossy_transport, ControllerStatusEffect, RemoteControlMessage, RemoteControlType,
};
use crate::remote_control_redundant::rides_redundant_stream;
use crate::time_util::now_ms;

/// When a remote-control packet came off the data channel, taken before
/// parse and auth so host queueing is not folded into the network leg.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SocketReceive {
    pub(crate) at: Instant,
    pub(crate) wall_ms: u64,
}

impl SocketReceive {
    pub(crate) fn now() -> Self {
        Self {
            at: Instant::now(),
            wall_ms: now_ms(),
        }
    }

    /// The same stamp moved back to an earlier receive instant (an input the
    /// sequencer held), keeping the wall clock in step with the monotonic one.
    pub(crate) fn rewound_to(self, at: Instant) -> Self {
        let back_ms = self.at.saturating_duration_since(at).as_millis() as u64;
        Self {
            at: at.min(self.at),
            wall_ms: self.wall_ms.saturating_sub(back_ms),
        }
    }
}

/// Probe samples kept per controller; the offset comes from the lowest-RTT one.
pub(crate) const CLOCK_WINDOW_SAMPLES: usize = 16;
/// A controller with no probe answer for this long is forgotten entirely.
const CLOCK_WINDOW_RETAIN_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy)]
struct ClockSample {
    /// `host_clock - controller_clock`, as `latency_probe` estimates it.
    controller_to_host_offset_us: i64,
    rtt_ms: f64,
    at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClockEstimate {
    pub(crate) offset_ms: f64,
    /// NTP bound: half the selected sample's round trip.
    pub(crate) err_ms: f64,
}

/// Rolling per-controller window of latency-probe offset samples (#371).
#[derive(Debug, Default)]
pub(crate) struct ClockWindows {
    peers: HashMap<String, VecDeque<ClockSample>>,
}

impl ClockWindows {
    pub(crate) fn record(&mut self, peer: &str, offset_us: i64, rtt_ms: f64, now_ms: u64) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        self.peers.retain(|_, window| {
            window
                .back()
                .is_some_and(|newest| now_ms.saturating_sub(newest.at_ms) <= CLOCK_WINDOW_RETAIN_MS)
        });
        let window = self.peers.entry(peer.to_string()).or_default();
        if window.len() >= CLOCK_WINDOW_SAMPLES {
            window.pop_front();
        }
        window.push_back(ClockSample {
            controller_to_host_offset_us: offset_us,
            rtt_ms,
            at_ms: now_ms,
        });
    }

    /// Lowest-RTT sample among those within the diagnostics staleness rule;
    /// `None` when every answer is older (the probe stopped, offsets drift).
    pub(crate) fn estimate(&self, peer: &str, now_ms: u64) -> Option<ClockEstimate> {
        let best = self
            .peers
            .get(peer)?
            .iter()
            .filter(|sample| now_ms.saturating_sub(sample.at_ms) <= CLOCK_OFFSET_STALE_MS)
            .min_by(|a, b| a.rtt_ms.total_cmp(&b.rtt_ms))?;
        Some(ClockEstimate {
            offset_ms: best.controller_to_host_offset_us as f64 / 1000.0,
            err_ms: best.rtt_ms / 2.0,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.peers.clear();
    }
}

/// `receive - (sentAtMs + offset)` on the host clock. A negative leg inside
/// the offset error is 0; beyond it (a clock step) there is no sample.
pub(crate) fn input_leg_ms(sent_at_ms: u64, received_wall_ms: u64, clock: ClockEstimate) -> Option<f64> {
    let leg = received_wall_ms as f64 - (sent_at_ms as f64 + clock.offset_ms);
    if !leg.is_finite() || leg < -clock.err_ms {
        return None;
    }
    Some(leg.max(0.0))
}

/// One host share this controller holds a grant on. Keyed by the
/// LiveKit-verified host identity: window ids are host-local, so the window
/// id alone collides across hosts (#371).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ControlTargetKey {
    pub(crate) host_identity: String,
    pub(crate) window_id: u32,
    pub(crate) share_instance_id: Option<String>,
}

/// A grant whose remote window has been gone this long is ended. Long enough
/// to ride out a transport-replacement retire/reveal, short enough that a
/// host that left or crashed without a `stopped` does not linger.
pub(crate) const VANISHED_HOST_GRACE: Duration = Duration::from_secs(10);

/// Controller-side grants, driven by host `active`/`stopped`/`disabled`
/// statuses and reconciled against the remote window still existing, so a
/// host that vanishes without a `stopped` cannot leave the session open.
#[derive(Debug, Default)]
pub(crate) struct ControllerTargets {
    /// Value: when the target's window was first seen missing.
    targets: HashMap<ControlTargetKey, Option<Instant>>,
}

impl ControllerTargets {
    pub(crate) fn apply_status(&mut self, key: ControlTargetKey, effect: ControllerStatusEffect) {
        match effect {
            ControllerStatusEffect::Activate => {
                // A re-grant (possibly on a new share instance) replaces
                // whatever this host's window had.
                self.targets.retain(|target, _| !same_window(target, &key));
                self.targets.insert(key, None);
            }
            // When both sides name a share instance it must match, so a late
            // `stopped` for an old instance spares the new one.
            ControllerStatusEffect::Terminate => self.targets.retain(|target, _| {
                let instance_differs = matches!(
                    (&target.share_instance_id, &key.share_instance_id),
                    (Some(a), Some(b)) if a != b
                );
                !same_window(target, &key) || instance_differs
            }),
            ControllerStatusEffect::Feedback => {}
        }
    }

    pub(crate) fn keys(&self) -> Vec<ControlTargetKey> {
        self.targets.keys().cloned().collect()
    }

    /// One presence observation per target; ends a target whose window has
    /// been missing for [`VANISHED_HOST_GRACE`].
    pub(crate) fn observe_presence(&mut self, key: &ControlTargetKey, present: bool, now: Instant) {
        let Some(missing_since) = self.targets.get_mut(key) else {
            return;
        };
        if present {
            *missing_since = None;
            return;
        }
        let since = *missing_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= VANISHED_HOST_GRACE {
            self.targets.remove(key);
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.targets.is_empty()
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.targets.clear();
    }
}

fn same_window(a: &ControlTargetKey, b: &ControlTargetKey) -> bool {
    a.host_identity == b.host_identity && a.window_id == b.window_id
}

const PENDING_RTT_TTL: Duration = Duration::from_secs(15);
const MAX_PENDING_RTT: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingKey {
    host_identity: String,
    window_id: u32,
    input_id: String,
}

/// Controller send stamps for v2 discrete ops awaiting their `result`.
#[derive(Debug, Default)]
pub(crate) struct PendingRtt {
    pending: HashMap<PendingKey, (u64, Instant)>,
}

impl PendingRtt {
    /// Remember an outbound v2 discrete op. Needs both `inputId` and the
    /// production-time `sentAtMs`; anything else is not pairable.
    pub(crate) fn note_send(&mut self, message: &RemoteControlMessage, now: Instant) {
        let (Some(input_id), Some(sent_at_ms)) = (message.input_id.as_ref(), message.sent_at_ms)
        else {
            return;
        };
        self.pending
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < PENDING_RTT_TTL);
        if self.pending.len() >= MAX_PENDING_RTT {
            if let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
            {
                self.pending.remove(&oldest);
            }
        }
        self.pending.insert(
            PendingKey {
                host_identity: message.target_user_id.clone(),
                window_id: message.window_id,
                input_id: input_id.clone(),
            },
            (sent_at_ms, now),
        );
    }

    /// Pair a host `result` (its `controllerId` is the verified host) with
    /// its send. The first result consumes the entry, so a repeated result
    /// never produces a second sample. `None` without `hostHoldMs` (an old
    /// host): that round trip would fold host replay time into the network.
    pub(crate) fn take_rtt_ms(
        &mut self,
        result: &RemoteControlMessage,
        result_recv_ms: u64,
    ) -> Option<f64> {
        if result.message_type != RemoteControlType::Result {
            return None;
        }
        let key = PendingKey {
            host_identity: result.controller_id.clone(),
            window_id: result.window_id,
            input_id: result.input_id.clone()?,
        };
        let (sent_at_ms, _) = self.pending.remove(&key)?;
        let hold_ms = result.host_hold_ms?;
        input_rtt_ms(sent_at_ms, result_recv_ms, hold_ms)
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }
}

/// `(result_recv - sent) - hostHoldMs`; `None` when negative (clock step).
pub(crate) fn input_rtt_ms(sent_at_ms: u64, result_recv_ms: u64, host_hold_ms: u64) -> Option<f64> {
    let rtt = result_recv_ms
        .checked_sub(sent_at_ms)?
        .checked_sub(host_hold_ms)?;
    Some(rtt as f64)
}

/// Excess above this during an active send counts as a head-of-line stall.
pub(crate) const STALL_EXCESS_MS: f64 = 50.0;
/// A send gap below this means the controller was actively sending.
pub(crate) const ACTIVE_SEND_GAP_MS: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ExcessSample {
    pub(crate) excess_ms: f64,
    pub(crate) stall: bool,
}

#[derive(Debug, Clone, Copy)]
struct LastInput {
    seq: u64,
    sent_at_ms: u64,
    received_at: Instant,
}

/// Host-side `excess = (recv_i - recv_{i-1}) - (sent_i - sent_{i-1})` per
/// (controller, window) over ordered (non-hover) inputs. A repeated input
/// (#399/#421 redundant copies) is skipped, never scored as a gap.
#[derive(Debug, Default)]
pub(crate) struct StallDetector {
    last: HashMap<(String, u32), LastInput>,
}

impl StallDetector {
    /// A redundant copy (or older) of the last scored input on this stream.
    fn is_repeat(&self, controller_id: &str, window_id: u32, seq: u64, sent_at_ms: u64) -> bool {
        self.last
            .get(&(controller_id.to_string(), window_id))
            .is_some_and(|previous| seq <= previous.seq && sent_at_ms <= previous.sent_at_ms)
    }

    pub(crate) fn observe(
        &mut self,
        controller_id: &str,
        window_id: u32,
        seq: u64,
        sent_at_ms: u64,
        received_at: Instant,
    ) -> Option<ExcessSample> {
        let key = (controller_id.to_string(), window_id);
        let current = LastInput {
            seq,
            sent_at_ms,
            received_at,
        };
        let Some(previous) = self.last.get(&key).copied() else {
            self.last.insert(key, current);
            return None;
        };
        if seq <= previous.seq && sent_at_ms <= previous.sent_at_ms {
            return None;
        }
        self.last.insert(key, current);
        // A controller clock step backwards (or a reloaded page) restarts.
        let send_gap_ms = sent_at_ms.checked_sub(previous.sent_at_ms)? as f64;
        let recv_gap_ms = received_at
            .saturating_duration_since(previous.received_at)
            .as_secs_f64()
            * 1000.0;
        let excess_ms = (recv_gap_ms - send_gap_ms).max(0.0);
        Some(ExcessSample {
            excess_ms,
            stall: excess_ms > STALL_EXCESS_MS && send_gap_ms < ACTIVE_SEND_GAP_MS,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.last.clear();
    }
}

fn controller_targets() -> &'static Mutex<ControllerTargets> {
    static TARGETS: OnceLock<Mutex<ControllerTargets>> = OnceLock::new();
    TARGETS.get_or_init(Default::default)
}

fn pending_rtt() -> &'static Mutex<PendingRtt> {
    static PENDING: OnceLock<Mutex<PendingRtt>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

fn stall_detector() -> &'static Mutex<StallDetector> {
    static DETECTOR: OnceLock<Mutex<StallDetector>> = OnceLock::new();
    DETECTOR.get_or_init(Default::default)
}

fn clock_windows() -> &'static Mutex<ClockWindows> {
    static WINDOWS: OnceLock<Mutex<ClockWindows>> = OnceLock::new();
    WINDOWS.get_or_init(Default::default)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Controller: a host status for one of our grants (verified sender in
/// `controller_id`). Starts the controller session on `active`.
pub(crate) fn controller_status(message: &RemoteControlMessage, effect: ControllerStatusEffect) {
    let key = ControlTargetKey {
        host_identity: message.controller_id.clone(),
        window_id: message.window_id,
        share_instance_id: message.share_instance_id.clone(),
    };
    let (active, hosts) = {
        let mut targets = lock(controller_targets());
        targets.apply_status(key, effect);
        let hosts: Vec<String> = targets.keys().into_iter().map(|k| k.host_identity).collect();
        (targets.is_active(), hosts)
    };
    if effect == ControllerStatusEffect::Activate {
        rc_latency_telemetry::sync(RcRole::Controller, active);
        // #372: `controller_id` is the verified host on a status packet.
        crate::sfu_edge::note_peer_node_hash(&message.controller_id, message.lk_node_hash.as_deref());
    }
    if effect != ControllerStatusEffect::Feedback {
        crate::sfu_edge::set_session_peers(RcRole::Controller, hosts);
    }
}

/// Controller sweeper tick: end grants whose remote window has been gone for
/// the grace (host left or crashed without a `stopped`), then reconcile the
/// aggregator.
pub(crate) fn sync_controller(window_exists: impl Fn(&str, u32) -> bool) {
    sync_controller_at(window_exists, Instant::now());
}

fn sync_controller_at(window_exists: impl Fn(&str, u32) -> bool, now: Instant) {
    let keys = lock(controller_targets()).keys();
    // Checked without our lock held: `window_exists` takes compositor locks.
    let presence: Vec<_> = keys
        .into_iter()
        .map(|key| {
            let present = window_exists(&key.host_identity, key.window_id);
            (key, present)
        })
        .collect();
    let active = {
        let mut targets = lock(controller_targets());
        for (key, present) in &presence {
            targets.observe_presence(key, *present, now);
        }
        targets.is_active()
    };
    if !active {
        lock(pending_rtt()).clear();
    }
    rc_latency_telemetry::sync(RcRole::Controller, active);
}

/// Controller: record an outbound packet's send stamp if it is pairable.
pub(crate) fn controller_sent(message: &RemoteControlMessage) {
    if message.input_id.is_some() && message.sent_at_ms.is_some() {
        lock(pending_rtt()).note_send(message, Instant::now());
    }
}

/// Controller: a correlated host `result` arrived.
pub(crate) fn controller_result(message: &RemoteControlMessage, result_recv_ms: u64) {
    if let Some(rtt_ms) = lock(pending_rtt()).take_rtt_ms(message, result_recv_ms) {
        rc_latency_telemetry::record_stage(RcRole::Controller, Stage::InputRttMs, rtt_ms);
    }
}

/// Host: a latency-probe answer from a controller that holds a grant.
pub(crate) fn record_controller_clock(peer: &str, offset_us: i64, rtt_ms: f64) {
    lock(clock_windows()).record(peer, offset_us, rtt_ms, now_ms());
}

/// Host: one authorized controller input arrived. `seq_accepted` is the
/// lossy-stream watermark verdict, so a stale or repeated hover move is not
/// a leg sample. Hover moves are never ordered behind anything, so they are
/// not stall-scored.
pub(crate) fn host_input(message: &RemoteControlMessage, received: SocketReceive, seq_accepted: bool) {
    let Some(sent_at_ms) = message.sent_at_ms else {
        return;
    };
    let lossy = rides_lossy_transport(message);
    let ordered = rides_redundant_stream(message);
    let repeat = ordered
        && lock(stall_detector()).is_repeat(
            &message.controller_id,
            message.window_id,
            message.seq,
            sent_at_ms,
        );
    if seq_accepted && !repeat {
        host_leg(&message.controller_id, sent_at_ms, received.wall_ms, lossy);
    }
    if !ordered {
        return;
    }
    let sample = lock(stall_detector()).observe(
        &message.controller_id,
        message.window_id,
        message.seq,
        sent_at_ms,
        received.at,
    );
    if let Some(sample) = sample {
        rc_latency_telemetry::record_stage(
            RcRole::Host,
            Stage::InputExcessDelayMs,
            sample.excess_ms,
        );
        if sample.stall {
            rc_latency_telemetry::add_counter(RcRole::Host, Counter::InputStallCount, 1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LegSample {
    pub(crate) stage: Stage,
    pub(crate) leg_ms: f64,
    pub(crate) offset_err_ms: u64,
}

/// The one-way sample for one input, tagged by transport, or `None` without
/// a fresh offset for this controller.
pub(crate) fn leg_sample(
    windows: &ClockWindows,
    controller_id: &str,
    sent_at_ms: u64,
    received_wall_ms: u64,
    lossy: bool,
) -> Option<LegSample> {
    let clock = windows.estimate(controller_id, received_wall_ms)?;
    Some(LegSample {
        stage: if lossy {
            Stage::InputLegLossyMs
        } else {
            Stage::InputLegMs
        },
        leg_ms: input_leg_ms(sent_at_ms, received_wall_ms, clock)?,
        offset_err_ms: clock.err_ms.ceil() as u64,
    })
}

fn host_leg(controller_id: &str, sent_at_ms: u64, received_wall_ms: u64, lossy: bool) {
    let sample = leg_sample(
        &lock(clock_windows()),
        controller_id,
        sent_at_ms,
        received_wall_ms,
        lossy,
    );
    if let Some(sample) = sample {
        rc_latency_telemetry::record_stage_with_max(
            RcRole::Host,
            sample.stage,
            sample.leg_ms,
            MaxCounter::OffsetErrMs,
            sample.offset_err_ms,
        );
    }
}

/// Host: all control revoked (policy off or room left). Controller state is
/// not touched here; [`sync_controller`] ends it once the windows are gone.
pub(crate) fn clear_host_state() {
    lock(stall_detector()).clear();
    lock(clock_windows()).clear();
}

/// Controller state only: host clock/stall state is shared with
/// `remote_control`'s host-input tests, which run in parallel.
#[cfg(test)]
fn clear_all() {
    lock(controller_targets()).clear();
    lock(pending_rtt()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_control_core::{RemoteControlAction, RemoteControlModifiers, VERSION};

    fn message(
        kind: RemoteControlType,
        action: Option<RemoteControlAction>,
    ) -> RemoteControlMessage {
        let mut value = serde_json::json!({
            "v": VERSION,
            "kind": kind,
            "targetUserId": "host-a",
            "controllerId": "controller-b",
            "windowId": 42,
            "seq": 1,
            "modifiers": RemoteControlModifiers::default(),
        });
        if let Some(action) = action {
            value["action"] = serde_json::to_value(action).unwrap();
        }
        serde_json::from_value(value).unwrap()
    }

    fn key(host: &str, window_id: u32, share: Option<&str>) -> ControlTargetKey {
        ControlTargetKey {
            host_identity: host.to_string(),
            window_id,
            share_instance_id: share.map(str::to_string),
        }
    }

    /// Offset from the minimum-RTT sample of the last 16; err = min_rtt / 2.
    #[test]
    fn clock_offset_comes_from_the_minimum_rtt_sample_of_the_window() {
        let mut windows = ClockWindows::default();
        assert_eq!(windows.estimate("c", 1_000), None, "no probe yet");
        windows.record("c", 40_000, 30.0, 1_000);
        windows.record("c", 25_000, 8.0, 3_000);
        windows.record("c", 60_000, 50.0, 5_000);
        windows.record("c", 0, f64::NAN, 5_500);
        assert_eq!(
            windows.estimate("c", 6_000),
            Some(ClockEstimate {
                offset_ms: 25.0,
                err_ms: 4.0
            })
        );
        assert_eq!(windows.estimate("other", 6_000), None, "per controller");
        // 16 newer, slower samples push the 8 ms one out of the window.
        for index in 0..CLOCK_WINDOW_SAMPLES as u64 {
            windows.record("c", 30_000 + index as i64, 12.0 + index as f64, 7_000 + index);
        }
        assert_eq!(
            windows.estimate("c", 7_100),
            Some(ClockEstimate {
                offset_ms: 30.0,
                err_ms: 6.0
            })
        );
        // The best sample ages out after 10 s even while newer ones arrive.
        windows.record("c", 90_000, 40.0, 7_000 + CLOCK_OFFSET_STALE_MS + 1);
        assert_eq!(
            windows.estimate("c", 7_000 + CLOCK_OFFSET_STALE_MS + 1).map(|e| e.offset_ms),
            Some(30.001)
        );
        assert_eq!(
            windows.estimate("c", 7_015 + CLOCK_OFFSET_STALE_MS + 1).map(|e| e.offset_ms),
            Some(90.0),
            "only the fresh sample is left"
        );
    }

    /// Same staleness rule as diagnostics: 10 s after the newest answer.
    #[test]
    fn a_stale_offset_gives_no_leg_sample() {
        let mut windows = ClockWindows::default();
        windows.record("c", -25_000_000, 10.0, 1_000);
        let fresh = leg_sample(&windows, "c", 2_000, 1_000 + CLOCK_OFFSET_STALE_MS, false);
        assert!(fresh.is_some(), "exactly 10 s old is still fresh");
        assert_eq!(
            leg_sample(&windows, "c", 2_000, 1_001 + CLOCK_OFFSET_STALE_MS, false),
            None
        );
        windows.record("c", -25_000_000, 10.0, 20_000);
        assert!(leg_sample(&windows, "c", 21_000, 20_500, true).is_some(), "a new answer revives it");
        windows.clear();
        assert_eq!(leg_sample(&windows, "c", 21_000, 20_500, true), None);
    }

    #[test]
    fn a_held_input_is_rewound_to_its_own_receive_time() {
        let t0 = Instant::now();
        let reference = SocketReceive {
            at: t0 + Duration::from_millis(80),
            wall_ms: 10_080,
        };
        let held = reference.rewound_to(t0);
        assert_eq!(held.at, t0);
        assert_eq!(held.wall_ms, 10_000);
        assert_eq!(reference.rewound_to(reference.at + Duration::from_secs(1)).wall_ms, 10_080);
    }

    #[test]
    fn one_way_leg_math_with_a_synthetic_offset_and_transport_tag() {
        // Host clock is 25 s behind the controller's; the input took 30 ms.
        let mut windows = ClockWindows::default();
        windows.record("c", -25_000_000, 9.0, 100_000);
        let sent_on_controller = 125_000;
        let received_on_host = 100_030;
        assert_eq!(
            leg_sample(&windows, "c", sent_on_controller, received_on_host, false),
            Some(LegSample {
                stage: Stage::InputLegMs,
                leg_ms: 30.0,
                offset_err_ms: 5
            })
        );
        assert_eq!(
            leg_sample(&windows, "c", sent_on_controller, received_on_host, true).map(|s| s.stage),
            Some(Stage::InputLegLossyMs)
        );
        let clock = windows.estimate("c", 100_000).unwrap();
        assert_eq!(input_leg_ms(125_000, 99_997, clock), Some(0.0), "within err: 0");
        assert_eq!(input_leg_ms(125_000, 99_990, clock), None, "beyond err: clock step");
    }

    #[test]
    fn rtt_subtracts_host_hold_and_pairs_only_once() {
        let t0 = Instant::now();
        let mut pending = PendingRtt::default();
        let mut sent = message(RemoteControlType::Key, Some(RemoteControlAction::Down));
        sent.input_id = Some("input-1".to_string());
        sent.sent_at_ms = Some(1_000);
        pending.note_send(&sent, t0);

        let mut result = message(RemoteControlType::Result, None);
        // The host is the sender of a result: its identity rides controllerId.
        result.controller_id = "host-a".to_string();
        result.target_user_id = "controller-b".to_string();
        result.input_id = Some("input-1".to_string());
        result.host_hold_ms = Some(12);
        assert_eq!(pending.take_rtt_ms(&result, 1_090), Some(78.0));
        assert_eq!(
            pending.take_rtt_ms(&result, 1_095),
            None,
            "a repeated result is not a sample"
        );
    }

    #[test]
    fn rtt_needs_host_hold_matching_host_and_stamped_send() {
        let t0 = Instant::now();
        let mut pending = PendingRtt::default();
        let mut unstamped = message(RemoteControlType::Key, Some(RemoteControlAction::Down));
        unstamped.input_id = Some("input-0".to_string());
        pending.note_send(&unstamped, t0);
        assert_eq!(pending.len(), 0, "no sentAtMs, nothing to pair");

        let mut sent = unstamped.clone();
        sent.sent_at_ms = Some(1_000);
        pending.note_send(&sent, t0);
        let mut result = message(RemoteControlType::Result, None);
        result.controller_id = "host-other".to_string();
        result.input_id = Some("input-0".to_string());
        result.host_hold_ms = Some(1);
        assert_eq!(
            pending.take_rtt_ms(&result, 1_050),
            None,
            "another host's window 42"
        );
        result.controller_id = "host-a".to_string();
        result.host_hold_ms = None;
        assert_eq!(
            pending.take_rtt_ms(&result, 1_050),
            None,
            "old host: no hostHoldMs"
        );
        assert_eq!(
            pending.len(),
            0,
            "the unpairable result still consumed the send"
        );
        assert_eq!(
            input_rtt_ms(1_000, 1_005, 10),
            None,
            "negative rtt is dropped"
        );
    }

    #[test]
    fn pending_rtt_is_bounded_and_expires() {
        let t0 = Instant::now();
        let mut pending = PendingRtt::default();
        let mut sent = message(RemoteControlType::Text, None);
        sent.sent_at_ms = Some(1);
        for index in 0..(MAX_PENDING_RTT + 10) {
            sent.input_id = Some(format!("input-{index}"));
            pending.note_send(&sent, t0 + Duration::from_millis(index as u64));
        }
        assert_eq!(pending.len(), MAX_PENDING_RTT);
        sent.input_id = Some("late".to_string());
        pending.note_send(&sent, t0 + PENDING_RTT_TTL + Duration::from_secs(5));
        assert_eq!(pending.len(), 1, "expired sends are pruned");
    }

    #[test]
    fn excess_and_stall_detection_on_a_synthetic_series() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut detector = StallDetector::default();
        assert_eq!(
            detector.observe("c", 1, 1, 1_000, at(0)),
            None,
            "first input has no gap"
        );
        // Continuous send every 16 ms, arriving on time.
        let on_time = detector.observe("c", 1, 2, 1_016, at(16)).unwrap();
        assert_eq!(
            on_time,
            ExcessSample {
                excess_ms: 0.0,
                stall: false
            }
        );
        // 120 ms arrival gap during a 16 ms send gap: one head-of-line stall.
        let stalled = detector.observe("c", 1, 3, 1_032, at(136)).unwrap();
        assert!(stalled.stall);
        assert!((stalled.excess_ms - 104.0).abs() < 0.5);
        // An idle gap (user stopped typing for 2 s) is not a stall.
        let idle = detector.observe("c", 1, 4, 3_032, at(2_200)).unwrap();
        assert!(!idle.stall);
        // Arriving bunched after a delay never scores negative.
        let bunched = detector.observe("c", 1, 5, 3_048, at(2_201)).unwrap();
        assert_eq!(bunched.excess_ms, 0.0);
    }

    #[test]
    fn repeated_inputs_are_skipped_and_streams_are_independent() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut detector = StallDetector::default();
        detector.observe("c", 1, 7, 1_000, at(0));
        assert_eq!(
            detector.observe("c", 1, 7, 1_000, at(90)),
            None,
            "redundant copy"
        );
        assert_eq!(detector.observe("c", 1, 6, 990, at(95)), None, "older copy");
        let next = detector.observe("c", 1, 8, 1_010, at(100)).unwrap();
        assert!(
            next.stall,
            "the gap is measured from the first delivery, not the copy"
        );
        assert_eq!(
            detector.observe("c", 2, 1, 1_020, at(400)),
            None,
            "other window: own stream"
        );
        assert_eq!(
            detector.observe("d", 1, 1, 1_020, at(400)),
            None,
            "other controller"
        );
        // A reloaded page restarts seq at 1 while its clock moved on.
        let reloaded = detector.observe("c", 1, 1, 5_000, at(4_100)).unwrap();
        assert!(!reloaded.stall);
    }

    #[test]
    fn controller_targets_key_by_verified_host_and_share_instance() {
        let mut targets = ControllerTargets::default();
        targets.apply_status(
            key("host-a", 42, Some("s1")),
            ControllerStatusEffect::Activate,
        );
        targets.apply_status(key("host-b", 42, None), ControllerStatusEffect::Activate);
        targets.apply_status(key("host-b", 42, None), ControllerStatusEffect::Terminate);
        assert!(targets.is_active(), "host-b's window 42 is not host-a's");
        targets.apply_status(
            key("host-a", 42, Some("s1")),
            ControllerStatusEffect::Feedback,
        );
        assert!(targets.is_active());
        // Republish: the new instance replaces the old; a late stop for the
        // old instance does not end the new grant.
        targets.apply_status(
            key("host-a", 42, Some("s2")),
            ControllerStatusEffect::Activate,
        );
        assert_eq!(targets.keys(), vec![key("host-a", 42, Some("s2"))]);
        targets.apply_status(
            key("host-a", 42, Some("s1")),
            ControllerStatusEffect::Terminate,
        );
        assert_eq!(targets.keys(), vec![key("host-a", 42, Some("s2"))]);
        // A stop that names no instance ends the window's grant.
        targets.apply_status(key("host-a", 42, None), ControllerStatusEffect::Terminate);
        assert!(!targets.is_active());
    }

    /// Both tests below drive the global controller target table.
    fn controller_targets_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        lock(&LOCK)
    }

    /// #372: the host's `active` status carries its `lkNodeHash`; the real
    /// controller-status path stamps `same_edge` on the controller summary.
    #[test]
    fn active_status_lk_node_hash_sets_controller_same_edge() {
        let _targets = controller_targets_test_lock();
        crate::sfu_edge::set_local_for_test(
            Some(crate::sfu_edge::LocalEdge {
                lk_region: Some("us-east-1".to_string()),
                self_hosted_sfu: "false",
                node_hash: Some("69e94d0a".to_string()),
            }),
            // Same state as the host test in remote_control.rs: the module
            // state is global and the two may run in parallel.
            Some(crate::sfu_edge::IcePath {
                candidate_type: "relay",
                protocol: "tcp",
            }),
        );
        let mut active = message(RemoteControlType::Status, None);
        active.controller_id = "host-same-edge".to_string();
        // A cross-edge host: `false` holds even if a sibling test's hashless
        // host is active at the same time (`same_edge_all`).
        active.lk_node_hash = Some("e4c80fa5".to_string());
        // Sibling tests share the controller aggregator; retry until an
        // interval reflects this activation.
        let mut line = String::new();
        for _ in 0..20 {
            controller_status(&active, ControllerStatusEffect::Activate);
            rc_latency_telemetry::record_stage(RcRole::Controller, Stage::InputRttMs, 12.0);
            let Some(summary) = rc_latency_telemetry::take_interval_for_test(RcRole::Controller) else {
                continue;
            };
            line = summary.log_line();
            if line.contains("same_edge=false") {
                break;
            }
        }
        assert!(line.contains("role=controller"), "{line}");
        assert!(line.contains("same_edge=false"), "{line}");
        assert!(line.contains("lk_region=us-east-1"), "{line}");
        lock(controller_targets()).apply_status(
            ControlTargetKey {
                host_identity: "host-same-edge".to_string(),
                window_id: 42,
                share_instance_id: None,
            },
            ControllerStatusEffect::Terminate,
        );
    }

    #[test]
    fn a_vanished_host_closes_the_controller_session_after_the_grace() {
        let _targets = controller_targets_test_lock();
        clear_all();
        let mut active = message(RemoteControlType::Status, None);
        active.controller_id = "host-gone".to_string();
        controller_status(&active, ControllerStatusEffect::Activate);
        assert!(lock(controller_targets()).is_active());
        let t0 = Instant::now();
        let gone = |host: &str, _: u32| host != "host-gone";
        sync_controller_at(gone, t0);
        assert!(
            lock(controller_targets()).is_active(),
            "a brief retire is ridden out"
        );
        sync_controller_at(|_, _| true, t0 + Duration::from_secs(5));
        sync_controller_at(gone, t0 + Duration::from_secs(6));
        sync_controller_at(gone, t0 + Duration::from_secs(15));
        assert!(
            lock(controller_targets()).is_active(),
            "reappearing reset the grace"
        );
        sync_controller_at(gone, t0 + Duration::from_secs(16));
        assert!(!lock(controller_targets()).is_active(), "no stopped needed");
        clear_all();
    }
}
