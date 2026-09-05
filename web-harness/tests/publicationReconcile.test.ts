import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  discoverWindowPublications,
  reconcileWindowPublications,
  recoveryStep,
  runReconciliationPass,
  RecoveryLedger,
  ORPHANED_GRACE_MS,
  RECONCILE_INTERVAL_MS,
  RECOVERY_DEADLINE_MS,
  type DiscoveredWindowPublication,
  type ReconcileActions,
  type RoomLike,
  type TrackedShareWindow,
} from '../src/publicationReconcile';

function room(
  publications: Array<{
    identity: string;
    trackSid: string;
    trackName: string;
    kind?: string;
    isSubscribed?: boolean;
  }>
): RoomLike {
  const byIdentity = new Map<string, ReturnType<typeof participantFor>>();
  for (const publication of publications) {
    const existing = byIdentity.get(publication.identity) ?? participantFor(publication.identity);
    existing.trackPublications.set(publication.trackSid, {
      trackSid: publication.trackSid,
      trackName: publication.trackName,
      kind: publication.kind ?? 'video',
      isSubscribed: publication.isSubscribed ?? true,
    });
    byIdentity.set(publication.identity, existing);
  }
  return { remoteParticipants: byIdentity };
}

function participantFor(identity: string) {
  return {
    identity,
    trackPublications: new Map<
      string,
      { trackSid: string; trackName?: string; kind?: string; isSubscribed?: boolean }
    >(),
  };
}

function publication(
  identity: string,
  windowId: number,
  trackSid: string,
  subscribed = true
): DiscoveredWindowPublication {
  return { identity, windowId, trackSid, subscribed };
}

function tracked(identity: string, windowId: number, trackSid: string): TrackedShareWindow {
  return { identity, windowId, trackSid };
}

function recordingActions() {
  const subscribed: Array<[string, string, boolean]> = [];
  const attached: Array<[string, string]> = [];
  const retired: Array<[string, string]> = [];
  const logs: string[] = [];
  const actions: ReconcileActions = {
    setSubscribed: (identity, trackSid, value) => subscribed.push([identity, trackSid, value]),
    attachTrack: (identity, trackSid) => attached.push([identity, trackSid]),
    retireTile: (identity, trackSid) => retired.push([identity, trackSid]),
    log: (message) => logs.push(message),
  };
  return { actions, subscribed, attached, retired, logs };
}

test('discoverWindowPublications: reads shared-window publications straight off the room, not an event replay', () => {
  const discovered = discoverWindowPublications(
    room([
      { identity: 'bob', trackSid: 'TR_A', trackName: 'petal-window-7' },
      { identity: 'bob', trackSid: 'TR_CAM', trackName: 'petal-camera-bob' },
      { identity: 'carol', trackSid: 'TR_B', trackName: 'petal-window-9', isSubscribed: false },
    ])
  );
  assert.deepEqual(discovered, [
    publication('bob', 7, 'TR_A'),
    publication('carol', 9, 'TR_B', false),
  ]);
});

test('discoverWindowPublications: ignores audio publications and unparseable track names', () => {
  const discovered = discoverWindowPublications(
    room([
      { identity: 'bob', trackSid: 'TR_MIC', trackName: 'petal-window-7', kind: 'audio' },
      { identity: 'bob', trackSid: 'TR_X', trackName: 'petal-window-notanumber' },
      { identity: 'bob', trackSid: 'TR_Y', trackName: 'petal-window-0' },
    ])
  );
  assert.deepEqual(discovered, []);
});

test('reconcileWindowPublications: reports nothing when a subscribed publication has a matching tile', () => {
  assert.deepEqual(
    reconcileWindowPublications([publication('bob', 7, 'TR_A')], [tracked('bob', 7, 'TR_A')]),
    []
  );
});

test('reconcileWindowPublications: names a live publication with no tile as not-receiving — the reported symptom', () => {
  const findings = reconcileWindowPublications([publication('bob', 7, 'TR_A')], []);
  assert.equal((findings).length, 1);
  assert.deepEqual(findings[0].divergence, { kind: 'not-receiving' });
  assert.equal(findings[0].trackSid, 'TR_A');
});

test('reconcileWindowPublications: names lost subscription demand as not-subscribed', () => {
  const findings = reconcileWindowPublications(
    [publication('bob', 7, 'TR_A', false)],
    [tracked('bob', 7, 'TR_A')]
  );
  assert.deepEqual(findings[0].divergence, { kind: 'not-subscribed' });
});

test('reconcileWindowPublications: names a tile with no backing publication as orphaned', () => {
  // The `TrackUnpublished`-without-`TrackUnsubscribed` gap: nothing else in
  // the web receiver ever removes this tile.
  const findings = reconcileWindowPublications([], [tracked('bob', 7, 'TR_A')]);
  assert.deepEqual(findings, [
    { identity: 'bob', windowId: 7, trackSid: null, divergence: { kind: 'orphaned' } },
  ]);
});

test('reconcileWindowPublications: names a superseded sid as a replacement carrying both identities', () => {
  const findings = reconcileWindowPublications(
    [publication('bob', 7, 'TR_NEW')],
    [tracked('bob', 7, 'TR_OLD')]
  );
  assert.deepEqual(findings[0].divergence, {
    kind: 'replaced',
    from: 'TR_OLD',
    to: 'TR_NEW',
    subscribed: true,
  });
});

test('reconcileWindowPublications: a replacement carries the NEW publication demand state', () => {
  // A republish whose TrackSubscribed was lost can leave the replacement
  // unsubscribed; the decision table needs that fact to choose demand first.
  const findings = reconcileWindowPublications(
    [publication('bob', 7, 'TR_NEW', false)],
    [tracked('bob', 7, 'TR_OLD')]
  );
  assert.deepEqual(findings[0].divergence, {
    kind: 'replaced',
    from: 'TR_OLD',
    to: 'TR_NEW',
    subscribed: false,
  });
});

test('reconcileWindowPublications: keeps the same window id from two owners separate', () => {
  const findings = reconcileWindowPublications(
    [publication('bob', 7, 'TR_A'), publication('carol', 7, 'TR_B')],
    [tracked('bob', 7, 'TR_A')]
  );
  assert.equal((findings).length, 1);
  assert.equal(findings[0].identity, 'carol');
});

test('recoveryStep: holds a fresh orphan for the grace window, then reports the truth without spending an attempt', () => {
  // A sender-side republish briefly leaves NO publication for the window id;
  // a single sample taken mid-swap must not retire the tile (#627).
  assert.equal(recoveryStep({ kind: 'orphaned' }, null, null), 'wait');
  assert.equal(recoveryStep({ kind: 'orphaned' }, null, ORPHANED_GRACE_MS - 1), 'wait');
  assert.equal(recoveryStep({ kind: 'orphaned' }, null, ORPHANED_GRACE_MS), 'report-truth');
  // The attempt clock never buys an orphan anything.
  assert.equal(recoveryStep({ kind: 'orphaned' }, RECOVERY_DEADLINE_MS * 10, null), 'wait');
});

test('recoveryStep: attaches a subscribed replacement and never retires a live tile for it', () => {
  // The SDK holds the replacement track and will not re-emit TrackSubscribed
  // for it — attaching through the real tile path is the only convergence.
  const divergence = { kind: 'replaced', from: 'TR_OLD', to: 'TR_NEW', subscribed: true } as const;
  assert.equal(recoveryStep(divergence, null), 'attach');
  assert.equal(recoveryStep(divergence, RECOVERY_DEADLINE_MS * 10), 'attach');
});

test('recoveryStep: an unsubscribed replacement gets demand first, on the attempt cadence', () => {
  // The 2026-07-30 hole: this used to be a bookkeeping no-op, so a republish
  // that lost demand left the tile bound to the dead sid forever.
  const divergence = { kind: 'replaced', from: 'TR_OLD', to: 'TR_NEW', subscribed: false } as const;
  assert.equal(recoveryStep(divergence, null), 'attempt');
  assert.equal(recoveryStep(divergence, 1_000), 'wait');
  assert.equal(recoveryStep(divergence, RECOVERY_DEADLINE_MS), 'attempt');
});

test('recoveryStep: lost demand re-arms after the deadline instead of terminally giving up', () => {
  // While the publication exists demand stays legitimate; giving up after one
  // attempt is what left the 2026-07-30 session permanently blank.
  const divergence = { kind: 'not-subscribed' } as const;
  assert.equal(recoveryStep(divergence, null), 'attempt');
  assert.equal(recoveryStep(divergence, 1_000), 'wait');
  assert.equal(recoveryStep(divergence, RECOVERY_DEADLINE_MS), 'attempt');
  assert.equal(recoveryStep(divergence, RECOVERY_DEADLINE_MS * 100), 'attempt');
});

test('recoveryStep: report-truth is exclusively the orphan verdict', () => {
  // Retiring while a publication exists destroys a share the viewer could
  // still converge on. Only "the SFU holds nothing" justifies teardown.
  for (const divergence of [
    { kind: 'not-receiving' },
    { kind: 'not-subscribed' },
    { kind: 'replaced', from: 'a', to: 'b', subscribed: true },
    { kind: 'replaced', from: 'a', to: 'b', subscribed: false },
  ] as const) {
    for (const attempted of [null, 1_000, RECOVERY_DEADLINE_MS, RECOVERY_DEADLINE_MS * 100]) {
      assert.notEqual(recoveryStep(divergence, attempted), 'report-truth');
    }
  }
  assert.equal(recoveryStep({ kind: 'orphaned' }, null, ORPHANED_GRACE_MS), 'report-truth');
});

test('recoveryStep: attaches a track the SDK already holds instead of faking or retiring', () => {
  // Measured on the Rust side: the SDK will not re-emit TrackSubscribed for
  // a track it holds. The old table retired here — which repaired nothing
  // (the tile usually did not exist) and left the viewer blank forever.
  for (const attempted of [null, 0, RECOVERY_DEADLINE_MS * 100]) {
    assert.equal(recoveryStep({ kind: 'not-receiving' }, attempted), 'attach');
  }
});

test('RecoveryLedger: attempts are keyed by sid — a republished track gets a fresh attempt', () => {
  const ledger = new RecoveryLedger();
  assert.equal(ledger.attemptedFor('bob', 7, 'TR_A', 1_000), null);
  ledger.recordAttempt('bob', 7, 'TR_A', 1_000);
  assert.equal(ledger.attemptedFor('bob', 7, 'TR_A', 3_000), 2_000);
  // Same window, new sid (a republish): its own clock, immediately available.
  assert.equal(ledger.attemptedFor('bob', 7, 'TR_B', 3_000), null);

  // A sid that is no longer demand-divergent loses its clock.
  ledger.retainAttempts(new Set(['bob:7:TR_A']));
  assert.equal(ledger.attemptedFor('bob', 7, 'TR_A', 3_000), 2_000);
  ledger.retainAttempts(new Set());
  assert.equal(ledger.attemptedFor('bob', 7, 'TR_A', 3_000), null);
  assert.equal(ledger.size, 0);
});

test('runReconciliationPass: retires a tile the SFU has no publication for — but only on a second consecutive sighting', () => {
  const { actions, retired, subscribed } = recordingActions();
  const ledger = new RecoveryLedger();
  const tiles = [tracked('bob', 7, 'TR_A')];

  // First sighting: could be a republish in flight. Hold.
  runReconciliationPass([], tiles, ledger, actions, 0);
  assert.deepEqual(retired, []);

  // Still absent one interval later: the share really ended.
  runReconciliationPass([], tiles, ledger, actions, RECONCILE_INTERVAL_MS);
  assert.deepEqual(retired, [['bob', 'TR_A']]);
  assert.deepEqual(subscribed, []);
});

test('runReconciliationPass: an orphan sighting inside the grace window never retires, even back-to-back', () => {
  const { actions, retired } = recordingActions();
  const ledger = new RecoveryLedger();
  const tiles = [tracked('bob', 7, 'TR_A')];
  runReconciliationPass([], tiles, ledger, actions, 0);
  runReconciliationPass([], tiles, ledger, actions, ORPHANED_GRACE_MS - 1);
  assert.deepEqual(retired, []);
});

test('runReconciliationPass: a republish landing between passes is attached, not torn down', () => {
  // The #627 amplifier: pass 1 samples the gap between unpublish and the
  // replacement's announcement. The replacement then lands, so pass 2 must
  // bind the tile to the new track instead of retiring it.
  const { actions, retired, subscribed, attached } = recordingActions();
  const ledger = new RecoveryLedger();
  const tiles = [tracked('bob', 7, 'TR_OLD')];

  runReconciliationPass([], tiles, ledger, actions, 0);
  assert.deepEqual(retired, []);

  runReconciliationPass(
    [publication('bob', 7, 'TR_NEW')],
    tiles,
    ledger,
    actions,
    RECONCILE_INTERVAL_MS
  );
  assert.deepEqual(retired, []);
  assert.deepEqual(subscribed, []);
  assert.deepEqual(attached, [['bob', 'TR_NEW']]);
});

test('runReconciliationPass: a reappearing publication resets the orphan clock for a later genuine end', () => {
  const { actions, retired } = recordingActions();
  const ledger = new RecoveryLedger();
  const tiles = [tracked('bob', 7, 'TR_A')];

  // Orphan sighting, then the publication comes back before the next pass.
  runReconciliationPass([], tiles, ledger, actions, 0);
  runReconciliationPass([publication('bob', 7, 'TR_A')], tiles, ledger, actions, 5_000);
  assert.deepEqual(retired, []);

  // Much later the share genuinely ends: the stale sighting from t=0 must not
  // make the first new sample look grace-expired.
  runReconciliationPass([], tiles, ledger, actions, 100_000);
  assert.deepEqual(retired, []);
  runReconciliationPass([], tiles, ledger, actions, 100_000 + RECONCILE_INTERVAL_MS);
  assert.deepEqual(retired, [['bob', 'TR_A']]);
});

test('RecoveryLedger: an orphan clock never outlives the tile lifecycle it was started for', () => {
  // The lifecycle hole this guard would otherwise have: a sighting recorded
  // for a tile that then goes away through the normal TrackUnsubscribed path
  // must not still be running when that window id next orphans — a long-
  // expired clock would retire the NEW tile on its first sighting, which is
  // the single-sample teardown the grace exists to prevent.
  const ledger = new RecoveryLedger();
  ledger.recordOrphanSighting('bob', 7, 'TR_A', 0);
  assert.equal(ledger.orphanSightingCount, 1);

  // A pass in which nothing is orphaned retires the clock with the lifecycle.
  ledger.retainOrphanSightings(new Set());
  assert.equal(ledger.orphanSightingCount, 0);
  assert.equal(ledger.orphanedFor('bob', 7, 'TR_A', 100_000), null);
});

test('RecoveryLedger: a new sid for the same window starts its own grace clock', () => {
  // A replacement tile that appeared and orphaned entirely between two passes
  // leaves the run of orphaned passes looking unbroken, so the sid is what
  // distinguishes the new share from the one whose clock already expired.
  const ledger = new RecoveryLedger();
  ledger.recordOrphanSighting('bob', 7, 'TR_A', 0);
  assert.equal(ledger.orphanedFor('bob', 7, 'TR_A', 100_000), 100_000);
  assert.equal(ledger.orphanedFor('bob', 7, 'TR_B', 100_000), null);
});

test('runReconciliationPass: a tile removed by the normal path cannot leave a stale clock that retires its successor', () => {
  const { actions, retired } = recordingActions();
  const ledger = new RecoveryLedger();

  // Sighted orphaned once, then the tile disappears via TrackUnsubscribed —
  // so it is no longer tracked and no publication is ever discovered.
  runReconciliationPass([], [tracked('bob', 7, 'TR_A')], ledger, actions, 0);
  runReconciliationPass([], [], ledger, actions, RECONCILE_INTERVAL_MS);
  assert.deepEqual(retired, []);

  // Much later bob re-shares window 7 and it orphans: the first sighting of
  // the NEW tile must hold, not inherit the old tile's expired clock.
  runReconciliationPass([], [tracked('bob', 7, 'TR_B')], ledger, actions, 100_000);
  assert.deepEqual(retired, []);
  runReconciliationPass(
    [],
    [tracked('bob', 7, 'TR_B')],
    ledger,
    actions,
    100_000 + RECONCILE_INTERVAL_MS
  );
  assert.deepEqual(retired, [['bob', 'TR_B']]);
});

test('runReconciliationPass: retiring an orphan clears its sighting, so a re-share gets a fresh grace window', () => {
  const { actions, retired } = recordingActions();
  const ledger = new RecoveryLedger();

  runReconciliationPass([], [tracked('bob', 7, 'TR_A')], ledger, actions, 0);
  runReconciliationPass([], [tracked('bob', 7, 'TR_A')], ledger, actions, RECONCILE_INTERVAL_MS);
  assert.deepEqual(retired, [['bob', 'TR_A']]);

  // The peer re-shares window 7 later and it orphans again: first sighting
  // must hold, not inherit the retired share's expired clock.
  runReconciliationPass([], [tracked('bob', 7, 'TR_B')], ledger, actions, 60_000);
  assert.equal(retired.length, 1);
  runReconciliationPass([], [tracked('bob', 7, 'TR_B')], ledger, actions, 60_000 + RECONCILE_INTERVAL_MS);
  assert.deepEqual(retired, [['bob', 'TR_A'], ['bob', 'TR_B']]);
});

test('runReconciliationPass: rate-limits demand to the deadline cadence and never retires while the publication exists', () => {
  const { actions, subscribed, retired } = recordingActions();
  const ledger = new RecoveryLedger();
  const discovered = [publication('bob', 7, 'TR_A', false)];
  const tiles = [tracked('bob', 7, 'TR_A')];

  runReconciliationPass(discovered, tiles, ledger, actions, 0);
  assert.deepEqual(subscribed, [['bob', 'TR_A', true]]);

  // Inside the deadline: no second attempt, and the tile is not touched.
  runReconciliationPass(discovered, tiles, ledger, actions, RECOVERY_DEADLINE_MS - 1);
  assert.equal((subscribed).length, 1);
  assert.deepEqual(retired, []);

  // Past the deadline the attempt RE-ARMS: the publication still exists, so
  // demand stays legitimate — terminally giving up here is what left the
  // 2026-07-30 session blank for good.
  runReconciliationPass(discovered, tiles, ledger, actions, RECOVERY_DEADLINE_MS);
  assert.equal((subscribed).length, 2);
  assert.deepEqual(retired, []);

  // And keeps re-arming, at the deadline cadence, for as long as it diverges.
  runReconciliationPass(discovered, tiles, ledger, actions, RECOVERY_DEADLINE_MS * 2);
  assert.equal((subscribed).length, 3);
  assert.deepEqual(retired, []);
});

test('runReconciliationPass: each republished sid gets its own fresh attempt under sender churn', () => {
  // The live 2026-07-30 failure shape: the sender republishes every ~8s, so
  // each pass sees the same window under a NEW sid. The old window-keyed
  // ledger spent its one attempt on the first sid and then denied recovery to
  // every later one ('wait' until the deadline, then retire) — permanently
  // blank while a live publication existed.
  const { actions, subscribed, retired } = recordingActions();
  const ledger = new RecoveryLedger();

  runReconciliationPass([publication('bob', 7, 'TR_1', false)], [], ledger, actions, 0);
  runReconciliationPass([publication('bob', 7, 'TR_2', false)], [], ledger, actions, 8_000);
  runReconciliationPass([publication('bob', 7, 'TR_3', false)], [], ledger, actions, 16_000);
  assert.deepEqual(subscribed, [
    ['bob', 'TR_1', true],
    ['bob', 'TR_2', true],
    ['bob', 'TR_3', true],
  ]);
  assert.deepEqual(retired, []);
});

test('runReconciliationPass: never calls setSubscribed(false) — it is a one-way door on the SDK handle', () => {
  const { actions, subscribed } = recordingActions();
  const ledger = new RecoveryLedger();
  for (const now of [0, 1_000, RECOVERY_DEADLINE_MS, RECOVERY_DEADLINE_MS * 10]) {
    runReconciliationPass([publication('bob', 7, 'TR_A', false)], [], ledger, actions, now);
    runReconciliationPass([publication('carol', 9, 'TR_B')], [], ledger, actions, now);
  }
  assert.equal(subscribed.every(([, , value]) => value === true), true);
});

test('runReconciliationPass: attaches a track the SDK holds but no tile renders — the reported #298 symptom', () => {
  const { actions, subscribed, attached, retired, logs } = recordingActions();
  runReconciliationPass([publication('bob', 7, 'TR_A')], [], new RecoveryLedger(), actions, 0);
  assert.deepEqual(subscribed, []);
  assert.deepEqual(attached, [['bob', 'TR_A']]);
  assert.deepEqual(retired, []);
  assert.ok(logs.join(' ').includes('not-receiving'));
});

test('runReconciliationPass: rebinds a tile stuck on a dead sid to the subscribed replacement', () => {
  const { actions, subscribed, attached, retired } = recordingActions();
  runReconciliationPass(
    [publication('bob', 7, 'TR_NEW')],
    [tracked('bob', 7, 'TR_OLD')],
    new RecoveryLedger(),
    actions,
    0
  );
  assert.deepEqual(subscribed, []);
  assert.deepEqual(attached, [['bob', 'TR_NEW']]);
  assert.deepEqual(retired, []);
});

test('runReconciliationPass: a tile on a dead sid whose replacement is unsubscribed gets demand for the NEW sid', () => {
  // The exact "bound to a dead sid with no resubscribe" trap: previously this
  // was a bookkeeping no-op forever.
  const { actions, subscribed, attached, retired } = recordingActions();
  runReconciliationPass(
    [publication('bob', 7, 'TR_NEW', false)],
    [tracked('bob', 7, 'TR_OLD')],
    new RecoveryLedger(),
    actions,
    0
  );
  assert.deepEqual(subscribed, [['bob', 'TR_NEW', true]]);
  assert.deepEqual(attached, []);
  assert.deepEqual(retired, []);
});

test('runReconciliationPass: converges through the full republish-with-lost-events sequence', () => {
  // publish -> subscribed+rendering -> republish whose TrackSubscribed is
  // lost -> demand -> subscription lands -> attach. The viewer must end bound
  // to the live sid with zero retires along the way.
  const { actions, subscribed, attached, retired } = recordingActions();
  const ledger = new RecoveryLedger();
  let tiles = [tracked('bob', 7, 'TR_OLD')];

  // Republish: old track gone, new sid announced but demand not yet on.
  runReconciliationPass([publication('bob', 7, 'TR_NEW', false)], tiles, ledger, actions, 0);
  assert.deepEqual(subscribed, [['bob', 'TR_NEW', true]]);

  // Demand landed: the SDK now holds the track, but the TrackSubscribed event
  // was consumed against a superseded session — attach repairs it.
  runReconciliationPass(
    [publication('bob', 7, 'TR_NEW')],
    tiles,
    ledger,
    actions,
    RECONCILE_INTERVAL_MS
  );
  assert.deepEqual(attached, [['bob', 'TR_NEW']]);

  // The attach rebinds the tile; the next pass reconciles clean.
  tiles = [tracked('bob', 7, 'TR_NEW')];
  const findings = runReconciliationPass(
    [publication('bob', 7, 'TR_NEW')],
    tiles,
    ledger,
    actions,
    RECONCILE_INTERVAL_MS * 2
  );
  assert.deepEqual(findings, []);
  assert.deepEqual(retired, []);
});

test('runReconciliationPass: never touches a healthy share while repairing a diverged one', () => {
  const { actions, retired, subscribed } = recordingActions();
  const ledger = new RecoveryLedger();
  const discovered = [publication('carol', 9, 'TR_B')];
  const tiles = [tracked('bob', 7, 'TR_A'), tracked('carol', 9, 'TR_B')];
  runReconciliationPass(discovered, tiles, ledger, actions, 0);
  runReconciliationPass(discovered, tiles, ledger, actions, RECONCILE_INTERVAL_MS);
  assert.deepEqual(retired, [['bob', 'TR_A']]);
  assert.deepEqual(subscribed, []);
});

test('runReconciliationPass: re-arms a window that recovers, so a later re-share still gets an attempt', () => {
  const { actions, subscribed } = recordingActions();
  const ledger = new RecoveryLedger();
  const lostDemand = [publication('bob', 7, 'TR_A', false)];
  const tiles = [tracked('bob', 7, 'TR_A')];

  runReconciliationPass(lostDemand, tiles, ledger, actions, 0);
  assert.equal((subscribed).length, 1);

  // Recovered.
  runReconciliationPass([publication('bob', 7, 'TR_A')], tiles, ledger, actions, 1_000);
  assert.equal(ledger.size, 0);

  // Diverges again much later: a fresh attempt is available.
  runReconciliationPass(lostDemand, tiles, ledger, actions, 100_000);
  assert.equal((subscribed).length, 2);
});
