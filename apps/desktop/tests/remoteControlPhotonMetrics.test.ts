import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  nearestRankPercentile,
  photonStageSplit,
  summarizePhotonSamples
} from '../scripts/remote-control-photon-metrics.mjs';

test('press-to-photon percentile uses nearest-rank semantics', () => {
  const values = Array.from({ length: 20 }, (_, index) => index + 1);
  assert.equal(nearestRankPercentile(values, 0.5), 10);
  assert.equal(nearestRankPercentile(values, 0.95), 19);
  assert.equal(nearestRankPercentile([], 0.95), null);
});

test('press-to-photon summary gates overall and each input kind at p95', () => {
  const samples = [
    ...Array.from({ length: 20 }, (_, index) => ({
      inputKind: 'text',
      pressToEstimatedPhotonMs: 80 + index
    })),
    ...Array.from({ length: 20 }, (_, index) => ({
      inputKind: 'click',
      pressToEstimatedPhotonMs: 110 + index
    }))
  ];

  const passing = summarizePhotonSamples(samples, 150);
  assert.equal(passing.pass, true);
  assert.equal(passing.samples, 40);
  assert.equal(passing.byInput.text.p95Ms, 98);
  assert.equal(passing.byInput.click.p95Ms, 128);

  const failing = summarizePhotonSamples(samples, 100);
  assert.equal(failing.pass, false);
  assert.equal(failing.byInput.text.pass, true);
  assert.equal(failing.byInput.click.pass, false);
});

test('press-to-photon stage split joins the sentinel ledger and host log by host clock', () => {
  const sent = 1_000_000;
  const sample = { sentAtEpochMs: sent, observedGeneration: 7, pressToEstimatedPhotonMs: 812.4 };
  const ledger = [
    { kind: 'event', type: 'keyDown', tMs: sent - 400 },
    { kind: 'redraw', generation: 6, tMs: sent - 390 },
    { kind: 'cursor', tMs: sent + 2 },
    { kind: 'event', type: 'keyDown', tMs: sent + 14 },
    { kind: 'axAction', action: 'setValue', tMs: sent + 15 },
    { kind: 'redraw', generation: 7, tMs: sent + 16.2 }
  ];
  const hostLog = [
    `x remote-control-latency: host inject_ts_ms=${sent - 500} kind=Text action=None window=9 seq=11 controller='c'`,
    `x remote-control-latency: host inject_ts_ms=${sent + 9} kind=Text action=None window=9 seq=12 controller='c'`,
    `x remote-control-latency: host frame_pushed_ts_ms=${sent + 40} window=9 input_seq=12 since_input_ms=31 path=snapshot accepted=true frame_id=3`,
    `x remote-control-latency: host frame_pushed_ts_ms=${sent + 52} window=9 input_seq=12 since_input_ms=43 path=raw accepted=false frame_id=4`,
    `x remote-control-latency: host frame_pushed_ts_ms=${sent + 60} window=9 input_seq=11 since_input_ms=560 path=raw accepted=true frame_id=5`
  ].join('\n');
  assert.deepEqual(photonStageSplit(sample, ledger, hostLog), {
    hostSeq: 12,
    injectMs: 9,
    sentinelInputMs: 14,
    sentinelRedrawMs: 16.2,
    pushes: [
      { atMs: 40, path: 'snapshot', accepted: true },
      { atMs: 52, path: 'raw', accepted: false }
    ],
    photonMs: 812.4
  });
  assert.equal(photonStageSplit({ observedGeneration: 1 }, ledger, hostLog), null);
});
