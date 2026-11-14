import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test, { mock } from 'node:test';

import {
  PHOTON_HOST_MIN_INTERVAL_MS,
  PHOTON_RING_CAPACITY,
  PHOTON_RING_MS,
  PHOTON_SENT_CAPACITY,
  PHOTON_SENT_TTL_MS,
  PHOTON_TIMEOUT_MS,
  PHOTON_WHEEL_BURST_IDLE_MS,
  PhotonCorrelator,
  PhotonInputFilter,
  type PhotonInput,
  type PhotonOutcome,
} from '@petal/shared/logic/photonCorrelator';
import { installTestSink, uninstallTestSink } from '../src/analytics.ts';
import type { HarnessContext } from '../src/context.ts';
import type { RcFrameMetadata } from '../src/rcReceiverTiming.ts';
import { parseRemoteControlJson } from '../src/remoteControl.ts';
import { RC_LATENCY_SYNC_MS, setupRemoteControlUi } from '../src/remoteControlUi.ts';
import type { RemoteControlMessage } from '../src/trackNames.ts';

type Step =
  | { op: 'sent'; seq: number; at: number }
  | { op: 'presented'; frameId: number; at: number }
  | { op: 'inputFrame'; seq: number; frameId: number; at: number }
  | { op: 'tick'; at: number }
  | { op: 'reset' };

const contract = JSON.parse(readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')) as {
  photonCorrelator: Record<string, number>;
  photonCorrelatorVectors: Array<{ name: string; steps: Step[]; expected: PhotonOutcome[] }>;
  photonInputFilterVectors: Array<{ name: string; steps: Array<{ input: PhotonInput; at: number; admit: boolean }> }>;
  remoteControlMessages: Array<{ name: string; message: Record<string, unknown> }>;
};

test('photon constants match the shared contract', () => {
  assert.deepEqual(contract.photonCorrelator, {
    sentCapacity: PHOTON_SENT_CAPACITY,
    sentTtlMs: PHOTON_SENT_TTL_MS,
    ringMs: PHOTON_RING_MS,
    ringCapacity: PHOTON_RING_CAPACITY,
    timeoutMs: PHOTON_TIMEOUT_MS,
    wheelBurstIdleMs: PHOTON_WHEEL_BURST_IDLE_MS,
    hostMinIntervalMs: PHOTON_HOST_MIN_INTERVAL_MS,
  });
});

test('correlator passes the shared contract vectors (before/after, skipped ids, expiry, duplicates)', () => {
  for (const vector of contract.photonCorrelatorVectors) {
    const correlator = new PhotonCorrelator();
    const got: PhotonOutcome[] = [];
    for (const step of vector.steps) {
      if (step.op === 'sent') got.push(...correlator.noteSent(step.seq, step.at));
      else if (step.op === 'presented') got.push(...correlator.notePresented(step.frameId, step.at));
      else if (step.op === 'inputFrame') got.push(...correlator.noteInputFrame(step.seq, step.frameId, step.at));
      else if (step.op === 'tick') got.push(...correlator.expire(step.at));
      else correlator.reset();
    }
    assert.deepEqual(got, vector.expected, vector.name);
  }
});

test('input filter passes the shared contract vectors (discrete ops, first wheel of a burst)', () => {
  for (const vector of contract.photonInputFilterVectors) {
    const filter = new PhotonInputFilter();
    for (const step of vector.steps) {
      assert.equal(filter.admit(step.input, step.at), step.admit, `${vector.name} ${JSON.stringify(step)}`);
    }
  }
});

test('inputFrame parses; a malformed frame id is dropped; an unknown kind stays ignored (old-peer guard)', () => {
  const fixture = contract.remoteControlMessages.find((entry) => entry.name === 'input-frame')!.message;
  const parsed = parseRemoteControlJson(JSON.stringify(fixture));
  assert.ok(parsed && parsed.kind === 'inputFrame');
  assert.equal(parsed.frameId, 1234);
  assert.equal(parsed.contentChange, 'detected');
  assert.equal(parseRemoteControlJson(JSON.stringify({ ...fixture, frameId: -1 })), null);
  assert.equal(parseRemoteControlJson(JSON.stringify({ ...fixture, frameId: 2 ** 32 })), null);
  const newer = parseRemoteControlJson(JSON.stringify({ ...fixture, contentChange: 'region' }));
  assert.equal(newer?.kind === 'inputFrame' && newer.contentChange, 'unknown');
  // This is how every pre-#376 web controller treats inputFrame.
  assert.equal(parseRemoteControlJson(JSON.stringify({ ...fixture, kind: 'inputFrameV2' })), null);
});

type FrameCallback = (now: number, metadata: RcFrameMetadata) => void;

test('web controller: real send funnel + rVFC presents + inputFrame produce photon_ms and photon_timeout', async () => {
  const callbacks = new Map<number, FrameCallback>();
  let handle = 0;
  const video = {
    requestVideoFrameCallback: (callback: FrameCallback) => {
      handle += 1;
      callbacks.set(handle, callback);
      return handle;
    },
    cancelVideoFrameCallback: (id: number) => callbacks.delete(id),
  };
  const present = (rtpTimestamp: number, expectedDisplayTime: number) => {
    const pending = [...callbacks.values()];
    callbacks.clear();
    for (const callback of pending) callback(0, { expectedDisplayTime, rtpTimestamp });
  };
  const frames = new Map<number, { userTimestamp: bigint; frameId: number }>();
  const track = {
    kind: 'video',
    getRTCStatsReport: () => Promise.resolve(null),
    lookupFrameMetadata: ({ rtpTimestamp }: { rtpTimestamp: number }) => frames.get(rtpTimestamp),
  };
  const published: RemoteControlMessage[] = [];
  const tile = {
    id: 'tile-1',
    dataset: { owner: 'native-host', trackSid: 'TR_1', windowId: '7' },
    classList: { toggle: () => {} },
    querySelector: (selector: string) => (selector === 'video' ? video : { classList: { toggle: () => {} } }),
  };
  const scope = globalThis as { document?: unknown; RTCRtpScriptTransform?: unknown };
  const priorDocument = scope.document;
  const priorTransform = scope.RTCRtpScriptTransform;
  scope.document = { getElementById: (id: string) => (id === 'tile-1' ? tile : null), querySelectorAll: () => [] };
  scope.RTCRtpScriptTransform = function RTCRtpScriptTransform() {};
  const ctx = {
    state: {
      room: {
        localParticipant: {
          identity: 'web',
          publishData: (bytes: Uint8Array) => {
            published.push(JSON.parse(new TextDecoder().decode(bytes)));
            return Promise.resolve();
          },
        },
        remoteParticipants: new Map([
          ['native-host', { identity: 'native-host', trackPublications: new Map([['TR_1', { track }]]) }],
        ]),
      },
      remoteControlSeq: 0,
      activeRemoteControl: null as unknown,
      frameMetadataWorker: {},
    },
    ui: { logEvent: () => {}, showToast: () => {} },
    hook: {},
    cb: { shareTileForWindowId: () => null },
  } as unknown as HarnessContext;
  let perfNow = 10_000;
  mock.method(performance, 'now', () => perfNow);
  const sink = installTestSink();
  mock.timers.enable({ apis: ['setInterval', 'Date'], now: 50_000 });
  const encoder = new TextEncoder();
  const inputFrame = (seq: number, frameId: number) =>
    encoder.encode(
      JSON.stringify({ v: 1, kind: 'inputFrame', targetUserId: 'web', controllerId: 'native-host', windowId: 7, seq, frameId, contentChange: 'detected' })
    );
  try {
    const ui = setupRemoteControlUi(ctx) as unknown as {
      publishRemoteControl: (message: RemoteControlMessage) => Promise<void>;
      handleRemoteControlPayload: (payload: Uint8Array, sender?: string) => void;
      stopRemoteControl: () => void;
    };
    ctx.state.activeRemoteControl = { tileId: 'tile-1', targetUserId: 'native-host', windowId: 7, pointerId: null, grantToken: 'grant' };
    mock.timers.tick(RC_LATENCY_SYNC_MS);
    assert.equal(callbacks.size, 1, 'the existing rVFC loop is the only frame subscription');

    const base = { v: 1 as const, targetUserId: 'native-host', controllerId: 'web', windowId: 7, grantToken: 'grant' };
    const modifiers = { alt: false, ctrl: false, meta: false, shift: false };
    const click = (seq: number) =>
      ({ ...base, seq, kind: 'pointer', action: 'click', x: 0.5, y: 0.5, button: 0, buttons: 0, modifiers }) as RemoteControlMessage;
    // A hover move is never remembered, so its inputFrame (a buggy host) scores nothing.
    await ui.publishRemoteControl({ ...base, seq: 1, kind: 'pointer', action: 'move', x: 0.1, y: 0.1, button: -1, buttons: 0, modifiers } as RemoteControlMessage);
    await ui.publishRemoteControl(click(2));
    assert.equal(published.length, 2);
    // Video beats data: frame 40 lights 60 ms after the click, inputFrame arrives later.
    frames.set(900, { userTimestamp: 1n, frameId: 40 });
    perfNow = 10_060;
    present(900, 10_060);
    perfNow = 10_090;
    // A spoofed sender is not the window owner: ignored (else seq 2 would wait for frame 999).
    ui.handleRemoteControlPayload(inputFrame(2, 999), 'someone-else');
    ui.handleRemoteControlPayload(inputFrame(1, 40), 'native-host');
    ui.handleRemoteControlPayload(inputFrame(2, 40), 'native-host');
    // A duplicate changes nothing.
    ui.handleRemoteControlPayload(inputFrame(2, 40), 'native-host');

    // A second click whose announced frame never lights: one timeout, not a 2 s sample.
    perfNow = 11_000;
    await ui.publishRemoteControl(click(3));
    ui.handleRemoteControlPayload(inputFrame(3, 99), 'native-host');
    perfNow = 12_100;
    mock.timers.tick(RC_LATENCY_SYNC_MS);

    ui.stopRemoteControl();
    mock.timers.tick(RC_LATENCY_SYNC_MS);
    const events = sink.filter((event) => event.name === 'remote_control_latency_summary');
    assert.equal(events.length, 1);
    const props = events[0].properties;
    assert.equal(props.photon_ms_n, 1);
    assert.equal(props.photon_ms_max, 60);
    assert.equal(props.photon_timeout, 1);
    assert.equal(props.content_change, 'detected');
  } finally {
    mock.timers.reset();
    mock.restoreAll();
    uninstallTestSink();
    scope.document = priorDocument;
    scope.RTCRtpScriptTransform = priorTransform;
  }
});
