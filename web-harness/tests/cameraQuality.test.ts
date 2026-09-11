import assert from 'node:assert/strict';
import { test } from 'node:test';

import { cameraChartReference, measureCameraQuality } from '../src/cameraQuality.ts';

test('camera chart reference is aligned and self-consistent', () => {
  const reference = cameraChartReference(320, 240);
  const result = measureCameraQuality(reference);
  assert.equal(result.aligned, true);
  assert.ok(result.ssim > 0.99);
  assert.equal(result.psnr, Number.POSITIVE_INFINITY);
  assert.equal(result.edgeRatio, 1);
});

test('camera quality rejects a flat/downsampled negative control', () => {
  const reference = cameraChartReference(320, 240);
  const blurred = {
    ...reference,
    data: new Uint8ClampedArray(reference.data.length).fill(96),
  };
  const result = measureCameraQuality(blurred);
  assert.equal(result.aligned, false);
});
