import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Namespace import: each test fails on its own against an older module.
import * as cameraOn from '../src/lib/meeting/cameraOn';
import type { CameraOnSteps } from '../src/lib/meeting/cameraOn';
import { createJoinGate } from '../src/lib/meeting/joinGate';

const notInRoom = async (): Promise<never> => {
  throw 'not in room';
};

function steps(overrides: Partial<CameraOnSteps> & { calls?: string[] } = {}): CameraOnSteps {
  const calls = overrides.calls ?? [];
  return {
    acquireSelfView: async () => (calls.push('preview'), true),
    whenJoined: async () => (calls.push('joined'), true),
    stillWanted: () => true,
    startPublish: async () => (calls.push('publish'), { published: true, generation: 1 }),
    ...overrides
  };
}

test('mid-join click: native keeps it for the join, nothing is shown', async () => {
  const outcome = await cameraOn.turnCameraOnSequence(
    steps({ startPublish: async () => ({ published: false, generation: 7 }) })
  );
  assert.deepEqual(outcome, { kind: 'requested', result: { published: false, generation: 7 } });
});

test("refused before this route's join reached native: waits for it, asks once more", async () => {
  const gate = createJoinGate();
  const calls: string[] = [];
  let attempts = 0;
  const run = cameraOn.turnCameraOnSequence(
    steps({
      calls,
      whenJoined: () => gate.whenJoined(),
      startPublish: async () => {
        calls.push('publish');
        attempts += 1;
        if (attempts === 1) throw 'not in room';
        return { published: true, generation: 2 };
      }
    })
  );
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls, ['preview', 'publish'], 'no second attempt before joining');
  gate.update('connected');
  assert.equal((await run).kind, 'requested');
  assert.equal(attempts, 2);
});

// PR #503 F2: a refusal means native has no meeting pending at all, so it is
// never a publish failure, and there is no settle window to wait out: a
// second refusal in a stale "connected" phase drops the preview at once.
test('a refusal is never a publish failure and waits on no timer', async () => {
  const gate = createJoinGate('connected');
  let attempts = 0;
  const started = Date.now();
  const outcome = await cameraOn.turnCameraOnSequence(
    steps({
      whenJoined: () => gate.whenJoined(),
      startPublish: async () => {
        attempts += 1;
        return notInRoom();
      }
    })
  );
  assert.deepEqual(outcome, { kind: 'no-room' });
  assert.equal(attempts, 2);
  assert.ok(Date.now() - started < 1000, 'no grace period');
  const source = readFileSync(new URL('../src/lib/meeting/cameraOn.ts', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /setTimeout|SETTLE/);
});

test('a meeting that fails to join drops the preview quietly', async () => {
  const outcome = await cameraOn.turnCameraOnSequence(
    steps({ whenJoined: async () => false, startPublish: notInRoom })
  );
  assert.deepEqual(outcome, { kind: 'no-room' });
});

test('camera turned off while waiting never publishes again', async () => {
  let wanted = true;
  const gate = createJoinGate();
  let attempts = 0;
  const run = cameraOn.turnCameraOnSequence(
    steps({
      whenJoined: () => gate.whenJoined(),
      stillWanted: () => wanted,
      startPublish: async () => {
        attempts += 1;
        throw 'not in room';
      }
    })
  );
  await new Promise((r) => setTimeout(r, 0));
  wanted = false;
  gate.update('connected');
  assert.deepEqual(await run, { kind: 'superseded' });
  assert.equal(attempts, 1);
});

test('no preview means no publish attempt', async () => {
  const calls: string[] = [];
  const outcome = await cameraOn.turnCameraOnSequence(steps({ calls, acquireSelfView: async () => false }));
  assert.deepEqual(outcome, { kind: 'no-preview' });
  assert.deepEqual(calls, []);
});

// PR #503 F5: an OFF (or failure) from an older room generation answers a
// request that is gone; it must not stop the camera started for the new
// room, nor toast. A current one still does both.
test('camera-publish-state from an older generation is ignored', () => {
  const bound = 5;
  assert.equal(cameraOn.cameraPublishStateAction({ publishing: false, error: null, generation: 4 }, bound), 'ignore');
  assert.equal(cameraOn.cameraPublishStateAction({ publishing: false, error: 'x', generation: 4 }, bound), 'ignore');
  assert.equal(cameraOn.cameraPublishStateAction({ publishing: false, error: 'x', generation: 5 }, bound), 'failed');
  assert.equal(cameraOn.cameraPublishStateAction({ publishing: false, error: null, generation: 6 }, bound), 'off');
  assert.equal(cameraOn.cameraPublishStateAction({ publishing: true, error: null, generation: 6 }, bound), 'on');
});

// PR #503 F4: the deferred camera is published at the room commit, before
// join() resolves (the macOS audio tail can hold it for tens of seconds), so
// the route must already be listening when it calls join().
test('the meeting route listens for camera-publish-state before it joins', () => {
  const route = readFileSync(
    new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
    'utf8'
  );
  const listener = route.indexOf('listen<CameraPublishState>(');
  const join = route.indexOf('await meeting.join()');
  assert.ok(listener > 0 && join > 0);
  assert.ok(listener < join, 'listener registered before join()');
  assert.match(route, /cameraPublishStateAction\(event\.payload, cameraGeneration\)/);
});

test('join gate follows the live phase across a failed join, retry and reconnect', async () => {
  const gate = createJoinGate();
  const first = gate.whenJoined();
  gate.update('disconnected'); // initial join failed
  assert.equal(await first, false);
  assert.equal(await gate.whenJoined(), false);

  gate.update('connecting'); // user pressed retry
  const second = gate.whenJoined();
  gate.update('connected');
  assert.equal(await second, true);
  assert.equal(await gate.whenJoined(), true);

  gate.update('connecting'); // connection lost, recovering
  const third = gate.whenJoined();
  let settled = false;
  void third.then(() => (settled = true));
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(settled, false, 'waits through recovery');
  gate.update('connected');
  assert.equal(await third, true);
});

test('join gate releases waiters as not-joined on dispose', async () => {
  const gate = createJoinGate();
  const waiting = gate.whenJoined();
  gate.dispose();
  assert.equal(await waiting, false);
  assert.equal(await gate.whenJoined(), false);
});
