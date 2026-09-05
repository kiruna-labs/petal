import assert from 'node:assert/strict';
import { test } from 'node:test';

import { setupCockpit, type CockpitClock } from '../src/cockpit.ts';
import type { HarnessContext } from '../src/context.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { COCKPIT_TOPIC, type CockpitReportMessage } from '../src/trackNames.ts';

// ---------------------------------------------------------------------------
// Test-cockpit Phase 0 walking skeleton (#254): unit coverage for the
// `runScenario` step machine and the INFRA-FAIL vs TEST-FAIL classification
// every later cockpit scenario reuses. These build a minimal fake
// HarnessContext (same pattern as tests/remoteControl.test.ts) rather than a
// real DOM/LiveKit room, since cockpit.ts only touches ctx.state/hook/cb.
// ---------------------------------------------------------------------------

function cockpitContext(
  options: {
    hasRoomInitially?: boolean;
    connectToMeeting?: (code: string, identity: string) => Promise<void>;
    startTestPatternShare?: () => Promise<void>;
    startCockpitWebcam?: () => Promise<{ trackName: string }>;
    stopCockpitWebcam?: () => Promise<{ trackName: string; stopped: boolean }>;
    startCockpitAudioTone?: () => Promise<{ trackName: string }>;
    measureCockpitRemoteAudio?: HarnessContext['cb']['measureCockpitRemoteAudio'];
    measureCockpitRemoteCamera?: HarnessContext['cb']['measureCockpitRemoteCamera'];
    publishCockpitDrawStroke?: () => Promise<{ windowId: number }>;
    publishCockpitTelepointer?: () => Promise<{ windowId: number }>;
    remoteParticipantCount?: number;
    remoteParticipantIds?: string[];
    localIdentity?: string;
  } = {}
): { ctx: HarnessContext; published: CockpitReportMessage[] } {
  const published: CockpitReportMessage[] = [];
  const decoder = new TextDecoder();
  const publishData = async (data: Uint8Array, publishOptions: { topic?: string }) => {
    assert.equal(publishOptions.topic, COCKPIT_TOPIC);
    published.push(JSON.parse(decoder.decode(data)) as CockpitReportMessage);
  };

  const state: {
    room: {
      localParticipant: { identity: string; publishData: typeof publishData };
      remoteParticipants: Map<string, unknown>;
    } | null;
  } = {
    room:
      options.hasRoomInitially === false
        ? null
        : {
            localParticipant: { identity: options.localIdentity ?? 'web-test', publishData },
            remoteParticipants: new Map(
              (options.remoteParticipantIds ?? Array.from({ length: options.remoteParticipantCount ?? 0 }, (_, index) => `remote-${index + 1}`)).map((identity) => [
                identity,
                {},
              ])
            ),
          },
  };

  const ctx = {
    state,
    hook: {},
    cb: {
      connectToMeeting:
        options.connectToMeeting ??
        (async () => {
          state.room = {
            localParticipant: { identity: options.localIdentity ?? 'web-test', publishData },
            remoteParticipants: new Map(
              (options.remoteParticipantIds ?? Array.from({ length: options.remoteParticipantCount ?? 0 }, (_, index) => `remote-${index + 1}`)).map((identity) => [
                identity,
                {},
              ])
            ),
          };
        }),
      resolveIdentity: () => 'web-test',
      startTestPatternShare: options.startTestPatternShare ?? (async () => {}),
      startCockpitWebcam: options.startCockpitWebcam ?? (async () => ({ trackName: 'petal-camera-web-test' })),
      stopCockpitWebcam: options.stopCockpitWebcam ?? (async () => ({ trackName: 'petal-camera-web-test', stopped: true })),
      startCockpitAudioTone: options.startCockpitAudioTone ?? (async () => ({ trackName: 'petal-web-harness-tone' })),
      measureCockpitRemoteAudio:
        options.measureCockpitRemoteAudio ??
        (async () => ({
          ok: true,
          rms: 0.35,
          energyDelta: 0.49,
          durationDelta: 4,
          trackSid: 'TR_fake',
          publisher: 'native-peer',
          detail: 'fake audible audio',
        })),
      measureCockpitRemoteCamera:
        options.measureCockpitRemoteCamera ??
        (async () => ({
          ok: true,
          classification: 'PASS' as const,
          fps: 29.5,
          width: 640,
          height: 480,
          framesDecodedDelta: 120,
          nonBlackRatio: 0.98,
          interFrameDiff: 21.4,
          trackSid: 'TR_camera',
          publisher: 'native-peer',
          detail: 'fake visible camera tile',
        })),
      publishCockpitDrawStroke: options.publishCockpitDrawStroke ?? (async () => ({ windowId: 123 })),
      publishCockpitTelepointer: options.publishCockpitTelepointer ?? (async () => ({ windowId: 123 })),
    },
  } as unknown as HarnessContext;

  return { ctx, published };
}

// Deterministic, wall-clock-based frame counter: after the self-check's real
// ~1000ms wait, this always advances by roughly 100 -- comfortably above the
// >=25 threshold -- without depending on a fake-timer setup or an interval
// racing the awaited timeout.
function advancingFrameCounter(): () => number {
  return () => Math.floor(Date.now() / 10);
}

// #617: SOAK-W2N-STALL's heartbeat count used to be a real wall-clock
// `soakMs / soakHeartbeatMs` quotient (`120 / 40 = 3`), which any real
// scheduling jitter around a `setTimeout` could push to a 4th heartbeat
// before `soak-complete` fired -- flaky independent of any product change.
// This fake `CockpitClock` makes the loop's cadence deterministic: `sleep`
// advances a virtual clock synchronously (no real timer, no jitter) and only
// yields to the microtask queue, so `now() < deadline` always evaluates the
// same way for a given `soakMs`/`soakHeartbeatMs` pair. The exact-heartbeat
// -count assertions below stay meaningful because the loop's real cadence
// logic (`src/cockpit.ts`'s `runSoakHeartbeatWatch`) is still what's under
// test -- only its notion of time is substituted.
function createFakeCockpitClock(startMs = 0): CockpitClock {
  let now = startMs;
  return {
    now: () => now,
    sleep: async (ms: number) => {
      now += ms;
      await Promise.resolve();
    },
  };
}

async function withRemoteShareVideo<T>(fn: () => Promise<T>): Promise<T> {
  const originalDocument = globalThis.document;
  const remoteTile = {
    dataset: { owner: 'native-1', windowId: '456' },
  };
  const remoteVideo = { videoWidth: 1280, videoHeight: 720 };
  const fakeDocument = {
    querySelectorAll: (selector: string) =>
      selector === '.share-tile video'
        ? [remoteVideo]
        : selector === '.share-tile[data-owner][data-window-id]'
          ? [remoteTile]
          : [],
  } as unknown as Document;
  Object.defineProperty(globalThis, 'document', { configurable: true, value: fakeDocument });
  try {
    return await fn();
  } finally {
    Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
  }
}

async function withLocationSearch<T>(search: string, fn: () => Promise<T>): Promise<T> {
  const originalLocation = globalThis.location;
  Object.defineProperty(globalThis, 'location', {
    configurable: true,
    value: { search },
  });
  try {
    return await fn();
  } finally {
    Object.defineProperty(globalThis, 'location', {
      configurable: true,
      value: originalLocation,
    });
  }
}

test('runScenario: self-check failure classifies INFRA-FAIL and still reports it (best-effort join)', async () => {
  // Found live (2026-07-16, MULTI-3/CHAOS-DEVICE investigation): a self-check
  // failure that never joins is otherwise COMPLETELY unreportable --
  // publishReport silently no-ops with no active room connection, so the
  // native cockpit only ever saw generic silence, never the real reason.
  // Fixed: a best-effort join happens specifically so this diagnosis can be
  // published; the scenario action itself still never runs on this path.
  const { ctx, published } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, () => 0); // frame counter never advances
  const result = await cockpit.runScenario('share-w2n-q', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  assert.equal(result.steps[0].step, 'self-check');
  assert.equal(result.steps[0].ok, false);
  assert.equal(result.steps[1].step, 'aborted');
  assert.equal(result.steps[1].ok, false);
  assert.ok(!result.steps.some((step) => step.step === 'join'));
  // The best-effort join succeeded (test harness's default fake
  // connectToMeeting), so both steps above actually published this time.
  assert.equal(published.length, 2);
  assert.deepEqual(ctx.hook.cockpitAutoScenario?.lastResult, result);
});

test('runScenario: self-check failure with no ?code= has nothing to join, stays unreportable', async () => {
  const { ctx, published } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, () => 0); // frame counter never advances
  const result = await cockpit.runScenario('share-w2n-q', null);

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  assert.equal(result.steps[0].step, 'self-check');
  // No code at all means truly nothing to join or report to.
  assert.equal(published.length, 0);
});

test('runScenario: missing ?code= classifies INFRA-FAIL', async () => {
  const { ctx } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('share-w2n-q', null);

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  assert.equal(
    result.steps.find((step) => step.step === 'join')?.ok,
    false
  );
});

test('runScenario: join failure classifies TEST-FAIL -- a real Petal-facing regression, not infra', async () => {
  const { ctx } = cockpitContext({
    hasRoomInitially: false,
    connectToMeeting: async () => {
      throw new Error('token mint rejected');
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('share-w2n-q', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'TEST-FAIL');
  const joinStep = result.steps.find((step) => step.step === 'join');
  assert.equal(joinStep?.ok, false);
  assert.match(joinStep?.detail ?? '', /token mint rejected/);
});

test('runScenario: sharePattern failure classifies TEST-FAIL', async () => {
  const { ctx } = cockpitContext({
    hasRoomInitially: false,
    startTestPatternShare: async () => {
      throw new Error('publishTrack rejected');
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('share-w2n-q', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'TEST-FAIL');
  assert.equal(
    result.steps.find((step) => step.step === 'join')?.ok,
    true
  );
  assert.equal(
    result.steps.find((step) => step.step === 'scenario')?.ok,
    false
  );
});

test('runScenario: an action that runs and FAILS still publishes a terminal report', async () => {
  // Regression guard: a failing action used to end the run with no terminal
  // step, so the native cockpit timed out and classified a real product
  // failure as INFRA-FAIL "may not be implemented web-side yet".
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    measureCockpitRemoteAudio: async () => ({
      ok: false,
      rms: 0,
      energyDelta: 0,
      durationDelta: 4.01,
      trackSid: 'TR_test',
      publisher: 'native-peer',
      detail: 'received audio decoded to silence',
    }),
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('AUD-N2W', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'TEST-FAIL');
  const terminal = published.find((message) => message.step === 'scenario');
  assert.ok(terminal, 'a failed action must publish a terminal `scenario` step');
  assert.equal(terminal?.ok, false);
  assert.match(String(terminal?.detail), /decoded to silence/);
  // The measured evidence must survive onto the terminal report -- it is what
  // the native assertion reads to say WHY the run failed.
  assert.equal(terminal?.remoteAudioDurationDelta, 4.01);
});

test('runScenario: AUD-N2W treats a measurement that throws as INFRA-FAIL', async () => {
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    measureCockpitRemoteAudio: async () => {
      throw new Error(
        'recorded waveform rms=0.3500 and inbound-rtp stats rms=0.0000 disagree across the 0.01 audibility bar -- cannot measure audibility'
      );
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('AUD-N2W', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  const terminal = published.find((message) => message.step === 'scenario');
  assert.ok(terminal, 'an infra refusal must still publish a terminal report');
  assert.equal(terminal?.classification, 'INFRA-FAIL');
  assert.match(String(terminal?.detail), /disagree across the 0.01 audibility bar/);
});

test('runScenario: full happy path is PASS and reports every reachable step over petal.cockpit', async () => {
  const { ctx, published } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('share-w2n-q', 'abc-defg-hjk');

  assert.equal(result.ok, true);
  assert.equal(result.classification, 'PASS');
  assert.deepEqual(
    result.steps.map((step) => step.step),
    ['self-check', 'join', 'sharePattern', 'done']
  );
  assert.ok(result.steps.every((step) => step.ok));

  // self-check couldn't publish (no room yet); join/sharePattern/done could.
  assert.equal(published.length, 3);
  for (const message of published) {
    assert.equal(message.v, 1);
    assert.equal(message.scenarioId, 'share-w2n-q');
    assert.equal(message.reporterId, 'web-test');
  }
  assert.deepEqual(
    published.map((message) => message.step),
    ['join', 'sharePattern', 'done']
  );
});

test('runScenario: CAM publishes a camera track instead of falling through to generic sharePattern', async () => {
  const calls: string[] = [];
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    startTestPatternShare: async () => {
      calls.push('sharePattern');
    },
    startCockpitWebcam: async () => {
      calls.push('camera');
      return { trackName: 'petal-camera-web-test' };
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('CAM', 'abc-defg-hjk');

  assert.equal(result.classification, 'PASS');
  assert.deepEqual(calls, ['camera']);
  const done = published.find((message) => message.step === 'done');
  assert.equal(done?.cameraPublished, true);
  assert.equal(done?.trackName, 'petal-camera-web-test');
});

test('runScenario: CHAOS-DEVICE can publish and remove the synthetic camera', async () => {
  const calls: string[] = [];
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    startCockpitWebcam: async () => {
      calls.push('start-camera');
      return { trackName: 'petal-camera-web-test' };
    },
    stopCockpitWebcam: async () => {
      calls.push('stop-camera');
      return { trackName: 'petal-camera-web-test', stopped: true };
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('CHAOS-DEVICE', 'abc-defg-hjk');

  assert.equal(result.ok, true);
  assert.equal(result.classification, 'PASS');
  assert.deepEqual(calls, ['start-camera', 'stop-camera']);
  const done = published.find((message) => message.step === 'done');
  assert.equal(done?.cameraPublished, true);
  assert.equal(done?.cameraDisappeared, true);
  assert.equal(done?.trackName, 'petal-camera-web-test');
});

test('runScenario: AUD publishes a synthetic audio track instead of falling through to generic sharePattern', async () => {
  const calls: string[] = [];
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    startTestPatternShare: async () => {
      calls.push('sharePattern');
    },
    startCockpitAudioTone: async () => {
      calls.push('audio');
      return { trackName: 'petal-web-harness-tone' };
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('AUD', 'abc-defg-hjk');

  assert.equal(result.classification, 'PASS');
  assert.deepEqual(calls, ['audio']);
  const done = published.find((message) => message.step === 'done');
  assert.equal(done?.audioPublished, true);
  assert.equal(done?.trackName, 'petal-web-harness-tone');
});

test('runScenario: DRAW-N publishes a draw stroke for the remote share window', async () => {
  const calls: string[] = [];
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    publishCockpitDrawStroke: async () => {
      calls.push('draw');
      return { windowId: 456 };
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await withRemoteShareVideo(() => cockpit.runScenario('DRAW-N', 'abc-defg-hjk'));

  assert.equal(result.classification, 'PASS');
  assert.deepEqual(calls, ['draw']);
  const done = published.find((message) => message.step === 'done');
  assert.equal(done?.strokeDelivered, true);
  assert.equal(done?.windowId, 456);
});

test('runScenario: TELE publishes a telepointer movement for the remote share window', async () => {
  const calls: string[] = [];
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    publishCockpitTelepointer: async () => {
      calls.push('tele');
      return { windowId: 456 };
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await withRemoteShareVideo(() => cockpit.runScenario('TELE', 'abc-defg-hjk'));

  assert.equal(result.classification, 'PASS');
  assert.deepEqual(calls, ['tele']);
  const done = published.find((message) => message.step === 'done');
  assert.equal(done?.telepointerMoved, true);
  assert.equal(done?.windowId, 456);
});

test('runScenario: SOAK-W2N-STALL publishes bounded heartbeat reports before PASS', async () => {
  const { ctx, published } = cockpitContext({ hasRoomInitially: false });
  // #617: a fake, synchronously-advancing clock is injected as the loop's
  // only notion of time so `120ms / 40ms = 3 heartbeats` is guaranteed by
  // construction rather than by hoping a real `setTimeout` never drifts.
  const cockpit = setupCockpit(ctx, advancingFrameCounter(), createFakeCockpitClock());
  const result = await withLocationSearch('?soakMs=120&soakHeartbeatMs=40', () =>
    cockpit.runScenario('SOAK-W2N-STALL', 'abc-defg-hjk')
  );

  assert.equal(result.ok, true);
  assert.equal(result.classification, 'PASS');
  // Ordering invariant: heartbeats fire strictly between soak-start and
  // soak-complete, never before or after.
  assert.deepEqual(
    result.steps.map((step) => step.step),
    ['self-check', 'join', 'soak-start', 'soak-heartbeat', 'soak-heartbeat', 'soak-heartbeat', 'soak-complete', 'done']
  );
  assert.ok(result.steps.every((step) => step.ok));

  const heartbeatReports = published.filter((message) => message.step === 'soak-heartbeat');
  const soakStartIndex = published.findIndex((message) => message.step === 'soak-start');
  const soakCompleteIndex = published.findIndex((message) => message.step === 'soak-complete');
  assert.ok(soakStartIndex >= 0 && soakCompleteIndex > soakStartIndex);
  const heartbeatIndices = published
    .map((message, index) => ({ message, index }))
    .filter(({ message }) => message.step === 'soak-heartbeat')
    .map(({ index }) => index);
  // Every heartbeat published strictly inside the soak-start..soak-complete
  // window -- catches a heartbeat that fires outside the soak window.
  assert.ok(heartbeatIndices.every((index) => index > soakStartIndex && index < soakCompleteIndex));

  // With a deterministic clock this count is exact -- not a bound -- because
  // nothing can make `sleep` advance by more or less than requested.
  assert.equal(heartbeatReports.length, 3);
  assert.ok(heartbeatReports.every((message) => message.scenarioId === 'SOAK-W2N-STALL'));
  assert.match(published.find((message) => message.step === 'soak-start')?.detail ?? '', /120ms/);
  // Self-consistency invariant: the final report's heartbeatCount must agree
  // with the number of heartbeat reports actually published.
  assert.equal(published.at(-1)?.heartbeatCount, heartbeatReports.length);
  assert.equal(published.at(-1)?.heartbeatCount, 3);
  assert.equal(published.at(-1)?.heartbeatOk, true);
  assert.equal(published.at(-1)?.step, 'done');
});

test('runScenario: unknown scenarios report INFRA-FAIL instead of a false generic PASS', async () => {
  const { ctx } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('NOT-IMPLEMENTED', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  assert.match(result.steps.at(-1)?.detail ?? '', /not implemented/i);
});

test('runScenario: unsupported SOAK-like IDs do not pass as a stall-watch scaffold', async () => {
  const { ctx } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await withLocationSearch('?soakMs=50&soakHeartbeatMs=10', () =>
    cockpit.runScenario('SOAK-2', 'abc-defg-hjk')
  );

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  assert.ok(!result.steps.some((step) => step.step === 'soak-heartbeat'));
  assert.match(result.steps.at(-1)?.detail ?? '', /not implemented/i);
});

test('runScenario: MULTI-3 reports bounded roster PASS', async () => {
  const { ctx, published } = cockpitContext({ hasRoomInitially: false, remoteParticipantCount: 2 });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('MULTI-3', 'abc-defg-hjk');

  assert.equal(result.ok, true);
  assert.equal(result.classification, 'PASS');
  assert.ok(result.steps.some((step) => step.step === 'multiPeerRoster'));
  const rosterReport = published.find((message) => message.step === 'multiPeerRoster');
  assert.equal(rosterReport?.participantCount, 3);
  assert.equal(rosterReport?.remoteParticipantCount, 2);
  // The typed v1 extension allows native aggregation to compare the two web
  // peers' browser-observed room membership without retaining raw identities.
  assert.match(rosterReport?.rosterFingerprint ?? '', /^[a-f0-9]{64}$/);
  assert.equal(rosterReport?.rosterFingerprintAlgorithm, 'sha-256');
  assert.equal(rosterReport?.rosterIncludesReporter, true);
  assert.equal(rosterReport?.rosterUnique, true);
  assert.doesNotMatch(JSON.stringify(rosterReport), /remote-1|remote-2/);
});

test('runScenario: MULTI-3 produces the same roster fingerprint for peers that observe the same roster in different order', async () => {
  const first = cockpitContext({
    hasRoomInitially: false,
    localIdentity: 'web-1',
    remoteParticipantIds: ['native-1', 'web-2'],
  });
  const second = cockpitContext({
    hasRoomInitially: false,
    localIdentity: 'web-2',
    remoteParticipantIds: ['web-1', 'native-1'],
  });
  const firstCockpit = setupCockpit(first.ctx, advancingFrameCounter());
  const secondCockpit = setupCockpit(second.ctx, advancingFrameCounter());

  await firstCockpit.runScenario('MULTI-3', 'abc-defg-hjk');
  await secondCockpit.runScenario('MULTI-3', 'abc-defg-hjk');

  const firstFingerprint = first.published.find((message) => message.step === 'multiPeerRoster')?.rosterFingerprint;
  const secondFingerprint = second.published.find((message) => message.step === 'multiPeerRoster')?.rosterFingerprint;
  assert.match(firstFingerprint ?? '', /^[a-f0-9]{64}$/);
  assert.equal(firstFingerprint, secondFingerprint);
});

test('runScenario: MULTI-3 refuses a roster whose local reporter identity is unavailable', async () => {
  const { ctx } = cockpitContext({ remoteParticipantCount: 2, localIdentity: '' });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  // The room is connected, but its missing local identity makes roster
  // correlation untrustworthy and must never produce a false PASS.
  await withLocationSearch('?multiPeerWaitMs=1', async () => {
    const result = await cockpit.runScenario('MULTI-3', 'abc-defg-hjk');
    assert.equal(result.ok, false);
    assert.equal(result.classification, 'INFRA-FAIL');
    assert.match(result.steps.at(-1)?.detail ?? '', /reporterIncluded=false/);
  });
});

test('runScenario: MULTI-3 rejects a roster where a remote identity collides with the reporter', async () => {
  const { ctx } = cockpitContext({
    remoteParticipantIds: ['remote-1', 'web-test'],
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  await withLocationSearch('?multiPeerWaitMs=1', async () => {
    const result = await cockpit.runScenario('MULTI-3', 'abc-defg-hjk');
    assert.equal(result.ok, false);
    assert.equal(result.classification, 'INFRA-FAIL');
    assert.match(result.steps.at(-1)?.detail ?? '', /unique=false/);
  });
});

test('runScenario: RC-P1080 scaffold reports INFRA-FAIL instead of generic PASS', async () => {
  const { ctx } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('RC-P1080', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  assert.match(result.steps.at(-1)?.detail ?? '', /1080p share tier/i);
});

test('hook.cockpitAutoScenario.join resolves the bare access code to the internal room-<hash> credential before calling connectToMeeting', async () => {
  // Regression test for the live SHARE-W2N-Q proof (#254): join() originally
  // passed the raw `?code=` access code straight through, but connectToMeeting
  // sends its `room` argument to the backend verbatim, and the backend only
  // accepts the full internal credential -- every interactive join path
  // (autoJoinFromUrl, the join field) resolves this via
  // meetingCredentialFromInviteInput/parseJoinInput first. Confirmed live: the
  // raw access code was rejected by the real backend with "room credential is
  // required" (400), which connectToMeeting's own error handling swallows
  // silently (shows a toast, never throws) -- so join() must also verify
  // ctx.state.room actually got set rather than trusting a clean return.
  const calls: string[] = [];
  const { ctx } = cockpitContext({
    hasRoomInitially: false,
    connectToMeeting: async (code, identity) => {
      calls.push(`join:${code}:${identity}`);
      (ctx.state as { room: unknown }).room = { localParticipant: { identity } };
    },
    startTestPatternShare: async () => {
      calls.push('share');
    },
  });
  setupCockpit(ctx, () => 0);

  const expectedCredential = internalCredentialForAccessCode('abc-defg-hjk');
  await ctx.hook.cockpitAutoScenario?.join('abc-defg-hjk');
  await ctx.hook.cockpitAutoScenario?.sharePattern();

  assert.deepEqual(calls, [`join:${expectedCredential}:web-test`, 'share']);
});

test('hook.cockpitAutoScenario.join throws (does not silently "succeed") when connectToMeeting never sets an active room', async () => {
  const { ctx } = cockpitContext({
    hasRoomInitially: false,
    connectToMeeting: async () => {
      // Mirrors connectToMeeting's real behavior on a token/connect failure:
      // it shows a UI error and returns normally, WITHOUT throwing and
      // WITHOUT setting state.room.
    },
  });
  setupCockpit(ctx, () => 0);

  await assert.rejects(() => ctx.hook.cockpitAutoScenario!.join('abc-defg-hjk'), /connectToMeeting returned without an active room/);
});

// ---------------------------------------------------------------------------
// CAM-N2W (journey CAM-05, #815). The wire-level half of the oracle: what the
// native side actually reads is the published report, so the INFRA-vs-product
// distinction has to survive onto it. Without that, an instrument that could
// not see arrives as a plain `ok: false` and gets blamed on the product --
// the #821 shape, in video form.
// ---------------------------------------------------------------------------

test('runScenario: CAM-N2W reports a visible camera tile as PASS with its measured evidence', async () => {
  const { ctx, published } = cockpitContext({ hasRoomInitially: false });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('CAM-N2W', 'abc-defg-hjk');

  assert.equal(result.ok, true);
  assert.equal(result.classification, 'PASS');
  const step = published.find((message) => message.step === 'remoteCameraVisible');
  assert.ok(step, 'CAM-N2W must publish a remoteCameraVisible step');
  assert.equal(step?.remoteCameraVisible, true);
  assert.equal(step?.remoteCameraWidth, 640);
  assert.equal(step?.remoteCameraNonBlackRatio, 0.98);
});

test('runScenario: CAM-N2W blames a black tile on the product, not the instrument', async () => {
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    measureCockpitRemoteCamera: async () => ({
      ok: false,
      classification: 'TEST-FAIL' as const,
      fps: 29.5,
      width: 640,
      height: 480,
      framesDecodedDelta: 120,
      nonBlackRatio: 0,
      interFrameDiff: 0.1,
      trackSid: 'TR_camera',
      publisher: 'native-peer',
      detail: 'the camera tile is BLACK: maxLuma=2 with the canvas control green',
    }),
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('CAM-N2W', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'TEST-FAIL');
  const terminal = published.find((message) => message.step === 'remoteCameraVisible');
  assert.equal(terminal?.remoteCameraVisible, false);
  assert.match(String(terminal?.detail), /BLACK/);
});

test('runScenario: CAM-N2W puts a blind viewer on the wire as INFRA-FAIL', async () => {
  const { ctx, published } = cockpitContext({
    hasRoomInitially: false,
    measureCockpitRemoteCamera: async () => ({
      ok: false,
      classification: 'INFRA-FAIL' as const,
      fps: 0,
      width: 640,
      height: 480,
      framesDecodedDelta: 120,
      nonBlackRatio: 0,
      interFrameDiff: 0,
      trackSid: 'TR_camera',
      publisher: 'native-peer',
      detail: 'the canvas positive control failed -- this viewer cannot see',
    }),
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('CAM-N2W', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
  const terminal = published.find((message) => message.step === 'scenario');
  assert.ok(terminal, 'a refused verdict must still publish a terminal report');
  assert.equal(terminal?.classification, 'INFRA-FAIL');
  assert.match(String(terminal?.detail), /positive control/);
});

test('runScenario: CAM-N2W treats a measurement that throws as INFRA-FAIL', async () => {
  const { ctx } = cockpitContext({
    hasRoomInitially: false,
    measureCockpitRemoteCamera: async () => {
      throw new Error('requestVideoFrameCallback is unavailable');
    },
  });
  const cockpit = setupCockpit(ctx, advancingFrameCounter());
  const result = await cockpit.runScenario('CAM-N2W', 'abc-defg-hjk');

  assert.equal(result.ok, false);
  assert.equal(result.classification, 'INFRA-FAIL');
});
