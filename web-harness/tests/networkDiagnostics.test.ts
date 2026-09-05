import assert from 'node:assert/strict';
import { test } from 'node:test';

import { buildNetworkDiagnosticsRows, shouldRenderNetworkDiagnostics } from '../src/networkDiagnostics.ts';
import type { PipelineStatsMessage } from '../src/trackNames.ts';
import type { StartupTimelineSnapshot } from '../src/startupTimeline.ts';

function senderMessage(overrides: Partial<PipelineStatsMessage> = {}): PipelineStatsMessage {
  return {
    v: 1,
    role: 'sender',
    reporterId: 'native-1',
    ownerIdentity: 'native-1',
    windowId: 42,
    seq: 1,
    sentAtMs: 1000,
    grabbed: { width: 1280, height: 720, fps: 30, kbps: null },
    encodedSent: { width: 1280, height: 720, fps: 29, kbps: 1800 },
    received: null,
    decoded: null,
    captureState: {
      state: 'occluded',
      fps: 0,
      dirtyRectCount: 0,
      dirtyAreaPx: 0,
      occlusionPct: 98,
      cpu: {
        lockCopyMs: 0.7,
        convertMs: 1.4,
        captureFrameReturnMs: 0.2,
      },
    },
    receiverFreeze: null,
    ...overrides,
  };
}

function receiverMessage(overrides: Partial<PipelineStatsMessage> = {}): PipelineStatsMessage {
  return {
    v: 1,
    role: 'receiver',
    reporterId: 'web-1',
    ownerIdentity: 'native-1',
    windowId: 42,
    seq: 2,
    sentAtMs: 1100,
    grabbed: null,
    encodedSent: null,
    received: { width: 1280, height: 720, fps: 28, kbps: 1600 },
    decoded: { width: 1280, height: 720, fps: 27, kbps: null },
    captureState: null,
    receiverFreeze: {
      freezeCount: 2,
      framesDropped: 5,
      qualityLimitationReason: 'stats-frame-starvation',
    },
    ...overrides,
  };
}

test('network diagnostics rows merge remote sender capture with local receiver freeze metrics', () => {
  const rows = buildNetworkDiagnosticsRows(
    {
      sent: [receiverMessage()],
      received: [{ message: senderMessage(), senderIdentity: 'native-1', receivedAt: 1200 }],
    },
    'web-1'
  );

  assert.equal(rows.length, 1);
  assert.equal(rows[0].title, 'Window 42');
  assert.equal(rows[0].subtitle, 'native-1 share');
  assert.equal(rows[0].captureLabel, 'Occluded');
  assert.equal(rows[0].occlusion, '98%');
  assert.equal(rows[0].convertMs, '1.40 ms');
  assert.equal(rows[0].freezeCount, '2');
  assert.equal(rows[0].framesDropped, '5');
  assert.equal(rows[0].qualityLimitationReason, 'stats-frame-starvation');
});

test('network diagnostics rows show local sender capture and remote viewer freeze metrics', () => {
  const rows = buildNetworkDiagnosticsRows(
    {
      sent: [senderMessage({ reporterId: 'web-1', ownerIdentity: 'web-1' })],
      received: [
        {
          message: receiverMessage({ ownerIdentity: 'web-1', reporterId: 'native-1' }),
          senderIdentity: 'native-1',
          receivedAt: 1200,
        },
      ],
    },
    'web-1'
  );

  assert.equal(rows.length, 1);
  assert.equal(rows[0].subtitle, 'shared by you');
  assert.equal(rows[0].captureLabel, 'Occluded');
  assert.equal(rows[0].freezeCount, '2');
});

test('network diagnostics keeps replacement publications separate', () => {
  const first = senderMessage({ publicationSid: 'TR_old', shareEpoch: 'e1' });
  const replacement = senderMessage({ publicationSid: 'TR_new', shareEpoch: 'e2' });
  const rows = buildNetworkDiagnosticsRows({ sent: [first, replacement], received: [] }, 'native-1');
  assert.equal(rows.length, 2);
  assert.notEqual(rows[0].id, rows[1].id);
});

test('sender epoch evidence and receiver pre-epoch lifecycle combine by shared publication SID', () => {
  const sender = senderMessage({ publicationSid: 'TR_current', shareEpoch: 'e-owner' });
  const receiver = receiverMessage({ publicationSid: 'TR_current', shareEpoch: null, lifecycle: 'firstPresented' });
  const rows = buildNetworkDiagnosticsRows({ sent: [sender, receiver], received: [] }, 'native-1');
  assert.equal(rows.length, 1);
  assert.equal(rows[0].lifecycle, 'first presented');
});

test('network diagnostics exposes startup classification, demand, rVFC timing, resolution, fps and literal RID', () => {
  const sender = senderMessage({ publicationSid: 'TR_current', shareEpoch: 'e-owner' });
  const receiver = receiverMessage({ publicationSid: 'TR_current', shareEpoch: null, lifecycle: 'firstPresented' });
  const startup: StartupTimelineSnapshot = {
    correlation: { ownerAlias: 'peer-1', windowId: 42, publicationSid: 'TR_current', shareEpoch: 'e-owner' },
    clock: { basis: 'receiver-performance-now', crossPeerComparable: false, uncertainty: 'uncalibrated-cross-peer-clocks' },
    events: [
      { kind: 'viewerDemand', atMonotonicMs: 20, elapsedMs: 20, requestedSubscription: 'high', demandWidth: 1920, demandHeight: 1200, requestedWidth: 1920, requestedHeight: 1200 },
      { kind: 'firstPresented', atMonotonicMs: 120, elapsedMs: 120, presentationSource: 'requestVideoFrameCallback' },
      { kind: 'statsTransition', atMonotonicMs: 1000, elapsedMs: 1000, decodedWidth: 1920, decodedHeight: 1200, decodedFps: 29.5, presentedFps: 29.2, rid: 'f', capturePath: 'visible-raw', captureFps: 30 },
    ],
    classification: { cause: 'healthy', detail: 'healthy', evidenceComplete: true },
  };
  const [row] = buildNetworkDiagnosticsRows({ sent: [sender, receiver], received: [] }, 'native-1', [startup]);
  assert.equal(row.startupCause, 'healthy');
  assert.equal(row.firstPresented, '120 ms (estimated display, rVFC)');
  assert.equal(row.requestedSubscription, 'high 1920x1200 (device 1920x1200)');
  assert.equal(row.decodedPresentation, '1920x1200 @ 29 fps');
  assert.equal(row.rid, 'f');
  assert.match(row.clockUncertainty, /uncalibrated/);
});

test('network diagnostics render only while both the debug drawer and nested panel are open', () => {
  assert.equal(shouldRenderNetworkDiagnostics({ open: true }, { open: true }), true);
  assert.equal(shouldRenderNetworkDiagnostics({ open: true }, { open: false }), false);
  assert.equal(shouldRenderNetworkDiagnostics({ open: false }, { open: true }), false);
  assert.equal(shouldRenderNetworkDiagnostics({ open: false }, { open: false }), false);
  assert.equal(shouldRenderNetworkDiagnostics(null, { open: true }), false);
  assert.equal(shouldRenderNetworkDiagnostics({ open: true }, null), false);
});
