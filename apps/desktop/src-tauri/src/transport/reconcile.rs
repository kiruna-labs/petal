//! Receiver-side publication reconciliation (#298).
//!
//! ## The gap this closes
//!
//! The sender already reconciles: `session/share.rs`'s
//! `repair_active_share_publications_after_reconnect` walks
//! `local_participant().track_publications()` after a reconnect and gives each
//! still-intended share one generation-gated replacement publication (#303).
//!
//! The RECEIVER did not. `subscriber::start_compositor_feed` is purely
//! event-driven: this peer receives a shared window if and only if a
//! `TrackSubscribed` arrived, and stops if and only if a `TrackUnsubscribed`/
//! `TrackUnpublished`/`ParticipantDisconnected` arrived. Nothing ever compares
//! that local picture against what the SFU actually holds, so once the two
//! diverge the receiver stays wrong indefinitely:
//!
//!   * **publication present, nothing receiving it** -- the reported #298
//!     symptom. The share is live on the SFU; this peer never got (or dropped)
//!     the `TrackSubscribed`. `retire_no_frame_windows` cannot help, because
//!     there is no window to retire. It never comes back on its own, and
//!     reloading the receiver does not restore it.
//!   * **receiving state present, publication gone** -- a tile frozen on its
//!     last frame. The 30s no-frame watchdog eventually retires it, but only
//!     after 30s of asserting a share that does not exist.
//!   * **registry bound to a superseded SID** -- after a replacement the
//!     registry still names the old publication, so the SID guard
//!     (`should_remove_window`) evaluates every later teardown against the
//!     wrong identity.
//!
//! ## The contract
//!
//! `discover_window_publications(&Room)` is the single authoritative answer to
//! "which shared-window publications exist right now": it reads
//! `Room::remote_participants()` -> `track_publications()` off the SDK's own
//! live room object, filtered to video tracks whose name parses as
//! `petal-window-<id>`. It is not a cache and not an event replay.
//!
//! `reconcile()` diffs that truth against the receiver's local picture and
//! names each divergence; `recovery_step()` decides, per divergence, whether
//! to spend the ONE permitted recovery attempt or to change the displayed
//! state to the truth. Both are pure, so the decision table is testable
//! without a room. `run_reconciliation_pass` is the thin driver.
//!
//! ## Two hazards this deliberately designs around
//!
//! **1. It must never become a second discovery path.** `subscriber.rs`'s
//! `TrackSubscribed` arm spawns a fresh `NativeVideoStream` per event, on
//! purpose, so that a republish under the same window id starts a clean decode
//! loop. A reconciliation pass that opened windows or started decode loops
//! itself would double them for tracks already being handled -- doubled CPU
//! and corrupted frame stats. So the only recovery lever here is
//! `RemoteTrackPublication::set_subscribed(true)`, which merely expresses this
//! subscriber's demand (it cannot create a duplicate publication) and routes
//! recovery back THROUGH the real `TrackSubscribed` arm.
//!
//! **1b. `set_subscribed(false)` is a one-way door -- never call it here.**
//! Measured against a real SFU (`examples/publication_reconcile_probe`), and
//! confirmed in the vendored SDK: `set_subscribed(false)` calls
//! `set_track(None)` locally, but `set_track(Some(..))` only ever runs from
//! `remote_participant.rs`'s `add_subscribed_media_track`, driven by a NEW
//! `EngineEvent::MediaTrack` off the peer connection. Re-subscribing resumes
//! media on the EXISTING transceiver, so no new media track arrives, no
//! `TrackSubscribed` is re-emitted, and the publication is left with
//! `track == None` permanently -- while frames flow. Since
//! `is_subscribed()` IS `track().is_some()`, an "unsubscribe to force a
//! resubscribe" recovery would permanently poison the exact state this module
//! reads. The probe measured this directly: frames resumed 244 -> 423 with
//! zero re-emitted `TrackSubscribed` and `is_subscribed()` still false.
//!
//! **2. Local truth is the SID registry, not the compositor window set --
//! and the SID registry (`WINDOW_PUBLICATIONS`) is a DIFFERENT map from the
//! decode loop's own `window_states`, corrected here per counselors review
//! of #682's fix (an earlier draft of this comment conflated the two).** The
//! registry entry is written in the `TrackSubscribed` arm and dropped on
//! teardown, so "no registry entry" means "no publication we're tracking" --
//! it says nothing directly about `subscriber.rs`'s decode loop, which lives
//! in a separate `window_states` map this module never touches (grep
//! confirms zero references). Before #682, a `window_states` entry could be
//! removed or replaced while its `tokio::spawn`ed decode loop kept running
//! undetached, parked forever in `stream.next().await`. #682 ties each
//! `ReceiveWindowState` to a `CancellationToken` its decode loop races
//! against, and routes every removal/replacement through
//! `remove_window_state`/`insert_window_state`, which cancel it -- so on the
//! paths that actually call those two functions (an explicit teardown, a
//! republish, or the feed loop itself exiting on leave/rejoin), a
//! `window_states` entry going away now cancels its loop by construction.
//! **This module's own `Orphaned`/`ReportTruth` retire path
//! (`run_reconciliation_pass` below) is NOT one of those paths** -- it drops
//! the SID registry entry and the compositor window, but has no access to
//! `window_states` to cancel anything directly. On that path, the decode
//! loop's own 30s no-frame watchdog (`subscriber.rs`'s
//! `retire_no_frame_windows`) is the sole backstop: once the SFU actually
//! stops delivering frames for the orphaned publication, the watchdog calls
//! `remove_window_state`, which cancels it. Keying off open compositor
//! windows instead would misread a *manually hidden* remote window as a lost
//! subscription and toggle a live track -- exactly the double-decode-loop bug.
//! Everything here is keyed (owner, window) for identity and compared by
//! **track SID** for currency; a replacement legitimately reuses the window id
//! with a new SID and must not be suppressed.
//!
//! ## Not a join-path snapshot
//!
//! Join is already covered structurally by PR #364: the event receiver
//! `Room::connect` registers is threaded into `start_compositor_feed` before
//! the SDK dispatches anything, and `TrackSubscribed` is dispatched for
//! already-published tracks to that connect-time receiver. This module
//! deliberately does not run until `FIRST_PASS_GRACE` after the feed starts,
//! so it only ever handles divergence *after the fact* and never races the
//! live `TrackSubscribed` for a track that is arriving normally.
//!
//! ## Bounded recovery, and what is honestly not recoverable
//!
//! Because `is_subscribed()` is `track().is_some()`, exactly one divergence is
//! recoverable from the client:
//!
//!   * **`NotSubscribed`** -- the SDK holds no track, so nothing can be
//!     receiving it. One `set_subscribed(true)` restores the stream; the probe
//!     measures frames resuming 34 -> 123.
//!   * **`NotReceiving`** -- the SDK holds the track but this receiver has no
//!     decode loop for it. There is NO safe client lever: the SDK will not
//!     re-emit `TrackSubscribed` for a track it already holds, and forcing one
//!     via the unsubscribe door above destroys the handle for good. So this
//!     reports the truth immediately rather than pretending. #298's own
//!     requirement is "restore the receiver OR change the displayed state to
//!     the truth" -- this is the second branch, chosen because measurement
//!     showed the first is unavailable, not because it is easier.
//!
//! Attempts are keyed by the exact publication -- SID included -- and RE-ARM
//! after `RECOVERY_DEADLINE`. Both halves are load-bearing (2026-07-30 live
//! session): a sender caught in a republish loop mints a NEW SID for the same
//! window every ~8s, so a window-keyed "one attempt per room generation"
//! ledger spends its only attempt on a SID that is dead seconds later and
//! then denies recovery to every subsequent republish -- the receiver stays
//! blank/frozen for the rest of the meeting while a live publication exists.
//! A publication that still exists always justifies demand, so
//! `NotSubscribed` is never terminal: it re-expresses demand at the deadline
//! cadence (bounded RATE, not bounded count) until the subscription lands
//! back through the real `TrackSubscribed` arm or the publication disappears
//! (`Orphaned`). The web receiver carries the identical rule
//! (`RecoveryLedger` in `web-harness/src/publicationReconcile.ts`).
//!
//! ## The one verdict that is not trusted on a single sample
//!
//! `Orphaned` must survive `ORPHANED_GRACE` -- in practice a second
//! consecutive pass -- before it retires anything (#630). It is now the only
//! divergence that destroys a window outright, and a sender-side republish
//! briefly looks exactly like it: no publication for the window id, because the
//! old track is unpublished and the replacement is not yet announced. Waiting
//! one extra tick costs an ended share nothing anyone can perceive and saves a
//! republishing share from being destroyed and recreated on pass timing alone.
//! The web receiver carries the identical rule (`ORPHANED_GRACE_MS` in
//! `web-harness/src/publicationReconcile.ts`); the two decision tables are
//! meant to stay identical, so change them together.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use livekit::prelude::*;
use livekit::track::TrackKind;

/// How long after the compositor feed starts before the first reconciliation
/// pass may run. Keeps this module strictly an after-the-fact repair: normal
/// join-time `TrackSubscribed` delivery has long since completed.
pub const FIRST_PASS_GRACE: Duration = Duration::from_secs(15);

/// How long a recovery attempt has to produce a subscribed, receiving track
/// before the receiver gives up and displays the truth instead. Comfortably
/// longer than a healthy resubscribe, and an exact multiple of the 5s
/// compositor-feed watchdog tick this pass rides on.
pub const RECOVERY_DEADLINE: Duration = Duration::from_secs(10);

/// How long a window must have had NO publication at all before the `Orphaned`
/// verdict is trusted (#630, mirroring the web receiver's `ORPHANED_GRACE_MS`).
///
/// A sender-side republish briefly leaves no publication for a window id -- the
/// old track is unpublished and the replacement is not yet announced. A pass
/// landing inside that swap sees the same thing a genuinely ended share looks
/// like, and `Orphaned` is the ONE divergence that still destroys a native
/// window outright, so a single mid-swap sample would destroy and recreate a
/// healthy window purely on pass timing. `Replaced` never had this problem
/// because it adopts. Requiring the verdict to survive a second consecutive
/// pass costs an ended share one extra tick before it disappears, and costs a
/// republish nothing.
pub const ORPHANED_GRACE: Duration = Duration::from_millis(1_500);

/// One shared-window publication as the SDK reports it right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPublication {
    pub owner_identity: String,
    pub window_id: u32,
    pub sid: String,
    pub subscribed: bool,
}

/// The receiver's local picture of one (owner, window) pair: the track SID it
/// is currently receiving. Sourced from the `WINDOW_PUBLICATIONS` registry, so
/// an entry existing means a decode loop exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedWindow {
    pub owner_identity: String,
    pub window_id: u32,
    pub sid: String,
}

pub type WindowKey = (String, u32);

/// A window keyed by the exact share occupying it, SID included.
///
/// The orphan grace is measured per-SID rather than per-window: a different SID
/// is a different share, so a replacement window that appeared and orphaned
/// entirely between two passes -- where the run of orphaned passes looks
/// unbroken but the share underneath it changed -- measures its own grace
/// instead of inheriting the expired clock of the share it replaced.
pub type OrphanKey = (String, u32, String);

/// A named way in which local receiver state disagrees with the SFU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// The SDK holds the track, but this receiver has no decode loop for it --
    /// the `TrackSubscribed` never landed, or was consumed against a
    /// superseded session. Not client-recoverable; see the module doc.
    NotReceiving,
    /// The SDK holds no track for this publication, so nothing can be
    /// receiving it. Ordinary subscription churn, and the one divergence a
    /// client can actually repair.
    NotSubscribed,
    /// The publication exists under a different SID than the one being
    /// received -- the share was replaced and the replacement was never
    /// adopted, so the teardown guard is pointed at a dead identity.
    Replaced { from: String, to: String },
    /// Local state claims a share the SFU no longer holds.
    Orphaned,
}

impl Divergence {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotReceiving => "not-receiving",
            Self::NotSubscribed => "not-subscribed",
            Self::Replaced { .. } => "replaced",
            Self::Orphaned => "orphaned",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub owner_identity: String,
    pub window_id: u32,
    /// The authoritative SID, when the publication still exists.
    pub sid: Option<String>,
    pub divergence: Divergence,
}

impl Finding {
    pub fn key(&self) -> WindowKey {
        (self.owner_identity.clone(), self.window_id)
    }
}

/// What the driver should do about one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStep {
    /// Repoint the SID guard at the authoritative publication. Not a recovery
    /// attempt and not rate-limited: leaving the guard on a dead SID is itself
    /// the bug, and adopting starts no decode loop and sends no request.
    ///
    /// Only ever returned for `Replaced`. Adoption writes the registry, and
    /// the registry's meaning ("a decode loop exists for this SID") is what
    /// makes `NotReceiving` detection sound -- so it must never be written for
    /// a key nothing is receiving. The backstop for a replacement whose new
    /// track genuinely never arrives is the existing 30s no-frame watchdog,
    /// not this module: retiring on replacement instead is exactly the #355
    /// regression, where a live window died on its predecessor's teardown.
    Adopt,
    /// This key already spent its attempt and the deadline has not passed.
    /// Change nothing -- deliberately distinct from `Adopt`, which would
    /// record a decode loop that does not exist and make the next pass read
    /// the key as healthy.
    Wait,
    /// Spend this key's single permitted recovery attempt.
    Attempt,
    /// Stop asserting the share: drop the state, retire the window, report the
    /// truth.
    ReportTruth,
}

/// Walk the SDK's live room object for every shared-window publication.
///
/// The reusable authoritative seam: anything that needs "what should exist
/// right now" asks here rather than replaying events. `Room::remote_participants`
/// excludes the local participant, so a sender's own shares never appear.
pub fn discover_window_publications(room: &Room) -> Vec<WindowPublication> {
    let mut found = Vec::new();
    for (identity, participant) in room.remote_participants() {
        let owner_identity = identity.to_string();
        for publication in participant.track_publications().values() {
            if publication.kind() != TrackKind::Video {
                continue;
            }
            let Some(window_id) =
                crate::transport::publisher::window_id_from_track_name(&publication.name())
            else {
                continue;
            };
            found.push(WindowPublication {
                owner_identity: owner_identity.clone(),
                window_id,
                sid: publication.sid().to_string(),
                subscribed: publication.is_subscribed(),
            });
        }
    }
    found.sort_by(|a, b| {
        (&a.owner_identity, a.window_id, &a.sid).cmp(&(&b.owner_identity, b.window_id, &b.sid))
    });
    found
}

/// Diff authoritative publications against what the receiver is receiving.
///
/// Pure: the driver supplies both sides so the decision table is testable
/// without a room.
pub fn reconcile(discovered: &[WindowPublication], tracked: &[TrackedWindow]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut seen: HashSet<WindowKey> = HashSet::new();

    let tracked_by_key: HashMap<WindowKey, &TrackedWindow> = tracked
        .iter()
        .map(|entry| ((entry.owner_identity.clone(), entry.window_id), entry))
        .collect();

    for publication in discovered {
        let key = (publication.owner_identity.clone(), publication.window_id);
        // One participant publishing two live tracks for one window id is not
        // a state this receiver can represent. Reconcile against the first and
        // leave the duplicate alone rather than flapping between them.
        if !seen.insert(key.clone()) {
            continue;
        }
        let divergence = match tracked_by_key.get(&key) {
            Some(local) if local.sid == publication.sid => {
                // Currency agrees. The only remaining question is whether our
                // subscription demand is still on.
                (!publication.subscribed).then_some(Divergence::NotSubscribed)
            }
            Some(local) => Some(Divergence::Replaced {
                from: local.sid.clone(),
                to: publication.sid.clone(),
            }),
            None if publication.subscribed => Some(Divergence::NotReceiving),
            None => Some(Divergence::NotSubscribed),
        };
        if let Some(divergence) = divergence {
            findings.push(Finding {
                owner_identity: publication.owner_identity.clone(),
                window_id: publication.window_id,
                sid: Some(publication.sid.clone()),
                divergence,
            });
        }
    }

    for entry in tracked {
        let key = (entry.owner_identity.clone(), entry.window_id);
        if seen.contains(&key) {
            continue;
        }
        findings.push(Finding {
            owner_identity: entry.owner_identity.clone(),
            window_id: entry.window_id,
            sid: None,
            divergence: Divergence::Orphaned,
        });
    }

    findings.sort_by(|a, b| (&a.owner_identity, a.window_id).cmp(&(&b.owner_identity, b.window_id)));
    findings
}

/// The bounded-recovery decision table.
///
/// `attempted_for` is `Some(elapsed_since_attempt)` when this key has already
/// spent its single attempt in the current room generation. `orphaned_for` is
/// `Some(elapsed_since_first_sighting)` when this exact share has already been
/// seen orphaned in the current unbroken run of orphaned passes, and `None` on
/// the first sighting of a run.
pub fn recovery_step(
    divergence: &Divergence,
    attempted_for: Option<Duration>,
    orphaned_for: Option<Duration>,
) -> RecoveryStep {
    match divergence {
        // The SFU says this share is gone. There is nothing to recover, and
        // continuing to display it is the lie #298 is about -- but only once
        // the absence has outlived a republish in flight (#630). A single
        // sample taken mid-swap is indistinguishable from an ended share, and
        // this is the one divergence that still destroys the window.
        Divergence::Orphaned => match orphaned_for {
            Some(elapsed) if elapsed >= ORPHANED_GRACE => RecoveryStep::ReportTruth,
            _ => RecoveryStep::Wait,
        },
        // A replacement only needs the guard repointed. Deliberately NOT a
        // resubscribe: the new SID's own `TrackSubscribed` may still be in
        // flight, and forcing one would risk a second decode loop for a track
        // already being handled. If the replacement genuinely never delivers,
        // the existing 30s no-frame watchdog retires the window.
        Divergence::Replaced { .. } => RecoveryStep::Adopt,
        // The SDK holds the track already. It will not re-emit
        // `TrackSubscribed` for it, and the only way to force one destroys the
        // handle permanently (module doc, hazard 1b). Measured, not assumed:
        // there is nothing safe to attempt, so report the truth instead of
        // leaving a blank window asserting a share.
        Divergence::NotReceiving => RecoveryStep::ReportTruth,
        Divergence::NotSubscribed => match attempted_for {
            None => RecoveryStep::Attempt,
            // Attempt made, deadline not reached: let it land.
            Some(elapsed) if elapsed < RECOVERY_DEADLINE => RecoveryStep::Wait,
            // Past the deadline the attempt RE-ARMS rather than turning
            // terminal: the publication still exists (an absent one is
            // `Orphaned`), so demand stays legitimate, and giving up here is
            // what froze the 2026-07-30 session for good. Bounded in rate by
            // the deadline cadence, not in count.
            Some(_) => RecoveryStep::Attempt,
        },
    }
}

/// Per-generation record of demand attempts.
///
/// Attempts are keyed by the exact publication (`OrphanKey`, SID included),
/// not by `(owner, window)`: a republish mints a new SID for the same window,
/// and a new publication always deserves its own attempt clock -- see the
/// module doc's "bounded rate, not bounded count" section.
#[derive(Debug, Default)]
pub struct RecoveryLedger {
    attempts: HashMap<OrphanKey, Instant>,
    orphan_sightings: HashMap<OrphanKey, Instant>,
}

impl RecoveryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attempted_for(&self, key: &OrphanKey, now: Instant) -> Option<Duration> {
        self.attempts
            .get(key)
            .map(|at| now.saturating_duration_since(*at))
    }

    pub fn record_attempt(&mut self, key: OrphanKey, now: Instant) {
        self.attempts.insert(key, now);
    }

    /// Drops every attempt clock for a publication that is no longer both
    /// present and demand-divergent: a key that reconciles clean gets its
    /// budget back, and a SID that vanished can never be measured again --
    /// the same lifecycle guard `retain_orphan_sightings` applies to the
    /// orphan clocks.
    pub fn retain_attempts(&mut self, demand_divergent_now: &HashSet<OrphanKey>) {
        self.attempts
            .retain(|key, _| demand_divergent_now.contains(key));
    }

    /// Elapsed time since this exact share was first seen orphaned in the
    /// current unbroken run of orphaned passes, or `None` if this pass is the
    /// first of a run.
    pub fn orphaned_for(&self, key: &OrphanKey, now: Instant) -> Option<Duration> {
        self.orphan_sightings
            .get(key)
            .map(|at| now.saturating_duration_since(*at))
    }

    /// Records the FIRST sighting of a run only -- the clock never restarts, or
    /// the grace could never expire.
    pub fn record_orphan_sighting(&mut self, key: OrphanKey, now: Instant) {
        self.orphan_sightings.entry(key).or_insert(now);
    }

    /// Drops every clock for a share not orphaned in the pass just evaluated,
    /// making the grace measure CONSECUTIVE passes rather than "orphaned once,
    /// ever".
    ///
    /// This is the lifecycle guard, not bookkeeping. A clock that outlives the
    /// window it was started for -- the publication returns, or the window is
    /// retired through another path -- would still be running when that window
    /// id next orphans, and its long-expired value would retire the new window
    /// on its FIRST sighting: exactly the single-sample teardown the grace
    /// exists to prevent. This repo has been bitten repeatedly by guards that
    /// were correct while the state behind them did not survive a
    /// retire/reveal (#416), so the clock is pruned rather than trusted.
    pub fn retain_orphan_sightings(&mut self, orphaned_now: &HashSet<OrphanKey>) {
        self.orphan_sightings
            .retain(|key, _| orphaned_now.contains(key));
    }

    /// A reconnect has no trustworthy publication snapshot: LiveKit clears
    /// remote participants before it announces the completed connection state.
    /// Drop every recovery clock so a long reconnect cannot turn a stale
    /// absence into an immediate terminal `Orphaned` verdict on resume.
    pub fn clear_for_reconnect(&mut self) {
        self.attempts.clear();
        self.orphan_sightings.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.attempts.len()
    }

    #[cfg(test)]
    pub fn orphan_sighting_count(&self) -> usize {
        self.orphan_sightings.len()
    }
}

/// Decide the step for every finding in one pass, and advance the orphan
/// grace clocks that decision depends on.
///
/// Split out of `run_reconciliation_pass` deliberately. The pass itself needs a
/// live `Room` and `AppHandle`, so it cannot be tested; the bugs this module
/// has actually shipped live in which state the pass feeds the decision table,
/// not in the table itself -- which pass prunes, in what order it reads and
/// records, and whether a clock survives a window lifecycle. Keeping that logic
/// here means a test can drive the real decision path rather than only the pure
/// function it delegates to.
///
/// MUST be called on every pass, including one with no findings at all: a pass
/// in which everything reconciles clean is exactly how a run of orphaned passes
/// ends, and skipping it would leave a clock running across the gap.
pub fn plan_recovery_steps(
    findings: &[Finding],
    tracked: &[TrackedWindow],
    ledger: &mut RecoveryLedger,
    now: Instant,
) -> Vec<RecoveryStep> {
    // An `Orphaned` finding carries no SID -- there is no publication left to
    // name one -- so the window's own tracked SID identifies the share whose
    // grace is running. `reconcile` only ever reports `Orphaned` for a key
    // present in `tracked`, so this lookup always resolves.
    let orphan_key_for = |finding: &Finding| -> Option<OrphanKey> {
        if finding.divergence != Divergence::Orphaned {
            return None;
        }
        tracked
            .iter()
            .find(|entry| {
                entry.owner_identity == finding.owner_identity
                    && entry.window_id == finding.window_id
            })
            .map(|entry| {
                (
                    entry.owner_identity.clone(),
                    entry.window_id,
                    entry.sid.clone(),
                )
            })
    };

    // Attempt clocks are keyed by SID and live exactly as long as their
    // publication stays demand-divergent -- a republish's new SID starts with
    // a fresh clock instead of inheriting a dead SID's spent one.
    let attempt_key_for = |finding: &Finding| -> Option<OrphanKey> {
        if finding.divergence != Divergence::NotSubscribed {
            return None;
        }
        finding
            .sid
            .clone()
            .map(|sid| (finding.owner_identity.clone(), finding.window_id, sid))
    };

    // Prune before reading any clock: this pass's orphans keep their sighting,
    // every other share loses one it can no longer legitimately be measuring.
    let orphaned_now: HashSet<OrphanKey> =
        findings.iter().filter_map(|f| orphan_key_for(f)).collect();
    ledger.retain_orphan_sightings(&orphaned_now);
    let demand_divergent_now: HashSet<OrphanKey> =
        findings.iter().filter_map(|f| attempt_key_for(f)).collect();
    ledger.retain_attempts(&demand_divergent_now);

    findings
        .iter()
        .map(|finding| {
            let orphaned_for = orphan_key_for(finding).and_then(|orphan_key| {
                let elapsed = ledger.orphaned_for(&orphan_key, now);
                ledger.record_orphan_sighting(orphan_key, now);
                elapsed
            });
            let attempted_for = attempt_key_for(finding)
                .and_then(|attempt_key| ledger.attempted_for(&attempt_key, now));
            recovery_step(&finding.divergence, attempted_for, orphaned_for)
        })
        .collect()
}

/// Reconciliation planning with the feed's connection lifecycle applied.
///
/// During a reconnect the SFU snapshot is known to be transiently empty, so
/// suppress all recovery side effects. Returning `Wait` preserves the
/// finding-to-step shape used by the production pass while `clear_for_reconnect`
/// ensures a real departure is measured from a fresh grace period after
/// `Reconnected`.
pub fn plan_recovery_steps_for_connection(
    reconnecting: bool,
    findings: &[Finding],
    tracked: &[TrackedWindow],
    ledger: &mut RecoveryLedger,
    now: Instant,
) -> Vec<RecoveryStep> {
    if reconnecting {
        ledger.clear_for_reconnect();
        return vec![RecoveryStep::Wait; findings.len()];
    }
    plan_recovery_steps(findings, tracked, ledger, now)
}

/// Every shared-window publication keyed the way the receiver keys its own
/// state, so the driver can reach the SDK object behind a finding.
#[cfg(target_os = "macos")]
fn publications_by_key(room: &Room) -> HashMap<WindowKey, RemoteTrackPublication> {
    let mut by_key = HashMap::new();
    for (identity, participant) in room.remote_participants() {
        let owner_identity = identity.to_string();
        for publication in participant.track_publications().values() {
            if publication.kind() != TrackKind::Video {
                continue;
            }
            let Some(window_id) =
                crate::transport::publisher::window_id_from_track_name(&publication.name())
            else {
                continue;
            };
            by_key
                .entry((owner_identity.clone(), window_id))
                .or_insert_with(|| publication.clone());
        }
    }
    by_key
}

/// One authoritative reconciliation pass. Runs on the compositor feed's
/// existing 5s watchdog tick, which is what lets it cover all three #298
/// triggers without a separate signal per trigger: a reconnect, a publication
/// replacement, and subscription churn all surface here as the same thing --
/// a disagreement with `discover_window_publications`.
#[cfg(target_os = "macos")]
pub(crate) fn run_reconciliation_pass(
    app: &tauri::AppHandle,
    room: &Room,
    ledger: &mut RecoveryLedger,
    reconnecting: bool,
) {
    let discovered = discover_window_publications(room);
    let tracked = crate::transport::subscriber::tracked_window_publications();
    let findings = reconcile(&discovered, &tracked);

    // Attempt-budget hygiene (the old `clear_healthy`) now lives inside
    // `plan_recovery_steps` -- `retain_attempts` keeps a clock only while its
    // exact SID stays demand-divergent, which both restores the budget of a
    // key that reconciled clean and drops clocks for SIDs that vanished.

    // There is deliberately NO early return for an empty `findings` here.
    // A pass with nothing divergent is precisely how a run of orphaned passes
    // ends, so it MUST still reach `plan_recovery_steps` to prune the grace
    // clocks -- returning early would leave a clock running across the gap for
    // a share that recovered, and the next orphan would inherit it and retire
    // on its first sighting. That ordering used to be a comment; making the
    // empty case flow through the same path instead means it cannot be
    // reintroduced by a later edit. The loop below is a no-op when empty, and
    // `by_key` stays lazy so the SDK walk is still skipped.
    let now = Instant::now();
    let steps =
        plan_recovery_steps_for_connection(reconnecting, &findings, &tracked, ledger, now);

    let by_key: std::cell::OnceCell<HashMap<WindowKey, RemoteTrackPublication>> =
        std::cell::OnceCell::new();

    for (finding, step) in findings.into_iter().zip(steps) {
        let key = finding.key();
        let (owner_identity, window_id) = key.clone();
        let label = finding.divergence.label();
        match step {
            RecoveryStep::Wait => {
                if finding.divergence == Divergence::Orphaned {
                    // INFO, not DEBUG: the file sink ships at `info` and
                    // `RUST_LOG` only overrides under `cargo run`/`tauri dev`,
                    // so a DEBUG line here would make "held a window through a
                    // republish" indistinguishable from "the pass found
                    // nothing" on a real user's build. The terminal retire
                    // logs at `warn`, so without this the disappearing case is
                    // diagnosable and the staying-too-long case is not. Bounded
                    // by the grace: at most one line per held window per pass.
                    log::info!(
                        "reconcile: window {window_id} from '{owner_identity}' outcome=waiting \
                         divergence={label}; no publication yet, holding for a republish in \
                         flight before retiring"
                    );
                } else {
                    log::debug!(
                        "reconcile: window {window_id} from '{owner_identity}' outcome=waiting \
                         divergence={label}; attempt already spent, deadline not reached"
                    );
                }
            }
            RecoveryStep::Adopt => {
                let Some(publication) = by_key.get_or_init(|| publications_by_key(room)).get(&key)
                else {
                    continue;
                };
                log::info!(
                    "reconcile: window {window_id} from '{owner_identity}' outcome=adopt \
                     divergence={label} sid={}",
                    publication.sid()
                );
                crate::transport::subscriber::adopt_window_publication(
                    &owner_identity,
                    window_id,
                    publication.clone(),
                );
            }
            RecoveryStep::Attempt => {
                let Some(publication) = by_key.get_or_init(|| publications_by_key(room)).get(&key)
                else {
                    continue;
                };
                // Key the clock by the finding's SID -- the same value
                // `plan_recovery_steps` reads -- so the record and the next
                // pass's lookup can never disagree.
                let sid = finding
                    .sid
                    .clone()
                    .unwrap_or_else(|| publication.sid().to_string());
                ledger.record_attempt((owner_identity.clone(), window_id, sid.clone()), now);
                log::warn!(
                    "reconcile: window {window_id} from '{owner_identity}' outcome=recovering \
                     divergence={label} sid={sid}; re-expressing subscription demand"
                );
                attempt_resubscribe(publication.clone());
            }
            RecoveryStep::ReportTruth => {
                // #627: only a GONE publication justifies hiding the window.
                // `Orphaned` is exactly that case -- the SFU holds nothing, so
                // the window was asserting a share that does not exist. Every
                // other terminal divergence has a live publication behind it
                // that this receiver merely failed to re-establish; hiding
                // that window makes a real share vanish and reveal the
                // desktop, which is the disruption the never-black rule
                // exists to prevent. Hold its last frame instead.
                if finding.divergence == Divergence::Orphaned {
                    log::warn!(
                        "reconcile: window {window_id} from '{owner_identity}' outcome=terminal \
                         divergence={label}; the SFU holds no publication, retiring rather than \
                         displaying a share that does not exist"
                    );
                    crate::transport::subscriber::forget_window_publication(
                        &owner_identity,
                        window_id,
                    );
                    crate::compositor::remove_window(
                        app,
                        &owner_identity,
                        window_id,
                        crate::compositor::RemoveWindowReason::ReconciledPublicationGone,
                    );
                    continue;
                }
                // The tracked entry is deliberately KEPT here. `Orphaned` is
                // only ever reported for a key present in `tracked` (see
                // `reconcile`'s second loop), so forgetting a held window would
                // make it invisible to the one path that can still retire it --
                // leaving it frozen on screen for the rest of the meeting, a
                // worse outcome than the vanishing this change fixes.
                // `hold_window_last_frame` is idempotent, so a divergence that
                // recurs every pass does not re-log or re-signal.
                if !crate::compositor::hold_window_last_frame(
                    app,
                    &owner_identity,
                    window_id,
                    crate::compositor::HoldWindowReason::ReconciledUnrecoverable,
                ) {
                    log::warn!(
                        "reconcile: window {window_id} from '{owner_identity}' outcome=terminal \
                         divergence={label}; nothing displayable to hold, retiring"
                    );
                    crate::transport::subscriber::forget_window_publication(
                        &owner_identity,
                        window_id,
                    );
                    crate::compositor::remove_window(
                        app,
                        &owner_identity,
                        window_id,
                        crate::compositor::RemoveWindowReason::ReconciledUnrecoverable,
                    );
                }
            }
        }
    }
}

/// The single permitted recovery action: re-express subscription demand.
///
/// `set_subscribed(true)` cannot produce a duplicate publication -- it only
/// tells the SFU this subscriber wants the track -- and recovery lands back
/// through the real `TrackSubscribed` arm rather than opening a window behind
/// its back. There is deliberately no `set_subscribed(false)` here; see the
/// module doc's hazard 1b for the measurement behind that.
#[cfg(target_os = "macos")]
fn attempt_resubscribe(publication: RemoteTrackPublication) {
    tauri::async_runtime::spawn(async move {
        publication.set_subscribed(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication(owner: &str, window_id: u32, sid: &str, subscribed: bool) -> WindowPublication {
        WindowPublication {
            owner_identity: owner.to_string(),
            window_id,
            sid: sid.to_string(),
            subscribed,
        }
    }

    fn tracked(owner: &str, window_id: u32, sid: &str) -> TrackedWindow {
        TrackedWindow {
            owner_identity: owner.to_string(),
            window_id,
            sid: sid.to_string(),
        }
    }

    #[test]
    fn a_healthy_subscribed_share_produces_no_finding() {
        let findings = reconcile(
            &[publication("bob", 7, "TR_A", true)],
            &[tracked("bob", 7, "TR_A")],
        );
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn a_subscribed_publication_nothing_is_receiving_is_the_reported_symptom() {
        // #298: the share is live on the SFU and this receiver has nothing for
        // it, so no watchdog can notice and it never comes back.
        let findings = reconcile(&[publication("bob", 7, "TR_A", true)], &[]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].divergence, Divergence::NotReceiving);
        assert_eq!(findings[0].sid.as_deref(), Some("TR_A"));
    }

    #[test]
    fn an_unsubscribed_publication_is_subscription_churn_not_a_lost_subscription() {
        let findings = reconcile(&[publication("bob", 7, "TR_A", false)], &[]);
        assert_eq!(findings[0].divergence, Divergence::NotSubscribed);
    }

    #[test]
    fn demand_lost_on_a_track_we_are_still_receiving_is_subscription_churn() {
        let findings = reconcile(
            &[publication("bob", 7, "TR_A", false)],
            &[tracked("bob", 7, "TR_A")],
        );
        assert_eq!(findings[0].divergence, Divergence::NotSubscribed);
    }

    #[test]
    fn tracked_state_whose_publication_is_gone_is_orphaned() {
        let findings = reconcile(&[], &[tracked("bob", 7, "TR_A")]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].divergence, Divergence::Orphaned);
        assert_eq!(findings[0].sid, None);
    }

    #[test]
    fn a_superseded_sid_is_reported_as_a_replacement_carrying_both_identities() {
        let findings = reconcile(
            &[publication("bob", 7, "TR_NEW", true)],
            &[tracked("bob", 7, "TR_OLD")],
        );
        assert_eq!(
            findings[0].divergence,
            Divergence::Replaced {
                from: "TR_OLD".to_string(),
                to: "TR_NEW".to_string()
            }
        );
    }

    #[test]
    fn a_replacement_reusing_the_window_id_is_adopted_never_resubscribed() {
        // The parallel-agent hazard: a republish reuses window_id with a new
        // sid and its own TrackSubscribed may still be in flight. Forcing a
        // resubscribe here would risk a second decode loop for one track.
        let divergence = Divergence::Replaced {
            from: "TR_OLD".to_string(),
            to: "TR_NEW".to_string(),
        };
        assert_eq!(recovery_step(&divergence, None, None), RecoveryStep::Adopt);
        assert_eq!(
            recovery_step(&divergence, Some(RECOVERY_DEADLINE * 10), None),
            RecoveryStep::Adopt,
            "a replacement must never escalate to retiring a live tile"
        );
    }

    #[test]
    fn same_window_id_from_two_owners_is_kept_separate() {
        let findings = reconcile(
            &[
                publication("bob", 7, "TR_A", true),
                publication("carol", 7, "TR_B", true),
            ],
            &[tracked("bob", 7, "TR_A")],
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].owner_identity, "carol");
        assert_eq!(findings[0].divergence, Divergence::NotReceiving);
    }

    #[test]
    fn one_owners_orphan_does_not_disturb_another_owners_healthy_share() {
        let findings = reconcile(
            &[publication("carol", 7, "TR_B", true)],
            &[tracked("bob", 7, "TR_A"), tracked("carol", 7, "TR_B")],
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].owner_identity, "bob");
        assert_eq!(findings[0].divergence, Divergence::Orphaned);
    }

    #[test]
    fn lost_subscription_demand_re_arms_at_the_deadline_cadence_and_is_never_terminal() {
        // 2026-07-30 live session: terminally giving up on demand while the
        // publication still exists left the receiver blank for the rest of
        // the meeting. Demand is bounded in RATE (one call per SID per
        // deadline), never in count -- an absent publication is `Orphaned`,
        // which is the only terminal verdict.
        let divergence = Divergence::NotSubscribed;
        assert_eq!(recovery_step(&divergence, None, None), RecoveryStep::Attempt);
        assert_eq!(
            recovery_step(&divergence, Some(Duration::from_secs(1)), None),
            RecoveryStep::Wait,
            "a second pass inside the deadline must not spend a second attempt"
        );
        assert_eq!(
            recovery_step(&divergence, Some(RECOVERY_DEADLINE), None),
            RecoveryStep::Attempt,
            "past the deadline the attempt re-arms rather than turning terminal"
        );
        assert_eq!(
            recovery_step(&divergence, Some(RECOVERY_DEADLINE * 100), None),
            RecoveryStep::Attempt
        );
    }

    #[test]
    fn a_track_the_sdk_already_holds_reports_truth_rather_than_faking_a_recovery() {
        // Measured, not assumed (module doc hazard 1b): the SDK will not
        // re-emit TrackSubscribed for a track it already holds, and the only
        // lever that would force one destroys the handle permanently. There is
        // nothing safe to attempt, so never spend one pretending otherwise.
        for attempted in [None, Some(Duration::ZERO), Some(RECOVERY_DEADLINE * 100)] {
            assert_eq!(
                recovery_step(&Divergence::NotReceiving, attempted, None),
                RecoveryStep::ReportTruth
            );
        }
    }

    #[test]
    fn only_a_replacement_ever_writes_the_registry() {
        // The registry means "a decode loop exists for this sid". Writing it
        // for a key nothing is receiving would make the next pass read the
        // key as healthy and silently end reconciliation.
        for divergence in [
            Divergence::NotReceiving,
            Divergence::NotSubscribed,
            Divergence::Orphaned,
        ] {
            for attempted in [None, Some(Duration::from_secs(1)), Some(RECOVERY_DEADLINE)] {
                for orphaned in [None, Some(Duration::ZERO), Some(ORPHANED_GRACE * 100)] {
                    assert_ne!(
                        recovery_step(&divergence, attempted, orphaned),
                        RecoveryStep::Adopt,
                        "{divergence:?} must never adopt"
                    );
                }
            }
        }
        assert_eq!(
            recovery_step(
                &Divergence::Replaced {
                    from: "TR_OLD".to_string(),
                    to: "TR_NEW".to_string()
                },
                None,
                None
            ),
            RecoveryStep::Adopt
        );
    }

    #[test]
    fn an_orphan_holds_for_the_grace_then_reports_truth_without_spending_an_attempt() {
        // #630: a republish briefly leaves no publication for the window id.
        // One sample taken inside that swap must not destroy the window --
        // `Orphaned` is the only divergence that still retires one outright.
        assert_eq!(
            recovery_step(&Divergence::Orphaned, None, None),
            RecoveryStep::Wait
        );
        assert_eq!(
            recovery_step(&Divergence::Orphaned, None, Some(ORPHANED_GRACE / 2)),
            RecoveryStep::Wait
        );
        assert_eq!(
            recovery_step(&Divergence::Orphaned, None, Some(ORPHANED_GRACE)),
            RecoveryStep::ReportTruth
        );
        // A share that really ended still stops being displayed -- the #298
        // intent is unchanged, it just takes one more pass to conclude it.
        assert_eq!(
            recovery_step(&Divergence::Orphaned, None, Some(ORPHANED_GRACE * 100)),
            RecoveryStep::ReportTruth
        );
        // The attempt clock never buys an orphan anything either way.
        assert_eq!(
            recovery_step(&Divergence::Orphaned, Some(RECOVERY_DEADLINE * 10), None),
            RecoveryStep::Wait
        );
    }

    #[test]
    fn an_orphan_clock_never_outlives_the_window_lifecycle_it_was_started_for() {
        // The failure this repo keeps hitting (#416): the guard is correct and
        // the state behind it does not survive a retire/reveal. A clock left
        // running past its window would retire the NEXT share to occupy that
        // id on its FIRST sighting -- the exact single-sample teardown the
        // grace exists to prevent.
        let mut ledger = RecoveryLedger::new();
        let key: OrphanKey = ("bob".to_string(), 7, "TR_A".to_string());
        let now = Instant::now();

        ledger.record_orphan_sighting(key.clone(), now);
        assert_eq!(ledger.orphan_sighting_count(), 1);

        // A pass in which nothing is orphaned ends the run and the clock.
        ledger.retain_orphan_sightings(&HashSet::new());
        assert_eq!(ledger.orphan_sighting_count(), 0);
        assert_eq!(ledger.orphaned_for(&key, now + ORPHANED_GRACE * 100), None);
    }

    #[test]
    fn a_new_sid_for_the_same_window_measures_its_own_grace() {
        // A replacement window that appeared and orphaned entirely between two
        // passes leaves the run of orphaned passes looking unbroken, so the SID
        // is what separates the new share from the one whose clock expired.
        let mut ledger = RecoveryLedger::new();
        let now = Instant::now();
        let old: OrphanKey = ("bob".to_string(), 7, "TR_A".to_string());
        let new: OrphanKey = ("bob".to_string(), 7, "TR_B".to_string());

        ledger.record_orphan_sighting(old.clone(), now);
        let later = now + ORPHANED_GRACE * 100;
        assert!(ledger.orphaned_for(&old, later).is_some());
        assert_eq!(ledger.orphaned_for(&new, later), None);
    }

    #[test]
    fn an_orphan_clock_starts_once_and_never_restarts_under_a_repeating_verdict() {
        // If a re-sighting reset the clock, the grace could never expire and a
        // genuinely ended share would stay on screen for the whole meeting --
        // the opposite failure, and the reason #298 exists.
        let mut ledger = RecoveryLedger::new();
        let key: OrphanKey = ("bob".to_string(), 7, "TR_A".to_string());
        let started = Instant::now();
        ledger.record_orphan_sighting(key.clone(), started);

        let orphaned_now = HashSet::from([key.clone()]);
        let second_pass = started + ORPHANED_GRACE;
        ledger.retain_orphan_sightings(&orphaned_now);
        ledger.record_orphan_sighting(key.clone(), second_pass);

        assert_eq!(
            ledger.orphaned_for(&key, second_pass),
            Some(ORPHANED_GRACE),
            "the clock must still be measured from the FIRST sighting"
        );
        assert_eq!(
            recovery_step(
                &Divergence::Orphaned,
                None,
                ledger.orphaned_for(&key, second_pass)
            ),
            RecoveryStep::ReportTruth
        );
    }

    /// Drive the real decision path for one pass: reconcile, then plan.
    ///
    /// This is deliberately the whole chain a pass runs, not `recovery_step`
    /// alone -- the defects this guards against are in which state the pass
    /// feeds the table (prune order, SID resolution, whether a clean pass ends
    /// a run), all of which a direct `recovery_step` call would step over.
    fn pass(
        discovered: &[WindowPublication],
        tracked: &[TrackedWindow],
        ledger: &mut RecoveryLedger,
        now: Instant,
    ) -> Vec<(Finding, RecoveryStep)> {
        let findings = reconcile(discovered, tracked);
        let steps = plan_recovery_steps(&findings, tracked, ledger, now);
        findings.into_iter().zip(steps).collect()
    }

    fn steps_for(outcome: &[(Finding, RecoveryStep)]) -> Vec<RecoveryStep> {
        outcome.iter().map(|(_, step)| *step).collect()
    }

    #[test]
    fn a_pass_landing_mid_republish_holds_the_window_and_the_next_pass_retires_it() {
        // The #630 defect end to end: with no publication for the window id,
        // one sample must not destroy the window, and a second consecutive
        // sighting still must.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();
        let tiles = [tracked("bob", 7, "TR_A")];

        assert_eq!(
            steps_for(&pass(&[], &tiles, &mut ledger, start)),
            vec![RecoveryStep::Wait],
            "a single mid-swap sample must never retire the window"
        );
        assert_eq!(
            steps_for(&pass(&[], &tiles, &mut ledger, start + ORPHANED_GRACE)),
            vec![RecoveryStep::ReportTruth],
            "a share that really ended still stops being displayed"
        );
    }

    #[test]
    fn a_republish_landing_between_passes_is_adopted_rather_than_retired() {
        // The republish this grace exists for: pass one samples the gap between
        // unpublish and the replacement being announced, then the replacement
        // lands and must be adopted -- never torn down.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();
        let tiles = [tracked("bob", 7, "TR_OLD")];

        assert_eq!(
            steps_for(&pass(&[], &tiles, &mut ledger, start)),
            vec![RecoveryStep::Wait]
        );
        assert_eq!(
            steps_for(&pass(
                &[publication("bob", 7, "TR_NEW", true)],
                &tiles,
                &mut ledger,
                start + ORPHANED_GRACE
            )),
            vec![RecoveryStep::Adopt]
        );
    }

    #[test]
    fn a_clean_pass_between_orphan_sightings_ends_the_run_and_the_clock() {
        // The empty-findings path: `run_reconciliation_pass` returns early when
        // nothing diverges, so the prune has to happen BEFORE that return or a
        // recovered share leaves its clock running across the gap.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();
        let tiles = [tracked("bob", 7, "TR_A")];
        let healthy = [publication("bob", 7, "TR_A", true)];

        assert_eq!(
            steps_for(&pass(&[], &tiles, &mut ledger, start)),
            vec![RecoveryStep::Wait]
        );

        // Recovered: no findings at all, which is what ends the run.
        assert!(pass(&healthy, &tiles, &mut ledger, start + ORPHANED_GRACE).is_empty());
        assert_eq!(ledger.orphan_sighting_count(), 0);

        // Orphaned again much later: the first sighting of the NEW run must
        // hold rather than inherit the clock from the old one.
        assert_eq!(
            steps_for(&pass(&[], &tiles, &mut ledger, start + ORPHANED_GRACE * 100)),
            vec![RecoveryStep::Wait]
        );
    }

    #[test]
    fn a_successor_share_in_the_same_window_is_not_retired_on_its_first_sighting() {
        // A window retired for a genuinely ended share, then re-shared and
        // orphaned, with no pass ever observing a publication in between: the
        // run of orphaned passes looks unbroken, so only the SID distinguishes
        // the new share from the expired clock of the old one.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();

        assert_eq!(
            steps_for(&pass(&[], &[tracked("bob", 7, "TR_A")], &mut ledger, start)),
            vec![RecoveryStep::Wait]
        );
        assert_eq!(
            steps_for(&pass(
                &[],
                &[tracked("bob", 7, "TR_A")],
                &mut ledger,
                start + ORPHANED_GRACE
            )),
            vec![RecoveryStep::ReportTruth]
        );

        // Same owner and window id, different share.
        assert_eq!(
            steps_for(&pass(
                &[],
                &[tracked("bob", 7, "TR_B")],
                &mut ledger,
                start + ORPHANED_GRACE * 100
            )),
            vec![RecoveryStep::Wait],
            "the successor must measure its own grace, not inherit an expired clock"
        );
    }

    #[test]
    fn one_owners_orphan_grace_does_not_delay_or_disturb_another_owners_share() {
        // Two peers, one orphaned and one healthy: the grace must be per-share,
        // and a healthy share must never be dragged into it.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();
        let tiles = [tracked("bob", 7, "TR_A"), tracked("carol", 9, "TR_B")];
        let carol_only = [publication("carol", 9, "TR_B", true)];

        let first = pass(&carol_only, &tiles, &mut ledger, start);
        assert_eq!(first.len(), 1, "only bob's window diverges");
        assert_eq!(first[0].0.owner_identity, "bob");
        assert_eq!(steps_for(&first), vec![RecoveryStep::Wait]);

        let second = pass(&carol_only, &tiles, &mut ledger, start + ORPHANED_GRACE);
        assert_eq!(second[0].0.owner_identity, "bob");
        assert_eq!(steps_for(&second), vec![RecoveryStep::ReportTruth]);
    }

    #[test]
    fn the_ledger_keys_attempts_by_sid_so_a_republished_track_gets_a_fresh_attempt() {
        let mut ledger = RecoveryLedger::new();
        let old = ("bob".to_string(), 7u32, "TR_OLD".to_string());
        let new = ("bob".to_string(), 7u32, "TR_NEW".to_string());
        let now = Instant::now();
        assert_eq!(ledger.attempted_for(&old, now), None);
        ledger.record_attempt(old.clone(), now);
        assert!(ledger.attempted_for(&old, now).is_some());
        // Same window, new SID (a republish): its own clock, immediately
        // available -- the 2026-07-30 hole was the window-keyed ledger denying
        // every republished SID the attempt its predecessor had spent.
        assert_eq!(ledger.attempted_for(&new, now), None);

        // Still demand-divergent -> stays spent.
        ledger.retain_attempts(&HashSet::from([old.clone()]));
        assert!(ledger.attempted_for(&old, now).is_some());

        // No longer demand-divergent (recovered or vanished) -> clock dropped.
        ledger.retain_attempts(&HashSet::new());
        assert_eq!(ledger.attempted_for(&old, now), None);
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn under_republish_churn_every_new_sid_is_attempted_rather_than_waited_out() {
        // The live 2026-07-30 failure shape driven through the real pass
        // chain: the sender republishes every ~8s, so each pass sees the same
        // window under a NEW, unsubscribed SID. Each one must get demand.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();
        for (index, sid) in ["TR_1", "TR_2", "TR_3"].iter().enumerate() {
            let outcome = pass(
                &[publication("bob", 7, sid, false)],
                &[],
                &mut ledger,
                start + Duration::from_secs(8 * index as u64),
            );
            assert_eq!(
                steps_for(&outcome),
                vec![RecoveryStep::Attempt],
                "republish {index} ({sid}) must be attempted, not waited out on a dead SID's clock"
            );
        }
    }

    #[test]
    fn an_unrecovered_sid_re_arms_after_the_deadline_instead_of_reporting_truth() {
        // Same SID stays unsubscribed across passes: demand is re-expressed at
        // the deadline cadence forever -- never terminal while the publication
        // exists.
        let mut ledger = RecoveryLedger::new();
        let start = Instant::now();
        let lost = [publication("bob", 7, "TR_A", false)];
        let tiles = [tracked("bob", 7, "TR_A")];

        assert_eq!(
            steps_for(&pass(&lost, &tiles, &mut ledger, start)),
            vec![RecoveryStep::Attempt]
        );
        // record_attempt is the driver's job; plan alone reads clocks.
        ledger.record_attempt(("bob".to_string(), 7, "TR_A".to_string()), start);
        assert_eq!(
            steps_for(&pass(
                &lost,
                &tiles,
                &mut ledger,
                start + RECOVERY_DEADLINE - Duration::from_secs(1)
            )),
            vec![RecoveryStep::Wait]
        );
        assert_eq!(
            steps_for(&pass(&lost, &tiles, &mut ledger, start + RECOVERY_DEADLINE)),
            vec![RecoveryStep::Attempt],
            "the attempt re-arms at the deadline; NotSubscribed is never terminal"
        );
    }
}
