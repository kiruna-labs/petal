import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ConnectionError } from 'livekit-client';

import { connectWithRetry, isTransientConnectError, CONNECT_RETRY_DELAYS_MS } from '../src/connection.ts';

test('initial connect retries transient failures and then succeeds', async () => {
  let calls = 0;
  const delays: number[] = [];
  const retries: string[] = [];

  await connectWithRetry(
    async () => {
      calls += 1;
      if (calls === 1) throw ConnectionError.serverUnreachable('could not establish signal connection');
      if (calls === 2) throw new TypeError('WebSocket closed before handshake');
    },
    {
      delay: async (ms) => {
        delays.push(ms);
      },
      onRetry: (_attempt, error) => retries.push(error.message),
    }
  );

  assert.equal(calls, 3);
  assert.deepEqual(delays, [...CONNECT_RETRY_DELAYS_MS]);
  assert.deepEqual(retries, [
    'could not establish signal connection',
    'WebSocket closed before handshake',
  ]);
});

test('initial connect never retries an auth rejection', async () => {
  let calls = 0;

  await assert.rejects(
    () =>
      connectWithRetry(
        async () => {
          calls += 1;
          throw ConnectionError.notAllowed('invalid token', 401);
        },
        { delay: async () => {} }
      ),
    /invalid token/
  );
  assert.equal(calls, 1);
});

test('initial connect surfaces the final transient error after exhausting retries', async () => {
  let calls = 0;

  await assert.rejects(
    () =>
      connectWithRetry(
        async () => {
          calls += 1;
          throw ConnectionError.serverUnreachable('still unreachable');
        },
        { delay: async () => {}, retryDelaysMs: [1, 2] }
      ),
    /still unreachable/
  );
  assert.equal(calls, 3);
});

test('transient classification: server/network errors retry, user-shaped ones do not', () => {
  assert.equal(isTransientConnectError(ConnectionError.serverUnreachable('down')), true);
  assert.equal(isTransientConnectError(ConnectionError.internal('boom')), true);
  assert.equal(isTransientConnectError(ConnectionError.timeout('slow')), true);
  assert.equal(isTransientConnectError(new TypeError('fetch failed')), true);
  assert.equal(isTransientConnectError(ConnectionError.notAllowed('nope', 401)), false);
  assert.equal(isTransientConnectError(ConnectionError.cancelled('user left')), false);
});
