import { test } from 'node:test';
import assert from 'node:assert/strict';

import { requestTokenWithRetry } from '../src/connection.ts';

const TOKEN_URL = 'https://app.petal.live/api/token';
const TOKEN_RESPONSE = {
  url: 'wss://livekit.invalid',
  token: 'token',
  room: 'petal-room-test',
};

function jsonResponse(status: number, body: unknown, headers?: HeadersInit): Response {
  return new Response(JSON.stringify(body), { status, headers });
}

test('token request retries transient 429 responses and honors Retry-After', async () => {
  const delays: number[] = [];
  const responses = [
    jsonResponse(429, { error: 'rate limited' }, { 'Retry-After': '2' }),
    jsonResponse(200, TOKEN_RESPONSE),
  ];
  const fetchImpl: typeof fetch = async () => responses.shift()!;

  const result = await requestTokenWithRetry(TOKEN_URL, 'test', 'web-riley', 'Riley', {
    fetchImpl,
    delay: async (ms) => {
      delays.push(ms);
    },
  });

  assert.deepEqual(result, TOKEN_RESPONSE);
  assert.deepEqual(delays, [2000]);
  assert.equal(responses.length, 0);
});

test('token request retries network failures with configured backoff', async () => {
  const delays: number[] = [];
  let calls = 0;
  const fetchImpl: typeof fetch = async () => {
    calls += 1;
    if (calls === 1) throw new TypeError('fetch failed');
    return jsonResponse(200, TOKEN_RESPONSE);
  };

  const result = await requestTokenWithRetry(TOKEN_URL, 'test', 'web-riley', 'Riley', {
    fetchImpl,
    retryDelaysMs: [123],
    delay: async (ms) => {
      delays.push(ms);
    },
  });

  assert.deepEqual(result, TOKEN_RESPONSE);
  assert.equal(calls, 2);
  assert.deepEqual(delays, [123]);
});

test('token request bounds a hung attempt with the per-attempt timeout and retries it', async () => {
  let calls = 0;
  const retryErrors: string[] = [];
  const fetchImpl = (async (_url: RequestInfo | URL, init?: RequestInit) => {
    calls += 1;
    if (calls === 1) {
      // A stalled network: resolves never, rejects only on the harness abort.
      return new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener('abort', () =>
          reject(new DOMException('The operation was aborted.', 'AbortError'))
        );
      });
    }
    return jsonResponse(200, TOKEN_RESPONSE);
  }) as typeof fetch;

  const result = await requestTokenWithRetry(TOKEN_URL, 'test', 'web-riley', 'Riley', {
    fetchImpl,
    attemptTimeoutMs: 20,
    retryDelaysMs: [1],
    delay: async () => {},
    onRetry: (_attempt, error) => retryErrors.push(error.message),
  });

  assert.deepEqual(result, TOKEN_RESPONSE);
  assert.equal(calls, 2);
  // The deadline abort must read as a network timeout (matched by the
  // "could not reach the meeting server" copy), not as a user cancel.
  assert.deepEqual(retryErrors, ['token request timed out']);
});

test('token request surfaces non-transient 4xx responses without retrying', async () => {
  let calls = 0;
  const fetchImpl: typeof fetch = async () => {
    calls += 1;
    return jsonResponse(403, { error: 'invalid room credential' });
  };

  await assert.rejects(
    () =>
      requestTokenWithRetry(TOKEN_URL, 'test', 'web-riley', 'Riley', {
        fetchImpl,
        retryDelaysMs: [1, 2, 3],
        delay: async () => {},
      }),
    /invalid room credential/
  );
  assert.equal(calls, 1);
});
