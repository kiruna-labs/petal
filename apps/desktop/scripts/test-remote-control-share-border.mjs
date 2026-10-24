import assert from 'node:assert/strict';
import test from 'node:test';

import {
  awaitShareBorderStacked,
  BORDER_TRACKER_SETTLE_MS,
  classifyShareBorderStackReport,
  shareStateSummary,
  sourceWindowIsGone,
} from './remote-control-share-border.mjs';

// The literal report the v0.9.21 release gate threw on (run 34459415604,
// petal-dev.log 09:23:34.984), plus the share state that build did not report.
const RELEASE_GATE_REPORT = {
  windowId: 163,
  source: { number: 163, stackIndex: 12 },
  border: { number: 145, stackIndex: null },
  overlays: [{ number: 149, stackIndex: 11 }],
  shareState: {
    share: 'onScreen',
    registered: true,
    trackerHidden: true,
    hideRequested: false,
  },
};

const HEALTHY_REPORT = {
  windowId: 163,
  source: { number: 163, stackIndex: 13 },
  border: { number: 145, stackIndex: 11 },
  overlays: [{ number: 149, stackIndex: 12 }],
  shareState: {
    share: 'onScreen',
    registered: true,
    trackerHidden: false,
    hideRequested: false,
  },
};

function withShareState(report, shareState) {
  return { ...report, shareState: { ...report.shareState, ...shareState } };
}

test('a tracker-hidden border is attributed to the lost SOURCE window, not the border', () => {
  const verdict = classifyShareBorderStackReport(RELEASE_GATE_REPORT, 163);
  assert.equal(verdict.ok, false);
  assert.equal(verdict.reason, 'source-window-lost');
  assert.match(verdict.message, /SOURCE window left the window stack/);
  assert.match(verdict.message, /trackerHidden=true/);
});

test('an off-screen border with a live share is still reported as a border failure', () => {
  const verdict = classifyShareBorderStackReport(
    withShareState(RELEASE_GATE_REPORT, { trackerHidden: false }),
    163
  );
  assert.equal(verdict.ok, false);
  assert.equal(verdict.reason, 'border-missing');
  assert.match(verdict.message, /while its share is live/);
});

test('a share teardown already in flight is not a border failure', () => {
  for (const shareState of [
    { trackerHidden: false, hideRequested: true },
    { trackerHidden: false, share: 'closed' },
    { trackerHidden: false, share: 'offScreen' },
    { trackerHidden: false, share: 'notShared' },
  ]) {
    const verdict = classifyShareBorderStackReport(
      withShareState(RELEASE_GATE_REPORT, shareState),
      163
    );
    assert.equal(verdict.reason, 'source-window-lost', JSON.stringify(shareState));
  }
});

test('an unregistered border on a live share is a border failure', () => {
  const verdict = classifyShareBorderStackReport(
    {
      ...RELEASE_GATE_REPORT,
      border: null,
      shareState: { share: 'onScreen', registered: false, trackerHidden: false, hideRequested: false },
    },
    163
  );
  assert.equal(verdict.reason, 'border-missing');
});

test('a healthy report passes and its summary carries the share state', () => {
  const verdict = classifyShareBorderStackReport(HEALTHY_REPORT, 163);
  assert.equal(verdict.ok, true);
  assert.match(verdict.summary, /# share-border-stack window=163 border=11 source=13 \(border in front\)/);
  assert.match(verdict.summary, /share=onScreen/);
});

test('a border behind its source is still caught', () => {
  const verdict = classifyShareBorderStackReport(
    { ...HEALTHY_REPORT, border: { number: 145, stackIndex: 14 } },
    163
  );
  assert.equal(verdict.reason, 'border-behind-source');
});

test('a source absent from the on-screen stack is named as such', () => {
  const verdict = classifyShareBorderStackReport(
    { ...HEALTHY_REPORT, source: { number: 163, stackIndex: null } },
    163
  );
  assert.equal(verdict.reason, 'source-missing');
});

test('a Petal build that reports no share state degrades to the old attribution, loudly', () => {
  const { shareState, ...noState } = RELEASE_GATE_REPORT;
  assert.equal(sourceWindowIsGone(noState), false);
  assert.equal(shareStateSummary(noState), 'shareState=unreported');
  const verdict = classifyShareBorderStackReport(noState, 163);
  assert.equal(verdict.reason, 'border-missing');
  assert.match(verdict.message, /shareState=unreported/);
});

test('every failure message carries the full report', () => {
  for (const report of [
    RELEASE_GATE_REPORT,
    { ...HEALTHY_REPORT, border: { number: 145, stackIndex: 14 } },
    { ...HEALTHY_REPORT, source: { number: 163, stackIndex: null } },
  ]) {
    const verdict = classifyShareBorderStackReport(report, 163);
    assert.equal(verdict.ok, false);
    assert.match(verdict.message, /"windowId":163/);
  }
});

test('a missing report is named rather than throwing on property access', () => {
  const verdict = classifyShareBorderStackReport(null, 163);
  assert.equal(verdict.reason, 'no-report');
});

// The literal report the clean press-to-photon bootstrap threw on (run
// 40004629026, 18:51:13.790); the tracker reordered the border 14ms later.
const BEHIND_SOURCE_REPORT = {
  windowId: 212,
  source: { number: 212, stackIndex: 10 },
  border: { number: 155, stackIndex: 11 },
  overlays: [{ number: 159, stackIndex: 9 }],
  shareState: { share: 'onScreen', registered: true, trackerHidden: false, hideRequested: false },
};
const CONVERGED_REPORT = {
  ...BEHIND_SOURCE_REPORT,
  border: { number: 155, stackIndex: 9 },
  overlays: [{ number: 159, stackIndex: 8 }],
};

function fakeClock() {
  let t = 0;
  return {
    now: () => t,
    sleep: async (ms) => {
      t += ms;
    },
  };
}

// Serves `reports` in order, repeating the last one; `reads()` counts calls.
function reportsSequence(...reports) {
  let count = 0;
  const read = async () => reports[Math.min(count++, reports.length - 1)];
  return { read, reads: () => count };
}

test('a border behind its source for one tracker tick converges and passes, saying so', async () => {
  const { read, reads } = reportsSequence(BEHIND_SOURCE_REPORT, CONVERGED_REPORT);
  const verdict = await awaitShareBorderStacked(read, 212, fakeClock());
  assert.equal(verdict.ok, true);
  assert.equal(reads(), 2);
  assert.match(verdict.summary, /converged after 50ms; first read had the border behind its source/);
});

test('a border that stays behind its source past the settle bound still fails', async () => {
  const { read, reads } = reportsSequence(BEHIND_SOURCE_REPORT);
  const verdict = await awaitShareBorderStacked(read, 212, fakeClock());
  assert.equal(verdict.ok, false);
  assert.equal(verdict.reason, 'border-behind-source');
  assert.match(verdict.message, /still behind after the 500ms border-tracker settle bound/);
  assert.equal(reads(), 1 + BORDER_TRACKER_SETTLE_MS / 50);
});

test('only a behind-source read is re-checked; every other failure is final on the first read', async () => {
  for (const report of [
    RELEASE_GATE_REPORT,
    withShareState(RELEASE_GATE_REPORT, { trackerHidden: false }),
    null,
  ]) {
    const { read, reads } = reportsSequence(report, HEALTHY_REPORT);
    const verdict = await awaitShareBorderStacked(read, 163, fakeClock());
    assert.equal(verdict.ok, false, JSON.stringify(report));
    assert.equal(reads(), 1);
  }
});

test('a healthy first read passes without re-reading', async () => {
  const { read, reads } = reportsSequence(HEALTHY_REPORT, BEHIND_SOURCE_REPORT);
  const verdict = await awaitShareBorderStacked(read, 163, fakeClock());
  assert.equal(verdict.ok, true);
  assert.equal(reads(), 1);
  assert.doesNotMatch(verdict.summary, /converged/);
});

test('a behind-source read that turns into a lost source reports the lost source', async () => {
  const { read } = reportsSequence(BEHIND_SOURCE_REPORT, { ...RELEASE_GATE_REPORT, windowId: 212 });
  const verdict = await awaitShareBorderStacked(read, 212, fakeClock());
  assert.equal(verdict.reason, 'source-window-lost');
});
