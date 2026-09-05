import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  encodedAudioReceiverStatsFromReport,
  encodedAudioWorkaroundDisabled,
  chromiumWorkaroundAllowed,
  chromiumWorkaroundVersion,
  installEncodedAudioWorkaroundFromUrl,
  recordEncodedAudioFrame,
  type EncodedAudioProbeState,
} from '../src/encodedAudioProbe.ts';

function state(): EncodedAudioProbeState {
  return {
    enabled: true,
    supported: null,
    peerConnectionCount: 0,
    audioReceiverCount: 0,
    frameCount: 0,
    frames: [],
    receiverStats: [],
    errorCode: null,
  };
}

test('keeps encoded audio bytes out of the bounded diagnostic summary', () => {
  const probe = state();
  for (let i = 0; i < 25; i += 1) {
    recordEncodedAudioFrame(probe, {
      data: new Uint8Array([1, 2, 3, 4]).buffer,
      timestamp: i,
      getMetadata: () => ({ payloadType: 111, sequenceNumber: 77 }),
    });
  }

  assert.equal(probe.frameCount, 25);
  assert.equal(probe.frames.length, 20);
  assert.deepEqual(probe.frames[0], {
    rtpTimestamp: 0,
    payloadType: 111,
    sequenceNumber: 77,
    byteLength: 4,
  });
  assert.equal(JSON.stringify(probe.frames).includes('1,2,3,4'), false);
});

test('extracts redacted inbound and codec evidence without stats IDs', () => {
  const report = new Map<string, unknown>([
    ['inbound', {
      id: 'inbound', type: 'inbound-rtp', kind: 'audio', codecId: 'codec',
      packetsReceived: 12, packetsDiscarded: 3, bytesReceived: 456,
      totalSamplesReceived: 4800, totalSamplesDuration: 0.1, totalAudioEnergy: 8,
      jitterBufferEmittedCount: 4800,
    }],
    ['codec', { id: 'codec', type: 'codec', mimeType: 'audio/opus', payloadType: 111 }],
  ]) as unknown as RTCStatsReport;

  assert.deepEqual(encodedAudioReceiverStatsFromReport(report), {
    packetsReceived: 12,
    packetsDiscarded: 3,
    bytesReceived: 456,
    totalSamplesReceived: 4800,
    totalSamplesDuration: 0.1,
    totalAudioEnergy: 8,
    jitterBufferEmittedCount: 4800,
    codecMimeType: 'audio/opus',
    codecPayloadType: 111,
  });
});

const CHROME_UA = 'Mozilla/5.0 AppleWebKit/537.36 Chrome/138.0.0.0 Safari/537.36';

class WorkaroundTrack {
  kind = 'audio';
  private readonly listeners = new Map<string, Array<() => void>>();

  addEventListener(name: string, listener: () => void) {
    this.listeners.set(name, [...(this.listeners.get(name) ?? []), listener]);
  }

  emit(name: string) {
    for (const listener of this.listeners.get(name) ?? []) listener();
  }
}

class WorkaroundReceiverPrototype {
  createEncodedStreams() {
    return {
      readable: new ReadableStream(),
      writable: new WritableStream(),
    };
  }
}

class WorkaroundPeerConnection {
  static last: WorkaroundPeerConnection | null = null;
  readonly listeners = new Map<string, Array<(event?: RTCTrackEvent) => void>>();
  readonly configuration: RTCConfiguration | undefined;
  connectionState = 'new';
  closeCount = 0;

  constructor(configuration?: RTCConfiguration) {
    this.configuration = configuration;
    WorkaroundPeerConnection.last = this;
  }

  addEventListener(name: string, listener: (event?: RTCTrackEvent) => void) {
    this.listeners.set(name, [...(this.listeners.get(name) ?? []), listener]);
  }

  emitAudio(receiver: RTCRtpReceiver, track = new WorkaroundTrack()) {
    for (const listener of this.listeners.get('track') ?? []) {
      listener({ track, receiver } as unknown as RTCTrackEvent);
    }
    return track;
  }

  close() {
    this.closeCount += 1;
    this.connectionState = 'closed';
    for (const listener of this.listeners.get('close') ?? []) listener();
    for (const listener of this.listeners.get('connectionstatechange') ?? []) listener();
  }
}

function workaroundTarget(search = '') {
  return {
    location: new URL(`https://meet.petal.live/test${search}`),
    navigator: { userAgent: CHROME_UA },
    RTCPeerConnection: WorkaroundPeerConnection,
    RTCRtpReceiver: WorkaroundReceiverPrototype,
  } as unknown as Window & typeof globalThis;
}

function audioStreams(options: { rejectWrite?: boolean } = {}) {
  return {
    readable: new ReadableStream({
      start(controller) {
        controller.enqueue({ data: new Uint8Array([1, 2]).buffer, timestamp: 10 });
        if (!options.rejectWrite) controller.close();
      },
    }),
    writable: new WritableStream({
      write() {
        if (options.rejectWrite) throw new Error('test-only rejection');
      },
    }),
  };
}

test('allowlist accepts only bounded Chrome/Chromium versions', () => {
  assert.equal(chromiumWorkaroundVersion(CHROME_UA), 138);
  assert.equal(chromiumWorkaroundAllowed(CHROME_UA), true);
  assert.equal(chromiumWorkaroundAllowed(CHROME_UA.replace('Chrome/138', 'Chrome/119')), false);
  assert.equal(chromiumWorkaroundAllowed(CHROME_UA.replace('Chrome/138', 'Chrome/140')), true);
  assert.equal(chromiumWorkaroundAllowed(CHROME_UA.replace('Chrome/138', 'Edg/138')), false);
  assert.equal(encodedAudioWorkaroundDisabled('?disableEncodedAudioWorkaround=1'), true);
  assert.equal(encodedAudioWorkaroundDisabled('?disableEncodedAudioWorkaround=0'), false);
});

test('unsupported browser and kill switch preserve the native constructor', () => {
  const unsupported = workaroundTarget();
  (unsupported as unknown as { navigator: Navigator }).navigator = { userAgent: 'Mozilla/5.0 Safari/605.1.15' } as Navigator;
  const unsupportedState = installEncodedAudioWorkaroundFromUrl(unsupported as never);
  assert.equal(unsupportedState.enabled, false);
  assert.equal((unsupportedState.failures[0]?.code), 'unsupported-browser');
  assert.equal(unsupported.RTCPeerConnection, WorkaroundPeerConnection);

  const killed = workaroundTarget('?disableEncodedAudioWorkaround=1');
  const killedState = installEncodedAudioWorkaroundFromUrl(killed as never);
  assert.equal(killedState.enabled, false);
  assert.equal(killedState.disabledReason, 'kill-switch');
  assert.equal(killed.RTCPeerConnection, WorkaroundPeerConnection);
});

test('E2EE/receiver-transform markers preserve fallback while normal encoded config still installs', async () => {
  const marked = workaroundTarget() as unknown as { __petalE2eeEnabled: boolean };
  marked.__petalE2eeEnabled = true;
  const state = installEncodedAudioWorkaroundFromUrl(marked as never);
  assert.equal(state.enabled, false);
  assert.equal(state.disabledReason, 'e2ee-or-receiver-transform-configured');

  const target = workaroundTarget();
  const normalState = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection({ encodedInsertableStreams: true } as RTCConfiguration);
  assert.equal(
    ((connection as unknown as WorkaroundPeerConnection).configuration as RTCConfiguration & { encodedInsertableStreams?: boolean })
      ?.encodedInsertableStreams,
    true,
  );
  assert.equal(normalState.enabled, true);
  assert.equal((connection as unknown as WorkaroundPeerConnection).listeners.has('track'), true);

  const explicitlyConfigured = workaroundTarget();
  const explicitState = installEncodedAudioWorkaroundFromUrl(explicitlyConfigured as never);
  const explicitConnection = new explicitlyConfigured.RTCPeerConnection({ receiverTransformConfigured: true } as RTCConfiguration & {
    receiverTransformConfigured: boolean;
  });
  assert.equal(explicitState.enabled, false);
  assert.equal((explicitConnection as unknown as WorkaroundPeerConnection).listeners.has('track'), false);

  // The receiver should remain claimable with the normal LiveKit constructor
  // option; only an explicit competing transform marker opts out.
  (connection as unknown as WorkaroundPeerConnection).emitAudio({
    createEncodedStreams: () => audioStreams(),
  } as unknown as RTCRtpReceiver);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(normalState.claimedReceiverCount, 1);
});

test('constructor failure falls back before claiming a receiver', () => {
  class ThrowingPeerConnection extends WorkaroundPeerConnection {
    constructor(configuration?: RTCConfiguration) {
      if ((configuration as RTCConfiguration & { encodedInsertableStreams?: boolean } | undefined)?.encodedInsertableStreams) {
        throw new Error('unsupported constructor option');
      }
      super(configuration);
    }
  }
  const target = workaroundTarget() as unknown as { RTCPeerConnection: typeof ThrowingPeerConnection };
  target.RTCPeerConnection = ThrowingPeerConnection;
  const state = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection();
  assert.equal(state.constructorFallbackCount, 1);
  assert.equal(state.enabled, false);
  assert.equal(
    ((connection as unknown as WorkaroundPeerConnection).configuration as RTCConfiguration & { encodedInsertableStreams?: boolean })
      ?.encodedInsertableStreams,
    undefined,
  );
  assert.equal(state.claimedReceiverCount, 0);
});

test('createEncodedStreams failure preserves normal receiver fallback', async () => {
  const target = workaroundTarget();
  const state = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection();
  const receiver = {
    createEncodedStreams() { throw new Error('not available'); },
  } as unknown as RTCRtpReceiver;
  (connection as unknown as WorkaroundPeerConnection).emitAudio(receiver);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(state.claimedReceiverCount, 0);
  assert.equal(state.reconnectRequestCount, 0);
  assert.equal(state.failures.at(-1)?.code, 'create-encoded-streams-failed');
});

test('installs each audio receiver once and cleans up on track end', async () => {
  let createCount = 0;
  const target = workaroundTarget();
  const state = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection();
  const receiver = {
    createEncodedStreams() { createCount += 1; return audioStreams(); },
  } as unknown as RTCRtpReceiver;
  const track = (connection as unknown as WorkaroundPeerConnection).emitAudio(receiver);
  (connection as unknown as WorkaroundPeerConnection).emitAudio(receiver, track);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(createCount, 1);
  assert.equal(state.audioReceiverCount, 1);
  assert.equal(state.claimedReceiverCount, 1);
  track.emit('ended');
  assert.equal(state.cleanedReceiverCount, 1);
});

test('defers claiming until receiver transforms are visible', async () => {
  const target = workaroundTarget();
  const state = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection();
  let createCount = 0;
  const receiver = {
    transform: undefined as unknown,
    createEncodedStreams() { createCount += 1; return audioStreams(); },
  } as unknown as RTCRtpReceiver & { transform?: unknown };
  (connection as unknown as WorkaroundPeerConnection).emitAudio(receiver);
  receiver.transform = {};
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(createCount, 0);
  assert.equal(state.claimedReceiverCount, 0);
  assert.equal(state.failures.at(-1)?.code, 'receiver-transform-configured');
  assert.equal(state.cleanedReceiverCount, 1);
});

test('peer connection close cleans every receiver pipe exactly once', async () => {
  const target = workaroundTarget();
  const state = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection();
  const receiver = { createEncodedStreams: () => audioStreams() } as unknown as RTCRtpReceiver;
  const track = (connection as unknown as WorkaroundPeerConnection).emitAudio(receiver);
  await new Promise((resolve) => setImmediate(resolve));
  (connection as unknown as WorkaroundPeerConnection).close();
  track.emit('ended');
  assert.equal(state.cleanedReceiverCount, 1);
});

test('async pipe rejection requests PC close/reconnect and records bounded failure', async () => {
  const target = workaroundTarget();
  const state = installEncodedAudioWorkaroundFromUrl(target as never);
  const connection = new target.RTCPeerConnection();
  const receiver = {
    createEncodedStreams() { return audioStreams({ rejectWrite: true }); },
  } as unknown as RTCRtpReceiver;
  (connection as unknown as WorkaroundPeerConnection).emitAudio(receiver);
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(state.claimedReceiverCount, 1);
  assert.equal(state.reconnectRequestCount, 1);
  assert.equal((connection as unknown as WorkaroundPeerConnection).closeCount, 1);
  assert.equal(state.failures.at(-1)?.code, 'async-pipe-rejected');
  const fallback = new target.RTCPeerConnection();
  assert.equal(
    ((fallback as unknown as WorkaroundPeerConnection).configuration as RTCConfiguration & { encodedInsertableStreams?: boolean })
      ?.encodedInsertableStreams,
    undefined,
  );
});
