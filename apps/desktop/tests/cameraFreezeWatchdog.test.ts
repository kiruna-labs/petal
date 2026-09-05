import assert from 'node:assert/strict';
import test from 'node:test';

import {
  FREEZE_WATCHDOG_TIMEOUT_MS,
  nextCameraFreezeState,
  isCameraFrameStale,
  framesDecodedFromStatsReport,
  nextCameraDecodeHealthState,
  formatCameraDecodeHealth,
  classifyCameraReceiveHealth
} from '../src/lib/data/cameraFreezeWatchdog.ts';

// #247: unit tests for the local camera freeze-watchdog decision logic
// (galleryBridge.ts has none of this today -- see the issue). Mirrors the
// native no_frame_watchdog_* tests in transport/subscriber.rs.

test('nextCameraFreezeState advances progress when framesDecoded increases', () => {
  const t0 = 1_000;
  const state0 = nextCameraFreezeState(undefined, 10, t0);
  assert.deepEqual(state0, { lastFramesDecoded: 10, lastProgressAt: t0 });

  const t1 = t0 + 500;
  const state1 = nextCameraFreezeState(state0, 11, t1);
  assert.deepEqual(state1, { lastFramesDecoded: 11, lastProgressAt: t1 });
});

test('nextCameraFreezeState does not advance progress when framesDecoded is unchanged', () => {
  const t0 = 1_000;
  const state0 = nextCameraFreezeState(undefined, 10, t0);

  const t1 = t0 + 5_000;
  const state1 = nextCameraFreezeState(state0, 10, t1);
  // Same object identity/value: no progress means lastProgressAt must NOT move.
  assert.deepEqual(state1, state0);
});

test('nextCameraFreezeState treats a null (stats unavailable) reading as a no-op, not a reset', () => {
  const t0 = 1_000;
  const state0 = nextCameraFreezeState(undefined, 10, t0);

  const t1 = t0 + 5_000;
  const state1 = nextCameraFreezeState(state0, null, t1);
  // A transient stats read failure must not itself look like "no progress
  // starting now" -- it must preserve the existing progress timestamp.
  assert.deepEqual(state1, state0);
});

test('nextCameraFreezeState seeds a fresh state on first null reading', () => {
  const t0 = 1_000;
  const state = nextCameraFreezeState(undefined, null, t0);
  assert.deepEqual(state, { lastFramesDecoded: -1, lastProgressAt: t0 });
});

test('isCameraFrameStale flags a tile stalled for at least the timeout', () => {
  const subscribedAt = 0;
  const state = nextCameraFreezeState(undefined, 5, subscribedAt);

  assert.equal(isCameraFrameStale(state, subscribedAt + FREEZE_WATCHDOG_TIMEOUT_MS - 1), false);
  assert.equal(isCameraFrameStale(state, subscribedAt + FREEZE_WATCHDOG_TIMEOUT_MS), true);
  assert.equal(isCameraFrameStale(state, subscribedAt + FREEZE_WATCHDOG_TIMEOUT_MS + 60_000), true);
});

test('isCameraFrameStale clears immediately once progress resumes', () => {
  const t0 = 0;
  let state = nextCameraFreezeState(undefined, 5, t0);

  const staleAt = t0 + FREEZE_WATCHDOG_TIMEOUT_MS + 5_000;
  assert.equal(isCameraFrameStale(state, staleAt), true);

  // A real frame arrives (framesDecoded increases): progress resets and the
  // tile is fresh again at that same instant.
  state = nextCameraFreezeState(state, 6, staleAt);
  assert.equal(isCameraFrameStale(state, staleAt), false);
});

test('framesDecodedFromStatsReport reads the video inbound-rtp entry', () => {
  const report = new Map([
    ['audio-in', { type: 'inbound-rtp', kind: 'audio', framesDecoded: 999 }],
    ['video-in', { type: 'inbound-rtp', kind: 'video', framesDecoded: 42 }]
  ]) as unknown as RTCStatsReport;
  assert.equal(framesDecodedFromStatsReport(report), 42);
});

test('framesDecodedFromStatsReport returns null for missing/empty reports', () => {
  assert.equal(framesDecodedFromStatsReport(undefined), null);
  const empty = new Map() as unknown as RTCStatsReport;
  assert.equal(framesDecodedFromStatsReport(empty), null);
  const noVideo = new Map([
    ['audio-in', { type: 'inbound-rtp', kind: 'audio', framesDecoded: 999 }]
  ]) as unknown as RTCStatsReport;
  assert.equal(framesDecodedFromStatsReport(noVideo), null);
});

test('nextCameraDecodeHealthState emits periodic decoded-fps telemetry', () => {
  const t0 = 1_000;
  const seeded = nextCameraDecodeHealthState(undefined, 10, t0, 5_000);
  assert.equal(seeded.health, null);

  const early = nextCameraDecodeHealthState(seeded.state, 20, t0 + 4_000, 5_000);
  assert.equal(early.health, null);
  assert.deepEqual(early.state, seeded.state);

  const due = nextCameraDecodeHealthState(seeded.state, 25, t0 + 5_000, 5_000);
  assert.deepEqual(due.health, {
    framesDecoded: 25,
    decodedFps: 3,
    gapSinceLastFrameMs: 0
  });
});

test('classifyCameraReceiveHealth emits only confirmed unhealthy buckets', () => {
  const cases: Array<{
    name: string;
    fps: number | null;
    paused: boolean;
    stale: boolean;
    expected: ReturnType<typeof classifyCameraReceiveHealth>;
  }> = [
    { name: 'missing stats', fps: null, paused: false, stale: false, expected: null },
    { name: 'missing stats with stale UI state', fps: null, paused: false, stale: true, expected: null },
    { name: 'NaN stats', fps: Number.NaN, paused: false, stale: false, expected: null },
    { name: 'healthy', fps: 24, paused: false, stale: false, expected: null },
    {
      name: 'reduced',
      fps: 10,
      paused: false,
      stale: false,
      expected: { cadence: 'reduced', decoderRender: 'decoder_degraded' }
    },
    {
      name: 'severe',
      fps: 1,
      paused: false,
      stale: false,
      expected: { cadence: 'severe', decoderRender: 'decoder_degraded' }
    },
    {
      name: 'zero',
      fps: 0,
      paused: false,
      stale: false,
      expected: { cadence: 'stalled', decoderRender: 'decoder_degraded' }
    },
    {
      name: 'paused',
      fps: 30,
      paused: true,
      stale: false,
      expected: { cadence: 'stalled', decoderRender: 'decoder_degraded' }
    },
    {
      name: 'stale',
      fps: 30,
      paused: false,
      stale: true,
      expected: { cadence: 'stalled', decoderRender: 'decoder_degraded' }
    }
  ];

  for (const vector of cases) {
    assert.deepEqual(
      classifyCameraReceiveHealth(vector.fps, vector.paused, vector.stale),
      vector.expected,
      vector.name
    );
  }
});

test('missing stats stay unavailable through the periodic health composition', () => {
  const seeded = nextCameraDecodeHealthState(undefined, 12, 1_000, 5_000);
  const interval = nextCameraDecodeHealthState(seeded.state, null, 6_000, 5_000);
  assert.equal(interval.health?.framesDecoded, null);
  assert.equal(
    classifyCameraReceiveHealth(
      interval.health?.framesDecoded === null ? null : (interval.health?.decodedFps ?? null),
      true,
      true
    ),
    null,
    'unavailable stats must not become a paused/stale diagnostic'
  );
});

test('formatCameraDecodeHealth preserves the log contract fields', () => {
  assert.equal(
    formatCameraDecodeHealth({
      identity: 'alice',
      framesDecoded: 42,
      decodedFps: 29.94,
      gapSinceLastFrameMs: 120
    }),
    "gallery bridge: camera decode health for 'alice' -- frames_decoded=42 decoded_fps=29.9 gap_since_last_frame_ms=120"
  );
});
