// /api/ai-token unit tests (#655) — Gemini Live ephemeral-token minting for a
// live meeting participant.
//
// What is REAL here and what is mocked, deliberately:
//   REAL  — the LiveKit JWT crypto. Tokens are signed with `AccessToken` and
//           checked by the handler's production `TokenVerifier` path, so a
//           wrong-secret / expired / mismatched token is rejected by actual
//           signature+claim verification rather than by a stub that agrees
//           with us.
//   MOCK  — `listParticipants` (no LiveKit server in unit tests) and the
//           Gemini minter (no network, no API key, no spend).
//
// LIVEKIT_URL points at 127.0.0.1:1 so the few adapter tests that reach the
// real RoomServiceClient fail with an instant ECONNREFUSED instead of emitting
// DNS traffic for a fake hostname.

import assert from 'node:assert/strict';
import { AccessToken } from 'livekit-server-sdk';
import type { VercelRequest, VercelResponse } from '@vercel/node';
import { credentialForAccessCode, livekitRoomName } from '../lib/slug.js';
import type { RoomDiscoveryService, RoomMetadataService } from '../lib/livekit.js';
import {
  AI_TOKEN_ATTEMPT_BUCKET_CAPACITY,
  AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS,
  AI_TOKEN_IDENTITY_BUCKET_CAPACITY,
  AI_TOKEN_IP_BUCKET_CAPACITY,
  AI_TOKEN_UPSTREAM_BUDGET_MS,
  handleAiToken,
  handleToken,
  HttpError,
  resetTokenRateLimitsForTest,
  type AiTokenContext,
} from '../lib/handlers.js';
import {
  DEFAULT_GEMINI_LIVE_MODEL,
  loadGeminiEnv,
  mintGeminiEphemeralToken,
  type EphemeralTokenRequest,
  type GeminiTokenMinter,
} from '../lib/gemini.js';
import aiTokenHandler from '../api/ai-token.js';

const originalEnv = {
  LIVEKIT_URL: process.env.LIVEKIT_URL,
  LIVEKIT_API_KEY: process.env.LIVEKIT_API_KEY,
  LIVEKIT_API_SECRET: process.env.LIVEKIT_API_SECRET,
  GEMINI_API_KEY: process.env.GEMINI_API_KEY,
  GEMINI_LIVE_MODEL: process.env.GEMINI_LIVE_MODEL,
  GEMINI_API_VERSION: process.env.GEMINI_API_VERSION,
};

const API_KEY = 'ai_token_test_key';
const API_SECRET = 'ai_token_test_secret_ai_token_test_secret';
// Marker values: no log line, response body, or error message may contain them.
const GEMINI_KEY = 'AIzaTESTGEMINIKEYdonotleak';
const MINTED_TOKEN = 'authTokens/FAKE-do-not-log-this-token-value';

process.env.LIVEKIT_URL = 'ws://127.0.0.1:1';
process.env.LIVEKIT_API_KEY = API_KEY;
process.env.LIVEKIT_API_SECRET = API_SECRET;
process.env.GEMINI_API_KEY = GEMINI_KEY;
delete process.env.GEMINI_LIVE_MODEL;
delete process.env.GEMINI_API_VERSION;

function restoreEnv() {
  for (const [key, value] of Object.entries(originalEnv)) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
}

const ALICE_ID = '11111111-1111-4111-8111-111111111111';
const BOB_ID = 'web-22222222-2222-4222-8222-222222222222';
const CREDENTIAL = credentialForAccessCode('aaa-bbbb-ccc')!;
const ROOM = livekitRoomName(CREDENTIAL);
const OTHER_ROOM = livekitRoomName(credentialForAccessCode('ddd-eeee-fff')!);
const NOW_MS = Date.UTC(2026, 7, 5, 12, 0, 0);
const EXPECTED_EXPIRE = new Date(NOW_MS + 12 * 60_000).toISOString();
const EXPECTED_NEW_SESSION = new Date(NOW_MS + 30_000).toISOString();

interface JwtOptions {
  identity?: string;
  room?: string;
  apiKey?: string;
  apiSecret?: string;
  ttl?: number | string;
  roomJoin?: boolean;
}

async function livekitJwt(options: JwtOptions = {}): Promise<string> {
  const at = new AccessToken(options.apiKey ?? API_KEY, options.apiSecret ?? API_SECRET, {
    identity: options.identity ?? ALICE_ID,
    ttl: options.ttl ?? '24h',
  });
  at.addGrant({
    roomJoin: options.roomJoin ?? true,
    room: options.room ?? ROOM,
    canPublish: true,
    canSubscribe: true,
    canPublishData: true,
  });
  return at.toJwt();
}

function participants(identities: string[]): RoomDiscoveryService {
  return {
    async listParticipants() {
      return identities.map((identity) => ({ identity }));
    },
  } as unknown as RoomDiscoveryService;
}

interface MintRecord {
  request: EphemeralTokenRequest;
  model: string;
  apiKey: string;
}

// Models Google's DOCUMENTED create response, which is `{ "name": … }` and
// carries no expiry — so the default fake reports none, exactly like the real
// thing. A fake that echoed the requested expireTime back would have hidden the
// bug where the handler substituted its own request for Google's answer.
function recordingMinter(records: MintRecord[]): GeminiTokenMinter {
  return async (env, request) => {
    records.push({ request, model: env.model, apiKey: env.apiKey });
    return { token: MINTED_TOKEN, model: env.model };
  };
}

const okMinter: GeminiTokenMinter = async (env) => ({
  token: MINTED_TOKEN,
  model: env.model,
});

// A promise that never settles — stands in for a hung upstream.
function hangs<T>(): Promise<T> {
  return new Promise<T>(() => {});
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// A cancellable deadline, so a race that the work wins leaves no timer behind.
function deadline(ms: number): { promise: Promise<'timeout'>; cancel: () => void } {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const promise = new Promise<'timeout'>((resolve) => {
    timer = setTimeout(() => resolve('timeout'), ms);
  });
  return { promise, cancel: () => { if (timer) clearTimeout(timer); } };
}

// Slow-but-SUCCESSFUL upstreams: the exact shape that caused the amplification.
// Nothing fails here — the backend answers correctly, just not instantly.
function slowParticipants(identities: string[], delayMs: number): RoomDiscoveryService {
  return {
    async listParticipants() {
      await sleep(delayMs);
      return identities.map((identity) => ({ identity }));
    },
  } as unknown as RoomDiscoveryService;
}

function slowMinter(records: MintRecord[], delayMs: number): GeminiTokenMinter {
  return async (env, request) => {
    await sleep(delayMs);
    records.push({ request, model: env.model, apiKey: env.apiKey });
    return { token: MINTED_TOKEN, model: env.model };
  };
}

// `transport::backend_http::send_with_retry`'s loop, in miniature: cap each
// attempt, retry on timeout, give up on a real error. It is what the desktop
// client used to do to this route, and re-running the real handler under it is
// the only way to count what a single click actually costs.
async function simulateRetryingClient(options: {
  attemptTimeoutMs: number;
  retries: number;
  run: () => Promise<unknown>;
}): Promise<{ attempts: number; succeeded: boolean }> {
  const inflight: Promise<unknown>[] = [];
  let attempts = 0;
  let succeeded = false;
  for (let i = 0; i <= options.retries; i++) {
    attempts++;
    const work = options.run();
    // Keep hold of every abandoned attempt: it carries on server-side and its
    // mint still lands, which is the entire point of counting them.
    inflight.push(work.catch(() => undefined));
    const timer = deadline(options.attemptTimeoutMs);
    const outcome = await Promise.race([
      work.then(
        () => 'ok' as const,
        () => 'error' as const
      ),
      timer.promise,
    ]);
    timer.cancel();
    if (outcome === 'ok') {
      succeeded = true;
      break;
    }
    if (outcome === 'error') break; // a 4xx: send_with_retry does not retry these
  }
  await Promise.all(inflight);
  return { attempts, succeeded };
}

async function captureConsole(fn: () => Promise<void> | void): Promise<string[]> {
  const messages: string[] = [];
  const originals = {
    debug: console.debug,
    error: console.error,
    info: console.info,
    log: console.log,
    warn: console.warn,
  };
  const record =
    (level: keyof typeof originals) =>
    (...args: unknown[]) => {
      messages.push(`${level}: ${args.map(String).join(' ')}`);
    };
  console.debug = record('debug');
  console.error = record('error');
  console.info = record('info');
  console.log = record('log');
  console.warn = record('warn');
  try {
    await fn();
  } finally {
    console.debug = originals.debug;
    console.error = originals.error;
    console.info = originals.info;
    console.log = originals.log;
    console.warn = originals.warn;
  }
  return messages;
}

type TestResponse = {
  statusCode: number;
  headers: Record<string, string | number | readonly string[]>;
  body: unknown;
  ended: boolean;
  status(code: number): TestResponse;
  json(body: unknown): TestResponse;
  send(body: unknown): TestResponse;
  end(body?: unknown): TestResponse;
  setHeader(name: string, value: string | number | readonly string[]): TestResponse;
};

function res(): TestResponse {
  return {
    statusCode: 200,
    headers: {},
    body: undefined,
    ended: false,
    status(code: number) {
      this.statusCode = code;
      return this;
    },
    json(body: unknown) {
      this.body = body;
      this.ended = true;
      return this;
    },
    send(body: unknown) {
      this.body = body;
      this.ended = true;
      return this;
    },
    end(body?: unknown) {
      this.body = body;
      this.ended = true;
      return this;
    },
    setHeader(name: string, value: string | number | readonly string[]) {
      this.headers[name.toLowerCase()] = value;
      return this;
    },
  };
}

async function callAdapter(
  method: string,
  body: unknown,
  headers: Record<string, string> = {}
): Promise<TestResponse> {
  const response = res();
  await aiTokenHandler(
    { method, body, headers } as unknown as VercelRequest,
    response as unknown as VercelResponse
  );
  return response;
}

async function withoutGeminiKey(fn: () => Promise<void>) {
  const saved = process.env.GEMINI_API_KEY;
  delete process.env.GEMINI_API_KEY;
  try {
    await fn();
  } finally {
    if (saved === undefined) delete process.env.GEMINI_API_KEY;
    else process.env.GEMINI_API_KEY = saved;
  }
}

async function withGeminiModel(model: string | undefined, fn: () => Promise<void>) {
  const saved = process.env.GEMINI_LIVE_MODEL;
  if (model === undefined) delete process.env.GEMINI_LIVE_MODEL;
  else process.env.GEMINI_LIVE_MODEL = model;
  try {
    await fn();
  } finally {
    if (saved === undefined) delete process.env.GEMINI_LIVE_MODEL;
    else process.env.GEMINI_LIVE_MODEL = saved;
  }
}

async function withMissingLiveKitEnv(fn: () => Promise<void>) {
  const saved = {
    LIVEKIT_URL: process.env.LIVEKIT_URL,
    LIVEKIT_API_KEY: process.env.LIVEKIT_API_KEY,
    LIVEKIT_API_SECRET: process.env.LIVEKIT_API_SECRET,
  };
  delete process.env.LIVEKIT_URL;
  delete process.env.LIVEKIT_API_KEY;
  delete process.env.LIVEKIT_API_SECRET;
  try {
    await fn();
  } finally {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
}

async function baseContext(overrides: Partial<AiTokenContext> = {}): Promise<AiTokenContext> {
  return {
    authorization: `Bearer ${await livekitJwt()}`,
    nowMs: NOW_MS,
    service: participants([ALICE_ID, BOB_ID]),
    mintEphemeralToken: okMinter,
    ...overrides,
  };
}

function isHttpError(status: number) {
  return (err: unknown) => err instanceof HttpError && err.status === status;
}

interface CapturedFetch {
  url: string;
  method?: string;
  headers: Record<string, string>;
  body: Record<string, unknown>;
}

// Runs `fn` with global fetch replaced, so the REAL @google/genai minter can be
// exercised without a network call or an API key. The SDK calls bare `fetch`.
async function withStubbedFetch(
  respond: () => Response,
  fn: () => Promise<void>
): Promise<CapturedFetch[]> {
  const calls: CapturedFetch[] = [];
  const original = globalThis.fetch;
  globalThis.fetch = (async (input: unknown, init?: RequestInit) => {
    const rawHeaders = init?.headers;
    const headers =
      rawHeaders instanceof Headers
        ? Object.fromEntries(rawHeaders.entries())
        : ((rawHeaders as Record<string, string>) ?? {});
    calls.push({
      url: String(input),
      method: init?.method,
      headers,
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : {},
    });
    return respond();
  }) as typeof fetch;
  try {
    await fn();
  } finally {
    globalThis.fetch = original;
  }
  return calls;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

let failures = 0;
async function test(name: string, fn: () => Promise<void> | void) {
  try {
    await resetTokenRateLimitsForTest();
    await fn();
    console.log(`  ok   ${name}`);
  } catch (err) {
    failures++;
    console.error(`  FAIL ${name}`);
    console.error(err);
  }
}

async function main() {
  console.log('/api/ai-token happy path:');

  await test('mints a constrained ephemeral token for a verified live participant', async () => {
    const records: MintRecord[] = [];
    const response = await handleAiToken(
      { room: CREDENTIAL, identity: ALICE_ID },
      await baseContext({ mintEphemeralToken: recordingMinter(records) })
    );
    assert.deepEqual(response, {
      token: MINTED_TOKEN,
      // Google reported no expiry, so the response carries none — only the
      // ceiling we asked for, under a name that cannot be mistaken for it.
      requestedExpireTime: EXPECTED_EXPIRE,
      model: DEFAULT_GEMINI_LIVE_MODEL,
    });
    assert.equal(records.length, 1);
    assert.equal(records[0]!.request.uses, 1, 'token must be single-use');
    assert.equal(
      records[0]!.request.newSessionExpireTime,
      EXPECTED_NEW_SESSION,
      'the session must be opened within 30s of minting'
    );
    assert.equal(records[0]!.request.expireTime, EXPECTED_EXPIRE);
    assert.equal(records[0]!.request.responseModality, 'AUDIO');
    assert.equal(records[0]!.model, DEFAULT_GEMINI_LIVE_MODEL);
    assert.equal(records[0]!.apiKey, GEMINI_KEY, 'the real key reaches only the minter');
  });

  await test('the response model comes from GEMINI_LIVE_MODEL, so rotation needs no client release', async () => {
    await withGeminiModel('models/gemini-9.9-flash-live-preview', async () => {
      const records: MintRecord[] = [];
      const response = await handleAiToken(
        { room: CREDENTIAL, identity: ALICE_ID },
        await baseContext({ mintEphemeralToken: recordingMinter(records) })
      );
      assert.equal(response.model, 'models/gemini-9.9-flash-live-preview');
      assert.equal(records[0]!.model, 'models/gemini-9.9-flash-live-preview');
    });
  });

  await test('a blank GEMINI_LIVE_MODEL falls back to the pinned default', async () => {
    await withGeminiModel('   ', async () => {
      const response = await handleAiToken(
        { room: CREDENTIAL, identity: ALICE_ID },
        await baseContext()
      );
      assert.equal(response.model, DEFAULT_GEMINI_LIVE_MODEL);
    });
  });

  await test('the response carries only token/expiry/model fields — never a secret', async () => {
    const response = await handleAiToken(
      { room: CREDENTIAL, identity: ALICE_ID },
      await baseContext()
    );
    assert.deepEqual(Object.keys(response).sort(), ['model', 'requestedExpireTime', 'token']);
    const serialized = JSON.stringify(response);
    assert.ok(!serialized.includes(GEMINI_KEY));
    assert.ok(!serialized.includes(API_SECRET));
  });

  await test('the whole flow works end to end through the Vercel adapter', async () => {
    // The adapter has no injection seam, so this asserts the wiring
    // (CORS -> method -> body -> Authorization header -> handler) rather than
    // a successful mint: the live-participant check fails against the
    // unreachable RoomServiceClient and collapses to 403, as designed.
    const response = await callAdapter(
      'POST',
      { room: CREDENTIAL, identity: ALICE_ID },
      { authorization: `Bearer ${await livekitJwt()}` }
    );
    assert.equal(response.statusCode, 403);
    assert.deepEqual(response.body, { error: 'not currently a participant in this room' });
  });

  console.log('');
  console.log('auth layer 1 — LiveKit JWT proof of identity:');

  await test('a missing Authorization header is 401', async () => {
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: undefined })
        ),
      isHttpError(401)
    );
  });

  await test('a non-Bearer Authorization header is 401', async () => {
    const bare = await livekitJwt();
    for (const authorization of ['', 'Bearer', 'Bearer   ', 'Basic abc', bare]) {
      await assert.rejects(
        async () =>
          handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, await baseContext({ authorization })),
        isHttpError(401),
        `expected 401 for authorization ${JSON.stringify(authorization.slice(0, 12))}`
      );
    }
  });

  await test('a JWT signed with a foreign secret is 401 (real signature verification)', async () => {
    const forged = await livekitJwt({ apiSecret: 'not_our_secret_not_our_secret_xx' });
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: `Bearer ${forged}` })
        ),
      isHttpError(401)
    );
  });

  await test('an expired JWT is 401 — a stale join token cannot keep buying AI sessions', async () => {
    const expired = await livekitJwt({ ttl: -600 });
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: `Bearer ${expired}` })
        ),
      isHttpError(401)
    );
  });

  await test('garbage in the Bearer position is 401, not a 500', async () => {
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: 'Bearer not.a.jwt' })
        ),
      isHttpError(401)
    );
  });

  await test("a valid JWT for someone ELSE's identity is 403 (no minting on a teammate's bucket)", async () => {
    const bobToken = await livekitJwt({ identity: BOB_ID });
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: `Bearer ${bobToken}` })
        ),
      isHttpError(403)
    );
  });

  await test('a valid JWT for a DIFFERENT room is 403', async () => {
    const otherRoomToken = await livekitJwt({ room: OTHER_ROOM });
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: `Bearer ${otherRoomToken}` })
        ),
      isHttpError(403)
    );
  });

  await test('a valid JWT without roomJoin is 403', async () => {
    const noJoin = await livekitJwt({ roomJoin: false });
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ authorization: `Bearer ${noJoin}` })
        ),
      isHttpError(403)
    );
  });

  await test('a rejected JWT never reaches LiveKit or the minter', async () => {
    const records: MintRecord[] = [];
    let listed = 0;
    const counting = {
      async listParticipants() {
        listed++;
        return [{ identity: ALICE_ID }];
      },
    } as unknown as RoomDiscoveryService;
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({
            authorization: `Bearer ${await livekitJwt({ identity: BOB_ID })}`,
            service: counting,
            mintEphemeralToken: recordingMinter(records),
          })
        ),
      isHttpError(403)
    );
    assert.equal(listed, 0);
    assert.deepEqual(records, []);
  });

  console.log('');
  console.log('auth layer 2 — live-participant liveness (#109 anchor):');

  await test('a verified identity that is not currently connected is 403', async () => {
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ service: participants([BOB_ID]) })
        ),
      isHttpError(403)
    );
  });

  await test('the liveness check asks LiveKit about the DERIVED room name', async () => {
    let asked: string | undefined;
    const spy = {
      async listParticipants(room: string) {
        asked = room;
        return [{ identity: ALICE_ID }];
      },
    } as unknown as RoomDiscoveryService;
    await handleAiToken(
      { room: CREDENTIAL, identity: ALICE_ID },
      await baseContext({ service: spy })
    );
    assert.equal(asked, ROOM);
  });

  await test('a room LiveKit cannot resolve collapses to the same 403 (no existence oracle)', async () => {
    const throwing = {
      async listParticipants() {
        throw new Error('twirp: room not found');
      },
    } as unknown as RoomDiscoveryService;
    await assert.rejects(
      async () =>
        handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, await baseContext({ service: throwing })),
      isHttpError(403)
    );
  });

  await test('a non-participant never reaches the minter', async () => {
    const records: MintRecord[] = [];
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({
            service: participants([BOB_ID]),
            mintEphemeralToken: recordingMinter(records),
          })
        ),
      isHttpError(403)
    );
    assert.deepEqual(records, []);
  });

  console.log('');
  console.log('request validation:');

  await test('a missing or non-string room is 400', async () => {
    const bodies = [{}, { room: '' }, { room: '   ' }, { room: 42 }];
    for (const body of bodies) {
      await assert.rejects(
        async () => handleAiToken(body as { room: string }, await baseContext()),
        isHttpError(400)
      );
    }
  });

  await test('a missing identity is 400', async () => {
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL }, await baseContext()),
      isHttpError(400)
    );
  });

  await test('a human-readable, spoofable, or bridge identity is 400', async () => {
    for (const identity of ['alice', 'Jane Doe', `${ALICE_ID}-gallery`]) {
      await assert.rejects(
        async () => handleAiToken({ room: CREDENTIAL, identity }, await baseContext()),
        isHttpError(400),
        `expected 400 for identity ${identity}`
      );
    }
  });

  await test('a bare room label without a capability suffix is 400', async () => {
    await assert.rejects(
      async () => handleAiToken({ room: 'eng-sync', identity: ALICE_ID }, await baseContext()),
      isHttpError(400)
    );
  });

  await test('oversized room and identity fields are 400', async () => {
    const tooLong = 'x'.repeat(129);
    await assert.rejects(
      async () => handleAiToken({ room: tooLong, identity: ALICE_ID }, await baseContext()),
      isHttpError(400)
    );
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: tooLong }, await baseContext()),
      isHttpError(400)
    );
  });

  console.log('');
  console.log('configuration (the documented kill switch):');

  await test('missing GEMINI_API_KEY is 503 and never calls LiveKit or the minter', async () => {
    const records: MintRecord[] = [];
    let listed = 0;
    const counting = {
      async listParticipants() {
        listed++;
        return [{ identity: ALICE_ID }];
      },
    } as unknown as RoomDiscoveryService;
    await withoutGeminiKey(async () => {
      const response = await callAdapter('POST', { room: CREDENTIAL, identity: ALICE_ID });
      assert.equal(response.statusCode, 503);
      assert.deepEqual(response.body, { error: 'AI chat is not configured' });
      await assert.rejects(
        async () =>
          handleAiToken(
            { room: CREDENTIAL, identity: ALICE_ID },
            await baseContext({ service: counting, mintEphemeralToken: recordingMinter(records) })
          ),
        (err) => (err as Error).name === 'GeminiConfigError'
      );
    });
    assert.equal(listed, 0);
    assert.deepEqual(records, []);
  });

  await test('missing LiveKit env is 503 through the adapter', async () => {
    await withMissingLiveKitEnv(async () => {
      const response = await callAdapter(
        'POST',
        { room: CREDENTIAL, identity: ALICE_ID },
        { authorization: 'Bearer whatever' }
      );
      assert.equal(response.statusCode, 503);
      assert.deepEqual(response.body, { error: 'LiveKit not configured' });
    });
  });

  console.log('');
  console.log('rate limits (spend buckets commit on success; attempts bounded separately):');

  await test(`the identity+room bucket allows ${AI_TOKEN_IDENTITY_BUCKET_CAPACITY}/hour then 429s`, async () => {
    const context = await baseContext({ rateLimitKey: '203.0.113.7' });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
    }
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
      isHttpError(429)
    );
    // A different participant on the same IP is unaffected by Alice's bucket.
    const bob = await handleAiToken(
      { room: CREDENTIAL, identity: BOB_ID },
      await baseContext({
        authorization: `Bearer ${await livekitJwt({ identity: BOB_ID })}`,
        rateLimitKey: '203.0.113.7',
      })
    );
    assert.equal(bob.token, MINTED_TOKEN);
  });

  await test('the identity bucket refills over an hour', async () => {
    const context = await baseContext({ rateLimitKey: '203.0.113.8' });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
    }
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
      isHttpError(429)
    );
    const later = await handleAiToken(
      { room: CREDENTIAL, identity: ALICE_ID },
      { ...context, nowMs: NOW_MS + 60 * 60_000 }
    );
    assert.equal(later.token, MINTED_TOKEN);
  });

  await test(
    `ACCEPTED LIMITATION: churning identities evades the ${AI_TOKEN_IDENTITY_BUCKET_CAPACITY}/hour per-identity cap, ` +
      `leaving only the ${AI_TOKEN_IP_BUCKET_CAPACITY}/hour IP cap`,
    async () => {
    // This test DOCUMENTS a bypass; it does not endorse one. A caller who mints
    // under a fresh generated identity each time never meets their own
    // AI_TOKEN_IDENTITY_BUCKET_CAPACITY and is stopped only by the much larger
    // per-IP cap asserted below.
    //
    // Deliberately left open (product decision, #655). Every one of these calls
    // still has to present a valid LiveKit JWT for THIS room and be live in it,
    // which means holding the room credential — and the room credential IS the
    // capability to be in the meeting at all. Someone who can churn identities
    // was invited; the churn buys them nothing they did not already have, so
    // closing it would cost real complexity (durable per-caller identity) to
    // defend against a caller who is already inside. The IP cap is the bound
    // that matters, and it is what the assertions below actually pin.
    //
    // Distinct identities so the per-identity bucket never fires first — the
    // 429 below can only have come from the IP bucket.
    const identity = (i: number) => `web-33333333-3333-4333-8333-${i.toString().padStart(12, '0')}`;
    for (let i = 0; i < AI_TOKEN_IP_BUCKET_CAPACITY; i++) {
      const id = identity(i);
      await handleAiToken(
        { room: CREDENTIAL, identity: id },
        await baseContext({
          authorization: `Bearer ${await livekitJwt({ identity: id })}`,
          service: participants([id]),
          rateLimitKey: '198.51.100.4',
        })
      );
    }
    const overflow = identity(AI_TOKEN_IP_BUCKET_CAPACITY);
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: overflow },
          await baseContext({
            authorization: `Bearer ${await livekitJwt({ identity: overflow })}`,
            service: participants([overflow]),
            rateLimitKey: '198.51.100.4',
          })
        ),
      isHttpError(429)
    );
    // A different source IP is unaffected.
    const fresh = identity(AI_TOKEN_IP_BUCKET_CAPACITY + 1);
    const ok = await handleAiToken(
      { room: CREDENTIAL, identity: fresh },
      await baseContext({
        authorization: `Bearer ${await livekitJwt({ identity: fresh })}`,
        service: participants([fresh]),
        rateLimitKey: '198.51.100.5',
      })
    );
    assert.equal(ok.token, MINTED_TOKEN);
    }
  );

  await test(
    `repeated FAILURES stay bounded: the attempt bucket caps them at ${AI_TOKEN_ATTEMPT_BUCKET_CAPACITY}/hour`,
    async () => {
      // The spend buckets now commit only on a successful mint, so "always
      // fail" must not become an unlimited free probe. The attempt bucket is
      // what closes that, and it is charged on ENTRY — before any crypto or
      // upstream work — so an unauthenticated flood costs the backend nothing.
      const flood: AiTokenContext = {
        authorization: 'Bearer garbage',
        nowMs: NOW_MS,
        rateLimitKey: '198.51.100.9',
      };
      for (let i = 0; i < AI_TOKEN_ATTEMPT_BUCKET_CAPACITY; i++) {
        await assert.rejects(
          async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, flood),
          isHttpError(401),
          `attempt ${i + 1} should still be a plain 401`
        );
      }
      await assert.rejects(
        async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, flood),
        isHttpError(429)
      );
    }
  );

  await test('a FAILED liveness check costs no spend slot — the caller got nothing', async () => {
    const key = '198.51.100.30';
    // Alice is not in the room: 403 every time, and each 403 must be free.
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY * 3; i++) {
      await assert.rejects(
        async () =>
          handleAiToken(
            { room: CREDENTIAL, identity: ALICE_ID },
            await baseContext({ service: participants([BOB_ID]), rateLimitKey: key })
          ),
        isHttpError(403)
      );
    }
    // Her full hourly allowance must still be there once she really is live.
    const context = await baseContext({ rateLimitKey: key });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      const ok = await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
      assert.equal(ok.token, MINTED_TOKEN, `mint ${i + 1} must be allowed`);
    }
    // ...and only the real mints are charged.
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
      isHttpError(429)
    );
  });

  await test('a FAILED upstream mint costs no spend slot — Google billed nobody', async () => {
    const key = '198.51.100.31';
    const failing = await baseContext({
      rateLimitKey: key,
      mintEphemeralToken: () =>
        Promise.reject(Object.assign(new Error('upstream down'), { status: 500 })),
    });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY * 3; i++) {
      await assert.rejects(
        async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, failing),
        isHttpError(502)
      );
    }
    const context = await baseContext({ rateLimitKey: key });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      assert.equal(
        (await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context)).token,
        MINTED_TOKEN,
        `mint ${i + 1} must be allowed after ${AI_TOKEN_IDENTITY_BUCKET_CAPACITY * 3} upstream failures`
      );
    }
  });

  await test('a timed-out mint costs no spend slot either', async () => {
    const key = '198.51.100.32';
    const hanging = await baseContext({
      rateLimitKey: key,
      mintEphemeralToken: () => hangs(),
      upstreamTimeoutMs: 25,
    });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      await assert.rejects(
        async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, hanging),
        isHttpError(503)
      );
    }
    const context = await baseContext({ rateLimitKey: key });
    assert.equal(
      (await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context)).token,
      MINTED_TOKEN
    );
  });

  await test('the IP spend bucket is likewise charged only for real mints', async () => {
    const key = '198.51.100.33';
    // Enough failures to have drained a 60-capacity IP bucket under the old
    // charge-on-entry behaviour, spread over distinct identities so nothing
    // else could be the limiter.
    const identity = (i: number) => `web-44444444-4444-4444-8444-${i.toString().padStart(12, '0')}`;
    for (let i = 0; i < AI_TOKEN_IP_BUCKET_CAPACITY; i++) {
      const id = identity(i);
      await assert.rejects(
        async () =>
          handleAiToken(
            { room: CREDENTIAL, identity: id },
            await baseContext({
              authorization: `Bearer ${await livekitJwt({ identity: id })}`,
              service: participants([BOB_ID]),
              rateLimitKey: key,
            })
          ),
        isHttpError(403)
      );
    }
    const after = identity(AI_TOKEN_IP_BUCKET_CAPACITY);
    const ok = await handleAiToken(
      { room: CREDENTIAL, identity: after },
      await baseContext({
        authorization: `Bearer ${await livekitJwt({ identity: after })}`,
        service: participants([after]),
        rateLimitKey: key,
      })
    );
    assert.equal(ok.token, MINTED_TOKEN, 'failed attempts must not have drained the IP spend cap');
  });

  await test('ai-token minting does not drain the /api/token join bucket', async () => {
    const context = await baseContext({ rateLimitKey: '198.51.100.20' });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
    }
    const noRooms = {
      async listRooms() {
        return [];
      },
    } as unknown as RoomMetadataService;
    const joined = await handleToken(
      { room: CREDENTIAL, identity: ALICE_ID },
      { rateLimitKey: '198.51.100.20', nowMs: NOW_MS, service: noRooms }
    );
    assert.ok(joined.token.length > 0, 'joining a meeting must survive an AI-chat burst');
  });

  console.log('');
  console.log('upstream resilience (must answer inside vercel.json maxDuration: 10):');

  await test('a hung listParticipants times out as 503, not a platform kill', async () => {
    const hanging = {
      listParticipants: () => hangs<never>(),
    } as unknown as RoomDiscoveryService;
    const started = Date.now();
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ service: hanging, upstreamTimeoutMs: 25 })
        ),
      isHttpError(503)
    );
    assert.ok(Date.now() - started < 2_000, 'the timeout must actually fire');
  });

  await test('a hung Gemini mint times out as 503', async () => {
    const started = Date.now();
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ mintEphemeralToken: () => hangs(), upstreamTimeoutMs: 25 })
        ),
      isHttpError(503)
    );
    assert.ok(Date.now() - started < 2_000, 'the timeout must actually fire');
  });

  await test('an upstream Gemini error becomes a 502 carrying only its status number', async () => {
    const apiError = Object.assign(
      new Error('{"error":{"message":"quota for billing project 12345 exhausted"}}'),
      { status: 429, name: 'ApiError' }
    );
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({ mintEphemeralToken: () => Promise.reject(apiError) })
        ),
      (err) => {
        assert.ok(err instanceof HttpError);
        // Google's own status must never become OUR status (a 429 upstream
        // would read as our rate limit), and its raw JSON body must never
        // become our message.
        assert.equal(err.status, 502);
        assert.equal(err.message, 'ai token service unavailable (upstream 429)');
        return true;
      }
    );
  });

  await test('a mint that returns no token name is a 502, never an empty token', async () => {
    await assert.rejects(
      async () =>
        handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({
            mintEphemeralToken: () =>
              Promise.reject(new Error('gemini auth token response carried no token name')),
          })
        ),
      isHttpError(502)
    );
  });

  console.log('');
  console.log('cost: one call must mint exactly one billable token:');

  await test('both upstream calls share ONE budget — a slow liveness cannot buy the mint a second one', async () => {
    // The defect this pins: two INDEPENDENT 4s budgets meant the route could
    // legitimately spend 8s, well past the 5s its own client was willing to
    // wait. Here the liveness call eats most of a scaled budget and the mint
    // then hangs; the route must still answer inside the ONE budget, not twice
    // it. Under per-call budgets this takes ~1.8x the budget and fails.
    const budgetMs = 300;
    const context = await baseContext({
      service: slowParticipants([ALICE_ID], Math.round(budgetMs * 0.8)),
      mintEphemeralToken: () => hangs(),
      upstreamTimeoutMs: budgetMs,
    });
    const started = Date.now();
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
      isHttpError(503)
    );
    const elapsed = Date.now() - started;
    assert.ok(
      elapsed < budgetMs * 1.4,
      `the budget must bound BOTH upstream calls together; took ${elapsed}ms of a ${budgetMs}ms budget`
    );
  });

  await test('a mint is never STARTED once the budget is already spent', async () => {
    // Paying Google for a token we have no time left to hand back is pure
    // waste, so an exhausted budget must stop before the minter, not inside it.
    const records: MintRecord[] = [];
    const budgetMs = 120;
    const context = await baseContext({
      service: slowParticipants([ALICE_ID], budgetMs),
      mintEphemeralToken: recordingMinter(records),
      upstreamTimeoutMs: budgetMs,
    });
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
      isHttpError(503)
    );
    assert.deepEqual(records, [], 'no token may be bought that cannot be delivered');
  });

  await test('one call mints exactly ONE token: a slow-but-successful backend is waited out, not retried', async () => {
    // Every number below is DERIVED from the shipped constants at 1/10
    // wall-clock scale, so shrinking the client's attempt budget or splitting
    // the server's budget per phase again breaks this test rather than quietly
    // restoring the amplification.
    const SCALE = 10;
    const serverBudgetMs = AI_TOKEN_UPSTREAM_BUDGET_MS / SCALE;
    const clientAttemptMs = AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS / SCALE;
    // What the desktop client used to allow one attempt, via the shared
    // retrying helper: 5s, i.e. less than the route's own 8s worst case.
    const preFixClientAttemptMs = 5_000 / SCALE;
    // A healthy but unhurried upstream. Nothing FAILS here — this is the shape
    // that made the bug so expensive: the backend answered correctly every
    // single time, just not fast enough for a client that had stopped
    // listening. 44% of the budget each, so the route answers at ~88% of it:
    // inside the shipped client budget, past the pre-fix one.
    const upstreamDelayMs = Math.round(serverBudgetMs * 0.44);

    const attempt = async (records: MintRecord[], rateLimitKey: string) => {
      const context = await baseContext({
        rateLimitKey,
        service: slowParticipants([ALICE_ID], upstreamDelayMs),
        mintEphemeralToken: slowMinter(records, upstreamDelayMs),
        upstreamTimeoutMs: serverBudgetMs,
      });
      return () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
    };

    // CONTROL — the PRE-FIX client budget against this very same handler. If
    // this leg does not amplify, the harness cannot see the bug at all and the
    // shipped assertion below would prove nothing. (Backoff between retries is
    // omitted: it changes the wall clock, never the number of tokens bought.)
    await resetTokenRateLimitsForTest();
    const preFixMints: MintRecord[] = [];
    const preFix = await simulateRetryingClient({
      attemptTimeoutMs: preFixClientAttemptMs,
      retries: 3,
      run: await attempt(preFixMints, '198.51.100.40'),
    });
    assert.equal(preFix.attempts, 4, 'the pre-fix client abandoned and re-POSTed three times');
    assert.equal(preFix.succeeded, false, 'and the user still saw a failure...');
    assert.equal(preFixMints.length, 4, '...having paid for four real tokens');

    // SHIPPED — the client waits the documented budget out instead.
    await resetTokenRateLimitsForTest();
    const mints: MintRecord[] = [];
    const shipped = await simulateRetryingClient({
      attemptTimeoutMs: clientAttemptMs,
      retries: 3,
      run: await attempt(mints, '198.51.100.41'),
    });
    assert.equal(shipped.attempts, 1, 'one attempt is the only safe number for a non-idempotent mint');
    assert.equal(shipped.succeeded, true, 'the slow-but-successful mint is delivered, not abandoned');
    assert.equal(mints.length, 1, 'exactly one billable token per call');
  });

  await test('the same slow call costs exactly one hourly slot, not four', async () => {
    const SCALE = 10;
    const serverBudgetMs = AI_TOKEN_UPSTREAM_BUDGET_MS / SCALE;
    const upstreamDelayMs = Math.round(serverBudgetMs * 0.44);
    const key = '198.51.100.42';
    const slow = await baseContext({
      rateLimitKey: key,
      service: slowParticipants([ALICE_ID], upstreamDelayMs),
      mintEphemeralToken: slowMinter([], upstreamDelayMs),
      upstreamTimeoutMs: serverBudgetMs,
    });
    assert.equal(
      (await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, slow)).token,
      MINTED_TOKEN
    );
    // AI_TOKEN_IDENTITY_BUCKET_CAPACITY - 1 must remain. Under the old timing
    // the same single click left only 2 of 6.
    const fast = await baseContext({ rateLimitKey: key });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY - 1; i++) {
      assert.equal(
        (await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, fast)).token,
        MINTED_TOKEN,
        `slot ${i + 2} of ${AI_TOKEN_IDENTITY_BUCKET_CAPACITY} must still be available`
      );
    }
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, fast),
      isHttpError(429)
    );
  });

  await test('the timing contract holds across vercel.json, this backend, and the desktop client', async () => {
    const { readFileSync } = await import('node:fs');
    const vercel = JSON.parse(
      readFileSync(new URL('../vercel.json', import.meta.url), 'utf8')
    ) as { functions?: Record<string, { maxDuration?: number }> };
    const maxDurationMs = (vercel.functions?.['api/*.ts']?.maxDuration ?? 0) * 1_000;
    assert.ok(maxDurationMs > 0, 'vercel.json must pin a maxDuration for this route');

    // The route must always ANSWER rather than be killed mid-mint...
    assert.ok(
      AI_TOKEN_UPSTREAM_BUDGET_MS < maxDurationMs,
      `the ${AI_TOKEN_UPSTREAM_BUDGET_MS}ms upstream budget must fit inside the ${maxDurationMs}ms function ceiling`
    );
    // ...and every client must outwait even a platform kill, because the
    // alternative is paying for a mint it then refuses to collect.
    assert.ok(
      AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS > maxDurationMs,
      'clients must outwait the function ceiling, not race it'
    );

    // Cross-language lockstep with the one shipped client. These two numbers
    // drifting apart IS the bug: 5s of client patience against 8s of server
    // work bought four tokens per click.
    const rust = readFileSync(
      new URL('../../apps/desktop/src-tauri/src/ai_chat/commands.rs', import.meta.url),
      'utf8'
    );
    const declared = rust.match(
      /AI_TOKEN_REQUEST_TIMEOUT:\s*Duration\s*=\s*Duration::from_secs\((\d+)\)/
    );
    assert.ok(declared, 'the desktop client must declare its own ai-token attempt timeout');
    assert.ok(
      Number(declared![1]) * 1_000 >= AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS,
      `the desktop client waits ${declared![1]}s; the contract is ${AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS}ms`
    );
    // ...and must not retry it. Built by concatenation so this assertion is not
    // itself the thing it forbids if the check ever moves into that file.
    const retryingHelper = ['send', '_with_retry'].join('');
    const fetchBody = rust.split('async fn fetch_ai_token')[1]?.split('\n}\n')[0];
    assert.ok(fetchBody, 'fetch_ai_token must exist in the desktop client');
    assert.ok(
      !fetchBody!.includes(retryingHelper),
      'the ai-token mint must not go through the retrying backend helper: it re-POSTs a mint'
    );
  });

  console.log('');
  console.log('expiry reporting (measured, never modelled):');

  await test('expireTime reports what GOOGLE returned, never what we asked for', async () => {
    // Google may clamp or ignore the requested window. Whatever it says is the
    // only truth about the created token; our request is a separate fact and is
    // reported under a separate name.
    const googleExpiry = new Date(NOW_MS + 5 * 60_000).toISOString();
    assert.notEqual(googleExpiry, EXPECTED_EXPIRE, 'the fixture must differ from the request');
    let response: Awaited<ReturnType<typeof handleAiToken>> | undefined;
    await withStubbedFetch(
      () => jsonResponse({ name: 'authTokens/CLAMPED', expireTime: googleExpiry }),
      async () => {
        const context = await baseContext();
        delete context.mintEphemeralToken; // the production minter
        response = await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
      }
    );
    assert.deepEqual(response, {
      token: 'authTokens/CLAMPED',
      expireTime: googleExpiry,
      requestedExpireTime: EXPECTED_EXPIRE,
      model: DEFAULT_GEMINI_LIVE_MODEL,
    });
  });

  await test('expireTime is OMITTED when Google returned none — the request is never substituted', async () => {
    let response: Awaited<ReturnType<typeof handleAiToken>> | undefined;
    await withStubbedFetch(
      // Google's documented response shape: the name, and nothing else.
      () => jsonResponse({ name: 'authTokens/NO-EXPIRY' }),
      async () => {
        const context = await baseContext();
        delete context.mintEphemeralToken;
        response = await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
      }
    );
    assert.ok(!('expireTime' in response!), 'an unknown expiry must be absent, not guessed');
    assert.equal(response!.requestedExpireTime, EXPECTED_EXPIRE);
  });

  await test('a blank or non-string expireTime from Google is treated as absent, not echoed', async () => {
    for (const value of ['', '   ', 42, null]) {
      let response: Awaited<ReturnType<typeof handleAiToken>> | undefined;
      await withStubbedFetch(
        () => jsonResponse({ name: 'authTokens/ODD', expireTime: value }),
        async () => {
          await resetTokenRateLimitsForTest();
          const context = await baseContext();
          delete context.mintEphemeralToken;
          response = await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
        }
      );
      assert.ok(
        !('expireTime' in response!),
        `expireTime ${JSON.stringify(value)} must not be reported as an expiry`
      );
    }
  });

  console.log('');
  console.log('the REAL @google/genai minter (global fetch stubbed, no network):');

  await test('sends the wire shape Google actually accepts, with the key in a header', async () => {
    let minted: { token: string; model: string; expireTime?: string } | undefined;
    const calls = await withStubbedFetch(
      () => jsonResponse({ name: 'authTokens/LIVE-SHAPE-CHECK' }),
      async () => {
        minted = await mintGeminiEphemeralToken(loadGeminiEnv(), {
          uses: 1,
          expireTime: EXPECTED_EXPIRE,
          newSessionExpireTime: EXPECTED_NEW_SESSION,
          responseModality: 'AUDIO',
        });
      }
    );
    assert.equal(calls.length, 1);
    const call = calls[0]!;
    assert.equal(call.method, 'POST');
    assert.equal(call.url, 'https://generativelanguage.googleapis.com/v1beta/auth_tokens');
    assert.equal(call.headers['x-goog-api-key'], GEMINI_KEY, 'key travels as a header');
    assert.ok(!call.url.includes(GEMINI_KEY), 'the key must never appear in a URL');

    assert.equal(call.body.uses, 1);
    assert.equal(call.body.expireTime, EXPECTED_EXPIRE);
    assert.equal(call.body.newSessionExpireTime, EXPECTED_NEW_SESSION);
    // The #654 spike proved raw REST rejects `liveConnectConstraints` with
    // `400 Unknown name`; the SDK's job is to emit these two fields instead.
    // If this assertion ever fails, model/modality locking has silently
    // stopped being applied.
    assert.equal(call.body.liveConnectConstraints, undefined);
    assert.deepEqual(call.body.bidiGenerateContentSetup, {
      model: DEFAULT_GEMINI_LIVE_MODEL,
      generationConfig: { responseModalities: ['AUDIO'] },
    });
    assert.equal(call.body.fieldMask, 'model,generationConfig.responseModalities');

    // The response's `name` IS the token, passed verbatim by clients. Google
    // sent no expireTime here, so the minter reports none rather than handing
    // our own request back as if it were Google's answer.
    assert.deepEqual(minted, {
      token: 'authTokens/LIVE-SHAPE-CHECK',
      model: DEFAULT_GEMINI_LIVE_MODEL,
    });
  });

  await test('honours GEMINI_LIVE_MODEL in the constraint it sends upstream', async () => {
    await withGeminiModel('models/gemini-9.9-flash-live-preview', async () => {
      const calls = await withStubbedFetch(
        () => jsonResponse({ name: 'authTokens/x' }),
        async () => {
          await mintGeminiEphemeralToken(loadGeminiEnv(), {
            uses: 1,
            expireTime: EXPECTED_EXPIRE,
            newSessionExpireTime: EXPECTED_NEW_SESSION,
            responseModality: 'AUDIO',
          });
        }
      );
      assert.equal(
        (calls[0]!.body.bidiGenerateContentSetup as { model: string }).model,
        'models/gemini-9.9-flash-live-preview'
      );
    });
  });

  await test('a response without a token name throws, and never echoes the body', async () => {
    await withStubbedFetch(
      () => jsonResponse({ unexpected: 'shape' }),
      async () => {
        await assert.rejects(
          () =>
            mintGeminiEphemeralToken(loadGeminiEnv(), {
              uses: 1,
              expireTime: EXPECTED_EXPIRE,
              newSessionExpireTime: EXPECTED_NEW_SESSION,
              responseModality: 'AUDIO',
            }),
          (err) => {
            assert.ok(err instanceof Error);
            assert.ok(!err.message.includes('unexpected'));
            return true;
          }
        );
      }
    );
  });

  await test('the full route works end to end against the real minter', async () => {
    let response: Awaited<ReturnType<typeof handleAiToken>> | undefined;
    const calls = await withStubbedFetch(
      () => jsonResponse({ name: 'authTokens/END-TO-END' }),
      async () => {
        const context = await baseContext();
        delete context.mintEphemeralToken; // use the production minter
        response = await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
      }
    );
    assert.equal(calls.length, 1, 'exactly one upstream mint per request');
    assert.deepEqual(response, {
      token: 'authTokens/END-TO-END',
      requestedExpireTime: EXPECTED_EXPIRE,
      model: DEFAULT_GEMINI_LIVE_MODEL,
    });
  });

  await test('an upstream 4xx from Google becomes our 502, never its own status or body', async () => {
    await withStubbedFetch(
      () =>
        jsonResponse(
          { error: { message: 'quota for billing project 12345 exhausted', code: 429 } },
          429
        ),
      async () => {
        const context = await baseContext();
        delete context.mintEphemeralToken;
        await assert.rejects(
          () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
          (err) => {
            assert.ok(err instanceof HttpError);
            assert.equal(err.status, 502);
            assert.ok(!err.message.includes('12345'));
            assert.ok(!err.message.includes('quota'));
            return true;
          }
        );
      }
    );
  });

  console.log('');
  console.log('vercel adapter:');

  await test('non-POST methods are 405', async () => {
    for (const method of ['GET', 'PUT', 'DELETE']) {
      const response = await callAdapter(method, undefined);
      assert.equal(response.statusCode, 405);
    }
  });

  await test('the CORS preflight allows the Authorization header browsers must send', async () => {
    const response = await callAdapter('OPTIONS', undefined, { origin: 'https://meet.petal.live' });
    assert.equal(response.statusCode, 204);
    assert.equal(
      response.headers['access-control-allow-headers'],
      'Content-Type, Authorization',
      'without Authorization in the preflight, every cross-origin ai-token call fails'
    );
    assert.equal(response.headers['access-control-allow-origin'], 'https://meet.petal.live');
  });

  await test('a disallowed browser origin is rejected before the handler runs', async () => {
    const response = await callAdapter(
      'POST',
      { room: CREDENTIAL, identity: ALICE_ID },
      { origin: 'https://evil.example' }
    );
    assert.equal(response.statusCode, 403);
    assert.deepEqual(response.body, { error: 'origin not allowed' });
  });

  await test('an unauthenticated POST through the adapter is 401', async () => {
    const response = await callAdapter('POST', { room: CREDENTIAL, identity: ALICE_ID });
    assert.equal(response.statusCode, 401);
    assert.deepEqual(response.body, { error: 'livekit authorization is required' });
  });

  await test('a malformed JSON string body is 400 through the adapter', async () => {
    const response = await callAdapter('POST', '{not json', { authorization: 'Bearer x' });
    assert.equal(response.statusCode, 400);
    assert.deepEqual(response.body, { error: 'invalid JSON body' });
  });

  console.log('');
  console.log('privacy (#128/#218): no room, identity, key, or token material in logs:');

  await test('a successful mint logs nothing at all', async () => {
    const messages = await captureConsole(async () => {
      const response = await handleAiToken(
        { room: CREDENTIAL, identity: ALICE_ID },
        await baseContext()
      );
      assert.equal(response.token, MINTED_TOKEN);
    });
    assert.deepEqual(messages, []);
  });

  await test('every rejection path logs no room, identity, key, or token material', async () => {
    const forbidden = [CREDENTIAL, ROOM, ALICE_ID, BOB_ID, GEMINI_KEY, API_SECRET, MINTED_TOKEN];
    const messages = await captureConsole(async () => {
      // 401 (no auth), 403 (wrong identity), 403 (not connected), 400 (bad
      // JSON), 503 (kill switch) — all through the real adapter + sendApiError.
      await callAdapter('POST', { room: CREDENTIAL, identity: ALICE_ID });
      await callAdapter(
        'POST',
        { room: CREDENTIAL, identity: ALICE_ID },
        { authorization: `Bearer ${await livekitJwt({ identity: BOB_ID })}` }
      );
      await callAdapter(
        'POST',
        { room: CREDENTIAL, identity: ALICE_ID },
        { authorization: `Bearer ${await livekitJwt()}` }
      );
      await callAdapter('POST', `{"room":"${CREDENTIAL}",`, {
        authorization: `Bearer ${await livekitJwt()}`,
      });
      await withoutGeminiKey(async () => {
        await callAdapter('POST', { room: CREDENTIAL, identity: ALICE_ID });
      });
    });
    const serialized = messages.join('\n');
    for (const value of forbidden) {
      assert.ok(
        !serialized.includes(value),
        `log output must never contain ${JSON.stringify(value)}; got:\n${serialized}`
      );
    }
  });

  await test('an upstream failure logs the sanitized message only, never the upstream body', async () => {
    const upstreamBody = `{"error":"room ${ROOM} identity ${ALICE_ID} key ${GEMINI_KEY}"}`;
    let thrown: unknown;
    const messages = await captureConsole(async () => {
      try {
        await handleAiToken(
          { room: CREDENTIAL, identity: ALICE_ID },
          await baseContext({
            mintEphemeralToken: () =>
              Promise.reject(Object.assign(new Error(upstreamBody), { status: 500 })),
          })
        );
      } catch (err) {
        thrown = err;
      }
      // Route the handler's error through the real error responder, which is
      // where a 5xx actually gets logged in production.
      const { sendApiError } = await import('../lib/http.js');
      const response = res();
      await sendApiError(response as unknown as VercelResponse, thrown, {
        operation: '/api/ai-token POST',
        fallbackStatus: 502,
        fallbackMessage: 'ai token service unavailable',
      });
      assert.equal(response.statusCode, 502);
      assert.deepEqual(response.body, { error: 'ai token service unavailable (upstream 500)' });
    });
    const serialized = messages.join('\n');
    for (const value of [ROOM, CREDENTIAL, ALICE_ID, GEMINI_KEY, upstreamBody]) {
      assert.ok(!serialized.includes(value), `log output leaked ${JSON.stringify(value)}`);
    }
  });

  await test('bucket keys never surface: a 429 body names no room or identity', async () => {
    const context = await baseContext({ rateLimitKey: '198.51.100.77' });
    for (let i = 0; i < AI_TOKEN_IDENTITY_BUCKET_CAPACITY; i++) {
      await handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context);
    }
    await assert.rejects(
      async () => handleAiToken({ room: CREDENTIAL, identity: ALICE_ID }, context),
      (err) => {
        assert.ok(err instanceof HttpError);
        assert.equal(err.status, 429);
        assert.ok(!err.message.includes(ALICE_ID));
        assert.ok(!err.message.includes(CREDENTIAL));
        return true;
      }
    );
  });

  await test('no committed source file carries a Gemini key or a hardcoded model id', async () => {
    const { readFileSync, readdirSync } = await import('node:fs');
    for (const root of ['../lib/', '../api/']) {
      const dir = new URL(root, import.meta.url);
      for (const entry of readdirSync(dir)) {
        if (!entry.endsWith('.ts')) continue;
        const source = readFileSync(new URL(entry, dir), 'utf8');
        assert.ok(!/AIza[0-9A-Za-z_-]{10,}/.test(source), `${entry} looks like it embeds an API key`);
        if (entry !== 'gemini.ts') {
          assert.ok(
            !/gemini-\d/.test(source),
            `${entry} hardcodes a Gemini model id; it belongs in lib/gemini.ts's env-driven default`
          );
        }
      }
    }
  });

  restoreEnv();
  console.log('');
  if (failures === 0) {
    console.log('ALL PASSED');
  } else {
    console.error(`${failures} CHECK(S) FAILED`);
    process.exit(1);
  }
}

main().catch((err) => {
  restoreEnv();
  console.error(err);
  process.exit(1);
});
