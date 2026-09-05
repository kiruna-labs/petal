import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  screenSharePublishEncoding,
  screenShareMaxBitrate,
  SCREENSHARE_MAX_FRAMERATE,
  TEST_PATTERN_SCREENSHARE_ENCODING
} from '../src/constants.ts';

// These tests stand in for an end-to-end scenario that CANNOT be written:
// `getDisplayMedia` opens a browser picker that unattended automation cannot
// click, so every cockpit scenario drives the synthetic test-pattern share
// instead. The real user-facing share path has therefore never been covered
// by any scenario -- and that is exactly where the 2026-09-01 field bug lived
// (a 2560x1600 desktop delivered as 320x180 at 15fps for a 37-minute call).

test('the real screen share always sets an explicit encoding', () => {
  // The defect was ABSENCE: with no encoding, livekit applies
  // ScreenSharePresets.h1080fps15 -- 2.5 Mbps, 15fps -- and the encoder sheds
  // resolution to fit. Any real encoding must beat that default outright.
  const encoding = screenSharePublishEncoding(2560, 1600);
  assert.ok(encoding.maxBitrate > 2_500_000, 'must exceed livekit default 2.5 Mbps');
  assert.ok(encoding.maxFramerate > 15, 'must exceed livekit default 15fps');
  assert.equal(encoding.maxFramerate, SCREENSHARE_MAX_FRAMERATE);
});

test('the bitrate ceiling scales with the captured pixel count', () => {
  // Mirrors the native ladder in transport/publisher.rs `video_encoding`, so a
  // browser sharer and a native sharer of the same display look alike.
  assert.equal(screenShareMaxBitrate(1280, 720), 4_000_000);
  assert.equal(screenShareMaxBitrate(1920, 1080), 8_000_000);
  assert.equal(screenShareMaxBitrate(2560, 1440), 12_000_000);
  assert.equal(screenShareMaxBitrate(3840, 2160), 18_000_000);
  // Strictly non-decreasing: a bigger capture must never get a smaller ceiling.
  const ladder = [
    screenShareMaxBitrate(1280, 720),
    screenShareMaxBitrate(1920, 1080),
    screenShareMaxBitrate(2560, 1440),
    screenShareMaxBitrate(3840, 2160)
  ];
  for (let i = 1; i < ladder.length; i += 1) {
    assert.ok(ladder[i] >= ladder[i - 1], 'bitrate ladder must be monotonic');
  }
});

test('unknown captured dimensions still get a real ceiling, never the default', () => {
  // `getSettings()` can return undefined width/height. Falling back to 0 must
  // not collapse to a tiny ceiling -- that would reproduce the original bug on
  // exactly the browsers that report nothing.
  for (const encoding of [
    screenSharePublishEncoding(undefined, undefined),
    screenSharePublishEncoding(0, 0)
  ]) {
    assert.ok(encoding.maxBitrate >= 8_000_000, 'unknown size must assume a typical display');
    assert.equal(encoding.maxFramerate, SCREENSHARE_MAX_FRAMERATE);
  }
});

test('the real share path and the test-pattern path both pin an encoding', () => {
  // The parity the source comment claims. The test-pattern share had an
  // explicit encoding and the real share did not, so the scenario suite was
  // green while production was broken. Neither may fall through to livekit's
  // default again.
  assert.ok(TEST_PATTERN_SCREENSHARE_ENCODING.maxBitrate > 2_500_000);
  assert.ok(TEST_PATTERN_SCREENSHARE_ENCODING.maxFramerate > 15);
  assert.ok(screenSharePublishEncoding(1920, 1080).maxBitrate > 2_500_000);
});

test('the real share publishes with maintain-resolution and an explicit encoding', () => {
  // Source-level because the publish call itself needs a live Room and a
  // picker click. `maintain-resolution` is the load-bearing half: it makes the
  // encoder drop FRAMES rather than PIXELS, which is what keeps text legible.
  const controls = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
  const realShare = controls.slice(controls.indexOf('getDisplayMedia'));
  const publishCall = realShare.slice(0, realShare.indexOf('setLocalParticipantMetadata'));
  assert.match(publishCall, /screenShareEncoding:\s*shareEncoding/);
  assert.match(publishCall, /degradationPreference:\s*'maintain-resolution'/);
  assert.match(publishCall, /videoCodec:\s*'h264'/);
  assert.match(publishCall, /contentHint = 'detail'/);
});
