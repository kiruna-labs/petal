import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  buildPipelineRows,
  buildGaugeCockpit,
  buildGaugeSeries,
  DIAGNOSTIC_THRESHOLDS,
  fmtTrackFrames,
  fmtTrackRtcp,
  smoothAreaPath,
  smoothLinePath,
  type TrackHealth,
  type NetworkSnapshot
} from '../src/lib/data/networkCockpit.ts';

function emptySnapshot(overrides: Partial<NetworkSnapshot> = {}): NetworkSnapshot {
  return {
    connected: false,
    roomName: null,
    serverHost: null,
    localIdentity: null,
    reconnectCount: 0,
    quality: [],
    history: [],
    tracks: [],
    analysis: [],
    ...overrides
  };
}

test('frontend cockpit thresholds stay pinned to diagnostics.rs constants', () => {
  const source = readFileSync(new URL('../src-tauri/src/diagnostics.rs', import.meta.url), 'utf8');

  assert.match(source, /const HIGH_RTT_MS: f64 = 150\.0;/);
  assert.match(source, /const HIGH_JITTER_MS: f64 = 30\.0;/);
  assert.match(source, /const HIGH_LOSS_PCT: f64 = 2\.0;/);
  assert.match(source, /const HIGH_JITTER_BUFFER_MS: f64 = 80\.0;/);
  assert.match(source, /const FLAPPING_RECONNECTS: u32 = 2;/);

  assert.deepEqual(DIAGNOSTIC_THRESHOLDS, {
    highRttMs: 150,
    highJitterMs: 30,
    highLossPct: 2,
    highJitterBufferMs: 80,
    flappingReconnects: 2,
    mediaPipelineBudgetMs: 18
  });
});

test('gauge model grades known healthy and poor network samples', () => {
  const healthy = buildGaugeCockpit(
    emptySnapshot({
      connected: true,
      history: [{ tMs: 1, rttMs: 30, jitterMs: 2, lossPct: 0, sendKbps: 1200, recvKbps: 800 }]
    }),
    true
  );
  const poor = buildGaugeCockpit(
    emptySnapshot({
      connected: true,
      history: [{ tMs: 1, rttMs: 320, jitterMs: 80, lossPct: 8, sendKbps: 1200, recvKbps: 800 }]
    }),
    true
  );

  assert.equal(healthy.dimensions.find((g) => g.id === 'jitter')?.tone, 'perfect');
  assert.equal(poor.dimensions.find((g) => g.id === 'jitter')?.tone, 'poor');
  assert.equal(poor.dimensions.find((g) => g.id === 'loss')?.tone, 'poor');
});

test('gauge history series scores each sample and mirrors the aggregate gauges', () => {
  const series = buildGaugeSeries(
    emptySnapshot({
      connected: true,
      history: [
        { tMs: 1, rttMs: 30, jitterMs: 2, lossPct: 0, sendKbps: 1200, recvKbps: 800 },
        { tMs: 2, rttMs: 320, jitterMs: 80, lossPct: 8, sendKbps: 1200, recvKbps: 800 }
      ]
    })
  );

  // One score per history sample, keyed by gauge id.
  assert.equal(series.jitter.length, 2);
  assert.equal(series.latency.length, 2);

  // Healthy sample scores perfect; degraded sample bottoms out. Lower metric =
  // higher score, so every series reads "up = better".
  assert.equal(series.jitter[0], 100);
  assert.equal(series.jitter[1], 8);
  assert.equal(series.loss[0], 100);
  assert.equal(series.loss[1], 8);
  assert.equal(series.latency[0], 100);
  assert.ok((series.latency[1] ?? 100) < 100);

  // Overall trend is the mean of the present dimension scores per sample.
  assert.equal(series.overall[0], 100);

  // Bandwidth/system need signals absent from a bare history sample -> null,
  // which the renderer bridges rather than fabricating a value.
  assert.deepEqual(series.bandwidth, [null, null]);
  assert.deepEqual(series.system, [null, null]);
});

test('gauge history series leaves absent per-sample metrics null (bridged, not faked)', () => {
  const series = buildGaugeSeries(
    emptySnapshot({
      connected: true,
      history: [
        { tMs: 1, rttMs: 30, jitterMs: null, lossPct: 0, sendKbps: 0, recvKbps: 0 },
        { tMs: 2, rttMs: 30, jitterMs: 5, lossPct: 0, sendKbps: 0, recvKbps: 0 }
      ]
    })
  );
  assert.equal(series.jitter[0], null);
  assert.equal(typeof series.jitter[1], 'number');
});

test('smooth path maps score to a stretchable 0..100 line, higher score = higher point', () => {
  // No points -> no path (unknown gauges render nothing).
  assert.equal(smoothLinePath([]), '');
  // A single score draws a flat line across the full width at that height.
  assert.equal(smoothLinePath([50]), 'M0 50.00L100 50.00');
  // Score 100 sits at the top (y=0), score 0 at the bottom (y=100).
  const line = smoothLinePath([100, 0]);
  assert.ok(line.startsWith('M0.00 0.00'));
  assert.ok(line.includes('C'));
  assert.ok(line.trimEnd().endsWith('100.00'));
  // The area closes the same line down to the baseline for a soft fill.
  const area = smoothAreaPath([100, 0]);
  assert.ok(area.endsWith('Z'));
  assert.ok(area.includes('L100.00 100'));
});

test('media health formatters expose keyframe and rtcp counters', () => {
  const send = {
    kind: 'video',
    direction: 'send',
    framesEncoded: 240,
    keyFramesEncoded: 4,
    nackCount: 8,
    pliCount: 2,
    firCount: 1
  } as TrackHealth;
  const recv = {
    kind: 'video',
    direction: 'recv',
    framesDecoded: 238,
    keyFramesDecoded: 3,
    nackCount: 5,
    pliCount: 1,
    firCount: 0
  } as TrackHealth;
  const audio = { kind: 'audio', direction: 'recv' } as TrackHealth;

  assert.equal(fmtTrackFrames(send), '240 enc / 4 key');
  assert.equal(fmtTrackFrames(recv), '238 dec / 3 key');
  assert.equal(fmtTrackFrames(audio), '—');
  assert.equal(fmtTrackRtcp(send), 'N 8 / P 2 / F 1');
  assert.equal(fmtTrackRtcp(recv), 'N 5 / P 1 / F 0');
  assert.equal(fmtTrackRtcp(audio), '—');
});

test('pipeline rows group normal send windows without requiring prefilled send-side windowId', () => {
  const rows = buildPipelineRows([
    {
      sid: 'send-1',
      name: 'petal-window-42',
      kind: 'video',
      direction: 'send',
      grabbed: { width: 1280, height: 720, fps: 0, kbps: null },
      encodedSent: { width: 1280, height: 720, fps: 29.8, kbps: 3200 }
    } as TrackHealth
  ]);

  assert.equal(rows.length, 1);
  assert.equal(rows[0].source, 'local');
  assert.equal(rows[0].windowId, 42);
  assert.equal(rows[0].nodes[0].state, 'measured');
  assert.match(rows[0].nodes[0].value, /0 fps/);
  assert.equal(rows[0].nodes[1].label, 'Encoded/sent');
  // Since #160, the viewer's received/decoded stages can arrive over the new
  // cross-peer channel -- an absent stage here is "waiting", not permanently
  // deferred.
  assert.equal(rows[0].nodes[2].detail, 'Waiting for viewer report');
  assert.equal(rows[0].displayEnqueued.label, 'Enqueued to display');
  // displayEnqueued is NOT part of #160's cross-peer scope (it's an inherently
  // local, receiver-side-only measurement) -- still genuinely deferred.
  assert.equal(rows[0].displayEnqueued.detail, 'Deferred #160');
});

test('pipeline rows group normal receive windows by owner and window id', () => {
  const rows = buildPipelineRows([
    {
      sid: 'recv-1',
      name: 'petal-window-7 (alice)',
      rawTrackName: 'petal-window-7',
      ownerIdentity: 'alice',
      windowId: 7,
      kind: 'video',
      direction: 'recv',
      received: { width: 960, height: 540, fps: 24, kbps: 900 },
      decoded: { width: 960, height: 540, fps: 23.5, kbps: null },
      displayEnqueued: { width: 960, height: 540, fps: 21.8, kbps: null }
    } as TrackHealth
  ]);

  assert.equal(rows.length, 1);
  assert.equal(rows[0].source, 'remote');
  assert.equal(rows[0].ownerIdentity, 'alice');
  // Since #160, a remote (native) sender can report grabbed/encoded stages
  // over the new cross-peer channel -- absent here is "waiting", not deferred.
  assert.equal(rows[0].nodes[0].detail, 'Waiting for sender report');
  assert.equal(rows[0].nodes[2].state, 'measured');
  assert.equal(rows[0].nodes[3].state, 'measured');
  assert.equal(rows[0].displayEnqueued.state, 'measured');
  assert.equal(rows[0].displayEnqueued.detail, 'display layer enqueue');
});

test('pipeline rows expose capture state and receiver freeze metrics', () => {
  const rows = buildPipelineRows([
    {
      sid: 'recv-1',
      name: 'petal-window-42 (native-1)',
      rawTrackName: 'petal-window-42',
      ownerIdentity: 'native-1',
      windowId: 42,
      kind: 'video',
      direction: 'recv',
      remoteCaptureState: {
        reporterId: 'native-1',
        sentAtMs: 1000,
        receivedAtMs: Date.now(),
        state: {
          state: 'occluded',
          fps: 0,
          dirtyRectCount: 0,
          dirtyAreaPx: 0,
          occlusionPct: 97,
          cpu: {
            lockCopyMs: 0.7,
            convertMs: 1.4,
            captureFrameReturnMs: 0.2
          }
        }
      },
      receiverFreeze: {
        freezeCount: 2,
        framesDropped: 5,
        qualityLimitationReason: 'stats-frame-starvation'
      }
    } as TrackHealth
  ]);

  assert.equal(rows.length, 1);
  assert.equal(rows[0].captureState.label, 'Occluded');
  assert.equal(rows[0].captureState.occlusion, '97%');
  assert.equal(rows[0].captureState.convertMs, '1.40 ms');
  assert.equal(rows[0].receiverFreeze.freezeCount, '2');
  assert.equal(rows[0].receiverFreeze.framesDropped, '5');
  assert.equal(rows[0].receiverFreeze.qualityLimitationReason, 'stats-frame-starvation');
});

test('pipeline rows exclude camera and audio tracks', () => {
  const rows = buildPipelineRows([
    { sid: 'camera', name: 'petal-camera-alice', kind: 'video', direction: 'recv' } as TrackHealth,
    { sid: 'audio', name: 'microphone', kind: 'audio', direction: 'send' } as TrackHealth
  ]);

  assert.deepEqual(rows, []);
});

test('pipeline rows keep legacy unnamed window capture explicit', () => {
  const rows = buildPipelineRows([
    {
      sid: 'legacy',
      name: 'petal-window-capture',
      kind: 'video',
      direction: 'send',
      encodedSent: { width: 640, height: 360, fps: 15, kbps: 450 }
    } as TrackHealth
  ]);

  assert.equal(rows.length, 1);
  assert.equal(rows[0].source, 'legacy');
  assert.equal(rows[0].title, 'Legacy window share');
  assert.equal(rows[0].nodes[0].detail, 'Legacy name lacks id');
  assert.equal(rows[0].nodes[1].state, 'measured');
});

test('pipeline rows label browser senders instead of fabricating local sender stages', () => {
  const rows = buildPipelineRows([
    {
      sid: 'browser-share',
      name: 'petal-window-91 (web-tester)',
      rawTrackName: 'petal-window-91',
      ownerIdentity: 'web-tester',
      windowId: 91,
      kind: 'video',
      direction: 'recv',
      received: { width: 1280, height: 720, fps: 30, kbps: 1100 },
      decoded: { width: 1280, height: 720, fps: 30, kbps: null }
    } as TrackHealth
  ]);

  assert.equal(rows.length, 1);
  assert.equal(rows[0].source, 'browser');
  // Since #160, a browser sender can report grab/encode stages over the new
  // cross-peer pipeline-stats channel, so an absent local stage before that
  // report arrives is an honest "waiting", not a permanent "browser can't
  // report" -- it's no longer fabrication to expect one eventually.
  assert.equal(rows[0].nodes[0].detail, 'Waiting for sender report');
  assert.equal(rows[0].nodes[1].detail, 'Waiting for sender report');
  assert.equal(rows[0].nodes[2].state, 'measured');
  assert.equal(rows[0].displayEnqueued.detail, 'Waiting for enqueue');
});
