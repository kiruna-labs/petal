import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { CAMERA_VIDEO_CONSTRAINTS, CAMERA_VIDEO_ENCODING } from '../src/constants.ts';

test('browser cameras request and encode 720p30 with 2.5 Mbps headroom', () => {
  assert.deepEqual(CAMERA_VIDEO_CONSTRAINTS, {
    width: { ideal: 1280 },
    height: { ideal: 720 },
    frameRate: { ideal: 30, max: 30 },
  });
  assert.deepEqual(CAMERA_VIDEO_ENCODING, {
    maxBitrate: 2_500_000,
    maxFramerate: 30,
  });
});

const controls = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');

test('every web camera publish pins one full-resolution encoding', () => {
  const cameraPublishes = controls.split("source: Track.Source.Camera,").slice(1);
  assert.ok(cameraPublishes.length >= 2, 'expected the synthetic and real webcam publish paths');
  for (const publish of cameraPublishes) {
    const options = publish.slice(0, publish.indexOf('});'));
    assert.match(options, /videoEncoding: CAMERA_VIDEO_ENCODING/);
    assert.match(options, /simulcast: false/);
    assert.match(options, /degradationPreference: 'maintain-resolution'/);
    assert.doesNotMatch(options, /videoSimulcastLayers/);
  }
});
