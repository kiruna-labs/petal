import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  evaluateRemoteCameraTile,
  type RemoteCameraSample,
} from '../src/cameraFrameAdvance.ts';

// ---------------------------------------------------------------------------
// CAM-N2W's verdict logic (journey CAM-05, #815). The cases that matter here
// are the two the scenario exists to tell apart: a tile that is genuinely
// black or frozen (a PRODUCT failure), and a viewer that could not see at all
// (an INSTRUMENT failure). Reporting the second as the first is how #821
// turned a working product into a P0.
// ---------------------------------------------------------------------------

/** A healthy window: advancing, bright, changing, with the canvas control green. */
function visibleSample(overrides: Partial<RemoteCameraSample> = {}): RemoteCameraSample {
  return {
    readyState: 4,
    videoWidth: 640,
    videoHeight: 480,
    frameCallbackCount: 118,
    framesDecodedDelta: 120,
    windowMs: 4000,
    canvasControlOk: true,
    sampledFrames: 2,
    maxLuma: 235,
    nonBlackRatio: 0.98,
    interFrameDiff: 21.4,
    ...overrides,
  };
}

test('a live, bright, changing camera tile passes', () => {
  const verdict = evaluateRemoteCameraTile(visibleSample());
  assert.equal(verdict.ok, true);
  assert.equal(verdict.classification, 'PASS');
  assert.match(verdict.detail, /640x480/);
});

// The mutation check the issue asks for: freeze the source and the assertion
// must go red. `PETAL_CAMERA_SYNTH_FREEZE=1` produces exactly this shape on
// the native side.
test('a frozen source fails as a product failure, not an instrument one', () => {
  const noFrames = evaluateRemoteCameraTile(
    visibleSample({ frameCallbackCount: 0, framesDecodedDelta: 0 })
  );
  assert.equal(noFrames.ok, false);
  assert.equal(noFrames.classification, 'TEST-FAIL');
  assert.match(noFrames.detail, /not advancing/);

  // The subtler freeze: decode counters keep climbing (a held last frame is
  // still re-rendered) while the picture never changes. Counters alone call
  // this healthy, which is why the pixel-difference sample exists.
  const heldFrame = evaluateRemoteCameraTile(visibleSample({ interFrameDiff: 0 }));
  assert.equal(heldFrame.ok, false);
  assert.equal(heldFrame.classification, 'TEST-FAIL');
  assert.match(heldFrame.detail, /FROZEN/);
});

test('a subscribed-but-black tile is a product failure', () => {
  const verdict = evaluateRemoteCameraTile(
    visibleSample({ maxLuma: 3, nonBlackRatio: 0, interFrameDiff: 0.2 })
  );
  assert.equal(verdict.ok, false);
  assert.equal(verdict.classification, 'TEST-FAIL');
  assert.match(verdict.detail, /BLACK/);
});

// The case that must NEVER read as a product failure. Everything about this
// sample looks like the black tile above -- the only difference is that the
// canvas could not report back a colour it was handed, so its reading is not
// evidence about anything.
test('a blind viewer is an infrastructure failure, never a product one', () => {
  const verdict = evaluateRemoteCameraTile(
    visibleSample({ canvasControlOk: false, maxLuma: 0, nonBlackRatio: 0, interFrameDiff: 0 })
  );
  assert.equal(verdict.ok, false);
  assert.equal(verdict.classification, 'INFRA-FAIL');
  assert.match(verdict.detail, /positive control/);
});

// A single readback cannot answer the freeze question, so it must not be
// allowed to answer it optimistically either. Found by review, not by a
// failing run: the plumbing returns one sample whenever the tile is still
// decoding at the first sample point.
test('a single readback is not enough to judge a tile', () => {
  const verdict = evaluateRemoteCameraTile(visibleSample({ sampledFrames: 1 }));
  assert.equal(verdict.ok, false);
  assert.equal(verdict.classification, 'INFRA-FAIL');
  assert.match(verdict.detail, /held frame/);
});

test('a window with no readback at all is an infrastructure failure', () => {
  const verdict = evaluateRemoteCameraTile(visibleSample({ sampledFrames: 0 }));
  assert.equal(verdict.ok, false);
  assert.equal(verdict.classification, 'INFRA-FAIL');
});

test('counters that went backwards mid-window are not measurable', () => {
  const verdict = evaluateRemoteCameraTile(visibleSample({ framesDecodedDelta: -5 }));
  assert.equal(verdict.ok, false);
  assert.equal(verdict.classification, 'INFRA-FAIL');
  assert.match(verdict.detail, /backwards/);
});

test('a tile with no decoded picture fails on dimensions and readyState', () => {
  const noSize = evaluateRemoteCameraTile(visibleSample({ videoWidth: 0, videoHeight: 0 }));
  assert.equal(noSize.ok, false);
  assert.equal(noSize.classification, 'TEST-FAIL');

  const notReady = evaluateRemoteCameraTile(visibleSample({ readyState: 1 }));
  assert.equal(notReady.ok, false);
  assert.equal(notReady.classification, 'TEST-FAIL');
});

// One of the two frame bars is allowed to be missing (a browser without
// `framesDecoded` in its stats is still a usable viewer), but not both.
test('either frame bar alone can carry the advancing proof', () => {
  const statsOnly = evaluateRemoteCameraTile(
    visibleSample({ frameCallbackCount: 0, framesDecodedDelta: 120 })
  );
  assert.equal(statsOnly.ok, true);

  const callbacksOnly = evaluateRemoteCameraTile(
    visibleSample({ frameCallbackCount: 118, framesDecodedDelta: null })
  );
  assert.equal(callbacksOnly.ok, true);
  assert.match(callbacksOnly.detail, /unavailable/);
});
