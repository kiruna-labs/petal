import assert from 'node:assert/strict';
import test from 'node:test';

import {
  boundsCoverDisplayRightEdge,
  boundsOverlap,
  displayRoutingVerdict,
  hotCornerActive,
  normalizedPointOnDisplay,
  parseAppleScriptBounds,
  pointInBounds,
  undeliveredDisplayResults,
} from './remote-control-display-share.mjs';

const DISPLAY = { x: 0, y: 0, width: 1920, height: 1080 };
const DISPLAY_ID = 1073741825;

test('normalized display point is the inverse of the host mapping', () => {
  assert.deepEqual(normalizedPointOnDisplay(DISPLAY, { x: 960, y: 540 }), { x: 0.5, y: 0.5 });
  assert.deepEqual(normalizedPointOnDisplay({ x: -1920, y: 0, width: 1920, height: 1080 }, { x: -960, y: 270 }), { x: 0.5, y: 0.25 });
  assert.throws(() => normalizedPointOnDisplay(DISPLAY, { x: 2000, y: 10 }), /not on display/);
  assert.throws(() => normalizedPointOnDisplay({ x: 0, y: 0, width: 0, height: 1 }, { x: 0, y: 0 }), /malformed/);
});

test('AppleScript bounds parse, point and overlap helpers', () => {
  const bounds = parseAppleScriptBounds('1120, 40, 1920, 1080');
  assert.deepEqual(bounds, [1120, 40, 1920, 1080]);
  assert.deepEqual(pointInBounds(bounds, 0.25, 0.5), { x: 1320, y: 560 });
  assert.throws(() => parseAppleScriptBounds('missing value'), /could not parse/);
  // The photon sentinel's top-left-origin rect on the 1920x1080 runner.
  const sentinel = [120, 360, 1080, 960];
  assert.equal(boundsOverlap(bounds, sentinel), false);
  assert.equal(boundsOverlap([213, 77, 886, 516], sentinel), true);
});

test('the clamped x = 1.0 point must land inside the target window', () => {
  // The runner's real constrained TextEdit bounds (the Dock caps the bottom).
  assert.equal(boundsCoverDisplayRightEdge([1120, 30, 1920, 990], DISPLAY, 510), true);
  assert.equal(boundsCoverDisplayRightEdge([1120, 30, 1919, 990], DISPLAY, 510), false);
  assert.equal(boundsCoverDisplayRightEdge([1120, 30, 1920, 990], DISPLAY, 1079), false);
});

test('hot-corner reading: unset, 0 and 1 are inert', () => {
  for (const inert of [null, undefined, '', '0', '1', ' 1\n']) assert.equal(hotCornerActive(inert), false, String(inert));
  for (const active of ['14', '2', ' 5\n']) assert.equal(hotCornerActive(active), true, String(active));
});

// Shapes of the real host lines (remote_control.rs).
const ENQUEUE = `[INFO] remote-control: enqueue replay kind=Pointer action=Some(Click) window=${DISPLAY_ID} seq=7 controller='web-x' target='native-autotest' status='-' frame=(0,0 1920x1080) target_pid=1`;
const LATENCY = `[INFO] remote-control-latency: host inject_ts_ms=1 kind=Pointer target_pid=Some(1)`;
const FIXED_DOWN = `[INFO] remote-control: AX pointer down window_id=${DISPLAY_ID} controller='web-x' button=Left route=display-session-tap`;

test('fixed routing passes: the sentinel only ever appears as target_pid', () => {
  const verdict = displayRoutingVerdict([ENQUEUE, LATENCY, FIXED_DOWN], DISPLAY_ID);
  assert.equal(verdict.ok, true, verdict.message);
  assert.equal(verdict.enqueued, 1);
});

test('pre-#349/#353 routing fails: every pid-1 process route is caught', () => {
  const regressions = [
    `[INFO] remote-control: semantic click route=direct pid=1 point=(1320.0,560.0)`,
    `[WARN] remote-control: session-tap route could not raise target window_id=${DISPLAY_ID} pid=1 -- refusing to report delivery (#599)`,
    `[INFO] remote-control: key refused window=${DISPLAY_ID} pid=1 focus=mismatch frontmost=false raise=skipped`,
    `[WARN] remote-control: AX text shortcut Copy found no text element in pid 1; falling back to CGEvent`,
    `[WARN] remote-control: raise(pid=1, window_id=${DISPLAY_ID}) brought the app forward but AXRaise on the specific window failed -- not reporting success`,
    `[ERROR] remote-control: pointer RELEASE not injected -- host may hold a phantom left button: reason=routes-exhausted window_id=${DISPLAY_ID} controller='web-x' seq=9 (#134)`,
  ];
  for (const line of regressions) {
    const verdict = displayRoutingVerdict([ENQUEUE, line], DISPLAY_ID);
    assert.equal(verdict.ok, false, line);
    assert.equal(verdict.violations.length, 1, line);
  }
});

test('pid 10/100 and other windows are not mistaken for the sentinel', () => {
  const unrelated = [
    `[INFO] remote-control: semantic click route=direct pid=1234 point=(1,1)`,
    `[INFO] remote-control: key refused window=165 pid=10 focus=mismatch`,
    `[ERROR] remote-control: pointer RELEASE not injected -- reason=routes-exhausted window_id=165 seq=2 (#134)`,
  ];
  assert.equal(displayRoutingVerdict([ENQUEUE, ...unrelated], DISPLAY_ID).ok, true);
});

test('a blind log (no display replay lines) is a failure, never a pass', () => {
  const verdict = displayRoutingVerdict([], DISPLAY_ID);
  assert.equal(verdict.ok, false);
  assert.match(verdict.message, /blind/);
  // An enqueue for a WINDOW share does not count as display evidence.
  assert.equal(displayRoutingVerdict([ENQUEUE.replace('target_pid=1', 'target_pid=9796')], DISPLAY_ID).ok, false);
});

test('undelivered terminal results are reported per display target', () => {
  const records = [
    { windowId: DISPLAY_ID, outcome: 'applied' },
    { windowId: DISPLAY_ID, outcome: 'submitted' },
    { windowId: DISPLAY_ID, outcome: 'replayFailed', failureCode: 'windowMismatch' },
    { windowId: 165, outcome: 'replayFailed' },
  ];
  assert.deepEqual(undeliveredDisplayResults(records, DISPLAY_ID), [records[2]]);
});
