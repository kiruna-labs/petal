import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import {
  createCameraJoinPublish,
  isNotInRoomRefusal,
  type CameraJoinPublishSeams
} from '../src/lib/meeting/cameraJoinPublish.ts';
import type { MeetingPhase } from '../src/lib/meeting/meetingSession.svelte.ts';
import type { StartCameraPublishResult } from '../src/lib/ipc.ts';

// #503: clicking Camera while the meeting was still joining showed the sticky
// "publishing to others failed" toast, because native refuses with "not in
// room" until the room commits. Retry a moment later always worked.

type Step = StartCameraPublishResult | string;

function rig(phase: MeetingPhase, steps: Step[]) {
  const log: string[] = [];
  const state = { phase };
  const scripted = [...steps];
  const seams: CameraJoinPublishSeams = {
    startPublish: async () => {
      log.push(`start@${state.phase}`);
      const next = scripted.shift() ?? 'no more scripted results';
      if (typeof next === 'string') throw next;
      return next;
    },
    meetingPhase: () => state.phase,
    onPublished: () => log.push('published'),
    onRetrying: () => log.push('retrying-toast'),
    onFailed: (e) => log.push(`retry-toast:${String(e)}`)
  };
  const camera = createCameraJoinPublish(seams);
  return { camera, log, state };
}

test('a not-in-room refusal while connecting shows no toast and publishes after connect', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', { published: true }]);
  await camera.publish();
  assert.deepEqual(log, ['start@connecting'], 'no toast of any kind during the join');
  assert.equal(camera.deferred, true, 'the camera intent is kept for the join');

  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting', 'start@connected', 'published']);
  assert.equal(camera.deferred, false);
});

test('a refusal during connection recovery is deferred the same way', async () => {
  // recoverLostConnection reports 'connecting' for the whole rejoin.
  const { camera, log, state } = rig('connecting', ['not in room', 'not in room', { published: true }]);
  await camera.publish();
  await camera.publish(); // the user clicks again mid-recovery
  assert.deepEqual(log, ['start@connecting', 'start@connecting']);
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log.slice(2), ['start@connected', 'published']);
});

test('a refusal while connected still shows the Retry toast', async () => {
  const { camera, log } = rig('connected', ['not in room']);
  await camera.publish();
  assert.deepEqual(log, ['start@connected', 'retry-toast:not in room']);
});

test('a refusal at the start of a connection loss toasts and still publishes after recovery', async () => {
  // Native drops the room before room-left reaches the route, so the route
  // still reads 'connected' when the refusal lands.
  const { camera, log, state } = rig('connected', ['not in room', { published: true }]);
  await camera.publish();
  assert.deepEqual(log, ['start@connected', 'retry-toast:not in room']);
  assert.equal(camera.deferred, true, 'the intent survives into the recovery');
  state.phase = 'connecting';
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log.slice(2), ['start@connected', 'published']);
});

test('a real failure while connecting still shows the Retry toast', async () => {
  const { camera, log } = rig('connecting', ['camera capture failed: device busy']);
  await camera.publish();
  assert.deepEqual(log, ['start@connecting', 'retry-toast:camera capture failed: device busy']);
  assert.equal(camera.deferred, false);
});

test('the deferred publish that fails after connect shows the Retry toast', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', 'not in room']);
  await camera.publish();
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting', 'start@connected', 'retry-toast:not in room']);
});

test('a real failure after connect toasts and leaves nothing deferred', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', 'capture failed']);
  await camera.publish();
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting', 'start@connected', 'retry-toast:capture failed']);
  assert.equal(camera.deferred, false);
  await camera.meetingConnected();
  assert.equal(log.length, 3);
});

test('a refusal that lands after the meeting connected retries at once', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', { published: true }]);
  const first = camera.publish();
  state.phase = 'connected'; // the connected edge passes while start is in flight
  await first;
  assert.deepEqual(log, ['start@connecting', 'start@connected', 'published']);
});

test('turning the camera off drops a deferred publish', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', { published: true }]);
  await camera.publish();
  camera.cancel();
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting']);
});

test('a refusal that arrives after the camera was turned off does not re-arm it', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', { published: true }]);
  const pending = camera.publish();
  camera.cancel();
  await pending;
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting']);
});

test('self-heal and success results report as before', async () => {
  const healing = rig('connected', [{ published: false }]);
  await healing.camera.publish();
  assert.deepEqual(healing.log, ['start@connected', 'retrying-toast']);
  const ok = rig('connecting', [{ published: true }]);
  await ok.camera.publish();
  assert.deepEqual(ok.log, ['start@connecting', 'published']);
});

test('only the exact native "not in room" refusal is deferrable', () => {
  assert.equal(isNotInRoomRefusal('not in room'), true);
  assert.equal(isNotInRoomRefusal(new Error('not in room')), true);
  assert.equal(isNotInRoomRefusal('camera start cancelled'), false);
  assert.equal(isNotInRoomRefusal(undefined), false);
});

// The meeting route must actually route the Video toggle through the
// controller, call it on the connected edge, and drop it on camera-off.
const route = readFileSync(
  fileURLToPath(new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url)),
  'utf8'
);

test('the meeting route publishes through the join-aware controller', () => {
  const turnOn = route.match(/async function turnCameraOn\(\)[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(turnOn, /await cameraPublish\.publish\(\);/);
  assert.doesNotMatch(
    turnOn,
    /invoke<StartCameraPublishResult>\(COMMANDS\.startCameraPublish\)/,
    'turnCameraOn must not call start_camera_publish directly'
  );
  assert.match(route, /meetingPhase: \(\) => meeting\.meetingPhase/);
  assert.match(
    route,
    /if \(!cameraListenerReady \|\| meeting\.meetingPhase !== 'connected'\) return;\s*untrack\(\(\) => void cameraPublish\.meetingConnected\(\)\);/,
    'the connected edge must run the deferred publish'
  );
  // The connected edge must wait for the camera-publish-state listener, or
  // a self-heal's terminal failure after the deferred publish is lost.
  const listenerAt = route.indexOf('unlistenCameraPublishState = await listen');
  const readyAt = route.indexOf('cameraListenerReady = true;');
  assert.ok(listenerAt !== -1 && readyAt > listenerAt, 'ready only after the listener is registered');
  const release = route.match(/function releaseSelfViewPreview\(\)[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(release, /cameraPublish\.cancel\(\);/, 'camera-off must drop a deferred publish');
});
