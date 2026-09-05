// Receiver-side publication reconciliation (#298) — the browser mirror of the
// desktop contract in `apps/desktop/src-tauri/src/transport/reconcile.rs`.
// The divergence names, the one-attempt bound, and the terminal "report the
// truth" decision table are deliberately identical on both receivers —
// including the rule that an `orphaned` verdict must persist past
// ORPHANED_GRACE_MS (two consecutive passes) before it retires anything, so a
// pass landing mid-republish cannot tear down a healthy tile (#627 here, #630
// native).
//
// CONVERGENCE CONTRACT (the 2026-07-30 live-session fix): as long as a
// shared-window publication exists in the room, the viewer must converge on
// rendering it — it must never be left permanently blank. The pass is
// authoritative (it diffs the SDK's live publication list against the tiles
// actually rendering), and every divergence now has a converging action:
//
//   * `not-subscribed` (demand off) — re-express demand with
//     `setSubscribed(true)`. Attempts are keyed by SID and RE-ARM after
//     `RECOVERY_DEADLINE_MS`: a sender that republishes every ~8s mints a new
//     SID each cycle, and the old "one attempt per window per session" ledger
//     spent its only attempt on a SID that was already dead — every later
//     republish was then denied recovery forever (the 2026-07-30 blank-viewer
//     session). A publication that still exists always justifies demand.
//   * `not-receiving` / `replaced` with a subscribed track — the SDK holds the
//     live track but no tile is rendering it (or a tile is bound to a dead
//     SID). The SDK will not re-emit `TrackSubscribed` for a track it already
//     holds, so the pass ATTACHES the held track through the real
//     `addShareTile` path (`attachTrack` action). This cannot double-render:
//     the tile registry keys one tile per (owner, window) and
//     `attachVideoTrackIfChanged` is idempotent.
//   * `orphaned` (no publication at all) — unchanged: survive
//     ORPHANED_GRACE_MS (two consecutive passes, #627/#630), then retire.
//
// Retiring a tile is now EXCLUSIVELY the orphan verdict's job. The previous
// table retired for expired `not-subscribed` and for `not-receiving` too,
// which destroyed tiles while a live publication existed and (because the
// retire targeted a tile that often did not exist) repaired nothing — the
// viewer stayed blank for the rest of the session (#634 is closed by this).
//
// As on the desktop side, this is strictly an AFTER-THE-FACT repair. It never
// runs as part of the join path — LiveKit dispatches `TrackSubscribed` for
// already-published tracks, and a pass racing that would attach a track that
// is arriving normally (FIRST_PASS_GRACE_MS keeps it clear).
//
// `setSubscribed(false)` is deliberately NEVER called: measured against a real
// SFU on the Rust side (`examples/publication_reconcile_probe`), unsubscribing
// clears the SDK's track handle and re-subscribing resumes media on the
// existing transceiver WITHOUT re-emitting `TrackSubscribed`, leaving the
// publication permanently reporting itself unsubscribed while frames flow —
// see `transport/reconcile.rs`'s module doc, hazard 1b.

import { isAiTrackName } from './trackNames.ts';

/** One shared-window publication as the SDK reports it right now. */
export interface DiscoveredWindowPublication {
  identity: string;
  windowId: number;
  trackSid: string;
  subscribed: boolean;
}

/** The receiver's local picture: one live share tile. */
export interface TrackedShareWindow {
  identity: string;
  windowId: number;
  trackSid: string;
}

export type Divergence =
  /**
   * The SDK holds the track, but no tile is rendering it. The SDK will not
   * re-emit `TrackSubscribed` for a track it already holds, so the repair is
   * to attach the held track directly through the real tile path.
   */
  | { kind: 'not-receiving' }
  /**
   * The SDK holds no track for this publication, so nothing can be receiving
   * it. Repaired by re-expressing demand (`setSubscribed(true)`).
   */
  | { kind: 'not-subscribed' }
  /**
   * The tile is bound to a superseded sid. `subscribed` reports the
   * REPLACEMENT publication's demand state: a subscribed replacement can be
   * attached immediately, an unsubscribed one needs demand first — the old
   * table returned a bookkeeping-only step for both, so a republish whose
   * `TrackSubscribed` was lost left the tile on the dead sid forever.
   */
  | { kind: 'replaced'; from: string; to: string; subscribed: boolean }
  /** A tile claiming a share the SFU no longer holds. */
  | { kind: 'orphaned' };

export interface ReconcileFinding {
  identity: string;
  windowId: number;
  /** The authoritative sid, when the publication still exists. */
  trackSid: string | null;
  divergence: Divergence;
}

/**
 * `attach` binds the SDK-held track to a tile through the real tile path.
 * `attempt` re-expresses subscription demand. `wait` is deliberately distinct
 * from both: a sid that has spent its attempt and is inside the deadline must
 * change nothing, rather than record state that would make the next pass read
 * it as healthy. `report-truth` (retire) is exclusively the orphan verdict.
 */
export type RecoveryStep = 'attach' | 'wait' | 'attempt' | 'report-truth';

/**
 * How long a demand attempt has to land before it is RE-ARMED. Not a terminal
 * deadline: while the publication still exists, demand stays legitimate, so
 * the pass keeps re-attempting at this cadence instead of ever giving up.
 */
export const RECOVERY_DEADLINE_MS = 10_000;

/**
 * How long a tile's window id must have had NO publication at all before the
 * orphan verdict is trusted. A sender-side republish briefly leaves no
 * publication for the id (old track unpublished, replacement not yet
 * announced); a single pass landing inside that swap must read as in-flight,
 * not as a share that ended (#627). Mirrors the intent of tiles.ts's 1500 ms
 * SHARE_REPLACEMENT_GRACE_MS, which is why `replaced` never had this problem.
 * With passes every RECONCILE_INTERVAL_MS this means an orphan retires on its
 * second consecutive sighting, never its first. "Consecutive" is enforced by
 * RecoveryLedger.retainOrphanSightings — a clock never outlives the tile
 * lifecycle it was started for.
 */
export const ORPHANED_GRACE_MS = 1_500;

/** How often the reconciliation pass runs once a session is connected. */
export const RECONCILE_INTERVAL_MS = 5_000;

/**
 * How long after connect the first pass may run. Keeps this strictly an
 * after-the-fact repair, well clear of normal join-time `TrackSubscribed`
 * delivery and of the 1500 ms replacement lease in `tiles.ts`.
 */
export const FIRST_PASS_GRACE_MS = 15_000;

const WINDOW_TRACK_PREFIX = 'petal-window-';

function windowIdFromTrackName(trackName: string | undefined | null): number | null {
  // #657: `petal-ai-window-<id>` names a window too, but it is the assistant's
  // AUDIO, not a shared-window video publication. Rejected explicitly so it
  // can never be reconciled into a share tile. (It also fails the prefix test
  // below -- this is belt and braces on the one function that turns a track
  // name into a renderable window.)
  if (isAiTrackName(trackName)) return null;
  if (!trackName?.startsWith(WINDOW_TRACK_PREFIX)) return null;
  const raw = trackName.slice(WINDOW_TRACK_PREFIX.length);
  if (!/^\d+$/.test(raw)) return null;
  const id = Number(raw);
  if (!Number.isSafeInteger(id) || id < 1 || id > 0xffff_ffff) return null;
  return id;
}

/** Minimal structural view of the SDK room, so this is testable without one. */
export interface RoomLike {
  remoteParticipants: Map<
    string,
    {
      identity: string;
      trackPublications: Map<
        string,
        {
          trackSid: string;
          trackName?: string;
          kind?: string;
          isSubscribed?: boolean;
        }
      >;
    }
  >;
}

/**
 * The authoritative answer to "which shared-window publications exist right
 * now", read off the SDK's own live room object rather than an event replay.
 * The mirror of Rust's `discover_window_publications(&Room)`.
 */
export function discoverWindowPublications(room: RoomLike): DiscoveredWindowPublication[] {
  const found: DiscoveredWindowPublication[] = [];
  for (const participant of room.remoteParticipants.values()) {
    for (const publication of participant.trackPublications.values()) {
      if (publication.kind !== undefined && publication.kind !== 'video') continue;
      const windowId = windowIdFromTrackName(publication.trackName);
      if (windowId === null) continue;
      found.push({
        identity: participant.identity,
        windowId,
        trackSid: publication.trackSid,
        subscribed: publication.isSubscribed === true,
      });
    }
  }
  found.sort(
    (a, b) =>
      a.identity.localeCompare(b.identity) ||
      a.windowId - b.windowId ||
      a.trackSid.localeCompare(b.trackSid)
  );
  return found;
}

function key(identity: string, windowId: number): string {
  return `${identity}:${windowId}`;
}

/**
 * Orphan sightings are keyed by the exact tile, sid included — a different sid
 * is a different share, so its grace must start over even if the same peer and
 * window id orphaned moments earlier.
 */
function orphanKey(identity: string, windowId: number, trackSid: string): string {
  return `${identity}:${windowId}:${trackSid}`;
}

/** Diff authoritative publications against the tiles actually rendering. */
export function reconcileWindowPublications(
  discovered: readonly DiscoveredWindowPublication[],
  tracked: readonly TrackedShareWindow[]
): ReconcileFinding[] {
  const findings: ReconcileFinding[] = [];
  const seen = new Set<string>();
  const trackedByKey = new Map<string, TrackedShareWindow>();
  for (const entry of tracked) trackedByKey.set(key(entry.identity, entry.windowId), entry);

  for (const publication of discovered) {
    const k = key(publication.identity, publication.windowId);
    // One participant publishing two live tracks for one window id is not a
    // state a single tile can represent; reconcile the first, leave the rest.
    if (seen.has(k)) continue;
    seen.add(k);
    const local = trackedByKey.get(k);
    let divergence: Divergence | null = null;
    if (!local) {
      divergence = publication.subscribed ? { kind: 'not-receiving' } : { kind: 'not-subscribed' };
    } else if (local.trackSid === publication.trackSid) {
      divergence = publication.subscribed ? null : { kind: 'not-subscribed' };
    } else {
      divergence = {
        kind: 'replaced',
        from: local.trackSid,
        to: publication.trackSid,
        subscribed: publication.subscribed,
      };
    }
    if (divergence) {
      findings.push({
        identity: publication.identity,
        windowId: publication.windowId,
        trackSid: publication.trackSid,
        divergence,
      });
    }
  }

  for (const entry of tracked) {
    if (seen.has(key(entry.identity, entry.windowId))) continue;
    findings.push({
      identity: entry.identity,
      windowId: entry.windowId,
      trackSid: null,
      divergence: { kind: 'orphaned' },
    });
  }

  findings.sort(
    (a, b) => a.identity.localeCompare(b.identity) || a.windowId - b.windowId
  );
  return findings;
}

/**
 * The bounded-recovery decision table. `attemptedForMs` is the time since this
 * key spent its single permitted attempt, or null if it has not.
 */
export function recoveryStep(
  divergence: Divergence,
  attemptedForMs: number | null,
  orphanedForMs: number | null = null
): RecoveryStep {
  // Demand attempts are bounded in RATE, not in count: one call per sid per
  // RECOVERY_DEADLINE_MS, re-armed for as long as the publication exists.
  // Terminally giving up here is what left the 2026-07-30 session blank.
  const demandStep = (): RecoveryStep => {
    if (attemptedForMs === null) return 'attempt';
    return attemptedForMs < RECOVERY_DEADLINE_MS ? 'wait' : 'attempt';
  };
  switch (divergence.kind) {
    // The SFU says this share is gone. Continuing to display it is the lie
    // #298 is about — but only once the absence has outlived a republish
    // in flight. One sample taken mid-swap retires a healthy share (#627).
    case 'orphaned':
      if (orphanedForMs === null || orphanedForMs < ORPHANED_GRACE_MS) return 'wait';
      return 'report-truth';
    // A subscribed replacement's track is already in the SDK's hands; attach
    // it through the real tile path (idempotent — if the replacement's own
    // `TrackSubscribed` also landed, the tile is already bound to the new sid
    // and this finding never exists). An unsubscribed replacement first needs
    // demand, exactly like `not-subscribed` — the old table returned a
    // bookkeeping no-op for both, so a republish that lost its
    // `TrackSubscribed` left the tile bound to the dead sid forever.
    case 'replaced':
      return divergence.subscribed ? 'attach' : demandStep();
    // The SDK holds the track but nothing is rendering it. It will not
    // re-emit `TrackSubscribed` for a track it already holds, so attach the
    // held track directly rather than retiring a tile that mostly does not
    // even exist (the old behavior — which repaired nothing, forever).
    case 'not-receiving':
      return 'attach';
    case 'not-subscribed':
      return demandStep();
  }
}

/**
 * Per-session record of demand attempts.
 *
 * Attempts are keyed by the exact publication — sid included — not by
 * (identity, windowId). A sender-side republish mints a NEW sid for the same
 * window every cycle; under the window-keyed ledger the single permitted
 * attempt was spent on a sid that died seconds later, and every subsequent
 * republish of that window was then denied recovery for the rest of the
 * session (the 2026-07-30 permanently-blank viewer). A new sid is a new
 * publication and always deserves its own attempt clock.
 */
export class RecoveryLedger {
  private readonly attempts = new Map<string, number>();
  private readonly orphanSightings = new Map<string, number>();

  attemptedFor(identity: string, windowId: number, trackSid: string, now: number): number | null {
    const at = this.attempts.get(orphanKey(identity, windowId, trackSid));
    return at === undefined ? null : Math.max(0, now - at);
  }

  recordAttempt(identity: string, windowId: number, trackSid: string, now: number): void {
    this.attempts.set(orphanKey(identity, windowId, trackSid), now);
  }

  /**
   * Drops every attempt clock for a publication that is no longer both
   * present and demand-divergent — a key that reconciles clean gets its
   * budget back, and a sid that vanished can never be measured again.
   */
  retainAttempts(demandDivergentNow: ReadonlySet<string>): void {
    for (const k of this.attempts.keys()) {
      if (!demandDivergentNow.has(k)) this.attempts.delete(k);
    }
  }

  /**
   * Ms since this exact tile was first seen orphaned in the CURRENT unbroken
   * run of orphaned passes, or null if this pass is the first of a run.
   */
  orphanedFor(identity: string, windowId: number, trackSid: string, now: number): number | null {
    const at = this.orphanSightings.get(orphanKey(identity, windowId, trackSid));
    return at === undefined ? null : Math.max(0, now - at);
  }

  /** Records the FIRST sighting of a run only — the clock never restarts. */
  recordOrphanSighting(identity: string, windowId: number, trackSid: string, now: number): void {
    const k = orphanKey(identity, windowId, trackSid);
    if (!this.orphanSightings.has(k)) this.orphanSightings.set(k, now);
  }

  /**
   * Drops every sighting for a tile not orphaned in the pass just evaluated, so
   * the grace measures CONSECUTIVE passes rather than "orphaned once, ever".
   *
   * This is the lifecycle guard, not just bookkeeping. A sighting that outlives
   * the tile it described — the publication returns, or the tile is torn down
   * through the normal `TrackUnsubscribed` path — would otherwise still be
   * sitting in the map when that window id next orphans, and its long-expired
   * clock would retire the new tile on its FIRST sighting: exactly the
   * single-sample teardown this grace exists to prevent. The sid in the key is
   * the second half of that guarantee, covering a replacement tile that
   * appeared and orphaned entirely between two passes, where the run of
   * orphaned passes looks unbroken but the share underneath it changed.
   */
  retainOrphanSightings(orphanedNow: ReadonlySet<string>): void {
    for (const k of this.orphanSightings.keys()) {
      if (!orphanedNow.has(k)) this.orphanSightings.delete(k);
    }
  }

  /** Test/observability hook: how many orphan clocks are currently running. */
  get orphanSightingCount(): number {
    return this.orphanSightings.size;
  }

  get size(): number {
    return this.attempts.size;
  }
}

/** Everything a pass needs to act, kept injectable so it is testable. */
export interface ReconcileActions {
  /** Express subscription demand for one publication — the demand lever. */
  setSubscribed(identity: string, trackSid: string, subscribed: boolean): void;
  /**
   * Bind the SDK-held track for this publication to its tile through the real
   * `addShareTile` path. The lever for a track the SDK already holds — the SDK
   * will not re-emit `TrackSubscribed` for it, so waiting on events cannot
   * converge. Must be idempotent (the tile path already is).
   */
  attachTrack(identity: string, trackSid: string): void;
  /** Stop asserting a share: tear the tile down. Orphan verdicts only. */
  retireTile(identity: string, trackSid: string): void;
  log(message: string): void;
}

/**
 * One authoritative reconciliation pass.
 *
 * Returns the findings it acted on, so callers (and tests) can assert on the
 * decisions rather than only their side effects.
 */
export function runReconciliationPass(
  discovered: readonly DiscoveredWindowPublication[],
  tracked: readonly TrackedShareWindow[],
  ledger: RecoveryLedger,
  actions: ReconcileActions,
  now: number
): ReconcileFinding[] {
  const findings = reconcileWindowPublications(discovered, tracked);
  // Attempt clocks live exactly as long as their sid stays demand-divergent:
  // a key that reconciles clean gets its budget back, and a republish's new
  // sid starts with a fresh clock instead of inheriting a dead sid's.
  const demandDivergentNow = new Set<string>();
  for (const finding of findings) {
    const demandDivergent =
      finding.divergence.kind === 'not-subscribed' ||
      (finding.divergence.kind === 'replaced' && !finding.divergence.subscribed);
    if (demandDivergent && finding.trackSid) {
      demandDivergentNow.add(orphanKey(finding.identity, finding.windowId, finding.trackSid));
    }
  }
  ledger.retainAttempts(demandDivergentNow);
  // An orphaned finding carries no sid (there is no publication to name one),
  // so the tile's own sid is what identifies the share whose grace is running.
  const orphanSidFor = (finding: ReconcileFinding): string | undefined =>
    tracked.find((t) => t.identity === finding.identity && t.windowId === finding.windowId)
      ?.trackSid;

  // Prune before reading any clock: this pass's orphans keep their sighting,
  // every other tile loses one it can no longer legitimately be measuring.
  const orphanedNow = new Set<string>();
  for (const finding of findings) {
    if (finding.divergence.kind !== 'orphaned') continue;
    const sid = orphanSidFor(finding);
    if (sid) orphanedNow.add(orphanKey(finding.identity, finding.windowId, sid));
  }
  ledger.retainOrphanSightings(orphanedNow);

  for (const finding of findings) {
    let orphanedForMs: number | null = null;
    if (finding.divergence.kind === 'orphaned') {
      const sid = orphanSidFor(finding);
      if (sid) {
        orphanedForMs = ledger.orphanedFor(finding.identity, finding.windowId, sid, now);
        ledger.recordOrphanSighting(finding.identity, finding.windowId, sid, now);
      }
    }
    const step = recoveryStep(
      finding.divergence,
      finding.trackSid === null
        ? null
        : ledger.attemptedFor(finding.identity, finding.windowId, finding.trackSid, now),
      orphanedForMs
    );
    const where = `window ${finding.windowId} from ${finding.identity}`;
    if (step === 'wait' && orphanedForMs === null && finding.divergence.kind === 'orphaned') {
      actions.log(
        `reconcile: ${where} has no publication; holding for a republish in flight before retiring`
      );
    }
    if (step === 'wait') continue;
    if (step === 'attach' && finding.trackSid) {
      actions.log(
        `reconcile: ${where} diverged (${finding.divergence.kind}); attaching the SDK-held track ${finding.trackSid}`
      );
      actions.attachTrack(finding.identity, finding.trackSid);
      continue;
    }
    if (step === 'attempt' && finding.trackSid) {
      ledger.recordAttempt(finding.identity, finding.windowId, finding.trackSid, now);
      actions.log(
        `reconcile: ${where} diverged (${finding.divergence.kind}); re-expressing subscription demand for ${finding.trackSid}`
      );
      // `setSubscribed(true)` only expresses this subscriber's demand — it can
      // never create a duplicate publication, and recovery lands back through
      // the real `TrackSubscribed` handler rather than attaching a track
      // directly. There is deliberately no `setSubscribed(false)`.
      actions.setSubscribed(finding.identity, finding.trackSid, true);
      continue;
    }
    if (step === 'report-truth') {
      actions.log(
        `reconcile: ${where} terminal (${finding.divergence.kind}); the SFU holds no publication, retiring`
      );
      const sid =
        finding.trackSid ??
        tracked.find((t) => t.identity === finding.identity && t.windowId === finding.windowId)
          ?.trackSid;
      if (sid) actions.retireTile(finding.identity, sid);
    }
  }

  return findings;
}
