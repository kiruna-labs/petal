import assert from 'node:assert/strict';
import test from 'node:test';

import {
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
