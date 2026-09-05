import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  nearestRankPercentile,
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
