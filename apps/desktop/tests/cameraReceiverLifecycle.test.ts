import assert from 'node:assert/strict';
import test from 'node:test';

import {
  CAMERA_RECEIVER_LIFECYCLE_PHASES,
  CAMERA_RECEIVER_LIFECYCLE_ROUTE,
  LIFECYCLE_FIELD_LIMITS,
  buildCameraReceiverLifecyclePayload,
  createCameraReceiverLifecycleRecorder,
  type CameraReceiverLifecyclePayload
} from '../src/lib/data/cameraReceiverLifecycle.ts';

// Durable receiver-lifecycle evidence: an ABSENT periodic interval must be
// attributable to a specific boundary (bridge never connected vs never
// subscribed vs never decoded), and a rejected invoke must never be swallowed
// again. Mirrors the cameraReceiverLifecycle sink tests in diagnostics.rs.

test('buildCameraReceiverLifecyclePayload carries the closed phase and route', () => {
  const payload = buildCameraReceiverLifecyclePayload({
    phase: 'subscribed',
    participantIdentity: 'alice',
    trackName: 'petal-camera-alice',
    trackSid: 'TR_abc',
    bridgeAgeMs: 1234
  });
  assert.equal(payload.phase, 'subscribed');
  assert.equal(payload.route, CAMERA_RECEIVER_LIFECYCLE_ROUTE);
  assert.equal(payload.participantIdentity, 'alice');
  assert.equal(payload.trackName, 'petal-camera-alice');
  assert.equal(payload.trackSid, 'TR_abc');
  assert.equal(payload.bridgeAgeMs, 1234);
  // A field the caller did not supply is missing, never an empty string that
  // reads like a value the webview actually sent.
  assert.equal(payload.detail, null);
});

test('buildCameraReceiverLifecyclePayload refuses an unrecognized phase', () => {
  for (const phase of CAMERA_RECEIVER_LIFECYCLE_PHASES) {
    assert.equal(buildCameraReceiverLifecyclePayload({ phase }).phase, phase);
  }
  const unknown = buildCameraReceiverLifecyclePayload({
    // @ts-expect-error -- deliberately outside the closed list.
    phase: 'not_a_real_phase'
  });
  assert.equal(unknown.phase, 'unknown_phase');
});

test('buildCameraReceiverLifecyclePayload caps and strips free-form fields', () => {
  const payload = buildCameraReceiverLifecyclePayload({
    phase: 'first_decode',
    participantIdentity: `a\n${'i'.repeat(500)}`,
    trackName: 't'.repeat(500),
    trackSid: 's'.repeat(500),
    detail: `x\u0000${'d'.repeat(500)}`
  });
  assert.equal(payload.participantIdentity, `a${'i'.repeat(LIFECYCLE_FIELD_LIMITS.participantIdentity - 1)}`);
  assert.equal(payload.trackName!.length, LIFECYCLE_FIELD_LIMITS.trackName);
  assert.equal(payload.trackSid!.length, LIFECYCLE_FIELD_LIMITS.trackSid);
  assert.equal(payload.detail!.length, LIFECYCLE_FIELD_LIMITS.detail);
  // A control character must never survive into a single-line log record.
  assert.ok(!/[\u0000-\u001f\u007f]/.test(JSON.stringify(payload)), JSON.stringify(payload));
});

test('buildCameraReceiverLifecyclePayload reports a non-finite bridge age as missing', () => {
  assert.equal(buildCameraReceiverLifecyclePayload({ phase: 'bridge_connected', bridgeAgeMs: Number.NaN }).bridgeAgeMs, null);
  assert.equal(buildCameraReceiverLifecyclePayload({ phase: 'bridge_connected', bridgeAgeMs: Number.POSITIVE_INFINITY }).bridgeAgeMs, null);
  assert.equal(buildCameraReceiverLifecyclePayload({ phase: 'bridge_connected', bridgeAgeMs: -1 }).bridgeAgeMs, null);
  assert.equal(buildCameraReceiverLifecyclePayload({ phase: 'bridge_connected', bridgeAgeMs: 12.6 }).bridgeAgeMs, 13);
});

test('recorder reports success and counts every attempt', async () => {
  const seen: CameraReceiverLifecyclePayload[] = [];
  const recorder = createCameraReceiverLifecycleRecorder({
    invoke: async (payload) => {
      seen.push(payload);
    }
  });
  assert.equal(await recorder.record({ phase: 'bridge_connecting' }), true);
  assert.equal(await recorder.record({ phase: 'bridge_connected' }), true);
  assert.equal(recorder.attempts(), 2);
  assert.equal(recorder.failures(), 0);
  assert.deepEqual(
    seen.map((p) => p.phase),
    ['bridge_connecting', 'bridge_connected']
  );
});

test('recorder retries exactly once as lifecycle_failed and never recurses', async () => {
  const seen: CameraReceiverLifecyclePayload[] = [];
  const failures: string[] = [];
  const recorder = createCameraReceiverLifecycleRecorder({
    invoke: async (payload) => {
      seen.push(payload);
      throw new Error('ipc bridge is gone');
    },
    onFailure: (failure) => failures.push(`${failure.phase}:${failure.attempt}`)
  });
  assert.equal(await recorder.record({ phase: 'subscribed' }), false);
  // Two attempts for one record: the original plus ONE terminal retry. A dead
  // IPC bridge must not be able to spin.
  assert.deepEqual(
    seen.map((p) => p.phase),
    ['subscribed', 'lifecycle_failed']
  );
  assert.equal(seen[1].detail, 'after=subscribed');
  assert.equal(recorder.attempts(), 2);
  assert.equal(recorder.failures(), 2);
  assert.equal(failures.length, 2);
});

test('recorder recovers when only the first attempt fails', async () => {
  const seen: CameraReceiverLifecyclePayload[] = [];
  const recorder = createCameraReceiverLifecycleRecorder({
    invoke: async (payload) => {
      seen.push(payload);
      if (payload.phase !== 'lifecycle_failed') throw new Error('transient');
    }
  });
  assert.equal(await recorder.record({ phase: 'first_decode' }), true);
  assert.deepEqual(
    seen.map((p) => p.phase),
    ['first_decode', 'lifecycle_failed']
  );
  assert.equal(recorder.failures(), 1);
});

test('recorder never places a rejection message in a durable record', async () => {
  const seen: CameraReceiverLifecyclePayload[] = [];
  const secret = 'https://app.petal.live/?token=super-secret-token';
  const recorder = createCameraReceiverLifecycleRecorder({
    invoke: async (payload) => {
      seen.push(payload);
      throw new Error(secret);
    }
  });
  await recorder.record({ phase: 'interval_failed', detail: 'invoke_rejected' });
  const serialized = JSON.stringify(seen);
  assert.ok(!serialized.includes('super-secret-token'), serialized);
  assert.ok(!serialized.includes('petal.live'), serialized);
});

test('a terminal lifecycle_failed record is not retried again', async () => {
  const seen: CameraReceiverLifecyclePayload[] = [];
  const recorder = createCameraReceiverLifecycleRecorder({
    invoke: async (payload) => {
      seen.push(payload);
      throw new Error('dead');
    }
  });
  assert.equal(await recorder.record({ phase: 'lifecycle_failed' }), false);
  assert.equal(seen.length, 1);
  assert.equal(recorder.attempts(), 1);
});
