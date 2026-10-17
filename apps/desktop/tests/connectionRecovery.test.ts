import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import {
  GALLERY_BRIDGE_MAX_RESTARTS,
  REJOIN_BACKOFF_MS,
  galleryBridgeRestartDelayMs,
  rejoinedMessage,
  runBoundedRejoin,
  type RejoinSeams
} from '../src/lib/meeting/connectionRecovery.ts';
import {
  clearMeetingRouteRoom,
  connectionLossNeedsToast,
  setMeetingRouteRoom
} from '../src/lib/stores/meetingRoute.ts';

// PETAL-DESKTOP-2R (#354): when LiveKit's SDK gives up reconnecting, the
// native room emits `room-left { connectionLost: true }`. The meeting route
// must rejoin with bounded backoff instead of showing "Meeting ended".

function fakeSeams(joinResults: Array<'ok' | Error>, overrides: Partial<RejoinSeams> = {}) {
  const log: string[] = [];
  const results = [...joinResults];
  const seams: RejoinSeams = {
    join: async () => {
      log.push('join');
      const next = results.shift() ?? new Error('no more scripted results');
      if (next !== 'ok') throw next;
    },
    leave: async () => {
      log.push('leave');
    },
    sleep: async (ms) => {
      log.push(`sleep:${ms}`);
    },
    cancelled: () => false,
    onAttempt: (attempt, total) => log.push(`attempt:${attempt}/${total}`),
    ...overrides
  };
  return { seams, log };
}

test('a lost connection is rejoined after the backoff, retrying failed attempts', async () => {
  const { seams, log } = fakeSeams([new Error('backend token request timed out'), 'ok']);
  const outcome = await runBoundedRejoin(seams, [10, 20, 30]);
  assert.deepEqual(outcome, { kind: 'rejoined', attempts: 2 });
  assert.deepEqual(log, ['sleep:10', 'attempt:1/3', 'join', 'sleep:20', 'attempt:2/3', 'join']);
});

test('the rejoin is bounded: every attempt failing ends in a terminal failure', async () => {
  const lastError = new Error('room connect failed');
  const { seams, log } = fakeSeams([new Error('a'), new Error('b'), lastError]);
  const outcome = await runBoundedRejoin(seams, [1, 2, 3]);
  assert.equal(outcome.kind, 'failed');
  assert.equal(outcome.kind === 'failed' && outcome.lastError, lastError);
  assert.equal(log.filter((l) => l === 'join').length, 3);
});

test('Leave during the backoff stops the rejoin before any join', async () => {
  let cancelled = false;
  const { seams, log } = fakeSeams(['ok'], {
    sleep: async () => {
      cancelled = true; // the user clicked Leave while we waited
    },
    cancelled: () => cancelled
  });
  const outcome = await runBoundedRejoin(seams, [5]);
  assert.deepEqual(outcome, { kind: 'cancelled' });
  assert.ok(!log.includes('join'));
});

test('a join that lands after the user left is undone, not kept', async () => {
  let cancelled = false;
  const { seams, log } = fakeSeams([], {
    join: async () => {
      log.push('join');
      cancelled = true; // Leave clicked while the join was in flight
    },
    cancelled: () => cancelled
  });
  const outcome = await runBoundedRejoin(seams, [0]);
  assert.deepEqual(outcome, { kind: 'cancelled' });
  assert.deepEqual(log.slice(-2), ['join', 'leave']);
});

test('the default backoff escalates and stays bounded', () => {
  assert.ok(REJOIN_BACKOFF_MS.length >= 3 && REJOIN_BACKOFF_MS.length <= 5);
  for (let i = 1; i < REJOIN_BACKOFF_MS.length; i += 1) {
    assert.ok(REJOIN_BACKOFF_MS[i] > REJOIN_BACKOFF_MS[i - 1]);
  }
});

test('the reconnected toast tells the user when shares need restarting', () => {
  assert.equal(rejoinedMessage(0), 'Reconnected');
  assert.equal(rejoinedMessage(1), 'Reconnected — share your window again');
  assert.equal(rejoinedMessage(3), 'Reconnected — share your windows again');
});

const meetingSessionSource = readFileSync(
  fileURLToPath(new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url)),
  'utf8'
);

test('combined room-left order: user leave, rejoin stash, failed join, mid-join stash, then dispatch', () => {
  const listener = meetingSessionSource.match(/unlistenRoomLeft = await listen[\s\S]*?\n {6}\}\);/)?.[0];
  assert.ok(listener, 'room-left listener should exist');
  const order = [
    'if (selfLeaveRequested) return;',
    'if (event.payload.userLeave) {',
    'rejoinCancelled = true;',
    'if (reconnecting) {',
    'if (joinFailed) return;',
    "if (meetingPhase === 'connecting') {",
    'handleRoomLeft(event.payload);'
  ].map((needle) => {
    const at = listener.indexOf(needle);
    assert.ok(at !== -1, `listener must contain ${needle}`);
    return at;
  });
  assert.deepEqual([...order].sort((a, b) => a - b), order, 'guards must run in this order');
  const handler = meetingSessionSource.match(/function handleRoomLeft\([\s\S]*?\n  \}/)?.[0] ?? '';
  assert.ok(handler.indexOf('recoverLostConnection(') < handler.indexOf("beginTerminalReturn('Meeting ended"));
});

test('the rejoin is cancellable from any surface, room-scoped on undo, and epoch-guarded', () => {
  const recover = meetingSessionSource.match(/async function recoverLostConnection\([\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(recover, /cancelled: \(\) => stale\(\) \|\| selfLeaveRequested \|\| rejoinCancelled/);
  assert.match(recover, /if \(\(await currentRoom\(\)\) === roomName\) await leaveRoom\(\);/);
  assert.match(recover, /const epoch = \+\+recoveryEpoch;/);
  // `reconnecting` is cleared on the success path only after the stash is
  // checked, and the dead-room replay happens before 'connected'.
  const tail = recover.slice(recover.lastIndexOf('const stashed = pendingRoomLeft;'));
  assert.ok(tail.indexOf('handleRoomLeft(stashed)') < tail.indexOf("meetingPhase = 'connected'"));
  assert.match(recover, /clearTimeout\(galleryBridgeRestartTimer\);/);
});

test('gallery-bridge restarts back off 2s -> 60s and are capped', () => {
  assert.equal(galleryBridgeRestartDelayMs(0), 2000);
  assert.equal(galleryBridgeRestartDelayMs(1), 4000);
  assert.equal(galleryBridgeRestartDelayMs(4), 32000);
  assert.equal(galleryBridgeRestartDelayMs(5), 60000);
  assert.equal(galleryBridgeRestartDelayMs(20), 60000);
  assert.ok(GALLERY_BRIDGE_MAX_RESTARTS > 0 && GALLERY_BRIDGE_MAX_RESTARTS <= 10);
  assert.doesNotMatch(meetingSessionSource, /Connection lost - returning you to the room list/);
  assert.match(meetingSessionSource, /if \(galleryBridgeRestarts >= GALLERY_BRIDGE_MAX_RESTARTS\)/);
  // A bridge that connects after stop/dispose is dropped, not kept.
  assert.match(meetingSessionSource, /if \(generation !== galleryBridgeGeneration \|\| disposed\) \{\s*void bridge\.disconnect\(\)/);
});

test('a lost connection with no meeting route mounted still shows the Disconnected toast', () => {
  setMeetingRouteRoom('room-a');
  assert.equal(connectionLossNeedsToast({ roomName: 'room-a', connectionLost: true }), false);
  assert.equal(connectionLossNeedsToast({ roomName: 'room-a', connectionLost: false }), false);
  clearMeetingRouteRoom('room-a');
  assert.equal(connectionLossNeedsToast({ roomName: 'room-a', connectionLost: true }), true);
});
