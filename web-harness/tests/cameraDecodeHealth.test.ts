import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  formatCameraDecodeHealth,
  framesDecodedFromStatsReport,
  nextCameraDecodeHealthState,
} from '../src/cameraDecodeHealth.ts';

function statsReport(entries: Array<Record<string, unknown>>): RTCStatsReport {
  return new Map(entries.map((entry, index) => [`stat-${index}`, entry])) as unknown as RTCStatsReport;
}

test('extracts video inbound framesDecoded from RTC stats', () => {
  const report = statsReport([
    { type: 'inbound-rtp', kind: 'audio', framesDecoded: 100 },
    { type: 'inbound-rtp', kind: 'video', framesDecoded: 42 },
  ]);

  assert.equal(framesDecodedFromStatsReport(report), 42);
  assert.equal(framesDecodedFromStatsReport(undefined), null);
});

test('camera decode health logs on the 5s poll cadence with fps and gap', () => {
  const first = nextCameraDecodeHealthState(undefined, 10, 1_000);
  assert.equal(first.health, null);

  const early = nextCameraDecodeHealthState(first.state, 20, 5_999);
  assert.equal(early.health, null);

  const due = nextCameraDecodeHealthState(early.state, 40, 10_000);
  assert.deepEqual(due.health, {
    framesDecoded: 40,
    // #623: this previously asserted the buggy 20/9 value.
    // The emitted window is the full 30-frame span since 1,000ms.
    decodedFps: 30 / 9,
    gapSinceLastFrameMs: 0,
  });
});

test('camera decode health keeps frames and time boundaries aligned after a late tick', () => {
  const first = nextCameraDecodeHealthState(undefined, 0, 0);
  const early = nextCameraDecodeHealthState(first.state, 8, 2_000);
  const late = nextCameraDecodeHealthState(early.state, 28, 7_000);

  assert.deepEqual(late.health, {
    framesDecoded: 28,
    decodedFps: 28 / 7,
    gapSinceLastFrameMs: 0,
  });
});

test('camera decode health reports stalled decode gap and stable log shape', () => {
  const first = nextCameraDecodeHealthState(undefined, 10, 1_000);
  const due = nextCameraDecodeHealthState(first.state, 10, 6_000);

  assert.deepEqual(due.health, {
    framesDecoded: 10,
    decodedFps: 0,
    gapSinceLastFrameMs: 5_000,
  });
  assert.equal(
    formatCameraDecodeHealth({
      identity: 'native-a',
      trackName: 'petal-camera-native-a',
      ...due.health!,
    }),
    'camera decode health: native-a / petal-camera-native-a -- frames_decoded=10 decoded_fps=0.0 gap_since_last_frame_ms=5000'
  );
});
