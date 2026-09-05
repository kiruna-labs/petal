//! Platform-neutral remote-control protocol and session state.
//!
//! OS target resolution, accessibility semantics, and input injection stay in
//! `remote_control`; this module owns the wire contract and the state whose
//! invariants must be identical on every host platform.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::platform::cg::WindowFrame;
use crate::sync_ext::MutexExt;
use crate::transport::publisher::RoomConnection;

pub(crate) trait PlatformControl: Send + Sync {
    fn accessibility_trusted(&self) -> bool;
    fn prompt_accessibility(&self) -> bool;
    fn replay(
        &self,
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
    ) -> Result<(), String>;
    fn clear_cached_app(&self, pid: i32);
    fn clear_resolution_cache(&self, window_id: u32);
    fn clear_window_gestures(&self, window_id: u32);
    fn clear_controller_gestures(&self, window_id: u32, controller_id: &str);
    fn clear_all_control_state(&self);
    fn release_window_gestures(&self, window_id: u32);
}

pub(crate) trait ControlSurface {
    fn emit_status(&self, status: RemoteControlStatus);
}
pub(crate) enum BoundedQueuePush<T> {
    Enqueued,
    Coalesced,
    Dropped(T),
}

struct BoundedQueueState<T, K> {
    discrete: VecDeque<T>,
    high_rate: VecDeque<(K, T)>,
}

pub(crate) struct BoundedCoalescingQueue<T, K> {
    state: Mutex<BoundedQueueState<T, K>>,
    ready: Condvar,
    high_rate_capacity: usize,
}

impl<T, K: PartialEq> BoundedCoalescingQueue<T, K> {
    pub(crate) fn new(high_rate_capacity: usize) -> Self {
        Self {
            state: Mutex::new(BoundedQueueState {
                discrete: VecDeque::new(),
                high_rate: VecDeque::new(),
            }),
            ready: Condvar::new(),
            high_rate_capacity,
        }
    }

    pub(crate) fn push(&self, task: T, key: Option<K>) -> BoundedQueuePush<T> {
        let Some(key) = key else {
            self.state.lock_unpoisoned().discrete.push_back(task);
            self.ready.notify_one();
            return BoundedQueuePush::Enqueued;
        };
        let mut state = self.state.lock_unpoisoned();
        if let Some((_, queued)) = state
            .high_rate
            .iter_mut()
            .find(|(queued_key, _)| *queued_key == key)
        {
            *queued = task;
            return BoundedQueuePush::Coalesced;
        }
        if state.high_rate.len() >= self.high_rate_capacity {
            return BoundedQueuePush::Dropped(task);
        }
        state.high_rate.push_back((key, task));
        drop(state);
        self.ready.notify_one();
        BoundedQueuePush::Enqueued
    }

    pub(crate) fn pop(&self) -> T {
        let mut state = self.state.lock_unpoisoned();
        loop {
            if let Some(task) = state
                .discrete
                .pop_front()
                .or_else(|| state.high_rate.pop_front().map(|(_, task)| task))
            {
                return task;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Non-blocking `pop`, drained in the same order. Test-only: production
    /// consumers are dedicated threads that must block, and a non-blocking
    /// pop in that position would spin. Exists so a test can assert the
    /// queue is EMPTY -- the assertion that proves a coalesce replaced an
    /// entry rather than adding one -- without deadlocking on `pop`.
    #[cfg(test)]
    pub(crate) fn try_pop(&self) -> Option<T> {
        let mut state = self.state.lock_unpoisoned();
        state
            .discrete
            .pop_front()
            .or_else(|| state.high_rate.pop_front().map(|(_, task)| task))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ControlGrantKey {
    pub(crate) window_id: u32,
    pub(crate) controller_id: String,
    pub(crate) target_kind: Option<RemoteControlTargetKind>,
    pub(crate) share_instance_id: Option<String>,
}

impl ControlGrantKey {
    pub(crate) fn legacy(window_id: u32, controller_id: impl Into<String>) -> Self {
        Self {
            window_id,
            controller_id: controller_id.into(),
            target_kind: None,
            share_instance_id: None,
        }
    }

    pub(crate) fn for_message(message: &RemoteControlMessage) -> Option<Self> {
        match (message.target_kind, message.share_instance_id.as_deref()) {
            (None, None) => Some(Self::legacy(
                message.window_id,
                message.controller_id.clone(),
            )),
            (
                Some(kind @ (RemoteControlTargetKind::Window | RemoteControlTargetKind::Display)),
                Some(share_instance_id),
            ) if !share_instance_id.is_empty() => Some(Self {
                window_id: message.window_id,
                controller_id: message.controller_id.clone(),
                target_kind: Some(kind),
                share_instance_id: Some(share_instance_id.to_string()),
            }),
            _ => None,
        }
    }

    pub(crate) fn for_admission(admission: &DiscreteAdmission) -> Option<Self> {
        match (
            admission.target_kind,
            admission.share_instance_id.as_deref(),
        ) {
            (None, None) => Some(Self::legacy(
                admission.window_id,
                admission.controller_id.clone(),
            )),
            (
                Some(kind @ (RemoteControlTargetKind::Window | RemoteControlTargetKind::Display)),
                Some(share_instance_id),
            ) if !share_instance_id.is_empty() => Some(Self {
                window_id: admission.window_id,
                controller_id: admission.controller_id.clone(),
                target_kind: Some(kind),
                share_instance_id: Some(share_instance_id.to_string()),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ControllerGrantEnvelope {
    pub(crate) target_kind: RemoteControlTargetKind,
    pub(crate) share_instance_id: String,
    pub(crate) control_session_id: String,
    pub(crate) grant_token: String,
    pub(crate) host_capabilities: Vec<RemoteControlCapability>,
    pub(crate) full_pointer: bool,
    pub(crate) next_input_seq: u64,
}

/// The one owner of portable authorization, reliability, sequencing, and
/// held-input state. Platform orchestration reaches these stores only through
/// the narrow accessors below; no second platform-local copy exists.
pub(crate) struct RemoteControlEngine {
    control_sessions: Mutex<HashMap<ControlGrantKey, String>>,
    warned_tokenless_inputs: Mutex<HashSet<(u32, String)>>,
    hot_path_capable_targets: Mutex<HashSet<(u32, String)>>,
    discrete_admissions: Mutex<DiscreteAdmissionState>,
    controller_pointer_positions: Mutex<HashMap<(u32, String), (f64, f64)>>,
    last_emitted_statuses: Mutex<HashMap<(u32, String), &'static str>>,
    warned_controller_id_mismatches: Mutex<HashSet<(u32, String)>>,
    last_unreliable_seqs: Mutex<HashMap<(u32, String, UnreliableSeqStream), u64>>,
    pressed_inputs: Mutex<HashMap<(u32, String), PressedInputs>>,
    replay_epochs: Mutex<HashMap<(u32, String), u64>>,
    pending_requests: Mutex<HashMap<ControlGrantKey, RemoteControlMessage>>,
    controller_grants: Mutex<HashMap<(u32, String), ControllerGrantEnvelope>>,
}

impl RemoteControlEngine {
    pub(crate) fn new() -> Self {
        Self {
            control_sessions: Mutex::new(HashMap::new()),
            warned_tokenless_inputs: Mutex::new(HashSet::new()),
            hot_path_capable_targets: Mutex::new(HashSet::new()),
            discrete_admissions: Mutex::new(DiscreteAdmissionState::default()),
            controller_pointer_positions: Mutex::new(HashMap::new()),
            last_emitted_statuses: Mutex::new(HashMap::new()),
            warned_controller_id_mismatches: Mutex::new(HashSet::new()),
            last_unreliable_seqs: Mutex::new(HashMap::new()),
            pressed_inputs: Mutex::new(HashMap::new()),
            replay_epochs: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashMap::new()),
            controller_grants: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn sessions(&self) -> &Mutex<HashMap<ControlGrantKey, String>> {
        &self.control_sessions
    }

    pub(crate) fn install_grant(&self, key: ControlGrantKey, token: String) {
        self.control_sessions.lock_unpoisoned().insert(key, token);
    }

    pub(crate) fn active_grant_token(&self, key: &ControlGrantKey) -> Option<String> {
        self.control_sessions.lock_unpoisoned().get(key).cloned()
    }

    pub(crate) fn grant_is_current(&self, admission: &DiscreteAdmission) -> bool {
        ControlGrantKey::for_admission(admission)
            .and_then(|key| self.active_grant_token(&key))
            .is_some_and(|token| token == admission.control_session_id)
    }
    pub(crate) fn revoke_grant(&self, key: &ControlGrantKey) -> bool {
        self.control_sessions
            .lock_unpoisoned()
            .remove(key)
            .is_some()
    }

    pub(crate) fn install_controller_grant(
        &self,
        window_id: u32,
        owner_identity: String,
        grant: ControllerGrantEnvelope,
    ) {
        self.controller_grants
            .lock_unpoisoned()
            .insert((window_id, owner_identity), grant);
    }

    pub(crate) fn remove_controller_grant(
        &self,
        window_id: u32,
        owner_identity: &str,
    ) -> Option<ControllerGrantEnvelope> {
        self.controller_grants
            .lock_unpoisoned()
            .remove(&(window_id, owner_identity.to_string()))
    }

    pub(crate) fn remove_controller_grants_for_window(&self, window_id: u32) {
        self.controller_grants
            .lock_unpoisoned()
            .retain(|(stored_window_id, _), _| *stored_window_id != window_id);
    }

    pub(crate) fn remove_controller_grants_for_owner(&self, owner_identity: &str) {
        self.controller_grants
            .lock_unpoisoned()
            .retain(|(_, stored_owner), _| stored_owner != owner_identity);
    }

    pub(crate) fn controller_grant(
        &self,
        window_id: u32,
        owner_identity: &str,
    ) -> Option<ControllerGrantEnvelope> {
        self.controller_grants
            .lock_unpoisoned()
            .get(&(window_id, owner_identity.to_string()))
            .cloned()
    }

    pub(crate) fn next_controller_grant(
        &self,
        window_id: u32,
        owner_identity: &str,
    ) -> Option<ControllerGrantEnvelope> {
        let mut grants = self.controller_grants.lock_unpoisoned();
        let grant = grants.get_mut(&(window_id, owner_identity.to_string()))?;
        let snapshot = grant.clone();
        grant.next_input_seq = grant.next_input_seq.wrapping_add(1).max(1);
        Some(snapshot)
    }

    pub(crate) fn clear_controller_grants(&self) {
        self.controller_grants.lock_unpoisoned().clear();
    }

    pub(crate) fn store_pending_request(
        &self,
        key: ControlGrantKey,
        message: RemoteControlMessage,
    ) {
        self.pending_requests.lock_unpoisoned().insert(key, message);
    }

    pub(crate) fn take_pending_request(
        &self,
        window_id: u32,
        controller_id: &str,
    ) -> Option<RemoteControlMessage> {
        let mut pending = self.pending_requests.lock_unpoisoned();
        let key = pending
            .keys()
            .find(|key| key.window_id == window_id && key.controller_id == controller_id)
            .cloned()?;
        pending.remove(&key)
    }

    pub(crate) fn clear_pending_requests(&self) {
        self.pending_requests.lock_unpoisoned().clear();
    }

    /// `seq` of the parked request for this (window, controller), if any.
    /// The consent timer captures the seq it armed for and only fires when
    /// the SAME request is still parked -- a re-request replaces the parked
    /// message and re-arms, so a stale timer must not deny the newer one.
    pub(crate) fn pending_request_seq(&self, window_id: u32, controller_id: &str) -> Option<u64> {
        self.pending_requests
            .lock_unpoisoned()
            .iter()
            .find(|(key, _)| key.window_id == window_id && key.controller_id == controller_id)
            .map(|(_, message)| message.seq)
    }

    pub(crate) fn has_pending_request(&self, window_id: u32, controller_id: &str) -> bool {
        self.pending_request_seq(window_id, controller_id).is_some()
    }

    /// Remove and return every parked request matching `retain_if_false`
    /// (a predicate over (window_id, controller_id) returning true for the
    /// entries to TAKE). Used by the revoke paths so a parked request never
    /// outlives its share / controller / meeting.
    pub(crate) fn take_pending_requests_where(
        &self,
        mut take: impl FnMut(u32, &str) -> bool,
    ) -> Vec<RemoteControlMessage> {
        let mut pending = self.pending_requests.lock_unpoisoned();
        let keys: Vec<ControlGrantKey> = pending
            .keys()
            .filter(|key| take(key.window_id, &key.controller_id))
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| pending.remove(&key))
            .collect()
    }

    pub(crate) fn pending_request_keys(&self) -> Vec<(u32, String)> {
        self.pending_requests
            .lock_unpoisoned()
            .keys()
            .map(|key| (key.window_id, key.controller_id.clone()))
            .collect()
    }
    pub(crate) fn clear_discrete_admissions(&self, mut retain: impl FnMut(u32, &str) -> bool) {
        let mut state = self.discrete_admissions.lock_unpoisoned();
        state
            .entries
            .retain(|key, _| retain(key.window_id, &key.controller_id));
        state
            .overload_epoch
            .retain(|key, _| retain(key.window_id, &key.controller_id));
        state
            .overload_until
            .retain(|key, _| retain(key.window_id, &key.controller_id));
    }

    pub(crate) fn admit_discrete_operation(
        &self,
        message: &RemoteControlMessage,
        admission: &DiscreteAdmission,
        now: Instant,
    ) -> AdmissionDecision {
        if !operation_fingerprint_matches(message, admission) {
            return AdmissionDecision::Malformed;
        }
        let mut state = self.discrete_admissions.lock_unpoisoned();
        prune_discrete_admissions(&mut state, now);
        let key = DiscreteAdmissionKey::from(admission);
        if let Some(existing) = state.entries.get(&key) {
            if existing.operation_fingerprint != admission.operation_fingerprint {
                return AdmissionDecision::Malformed;
            }
            return existing
                .terminal_disposition
                .map(AdmissionDecision::CompletedDuplicate)
                .unwrap_or(AdmissionDecision::InFlightDuplicate);
        }
        let grant_key = AdmissionGrantKey::from(admission);
        let grant_entries = state
            .entries
            .keys()
            .filter(|entry| {
                entry.controller_id == grant_key.controller_id
                    && entry.window_id == grant_key.window_id
                    && entry.control_session_id == grant_key.control_session_id
                    && entry.target_kind == grant_key.target_kind
                    && entry.share_instance_id == grant_key.share_instance_id
            })
            .count();
        if state.overload_until.contains_key(&grant_key)
            || grant_entries >= DISCRETE_ADMISSION_CAPACITY
        {
            let epoch = state.overload_epoch.entry(grant_key.clone()).or_default();
            *epoch = epoch.wrapping_add(1);
            state
                .overload_until
                .insert(grant_key, now + DISCRETE_OVERLOAD_WINDOW);
            return AdmissionDecision::Overloaded;
        }
        state.entries.insert(
            key,
            AdmissionEntry {
                operation_fingerprint: admission.operation_fingerprint.clone(),
                terminal_disposition: None,
                admitted_at: now,
            },
        );
        AdmissionDecision::Admitted
    }

    pub(crate) fn complete_discrete_operation(
        &self,
        admission: &DiscreteAdmission,
        disposition: TerminalDisposition,
    ) -> bool {
        let mut state = self.discrete_admissions.lock_unpoisoned();
        let Some(entry) = state
            .entries
            .get_mut(&DiscreteAdmissionKey::from(admission))
        else {
            return false;
        };
        if entry.terminal_disposition.is_some() {
            return false;
        }
        entry.terminal_disposition = Some(disposition);
        true
    }

    pub(crate) fn admission_is_still_inflight(
        &self,
        admission: &DiscreteAdmission,
        now: Instant,
    ) -> bool {
        let mut state = self.discrete_admissions.lock_unpoisoned();
        prune_discrete_admissions(&mut state, now);
        state
            .entries
            .get(&DiscreteAdmissionKey::from(admission))
            .is_some_and(|entry| entry.terminal_disposition.is_none())
    }
    pub(crate) fn reset_unreliable_seq(&self, window_id: u32, controller_id: &str) {
        self.last_unreliable_seqs.lock_unpoisoned().retain(
            |(stored_window_id, stored_controller_id, _), _| {
                *stored_window_id != window_id || stored_controller_id != controller_id
            },
        );
    }

    pub(crate) fn accept_unreliable_seq(
        &self,
        message: &RemoteControlMessage,
    ) -> UnreliableSeqDecision {
        let Some(stream) = unreliable_seq_stream(message) else {
            return UnreliableSeqDecision::Accepted;
        };
        let key = (message.window_id, message.controller_id.clone(), stream);
        let mut last_seqs = self.last_unreliable_seqs.lock_unpoisoned();
        let Some(last_seq) = last_seqs.get_mut(&key) else {
            last_seqs.insert(key, message.seq);
            return UnreliableSeqDecision::Accepted;
        };
        if message.seq < *last_seq {
            if *last_seq >= CONTROLLER_RESTART_WATERMARK_MIN
                && message.seq <= CONTROLLER_RESTART_SEQ_MAX
            {
                let previous = *last_seq;
                *last_seq = message.seq;
                return UnreliableSeqDecision::AcceptedRestart { stream, previous };
            }
            return UnreliableSeqDecision::Rejected {
                stream,
                last_seen: *last_seq,
            };
        }
        *last_seq = message.seq;
        UnreliableSeqDecision::Accepted
    }
    pub(crate) fn replay_epoch(&self, window_id: u32, controller_id: &str) -> u64 {
        let mut epochs = self.replay_epochs.lock_unpoisoned();
        *epochs
            .entry((window_id, controller_id.to_string()))
            .or_insert(0)
    }
    pub(crate) fn bump_replay_epoch(&self, window_id: u32, controller_id: &str, reason: &str) {
        let mut epochs = self.replay_epochs.lock_unpoisoned();
        let next = epochs
            .entry((window_id, controller_id.to_string()))
            .and_modify(|epoch| *epoch = epoch.wrapping_add(1))
            .or_insert(1);
        log::debug!(
            "remote-control: replay epoch for window {window_id} controller='{controller_id}' -> {next} ({reason})"
        );
    }

    pub(crate) fn bump_replay_epoch_for_window(&self, window_id: u32, reason: &str) {
        let mut epochs = self.replay_epochs.lock_unpoisoned();
        for ((stored_window_id, controller_id), epoch) in epochs.iter_mut() {
            if *stored_window_id == window_id {
                *epoch = epoch.wrapping_add(1);
                log::debug!(
                    "remote-control: replay epoch for window {stored_window_id} controller='{controller_id}' -> {epoch} ({reason})"
                );
            }
        }
    }

    pub(crate) fn bump_all_replay_epochs(&self, reason: &str) {
        let mut epochs = self.replay_epochs.lock_unpoisoned();
        for ((window_id, controller_id), epoch) in epochs.iter_mut() {
            *epoch = epoch.wrapping_add(1);
            log::debug!(
                "remote-control: replay epoch for window {window_id} controller='{controller_id}' -> {epoch} ({reason})"
            );
        }
    }

    pub(crate) fn is_current_replay_epoch(&self, task: &ReplayTask) -> bool {
        task.synthetic_release
            || self.replay_epoch(task.message.window_id, &task.message.controller_id)
                == task.replay_epoch
    }

    pub(crate) fn emit_status(&self, surface: &dyn ControlSurface, status: RemoteControlStatus) {
        surface.emit_status(status);
    }

    pub(crate) fn should_warn_controller_id_mismatch(
        &self,
        window_id: u32,
        controller_id: &str,
    ) -> bool {
        self.warned_controller_id_mismatches
            .lock_unpoisoned()
            .insert((window_id, controller_id.to_string()))
    }

    pub(crate) fn bind_trusted_sender(
        &self,
        trusted_sender: Option<String>,
        mut message: RemoteControlMessage,
    ) -> Option<RemoteControlMessage> {
        let Some(sender) = trusted_sender else {
            log::debug!("remote-control: dropping anonymous data packet");
            return None;
        };
        if message.controller_id != sender
            && self.should_warn_controller_id_mismatch(message.window_id, &sender)
        {
            log::warn!(
                "remote-control: controllerId '{}' did not match packet sender '{}'; using trusted sender",
                message.controller_id,
                sender
            );
        }
        message.controller_id = sender;
        Some(message)
    }

    pub(crate) fn data_packet_for(&self, message: &RemoteControlMessage) -> livekit::DataPacket {
        let target = message.target_user_id.clone();
        let is_hot_path_kind = matches!(
            (message.message_type, message.action),
            (RemoteControlType::Pointer, Some(RemoteControlAction::Move))
        ) || message.message_type == RemoteControlType::Wheel;
        let target_is_hot_path_capable = is_hot_path_kind
            && self
                .hot_path_capable_targets
                .lock_unpoisoned()
                .contains(&(message.window_id, target.clone()));
        let payload = target_is_hot_path_capable
            .then(|| binary_frame_for(message))
            .flatten()
            .unwrap_or_else(|| {
                serde_json::to_vec(message).expect("remote-control message is serializable")
            });
        livekit::DataPacket {
            payload,
            topic: Some(TOPIC.to_string()),
            reliable: unreliable_seq_stream(message).is_none(),
            destination_identities: if target.is_empty() {
                Vec::new()
            } else {
                vec![livekit::prelude::ParticipantIdentity(target)]
            },
        }
    }

    pub(crate) async fn publish_message(
        &self,
        room_connection: Arc<RoomConnection>,
        message: RemoteControlMessage,
    ) -> Result<(), String> {
        let packet = self.data_packet_for(&message);
        room_connection
            .room()
            .local_participant()
            .publish_data(packet)
            .await
            .map_err(|error| format!("publish remote-control data: {error}"))
    }
    pub(crate) fn warned_tokenless_inputs(&self) -> &Mutex<HashSet<(u32, String)>> {
        &self.warned_tokenless_inputs
    }

    pub(crate) fn hot_path_capable_targets(&self) -> &Mutex<HashSet<(u32, String)>> {
        &self.hot_path_capable_targets
    }

    pub(crate) fn discrete_admissions(&self) -> &Mutex<DiscreteAdmissionState> {
        &self.discrete_admissions
    }

    pub(crate) fn controller_pointer_positions(
        &self,
    ) -> &Mutex<HashMap<(u32, String), (f64, f64)>> {
        &self.controller_pointer_positions
    }

    pub(crate) fn last_emitted_statuses(&self) -> &Mutex<HashMap<(u32, String), &'static str>> {
        &self.last_emitted_statuses
    }

    pub(crate) fn warned_controller_id_mismatches(&self) -> &Mutex<HashSet<(u32, String)>> {
        &self.warned_controller_id_mismatches
    }

    pub(crate) fn last_unreliable_seqs(
        &self,
    ) -> &Mutex<HashMap<(u32, String, UnreliableSeqStream), u64>> {
        &self.last_unreliable_seqs
    }

    pub(crate) fn pressed_inputs(&self) -> &Mutex<HashMap<(u32, String), PressedInputs>> {
        &self.pressed_inputs
    }

    pub(crate) fn replay_epochs(&self) -> &Mutex<HashMap<(u32, String), u64>> {
        &self.replay_epochs
    }

    pub(crate) fn replay_task(
        &self,
        message: RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
        synthetic_release: bool,
    ) -> ReplayTask {
        let replay_epoch = self.replay_epoch(message.window_id, &message.controller_id);
        let admission = v2_discrete_admission(&message);
        ReplayTask {
            message,
            frame,
            target_pid,
            replay_epoch,
            synthetic_release,
            admission,
            terminal_on_success: true,
            result_sender: None,
        }
    }

    pub(crate) fn track_key_input(
        &self,
        message: &RemoteControlMessage,
        frame: WindowFrame,
        target_pid: Option<i32>,
    ) -> Vec<ReplayTask> {
        let store_key = (message.window_id, message.controller_id.clone());
        let now = Instant::now();
        let mut pressed_by_controller = self.pressed_inputs.lock_unpoisoned();
        if let Some(pressed) = pressed_by_controller.get_mut(&store_key) {
            pressed.last_activity_at = now;
        }
        match (message.message_type, message.action) {
            (RemoteControlType::Key, Some(RemoteControlAction::Down)) if !message.repeat => {
                let mut release = message.clone();
                release.action = Some(RemoteControlAction::Up);
                release.repeat = false;
                let release = self.replay_task(release, frame, target_pid, true);
                pressed_by_controller
                    .entry(store_key)
                    .or_insert_with(|| PressedInputs::new(now))
                    .keys
                    .insert(key_identity(message), HeldInput { release });
                Vec::new()
            }
            (RemoteControlType::Key, Some(RemoteControlAction::Up)) => {
                let release_identity = key_identity(message);
                let Some(pressed) = pressed_by_controller.get_mut(&store_key) else {
                    return Vec::new();
                };
                let synthetic = matching_key_release_identity(&pressed.keys, &release_identity)
                    .and_then(|identity| {
                        pressed.keys.remove(&identity).map(|held| (identity, held))
                    })
                    .and_then(|(identity, held)| {
                        (identity != release_identity).then_some(held.release)
                    })
                    .into_iter()
                    .collect();
                if pressed.is_empty() {
                    pressed_by_controller.remove(&store_key);
                }
                synthetic
            }
            _ => Vec::new(),
        }
    }

    pub(crate) fn drain_pressed_for_controller(
        &self,
        window_id: u32,
        controller_id: &str,
    ) -> Vec<ReplayTask> {
        self.pressed_inputs
            .lock_unpoisoned()
            .remove(&(window_id, controller_id.to_string()))
            .map(|pressed| pressed.into_releases().collect())
            .unwrap_or_default()
    }

    pub(crate) fn drain_pressed_for_window(&self, window_id: u32) -> Vec<ReplayTask> {
        let mut releases = Vec::new();
        self.pressed_inputs
            .lock_unpoisoned()
            .retain(|(stored_window_id, _), pressed| {
                if *stored_window_id == window_id {
                    releases.extend(pressed.drain_releases());
                    false
                } else {
                    true
                }
            });
        releases
    }

    pub(crate) fn drain_pressed_for_controller_id(&self, controller_id: &str) -> Vec<ReplayTask> {
        let mut releases = Vec::new();
        self.pressed_inputs
            .lock_unpoisoned()
            .retain(|(_, stored_controller_id), pressed| {
                if stored_controller_id == controller_id {
                    releases.extend(pressed.drain_releases());
                    false
                } else {
                    true
                }
            });
        releases
    }

    pub(crate) fn drain_all_pressed(&self) -> Vec<ReplayTask> {
        self.pressed_inputs
            .lock_unpoisoned()
            .drain()
            .flat_map(|(_, pressed)| pressed.into_releases())
            .collect()
    }
}

// App-owned engine (plan §3 3B/3C): a single process-global instance, not a
// per-share gadget. Room-generation scoping is enforced functionally by the
// lifecycle seams, not by swapping engine instances: leave/disconnect calls
// `revoke_all` (clears grants/sessions/pressed/epochs) and
// `invalidate_room_generation`; a transparent reconnect (same generation)
// preserves held input via `release_held_inputs_for_reconnect`; the per-room
// receiver loop exits as soon as `generation.is_current()` goes false. So a
// fresh room generation never observes a prior room's control state, while a
// same-generation reconnect keeps in-flight input. This is the deliberate
// design (the earlier SessionState Arc-engine scaffolding was dead code and
// was removed).
static ENGINE: LazyLock<RemoteControlEngine> = LazyLock::new(RemoteControlEngine::new);

pub(crate) fn remote_control_engine() -> &'static RemoteControlEngine {
    &ENGINE
}
/// LiveKit data-channel topic. Kept in lockstep with
/// `web-harness/src/trackNames.ts`.
pub const TOPIC: &str = "petal.remote-control";
pub(crate) const VERSION: u8 = 1;
pub(crate) const BINARY_MAGIC: u8 = 0x50;
/// #370 corrective pass: grew from 23 to 27 bytes to append a 4-byte
/// `token_fingerprint` (FNV-1a32 of the sender's live grant token). Closes
/// the bug where the original 23-byte hot-path frame had no room for grant
/// material at all, so `message_from_binary` hardcoded `grant_token: None`
/// and every binary packet fell into `is_authorized_input`'s former tokenless
/// compatibility path -- meant for old JSON clients, not this brand-new wire
/// variant. See `fnv1a32`, `binary_frame_for`, `message_from_binary`.
pub(crate) const BINARY_FRAME_LEN: usize = 27;

/// FNV-1a, 32-bit. Pure/stateless so it can be reimplemented identically in
/// TS (`web-harness/src/remoteControl.ts::fnv1a32`) -- keep both in lockstep;
/// a pinned test vector on both sides guards against silent divergence.
/// Standard constants: offset basis `0x811c9dc5`, prime `0x01000193`.
pub(crate) fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlType {
    Request,
    Release,
    Status,
    Pointer,
    Wheel,
    Key,
    Text,
    /// Additive v2 terminal outcome for a reliable discrete operation. Old
    /// peers ignore an unknown `kind`; it is never overloaded onto status.
    Result,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlAction {
    Move,
    Down,
    Up,
    /// A complete, non-dragging click. This is intentionally separate from
    /// down/up so the receiver can route a simple click atomically without
    /// creating held-input state.
    Click,
    #[serde(other)]
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlTargetKind {
    Window,
    Display,
    #[serde(other)]
    Unknown,
}

impl RemoteControlTargetKind {
    pub(crate) fn wire_code(self) -> u8 {
        match self {
            Self::Window => 1,
            Self::Display => 2,
            Self::Unknown => 255,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlCapability {
    LegacyControl,
    DiscretePointerV1,
    DiscreteScrollV1,
    WindowLocalPointer,
    GlobalKeyboard,
    UiaInvoke,
    UiaScroll,
    UnicodeText,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlReason {
    ControllerUpgradeRequired,
    /// Controller->host: the controller (or sharer) requests the stronger
    /// FullControl mode for a share. Host-side authority: the sharer approves
    /// before the mode flips; Petal never auto-escalates.
    RequestEscalation,
    /// Host->controller on a `denied` status: the sharer explicitly declined
    /// the control request (consent flow).
    ConsentDenied,
    /// Host->controller on a `denied` status: the sharer did not answer the
    /// consent prompt within [`crate::remote_control::CONSENT_TIMEOUT`]; a
    /// timeout never grants.
    ConsentTimedOut,
    #[serde(other)]
    Unknown,
}

/// The sharer's remote-control policy for the current meeting (host-side
/// authority, never on the wire). Seeded from the persisted Settings default
/// on every join and mutable for the current meeting only.
///
/// - `Off`: every request is refused with `disabled`.
/// - `Ask` (the default): a request is parked and the sharer is prompted
///   (Allow / Deny, 30 s timeout that resolves to deny). The grant token is
///   minted ONLY on an explicit Allow.
/// - `Auto`: the legacy behaviour -- an authenticated in-room requester is
///   granted immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlPolicy {
    Off,
    #[default]
    Ask,
    Auto,
}

impl RemoteControlPolicy {
    pub fn as_wire(self) -> &'static str {
        match self {
            RemoteControlPolicy::Off => "off",
            RemoteControlPolicy::Ask => "ask",
            RemoteControlPolicy::Auto => "auto",
        }
    }

    /// Parse a wire/settings string; unknown values map to the default
    /// (`Ask`) -- never to the more permissive `Auto`.
    pub fn from_wire(value: &str) -> RemoteControlPolicy {
        match value.trim() {
            "off" => RemoteControlPolicy::Off,
            "auto" => RemoteControlPolicy::Auto,
            _ => RemoteControlPolicy::Ask,
        }
    }

    /// Compact storage form for an `AtomicU8` session field.
    pub fn as_u8(self) -> u8 {
        match self {
            RemoteControlPolicy::Off => 0,
            RemoteControlPolicy::Ask => 1,
            RemoteControlPolicy::Auto => 2,
        }
    }

    pub fn from_u8(value: u8) -> RemoteControlPolicy {
        match value {
            0 => RemoteControlPolicy::Off,
            2 => RemoteControlPolicy::Auto,
            _ => RemoteControlPolicy::Ask,
        }
    }

    /// The legacy boolean view: does this policy accept requests at all?
    pub fn allows_requests(self) -> bool {
        self != RemoteControlPolicy::Off
    }

    /// The boolean->policy mapping used by the legacy `set_remote_control_allowed`
    /// command and the per-meeting pill: `false` is always `Off`; `true`
    /// restores the stored default, never the more permissive `Auto` unless
    /// that IS the default.
    pub fn from_allowed(allowed: bool, default: RemoteControlPolicy) -> RemoteControlPolicy {
        if !allowed {
            RemoteControlPolicy::Off
        } else if default == RemoteControlPolicy::Off {
            RemoteControlPolicy::Ask
        } else {
            default
        }
    }
}

/// The sharer-chosen per-share control policy on the HOST. This is host-side
/// authority only: it gates which delivery routes the host will use for a
/// given shared window/display. It is NOT a controller capability and never
/// changes the controller replay wire.
///
/// - [`RemoteControlMode::CursorPreserving`] (default): the base, light-touch
///   mode. Discrete gestures use the global-inject + cursor save/restore route
///   (the macOS `CursorTakeover` model); the wheel keeps its message route;
///   window-share keyboard uses a per-controller focus target. The sharer's
///   cursor is never left at the remote point.
/// - [`RemoteControlMode::FullControl`]: the stronger mode. The cursor stays
///   at the controller's point (global `SetCursorPos` + serialized `SendInput`)
///   for continuous pointer tracking, and the controller may be escalated into
///   it on request. Escalation is user-initiated; Petal never auto-escalates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlMode {
    /// Base/default mode (cursor-preserving, save/restore around real input).
    #[default]
    CursorPreserving,
    /// Stronger mode (the shipped global pointer route; cursor stays).
    FullControl,
    /// Additive value from a newer peer; never guessed as a real mode.
    #[serde(other)]
    Unknown,
}

impl RemoteControlMode {
    /// Parse an optional wire string ("cursorPreserving"/"fullControl") from
    /// the share-flow UI; unknown/None maps to the default cursor-preserving.
    pub fn from_wire_option(value: Option<&str>) -> RemoteControlMode {
        match value.map(str::trim) {
            Some("fullControl") | Some("full-control") => RemoteControlMode::FullControl,
            _ => RemoteControlMode::CursorPreserving,
        }
    }

    /// Parse a wire/serde string from share metadata; unknown defaults to
    /// cursor-preserving.
    pub fn from_wire(value: &str) -> RemoteControlMode {
        Self::from_wire_option(Some(value))
    }

    /// Map this mode onto the existing capability vocabulary, used to (a)
    /// sanity-check negotiation and (b) decide `UnsupportedRoute`/`NotInjectible`
    /// refusals when an operation falls outside the mode.
    ///
    /// `CursorPreserving` relies on the window-local/message-era capabilities
    /// plus the global keyboard route for parallel window-share keyboard;
    /// `FullControl` unlocks the continuous global pointer path.
    pub fn requires_capability_set(self) -> &'static [RemoteControlCapability] {
        match self {
            RemoteControlMode::CursorPreserving => &[
                RemoteControlCapability::WindowLocalPointer,
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::DiscreteScrollV1,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UiaInvoke,
                RemoteControlCapability::UiaScroll,
                RemoteControlCapability::UnicodeText,
            ],
            RemoteControlMode::FullControl => &[
                RemoteControlCapability::DiscretePointerV1,
                RemoteControlCapability::GlobalKeyboard,
                RemoteControlCapability::UnicodeText,
            ],
            RemoteControlMode::Unknown => &[],
        }
    }

    /// Whether a given operation type is within this mode's envelope. Used by
    /// the host's per-operation gate before any side effect.
    pub fn permits(self, logical_pointer_tracking: bool) -> bool {
        match self {
            // Continuous pointer tracking / hover-follow is full-control
            // semantics. Cursor-preserving handles discrete gestures only.
            RemoteControlMode::CursorPreserving => !logical_pointer_tracking,
            RemoteControlMode::FullControl => true,
            RemoteControlMode::Unknown => false,
        }
    }
}

/// Host-side escalation intent: a controller (or sharer) requesting the
/// stronger [`RemoteControlMode::FullControl`] for one share, and the sharer's
/// approve/deny outcome. Lives on the host; Petal never initiates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlEscalation {
    /// The controller asks the sharer for full control of this share.
    Request,
    /// The sharer approved; the per-share mode flips to FullControl.
    Approved,
    /// The sharer denied; the per-share mode stays CursorPreserving.
    Denied,
    /// Additive value from a newer peer; treated as a no-op request.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RemoteControlButton {
    Left,
    Middle,
    Right,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteControlModifiers {
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub meta: bool,
    #[serde(default)]
    pub shift: bool,
}

/// Tauri-command draft from a native viewer's compositor control overlay.
/// Native command handlers fill the authenticated routing fields and protocol
/// version; a webview cannot choose either identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteControlDraft {
    #[serde(rename = "kind")]
    pub message_type: RemoteControlType,
    #[serde(default)]
    pub action: Option<RemoteControlAction>,
    pub window_id: u32,
    #[serde(default)]
    pub target_owner_id: Option<String>,
    pub seq: u64,
    #[serde(default)]
    pub target_kind: Option<RemoteControlTargetKind>,
    #[serde(default)]
    pub share_instance_id: Option<String>,
    #[serde(default)]
    pub controller_capabilities: Vec<RemoteControlCapability>,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub button: Option<i16>,
    #[serde(default)]
    pub buttons: Option<u16>,
    #[serde(default)]
    pub click_count: Option<u32>,
    #[serde(default)]
    pub delta_x: Option<f64>,
    #[serde(default)]
    pub delta_y: Option<f64>,
    #[serde(default)]
    pub delta_mode: Option<u8>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub repeat: bool,
    #[serde(default)]
    pub location: Option<u8>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub grant_token: Option<String>,
    #[serde(default)]
    pub modifiers: RemoteControlModifiers,
}

/// Stable, privacy-safe host stage for a v2 discrete terminal result. This is
/// deliberately narrower than a diagnostic transcript: it carries no target
/// application detail, input contents, coordinates, or operating-system text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlDeliveryRoute {
    Admission,
    Resolve,
    Replay,
    /// Additive result metadata from a newer peer; retain the result instead
    /// of rejecting its known terminal outcome.
    #[serde(other)]
    Unknown,
}

/// Stable reason code for a failed v2 discrete terminal result. Newer peers
/// may add codes; receivers must treat an unknown optional code as metadata
/// they cannot display, not as a reason to drop the correlated result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlFailureCode {
    Unauthorized,
    AccessibilityDenied,
    GrantExpired,
    TargetOffScreen,
    TargetUnavailable,
    NotForeground,
    Occluded,
    IntegrityBlocked,
    SecureField,
    /// The op cannot be injected in the current mode (e.g. a continuous pointer
    /// tracking op under CursorPreserving, or an unconsumed best-effort key).
    /// Distinct from `UnsupportedRoute` so the controller can surface the
    /// user-initiated escalation affordance precisely.
    NotInjectible,
    UnsupportedRoute,
    StaleShareInstance,
    ResolveFailed,
    ReplayFailed,
    InjectionTimeout,
    Superseded,
    Malformed,
    AdmissionOverloaded,
    /// Additive result metadata from a newer peer; not emitted locally.
    #[serde(other)]
    Unknown,
}

/// Wire payload shared by native and the web harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteControlMessage {
    pub v: u8,
    #[serde(rename = "kind")]
    pub message_type: RemoteControlType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RemoteControlAction>,
    pub target_user_id: String,
    pub controller_id: String,
    pub window_id: u32,
    pub seq: u64,
    /// Missing means the legacy window target. Unknown future values are
    /// retained as metadata and never guessed as a display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<RemoteControlTargetKind>,
    /// Opaque identity of one live capture/publication instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_instance_id: Option<String>,
    /// Advertised by a controller on a request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controller_capabilities: Vec<RemoteControlCapability>,
    /// Advertised by a host on an accepted grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_capabilities: Vec<RemoteControlCapability>,
    /// Optional additive status metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RemoteControlReason>,
    /// Grant-bound v2 fields. They are optional so legacy v1 packets remain
    /// wire-compatible; native validates them before any v2 admission state
    /// is allocated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_fingerprint_version: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_route: Option<RemoteControlDeliveryRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<RemoteControlFailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_capability: Option<RemoteControlResultCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buttons: Option<u16>,
    /// #373: authoritative multi-click count for a pointer down/up/click
    /// (mirrors the DOM `PointerEvent`/`MouseEvent.detail`), so the host can
    /// synthesize a real double-click (click_state=2) instead of two
    /// independent single presses. Additive/optional -- old peers omit it and
    /// the host falls back to click_state=1 (unchanged prior behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_mode: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub repeat: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Capability for the currently active grant. Input packets echo this
    /// value; omission is retained on the wire so old packets deserialize, but
    /// acceptance is release-gated by `TOKENLESS_GRANT_COMPATIBILITY_ENABLED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_token: Option<String>,
    /// #370 corrective pass: advertised true ONLY on a host's `status:
    /// "active"` packet -- its mere presence is the capability signal a
    /// controller uses to decide whether it may switch pointer/wheel sends
    /// to the binary hot path for THIS (windowId, targetUserId) session.
    /// Absent/false (the default) means "use JSON," which is what an old,
    /// not-yet-upgraded host's status packet naturally does since it never
    /// sets this field at all -- no negotiation round-trip needed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_binary_hot_path: bool,
    #[serde(default)]
    pub modifiers: RemoteControlModifiers,
}
impl RemoteControlMessage {
    pub(crate) fn effective_target_kind(&self) -> RemoteControlTargetKind {
        self.target_kind.unwrap_or(RemoteControlTargetKind::Window)
    }

    pub(crate) fn has_capable_envelope(&self) -> bool {
        self.target_kind.is_some()
            || self.share_instance_id.is_some()
            || !self.controller_capabilities.is_empty()
            || !self.host_capabilities.is_empty()
    }
}

impl RemoteControlDraft {
    pub(crate) fn into_message(
        self,
        target_user_id: String,
        controller_id: String,
    ) -> RemoteControlMessage {
        RemoteControlMessage {
            v: VERSION,
            message_type: self.message_type,
            action: self.action,
            target_user_id,
            controller_id,
            window_id: self.window_id,
            seq: self.seq,
            target_kind: self.target_kind,
            share_instance_id: self.share_instance_id,
            controller_capabilities: self.controller_capabilities,
            host_capabilities: Vec::new(),
            reason: None,
            control_session_id: None,
            input_id: None,
            input_seq: None,
            operation_fingerprint_version: None,
            operation_fingerprint: None,
            outcome: None,
            delivery_route: None,
            failure_code: None,
            result_capability: None,
            x: self.x,
            y: self.y,
            button: self.button,
            buttons: self.buttons,
            click_count: self.click_count,
            delta_x: self.delta_x,
            delta_y: self.delta_y,
            delta_mode: self.delta_mode,
            key: self.key,
            code: self.code,
            repeat: self.repeat,
            location: self.location,
            text: self.text,
            status: None,
            message: None,
            grant_token: self.grant_token,
            supports_binary_hot_path: false,
            modifiers: self.modifiers,
        }
    }
}

/// Advertised only with an accepted v2 control grant. `retry_enabled` remains
/// false until the controller-side retry protocol is shipped and exercised.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteControlResultCapability {
    pub version: u8,
    pub retry_enabled: bool,
    pub retry_deadline_ms: u64,
    pub dedup_guarantee_window_ms: u64,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteControlStatus {
    pub window_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_identity: Option<String>,
    pub controller_id: String,
    pub status: &'static str,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_token: Option<String>,
    /// Additive status metadata (`consentDenied` / `consentTimedOut` on a
    /// `denied` status). Omitted from the wire and the Tauri event when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RemoteControlReason>,
}
#[derive(Debug, Clone)]
pub(crate) struct ReplayTask {
    pub(crate) message: RemoteControlMessage,
    pub(crate) frame: WindowFrame,
    pub(crate) target_pid: Option<i32>,
    pub(crate) replay_epoch: u64,
    pub(crate) synthetic_release: bool,
    /// Present only for v2 discrete input. It must survive queue hand-off so
    /// the replay worker, rather than enqueue time, owns the terminal result.
    pub(crate) admission: Option<DiscreteAdmission>,
    pub(crate) terminal_on_success: bool,
    pub(crate) result_sender: Option<TerminalResultSender>,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DiscreteAdmission {
    pub(crate) controller_id: String,
    pub(crate) window_id: u32,
    pub(crate) target_kind: Option<RemoteControlTargetKind>,
    pub(crate) share_instance_id: Option<String>,
    pub(crate) control_session_id: String,
    pub(crate) input_id: String,
    pub(crate) input_seq: u64,
    pub(crate) operation_fingerprint: String,
}

// Fingerprint grammar v1 is binary rather than JSON.  The leading byte is the
// grammar version, all integers/floats are little-endian, strings are u32
// length-prefixed UTF-8, and optional fields have a one-byte presence tag.
// Transport `seq` is intentionally absent: retries use a fresh transport seq.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DiscreteAdmissionKey {
    pub(crate) controller_id: String,
    pub(crate) window_id: u32,
    pub(crate) target_kind: Option<RemoteControlTargetKind>,
    pub(crate) share_instance_id: Option<String>,
    pub(crate) control_session_id: String,
    pub(crate) input_id: String,
    pub(crate) input_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AdmissionGrantKey {
    pub(crate) controller_id: String,
    pub(crate) window_id: u32,
    pub(crate) target_kind: Option<RemoteControlTargetKind>,
    pub(crate) share_instance_id: Option<String>,
    pub(crate) control_session_id: String,
}

impl From<&DiscreteAdmission> for AdmissionGrantKey {
    fn from(value: &DiscreteAdmission) -> Self {
        Self {
            controller_id: value.controller_id.clone(),
            window_id: value.window_id,
            target_kind: value.target_kind,
            share_instance_id: value.share_instance_id.clone(),
            control_session_id: value.control_session_id.clone(),
        }
    }
}

impl From<&DiscreteAdmission> for DiscreteAdmissionKey {
    fn from(value: &DiscreteAdmission) -> Self {
        Self {
            controller_id: value.controller_id.clone(),
            window_id: value.window_id,
            target_kind: value.target_kind,
            share_instance_id: value.share_instance_id.clone(),
            control_session_id: value.control_session_id.clone(),
            input_id: value.input_id.clone(),
            input_seq: value.input_seq,
        }
    }
}
#[derive(Clone)]
pub(crate) struct TerminalResultSender {
    pub(crate) publisher: Arc<RoomConnection>,
    pub(crate) local_identity: String,
}

impl std::fmt::Debug for TerminalResultSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalResultSender")
            .field("local_identity", &self.local_identity)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone)]
pub(crate) struct AdmissionEntry {
    pub(crate) operation_fingerprint: String,
    pub(crate) terminal_disposition: Option<TerminalDisposition>,
    pub(crate) admitted_at: Instant,
}

/// The full terminal result is cached with a discrete operation so duplicate
/// deliveries recover the exact same controller-visible disposition (#446).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalDisposition {
    pub(crate) outcome: &'static str,
    pub(crate) delivery_route: RemoteControlDeliveryRoute,
    pub(crate) failure_code: Option<RemoteControlFailureCode>,
}

impl TerminalDisposition {
    pub(crate) const fn success(
        outcome: &'static str,
        delivery_route: RemoteControlDeliveryRoute,
    ) -> Self {
        Self {
            outcome,
            delivery_route,
            failure_code: None,
        }
    }

    pub(crate) fn failure(
        outcome: &'static str,
        delivery_route: RemoteControlDeliveryRoute,
        failure_code: RemoteControlFailureCode,
    ) -> Self {
        Self {
            outcome,
            delivery_route,
            // A terminal disposition must never claim both success and a
            // failure. Keep this invariant at the source/cache boundary so
            // duplicate recovery cannot reintroduce a contradictory result.
            failure_code: if matches!(outcome, "applied" | "submitted") {
                None
            } else {
                Some(failure_code)
            },
        }
    }
}
pub(crate) fn unreliable_seq_stream(message: &RemoteControlMessage) -> Option<UnreliableSeqStream> {
    match (message.message_type, message.action) {
        // Plain hover moves are high-rate state updates and may be lossy.
        // Held-button moves are drag replay; they must stay ordered with the
        // reliable down/up packets or target apps can see down -> up before
        // any drag event and never create a selection.
        (RemoteControlType::Pointer, Some(RemoteControlAction::Move))
            if message.buttons.unwrap_or(0) == 0 =>
        {
            Some(UnreliableSeqStream::PointerMove)
        }
        (RemoteControlType::Wheel, _) if !has_complete_v2_operation_envelope(message) => {
            Some(UnreliableSeqStream::Wheel)
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
pub(crate) struct DiscreteAdmissionState {
    pub(crate) entries: HashMap<DiscreteAdmissionKey, AdmissionEntry>,
    pub(crate) overload_epoch: HashMap<AdmissionGrantKey, u64>,
    pub(crate) overload_until: HashMap<AdmissionGrantKey, Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionDecision {
    Admitted,
    InFlightDuplicate,
    CompletedDuplicate(TerminalDisposition),
    Malformed,
    Overloaded,
}

pub(crate) const DISCRETE_ADMISSION_CAPACITY: usize = 256;
pub(crate) const DISCRETE_OVERLOAD_WINDOW: Duration = Duration::from_millis(750);
pub(crate) const DISCRETE_IN_FLIGHT_TTL: Duration = Duration::from_secs(5);
pub(crate) const RESULT_RETRY_ENABLED: bool = false;
// Grant tokens first shipped in 0.7.5, the one-release tokenless compatibility
// window. The tokenless input path is disabled from 0.7.6 onward (#493).
pub(crate) const TOKENLESS_GRANT_COMPATIBILITY_ENABLED: bool = false;

fn is_canonical_operation_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn operation_fingerprint_matches(
    message: &RemoteControlMessage,
    admission: &DiscreteAdmission,
) -> bool {
    is_canonical_operation_fingerprint(&admission.operation_fingerprint)
        && canonical_operation_fingerprint(message, admission) == admission.operation_fingerprint
}

fn prune_discrete_admissions(state: &mut DiscreteAdmissionState, now: Instant) {
    state.entries.retain(|_, entry| {
        now.saturating_duration_since(entry.admitted_at)
            <= if entry.terminal_disposition.is_some() {
                DISCRETE_OVERLOAD_WINDOW
            } else {
                DISCRETE_IN_FLIGHT_TTL
            }
    });
    state.overload_until.retain(|_, until| now < *until);
}

pub(crate) fn v2_discrete_admission(message: &RemoteControlMessage) -> Option<DiscreteAdmission> {
    let eligible = matches!(
        (message.message_type, message.action),
        (RemoteControlType::Pointer, Some(RemoteControlAction::Click))
            | (RemoteControlType::Wheel, _)
            | (
                RemoteControlType::Key,
                Some(RemoteControlAction::Down | RemoteControlAction::Up)
            )
            | (RemoteControlType::Text, _)
    );
    if !eligible || message.operation_fingerprint_version != Some(1) {
        return None;
    }
    if message.has_capable_envelope()
        && (!matches!(
            message.target_kind,
            Some(RemoteControlTargetKind::Window | RemoteControlTargetKind::Display)
        ) || message
            .share_instance_id
            .as_deref()
            .is_none_or(str::is_empty))
    {
        return None;
    }
    let admission = DiscreteAdmission {
        controller_id: message.controller_id.clone(),
        window_id: message.window_id,
        target_kind: message.target_kind,
        share_instance_id: message.share_instance_id.clone(),
        control_session_id: message.control_session_id.clone()?,
        input_id: message.input_id.clone()?,
        input_seq: message.input_seq?,
        operation_fingerprint: message.operation_fingerprint.clone()?,
    };
    Some(admission)
}

/// Once a sender includes any v2 admission field, a partial or malformed v2
/// envelope must never fall back to legacy replay. That downgrade would turn a
/// rejected exactly-once operation into an untracked side effect.
pub(crate) fn has_complete_v2_operation_envelope(message: &RemoteControlMessage) -> bool {
    message
        .control_session_id
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        && message
            .input_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && message.input_seq.is_some()
        && message.operation_fingerprint_version == Some(1)
        && message
            .operation_fingerprint
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && matches!(
            message.target_kind,
            Some(RemoteControlTargetKind::Window | RemoteControlTargetKind::Display)
        )
        && message
            .share_instance_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
}

pub(crate) fn is_v2_discrete_attempt(message: &RemoteControlMessage) -> bool {
    message.control_session_id.is_some()
        || message.input_id.is_some()
        || message.input_seq.is_some()
        || message.operation_fingerprint_version.is_some()
        || message.operation_fingerprint.is_some()
        || message.has_capable_envelope()
}
pub(crate) fn canonical_operation_fingerprint(
    message: &RemoteControlMessage,
    admission: &DiscreteAdmission,
) -> String {
    fn kind_code(kind: RemoteControlType) -> u8 {
        match kind {
            RemoteControlType::Request => 1,
            RemoteControlType::Release => 2,
            RemoteControlType::Status => 3,
            RemoteControlType::Pointer => 4,
            RemoteControlType::Wheel => 5,
            RemoteControlType::Key => 6,
            RemoteControlType::Text => 7,
            RemoteControlType::Result => 8,
            RemoteControlType::Unknown => 255,
        }
    }
    fn action_code(action: Option<RemoteControlAction>) -> u8 {
        match action {
            Some(RemoteControlAction::Move) => 1,
            Some(RemoteControlAction::Down) => 2,
            Some(RemoteControlAction::Up) => 3,
            Some(RemoteControlAction::Click) => 4,
            Some(RemoteControlAction::Unknown) => 255,
            None => 0,
        }
    }
    fn string(bytes: &mut Vec<u8>, value: &str) {
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    fn optional_string(bytes: &mut Vec<u8>, value: Option<&str>) {
        bytes.push(u8::from(value.is_some()));
        if let Some(value) = value {
            string(bytes, value);
        }
    }
    fn optional_f64(bytes: &mut Vec<u8>, value: Option<f64>) {
        bytes.push(u8::from(value.is_some()));
        if let Some(value) = value {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn optional_i16(bytes: &mut Vec<u8>, value: Option<i16>) {
        bytes.push(u8::from(value.is_some()));
        if let Some(value) = value {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn optional_u16(bytes: &mut Vec<u8>, value: Option<u16>) {
        bytes.push(u8::from(value.is_some()));
        if let Some(value) = value {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn optional_u8(bytes: &mut Vec<u8>, value: Option<u8>) {
        bytes.push(u8::from(value.is_some()));
        if let Some(value) = value {
            bytes.push(value);
        }
    }
    let mut canonical = vec![
        1,
        message.v,
        kind_code(message.message_type),
        action_code(message.action),
    ];
    string(&mut canonical, &message.target_user_id);
    string(&mut canonical, &message.controller_id);
    canonical.extend_from_slice(&message.window_id.to_le_bytes());
    string(&mut canonical, &admission.control_session_id);
    string(&mut canonical, &admission.input_id);
    canonical.extend_from_slice(&admission.input_seq.to_le_bytes());
    optional_f64(&mut canonical, message.x);
    optional_f64(&mut canonical, message.y);
    optional_i16(&mut canonical, message.button);
    optional_u16(&mut canonical, message.buttons);
    optional_string(&mut canonical, message.key.as_deref());
    optional_string(&mut canonical, message.code.as_deref());
    canonical.push(u8::from(message.repeat));
    optional_u8(&mut canonical, message.location);
    optional_string(&mut canonical, message.text.as_deref());
    canonical.extend_from_slice(&[
        u8::from(message.modifiers.alt),
        u8::from(message.modifiers.ctrl),
        u8::from(message.modifiers.meta),
        u8::from(message.modifiers.shift),
    ]);
    if message.message_type == RemoteControlType::Wheel {
        canonical.push(3);
        optional_f64(&mut canonical, message.delta_x);
        optional_f64(&mut canonical, message.delta_y);
        optional_u8(&mut canonical, message.delta_mode);
    }
    // Preserve the existing v1 fingerprint bytes exactly for legacy peers.
    // Capable envelopes append a tagged suffix that binds target kind and the
    // live share instance without changing old vectors.
    if admission.target_kind.is_some() || admission.share_instance_id.is_some() {
        canonical.push(2);
        canonical.push(
            admission
                .target_kind
                .unwrap_or(RemoteControlTargetKind::Window)
                .wire_code(),
        );
        optional_string(&mut canonical, admission.share_instance_id.as_deref());
    }
    format!("{:x}", Sha256::digest(canonical))
}

fn fixed_point(value: f64) -> u16 {
    (value.clamp(0.0, 1.0) * u16::MAX as f64).round() as u16
}

pub(crate) fn binary_frame_for(message: &RemoteControlMessage) -> Option<Vec<u8>> {
    if is_v2_discrete_attempt(message) {
        return None;
    }
    let binary_kind = match (message.message_type, message.action) {
        (RemoteControlType::Pointer, Some(RemoteControlAction::Move)) => 4,
        (RemoteControlType::Wheel, None) => 5,
        _ => return None,
    };
    let (x, y) = message.x.zip(message.y)?;
    // #370 corrective pass: a hot-path frame with no grant token to
    // fingerprint can never pass the receiver's freshness check, so refuse
    // to encode one at all -- callers (`publish_message`) fall back to JSON,
    // which still carries `grantToken` as a real field.
    let token_fingerprint = fnv1a32(message.grant_token.as_deref()?.as_bytes());
    let mut bytes = Vec::with_capacity(BINARY_FRAME_LEN);
    bytes.extend_from_slice(&[
        BINARY_MAGIC,
        VERSION,
        binary_kind,
        if binary_kind == 4 { 1 } else { 0 },
    ]);
    bytes.extend_from_slice(&(message.seq as u32).to_le_bytes());
    bytes.extend_from_slice(&message.window_id.to_le_bytes());
    bytes.extend_from_slice(&fixed_point(x).to_le_bytes());
    bytes.extend_from_slice(&fixed_point(y).to_le_bytes());
    bytes.push(message.buttons.unwrap_or(0).min(u8::MAX as u16) as u8);
    let modifiers = (message.modifiers.alt as u8)
        | ((message.modifiers.ctrl as u8) << 1)
        | ((message.modifiers.meta as u8) << 2)
        | ((message.modifiers.shift as u8) << 3);
    bytes.push(modifiers);
    bytes.extend_from_slice(
        &(message
            .delta_x
            .unwrap_or(0.0)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &(message
            .delta_y
            .unwrap_or(0.0)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16)
            .to_le_bytes(),
    );
    bytes.push(message.delta_mode.unwrap_or(0));
    bytes.extend_from_slice(&token_fingerprint.to_le_bytes());
    Some(bytes)
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct KeyIdentity {
    pub(crate) code: Option<String>,
    pub(crate) key: Option<String>,
    pub(crate) location: Option<u8>,
}

#[derive(Debug)]
pub(crate) struct HeldInput {
    pub(crate) release: ReplayTask,
}

#[derive(Debug)]
pub(crate) struct PressedInputs {
    pub(crate) buttons: HashMap<RemoteControlButton, HeldInput>,
    pub(crate) keys: HashMap<KeyIdentity, HeldInput>,
    pub(crate) last_activity_at: Instant,
}

impl PressedInputs {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            buttons: HashMap::new(),
            keys: HashMap::new(),
            last_activity_at: now,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buttons.is_empty() && self.keys.is_empty()
    }

    pub(crate) fn into_releases(self) -> impl Iterator<Item = ReplayTask> {
        self.buttons
            .into_values()
            .map(|held| held.release)
            .chain(self.keys.into_values().map(|held| held.release))
    }

    pub(crate) fn drain_releases(&mut self) -> Vec<ReplayTask> {
        self.buttons
            .drain()
            .map(|(_, held)| held.release)
            .chain(self.keys.drain().map(|(_, held)| held.release))
            .collect()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UnreliableSeqStream {
    PointerMove,
    Wheel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnreliableSeqDecision {
    Accepted,
    AcceptedRestart {
        stream: UnreliableSeqStream,
        previous: u64,
    },
    Rejected {
        stream: UnreliableSeqStream,
        last_seen: u64,
    },
}

pub(crate) const CONTROLLER_RESTART_WATERMARK_MIN: u64 = 1_000;
pub(crate) const CONTROLLER_RESTART_SEQ_MAX: u64 = 2;

fn replay_epoch(window_id: u32, controller_id: &str) -> u64 {
    remote_control_engine().replay_epoch(window_id, controller_id)
}

fn pressed_inputs() -> &'static Mutex<HashMap<(u32, String), PressedInputs>> {
    remote_control_engine().pressed_inputs()
}

pub(crate) const HELD_INPUT_TTL: Duration = Duration::from_millis(1_200);
pub(crate) fn button_from_wire(button: Option<i16>) -> RemoteControlButton {
    match button {
        Some(1) => RemoteControlButton::Middle,
        Some(2) => RemoteControlButton::Right,
        _ => RemoteControlButton::Left,
    }
}

fn key_identity(message: &RemoteControlMessage) -> KeyIdentity {
    KeyIdentity {
        code: message.code.clone(),
        key: message.key.clone(),
        location: message.location,
    }
}

pub(crate) fn replay_task(
    message: RemoteControlMessage,
    frame: WindowFrame,
    target_pid: Option<i32>,
    synthetic_release: bool,
) -> ReplayTask {
    let replay_epoch = replay_epoch(message.window_id, &message.controller_id);
    let admission = v2_discrete_admission(&message);
    ReplayTask {
        message,
        frame,
        target_pid,
        replay_epoch,
        synthetic_release,
        admission,
        terminal_on_success: true,
        result_sender: None,
    }
}

pub(crate) fn release_pointer_task(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    target_pid: Option<i32>,
) -> ReplayTask {
    let mut release = message.clone();
    release.action = Some(RemoteControlAction::Up);
    release.buttons = Some(0);
    replay_task(release, frame, target_pid, true)
}

fn release_key_task(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    target_pid: Option<i32>,
) -> ReplayTask {
    let mut release = message.clone();
    release.action = Some(RemoteControlAction::Up);
    release.repeat = false;
    replay_task(release, frame, target_pid, true)
}

fn button_mask(button: RemoteControlButton) -> u16 {
    match button {
        RemoteControlButton::Left => 1,
        RemoteControlButton::Right => 2,
        RemoteControlButton::Middle => 4,
    }
}

fn drain_buttons_not_in_mask(pressed: &mut PressedInputs, buttons: u16) -> Vec<ReplayTask> {
    let stale_buttons = pressed
        .buttons
        .keys()
        .copied()
        .filter(|button| buttons & button_mask(*button) == 0)
        .collect::<Vec<_>>();
    stale_buttons
        .into_iter()
        .filter_map(|button| pressed.buttons.remove(&button).map(|held| held.release))
        .collect()
}

fn modifier_key_family(value: &str) -> Option<&'static str> {
    match value {
        "Shift" | "ShiftLeft" | "ShiftRight" => Some("Shift"),
        "Alt" | "Option" | "AltLeft" | "AltRight" => Some("Alt"),
        "Control" | "Ctrl" | "ControlLeft" | "ControlRight" => Some("Control"),
        "Meta" | "OS" | "Command" | "Super" | "MetaLeft" | "MetaRight" => Some("Meta"),
        _ => None,
    }
}

fn key_identity_matches_release(held: &KeyIdentity, release: &KeyIdentity) -> bool {
    if held == release {
        return true;
    }
    if held.code.is_some() && held.code == release.code {
        return true;
    }
    if held.key.is_some() && held.key == release.key {
        return true;
    }
    let held_family = held
        .code
        .as_deref()
        .and_then(modifier_key_family)
        .or_else(|| held.key.as_deref().and_then(modifier_key_family));
    let release_family = release
        .code
        .as_deref()
        .and_then(modifier_key_family)
        .or_else(|| release.key.as_deref().and_then(modifier_key_family));
    held_family.is_some() && held_family == release_family
}

fn matching_key_release_identity(
    keys: &HashMap<KeyIdentity, HeldInput>,
    release: &KeyIdentity,
) -> Option<KeyIdentity> {
    if keys.contains_key(release) {
        return Some(release.clone());
    }
    keys.keys()
        .find(|held| key_identity_matches_release(held, release))
        .cloned()
}

pub(crate) fn track_pressed_input(
    message: &RemoteControlMessage,
    frame: WindowFrame,
    target_pid: Option<i32>,
) -> Vec<ReplayTask> {
    let key = (message.window_id, message.controller_id.clone());
    let now = Instant::now();
    let mut pressed_by_controller = pressed_inputs().lock_unpoisoned();
    if let Some(pressed) = pressed_by_controller.get_mut(&key) {
        pressed.last_activity_at = now;
    }
    let mut synthetic_releases = Vec::new();
    match (message.message_type, message.action) {
        (RemoteControlType::Pointer, Some(RemoteControlAction::Down)) => {
            let button = button_from_wire(message.button);
            pressed_by_controller
                .entry(key)
                .or_insert_with(|| PressedInputs::new(now))
                .buttons
                .insert(
                    button,
                    HeldInput {
                        release: release_pointer_task(message, frame, target_pid),
                    },
                );
        }
        (RemoteControlType::Pointer, Some(RemoteControlAction::Move)) => {
            if let (Some(buttons), Some(pressed)) =
                (message.buttons, pressed_by_controller.get_mut(&key))
            {
                synthetic_releases.extend(drain_buttons_not_in_mask(pressed, buttons));
                if pressed.is_empty() {
                    pressed_by_controller.remove(&key);
                }
            }
        }
        (RemoteControlType::Pointer, Some(RemoteControlAction::Up)) => {
            let should_remove = if let Some(pressed) = pressed_by_controller.get_mut(&key) {
                let button = button_from_wire(message.button);
                if pressed.buttons.remove(&button).is_none() {
                    if let Some(buttons) = message.buttons {
                        synthetic_releases.extend(drain_buttons_not_in_mask(pressed, buttons));
                    } else {
                        synthetic_releases.extend(
                            pressed
                                .buttons
                                .drain()
                                .map(|(_, held)| held.release)
                                .collect::<Vec<_>>(),
                        );
                    }
                }
                pressed.is_empty()
            } else {
                false
            };
            if should_remove {
                pressed_by_controller.remove(&key);
            }
        }
        (RemoteControlType::Key, Some(RemoteControlAction::Down)) if !message.repeat => {
            pressed_by_controller
                .entry(key)
                .or_insert_with(|| PressedInputs::new(now))
                .keys
                .insert(
                    key_identity(message),
                    HeldInput {
                        release: release_key_task(message, frame, target_pid),
                    },
                );
        }
        (RemoteControlType::Key, Some(RemoteControlAction::Up)) => {
            let should_remove = if let Some(pressed) = pressed_by_controller.get_mut(&key) {
                let release_identity = key_identity(message);
                if let Some(matched_identity) =
                    matching_key_release_identity(&pressed.keys, &release_identity)
                {
                    if let Some(held) = pressed.keys.remove(&matched_identity) {
                        if matched_identity != release_identity {
                            synthetic_releases.push(held.release);
                        }
                    }
                }
                pressed.is_empty()
            } else {
                false
            };
            if should_remove {
                pressed_by_controller.remove(&key);
            }
        }
        _ => {}
    }
    synthetic_releases
}

pub(crate) fn drain_pressed_for_controller(window_id: u32, controller_id: &str) -> Vec<ReplayTask> {
    pressed_inputs()
        .lock_unpoisoned()
        .remove(&(window_id, controller_id.to_string()))
        .map(|pressed| pressed.into_releases().collect())
        .unwrap_or_default()
}

pub(crate) fn drain_pressed_for_window(window_id: u32) -> Vec<ReplayTask> {
    let mut releases = Vec::new();
    pressed_inputs()
        .lock_unpoisoned()
        .retain(|(stored_window_id, _), pressed| {
            if *stored_window_id == window_id {
                releases.extend(pressed.drain_releases());
                false
            } else {
                true
            }
        });
    releases
}

pub(crate) fn drain_pressed_for_controller_id(controller_id: &str) -> Vec<ReplayTask> {
    let mut releases = Vec::new();
    pressed_inputs()
        .lock_unpoisoned()
        .retain(|(_, stored_controller_id), pressed| {
            if stored_controller_id == controller_id {
                releases.extend(pressed.drain_releases());
                false
            } else {
                true
            }
        });
    releases
}

pub(crate) fn drain_all_pressed() -> Vec<ReplayTask> {
    pressed_inputs()
        .lock_unpoisoned()
        .drain()
        .flat_map(|(_, pressed)| pressed.into_releases())
        .collect()
}

pub(crate) fn drain_expired_pressed(now: Instant) -> Vec<ReplayTask> {
    let mut releases = Vec::new();
    pressed_inputs()
        .lock_unpoisoned()
        .retain(|(window_id, controller_id), pressed| {
            if !pressed.is_empty()
                && now.saturating_duration_since(pressed.last_activity_at) >= HELD_INPUT_TTL
            {
                log::warn!(
                    "remote-control: held input expired for window {} controller='{}' after {:?}",
                    window_id,
                    controller_id,
                    now.saturating_duration_since(pressed.last_activity_at)
                );
                releases.extend(pressed.drain_releases());
                false
            } else {
                true
            }
        });
    releases
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_mode_defaults_and_wire_parsing() {
        // Default is cursor-preserving.
        assert_eq!(RemoteControlMode::default(), RemoteControlMode::CursorPreserving);
        assert_eq!(RemoteControlMode::from_wire_option(None), RemoteControlMode::CursorPreserving);
        assert_eq!(RemoteControlMode::from_wire("cursorPreserving"), RemoteControlMode::CursorPreserving);
        assert_eq!(RemoteControlMode::from_wire("fullControl"), RemoteControlMode::FullControl);
        // Unknown/None never guesses a stronger mode.
        assert_eq!(RemoteControlMode::from_wire("banana"), RemoteControlMode::CursorPreserving);
        assert_eq!(RemoteControlMode::from_wire(""), RemoteControlMode::CursorPreserving);
        assert!(matches!(
            RemoteControlMode::from_wire("full-control"),
            RemoteControlMode::FullControl
        ));
    }

    #[test]
    fn control_mode_permits_pointer_tracking_only_in_full_control() {
        // Continuous pointer tracking (hover-follow) is full-control semantics.
        assert!(!RemoteControlMode::CursorPreserving.permits(true));
        assert!(RemoteControlMode::CursorPreserving.permits(false));
        assert!(RemoteControlMode::FullControl.permits(true));
        assert!(RemoteControlMode::FullControl.permits(false));
        assert!(!RemoteControlMode::Unknown.permits(false));
    }

    #[test]
    fn control_mode_serde_roundtrips_and_is_additive() {
        // Wire-safe: unknown additive values never guess a real mode.
        let preserved: RemoteControlMode = serde_json::from_str("\"cursorPreserving\"").unwrap();
        assert_eq!(preserved, RemoteControlMode::CursorPreserving);
        let full: RemoteControlMode = serde_json::from_str("\"fullControl\"").unwrap();
        assert_eq!(full, RemoteControlMode::FullControl);
        let unknown: RemoteControlMode = serde_json::from_str("\"futureMode\"").unwrap();
        assert!(matches!(unknown, RemoteControlMode::Unknown));
        assert!(!RemoteControlMode::Unknown.permits(false));
    }

    #[derive(Default)]
    struct FakePlatform {
        replayed: Mutex<Vec<(u32, Option<i32>)>>,
        cleared_windows: Mutex<Vec<u32>>,
        cleared_controllers: Mutex<Vec<(u32, String)>>,
    }

    impl PlatformControl for FakePlatform {
        fn accessibility_trusted(&self) -> bool {
            true
        }

        fn prompt_accessibility(&self) -> bool {
            true
        }

        fn replay(
            &self,
            message: &RemoteControlMessage,
            _frame: WindowFrame,
            target_pid: Option<i32>,
        ) -> Result<(), String> {
            self.replayed
                .lock_unpoisoned()
                .push((message.window_id, target_pid));
            Ok(())
        }

        fn clear_cached_app(&self, _pid: i32) {}

        fn clear_resolution_cache(&self, window_id: u32) {
            self.cleared_windows.lock_unpoisoned().push(window_id);
        }

        fn clear_window_gestures(&self, _window_id: u32) {}

        fn clear_all_control_state(&self) {}

        fn clear_controller_gestures(&self, window_id: u32, controller_id: &str) {
            self.cleared_controllers
                .lock_unpoisoned()
                .push((window_id, controller_id.to_string()));
        }

        fn release_window_gestures(&self, _window_id: u32) {}
    }

    #[derive(Default)]
    struct FakeSurface(Mutex<Vec<RemoteControlStatus>>);

    impl ControlSurface for FakeSurface {
        fn emit_status(&self, status: RemoteControlStatus) {
            self.0.lock_unpoisoned().push(status);
        }
    }

    fn message(json: &str) -> RemoteControlMessage {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn legacy_json_shape_is_unchanged_when_capability_fields_are_absent() {
        let message = message(
            r#"{"v":1,"kind":"request","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":6}"#,
        );
        let encoded = serde_json::to_value(message).unwrap();
        assert_eq!(encoded["kind"], "request");
        assert_eq!(encoded["windowId"], 42);
        for additive in [
            "targetKind",
            "shareInstanceId",
            "controllerCapabilities",
            "hostCapabilities",
            "reason",
        ] {
            assert!(encoded.get(additive).is_none(), "{additive}");
        }
    }

    #[test]
    fn unknown_optional_capability_metadata_never_changes_a_grant() {
        let engine = RemoteControlEngine::new();
        let legacy_key = ControlGrantKey::legacy(42, "web-1");
        engine.install_grant(legacy_key.clone(), "grant-a".to_string());
        let future = message(
            r#"{"v":1,"kind":"request","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":7,"targetKind":"future-target","controllerCapabilities":["legacyControl","future-capability"],"reason":"future-reason"}"#,
        );
        assert_eq!(future.target_kind, Some(RemoteControlTargetKind::Unknown));
        assert_eq!(
            future.controller_capabilities,
            [
                RemoteControlCapability::LegacyControl,
                RemoteControlCapability::Unknown,
            ]
        );
        assert_eq!(future.reason, Some(RemoteControlReason::Unknown));
        assert!(ControlGrantKey::for_message(&future).is_none());
        assert_eq!(
            engine.active_grant_token(&legacy_key).as_deref(),
            Some("grant-a")
        );
    }

    #[test]
    fn grant_identity_binds_target_kind_and_share_instance() {
        let engine = RemoteControlEngine::new();
        let key = ControlGrantKey {
            window_id: 42,
            controller_id: "web-1".to_string(),
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-a".to_string()),
        };
        engine.install_grant(key.clone(), "grant-a".to_string());

        let matching = DiscreteAdmission {
            controller_id: "web-1".to_string(),
            window_id: 42,
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-a".to_string()),
            control_session_id: "grant-a".to_string(),
            input_id: "input-a".to_string(),
            input_seq: 1,

            operation_fingerprint: "fingerprint".to_string(),
        };
        assert!(engine.grant_is_current(&matching));

        let mut stale_share = matching.clone();
        stale_share.share_instance_id = Some("share-b".to_string());
        assert!(!engine.grant_is_current(&stale_share));

        let mut wrong_kind = matching.clone();
        wrong_kind.target_kind = Some(RemoteControlTargetKind::Display);
        assert!(!engine.grant_is_current(&wrong_kind));

        let mut partial = matching.clone();
        partial.share_instance_id = None;
        assert!(!engine.grant_is_current(&partial));
        assert!(engine.revoke_grant(&key));
        assert!(!engine.grant_is_current(&matching));
    }
    #[test]
    fn discrete_admission_deduplicates_completes_and_expires() {
        let engine = RemoteControlEngine::new();
        let mut message = message(
            r#"{"v":1,"kind":"pointer","action":"click","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":1,"targetKind":"window","shareInstanceId":"share-a","x":0.5,"y":0.5,"button":0,"buttons":0,"operationFingerprintVersion":1}"#,
        );
        let mut admission = DiscreteAdmission {
            controller_id: "web-1".to_string(),
            window_id: 42,
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-a".to_string()),
            control_session_id: "grant-a".to_string(),
            input_id: "input-a".to_string(),
            input_seq: 1,
            operation_fingerprint: String::new(),
        };
        admission.operation_fingerprint = canonical_operation_fingerprint(&message, &admission);
        message.operation_fingerprint = Some(admission.operation_fingerprint.clone());

        let now = Instant::now();
        assert_eq!(
            engine.admit_discrete_operation(&message, &admission, now),
            AdmissionDecision::Admitted
        );
        assert_eq!(
            engine.admit_discrete_operation(&message, &admission, now),
            AdmissionDecision::InFlightDuplicate
        );
        let submitted =
            TerminalDisposition::success("submitted", RemoteControlDeliveryRoute::Replay);
        assert!(engine.complete_discrete_operation(&admission, submitted));
        assert_eq!(
            engine.admit_discrete_operation(&message, &admission, now),
            AdmissionDecision::CompletedDuplicate(submitted)
        );
        assert!(!engine.complete_discrete_operation(&admission, submitted));

        let mut expiring_message = message.clone();
        let mut expiring = admission.clone();
        expiring.input_id = "input-b".to_string();
        expiring.input_seq = 2;
        expiring.operation_fingerprint =
            canonical_operation_fingerprint(&expiring_message, &expiring);
        expiring_message.input_id = Some(expiring.input_id.clone());
        expiring_message.input_seq = Some(expiring.input_seq);
        expiring_message.operation_fingerprint = Some(expiring.operation_fingerprint.clone());
        assert_eq!(
            engine.admit_discrete_operation(&expiring_message, &expiring, now),
            AdmissionDecision::Admitted
        );
        engine
            .discrete_admissions
            .lock_unpoisoned()
            .entries
            .get_mut(&DiscreteAdmissionKey::from(&expiring))
            .unwrap()
            .admitted_at = now - DISCRETE_IN_FLIGHT_TTL - Duration::from_millis(1);
        assert!(!engine.admission_is_still_inflight(&expiring, now));
    }

    #[test]
    fn authenticated_sender_binding_overrides_spoofable_controller_id() {
        let engine = RemoteControlEngine::new();
        let packet = message(
            r#"{"v":1,"kind":"request","targetUserId":"native-1","controllerId":"victim","windowId":42,"seq":1}"#,
        );
        assert!(engine.bind_trusted_sender(None, packet.clone()).is_none());
        let bound = engine
            .bind_trusted_sender(Some("attacker".to_string()), packet)
            .unwrap();
        assert_eq!(bound.controller_id, "attacker");
    }

    #[test]
    fn bounded_queue_prioritizes_discrete_and_coalesces_high_rate_work() {
        let queue = BoundedCoalescingQueue::new(1);
        assert!(matches!(
            queue.push(1, Some("pointer")),
            BoundedQueuePush::Enqueued
        ));
        assert!(matches!(
            queue.push(2, Some("pointer")),
            BoundedQueuePush::Coalesced
        ));
        assert!(matches!(
            queue.push(3, Some("wheel")),
            BoundedQueuePush::Dropped(3)
        ));
        assert!(matches!(queue.push(4, None), BoundedQueuePush::Enqueued));
        assert_eq!(queue.pop(), 4);
        assert_eq!(queue.pop(), 2);
    }

    #[test]
    fn portable_packet_builder_pins_reliability_and_targeting() {
        let engine = RemoteControlEngine::new();
        let hover = message(
            r#"{"v":1,"kind":"pointer","action":"move","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":1,"x":0.5,"y":0.5,"buttons":0}"#,
        );
        let hover_packet = engine.data_packet_for(&hover);
        assert_eq!(hover_packet.topic.as_deref(), Some(TOPIC));
        assert!(!hover_packet.reliable);
        assert_eq!(hover_packet.destination_identities.len(), 1);

        let click = message(
            r#"{"v":1,"kind":"pointer","action":"click","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":2,"x":0.5,"y":0.5,"button":0,"buttons":0}"#,
        );
        assert!(engine.data_packet_for(&click).reliable);

        let result = message(
            r#"{"v":1,"kind":"result","targetUserId":"web-1","controllerId":"native-1","windowId":42,"seq":3,"outcome":"submitted"}"#,
        );
        assert!(engine.data_packet_for(&result).reliable);
    }

    #[test]
    fn unreliable_sequence_rejection_is_per_stream_and_restart_aware() {
        let engine = RemoteControlEngine::new();
        let mut hover = message(
            r#"{"v":1,"kind":"pointer","action":"move","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":10,"x":0.5,"y":0.5,"buttons":0}"#,
        );
        assert_eq!(
            engine.accept_unreliable_seq(&hover),
            UnreliableSeqDecision::Accepted
        );
        hover.seq = 9;
        assert_eq!(
            engine.accept_unreliable_seq(&hover),
            UnreliableSeqDecision::Rejected {
                stream: UnreliableSeqStream::PointerMove,
                last_seen: 10,
            }
        );
        hover.seq = CONTROLLER_RESTART_WATERMARK_MIN + 1;
        assert_eq!(
            engine.accept_unreliable_seq(&hover),
            UnreliableSeqDecision::Accepted
        );
        hover.seq = 1;
        assert_eq!(
            engine.accept_unreliable_seq(&hover),
            UnreliableSeqDecision::AcceptedRestart {
                stream: UnreliableSeqStream::PointerMove,
                previous: CONTROLLER_RESTART_WATERMARK_MIN + 1,
            }
        );
        engine.reset_unreliable_seq(42, "web-1");
        hover.seq = 0;
        assert_eq!(
            engine.accept_unreliable_seq(&hover),
            UnreliableSeqDecision::Accepted
        );
    }

    #[test]
    fn held_input_ownership_releases_on_revoke_and_expiry() {
        let frame = WindowFrame {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let down = message(
            r#"{"v":1,"kind":"pointer","action":"down","targetUserId":"native-1","controllerId":"portable-held-test","windowId":777,"seq":1,"x":0.5,"y":0.5,"button":0,"buttons":1}"#,
        );
        assert!(track_pressed_input(&down, frame, Some(99)).is_empty());
        let releases = drain_pressed_for_controller(777, "portable-held-test");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].message.action, Some(RemoteControlAction::Up));
        assert_eq!(releases[0].message.buttons, Some(0));

        let key_down = message(
            r#"{"v":1,"kind":"key","action":"down","targetUserId":"native-1","controllerId":"portable-held-test","windowId":777,"seq":2,"key":"Shift","code":"ShiftLeft"}"#,
        );
        track_pressed_input(&key_down, frame, Some(99));
        pressed_inputs()
            .lock_unpoisoned()
            .get_mut(&(777, "portable-held-test".to_string()))
            .unwrap()
            .last_activity_at = Instant::now() - HELD_INPUT_TTL - Duration::from_millis(1);
        let expired = drain_expired_pressed(Instant::now());
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].message.action, Some(RemoteControlAction::Up));
        assert_eq!(expired[0].message.code.as_deref(), Some("ShiftLeft"));
    }

    #[test]
    fn repeated_keydown_does_not_duplicate_the_held_release() {
        let frame = WindowFrame {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let down = message(
            r#"{"v":1,"kind":"key","action":"down","targetUserId":"native-1","controllerId":"repeat-held-test","windowId":778,"seq":1,"key":"a","code":"KeyA","location":0}"#,
        );
        let repeat = message(
            r#"{"v":1,"kind":"key","action":"down","targetUserId":"native-1","controllerId":"repeat-held-test","windowId":778,"seq":2,"key":"a","code":"KeyA","location":0,"repeat":true}"#,
        );
        track_pressed_input(&down, frame, Some(99));
        track_pressed_input(&repeat, frame, Some(99));
        let releases = drain_pressed_for_controller(778, "repeat-held-test");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].message.code.as_deref(), Some("KeyA"));
        assert_eq!(releases[0].message.location, Some(0));
        assert!(!releases[0].message.repeat);
    }

    #[test]
    fn capable_wheel_never_downgrades_to_the_legacy_binary_frame() {
        let wheel = message(
            r#"{"v":2,"kind":"wheel","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":18,"targetKind":"window","shareInstanceId":"share-42","controlSessionId":"session-42","inputId":"wheel-18","inputSeq":18,"operationFingerprintVersion":1,"operationFingerprint":"fingerprint-18","grantToken":"0123456789abcdef0123456789abcdef","x":0.5,"y":0.25,"deltaX":0,"deltaY":40,"deltaMode":0}"#,
        );
        assert!(has_complete_v2_operation_envelope(&wheel));
        assert!(binary_frame_for(&wheel).is_none());
        assert!(unreliable_seq_stream(&wheel).is_none());
        let mut partial = wheel.clone();
        partial.operation_fingerprint = None;
        assert!(is_v2_discrete_attempt(&partial));
        assert!(!has_complete_v2_operation_envelope(&partial));
        assert!(binary_frame_for(&partial).is_none());

        let engine = RemoteControlEngine::new();
        engine
            .hot_path_capable_targets()
            .lock_unpoisoned()
            .insert((wheel.window_id, wheel.target_user_id.clone()));
        let packet = engine.data_packet_for(&wheel);
        assert!(packet.reliable);
        let decoded: RemoteControlMessage = serde_json::from_slice(&packet.payload).unwrap();
        assert!(has_complete_v2_operation_envelope(&decoded));
    }

    #[test]
    fn capable_fingerprint_matches_shared_contract_vector() {
        let message = message(
            r#"{"v":1,"kind":"pointer","action":"click","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":18,"targetKind":"window","shareInstanceId":"share-instance-example","x":0.5,"y":0.25,"button":0,"buttons":0,"modifiers":{"alt":false,"ctrl":false,"meta":false,"shift":true}}"#,
        );
        let admission = DiscreteAdmission {
            controller_id: "web-1".to_string(),
            window_id: 42,
            target_kind: Some(RemoteControlTargetKind::Window),
            share_instance_id: Some("share-instance-example".to_string()),
            control_session_id: "grant_opaque_example".to_string(),
            input_id: "input-capable-example".to_string(),
            input_seq: 18,
            operation_fingerprint: String::new(),
        };
        assert_eq!(
            canonical_operation_fingerprint(&message, &admission),
            "a4e9531ddc93c944a2005c8fbfbc8f0bb5ed7fdda6a174a5dafdbd360b6fc72a"
        );
    }

    #[test]
    fn fake_platform_and_control_surface_exercise_portable_seams() {
        let platform = FakePlatform::default();
        let message = message(
            r#"{"v":1,"kind":"pointer","action":"click","targetUserId":"native-1","controllerId":"web-1","windowId":42,"seq":1,"x":0.5,"y":0.5}"#,
        );
        let frame = WindowFrame {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        platform.replay(&message, frame, Some(99)).unwrap();
        platform.clear_resolution_cache(42);
        platform.clear_controller_gestures(42, "web-1");
        assert_eq!(*platform.replayed.lock_unpoisoned(), vec![(42, Some(99))]);
        assert_eq!(*platform.cleared_windows.lock_unpoisoned(), vec![42]);
        assert_eq!(
            *platform.cleared_controllers.lock_unpoisoned(),
            vec![(42, "web-1".to_string())]
        );

        let surface = FakeSurface::default();
        RemoteControlEngine::new().emit_status(
            &surface,
            RemoteControlStatus {
                window_id: 42,
                owner_identity: Some("native-1".to_string()),
                controller_id: "web-1".to_string(),
                status: "active",
                message: "granted".to_string(),
                grant_token: Some("grant-a".to_string()),
                reason: None,
            },
        );
        assert_eq!(surface.0.lock_unpoisoned().as_slice().len(), 1);
        assert_eq!(surface.0.lock_unpoisoned()[0].status, "active");
    }
}
