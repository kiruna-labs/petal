import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import {
  cameraStateAction,
  createCameraJoinPublish,
  isNotInRoomRefusal,
  type CameraJoinPublishSeams,
  type CameraStateAction
} from '../src/lib/meeting/cameraJoinPublish.ts';
import type { MeetingPhase } from '../src/lib/meeting/meetingSession.svelte.ts';
import type { CameraPublishState, StartCameraPublishResult } from '../src/lib/ipc.ts';

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
  const { camera, log, state } = rig('connecting', ['not in room', { published: true, generation: 1, seq: 0 }]);
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
  const { camera, log, state } = rig('connecting', ['not in room', 'not in room', { published: true, generation: 1, seq: 0 }]);
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
  const { camera, log, state } = rig('connected', ['not in room', { published: true, generation: 1, seq: 0 }]);
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
  const { camera, log, state } = rig('connecting', ['not in room', { published: true, generation: 1, seq: 0 }]);
  const first = camera.publish();
  state.phase = 'connected'; // the connected edge passes while start is in flight
  await first;
  assert.deepEqual(log, ['start@connecting', 'start@connected', 'published']);
});

test('turning the camera off drops a deferred publish', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', { published: true, generation: 1, seq: 0 }]);
  await camera.publish();
  camera.cancel();
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting']);
});

test('a refusal that arrives after the camera was turned off does not re-arm it', async () => {
  const { camera, log, state } = rig('connecting', ['not in room', { published: true, generation: 1, seq: 0 }]);
  const pending = camera.publish();
  camera.cancel();
  await pending;
  state.phase = 'connected';
  await camera.meetingConnected();
  assert.deepEqual(log, ['start@connecting']);
});

test('self-heal and success results report as before', async () => {
  const healing = rig('connected', [{ published: false, generation: 1, seq: 0 }]);
  await healing.camera.publish();
  assert.deepEqual(healing.log, ['start@connected', 'retrying-toast']);
  const ok = rig('connecting', [{ published: true, generation: 1, seq: 0 }]);
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

// PR #503: native keeps a camera turned on while a join is in flight and
// publishes it at the commit, answering `published: false`. That is not a
// self-heal: nothing to show while the meeting is connecting.
test('a camera native keeps for the in-flight join shows nothing while connecting', async () => {
  const { camera, log } = rig('connecting', [{ published: false, generation: 2, seq: 0 }]);
  await camera.publish();
  assert.deepEqual(log, ['start@connecting']);
  assert.equal(camera.deferred, false, 'native owns it now; the connected edge does nothing');
});

test('camera-publish-state from an older room generation is ignored', () => {
  const at = (publishing: boolean, error: string | null, generation: number): CameraPublishState => ({
    publishing,
    error,
    generation,
    seq: 1
  });
  assert.equal(cameraStateAction(at(false, null, 4), 5), 'ignore');
  assert.equal(cameraStateAction(at(false, 'x', 4), 5), 'ignore');
  assert.equal(cameraStateAction(at(false, 'x', 5), 5), 'failed');
  assert.equal(cameraStateAction(at(false, null, 6), 5), 'off');
  assert.equal(cameraStateAction(at(true, null, 6), 5), 'on');
});

// PR #503 round 2, item 4: an OFF from an older generation could land while
// the new start was still in flight, before the route knew which generation
// its request belonged to, and set the toggle OFF.
test('a stale OFF that lands while the start is in flight is dropped', async () => {
  let resolveStart!: (result: StartCameraPublishResult) => void;
  const camera = createCameraJoinPublish({
    startPublish: () => new Promise((resolve) => (resolveStart = resolve)),
    meetingPhase: () => 'connecting',
    onPublished: () => {},
    onRetrying: () => {},
    onFailed: () => {}
  });
  const applied: CameraStateAction[] = [];
  const apply = (action: CameraStateAction) => applied.push(action);
  camera.receive({ publishing: true, error: null, generation: 3, seq: 1 }, apply);
  assert.deepEqual(applied, ['on'], 'nothing in flight: delivered at once');

  const pending = camera.publish();
  // The previous room's teardown (generation 3) lands before start returns.
  camera.receive({ publishing: false, error: null, generation: 3, seq: 3 }, apply);
  camera.receive({ publishing: false, error: 'stale heal', generation: 3, seq: 4 }, apply);
  assert.deepEqual(applied, ['on'], 'held while the start is in flight');
  resolveStart({ published: false, generation: 4, seq: 2 });
  await pending;
  assert.deepEqual(applied, ['on'], 'older than the request: dropped');

  camera.receive({ publishing: false, error: null, generation: 3, seq: 5 }, apply);
  camera.receive({ publishing: true, error: null, generation: 4, seq: 6 }, apply);
  camera.receive({ publishing: false, error: 'capture failed', generation: 4, seq: 7 }, apply);
  assert.deepEqual(applied, ['on', 'on', 'failed'], "the request's own outcomes still apply");
});

test('an outcome for the request itself that lands mid-start is applied after it', async () => {
  let resolveStart!: (result: StartCameraPublishResult) => void;
  const log: string[] = [];
  const camera = createCameraJoinPublish({
    startPublish: () => new Promise((resolve) => (resolveStart = resolve)),
    meetingPhase: () => 'connected',
    onPublished: () => log.push('published'),
    onRetrying: () => log.push('retrying'),
    onFailed: () => log.push('failed')
  });
  const pending = camera.publish();
  camera.receive({ publishing: false, error: 'no frames', generation: 7, seq: 8 }, (a) => log.push(a));
  resolveStart({ published: false, generation: 7, seq: 7 });
  await pending;
  assert.deepEqual(log, ['failed'], 'the failure stands: no "still connecting" over its Retry');
});

// Round-1 review: turning the camera off while a start was in flight held
// its OFF, then the start's `published: true` turned the toggle back ON.
test('a camera turned off while its start is in flight stays off', async () => {
  let resolveStart!: (result: StartCameraPublishResult) => void;
  const log: string[] = [];
  const camera = createCameraJoinPublish({
    startPublish: () => new Promise((resolve) => (resolveStart = resolve)),
    meetingPhase: () => 'connected',
    onPublished: () => log.push('published'),
    onRetrying: () => log.push('retrying'),
    onFailed: () => log.push('failed')
  });
  const pending = camera.publish();
  camera.cancel(); // the Video toggle turned off
  // The stop waits for the control lock, so its OFF is emitted after.
  camera.receive({ publishing: false, error: null, generation: 2, seq: 3 }, (a) => log.push(a));
  resolveStart({ published: true, generation: 2, seq: 2 });
  await pending;
  assert.deepEqual(log, ['off']);
});

function inFlightRig() {
  let resolveStart!: (result: StartCameraPublishResult) => void;
  const log: string[] = [];
  const camera = createCameraJoinPublish({
    startPublish: () => new Promise((resolve) => (resolveStart = resolve)),
    meetingPhase: () => 'connected',
    onPublished: () => log.push('published'),
    onRetrying: () => log.push('retrying'),
    onFailed: () => log.push('failed')
  });
  const off = (seq: number) =>
    camera.receive({ publishing: false, error: null, generation: 5, seq }, (a) => log.push(a));
  return { camera, log, off, resolve: (r: StartCameraPublishResult) => resolveStart(r) };
}

// Round-2 review: OFF then a quick ON. The stop's OFF (same generation) was
// emitted before the start took the control lock, so its seq is not above
// the start's: dropped, and the toggle ends ON with native publishing.
test('an OFF emitted before the start decided never stops it', async () => {
  const { camera, log, off, resolve } = inFlightRig();
  const pending = camera.publish();
  off(4); // the user's stop, delivered while the new start is in flight
  resolve({ published: true, generation: 5, seq: 4 });
  await pending;
  assert.deepEqual(log, ['published']);
  off(3); // a late copy of an older OFF, after the start returned
  assert.deepEqual(log, ['published']);
});

// Round-3 review: another surface (the menubar) turns the camera off while
// the route's start is still in flight. That OFF was emitted after the start
// decided (higher seq), so it wins over `published: true`.
test('an OFF emitted after the start decided wins over its publish', async () => {
  const { camera, log, off, resolve } = inFlightRig();
  const pending = camera.publish();
  off(6);
  resolve({ published: true, generation: 5, seq: 5 });
  await pending;
  assert.deepEqual(log, ['off'], 'the toggle ends OFF, matching native');
  off(7); // and any later OFF still applies
  assert.deepEqual(log, ['off', 'off']);
});

// PR #503 F4: the deferred camera is published at the room commit, before
// join() resolves (the macOS audio tail can hold it for tens of seconds), so
// the route must already be listening when it calls join(), and every
// outcome must go through the controller's generation filter.
test('the meeting route listens for camera-publish-state before it joins', () => {
  const listener = route.indexOf('listen<CameraPublishState>(');
  const join = route.indexOf('await meeting.join()');
  assert.ok(listener > 0 && join > 0);
  assert.ok(listener < join, 'listener registered before join()');
  assert.match(route, /cameraPublish\.receive\(event\.payload, \(action\) =>/);
});

// PR #503 round 2, item 3: a failed join the route then abandons left native
// keeping a camera for it, so a later camera-on outside any meeting waited
// forever. The route tells native it is gone.
test('disposing the meeting route abandons its pending join natively', () => {
  const meetingSession = readFileSync(
    fileURLToPath(new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url)),
    'utf8'
  );
  const dispose = meetingSession.match(/function dispose\(\) \{[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(dispose, /abandonPendingJoin\(\);/);
  assert.match(meetingSession, /invoke\(COMMANDS\.abandonPendingJoin, \{ roomName \}\)/);
  // A join still in flight at dispose (first join, Try again, recovery)
  // abandons again once it settles: native may only begin it after dispose.
  const joinThis = meetingSession.match(/async function joinThisRoom\([\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(joinThis, /finally \{\s*if \(disposed\) abandonPendingJoin\(\);/);
  assert.equal(meetingSession.match(/await joinRoom\(/g)?.length, 1, 'every join goes through it');
  assert.equal(meetingSession.match(/await joinThisRoom\(/g)?.length, 2);
  const ipc = readFileSync(fileURLToPath(new URL('../src/lib/ipc.ts', import.meta.url)), 'utf8');
  assert.match(ipc, /abandonPendingJoin: 'abandon_pending_join'/);
});
