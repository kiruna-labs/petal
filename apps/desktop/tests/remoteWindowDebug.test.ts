import assert from 'node:assert/strict';
import test from 'node:test';

import {
  DEBUG_LAST_FRAME_STALE_MS,
  findLocalWindowDebugTrack,
  findRemoteWindowDebugTrack,
  formatFrameCounters,
  formatGlassToGlassLatency,
  formatGlassToGlassLatencyChip,
  formatLastFrameAge,
  formatPacketLossCumulative,
  formatSharedBy,
  formatSharedFps,
  type TrackHealth
} from '../src/lib/data/remoteWindowDebug.ts';

function track(overrides: Partial<TrackHealth>): TrackHealth {
  return {
    sid: 'track',
    name: 'petal-window-1 (alice)',
    rawTrackName: 'petal-window-1',
    ownerIdentity: 'alice',
    windowId: 1,
    kind: 'video',
    direction: 'recv',
    width: 1280,
    height: 720,
    fps: 29.8,
    codecImpl: 'VideoToolbox',
    qualityLimitation: '',
    softwareEncoder: false,
    targetKbps: 0,
    actualKbps: 1200,
    packetsLost: 3,
    framesEncoded: 0,
    keyFramesEncoded: 0,
    framesDecoded: 240,
    keyFramesDecoded: 4,
    framesDropped: 2,
    nackCount: 0,
    firCount: 0,
    pliCount: 0,
    jitterBufferMs: 7.2,
    glassToGlassMs: null,
    glassToGlassEstimateMs: null,
    streamState: 'active',
    ...overrides
  };
}

test('debug track matching uses exact owner identity and window id fields', () => {
  const snapshot = {
    tracks: [
      track({ sid: 'alice-1', ownerIdentity: 'alice', windowId: 1, rawTrackName: 'petal-window-1' }),
      track({ sid: 'alice-12', ownerIdentity: 'alice', windowId: 12, rawTrackName: 'petal-window-12' }),
      track({ sid: 'bob-1', ownerIdentity: 'bob', windowId: 1, rawTrackName: 'petal-window-1' })
    ]
  };

  assert.equal(findRemoteWindowDebugTrack(snapshot, 'alice', 1)?.sid, 'alice-1');
  assert.equal(findRemoteWindowDebugTrack(snapshot, 'alice', 12)?.sid, 'alice-12');
  assert.equal(findRemoteWindowDebugTrack(snapshot, 'bob', 1)?.sid, 'bob-1');
});

test('local debug track matching filters sender video for the shared window', () => {
  const snapshot = {
    localIdentity: 'alice',
    tracks: [
      track({ sid: 'recv', direction: 'recv', ownerIdentity: 'alice', windowId: 1 }),
      track({ sid: 'send-other-window', direction: 'send', ownerIdentity: 'alice', windowId: 12 }),
      track({ sid: 'send-other-owner', direction: 'send', ownerIdentity: 'bob', windowId: 1 }),
      track({ sid: 'send-local-window', direction: 'send', ownerIdentity: 'alice', windowId: 1 })
    ]
  };

  assert.equal(findLocalWindowDebugTrack(snapshot, 1)?.sid, 'send-local-window');
  assert.equal(findLocalWindowDebugTrack(snapshot, 12)?.sid, 'send-other-window');
});

test('debug track matching does not parse decorated names or substring ids', () => {
  const snapshot = {
    tracks: [
      track({
        sid: 'legacy-looking-name',
        name: 'petal-window-1 (alice)',
        ownerIdentity: null,
        windowId: null,
        rawTrackName: null
      }),
      track({ sid: 'only-substring', ownerIdentity: 'alice', windowId: 12, rawTrackName: 'petal-window-12' })
    ]
  };

  assert.equal(findRemoteWindowDebugTrack(snapshot, 'alice', 1), null);
});

test('debug formatting labels owner, cumulative loss, frame counters, and stale age honestly', () => {
  const recv = track({ framesDecoded: 41, framesDropped: 5, packetsLost: 9 });

  assert.equal(formatSharedBy('Sana', 'sana@example.com'), 'Sana (sana@example.com)');
  assert.equal(formatSharedBy('sana@example.com', 'sana@example.com'), 'sana@example.com');
  assert.equal(formatPacketLossCumulative(recv), '9 cumulative');
  assert.equal(formatFrameCounters(recv, 43), '43 pushed / 41 decoded / 5 dropped');

  assert.deepEqual(formatLastFrameAge(9_500, 10_000), { label: '500 ms ago', stale: false });
  assert.deepEqual(formatLastFrameAge(10_000 - DEBUG_LAST_FRAME_STALE_MS, 10_000), {
    label: '3.0 s ago',
    stale: true
  });
});

test('formatSharedFps labels idle-keepalive activity instead of presenting it as a real encode rate', () => {
  // Live capture: an ordinary fps reading, no annotation needed.
  const live = track({
    direction: 'send',
    encodedSent: { width: 1280, height: 720, fps: 29.8, kbps: 3200 },
    captureState: {
      state: 'live',
      fps: 29.8,
      dirtyRectCount: 1,
      dirtyAreaPx: 100,
      occlusionPct: null,
      cpu: { lockCopyMs: null, convertMs: null, captureFrameReturnMs: null }
    }
  });
  assert.equal(formatSharedFps(live), '29.8');

  // Idle content: the once-per-second keepalive still ticks the encoder's
  // frame counter even though nothing new was captured -- must be labeled,
  // not shown as an ordinary "1.0" that looks comparable to "FPS captured".
  const idleWithKeepalive = track({
    direction: 'send',
    encodedSent: { width: 1280, height: 720, fps: 1.0, kbps: 10 },
    captureState: {
      state: 'idle',
      fps: 0,
      dirtyRectCount: 0,
      dirtyAreaPx: 0,
      occlusionPct: null,
      cpu: { lockCopyMs: null, convertMs: null, captureFrameReturnMs: null }
    }
  });
  assert.equal(formatSharedFps(idleWithKeepalive), '1.0 (idle keepalive)');

  // Idle with genuinely zero encode activity: nothing to annotate.
  const idleNoActivity = track({
    direction: 'send',
    encodedSent: { width: 1280, height: 720, fps: 0, kbps: 0 },
    captureState: {
      state: 'idle',
      fps: 0,
      dirtyRectCount: 0,
      dirtyAreaPx: 0,
      occlusionPct: null,
      cpu: { lockCopyMs: null, convertMs: null, captureFrameReturnMs: null }
    }
  });
  assert.equal(formatSharedFps(idleNoActivity), '0.0');

  assert.equal(formatSharedFps(null), 'n/a');
});

test('glass-to-glass latency distinguishes measured from estimate and includes caveats', () => {
  const measured = formatGlassToGlassLatency(track({ glassToGlassMs: 24.4, glassToGlassEstimateMs: 50 }));
  const estimate = formatGlassToGlassLatency(track({ glassToGlassMs: null, glassToGlassEstimateMs: 51.2 }));
  const missing = formatGlassToGlassLatency(null);

  assert.equal(measured.label, '24.4 ms measured');
  assert.match(measured.caveat, /data-channel clock calibration/);
  assert.equal(estimate.label, '51.2 ms estimate');
  assert.match(estimate.caveat, /RTT\/2/);
  assert.equal(missing.label, 'n/a');
});

test('#376: glass-to-glass latency chip always marks estimates with "~", never a measured value', () => {
  const measuredChip = formatGlassToGlassLatencyChip(track({ glassToGlassMs: 24.4, glassToGlassEstimateMs: 50 }));
  const estimateChip = formatGlassToGlassLatencyChip(track({ glassToGlassMs: null, glassToGlassEstimateMs: 51.6 }));
  const noDataChip = formatGlassToGlassLatencyChip(track({ glassToGlassMs: null, glassToGlassEstimateMs: null }));
  const noTrackChip = formatGlassToGlassLatencyChip(null);

  assert.deepEqual(measuredChip?.text, '24 ms');
  assert.equal(measuredChip?.estimate, false);
  assert.doesNotMatch(measuredChip?.text ?? '', /~/);

  assert.equal(estimateChip?.text, '~52 ms');
  assert.equal(estimateChip?.estimate, true);

  // No underlying data at all -> hide the chip entirely (never a fake "n/a"
  // in a compact "while controlling" affordance).
  assert.equal(noDataChip, null);
  assert.equal(noTrackChip, null);
});
