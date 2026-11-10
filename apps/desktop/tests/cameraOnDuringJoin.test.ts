import assert from 'node:assert/strict';
import test from 'node:test';

import { turnCameraOnSequence, type CameraOnSteps } from '../src/lib/meeting/cameraOn';
import { createJoinGate } from '../src/lib/meeting/joinGate';

const notInRoom = async (): Promise<never> => {
  throw 'not in room';
};

function steps(overrides: Partial<CameraOnSteps> & { calls?: string[] } = {}): CameraOnSteps {
  const calls = overrides.calls ?? [];
  return {
    acquireSelfView: async () => (calls.push('preview'), true),
    whenJoined: async () => (calls.push('joined'), true),
    afterRefusal: async () => (calls.push('settled'), 'held' as const),
    stillWanted: () => true,
    startPublish: async () => (calls.push('publish'), { published: true }),
    ...overrides
  };
}

test('mid-join click: native defers it, nothing is shown', async () => {
  const outcome = await turnCameraOnSequence(
    steps({ startPublish: async () => ({ published: false }) })
  );
  assert.deepEqual(outcome, { kind: 'publishing' });
});

test('refused before the meeting connected: waits, retries once, no error', async () => {
  const gate = createJoinGate();
  const calls: string[] = [];
  let attempts = 0;
  const run = turnCameraOnSequence(
    steps({
      calls,
      afterRefusal: (ms) => gate.afterRefusal(ms),
      startPublish: async () => {
        calls.push('publish');
        attempts += 1;
        if (attempts === 1) throw 'not in room';
        return { published: true };
      }
    })
  );
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls, ['preview', 'publish'], 'no second attempt before connecting');
  gate.update('connected');
  assert.deepEqual(await run, { kind: 'published' });
  assert.equal(attempts, 2);
});

test('a refusal with the meeting connected is a real failure', async () => {
  const outcome = await turnCameraOnSequence(steps({ startPublish: notInRoom }));
  assert.equal(outcome.kind, 'failed');
});

test('camera turned off while waiting never publishes again', async () => {
  let wanted = true;
  const gate = createJoinGate();
  let attempts = 0;
  const run = turnCameraOnSequence(
    steps({
      afterRefusal: (ms) => gate.afterRefusal(ms),
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

test('a meeting that fails to connect drops the camera quietly', async () => {
  const outcome = await turnCameraOnSequence(
    steps({ afterRefusal: async () => 'disconnected', startPublish: notInRoom })
  );
  assert.deepEqual(outcome, { kind: 'join-failed' });
});

test('no preview means no publish attempt', async () => {
  const calls: string[] = [];
  const outcome = await turnCameraOnSequence(steps({ calls, acquireSelfView: async () => false }));
  assert.deepEqual(outcome, { kind: 'no-preview' });
  assert.deepEqual(calls, []);
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

// PR #503 review F1: a camera native deferred to the join is dropped natively
// if that join fails, so the route must drop it too. Its camera-publish-state
// listener isn't registered until the first join resolves.
test('deferred to a join that then fails: the preview goes too', async () => {
  const gate = createJoinGate();
  const run = turnCameraOnSequence(
    steps({
      whenJoined: () => gate.whenJoined(),
      startPublish: async () => ({ published: false })
    })
  );
  await new Promise((r) => setTimeout(r, 0));
  gate.update('disconnected');
  assert.deepEqual(await run, { kind: 'join-failed' });
});

// PR #503 review F4: native can drop the room a moment before the meeting
// route hears room-left. A refusal while the route still says connected must
// not be retried instantly in that same stale phase.
test('a refusal while still "connected" waits for the phase to move, not an instant retry', async () => {
  const gate = createJoinGate('connected');
  const calls: string[] = [];
  let attempts = 0;
  const run = turnCameraOnSequence(
    steps({
      calls,
      afterRefusal: (ms) => gate.afterRefusal(ms),
      startPublish: async () => {
        calls.push('publish');
        attempts += 1;
        if (attempts === 1) throw 'not in room';
        return { published: true };
      }
    })
  );
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls, ['preview', 'publish'], 'no instant retry in the stale phase');
  gate.update('connecting'); // room-left arrives: reconnecting
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(attempts, 1, 'still waiting through recovery');
  gate.update('connected');
  assert.deepEqual(await run, { kind: 'published' });
  assert.equal(attempts, 2);
});

// Re-review: the connection can drop again between recovery and the retry.
// That second refusal waits for the next recovery too.
test('a retry refused by a second drop waits for the next recovery', async () => {
  const gate = createJoinGate();
  let attempts = 0;
  const run = turnCameraOnSequence(
    steps({
      afterRefusal: (ms) => gate.afterRefusal(ms),
      startPublish: async () => {
        attempts += 1;
        if (attempts === 2) gate.update('connecting'); // dropped again
        if (attempts < 3) throw 'not in room';
        return { published: true };
      }
    })
  );
  await new Promise((r) => setTimeout(r, 0));
  gate.update('connected');
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(attempts, 2, 'second attempt refused, now waiting');
  gate.update('connected');
  assert.deepEqual(await run, { kind: 'published' });
  assert.equal(attempts, 3);
});

test('join gate afterRefusal: held, moved-and-joined, moved-and-dropped', async () => {
  const gate = createJoinGate('connected');
  assert.equal(await gate.afterRefusal(5), 'held');
  const recovering = gate.afterRefusal(60_000);
  gate.update('connecting');
  gate.update('connected');
  assert.equal(await recovering, 'joined');
  const dropped = gate.afterRefusal(60_000);
  gate.update('disconnected');
  assert.equal(await dropped, 'disconnected');
  const disposedGate = createJoinGate('connected');
  const pending = disposedGate.afterRefusal(60_000);
  disposedGate.dispose();
  assert.equal(await pending, 'disconnected');
});
