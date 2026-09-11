import assert from 'node:assert/strict';
import test from 'node:test';

import {
  FREEZE_WATCHDOG_TIMEOUT_MS,
  nextCameraFreezeState,
  isCameraFrameStale,
  framesDecodedFromStatsReport,
  cameraInboundVideoStatFromReport,
  cameraReceiveStatsFromStatsReport,
  nextCameraDecodeHealthState,
  formatCameraDecodeHealth,
  classifyCameraReceiveHealth,
  composeCameraReceiveObservation,
  cameraPresentedFps
} from '../src/lib/data/cameraFreezeWatchdog.ts';

// #247: unit tests for the local camera freeze-watchdog decision logic
// (galleryBridge.ts has none of this today -- see the issue). Mirrors the
// native no_frame_watchdog_* tests in transport/subscriber.rs.

test('cameraPresentedFps measures WebView presentation progress independently', () => {
  assert.equal(cameraPresentedFps(10, 25, 5_000), 3);
  assert.equal(cameraPresentedFps(25, 10, 5_000), 0);
  assert.equal(cameraPresentedFps(10, 25, 0), 0);
});

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

test('cameraReceiveStatsFromStatsReport captures decoder, render, loss, and freeze counters', () => {
  const report = new Map([
    [
      'video-in',
      {
        type: 'inbound-rtp',
        kind: 'video',
        framesDecoded: 42,
        framesPerSecond: 29.5,
        framesDropped: 3,
        freezeCount: 2,
        totalFreezesDuration: 1.25,
        packetsLost: 4,
        nackCount: 7,
        bytesReceived: 120_000,
        packetsReceived: 900,
        packetsDiscarded: 2,
        retransmittedPacketsReceived: 8,
        keyFramesDecoded: 3,
        pliCount: 4,
        firCount: 1,
        jitterBufferDelay: 0.045,
        jitterBufferEmittedCount: 40,
        totalDecodeTime: 0.8,
        decoderImplementation: 'hardware H264'
      }
    ]
  ]) as unknown as RTCStatsReport;
  const stats = cameraReceiveStatsFromStatsReport(report);
  assert.ok(stats, 'a video inbound report must produce receiver stats');
  assert.deepEqual(
    {
      framesDecoded: stats.framesDecoded,
      framesPerSecond: stats.framesPerSecond,
      framesDropped: stats.framesDropped,
      freezeCount: stats.freezeCount,
      totalFreezesDurationMs: stats.totalFreezesDurationMs,
      packetsLost: stats.packetsLost,
      nackCount: stats.nackCount,
      bytesReceived: stats.bytesReceived,
      packetsReceived: stats.packetsReceived,
      packetsDiscarded: stats.packetsDiscarded,
      retransmittedPacketsReceived: stats.retransmittedPacketsReceived,
      keyFramesDecoded: stats.keyFramesDecoded,
      pliCount: stats.pliCount,
      firCount: stats.firCount,
      jitterBufferDelayMs: stats.jitterBufferDelayMs,
      jitterBufferEmittedCount: stats.jitterBufferEmittedCount,
      totalDecodeTimeMs: stats.totalDecodeTimeMs,
      decoderImplementation: stats.decoderImplementation
    },
    {
      framesDecoded: 42,
      framesPerSecond: 29.5,
      framesDropped: 3,
      freezeCount: 2,
      totalFreezesDurationMs: 1250,
      packetsLost: 4,
      nackCount: 7,
      bytesReceived: 120_000,
      packetsReceived: 900,
      packetsDiscarded: 2,
      retransmittedPacketsReceived: 8,
      keyFramesDecoded: 3,
      pliCount: 4,
      firCount: 1,
      jitterBufferDelayMs: 45,
      jitterBufferEmittedCount: 40,
      totalDecodeTimeMs: 800,
      decoderImplementation: 'hardware H264'
    }
  );
  assert.ok(stats.lossPct !== null && Math.abs(stats.lossPct - 400 / 904) < 1e-12);
  // A report with no dimensions/jitter/render counters stays honestly
  // unavailable rather than substituting a guess.
  assert.equal(stats.decodedWidth, null);
  assert.equal(stats.decodedHeight, null);
  assert.equal(stats.framesReceived, null);
  assert.equal(stats.framesRendered, null);
  assert.equal(stats.jitterMs, null);
});

test('framesDecodedFromStatsReport returns null for missing/empty reports', () => {
  assert.equal(framesDecodedFromStatsReport(undefined), null);
  const empty = new Map() as unknown as RTCStatsReport;
  assert.equal(framesDecodedFromStatsReport(empty), null);
  const noVideo = new Map([
    ['audio-in', { type: 'inbound-rtp', kind: 'audio', framesDecoded: 999 }]
  ]) as unknown as RTCStatsReport;
  assert.equal(framesDecodedFromStatsReport(noVideo), null);
  assert.equal(cameraReceiveStatsFromStatsReport(noVideo), null);
});

test('cameraReceiveStatsFromStatsReport extracts jitter, loss, and render counters', () => {
  const report = new Map([
    [
      'inbound',
      {
        type: 'inbound-rtp',
        kind: 'video',
        jitter: 0.012,
        packetsLost: 2,
        packetsReceived: 98,
        framesRendered: 47
      }
    ]
  ]) as unknown as RTCStatsReport;
  const stats = cameraReceiveStatsFromStatsReport(report);
  assert.ok(stats, 'a video inbound report must produce receiver stats');
  assert.deepEqual(
    {
      jitterMs: stats.jitterMs,
      lossPct: stats.lossPct,
      decodedWidth: stats.decodedWidth,
      decodedHeight: stats.decodedHeight,
      framesReceived: stats.framesReceived,
      bytesReceived: stats.bytesReceived,
      framesRendered: stats.framesRendered
    },
    {
      jitterMs: 12,
      lossPct: 2,
      decodedWidth: null,
      decodedHeight: null,
      framesReceived: null,
      bytesReceived: null,
      framesRendered: 47
    }
  );
});

test('receiver dimensions and counters come from the same primary inbound report', () => {
  const entries = [
    ['small', {
      type: 'inbound-rtp', kind: 'video', frameWidth: 320, frameHeight: 180,
      framesDecoded: 10, framesReceived: 11, bytesReceived: 1000, framesRendered: 9
    }],
    ['large', {
      type: 'inbound-rtp', kind: 'video', frameWidth: 1280, frameHeight: 720,
      framesDecoded: 42, framesReceived: 44, bytesReceived: 4000, framesRendered: 40
    }]
  ] as const;
  const report = new Map(entries) as unknown as RTCStatsReport;
  assert.equal(framesDecodedFromStatsReport(report), 42);
  const stats = cameraReceiveStatsFromStatsReport(report);
  assert.ok(stats, 'the primary report must produce stats');
  assert.deepEqual(
    {
      decodedWidth: stats.decodedWidth,
      decodedHeight: stats.decodedHeight,
      framesDecoded: stats.framesDecoded,
      framesReceived: stats.framesReceived,
      bytesReceived: stats.bytesReceived,
      framesRendered: stats.framesRendered
    },
    {
      decodedWidth: 1280,
      decodedHeight: 720,
      framesDecoded: 42,
      framesReceived: 44,
      bytesReceived: 4000,
      framesRendered: 40
    }
  );
  // The primary-report choice must be selected by decoded area, not by
  // Map insertion order: the same two reports listed small-first select
  // identically to large-first.
  assert.equal(cameraInboundVideoStatFromReport(report), entries[1][1]);
  const reversed = new Map([entries[1], entries[0]]) as unknown as RTCStatsReport;
  const reversedStats = cameraReceiveStatsFromStatsReport(reversed);
  assert.ok(reversedStats, 'the primary report must produce stats when listed first');
  assert.equal(reversedStats.decodedWidth, 1280);
  assert.equal(reversedStats.framesDecoded, 42);
  assert.equal(reversedStats.bytesReceived, 4000);
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
    gapSinceLastFrameMs: 0,
    intervalMs: 5_000,
    intervalSequence: 1
  });
});

test('six-second reduced cadence stays active while stalled cadence is stalled', () => {
  const reduced = composeCameraReceiveObservation(
    classifyCameraReceiveHealth(13, false, false), false, false
  );
  assert.deepEqual(reduced, {
    cadence: 'reduced',
    streamState: 'active',
    degraded: true,
    stallCause: 'not_applicable'
  });

  const stalled = composeCameraReceiveObservation(
    classifyCameraReceiveHealth(0, false, false), false, false
  );
  assert.deepEqual(stalled, {
    cadence: 'stalled',
    streamState: 'stalled',
    degraded: false,
    stallCause: 'decode_zero'
  });

  // #126 semantics survive composition: an SFU pause keeps its own state and
  // its pause cause instead of being flattened into a decoder-fault stall.
  const paused = composeCameraReceiveObservation(
    classifyCameraReceiveHealth(0, true, true), true, true
  );
  assert.deepEqual(paused, {
    cadence: 'stalled',
    streamState: 'paused',
    degraded: false,
    stallCause: 'stream_paused'
  });
});

test('decode interval state resets when a publication SID changes', () => {
  const first = nextCameraDecodeHealthState(undefined, 100, 0, 5_000, 'TR_old');
  const replacement = nextCameraDecodeHealthState(first.state, 1, 6_000, 5_000, 'TR_new');
  assert.equal(replacement.health, null);
  assert.deepEqual(replacement.state, {
    lastLoggedAt: 6_000,
    lastLoggedFramesDecoded: 1,
    intervalSequence: 0,
    trackSid: 'TR_new'
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
      expected: {
        cadence: 'reduced',
        decoderRender: 'decoder_degraded',
        stallCause: 'not_applicable'
      }
    },
    {
      name: 'severe',
      fps: 1,
      paused: false,
      stale: false,
      expected: {
        cadence: 'severe',
        decoderRender: 'decoder_degraded',
        stallCause: 'not_applicable'
      }
    },
    // #126: the three rows below all read `cadence: 'stalled'` and are three
    // unrelated faults. `stallCause` is the only thing separating them; if it
    // ever collapses back to one value these rows go red together.
    {
      name: 'zero decode rate, not yet stale',
      fps: 0,
      paused: false,
      stale: false,
      expected: {
        cadence: 'stalled',
        decoderRender: 'decoder_degraded',
        stallCause: 'decode_zero'
      }
    },
    {
      name: 'paused by the SFU (network, not decoder)',
      fps: 30,
      paused: true,
      stale: false,
      expected: {
        cadence: 'stalled',
        decoderRender: 'not_applicable',
        stallCause: 'stream_paused'
      }
    },
    {
      name: 'paused and stale -- the pause is why decode stopped',
      fps: 0,
      paused: true,
      stale: true,
      expected: {
        cadence: 'stalled',
        decoderRender: 'not_applicable',
        stallCause: 'stream_paused'
      }
    },
    {
      name: 'confirmed 30s decode stall',
      fps: 30,
      paused: false,
      stale: true,
      expected: {
        cadence: 'stalled',
        decoderRender: 'decoder_degraded',
        stallCause: 'decode_stale'
      }
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

// #126: 1,002 receive-side `camera-health` events were byte-identical because
// three conditions shared one output. This asserts the property that made them
// unactionable is gone -- the three stalled conditions must not collapse onto
// one signal again, whatever the individual field values become.
test('the three stalled conditions stay distinguishable from one another', () => {
  const streamPaused = classifyCameraReceiveHealth(0, true, false);
  const decodeStale = classifyCameraReceiveHealth(30, false, true);
  const decodeZero = classifyCameraReceiveHealth(0, false, false);

  for (const signal of [streamPaused, decodeStale, decodeZero]) {
    assert.ok(signal, 'each stalled condition must emit a signal');
    assert.equal(signal?.cadence, 'stalled', 'all three still read cadence=stalled');
  }

  const distinct = new Set(
    [streamPaused, decodeStale, decodeZero].map((signal) => JSON.stringify(signal))
  );
  assert.equal(
    distinct.size,
    3,
    'an SFU pause, a confirmed decode stall, and a zero decode rate must not ' +
      'produce identical events (#126)'
  );

  // A track the SFU paused has a healthy decoder; reporting a decoder fault
  // for it is what sent triage after the wrong subsystem.
  assert.equal(streamPaused?.decoderRender, 'not_applicable');
  assert.equal(decodeStale?.decoderRender, 'decoder_degraded');
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
      gapSinceLastFrameMs: 120,
      intervalMs: 5_000,
      intervalSequence: 1
    }),
    "gallery bridge: camera decode health for 'alice' -- frames_decoded=42 decoded_fps=29.9 browser_fps=unknown frames_dropped=unknown freeze_count=unknown freeze_ms=unknown packets_received=unknown packets_lost=unknown packets_discarded=unknown retransmitted_packets=unknown bytes_received=unknown nack=unknown pli=unknown fir=unknown key_frames_decoded=unknown jitter_buffer_ms=unknown jitter_buffer_emitted=unknown decode_ms=unknown decoder='unknown' gap_since_last_frame_ms=120"
  );
});
