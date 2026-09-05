//! Room-side coordination for AI chat (#657): who holds the push-to-talk
//! floor, which remote sessions are still alive, and how often a peer may ask
//! the host to start or stop one.
//!
//! Everything here takes `now` as a parameter rather than reading the clock,
//! so the state machines are exhaustively testable — these are exactly the
//! rules where a subtle bug shows up as a stuck microphone or a phantom "AI
//! active" badge that outlives the session, and neither is something a live
//! two-peer test reliably catches.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::state::EndReason;
use super::wire::{
    WindowKey, MAX_REQUESTS_PER_SENDER_PER_MINUTE, STATE_HEARTBEAT_SECONDS,
    STATE_MISSED_HEARTBEATS_BEFORE_STALE,
};

/// Longest a participant may hold the floor without releasing it. A client
/// that crashes or drops mid-utterance must not lock everyone else out
/// forever.
pub const MAX_HOLD: Duration = Duration::from_secs(60);

/// No audio from the holder's track for this long ends their turn: their
/// `pttEnd` may simply never arrive.
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of trying to take the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The claimant now holds it.
    Granted,
    /// Someone else holds it; the UI shows "Listening to <who>".
    Busy { holder: String },
}

/// Who may speak to the model right now.
///
/// Exactly one participant at a time: Gemini's manual-activity mode is a
/// single serial audio stream, so two speakers interleaved corrupt the turn
/// rather than mixing into it.
#[derive(Debug, Default)]
pub struct Floor {
    holder: Option<String>,
    claimed_at: Option<Instant>,
    last_audio_at: Option<Instant>,
}

impl Floor {
    pub fn holder(&self) -> Option<&str> {
        self.holder.as_deref()
    }

    /// First claimant wins. A re-claim by the current holder is idempotent, so
    /// a duplicated `pttStart` cannot desynchronize the state.
    pub fn claim(&mut self, who: &str, now: Instant) -> Claim {
        match &self.holder {
            Some(current) if current == who => {
                self.claimed_at = Some(now);
                Claim::Granted
            }
            Some(current) => Claim::Busy {
                holder: current.clone(),
            },
            None => {
                self.holder = Some(who.to_string());
                self.claimed_at = Some(now);
                self.last_audio_at = Some(now);
                Claim::Granted
            }
        }
    }

    /// Release the floor. Only the holder can, so a stray `pttEnd` from a peer
    /// cannot cut someone else off mid-sentence. Returns whether it changed.
    pub fn release(&mut self, who: &str) -> bool {
        if self.holder.as_deref() == Some(who) {
            self.clear();
            true
        } else {
            false
        }
    }

    /// Note that audio arrived from the holder, keeping the turn alive.
    pub fn note_audio(&mut self, who: &str, now: Instant) {
        if self.holder.as_deref() == Some(who) {
            self.last_audio_at = Some(now);
        }
    }

    /// Force-release when the holder vanishes. Called on
    /// `ParticipantDisconnected`, which must end the turn IMMEDIATELY rather
    /// than waiting out the timeout — the audio is definitively gone.
    pub fn release_on_disconnect(&mut self, who: &str) -> bool {
        self.release(who)
    }

    /// Release a turn that has outlived its limits. Returns the identity whose
    /// turn ended, so the caller can close the model's activity window and
    /// tell the room.
    pub fn expire(&mut self, now: Instant) -> Option<String> {
        let holder = self.holder.clone()?;
        let held_too_long = self
            .claimed_at
            .is_some_and(|at| now.duration_since(at) >= MAX_HOLD);
        let gone_quiet = self
            .last_audio_at
            .is_some_and(|at| now.duration_since(at) >= SILENCE_TIMEOUT);
        if held_too_long || gone_quiet {
            self.clear();
            Some(holder)
        } else {
            None
        }
    }

    fn clear(&mut self) {
        self.holder = None;
        self.claimed_at = None;
        self.last_audio_at = None;
    }
}

/// The observable content of one `state` message — everything a receiver
/// renders, with none of the liveness bookkeeping.
///
/// A struct rather than five positional parameters because it travels together
/// everywhere: into [`RemoteSessions::observe`], back out of it for a surface
/// that mounted late, and onto the `ai-chat-remote-state` event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionReport {
    pub active: bool,
    pub started_by: Option<String>,
    pub seconds_left: Option<u64>,
    pub active_speaker: Option<String>,
    /// Present when the session is not running — or is running but refused
    /// something — for a reason worth showing.
    pub error: Option<EndReason>,
}

/// What a receiver believes about one window's session.
#[derive(Debug, Clone)]
pub struct RemoteSession {
    pub report: SessionReport,
    last_heard: Instant,
}

/// Receiver-side view of every window's session, with liveness.
///
/// The host republishes `state` on a heartbeat; if it stops arriving the
/// session is presumed gone. Without this, a host that crashes leaves every
/// other participant showing a live assistant forever — the AI-chat analogue
/// of a phantom window.
#[derive(Debug, Default)]
pub struct RemoteSessions {
    sessions: HashMap<(u32, String), RemoteSession>,
}

impl RemoteSessions {
    /// Record a `state` message from the (already authorized) window owner.
    pub fn observe(&mut self, key: &WindowKey, report: SessionReport, now: Instant) {
        let id = (key.window_id, key.owner_identity.clone());
        if !report.active {
            // A refused start carries `active:false` with an error the REMOTE
            // user must see ("rate limited", "turned off", …). Retain it so
            // `remote_session` — and surfaces that mount after the refusal —
            // can show the reason instead of a silent dead button. A plain
            // stop (no error) stays a removal, so a phantom badge can never
            // reappear; the error record is pruned by `expire_stale` like any
            // other silent session.
            if report.error.is_some() {
                self.sessions.insert(
                    id,
                    RemoteSession {
                        report,
                        last_heard: now,
                    },
                );
            } else {
                self.sessions.remove(&id);
            }
            return;
        }
        self.sessions.insert(
            id,
            RemoteSession {
                report,
                last_heard: now,
            },
        );
    }

    pub fn get(&self, key: &WindowKey) -> Option<&RemoteSession> {
        self.sessions
            .get(&(key.window_id, key.owner_identity.clone()))
    }

    pub fn is_active(&self, key: &WindowKey) -> bool {
        self.get(key).is_some_and(|s| s.report.active)
    }

    /// Drop sessions whose heartbeat has stopped. Returns the cleared keys so
    /// each surface can be told to reset.
    pub fn expire_stale(&mut self, now: Instant) -> Vec<WindowKey> {
        let deadline = Duration::from_secs(
            STATE_HEARTBEAT_SECONDS * STATE_MISSED_HEARTBEATS_BEFORE_STALE as u64,
        );
        let stale: Vec<(u32, String)> = self
            .sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_heard) >= deadline)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            self.sessions.remove(id);
        }
        stale
            .into_iter()
            .map(|(window_id, owner_identity)| WindowKey {
                window_id,
                owner_identity,
            })
            .collect()
    }

    /// Drop everything hosted by a participant who left. Their sessions are
    /// definitively over; waiting for the heartbeat to lapse would leave a
    /// stale badge up for several more seconds.
    pub fn forget_owner(&mut self, owner_identity: &str) -> Vec<WindowKey> {
        let gone: Vec<(u32, String)> = self
            .sessions
            .keys()
            .filter(|(_, owner)| owner == owner_identity)
            .cloned()
            .collect();
        for id in &gone {
            self.sessions.remove(id);
        }
        gone.into_iter()
            .map(|(window_id, owner_identity)| WindowKey {
                window_id,
                owner_identity,
            })
            .collect()
    }
}

/// Per-sender rate limit on one kind of inbound message.
///
/// A peer must not be able to churn the host's Gemini session — or burn its
/// token budget — by spamming the topic. Cheap fixed-window counter; precision
/// is not the point, bounding the damage is.
///
/// The budget is per-instance because the two limited message kinds are not
/// comparable: a `startRequest` churns a WebSocket and a billable token, while
/// a `pttStart` is a human key press. One shared 5/minute bucket would either
/// leave start/stop wide open or cut a normal conversation off mid-sentence
/// (#661).
#[derive(Debug)]
pub struct RequestLimiter {
    recent: HashMap<String, Vec<Instant>>,
    max_per_minute: u32,
}

impl Default for RequestLimiter {
    fn default() -> Self {
        Self::new(MAX_REQUESTS_PER_SENDER_PER_MINUTE)
    }
}

impl RequestLimiter {
    /// A limiter with its own per-minute budget.
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            recent: HashMap::new(),
            max_per_minute,
        }
    }

    /// Whether `sender` may make a request now, recording it if so.
    pub fn allow(&mut self, sender: &str, now: Instant) -> bool {
        let window = Duration::from_secs(60);
        let max = self.max_per_minute;
        let entries = self.recent.entry(sender.to_string()).or_default();
        entries.retain(|at| now.duration_since(*at) < window);
        if entries.len() as u32 >= max {
            return false;
        }
        entries.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> WindowKey {
        WindowKey {
            window_id: 7,
            owner_identity: "alice".into(),
        }
    }

    /// A plain "session is running" report.
    fn live() -> SessionReport {
        SessionReport {
            active: true,
            ..SessionReport::default()
        }
    }

    #[test]
    fn first_claimant_wins_and_others_are_told_who_has_it() {
        let t0 = Instant::now();
        let mut floor = Floor::default();
        assert_eq!(floor.claim("alice", t0), Claim::Granted);
        assert_eq!(
            floor.claim("bob", t0),
            Claim::Busy {
                holder: "alice".into()
            }
        );
        assert_eq!(floor.holder(), Some("alice"));
    }

    #[test]
    fn reclaiming_your_own_floor_is_idempotent() {
        // A duplicated pttStart (double event, retry) must not desync state.
        let t0 = Instant::now();
        let mut floor = Floor::default();
        assert_eq!(floor.claim("alice", t0), Claim::Granted);
        assert_eq!(floor.claim("alice", t0 + Duration::from_secs(1)), Claim::Granted);
        assert_eq!(floor.holder(), Some("alice"));
    }

    #[test]
    fn only_the_holder_can_release() {
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        // A stray pttEnd from a peer must not cut Alice off mid-sentence.
        assert!(!floor.release("bob"));
        assert_eq!(floor.holder(), Some("alice"));
        assert!(floor.release("alice"));
        assert_eq!(floor.holder(), None);
    }

    #[test]
    fn floor_frees_up_after_release_for_the_next_speaker() {
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        floor.release("alice");
        assert_eq!(floor.claim("bob", t0 + Duration::from_secs(1)), Claim::Granted);
    }

    #[test]
    fn a_held_floor_expires_after_the_max_hold() {
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        // Keep audio flowing so only the max-hold rule can fire.
        floor.note_audio("alice", t0 + MAX_HOLD);
        assert_eq!(floor.expire(t0 + MAX_HOLD - Duration::from_secs(1)), None);
        assert_eq!(floor.expire(t0 + MAX_HOLD), Some("alice".into()));
        assert_eq!(floor.holder(), None);
    }

    #[test]
    fn a_silent_holder_loses_the_floor() {
        // Their pttEnd may never arrive — a lost keyup must not wedge the room.
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        assert_eq!(floor.expire(t0 + SILENCE_TIMEOUT - Duration::from_millis(1)), None);
        assert_eq!(floor.expire(t0 + SILENCE_TIMEOUT), Some("alice".into()));
    }

    #[test]
    fn continuing_audio_keeps_the_turn_alive() {
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        for i in 1..10 {
            let now = t0 + Duration::from_secs(i);
            floor.note_audio("alice", now);
            assert_eq!(floor.expire(now), None, "expired while still speaking");
        }
    }

    #[test]
    fn audio_from_a_non_holder_does_not_extend_the_turn() {
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        floor.note_audio("bob", t0 + Duration::from_secs(4));
        assert_eq!(floor.expire(t0 + SILENCE_TIMEOUT), Some("alice".into()));
    }

    #[test]
    fn disconnect_ends_the_turn_immediately() {
        // Not after the timeout: the audio is definitively gone.
        let t0 = Instant::now();
        let mut floor = Floor::default();
        floor.claim("alice", t0);
        assert!(floor.release_on_disconnect("alice"));
        assert_eq!(floor.holder(), None);
        assert_eq!(floor.claim("bob", t0), Claim::Granted);
    }

    #[test]
    fn sessions_expire_when_heartbeats_stop() {
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        sessions.observe(
            &key(),
            SessionReport {
                active: true,
                started_by: Some("bob".into()),
                seconds_left: Some(200),
                ..SessionReport::default()
            },
            t0,
        );
        assert!(sessions.is_active(&key()));

        // Still within the tolerated gap.
        let deadline = Duration::from_secs(
            STATE_HEARTBEAT_SECONDS * STATE_MISSED_HEARTBEATS_BEFORE_STALE as u64,
        );
        assert!(sessions
            .expire_stale(t0 + deadline - Duration::from_secs(1))
            .is_empty());
        assert!(sessions.is_active(&key()));

        // Host went away: the badge must clear rather than linger forever.
        let cleared = sessions.expire_stale(t0 + deadline);
        assert_eq!(cleared, vec![key()]);
        assert!(!sessions.is_active(&key()));
    }

    #[test]
    fn heartbeats_refresh_liveness() {
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        let beat = Duration::from_secs(STATE_HEARTBEAT_SECONDS);
        sessions.observe(
            &key(),
            SessionReport {
                active: true,
                seconds_left: Some(300),
                ..SessionReport::default()
            },
            t0,
        );
        for i in 1..10 {
            let now = t0 + beat * i;
            sessions.observe(
                &key(),
                SessionReport {
                    active: true,
                    seconds_left: Some(300 - i as u64 * 5),
                    ..SessionReport::default()
                },
                now,
            );
            assert!(
                sessions.expire_stale(now).is_empty(),
                "expired despite heartbeats"
            );
        }
        assert!(sessions.is_active(&key()));
    }

    #[test]
    fn an_inactive_state_clears_immediately() {
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        sessions.observe(&key(), live(), t0);
        sessions.observe(&key(), SessionReport::default(), t0);
        assert!(!sessions.is_active(&key()));
    }

    #[test]
    fn a_refused_start_error_is_retained_for_the_remote_user() {
        // A refused start (active=false, error set — e.g. rate limited) must
        // be visible to the REMOTE user who clicked: retain it so
        // `remote_session` answers with the reason instead of a silent dead
        // button, then prune it once stale like any other silent session.
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        sessions.observe(
            &key(),
            SessionReport {
                active: false,
                error: Some(EndReason::RateLimited),
                ..Default::default()
            },
            t0,
        );
        assert!(!sessions.is_active(&key()));
        let retained = sessions.get(&key()).expect("refusal error retained");
        assert_eq!(retained.report.error, Some(EndReason::RateLimited));

        // Pruned after the stale deadline (heartbeats never resume for a
        // refused start).
        let deadline = Duration::from_secs(
            STATE_HEARTBEAT_SECONDS * STATE_MISSED_HEARTBEATS_BEFORE_STALE as u64,
        );
        let pruned = sessions.expire_stale(t0 + deadline + Duration::from_millis(1));
        assert!(pruned.contains(&key()));
        assert!(sessions.get(&key()).is_none());
    }

    #[test]
    fn a_reported_error_survives_the_round_trip() {
        // The receiver UI renders the reason, so a `state` that says "busy"
        // must not come back out of the store as a plain live session.
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        sessions.observe(
            &key(),
            SessionReport {
                active: true,
                error: Some(EndReason::Busy),
                ..SessionReport::default()
            },
            t0,
        );
        assert_eq!(
            sessions.get(&key()).unwrap().report.error,
            Some(EndReason::Busy)
        );
    }

    #[test]
    fn owner_disconnect_clears_all_their_sessions_at_once() {
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        sessions.observe(&key(), live(), t0);
        sessions.observe(
            &WindowKey {
                window_id: 9,
                owner_identity: "alice".into(),
            },
            live(),
            t0,
        );
        sessions.observe(
            &WindowKey {
                window_id: 3,
                owner_identity: "carol".into(),
            },
            live(),
            t0,
        );
        let cleared = sessions.forget_owner("alice");
        assert_eq!(cleared.len(), 2);
        // Carol's session is untouched.
        assert!(sessions.is_active(&WindowKey {
            window_id: 3,
            owner_identity: "carol".into()
        }));
    }

    #[test]
    fn same_window_id_from_different_owners_is_not_confused() {
        // CGWindowIDs are only unique per machine — the owner is part of the key.
        let t0 = Instant::now();
        let mut sessions = RemoteSessions::default();
        let a = WindowKey {
            window_id: 5,
            owner_identity: "alice".into(),
        };
        let b = WindowKey {
            window_id: 5,
            owner_identity: "bob".into(),
        };
        sessions.observe(&a, live(), t0);
        assert!(sessions.is_active(&a));
        assert!(!sessions.is_active(&b), "collided across owners");
    }

    #[test]
    fn request_flooding_is_capped_per_sender() {
        let t0 = Instant::now();
        let mut limiter = RequestLimiter::default();
        for i in 0..MAX_REQUESTS_PER_SENDER_PER_MINUTE {
            assert!(limiter.allow("mallory", t0), "request {i} should pass");
        }
        assert!(
            !limiter.allow("mallory", t0),
            "a peer must not be able to churn the host's session"
        );
        // A different participant is unaffected.
        assert!(limiter.allow("bob", t0));
        // And the window rolls forward.
        assert!(limiter.allow("mallory", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn a_limiter_honours_its_own_budget_not_a_global_one() {
        // Push-to-talk gets its own, much larger bucket. If `allow` ignored the
        // instance's budget and kept using the start/stop constant, a normal
        // back-and-forth would be silenced after five key presses.
        let t0 = Instant::now();
        let mut limiter = RequestLimiter::new(MAX_REQUESTS_PER_SENDER_PER_MINUTE + 3);
        for i in 0..MAX_REQUESTS_PER_SENDER_PER_MINUTE + 3 {
            assert!(limiter.allow("bob", t0), "press {i} should pass");
        }
        assert!(
            !limiter.allow("bob", t0),
            "a larger budget must still be a bound, not an absence of one"
        );
    }
}
