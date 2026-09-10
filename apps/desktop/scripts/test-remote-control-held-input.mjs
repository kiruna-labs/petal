#!/usr/bin/env node
// #134: a remote-controlled LEFT drag could leave the host with a phantom held
// primary button. The suite caught it only in case 7 (`right drag held
// button`), the sole case that read `remote-control-status`'s `pressedInputs`
// -- so the leak from case 6 was reported against case 7's right drag, and the
// `buttons: 1` mask in the failure output was the only clue it was the wrong
// gesture. These tests pin both halves of the fix: the shared helper behaves,
// and every pressing case actually calls it.
import assert from 'node:assert/strict';
import path from 'node:path';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  DEFAULT_RELEASE_GRACE_MS,
  HOST_HELD_INPUT_TTL_MS,
  LEFT_DRAG_STRESS_CHECKPOINTS,
  RELEASE_NOT_INJECTED_NEEDLE,
  assertReleasedWithin,
  describeHeldButtons,
  distributeStressIterations,
  heldInputs,
  inputKey,
  parseReleaseFailureReason,
  releaseFailureMessage,
  releaseNotInjectedLines,
  stillHeldAfterRecovery,
  stressLeakOutcome,
  summarizeReleaseStressLeak,
} from './remote-control-held-input.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '../../..');

function scriptedStatus(snapshots) {
  let index = 0;
  return async () => snapshots[Math.min(index++, snapshots.length - 1)];
}

// A clock + sleep pair that advances only when the caller sleeps, so the poll
// loop's real timing behaviour is exercised without waiting for it.
function virtualClock() {
  let nowMs = 0;
  return {
    now: () => nowMs,
    sleep: async (ms) => {
      nowMs += ms;
    },
    elapsed: () => nowMs,
  };
}

const HELD_PRIMARY = {
  pending: [],
  pressedInputs: [{ buttons: 1, controllerId: 'web-test', keys: 0, windowId: 150 }],
  sessions: [{ controllerId: 'web-test', windowId: 150 }],
};
const RELEASED = { pending: [], pressedInputs: [], sessions: [{ controllerId: 'web-test', windowId: 150 }] };

test('a released gesture passes', async () => {
  const clock = virtualClock();
  const snapshot = await assertReleasedWithin({
    gesture: "case 6 'left drag'",
    readStatus: scriptedStatus([RELEASED]),
    sleep: clock.sleep,
    now: clock.now,
  });
  assert.deepEqual(snapshot, RELEASED);
  assert.equal(clock.elapsed(), 0, 'a clean release must not cost the suite any wall clock');
});

test('a press that is never released fails, naming its own gesture and the stuck button', async () => {
  const clock = virtualClock();
  await assert.rejects(
    assertReleasedWithin({
      gesture: "case 6 'left drag'",
      readStatus: scriptedStatus([HELD_PRIMARY]),
      sleep: clock.sleep,
      now: clock.now,
    }),
    (error) => {
      // The whole point of #134: the message names case 6's left drag, not
      // whichever case happens to run next.
      assert.match(error.message, /case 6 'left drag'/);
      assert.match(error.message, /primary/);
      assert.match(error.message, /#134/);
      return true;
    }
  );
});

test('a release still in flight is given a bounded grace, not failed on the first sample', async () => {
  const clock = virtualClock();
  const snapshot = await assertReleasedWithin({
    gesture: "case 7 'right drag'",
    readStatus: scriptedStatus([HELD_PRIMARY, HELD_PRIMARY, RELEASED]),
    sleep: clock.sleep,
    now: clock.now,
  });
  assert.deepEqual(snapshot, RELEASED);
  assert.ok(clock.elapsed() > 0 && clock.elapsed() < DEFAULT_RELEASE_GRACE_MS);
});

test('the grace period stays below the host TTL that would otherwise erase the evidence', async () => {
  // `HELD_INPUT_TTL` (remote_control_core.rs) synthesises a release after
  // 1200ms. A grace at or above that would let the host's own safety net clear
  // the button and report a leak as a pass.
  assert.ok(DEFAULT_RELEASE_GRACE_MS < HOST_HELD_INPUT_TTL_MS);
  const clock = virtualClock();
  await assert.rejects(
    assertReleasedWithin({
      gesture: 'case 6 left drag',
      readStatus: scriptedStatus([RELEASED]),
      sleep: clock.sleep,
      now: clock.now,
      graceMs: HOST_HELD_INPUT_TTL_MS,
    }),
    /must stay below the host's own held-input TTL/
  );
});

test('a snapshot with no pressedInputs field is treated as released, not as a crash', () => {
  assert.deepEqual(heldInputs(undefined), []);
  assert.deepEqual(heldInputs({}), []);
  assert.deepEqual(heldInputs({ pressedInputs: null }), []);
});

test('button masks are decoded the way the #134 investigation had to decode them by hand', () => {
  assert.equal(describeHeldButtons([{ buttons: 1 }]), 'primary');
  assert.equal(describeHeldButtons([{ buttons: 2 }]), 'right');
  assert.equal(describeHeldButtons([{ buttons: 4 }]), 'middle');
  assert.equal(describeHeldButtons([{ buttons: 5 }]), 'primary+middle');
  assert.equal(describeHeldButtons([{ buttons: 0 }]), 'mask=0');
  assert.equal(describeHeldButtons([]), 'none');
  assert.match(releaseFailureMessage('case 6 left drag', HELD_PRIMARY, 600), /600ms after the gesture ended/);
});

// ---- the scenario suite itself -------------------------------------------

const scenario = readFileSync(path.join(scriptDir, 'remote-control-scenario.mjs'), 'utf8');

function scenarioCases() {
  const start = scenario.indexOf('\nconst CASES = [');
  assert.ok(start > 0, 'remote-control-scenario.mjs must still declare a CASES array');
  const end = scenario.indexOf('\n];', start);
  assert.ok(end > start, 'the CASES array must still be terminated');
  const body = scenario.slice(start, end);
  const starts = [...body.matchAll(/\n {2}\{\n {4}id: (\d+),/g)];
  assert.ok(starts.length >= 30, `expected the known case list, found ${starts.length}`);
  return starts.map((match, index) => ({
    id: Number(match[1]),
    source: body.slice(match.index, index + 1 < starts.length ? starts[index + 1].index : body.length),
  }));
}

// A case "presses" if it sends a click, a drag, or a bare pointer Down.
function pressesAButton(source) {
  return /api\.click\(|api\.drag\(|action: 'down'/.test(source);
}

test('every case that presses a mouse button asserts the host released it', () => {
  const pressing = scenarioCases().filter((testCase) => pressesAButton(testCase.source));
  assert.ok(pressing.length >= 10, `expected the known pressing cases, found ${pressing.length}`);
  for (const testCase of pressing) {
    const assertsRelease = testCase.source.includes('assertReleased(')
      // Cases 25 and 28 assert something STRICTER than "empty at the end": that
      // the TTL sweeper, or an explicit disable, produced the release. Their
      // bespoke checks are the precedent this helper generalises -- do not
      // rewrite them into the generic one.
      || /pressedInputs\?\.length\) throw/.test(testCase.source);
    assert.ok(
      assertsRelease,
      `case ${testCase.id} presses a mouse button but never asserts remote-control-status reports it released. `
      + 'A case that presses must prove it released, or the next case that checks inherits the blame (#134).'
    );
  }
});

test('case 6 -- the left drag #134 was actually filed against -- asserts its own release', () => {
  const case6 = scenarioCases().find((testCase) => testCase.id === 6);
  assert.ok(case6, 'case 6 (left drag) must still exist');
  assert.match(case6.source, /assertReleased\(ctx, 'left drag'\)/);
});

test('case 25 keeps its own TTL-release assertion', () => {
  // The existing precedent for a host-side release assertion. #134 generalised
  // it; it must not have been swallowed by the generalisation.
  const case25 = scenarioCases().find((testCase) => testCase.id === 25);
  assert.ok(case25, 'case 25 (held-input TTL synthetic release) must still exist');
  assert.match(case25.source, /held input remained after TTL/);
  assert.match(case25.source, /'TTL synthetic mouse-up'/);
});

test('the release check has exactly one implementation', () => {
  // Cases 25 and 28 are the two documented lifecycle exceptions above. Any
  // third hand-rolled copy means the check drifted again.
  const bespoke = scenario.match(/pressedInputs\?\.length\) throw/g) ?? [];
  assert.equal(
    bespoke.length,
    2,
    'only cases 25 (TTL) and 28 (disable) may hand-roll the pressedInputs check; everything else uses assertReleased'
  );
});

test('the mirrored host TTL still matches the Rust constant it is derived from', () => {
  // `HOST_HELD_INPUT_TTL_MS` is a hand copy of a Rust value. If the host's TTL
  // ever drops below the grace period, the sweeper starts releasing buttons out
  // from under the assertion and a real leak reads as a pass -- silently. Fail
  // here instead (the #866 lesson: a mirrored constant needs a lockstep gate).
  const core = readFileSync(
    path.join(repoRoot, 'apps/desktop/src-tauri/src/remote_control_core.rs'),
    'utf8'
  );
  const match = core.match(/HELD_INPUT_TTL: Duration = Duration::from_millis\((\d[\d_]*)\)/);
  assert.ok(match, 'remote_control_core.rs must still declare HELD_INPUT_TTL in milliseconds');
  assert.equal(
    Number(match[1].replace(/_/g, '')),
    HOST_HELD_INPUT_TTL_MS,
    'the host held-input TTL changed; update HOST_HELD_INPUT_TTL_MS and re-check the grace period'
  );
});

test('a failed native release is observable in the host log, and the suite collects it', () => {
  // Step 2 of #134's plan: the controller's wire echo publishes an Up whether
  // or not native injection landed, so the ONLY way to learn why a release did
  // not land is the host's own log. Both halves have to exist: the host must
  // emit the line, and a failing case must capture it.
  const remoteControl = readFileSync(
    path.join(repoRoot, 'apps/desktop/src-tauri/src/remote_control.rs'),
    'utf8'
  );
  assert.match(
    remoteControl,
    /remote-control: pointer RELEASE not injected/,
    'remote_control.rs must log a failed pointer release with a stable signature (#134)'
  );
  assert.match(
    scenario,
    /pointer RELEASE not injected/,
    'captureCaseFailureForensics must collect that signature, or the log line reaches nobody (#134)'
  );
});

// ---- the #134 left-drag stress case --------------------------------------

const RELEASE_NOT_INJECTED_LINE =
  "2026-09-10T12:00:00 [ERROR] remote-control: pointer RELEASE not injected -- host may hold a phantom "
  + "primary button: reason=routes-exhausted window_id=150 controller='web-28c0' seq=41 (#134)";

test("#140's reason tag is read off the host's own line, and its absence is not mistaken for one", () => {
  assert.deepEqual(releaseNotInjectedLines(['unrelated', RELEASE_NOT_INJECTED_LINE]), [RELEASE_NOT_INJECTED_LINE]);
  assert.deepEqual(releaseNotInjectedLines(undefined), []);
  assert.equal(parseReleaseFailureReason(RELEASE_NOT_INJECTED_LINE), 'routes-exhausted');
  assert.equal(parseReleaseFailureReason('remote-control: something else entirely'), null);
  assert.ok(RELEASE_NOT_INJECTED_LINE.includes(RELEASE_NOT_INJECTED_NEEDLE));
});

test('a leak WITH a host release-failure line is reported as a failed injection, naming the reason', () => {
  const leak = summarizeReleaseStressLeak({
    iteration: 4,
    iterations: 12,
    snapshot: HELD_PRIMARY,
    logLines: [RELEASE_NOT_INJECTED_LINE],
  });
  assert.equal(leak.iteration, 4);
  assert.equal(leak.upWasProcessed, 'injection-failed');
  assert.deepEqual(leak.releaseFailureReasons, ['routes-exhausted']);
  assert.match(leak.detail, /iteration 4\/12/);
  assert.match(leak.detail, /primary/);
  assert.match(leak.detail, /native injection of the Up FAILED/);
});

test('a leak WITHOUT one says the Up was never processed -- the discriminator #134 turns on', () => {
  const leak = summarizeReleaseStressLeak({ iteration: 2, iterations: 12, snapshot: HELD_PRIMARY, logLines: [] });
  assert.equal(leak.upWasProcessed, 'never-processed');
  assert.deepEqual(leak.releaseFailureReasons, []);
  assert.match(leak.detail, /never PROCESSED/);
});

test('the held key is compared against the live session key -- the surviving hypothesis', () => {
  assert.equal(inputKey({ windowId: 150, controllerId: 'web-28c0' }), '(window 150, controller web-28c0)');
  const agreeing = summarizeReleaseStressLeak({ iteration: 1, iterations: 12, snapshot: HELD_PRIMARY });
  assert.equal(agreeing.keyMismatch, false);
  assert.match(agreeing.detail, /keys agree/);
  const mismatched = summarizeReleaseStressLeak({
    iteration: 1,
    iterations: 12,
    snapshot: {
      pressedInputs: [{ buttons: 1, controllerId: 'web-old', windowId: 150 }],
      sessions: [{ controllerId: 'web-new', windowId: 150 }],
    },
  });
  assert.equal(mismatched.keyMismatch, true);
  assert.match(mismatched.detail, /KEY MISMATCH/);
  assert.deepEqual(mismatched.pressedKeys, ['(window 150, controller web-old)']);
  assert.deepEqual(mismatched.sessionKeys, ['(window 150, controller web-new)']);
});

test('a held entry filed under the PRE-gesture key is named as the mid-gesture key change', () => {
  const leak = summarizeReleaseStressLeak({
    iteration: 5,
    iterations: 12,
    snapshot: {
      pressedInputs: [{ buttons: 1, controllerId: 'web-old', windowId: 150 }],
      sessions: [{ controllerId: 'web-new', windowId: 150 }],
    },
    sessionKeysBeforeGesture: ['(window 150, controller web-old)'],
  });
  assert.equal(leak.keyChangedDuringGesture, true);
  assert.equal(leak.heldKeyIsPreGestureKey, true);
  assert.match(leak.detail, /the held key is the PRE-GESTURE key -- the key changed mid-gesture/);
  // ... and a run where nothing moved must not claim it did.
  const stable = summarizeReleaseStressLeak({
    iteration: 5,
    iterations: 12,
    snapshot: HELD_PRIMARY,
    sessionKeysBeforeGesture: ['(window 150, controller web-test)'],
  });
  assert.equal(stable.keyChangedDuringGesture, false);
  assert.equal(stable.heldKeyIsPreGestureKey, false);
  assert.doesNotMatch(stable.detail, /PRE-GESTURE key/);
  // An unrecorded pre-gesture key is not evidence of stability either.
  assert.equal(summarizeReleaseStressLeak({ iteration: 1, iterations: 12, snapshot: HELD_PRIMARY }).keyChangedDuringGesture, false);
  assert.match(summarizeReleaseStressLeak({ iteration: 1, iterations: 12, snapshot: HELD_PRIMARY }).detail, /pre-gesture unrecorded/);
});

test('the recovery attempts are recorded, since a drain that misses is itself evidence', () => {
  const leak = summarizeReleaseStressLeak({
    iteration: 7,
    iterations: 12,
    snapshot: HELD_PRIMARY,
    clearedByHoverMove: false,
    clearedByTtl: true,
  });
  assert.equal(leak.clearedByHoverMove, false);
  assert.equal(leak.clearedByTtl, true);
  assert.match(leak.detail, /zero-mask move cleared it: false/);
  assert.match(leak.detail, /host TTL cleared it: true/);
});

test('a quarantined leak is recorded as a skip -- never a pass, and never a gate failure', () => {
  const leak = summarizeReleaseStressLeak({ iteration: 3, iterations: 12, snapshot: HELD_PRIMARY });
  const quarantined = stressLeakOutcome(leak, { quarantined: true });
  assert.equal(quarantined.status, 'skip');
  assert.notEqual(quarantined.status, 'pass');
  assert.match(quarantined.detail, /QUARANTINED \(#134/);
  assert.match(quarantined.detail, /iteration 3\/12/);
  // #134's own definition of done includes taking the quarantine off once the
  // root cause is fixed; that flip must produce a real failure.
  assert.equal(stressLeakOutcome(leak, { quarantined: false }).status, 'fail');
  assert.equal(stressLeakOutcome(leak).status, 'skip');
});

// ---- WHERE the stress drags happen (#134) --------------------------------
//
// The first stress run batched all twelve drags in a quiet tail and came back
// 12 for 12 clean, which argues the fault is not a flat per-drag probability
// but depends on conditions an isolated loop never creates. The same budget is
// now scattered across four checkpoints. These tests pin the placement, not
// just the arithmetic: the plan and the actual sequence must not drift apart.

test('the stress budget is split across the checkpoints, remainder to the earliest', () => {
  assert.deepEqual(distributeStressIterations(12, 4), [3, 3, 3, 3]);
  // A reduced budget spends its drags in the historical neighbourhood first
  // rather than thinning every site into uselessness.
  assert.deepEqual(distributeStressIterations(13, 4), [4, 3, 3, 3]);
  assert.deepEqual(distributeStressIterations(2, 4), [1, 1, 0, 0]);
  assert.deepEqual(distributeStressIterations(0, 4), [0, 0, 0, 0]);
  assert.equal(distributeStressIterations(12, 4).reduce((a, b) => a + b, 0), 12, 'no drag may be lost in the split');
  assert.throws(() => distributeStressIterations(-1, 4), /non-negative integer/);
  assert.throws(() => distributeStressIterations(1.5, 4), /non-negative integer/);
  assert.throws(() => distributeStressIterations(12, 0), /positive integer/);
});

test('each checkpoint declares where it sits and why that place was chosen', () => {
  assert.ok(LEFT_DRAG_STRESS_CHECKPOINTS.length >= 3, 'scattering means more than a couple of sites');
  const afterIds = LEFT_DRAG_STRESS_CHECKPOINTS.map((checkpoint) => checkpoint.afterCaseId);
  assert.deepEqual([...new Set(afterIds)], afterIds, 'two checkpoints in the same place are one checkpoint');
  assert.deepEqual([...afterIds].sort((a, b) => a - b), afterIds, 'checkpoints must be declared in execution order');
  for (const checkpoint of LEFT_DRAG_STRESS_CHECKPOINTS) {
    assert.ok(Number.isInteger(checkpoint.id), 'each checkpoint needs its own case id');
    assert.ok(Number.isInteger(checkpoint.afterCaseId));
    assert.ok(checkpoint.context && checkpoint.rationale, `checkpoint ${checkpoint.id} must say why it sits there`);
  }
  // The three conditions the 12-for-12 result pointed at, each with a site.
  const rationales = LEFT_DRAG_STRESS_CHECKPOINTS.map((checkpoint) => checkpoint.rationale).join(' ');
  assert.match(rationales, /case 6 leaking/, 'one checkpoint must sit in the historical neighbourhood');
  assert.match(rationales, /reconnect/, 'one checkpoint must sit where the (window_id, controller_id) key can change');
  assert.match(rationales, /teardown/, 'one checkpoint must sit after a lifecycle teardown');
});

test('a leak names the checkpoint it came from, not just the iteration number', () => {
  // Two runs that both leak on "iteration 2" mean different things if one is
  // after case 7's Escape and the other after case 29's reconnect.
  const leak = summarizeReleaseStressLeak({
    iteration: 2,
    iterations: 3,
    snapshot: HELD_PRIMARY,
    checkpoint: { index: 4, of: 4, id: 36, afterCaseId: 29, context: "case 29's reconnect during control" },
  });
  assert.equal(leak.checkpoint.afterCaseId, 29);
  assert.match(leak.detail, /iteration 2\/3/);
  assert.match(leak.detail, /stress checkpoint 4\/4/);
  assert.match(leak.detail, /immediately after case 29/);
  // ... and a leak with no checkpoint context must not invent one.
  assert.doesNotMatch(summarizeReleaseStressLeak({ iteration: 1, iterations: 3, snapshot: HELD_PRIMARY }).detail, /checkpoint/);
});

test('a scattered leak that survives recovery announces the handoff hazard it creates', () => {
  // The tail placement made a leak un-inheritable by construction. Scattering
  // gives that up, so an unrecovered phantom has to say that a LATER case may
  // report it as its own -- the mis-attribution #134 is about.
  assert.equal(stillHeldAfterRecovery({ clearedByHoverMove: true, clearedByTtl: null }), false);
  assert.equal(stillHeldAfterRecovery({ clearedByHoverMove: false, clearedByTtl: true }), false);
  assert.equal(stillHeldAfterRecovery({ clearedByHoverMove: false, clearedByTtl: false }), true);
  assert.equal(stillHeldAfterRecovery({}), null, 'recovery that never ran is not evidence either way');
  const stuck = summarizeReleaseStressLeak({
    iteration: 1,
    iterations: 3,
    snapshot: HELD_PRIMARY,
    clearedByHoverMove: false,
    clearedByTtl: false,
  });
  assert.equal(stuck.stillHeldAfterRecovery, true);
  assert.match(stuck.detail, /STILL held the button after recovery/);
  assert.match(stuck.detail, /LATER case's release assertion may inherit/);
  const recovered = summarizeReleaseStressLeak({
    iteration: 1,
    iterations: 3,
    snapshot: HELD_PRIMARY,
    clearedByHoverMove: true,
  });
  assert.equal(recovered.stillHeldAfterRecovery, false);
  assert.doesNotMatch(recovered.detail, /STILL held/);
});

test('the declared checkpoints are exactly the stress cases in the sequence, in those places', () => {
  const cases = scenarioCases();
  const stressIds = new Set(LEFT_DRAG_STRESS_CHECKPOINTS.map((checkpoint) => checkpoint.id));
  const stressCases = cases.filter((testCase) => stressIds.has(testCase.id));
  assert.equal(
    stressCases.length,
    LEFT_DRAG_STRESS_CHECKPOINTS.length,
    'every declared checkpoint must exist as a case, and no extra stress case may exist'
  );
  for (const [index, checkpoint] of LEFT_DRAG_STRESS_CHECKPOINTS.entries()) {
    const position = cases.findIndex((testCase) => testCase.id === checkpoint.id);
    assert.ok(position > 0, `checkpoint case ${checkpoint.id} must exist in CASES`);
    // The placement IS the experiment: this pins the case that runs
    // immediately before each checkpoint against the declared plan.
    assert.equal(
      cases[position - 1].id,
      checkpoint.afterCaseId,
      `checkpoint ${checkpoint.id} must run immediately after case ${checkpoint.afterCaseId}, `
      + `found case ${cases[position - 1].id} -- the placement and LEFT_DRAG_STRESS_CHECKPOINTS have drifted apart`
    );
    assert.match(
      cases[position].source,
      new RegExp(`runLeftDragReleaseStress\\(ctx, ${index}\\)`),
      `checkpoint case ${checkpoint.id} must run checkpoint index ${index}`
    );
  }
});

test('the drags are SCATTERED: no checkpoint is the last case, and they span the suite', () => {
  const cases = scenarioCases();
  const stressIds = new Set(LEFT_DRAG_STRESS_CHECKPOINTS.map((checkpoint) => checkpoint.id));
  const positions = cases.map((testCase, index) => (stressIds.has(testCase.id) ? index : -1)).filter((index) => index >= 0);
  // Batching them at the end is the arrangement that returned 12 for 12 clean;
  // the whole point of this change is that real traffic follows every burst.
  assert.ok(
    positions.every((position) => position < cases.length - 1),
    'a stress checkpoint must never be the last case -- a tail burst is the arrangement that found nothing'
  );
  assert.ok(
    positions[positions.length - 1] - positions[0] >= Math.floor(cases.length / 2),
    'the checkpoints must span the suite rather than clustering in one region'
  );
  // Not the same experiment repeated four times: the traffic before each
  // checkpoint has to differ.
  const preceding = positions.map((position) => cases[position - 1].id);
  assert.deepEqual([...new Set(preceding)], preceding, 'each checkpoint must follow a different case');
});

test('a stress checkpoint cannot fail the gate while it is quarantined', () => {
  const start = scenario.indexOf('async function runLeftDragReleaseStress(');
  assert.ok(start > 0, 'the stress runner must still exist');
  const runner = scenario.slice(start, scenario.indexOf('\nconst CASES = [', start));
  // Reuses the one release oracle rather than re-implementing the check.
  assert.match(runner, /await assertReleased\(\n?\s*ctx,\n?\s*`left drag stress checkpoint/);
  assert.match(runner, /stressLeakOutcome\(leak, \{ quarantined: LEFT_DRAG_STRESS_QUARANTINED \}\)/);
  assert.match(runner, /summarizeReleaseStressLeak\(\{/, 'the evidence path must stay the shared one');
  assert.match(runner, /sessionKeysBeforeGesture/, 'the pre-gesture key must be recorded before the drag, not reconstructed after it');
  assert.match(runner, /checkpoint,/, 'the leak record must carry which checkpoint produced it');
  assert.doesNotMatch(runner, /status: 'fail'/, 'the stress runner must never hand back a failure of its own making');
  // ... and neither may an unexpected error inside it (a wedged TextEdit, a
  // dropped socket): the extra drags must not be able to fail a release over a
  // bug that is already known and open.
  assert.match(runner, /if \(!LEFT_DRAG_STRESS_QUARANTINED\) throw error;/);
  assert.match(runner, /return skipCase\(`QUARANTINED \(#134\) -- the stress case could not complete/);
  // In-script quarantine, not a workflow list: `cross-machine-rc-suite.sh` and
  // `rc-live-suite.sh` run this same suite with no quarantine list at all.
  assert.match(scenario, /const LEFT_DRAG_STRESS_QUARANTINED = process\.env\.PETAL_RC_LEFT_DRAG_STRESS_QUARANTINE !== '0';/);
  // The gesture must stay a byte-for-byte copy of case 6's, or it is a
  // different experiment from the one #134 was filed about.
  const case6 = scenarioCases().find((testCase) => testCase.id === 6);
  const drag = /api\.drag\(\{ target, from: \$\{JSON\.stringify\(REMOTE_CONTROL_COORDINATES\.suiteDragFrom\)\}, to: \$\{JSON\.stringify\(REMOTE_CONTROL_COORDINATES\.suiteDragTo\)\}, steps: \$\{REMOTE_CONTROL_DRAG_STEPS\.suite\}, button: \$\{REMOTE_CONTROL_BUTTONS\.left\}/;
  assert.match(case6.source, drag);
  assert.match(runner, drag);
});

test('the TOTAL stress iteration count did not balloon when the drags were scattered', () => {
  // Same budget, different places. A checkpoint count that grew without the
  // total being re-checked would quietly multiply the suite's runtime.
  const match = /const DEFAULT_LEFT_DRAG_STRESS_ITERATIONS = (\d+);/.exec(scenario);
  assert.ok(match, 'the stress iteration count must stay a named default');
  assert.match(scenario, /PETAL_RC_LEFT_DRAG_STRESS_ITERATIONS must be a non-negative integer/,
    'a typo\'d override must fail loudly, not silently run zero iterations');
  const iterations = Number(match[1]);
  // Below ~10 the run is back to relying on luck, which is the whole problem.
  assert.ok(iterations >= 10 && iterations <= 20, `default total iterations ${iterations} must stay in 10..20`);
  const perCheckpoint = distributeStressIterations(iterations, LEFT_DRAG_STRESS_CHECKPOINTS.length);
  assert.equal(perCheckpoint.reduce((a, b) => a + b, 0), iterations);
  assert.ok(
    perCheckpoint.every((count) => count >= 2),
    `every checkpoint must get at least two consecutive drags, got ${JSON.stringify(perCheckpoint)}`
  );
  // The env var is the whole-run total, not a per-checkpoint count.
  assert.match(scenario, /const LEFT_DRAG_STRESS_PER_CHECKPOINT = distributeStressIterations\(\n\s*LEFT_DRAG_STRESS_ITERATIONS,/);
});
