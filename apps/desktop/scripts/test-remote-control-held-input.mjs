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
  RELEASE_NOT_INJECTED_NEEDLE,
  assertReleasedWithin,
  describeHeldButtons,
  heldInputs,
  inputKey,
  parseReleaseFailureReason,
  releaseFailureMessage,
  releaseNotInjectedLines,
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

test('the stress case exists, runs last, and cannot fail the gate while it is quarantined', () => {
  const cases = scenarioCases();
  const stress = cases.find((testCase) => testCase.id === 33);
  assert.ok(stress, 'case 33 (left drag release stress) must exist');
  assert.equal(cases[cases.length - 1].id, 33, 'the stress case must run LAST -- a press it leaks must not be inherited by another case');
  assert.match(stress.source, /runLeftDragReleaseStress\(ctx\)/);

  const start = scenario.indexOf('async function runLeftDragReleaseStress(');
  assert.ok(start > 0, 'the stress runner must still exist');
  const runner = scenario.slice(start, scenario.indexOf('\nconst CASES = [', start));
  // Reuses the one release oracle rather than re-implementing the check.
  assert.match(runner, /await assertReleased\(ctx, `left drag stress iteration/);
  assert.match(runner, /stressLeakOutcome\(leak, \{ quarantined: LEFT_DRAG_STRESS_QUARANTINED \}\)/);
  assert.match(runner, /sessionKeysBeforeGesture/, 'the pre-gesture key must be recorded before the drag, not reconstructed after it');
  assert.doesNotMatch(runner, /status: 'fail'/, 'the stress runner must never hand back a failure of its own making');
  // ... and neither may an unexpected error inside it (a wedged TextEdit, a
  // dropped socket): twelve extra drags must not be able to fail a release
  // over a bug that is already known and open.
  assert.match(runner, /if \(!LEFT_DRAG_STRESS_QUARANTINED\) throw error;/);
  assert.match(runner, /return skipCase\(`QUARANTINED \(#134\) -- the stress case could not complete/);
  // The gesture must stay a byte-for-byte copy of case 6's, or it is a
  // different experiment from the one #134 was filed about.
  const case6 = cases.find((testCase) => testCase.id === 6);
  const drag = /api\.drag\(\{ target, from: \$\{JSON\.stringify\(REMOTE_CONTROL_COORDINATES\.suiteDragFrom\)\}, to: \$\{JSON\.stringify\(REMOTE_CONTROL_COORDINATES\.suiteDragTo\)\}, steps: \$\{REMOTE_CONTROL_DRAG_STEPS\.suite\}, button: \$\{REMOTE_CONTROL_BUTTONS\.left\}/;
  assert.match(case6.source, drag);
  assert.match(runner, drag);
});

test('the stress iteration count stays in the range that makes a 1-in-3 fault likely to appear', () => {
  // Below ~10 the run is back to relying on luck, which is the whole problem.
  const match = /const DEFAULT_LEFT_DRAG_STRESS_ITERATIONS = (\d+);/.exec(scenario);
  assert.ok(match, 'the stress iteration count must stay a named default');
  assert.match(scenario, /PETAL_RC_LEFT_DRAG_STRESS_ITERATIONS must be a non-negative integer/,
    'a typo\'d override must fail loudly, not silently run zero iterations');
  const iterations = Number(match[1]);
  assert.ok(iterations >= 10 && iterations <= 20, `default iterations ${iterations} must stay in 10..20`);
});
