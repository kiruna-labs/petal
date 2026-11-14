//! #376: production press-to-photon, measured on the controller's clock.
//!
//! Host: a discrete input that injected (`note_injected`) becomes a marker;
//! the first capture frame that shows a change after it (`note_captured`, the
//! #373 match point) moves the marker to "awaiting publish"; the first
//! accepted publish of that capture or a later one (`take_published`) names
//! the trailer `frame_id`, and the host answers the controller with
//! `inputFrame { seq, frameId, contentChange }` (reliable, targeted, at most
//! 4/s per controller, discrete ops only).
//!
//! Controller: [`PhotonCorrelator`] pairs the send instant (by `seq`) with
//! the presentation of the first frame `>= frameId`, both on this machine's
//! monotonic clock. Rust mirror of `shared/logic/photonCorrelator.ts`, both
//! pinned by `contracts/petal-contracts.json` `photonCorrelatorVectors`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::rc_latency_telemetry::{self, Counter, Dimension, RcRole, Stage};
use crate::remote_control_core::{RemoteControlAction, RemoteControlMessage, RemoteControlType};
use crate::remote_control::CapturedContentChange;

pub(crate) const SENT_CAPACITY: usize = 64;
pub(crate) const SENT_TTL_MS: f64 = 2000.0;
pub(crate) const RING_MS: f64 = 2000.0;
pub(crate) const RING_CAPACITY: usize = 256;
pub(crate) const TIMEOUT_MS: f64 = 1000.0;
pub(crate) const WHEEL_BURST_IDLE_MS: f64 = 500.0;
pub(crate) const HOST_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// Host markers and awaiting-publish entries live as long as #373's markers.
const HOST_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PhotonOutcome {
    Photon { seq: u64, ms: f64 },
    Timeout { seq: u64 },
}

/// Discrete ops only (press, non-repeat key down, text, first wheel of a
/// burst); never a pointer move or up. Mirrors `PhotonInputFilter` in TS.
#[derive(Debug, Default)]
pub(crate) struct PhotonInputFilter {
    last_wheel_ms: Option<f64>,
}

impl PhotonInputFilter {
    pub(crate) fn admit(
        &mut self,
        kind: RemoteControlType,
        action: Option<RemoteControlAction>,
        repeat: bool,
        now_ms: f64,
    ) -> bool {
        match kind {
            RemoteControlType::Pointer => matches!(
                action,
                Some(RemoteControlAction::Down | RemoteControlAction::Click)
            ),
            RemoteControlType::Key => action == Some(RemoteControlAction::Down) && !repeat,
            RemoteControlType::Text => true,
            RemoteControlType::Wheel => {
                let first = self
                    .last_wheel_ms
                    .is_none_or(|last| now_ms - last >= WHEEL_BURST_IDLE_MS);
                self.last_wheel_ms = Some(now_ms);
                first
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SentEntry {
    seq: u64,
    sent_ms: f64,
    frame_id: Option<u64>,
    /// The publication the announcement named; `None` matches any.
    track: Option<u64>,
}

/// See `shared/logic/photonCorrelator.ts`; every method expires first.
/// Tracks are publication keys ([`track_key`] of the LiveKit track sid).
#[derive(Debug, Default)]
pub(crate) struct PhotonCorrelator {
    /// Send order.
    sent: VecDeque<SentEntry>,
    /// (track, frame id, present ms), presentation order.
    ring: VecDeque<(u64, u64, f64)>,
    last_track: Option<u64>,
    last_frame_id: Option<u64>,
}

fn same_track(announced: Option<u64>, presented: u64) -> bool {
    announced.is_none_or(|track| track == presented)
}

/// A process-local key for a LiveKit track sid (FNV-1a 64).
pub(crate) fn track_key(sid: &str) -> u64 {
    sid.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl PhotonCorrelator {
    pub(crate) fn note_sent(&mut self, seq: u64, sent_ms: f64) -> Vec<PhotonOutcome> {
        let out = self.expire(sent_ms);
        if self.sent.iter().any(|entry| entry.seq == seq) {
            return out;
        }
        if self.sent.len() >= SENT_CAPACITY {
            self.sent.pop_front();
        }
        self.sent.push_back(SentEntry {
            seq,
            sent_ms,
            frame_id: None,
            track: None,
        });
        out
    }

    pub(crate) fn note_presented(&mut self, track: u64, frame_id: u64, present_ms: f64) -> Vec<PhotonOutcome> {
        let mut out = self.expire(present_ms);
        if self.last_track != Some(track) {
            // A new (or first) publication: old frames and other-publication
            // announcements go; unannounced or same-track inputs stay.
            self.ring.clear();
            self.sent.retain(|entry| same_track(entry.track, track));
        } else if self.last_frame_id.is_some_and(|last| frame_id < last) {
            // Ids only grow within one publication; a drop is a restart we cannot place.
            self.reset();
        }
        self.last_track = Some(track);
        self.last_frame_id = Some(frame_id);
        self.ring.push_back((track, frame_id, present_ms));
        if self.ring.len() > RING_CAPACITY {
            self.ring.pop_front();
        }
        let mut kept = VecDeque::with_capacity(self.sent.len());
        for entry in self.sent.drain(..) {
            match entry.frame_id {
                Some(announced) if frame_id >= announced && same_track(entry.track, track) => {
                    out.extend(resolve(entry.seq, entry.sent_ms, present_ms));
                }
                _ => kept.push_back(entry),
            }
        }
        self.sent = kept;
        out
    }

    pub(crate) fn note_input_frame(
        &mut self,
        seq: u64,
        track: Option<u64>,
        frame_id: u64,
        now_ms: f64,
    ) -> Vec<PhotonOutcome> {
        let mut out = self.expire(now_ms);
        let Some(index) = self.sent.iter().position(|entry| entry.seq == seq) else {
            return out;
        };
        if self.sent[index].frame_id.is_some() {
            return out;
        }
        self.sent[index].frame_id = Some(frame_id);
        self.sent[index].track = track;
        let sent_ms = self.sent[index].sent_ms;
        // Another publication's frame never matches, whatever its id.
        if let Some((_, _, present_ms)) = self
            .ring
            .iter()
            .find(|(frame_track, id, present_ms)| {
                same_track(track, *frame_track) && *id >= frame_id && *present_ms >= sent_ms
            })
            .copied()
        {
            self.sent.remove(index);
            out.extend(resolve(seq, sent_ms, present_ms));
        }
        out
    }

    pub(crate) fn expire(&mut self, now_ms: f64) -> Vec<PhotonOutcome> {
        let mut out = Vec::new();
        self.sent.retain(|entry| {
            let age = now_ms - entry.sent_ms;
            match entry.frame_id {
                Some(_) if age > TIMEOUT_MS => {
                    out.push(PhotonOutcome::Timeout { seq: entry.seq });
                    false
                }
                None if age > SENT_TTL_MS => false,
                _ => true,
            }
        });
        while self
            .ring
            .front()
            .is_some_and(|(_, _, present_ms)| *present_ms < now_ms - RING_MS)
        {
            self.ring.pop_front();
        }
        out
    }

    pub(crate) fn reset(&mut self) {
        self.sent.clear();
        self.ring.clear();
        self.last_track = None;
        self.last_frame_id = None;
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.sent.len()
    }
}

fn resolve(seq: u64, sent_ms: f64, present_ms: f64) -> Option<PhotonOutcome> {
    let ms = present_ms - sent_ms;
    if !ms.is_finite() || ms < 0.0 {
        return None;
    }
    Some(if ms > TIMEOUT_MS {
        PhotonOutcome::Timeout { seq }
    } else {
        PhotonOutcome::Photon { seq, ms }
    })
}

/// Milliseconds on a process-wide monotonic axis (the controller's clock).
fn mono_ms(at: Instant) -> f64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(|| {
        let now = Instant::now();
        now.checked_sub(Duration::from_secs(3600)).unwrap_or(now)
    });
    at.saturating_duration_since(epoch).as_secs_f64() * 1000.0
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------- host side

/// What the host tells one controller about one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Announcement {
    pub(crate) controller_id: String,
    pub(crate) window_id: u32,
    pub(crate) seq: u64,
    pub(crate) frame_id: u32,
    pub(crate) content_change: &'static str,
}

#[derive(Debug, Clone)]
struct HostMarker {
    seq: u64,
    injected_at: Instant,
}

#[derive(Debug, Clone)]
struct AwaitingPublish {
    controller_id: String,
    window_id: u32,
    seq: u64,
    capture_seq: u64,
    content_change: &'static str,
    at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct HostPhoton {
    filters: HashMap<(u32, String), PhotonInputFilter>,
    /// Oldest pending discrete input per (window, controller).
    markers: HashMap<(u32, String), HostMarker>,
    last_marked: HashMap<String, Instant>,
    awaiting: Vec<AwaitingPublish>,
}

impl HostPhoton {
    /// An input finished injecting. Discrete ops only, 4/s per controller.
    pub(crate) fn note_injected(&mut self, message: &RemoteControlMessage, now: Instant) {
        let key = (message.window_id, message.controller_id.clone());
        let admitted = self.filters.entry(key.clone()).or_default().admit(
            message.message_type,
            message.action,
            message.repeat,
            mono_ms(now),
        );
        if !admitted {
            return;
        }
        if self
            .markers
            .get(&key)
            .is_some_and(|marker| now.saturating_duration_since(marker.injected_at) < HOST_TTL)
        {
            return;
        }
        if self
            .last_marked
            .get(&message.controller_id)
            .is_some_and(|at| now.saturating_duration_since(*at) < HOST_MIN_INTERVAL)
        {
            return;
        }
        self.last_marked.insert(message.controller_id.clone(), now);
        self.markers.insert(
            key,
            HostMarker {
                seq: message.seq,
                injected_at: now,
            },
        );
    }

    /// The #373 match point: a capture of `window_id` showed a change.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn note_captured(
        &mut self,
        window_id: u32,
        capture_seq: u64,
        change: CapturedContentChange,
        now: Instant,
    ) {
        self.expire(now);
        let content_change = match change {
            CapturedContentChange::None => return,
            CapturedContentChange::Detected => "detected",
            CapturedContentChange::Unknown => "unknown",
        };
        let keys: Vec<_> = self
            .markers
            .keys()
            .filter(|(window, _)| *window == window_id)
            .cloned()
            .collect();
        for key in keys {
            if let Some(marker) = self.markers.remove(&key) {
                self.awaiting.push(AwaitingPublish {
                    controller_id: key.1,
                    window_id,
                    seq: marker.seq,
                    capture_seq,
                    content_change,
                    at: now,
                });
            }
        }
    }

    /// A capture of `window_id` reached the encoder as trailer `frame_id`.
    /// It carries every change captured at or before it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn take_published(
        &mut self,
        window_id: u32,
        capture_seq: u64,
        frame_id: u32,
        now: Instant,
    ) -> Vec<Announcement> {
        self.expire(now);
        let mut out = Vec::new();
        self.awaiting.retain(|entry| {
            if entry.window_id != window_id || entry.capture_seq > capture_seq {
                return true;
            }
            out.push(Announcement {
                controller_id: entry.controller_id.clone(),
                window_id,
                seq: entry.seq,
                frame_id,
                content_change: entry.content_change,
            });
            false
        });
        out
    }

    fn expire(&mut self, now: Instant) {
        self.markers
            .retain(|_, marker| now.saturating_duration_since(marker.injected_at) < HOST_TTL);
        self.awaiting
            .retain(|entry| now.saturating_duration_since(entry.at) < HOST_TTL);
        self.last_marked
            .retain(|_, at| now.saturating_duration_since(*at) < HOST_TTL);
    }

    pub(crate) fn clear_controller(&mut self, window_id: u32, controller_id: &str) {
        let mine = |window: u32, controller: &str| window == window_id && controller == controller_id;
        self.filters.retain(|(window, controller), _| !mine(*window, controller));
        self.markers.retain(|(window, controller), _| !mine(*window, controller));
        self.awaiting
            .retain(|entry| !mine(entry.window_id, &entry.controller_id));
        self.last_marked.remove(controller_id);
    }

    pub(crate) fn clear_window(&mut self, window_id: u32) {
        self.filters.retain(|(window, _), _| *window != window_id);
        self.markers.retain(|(window, _), _| *window != window_id);
        self.awaiting.retain(|entry| entry.window_id != window_id);
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

fn host() -> &'static Mutex<HostPhoton> {
    static HOST: OnceLock<Mutex<HostPhoton>> = OnceLock::new();
    HOST.get_or_init(Default::default)
}

pub(crate) fn host_note_injected(message: &RemoteControlMessage) {
    lock(host()).note_injected(message, Instant::now());
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn host_note_captured(window_id: u32, capture_seq: u64, change: CapturedContentChange) {
    lock(host()).note_captured(window_id, capture_seq, change, Instant::now());
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn host_take_published(window_id: u32, capture_seq: u64, frame_id: u32) -> Vec<Announcement> {
    lock(host()).take_published(window_id, capture_seq, frame_id, Instant::now())
}

pub(crate) fn host_clear_controller(window_id: u32, controller_id: &str) {
    lock(host()).clear_controller(window_id, controller_id);
}

pub(crate) fn host_clear_window(window_id: u32) {
    lock(host()).clear_window(window_id);
}

pub(crate) fn host_clear() {
    lock(host()).clear();
}

// ---------------------------------------------------------- controller side

#[derive(Debug, Default)]
struct ControlledWindow {
    filter: PhotonInputFilter,
    correlator: PhotonCorrelator,
    grant: Option<u64>,
}

/// Per (host identity, window). Only windows we hold a grant on.
#[derive(Debug, Default)]
pub(crate) struct ControllerPhoton {
    windows: HashMap<(String, u32), ControlledWindow>,
}

impl ControllerPhoton {
    fn window(&mut self, host: &str, window_id: u32, grant: u64) -> &mut ControlledWindow {
        let entry = self.windows.entry((host.to_string(), window_id)).or_default();
        if entry.grant != Some(grant) {
            *entry = ControlledWindow {
                grant: Some(grant),
                ..Default::default()
            };
        }
        entry
    }

    pub(crate) fn sent(
        &mut self,
        message: &RemoteControlMessage,
        grant: u64,
        sent_ms: f64,
    ) -> Vec<PhotonOutcome> {
        let window = self.window(&message.target_user_id, message.window_id, grant);
        if !window
            .filter
            .admit(message.message_type, message.action, message.repeat, sent_ms)
        {
            return Vec::new();
        }
        window.correlator.note_sent(message.seq, sent_ms)
    }

    /// `message.controller_id` is the verified host.
    pub(crate) fn input_frame(
        &mut self,
        message: &RemoteControlMessage,
        grant: u64,
        now_ms: f64,
    ) -> Vec<PhotonOutcome> {
        let Some(frame_id) = message.frame_id else {
            return Vec::new();
        };
        self.window(&message.controller_id, message.window_id, grant)
            .correlator
            .note_input_frame(message.seq, message.track_sid.as_deref().map(track_key), u64::from(frame_id), now_ms)
    }

    pub(crate) fn presented(
        &mut self,
        host: &str,
        window_id: u32,
        grant: u64,
        track: u64,
        frame_id: u32,
        present_ms: f64,
    ) -> Vec<PhotonOutcome> {
        self.window(host, window_id, grant)
            .correlator
            .note_presented(track, u64::from(frame_id), present_ms)
    }

    /// Sweeper tick: drop windows we no longer control, time out the rest.
    pub(crate) fn tick(&mut self, live_grant: impl Fn(&str, u32) -> Option<u64>, now_ms: f64) -> Vec<PhotonOutcome> {
        self.windows
            .retain(|(host, window_id), window| live_grant(host, *window_id) == window.grant);
        self.windows
            .values_mut()
            .flat_map(|window| window.correlator.expire(now_ms))
            .collect()
    }
}

fn controller() -> &'static Mutex<ControllerPhoton> {
    static CONTROLLER: OnceLock<Mutex<ControllerPhoton>> = OnceLock::new();
    CONTROLLER.get_or_init(Default::default)
}

fn record(outcomes: Vec<PhotonOutcome>, content_change: Option<&str>) {
    for outcome in outcomes {
        match outcome {
            PhotonOutcome::Photon { ms, .. } => {
                rc_latency_telemetry::record_stage(RcRole::Controller, Stage::PhotonMs, ms);
                if let Some(change) = content_change {
                    rc_latency_telemetry::set_dimension(RcRole::Controller, Dimension::ContentChange, change);
                }
            }
            PhotonOutcome::Timeout { .. } => {
                rc_latency_telemetry::add_counter(RcRole::Controller, Counter::PhotonTimeout, 1);
            }
        }
    }
}

/// Controller: an input left for a host window we control. Stamped with
/// `Instant` here, never from wall-clock `sentAtMs` (no clock mixing).
pub(crate) fn controller_sent(message: &RemoteControlMessage) {
    let Some(grant) = crate::rc_input_timing::window_grant(&message.target_user_id, message.window_id) else {
        return;
    };
    let sent_ms = mono_ms(Instant::now());
    let outcomes = lock(controller()).sent(message, grant, sent_ms);
    record(outcomes, None);
}

/// Controller: a host's `inputFrame` (sender already verified as owner).
pub(crate) fn controller_input_frame(message: &RemoteControlMessage) {
    let Some(grant) = crate::rc_input_timing::window_grant(&message.controller_id, message.window_id) else {
        return;
    };
    let change = content_change_dimension(message.content_change.as_deref());
    if let Some(change) = change {
        lock(content_changes()).insert((message.controller_id.clone(), message.window_id), change);
    }
    let outcomes = lock(controller()).input_frame(message, grant, mono_ms(Instant::now()));
    record(outcomes, change);
}

/// The last `contentChange` a host announced per window, for the dimension
/// on samples that resolve at present time.
fn content_changes() -> &'static Mutex<HashMap<(String, u32), &'static str>> {
    static CHANGES: OnceLock<Mutex<HashMap<(String, u32), &'static str>>> = OnceLock::new();
    CHANGES.get_or_init(Default::default)
}

fn content_change_dimension(value: Option<&str>) -> Option<&'static str> {
    match value {
        Some("detected") => Some("detected"),
        Some(_) => Some("unknown"),
        None => None,
    }
}

/// Controller: the compositor just handed a frame of a controlled window to
/// the display; it lights about one refresh later. `Instant` only.
pub(crate) fn controller_presented(host: &str, window_id: u32, grant: u64, track: u64, frame_id: u32, refresh_ms: f64) {
    let present_ms = mono_ms(Instant::now()) + refresh_ms;
    let outcomes = lock(controller()).presented(host, window_id, grant, track, frame_id, present_ms);
    if outcomes.is_empty() {
        return;
    }
    let change = lock(content_changes()).get(&(host.to_string(), window_id)).copied();
    record(outcomes, change);
}

/// Controller sweeper tick (rides `rc_input_timing::sync_controller`).
pub(crate) fn controller_tick() {
    let outcomes = lock(controller()).tick(crate::rc_input_timing::window_grant, mono_ms(Instant::now()));
    record(outcomes, None);
    let live: Vec<(String, u32)> = lock(controller()).windows.keys().cloned().collect();
    lock(content_changes()).retain(|key, _| live.contains(key));
}

#[cfg(test)]
pub(crate) fn controller_state_for_test(host: &str, window_id: u32) -> Option<(Vec<u64>, usize)> {
    lock(controller()).windows.get(&(host.to_string(), window_id)).map(|window| {
        (
            window.correlator.ring.iter().map(|(_, id, _)| *id).collect(),
            window.correlator.sent.len(),
        )
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_control_core::{RemoteControlModifiers, VERSION};
    use serde::Deserialize;

    const RAW_CONTRACT: &str = include_str!("../../../../contracts/petal-contracts.json");

    #[derive(Deserialize)]
    struct Contract {
        #[serde(rename = "photonCorrelator")]
        config: Config,
        #[serde(rename = "photonCorrelatorVectors")]
        vectors: Vec<Vector>,
        #[serde(rename = "photonInputFilterVectors")]
        filters: Vec<FilterVector>,
        #[serde(rename = "remoteControlMessages")]
        messages: Vec<MessageFixture>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Config {
        sent_capacity: usize,
        sent_ttl_ms: f64,
        ring_ms: f64,
        ring_capacity: usize,
        timeout_ms: f64,
        wheel_burst_idle_ms: f64,
        host_min_interval_ms: u64,
    }

    #[derive(Deserialize)]
    struct Vector {
        name: String,
        steps: Vec<Step>,
        expected: Vec<Expected>,
    }

    #[derive(Deserialize)]
    #[serde(tag = "op", rename_all = "camelCase")]
    enum Step {
        Sent { seq: u64, at: f64 },
        Presented {
            track: String,
            #[serde(rename = "frameId")]
            frame_id: u64,
            at: f64,
        },
        InputFrame {
            seq: u64,
            track: String,
            #[serde(rename = "frameId")]
            frame_id: u64,
            at: f64,
        },
        Tick { at: f64 },
        Reset,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    #[serde(tag = "kind", rename_all = "camelCase")]
    enum Expected {
        Photon { seq: u64, ms: f64 },
        Timeout { seq: u64 },
    }

    #[derive(Deserialize)]
    struct FilterVector {
        name: String,
        steps: Vec<FilterStep>,
    }

    #[derive(Deserialize)]
    struct FilterStep {
        input: FilterInput,
        at: f64,
        admit: bool,
    }

    #[derive(Deserialize)]
    struct FilterInput {
        kind: RemoteControlType,
        #[serde(default)]
        action: Option<RemoteControlAction>,
        #[serde(default)]
        repeat: bool,
    }

    #[derive(Deserialize)]
    struct MessageFixture {
        name: String,
        message: serde_json::Value,
        fields: Vec<String>,
    }

    fn contract() -> Contract {
        serde_json::from_str(RAW_CONTRACT).expect("contract parses")
    }

    #[test]
    fn constants_match_the_shared_contract() {
        let c = contract().config;
        assert_eq!(c.sent_capacity, SENT_CAPACITY);
        assert_eq!(c.sent_ttl_ms, SENT_TTL_MS);
        assert_eq!(c.ring_ms, RING_MS);
        assert_eq!(c.ring_capacity, RING_CAPACITY);
        assert_eq!(c.timeout_ms, TIMEOUT_MS);
        assert_eq!(c.wheel_burst_idle_ms, WHEEL_BURST_IDLE_MS);
        assert_eq!(Duration::from_millis(c.host_min_interval_ms), HOST_MIN_INTERVAL);
    }

    #[test]
    fn correlator_passes_the_shared_contract_vectors() {
        let vectors = contract().vectors;
        for required in [
            "presented-after-input-frame",
            "presented-before-input-frame",
            "skipped-frame-ids-match-greater",
            "announced-never-presented-times-out",
            "duplicate-input-frame-after-resolve",
            "frame-id-drop-resets-without-timeout",
        ] {
            assert!(vectors.iter().any(|v| v.name == required), "contract lacks {required}");
        }
        for vector in vectors {
            let mut correlator = PhotonCorrelator::default();
            let mut got = Vec::new();
            for step in &vector.steps {
                got.extend(match step {
                    Step::Sent { seq, at } => correlator.note_sent(*seq, *at),
                    Step::Presented { track, frame_id, at } => {
                        correlator.note_presented(track_key(track), *frame_id, *at)
                    }
                    Step::InputFrame { seq, track, frame_id, at } => {
                        correlator.note_input_frame(*seq, Some(track_key(track)), *frame_id, *at)
                    }
                    Step::Tick { at } => correlator.expire(*at),
                    Step::Reset => {
                        correlator.reset();
                        Vec::new()
                    }
                });
            }
            let got: Vec<Expected> = got
                .into_iter()
                .map(|outcome| match outcome {
                    PhotonOutcome::Photon { seq, ms } => Expected::Photon { seq, ms },
                    PhotonOutcome::Timeout { seq } => Expected::Timeout { seq },
                })
                .collect();
            assert_eq!(got, vector.expected, "{}", vector.name);
        }
    }

    #[test]
    fn input_filter_passes_the_shared_contract_vectors() {
        for vector in contract().filters {
            let mut filter = PhotonInputFilter::default();
            for step in vector.steps {
                let admitted = filter.admit(step.input.kind, step.input.action, step.input.repeat, step.at);
                assert_eq!(admitted, step.admit, "{} {:?}@{}", vector.name, step.input.kind, step.at);
            }
        }
    }

    #[test]
    fn correlator_state_stays_bounded() {
        let mut correlator = PhotonCorrelator::default();
        for seq in 0..1_000 {
            correlator.note_sent(seq, 0.0);
            correlator.note_presented(1, seq, 1.0);
        }
        assert!(correlator.pending() <= SENT_CAPACITY);
        assert!(correlator.ring.len() <= RING_CAPACITY);
    }

    fn input(kind: RemoteControlType, action: Option<RemoteControlAction>, controller: &str, window_id: u32, seq: u64) -> RemoteControlMessage {
        let mut value = serde_json::json!({
            "v": VERSION,
            "kind": kind,
            "targetUserId": "host",
            "controllerId": controller,
            "windowId": window_id,
            "seq": seq,
            "modifiers": RemoteControlModifiers::default(),
        });
        if let Some(action) = action {
            value["action"] = serde_json::to_value(action).unwrap();
        }
        serde_json::from_value(value).unwrap()
    }

    fn click(controller: &str, window_id: u32, seq: u64) -> RemoteControlMessage {
        input(RemoteControlType::Pointer, Some(RemoteControlAction::Click), controller, window_id, seq)
    }

    #[test]
    fn host_announces_only_discrete_ops() {
        let t0 = Instant::now();
        let mut host = HostPhoton::default();
        let mut at = t0;
        let mut step = || {
            at += Duration::from_millis(300);
            at
        };
        for (message, announced) in [
            (input(RemoteControlType::Pointer, Some(RemoteControlAction::Move), "c", 1, 1), false),
            (input(RemoteControlType::Pointer, Some(RemoteControlAction::Up), "c", 1, 2), false),
            (input(RemoteControlType::Pointer, Some(RemoteControlAction::Down), "c", 1, 3), true),
            (input(RemoteControlType::Key, Some(RemoteControlAction::Up), "c", 1, 4), false),
            (input(RemoteControlType::Key, Some(RemoteControlAction::Down), "c", 1, 5), true),
            (input(RemoteControlType::Text, None, "c", 1, 6), true),
            (input(RemoteControlType::Wheel, None, "c", 1, 7), true),
        ] {
            let now = step();
            host.note_injected(&message, now);
            host.note_captured(1, message.seq, CapturedContentChange::Detected, now);
            let got = host.take_published(1, message.seq, 100 + message.seq as u32, now);
            assert_eq!(!got.is_empty(), announced, "{:?} {:?}", message.message_type, message.action);
            if announced {
                assert_eq!((got[0].seq, got[0].frame_id), (message.seq, 100 + message.seq as u32));
            }
        }
        // A wheel inside the burst is not a new press.
        let now = at + Duration::from_millis(100);
        host.note_injected(&input(RemoteControlType::Wheel, None, "c", 1, 8), now);
        host.note_captured(1, 8, CapturedContentChange::Detected, now);
        assert!(host.take_published(1, 8, 200, now).is_empty());
    }

    #[test]
    fn host_rate_limits_each_controller_to_four_per_second() {
        let t0 = Instant::now();
        let mut host = HostPhoton::default();
        let mut announced = 0;
        let mut other = 0;
        // 50 clicks/s for 2 s from one controller, interleaved with another.
        for i in 0..100u64 {
            let now = t0 + Duration::from_millis(i * 20);
            host.note_injected(&click("busy", 7, i), now);
            if i % 25 == 0 {
                host.note_injected(&click("calm", 7, 1_000 + i), now);
            }
            host.note_captured(7, i, CapturedContentChange::Detected, now);
            for a in host.take_published(7, i, i as u32, now) {
                if a.controller_id == "busy" {
                    announced += 1;
                } else {
                    other += 1;
                }
            }
        }
        assert!((7..=8).contains(&announced), "busy controller got {announced} in 2 s");
        assert_eq!(other, 4, "one controller's rate never starves another");
    }

    #[test]
    fn host_announces_with_the_first_publish_at_or_after_the_matched_capture() {
        let t0 = Instant::now();
        let mut host = HostPhoton::default();
        host.note_injected(&click("c", 3, 9), t0);
        // An idle frame does not end the inject.
        host.note_captured(3, 40, CapturedContentChange::None, t0);
        assert!(host.take_published(3, 40, 500, t0).is_empty());
        host.note_captured(3, 41, CapturedContentChange::Unknown, t0);
        // An older capture published late cannot carry the change.
        assert!(host.take_published(3, 40, 501, t0).is_empty());
        // Another window's publish never answers this one.
        assert!(host.take_published(4, 99, 502, t0).is_empty());
        let got = host.take_published(3, 42, 503, t0);
        assert_eq!(
            got,
            vec![Announcement {
                controller_id: "c".to_string(),
                window_id: 3,
                seq: 9,
                frame_id: 503,
                content_change: "unknown",
            }]
        );
        assert!(host.take_published(3, 43, 504, t0).is_empty(), "announced once");
        // A change nobody publishes within the TTL is dropped.
        host.note_injected(&click("c", 3, 10), t0 + Duration::from_secs(2));
        host.note_captured(3, 50, CapturedContentChange::Detected, t0 + Duration::from_secs(2));
        assert!(host.take_published(3, 50, 600, t0 + Duration::from_secs(4)).is_empty());
        // Clearing the window drops pending work.
        host.note_injected(&click("c", 3, 11), t0 + Duration::from_secs(5));
        host.clear_window(3);
        host.note_captured(3, 60, CapturedContentChange::Detected, t0 + Duration::from_secs(5));
        assert!(host.take_published(3, 60, 700, t0 + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn controller_correlates_per_window_and_restarts_on_grant_or_track_change() {
        let mut controller = ControllerPhoton::default();
        let (t9, t10) = (track_key("TR_9"), track_key("TR_10"));
        // Frames flow before any input; the first one binds the track epoch.
        assert!(controller.presented("host-a", 5, 1, t9, 70, 0.0).is_empty());
        let mut sent = click("me", 5, 21);
        sent.target_user_id = "host-a".to_string();
        assert!(controller.sent(&sent, 1, 0.0).is_empty());
        let mut frame = sent.clone();
        frame.message_type = RemoteControlType::InputFrame;
        frame.controller_id = "host-a".to_string();
        frame.frame_id = Some(77);
        frame.track_sid = Some("TR_9".to_string());
        // Presented before the announcement: resolved retroactively.
        assert!(controller.presented("host-a", 5, 1, t9, 77, 40.0).is_empty());
        assert_eq!(controller.input_frame(&frame, 1, 50.0), vec![PhotonOutcome::Photon { seq: 21, ms: 40.0 }]);

        // A pointer move is never remembered.
        let mut moved = input(RemoteControlType::Pointer, Some(RemoteControlAction::Move), "me", 5, 22);
        moved.target_user_id = "host-a".to_string();
        controller.sent(&moved, 1, 60.0);
        frame.seq = 22;
        frame.frame_id = Some(78);
        controller.presented("host-a", 5, 1, t9, 78, 70.0);
        assert!(controller.input_frame(&frame, 1, 80.0).is_empty());

        // A republish (new sid) drops what was announced for the old one.
        sent.seq = 23;
        controller.sent(&sent, 1, 100.0);
        frame.seq = 23;
        frame.frame_id = Some(90);
        assert!(controller.input_frame(&frame, 1, 110.0).is_empty());
        assert!(controller.presented("host-a", 5, 1, t10, 90, 120.0).is_empty());
        assert!(controller.tick(|_, _| Some(1), 2_000.0).is_empty(), "no false timeout");

        // A re-grant starts over too.
        sent.seq = 24;
        controller.sent(&sent, 1, 3_000.0);
        frame.seq = 24;
        frame.frame_id = Some(95);
        controller.input_frame(&frame, 1, 3_010.0);
        assert!(controller.input_frame(&frame, 2, 3_020.0).is_empty());
        assert!(controller.tick(|_, _| Some(2), 5_000.0).is_empty());
        // Losing control drops the window.
        assert!(controller.tick(|_, _| None, 5_001.0).is_empty());
        assert!(controller.windows.is_empty());
    }

    #[test]
    fn announced_input_with_no_present_counts_one_timeout() {
        let mut controller = ControllerPhoton::default();
        controller.presented("host-a", 5, 1, 9, 1, 0.0);
        let mut sent = click("me", 5, 31);
        sent.target_user_id = "host-a".to_string();
        controller.sent(&sent, 1, 0.0);
        let mut frame = sent.clone();
        frame.message_type = RemoteControlType::InputFrame;
        frame.controller_id = "host-a".to_string();
        frame.frame_id = Some(5);
        controller.input_frame(&frame, 1, 30.0);
        assert!(controller.tick(|_, _| Some(1), 900.0).is_empty());
        assert_eq!(controller.tick(|_, _| Some(1), 1_001.0), vec![PhotonOutcome::Timeout { seq: 31 }]);
        assert!(controller.tick(|_, _| Some(1), 2_000.0).is_empty());
    }

    #[test]
    fn an_input_sent_before_the_first_controlled_frame_survives_it() {
        let mut controller = ControllerPhoton::default();
        let mut sent = click("me", 5, 41);
        sent.target_user_id = "host-a".to_string();
        controller.sent(&sent, 1, 0.0);
        assert!(controller.presented("host-a", 5, 1, 9, 3, 20.0).is_empty());
        let mut frame = sent.clone();
        frame.message_type = RemoteControlType::InputFrame;
        frame.controller_id = "host-a".to_string();
        frame.frame_id = Some(3);
        assert_eq!(controller.input_frame(&frame, 1, 30.0), vec![PhotonOutcome::Photon { seq: 41, ms: 20.0 }]);
    }

    #[test]
    fn revoking_one_controller_clears_only_its_host_state() {
        let t0 = Instant::now();
        let mut host = HostPhoton::default();
        host.note_injected(&click("a", 2, 1), t0);
        host.note_injected(&click("b", 2, 2), t0);
        host.clear_controller(2, "a");
        // Re-granted at once: not rate-limited by the old session.
        host.note_injected(&click("a", 2, 3), t0 + Duration::from_millis(10));
        host.note_captured(2, 1, CapturedContentChange::Detected, t0 + Duration::from_millis(20));
        let mut got: Vec<_> = host
            .take_published(2, 1, 9, t0 + Duration::from_millis(20))
            .into_iter()
            .map(|a| (a.controller_id, a.seq))
            .collect();
        got.sort();
        assert_eq!(got, vec![("a".to_string(), 3), ("b".to_string(), 2)]);
    }

    /// The `inputFrame` fixture parses on this build, and an envelope that
    /// predates the kind (every shipped controller: serde `other`) ignores it.
    #[test]
    fn input_frame_fixture_parses_and_an_old_controller_ignores_it() {
        let fixture = contract()
            .messages
            .into_iter()
            .find(|m| m.name == "input-frame")
            .expect("input-frame fixture");
        let message: RemoteControlMessage = serde_json::from_value(fixture.message.clone()).unwrap();
        assert_eq!(message.message_type, RemoteControlType::InputFrame);
        assert_eq!((message.seq, message.frame_id, message.content_change.as_deref()), (15, Some(1234), Some("detected")));
        let json = serde_json::to_value(&message).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().filter(|k| *k != "modifiers").cloned().collect();
        keys.sort();
        assert_eq!(keys, fixture.fields);

        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(rename_all = "camelCase")]
        enum PreInputFrameKind {
            Request,
            Release,
            Status,
            Pointer,
            Wheel,
            Key,
            Text,
            Result,
            #[serde(other)]
            Unknown,
        }
        #[derive(Deserialize)]
        struct OldEnvelope {
            kind: PreInputFrameKind,
        }
        let old: OldEnvelope = serde_json::from_value(fixture.message).unwrap();
        assert_eq!(old.kind, PreInputFrameKind::Unknown);
    }

    /// Wiring guard: the producers need a live room, so pin that each sits
    /// on the real path: replay success, the #373 match point, the pump's
    /// accepted push, the controller send, its receive, and the present.
    #[test]
    fn every_producer_is_wired_into_the_real_control_path() {
        let rc = include_str!("remote_control.rs");
        let rc = rc.split_once("\n#[cfg(test)]\nmod tests {").map_or(rc, |(head, _)| head);
        let replay = &rc[rc.find("ReplayRunOutcome::Completed(Ok(())) => {").unwrap()..];
        assert!(replay[..400].contains("rc_photon::host_note_injected(&task.message)"));
        let revoke = &rc[rc.find("fn revoke(window_id: u32, controller_id: &str) {").unwrap()..];
        assert!(revoke[..1200].contains("rc_photon::host_clear_controller(window_id, controller_id)"));
        let captured = &rc[rc.find("pub(crate) fn note_captured_frame(").unwrap()..];
        assert!(captured[..300].contains("rc_photon::host_note_captured("));
        let send = &rc[rc.find("pub async fn remote_control_send(").unwrap()..];
        let send = &send[..send.find("\n}\n").unwrap()];
        assert_eq!(send.matches("rc_photon::controller_sent(").count(), 1, "never for fabricated chunk seqs");
        let chunked = &send[send.find("for (index, chunk) in chunks").unwrap()..send.find("return Ok(());").unwrap()];
        assert!(!chunked.contains("rc_photon::controller_sent("));
        assert!(rc.contains("RemoteControlType::InputFrame => {\n            if remote_window_exists(&message.controller_id, message.window_id) {\n                crate::rc_photon::controller_input_frame(&message);"));

        let share = include_str!("session/share.rs");
        assert_eq!(
            share.matches("announce_input_frames(&pump_app, window_id, capture_seq, &current.sid().to_string(), timing);").count(),
            2,
            "raw and retry pushes"
        );
        assert!(share.contains("crate::remote_control::announce_published_frame(app, window_id, capture_seq, timing.frame_id, track_sid)"));
        assert!(share.contains(".filter(|t| t.source_accepted)"));

        let subscriber = include_str!("transport/subscriber.rs");
        assert_eq!(subscriber.matches("photon_track,\n").count(), 2, "both receive loops stamp the track");
        let timing = include_str!("rc_receiver_timing.rs");
        assert!(timing.contains("crate::rc_photon::controller_presented("));
        assert!(rc.contains("message.track_sid = Some(track_sid.to_string());"), "inputFrame names its publication");
        let input_timing = include_str!("rc_input_timing.rs");
        assert!(input_timing.contains("crate::rc_photon::controller_tick();"));
    }
}
