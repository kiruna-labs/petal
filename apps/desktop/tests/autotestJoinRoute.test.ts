import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import {
  autotestMeetingRoute,
  onceAutotestJoinResult,
  replayAutotestJoinResult,
  shouldExitMainInitialization,
  subscribeToAutotestJoinResult
} from '../src/lib/data/autotestJoinRoute.ts';
import type { AutotestJoinResult } from '../src/lib/ipc.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));
const autotest = readFileSync(resolve(__dirname, '../src-tauri/src/autotest.rs'), 'utf8');
const ipc = readFileSync(resolve(__dirname, '../src/lib/ipc.ts'), 'utf8');
const mainRoute = readFileSync(resolve(__dirname, '../src/routes/main/+page.svelte'), 'utf8');

test('autotest success uses a safe encoded active-meeting route', () => {
  assert.equal(autotestMeetingRoute('room/a?b'), '/meeting/room%2Fa%3Fb');
});

test('durable result replay handles a join completed before the listener mounted', async () => {
  const received: string[] = [];
  const replayed = await replayAutotestJoinResult(
    async () => ({ status: 'joined', roomName: 'room-before-listener' }),
    (result) => received.push(result.status === 'joined' ? result.roomName : result.reason),
    () => true
  );
  assert.deepEqual(replayed, { status: 'joined', roomName: 'room-before-listener' });
  assert.deepEqual(received, ['room-before-listener']);
});

test('active listener forwards a terminal result before the replay pull runs', async () => {
  let handler: ((result: AutotestJoinResult) => void) | undefined;
  const received: string[] = [];
  let savedUnlisten: (() => void) | undefined;

  await subscribeToAutotestJoinResult(
    async (next) => {
      handler = next;
      return () => undefined;
    },
    (result) => received.push(result.status),
    () => true,
    (unlisten) => (savedUnlisten = unlisten)
  );
  handler?.({ status: 'failed', reason: 'permission_denied' });

  assert.equal(typeof savedUnlisten, 'function');
  assert.deepEqual(received, ['failed']);
});

test('event and replay deliver a failed terminal result only once', async () => {
  const received: string[] = [];
  const deliver = onceAutotestJoinResult((result) => received.push(result.status));
  const failed: AutotestJoinResult = { status: 'failed', reason: 'permission_denied' };

  assert.equal(deliver(failed), true);
  const replayed = await replayAutotestJoinResult(async () => failed, deliver, () => true);

  assert.deepEqual(replayed, failed);
  assert.deepEqual(received, ['failed']);
});

test('failed replay continues main initialization while joined replay exits for navigation', async () => {
  const failed = await replayAutotestJoinResult(
    async () => ({ status: 'failed', reason: 'permission_denied' }),
    () => undefined,
    () => true
  );
  const joined = await replayAutotestJoinResult(
    async () => ({ status: 'joined', roomName: 'navigate-away' }),
    () => undefined,
    () => true
  );

  assert.equal(shouldExitMainInitialization(failed), false);
  assert.equal(shouldExitMainInitialization(joined), true);
});

test('a main-route remount cannot replay a consumed terminal result', async () => {
  let terminal: AutotestJoinResult | null = { status: 'joined', roomName: 'one-shot-room' };
  const takeTerminal = async () => {
    const result = terminal;
    terminal = null;
    return result;
  };
  const firstMount: string[] = [];
  const secondMount: string[] = [];

  assert.deepEqual(
    await replayAutotestJoinResult(takeTerminal, (result) => firstMount.push(result.status), () => true),
    { status: 'joined', roomName: 'one-shot-room' }
  );
  assert.equal(
    await replayAutotestJoinResult(takeTerminal, (result) => secondMount.push(result.status), () => true),
    null
  );
  assert.deepEqual(firstMount, ['joined']);
  assert.deepEqual(secondMount, []);
});

test('delayed listener subscription tears itself down after route destruction', async () => {
  let resolveSubscribe!: (unlisten: () => void) => void;
  let tornDown = 0;
  let active = true;
  const pending = subscribeToAutotestJoinResult(
    () => new Promise((resolve) => (resolveSubscribe = resolve)),
    () => assert.fail('destroyed route must not receive a late event'),
    () => active,
    () => assert.fail('destroyed route must not retain a late listener')
  );
  active = false;
  resolveSubscribe(() => {
    tornDown += 1;
  });
  await pending;
  assert.equal(tornDown, 1);
});

test('backend and frontend retain the explicit env-gated, redacted contract', () => {
  assert.match(autotest, /AutotestJoinState/);
  assert.match(autotest, /autotest_join_result/);
  assert.match(autotest, /backend_token_unavailable/);
  assert.doesNotMatch(autotest, /join_room\('\{room\}'\) failed: \{e:\?\}/);
  assert.match(ipc, /autotestJoinResult: 'autotest_join_result'/);
  assert.match(mainRoute, /replayAutotestJoinResult/);
  assert.match(mainRoute, /subscribeToAutotestJoinResult/);
});
