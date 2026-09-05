import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  deriveRemoteWindowStats,
  formatBitrate,
  formatRemoteWindowDebugStats,
  formatRemoteWindowFreshness,
  sparkPath,
} from '../src/remoteWindowStats.ts';

function statsReport(entries: Array<Record<string, unknown>>): RTCStatsReport {
  return {
    forEach(callback: (value: unknown) => void) {
      entries.forEach((entry) => callback(entry));
    },
  } as unknown as RTCStatsReport;
}

test('deriveRemoteWindowStats computes bitrate, decoded FPS, and freeze age from WebRTC stats deltas', () => {
  const first = deriveRemoteWindowStats(
    statsReport([
      {
        type: 'inbound-rtp',
        kind: 'video',
        bytesReceived: 1000,
        framesReceived: 10,
        framesDecoded: 10,
        packetsLost: 2,
        jitter: 0.004,
        freezeCount: 1,
        framesDropped: 3,
        qualityLimitationReason: 'cpu',
      },
    ]),
    null,
    1000,
    { width: 640, height: 480 },
    { totalVideoFrames: 10 }
  );

  assert.equal(first.snapshot.width, 640);
  assert.equal(first.snapshot.height, 480);
  assert.equal(first.snapshot.secondsSinceLastDecodedFrame, 0);
  assert.equal(first.snapshot.secondsSinceLastPresentedFrame, 0);
  assert.equal(first.snapshot.bitrateBps, null);
  assert.equal(first.snapshot.presentedFps, null);
  assert.equal(first.snapshot.jitterMs, 4);
  assert.equal(first.snapshot.freezeCount, 1);
  assert.equal(first.snapshot.framesDropped, 3);
  assert.equal(first.snapshot.qualityLimitationReason, 'cpu');

  const second = deriveRemoteWindowStats(
    statsReport([
      {
        type: 'inbound-rtp',
        kind: 'video',
        bytesReceived: 126000,
        framesReceived: 40,
        framesDecoded: 40,
        frameWidth: 1280,
        frameHeight: 720,
      },
    ]),
    first.state,
    2000,
    undefined,
    { totalVideoFrames: 38 }
  );

  assert.equal(second.snapshot.fps, 30);
  assert.equal(second.snapshot.presentedFps, 28);
  assert.equal(second.snapshot.bitrateBps, 1_000_000);
  assert.equal(second.snapshot.width, 1280);
  assert.equal(second.snapshot.height, 720);
  assert.equal(second.snapshot.secondsSinceLastDecodedFrame, 0);
  assert.equal(second.snapshot.secondsSinceLastPresentedFrame, 0);

  const frozen = deriveRemoteWindowStats(
    statsReport([
      {
        type: 'inbound-rtp',
        kind: 'video',
        bytesReceived: 126000,
        framesReceived: 40,
        framesDecoded: 40,
      },
    ]),
    second.state,
    4500,
    undefined,
    { totalVideoFrames: 38 }
  );

  assert.equal(frozen.snapshot.fps, 0);
  assert.equal(frozen.snapshot.presentedFps, 0);
  assert.equal(frozen.snapshot.bitrateBps, 0);
  assert.equal(frozen.snapshot.secondsSinceLastDecodedFrame, 2.5);
  assert.equal(frozen.snapshot.secondsSinceLastPresentedFrame, 2.5);
});

test('formatRemoteWindowDebugStats keeps compact visible labels for the overlay', () => {
  const lines = formatRemoteWindowDebugStats({
    fps: 29.97,
    width: 1920,
    height: 1080,
    bitrateBps: 2_400_000,
    framesReceived: 120,
    framesDecoded: 118,
    framesPresented: 112,
    presentedFps: 27.5,
    secondsSinceLastDecodedFrame: 1.25,
    secondsSinceLastPresentedFrame: 1.25,
    packetsLost: 0,
    jitterMs: 3.6,
    freezeCount: null,
    framesDropped: null,
    qualityLimitationReason: null,
  });

  assert.deepEqual(lines.map((line) => line.label), [
    'Last frame',
    'FPS',
    'Size',
    'Bitrate',
    'Frames',
    'Presented',
    'Lost',
    'Jitter',
  ]);
  assert.equal(lines[0].value, '1.3s');
  assert.equal(lines[0].prominent, true);
  assert.equal(lines[2].value, '1920x1080');
  assert.equal(lines[3].value, '2.4 Mbps');
  assert.equal(lines[4].value, '120 / 118');
  assert.equal(lines[5].value, '112 / 27.5 fps');
  assert.equal(lines[7].value, '3.6ms');
  assert.equal(formatBitrate(null), 'warming up');
});

test('formatRemoteWindowFreshness distinguishes idle keepalive timing from a stale stream', () => {
  assert.equal(formatRemoteWindowFreshness(9_000, 10_000), 'live · updated 1s ago');
  assert.equal(formatRemoteWindowFreshness(6_000, 10_000), 'stale · updated 4s ago');
  assert.equal(formatRemoteWindowFreshness(null, 10_000), 'waiting · no frame received yet');
});

test('sparkPath draws nothing for fewer than two real samples', () => {
  assert.equal(sparkPath([]), '');
  assert.equal(sparkPath([5]), '');
  assert.equal(sparkPath([null, null]), '');
});

test('sparkPath spans the full width/height box and starts a new segment across a gap', () => {
  const path = sparkPath([1, 2, 3], 120, 28);
  // Ascending trend [1,2,3]: the first point (lowest value) sits near the
  // bottom of the box (y=26), the last point (highest value) near the top
  // (y=2) -- "up" in the path means "up" in the trend.
  assert.equal(path, 'M0.0 26.0L60.0 14.0L120.0 2.0');

  // A null sample breaks the line instead of interpolating across it.
  const withGap = sparkPath([1, null, 3], 120, 28);
  const segments = withGap.split('M').filter(Boolean);
  assert.equal(segments.length, 2);
});

test('sparkPath draws a flat line when every sample is equal', () => {
  const path = sparkPath([4, 4, 4]);
  assert.doesNotMatch(path, /NaN/);
  assert.match(path, /^M0\.0 \d+\.0L/);
});
