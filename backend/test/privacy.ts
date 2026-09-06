import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import type { VercelRequest, VercelResponse } from '../lib/vercel.js';
import { credentialForAccessCode, generateRoomCredential, livekitRoomName } from '../lib/slug.js';
import type { RoomDiscoveryService, RoomMetadataService } from '../lib/livekit.js';
import {
  handleAdminControl,
  handleCreateRoom,
  handleGalleryToken,
  handleListRooms,
  handleToken,
  HttpError,
  publicRoomIdForLiveKitRoom,
  resetTokenRateLimitsForTest,
  roomDiscoveryView,
  ROOM_CREATE_BUCKET_CAPACITY,
} from '../lib/handlers.js';
import adminHandler from '../api/admin.js';
import aiTokenHandler from '../api/ai-token.js';
import downloadHandler from '../api/download.js';
import galleryTokenHandler from '../api/gallery-token.js';
import roomsHandler from '../api/rooms.js';
import tokenHandler from '../api/token.js';
import updaterHandler from '../api/updater.js';
import { sendApiError } from '../lib/http.js';
import { _setSentryClientForTest } from '../lib/sentry.js';

const originalEnv = {
  LIVEKIT_URL: process.env.LIVEKIT_URL,
  LIVEKIT_API_KEY: process.env.LIVEKIT_API_KEY,
  LIVEKIT_API_SECRET: process.env.LIVEKIT_API_SECRET,
  PETAL_ADMIN_TOKEN: process.env.PETAL_ADMIN_TOKEN,
  GEMINI_API_KEY: process.env.GEMINI_API_KEY,
};

process.env.LIVEKIT_URL = 'ws://privacy-test.invalid';
process.env.LIVEKIT_API_KEY = 'privacy_test_key';
process.env.LIVEKIT_API_SECRET = 'privacy_test_secret';

const ALICE_ID = '11111111-1111-4111-8111-111111111111';
const WEB_ID = 'web-22222222-2222-4222-8222-222222222222';
const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
) as {
  roomMetadataRegistration: {
    request: { name: string; room: string; open: boolean };
    livekitRoom: string;
    metadata: string;
  };
};

function restoreEnv() {
  for (const [key, value] of Object.entries(originalEnv)) {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  }
}

function decodeJwtPayload(jwt: string): Record<string, any> {
  const [, payload] = jwt.split('.');
  assert.ok(payload, 'jwt payload segment is present');
  return JSON.parse(Buffer.from(payload, 'base64url').toString('utf-8'));
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

// Minimal Vercel req/res test doubles (mirrors test/distribution.ts) so the
// Sentry-capture tests below can drive the real api/*.ts adapters — the
// Sentry wiring lives in lib/http.ts's sendApiError, which is only reachable
// through those adapters, not through the bare lib/handlers.ts functions the
// rest of this file calls directly.
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

function reqPlain(method: string, headers: Record<string, string> = {}): VercelRequest {
  return { method, headers } as unknown as VercelRequest;
}

function reqWithBody(
  method: string,
  body: unknown,
  headers: Record<string, string> = {}
): VercelRequest {
  return { method, body, headers } as unknown as VercelRequest;
}

async function call(
  handler: (req: VercelRequest, res: VercelResponse) => Promise<void>,
  method = 'GET',
  headers: Record<string, string> = {}
): Promise<TestResponse> {
  const response = res();
  await handler(reqPlain(method, headers), response as unknown as VercelResponse);
  return response;
}

async function callWithBody(
  handler: (req: VercelRequest, res: VercelResponse) => Promise<void>,
  method: string,
  body: unknown,
  headers: Record<string, string> = {}
): Promise<TestResponse> {
  const response = res();
  await handler(reqWithBody(method, body, headers), response as unknown as VercelResponse);
  return response;
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
    if (saved.LIVEKIT_URL === undefined) delete process.env.LIVEKIT_URL;
    else process.env.LIVEKIT_URL = saved.LIVEKIT_URL;
    if (saved.LIVEKIT_API_KEY === undefined) delete process.env.LIVEKIT_API_KEY;
    else process.env.LIVEKIT_API_KEY = saved.LIVEKIT_API_KEY;
    if (saved.LIVEKIT_API_SECRET === undefined) delete process.env.LIVEKIT_API_SECRET;
    else process.env.LIVEKIT_API_SECRET = saved.LIVEKIT_API_SECRET;
  }
}

// #655: `undefined` is the AI-chat kill switch (GEMINI_API_KEY unset).
async function withGeminiKey(value: string | undefined, fn: () => Promise<void>) {
  const saved = process.env.GEMINI_API_KEY;
  if (value === undefined) delete process.env.GEMINI_API_KEY;
  else process.env.GEMINI_API_KEY = value;
  try {
    await fn();
  } finally {
    if (saved === undefined) delete process.env.GEMINI_API_KEY;
    else process.env.GEMINI_API_KEY = saved;
  }
}

async function withMissingBlobToken(fn: () => Promise<void>) {
  const saved = process.env.BLOB_READ_WRITE_TOKEN;
  delete process.env.BLOB_READ_WRITE_TOKEN;
  try {
    await fn();
  } finally {
    if (saved === undefined) delete process.env.BLOB_READ_WRITE_TOKEN;
    else process.env.BLOB_READ_WRITE_TOKEN = saved;
  }
}

interface SentryCapture {
  message: string;
  name: string;
  tags: Record<string, unknown>;
}

// Spies on the Sentry client the same way captureConsole() spies on console:
// swap in a mock, run fn(), record everything that would have been sent,
// then restore. dsn === undefined leaves SENTRY_DSN unset (the "fully off"
// case); any other string sets it for the duration of fn().
async function withSentry(
  dsn: string | undefined,
  fn: () => Promise<void> | void
): Promise<{ captures: SentryCapture[]; flushCalls: number[] }> {
  const captures: SentryCapture[] = [];
  const flushCalls: number[] = [];
  const savedDsn = process.env.SENTRY_DSN;
  if (dsn === undefined) delete process.env.SENTRY_DSN;
  else process.env.SENTRY_DSN = dsn;
  _setSentryClientForTest({
    init() {
      // no-op: a real SDK init would open network resources we don't want in
      // a unit test; we only care about what gets handed to captureException.
    },
    captureException(error: unknown, opts?: { tags?: Record<string, unknown> }) {
      const err = error as Error;
      captures.push({ message: err.message, name: err.name, tags: opts?.tags ?? {} });
      return 'mock-event-id';
    },
    async flush(timeoutMs: number) {
      flushCalls.push(timeoutMs);
      return true;
    },
  } as unknown as Parameters<typeof _setSentryClientForTest>[0]);
  try {
    await fn();
  } finally {
    _setSentryClientForTest(undefined);
    if (savedDsn === undefined) delete process.env.SENTRY_DSN;
    else process.env.SENTRY_DSN = savedDsn;
  }
  return { captures, flushCalls };
}

// Asserts none of the given secret/PII values appear anywhere in the
// serialized capture (message + tags) — the allowlist-first backstop.
function assertNoLeakedValues(captures: SentryCapture[], forbidden: string[]) {
  const serialized = JSON.stringify(captures);
  for (const value of forbidden) {
    assert.ok(
      !serialized.includes(value),
      `expected Sentry capture to never contain ${JSON.stringify(value)}, got ${serialized}`
    );
  }
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
  console.log('token privacy invariants:');

  await test('/api/token rejects bare guessable room labels', async () => {
    await assert.rejects(
      () => handleToken({ room: 'eng-sync', identity: 'mallory' }),
      (err) => err instanceof HttpError && err.status === 400
    );
  });

  await test('/api/token rejects oversized room, identity, and displayName fields', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aab')!;
    const tooLong = 'x'.repeat(129);
    await assert.rejects(
      () => handleToken({ room: `${tooLong}-${'a'.repeat(32)}`, identity: 'alice' }),
      (err) => err instanceof HttpError && err.status === 400
    );
    await assert.rejects(
      () => handleToken({ room: credential, identity: tooLong }),
      (err) => err instanceof HttpError && err.status === 400
    );
    await assert.rejects(
      () => handleToken({ room: credential, identity: 'alice', displayName: tooLong }),
      (err) => err instanceof HttpError && err.status === 400
    );
  });

  await test('/api/token rejects human-readable or spoofable identities', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aac')!;
    for (const identity of ['alice', 'Jane Doe', 'web-Jane Doe']) {
      await assert.rejects(
        () => handleToken({ room: credential, identity, displayName: 'Attacker' }),
        (err) => err instanceof HttpError && err.status === 400
      );
    }
  });

  await test('/api/token accepts generated participant identities used by clients', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aad')!;
    const native = await handleToken({ room: credential, identity: ALICE_ID, displayName: 'Alice' });
    const web = await handleToken({ room: credential, identity: WEB_ID, displayName: 'Web Alice' });
    assert.equal(decodeJwtPayload(native.token).sub, ALICE_ID);
    assert.equal(decodeJwtPayload(web.token).sub, WEB_ID);
  });

  await test('/api/token response exposes only connection material', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aae')!;
    const response = await handleToken({
      room: credential,
      identity: ALICE_ID,
      displayName: 'Alice Private',
    });

    assert.deepEqual(Object.keys(response).sort(), ['room', 'token', 'url']);
    assert.equal(response.room, livekitRoomName(credential));
    assert.equal(response.url, 'ws://privacy-test.invalid');
    assert.equal(typeof response.token, 'string');
    assert.ok(!JSON.stringify(response).includes('Alice Private'));
    assert.ok(!JSON.stringify(response).includes(ALICE_ID));
  });

  await test('/api/token response includes the LiveKit room display name', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aei')!;
    const livekitRoom = livekitRoomName(credential);
    const response = await handleToken(
      { room: credential, identity: ALICE_ID, displayName: 'Alice' },
      {
        service: {
          async listRooms() {
            return [{
              name: livekitRoom,
              metadata: JSON.stringify({ displayName: 'Eng meeting', open: true }),
              numParticipants: 1,
            }];
          },
          async createRoom() { throw new Error('not used'); },
          async updateRoomMetadata() { throw new Error('not used'); },
        } as unknown as RoomMetadataService,
      }
    );

    assert.equal(response.displayName, 'Eng meeting');
  });

  await test('/api/token mints scoped JWTs without logging room or identity', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aaf')!;
    let token = '';
    const messages = await captureConsole(async () => {
      const response = await handleToken({
        room: credential,
        identity: ALICE_ID,
        displayName: 'No Log Name',
      });
      token = response.token;
    });

    assert.deepEqual(messages, []);
    const claims = decodeJwtPayload(token);
    assert.equal(claims.sub, ALICE_ID);
    assert.equal(claims.name, 'No Log Name');
    assert.equal(claims.video?.room, livekitRoomName(credential));
    assert.equal(claims.video?.roomJoin, true);
    assert.equal(claims.video?.canPublishData, true);
    assert.ok(
      claims.exp - claims.nbf >= 23 * 60 * 60 && claims.exp - claims.nbf <= 25 * 60 * 60,
      `expected an approximately 24h token ttl, got ${claims.exp - claims.nbf}s`
    );
  });

  await test('/api/token clamps caller-controlled hidden and grant fields', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aag')!;
    const response = await handleToken({
      room: credential,
      identity: ALICE_ID,
      displayName: 'Grant Clamp',
      canPublish: false,
      canSubscribe: false,
      canPublishData: false,
      hidden: true,
    });

    const claims = decodeJwtPayload(response.token);
    assert.equal(claims.video?.hidden, false);
    assert.equal(claims.video?.canPublish, true);
    assert.equal(claims.video?.canSubscribe, true);
    assert.equal(claims.video?.canPublishData, true);
    assert.equal(claims.video?.roomJoin, true);
  });

  await test('/api/token rate limits by source without logging room or identity', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aah')!;
    const messages = await captureConsole(async () => {
      for (let i = 0; i < 20; i++) {
        await handleToken(
          {
            room: credential,
            identity: `web-33333333-3333-4333-8333-${i.toString().padStart(12, '0')}`,
            displayName: 'Rate Limited',
          },
          { rateLimitKey: '203.0.113.10', nowMs: 1_000 }
        );
      }
      await assert.rejects(
        () =>
          handleToken(
            {
              room: credential,
              identity: 'web-44444444-4444-4444-8444-444444444444',
              displayName: 'Rate Limited',
            },
            { rateLimitKey: '203.0.113.10', nowMs: 1_000 }
          ),
        (err) => err instanceof HttpError && err.status === 429
      );
    });

    assert.deepEqual(messages, []);
  });

  await test('/api/rooms discovery view exposes no join credential or participant identity', () => {
    const credential = credentialForAccessCode('aaa-aaaa-aai')!;
    const livekitRoom = livekitRoomName(credential);
    const view = roomDiscoveryView(
      livekitRoom,
      JSON.stringify({ displayName: 'Private Standup', open: true }),
      2
    );

    assert.deepEqual(Object.keys(view).sort(), ['id', 'name', 'occupancy', 'open']);
    assert.equal(view.id, publicRoomIdForLiveKitRoom(livekitRoom));
    assert.equal(view.name, 'Private Standup');
    assert.equal(view.open, true);
    assert.equal(view.occupancy, 2);
    assert.ok(!JSON.stringify(view).includes(credential));
    assert.ok(!JSON.stringify(view).includes(livekitRoom));
    assert.ok(!JSON.stringify(view).includes(ALICE_ID));
  });

  await test('/api/rooms create credential generation is access-code backed and non-throwing', () => {
    const credential = generateRoomCredential('Design Review');
    assert.match(credential, /^room-[0-9a-f]{32}$/);
    assert.equal(livekitRoomName(credential), `petal-room-${credential}`);
  });

  await test('/api/rooms rejects oversized names and accepts the 128-character boundary', async () => {
    const maxName = 'x'.repeat(128);
    const tooLong = 'x'.repeat(129);
    const service = {
      async createRoom(request: { name: string; metadata?: string }) {
        return { name: request.name, metadata: request.metadata ?? '', numParticipants: 0 };
      },
      async updateRoomMetadata(room: string, metadata: string) {
        return { name: room, metadata, numParticipants: 0 };
      },
    } as unknown as RoomMetadataService;

    const accepted = await handleCreateRoom({ name: maxName }, { service });
    assert.equal(accepted.room.name, maxName);

    await assert.rejects(
      () => handleCreateRoom({ name: tooLong }, { service }),
      (err) =>
        err instanceof HttpError &&
        err.status === 400 &&
        err.message === 'name must be 128 characters or fewer'
    );
  });

  await test('/api/rooms can stamp metadata onto an existing native credential', async () => {
    const { request, livekitRoom, metadata } = contractFixture.roomMetadataRegistration;
    const calls: Array<{ kind: string; room: string; metadata: string }> = [];
    const service = {
      async createRoom(request: { name: string; metadata?: string }) {
        calls.push({ kind: 'create', room: request.name, metadata: request.metadata ?? '' });
        return { name: request.name, metadata: '' };
      },
      async updateRoomMetadata(room: string, metadata: string) {
        calls.push({ kind: 'update', room, metadata });
        return { name: room, metadata, numParticipants: 0 };
      },
    } as unknown as RoomMetadataService;

    const response = await handleCreateRoom(request, { service });

    assert.equal(response.room.slug, request.room);
    assert.equal(response.room.livekitRoom, livekitRoom);
    assert.equal(response.room.name, request.name);
    assert.equal(response.room.open, request.open);
    assert.deepEqual(
      calls.map((call) => ({
        kind: call.kind,
        room: call.room,
        metadata: call.metadata,
      })),
      [
        { kind: 'create', room: livekitRoom, metadata },
        { kind: 'update', room: livekitRoom, metadata },
      ]
    );
  });

  await test('/api/rooms native rejoin preserves existing knock gate metadata', async () => {
    const { request, livekitRoom } = contractFixture.roomMetadataRegistration;
    let roomExists = false;
    let serverMetadata = '';
    const calls: Array<{ kind: string; room: string; metadata?: string }> = [];
    const service = {
      async createRoom(createRequest: { name: string; metadata?: string }) {
        calls.push({
          kind: 'create',
          room: createRequest.name,
          metadata: createRequest.metadata,
        });
        if (roomExists) {
          throw new Error('room already exists');
        }
        roomExists = true;
        serverMetadata = createRequest.metadata ?? '';
        return { name: createRequest.name, metadata: serverMetadata, numParticipants: 0 };
      },
      async listRooms() {
        calls.push({ kind: 'list', room: livekitRoom });
        return [{ name: livekitRoom, metadata: serverMetadata, numParticipants: 1 }];
      },
      async updateRoomMetadata(room: string, metadata: string) {
        calls.push({ kind: 'update', room, metadata });
        serverMetadata = metadata;
        return { name: room, metadata, numParticipants: 1 };
      },
    } as unknown as RoomMetadataService;

    const created = await handleCreateRoom(
      { name: request.name, room: request.room, open: false },
      { service }
    );
    const response = await handleCreateRoom(
      { name: 'Visitor local label', room: request.room, open: true },
      { service }
    );

    assert.equal(created.room.slug, request.room);
    assert.equal(created.room.livekitRoom, livekitRoom);
    assert.equal(created.room.open, false);
    assert.equal(response.room.slug, request.room);
    assert.equal(response.room.livekitRoom, livekitRoom);
    assert.equal(response.room.open, false);
    assert.deepEqual(JSON.parse(serverMetadata), {
      displayName: 'Visitor local label',
      open: false,
    });
    assert.deepEqual(
      calls.map((call) => ({
        kind: call.kind,
        room: call.room,
        metadata: call.metadata,
      })),
      [
        {
          kind: 'create',
          room: livekitRoom,
          metadata: JSON.stringify({ displayName: request.name, open: false }),
        },
        {
          kind: 'create',
          room: livekitRoom,
          metadata: JSON.stringify({ displayName: 'Visitor local label', open: true }),
        },
        { kind: 'list', room: livekitRoom, metadata: undefined },
        {
          kind: 'update',
          room: livekitRoom,
          metadata: JSON.stringify({ displayName: 'Visitor local label', open: false }),
        },
      ]
    );
  });

  await test('/api/rooms existing native metadata without open uses request fallback', async () => {
    const { request, livekitRoom } = contractFixture.roomMetadataRegistration;
    let serverMetadata = JSON.stringify({ displayName: request.name });
    const calls: Array<{ kind: string; room: string; metadata?: string }> = [];
    const service = {
      async createRoom(createRequest: { name: string; metadata?: string }) {
        calls.push({
          kind: 'create',
          room: createRequest.name,
          metadata: createRequest.metadata,
        });
        throw new Error('room already exists');
      },
      async listRooms() {
        calls.push({ kind: 'list', room: livekitRoom });
        return [{ name: livekitRoom, metadata: serverMetadata, numParticipants: 1 }];
      },
      async updateRoomMetadata(room: string, metadata: string) {
        calls.push({ kind: 'update', room, metadata });
        serverMetadata = metadata;
        return { name: room, metadata, numParticipants: 1 };
      },
    } as unknown as RoomMetadataService;

    const response = await handleCreateRoom(
      { name: 'Visitor local label', room: request.room, open: true },
      { service }
    );

    assert.equal(response.room.slug, request.room);
    assert.equal(response.room.livekitRoom, livekitRoom);
    assert.equal(response.room.open, true);
    assert.deepEqual(JSON.parse(serverMetadata), {
      displayName: 'Visitor local label',
      open: true,
    });
    assert.deepEqual(
      calls.map((call) => ({
        kind: call.kind,
        room: call.room,
        metadata: call.metadata,
      })),
      [
        {
          kind: 'create',
          room: livekitRoom,
          metadata: JSON.stringify({ displayName: 'Visitor local label', open: true }),
        },
        { kind: 'list', room: livekitRoom, metadata: undefined },
        {
          kind: 'update',
          room: livekitRoom,
          metadata: JSON.stringify({ displayName: 'Visitor local label', open: true }),
        },
      ]
    );
  });

  await test('/api/rooms rejects invalid existing credential stamps', async () => {
    await assert.rejects(
      () => handleCreateRoom({ name: 'Eng meeting', room: 'eng-meeting' }),
      (err) => err instanceof HttpError && err.status === 400
    );
  });

  await test('/api/rooms rate limits repeated creates by source', async () => {
    let createCalls = 0;
    const service = {
      async createRoom(request: { name: string; metadata?: string }) {
        createCalls++;
        return { name: request.name, metadata: request.metadata ?? '', numParticipants: 0 };
      },
      async updateRoomMetadata(room: string, metadata: string) {
        return { name: room, metadata, numParticipants: 0 };
      },
    } as unknown as RoomMetadataService;

    for (let i = 0; i < ROOM_CREATE_BUCKET_CAPACITY; i++) {
      await handleCreateRoom(
        { name: `Rate Limited ${i}` },
        { service, rateLimitKey: '198.51.100.8', nowMs: 3_000 }
      );
    }

    await assert.rejects(
      () =>
        handleCreateRoom(
          { name: 'Rate Limited final' },
          { service, rateLimitKey: '198.51.100.8', nowMs: 3_000 }
        ),
      (err) => err instanceof HttpError && err.status === 429
    );
    assert.equal(createCalls, ROOM_CREATE_BUCKET_CAPACITY);
  });

  await test('/api/admin rejects missing or invalid admin authorization', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aaj')!;
    delete process.env.PETAL_ADMIN_TOKEN;
    await assert.rejects(
      () => handleAdminControl({ action: 'close', room: credential }),
      (err) => err instanceof HttpError && err.status === 503
    );
    process.env.PETAL_ADMIN_TOKEN = 'admin-secret';
    await assert.rejects(
      () => handleAdminControl({ action: 'close', room: credential }),
      (err) => err instanceof HttpError && err.status === 401
    );
    await assert.rejects(
      () => handleAdminControl({ action: 'close', room: credential }, { authorization: 'Bearer wrong' }),
      (err) => err instanceof HttpError && err.status === 403
    );
  });

  await test('/api/admin can kick a participant and close a room with an admin bearer', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aak')!;
    const calls: string[] = [];
    const service = {
      async listRooms() {
        return [] as never;
      },
      async updateRoomMetadata(room: string, metadata: string) {
        calls.push(`meta:${room}:${metadata}`);
        return { name: room, metadata, numParticipants: 0 } as never;
      },
      async removeParticipant(room: string, identity: string) {
        calls.push(`kick:${room}:${identity}`);
      },
      async deleteRoom(room: string) {
        calls.push(`close:${room}`);
      },
    };
    process.env.PETAL_ADMIN_TOKEN = 'admin-secret';
    await handleAdminControl(
      { action: 'kick', room: credential, identity: ALICE_ID },
      { authorization: 'Bearer admin-secret', service }
    );
    await handleAdminControl(
      { action: 'close', room: credential },
      { authorization: 'Bearer admin-secret', service }
    );
    assert.deepEqual(calls, [
      `kick:${livekitRoomName(credential)}:${ALICE_ID}`,
      `close:${livekitRoomName(credential)}`,
    ]);
  });

  await test('/api/rooms rate limits repeated discovery by source', async () => {
    const saved = {
      LIVEKIT_URL: process.env.LIVEKIT_URL,
      LIVEKIT_API_KEY: process.env.LIVEKIT_API_KEY,
      LIVEKIT_API_SECRET: process.env.LIVEKIT_API_SECRET,
    };
    delete process.env.LIVEKIT_URL;
    delete process.env.LIVEKIT_API_KEY;
    delete process.env.LIVEKIT_API_SECRET;
    try {
      for (let i = 0; i < 60; i++) {
        await assert.rejects(
          () => handleListRooms({ rateLimitKey: '198.51.100.7', nowMs: 2_000 }),
          /LiveKit not configured/
        );
      }
      await assert.rejects(
        () => handleListRooms({ rateLimitKey: '198.51.100.7', nowMs: 2_000 }),
        (err) => err instanceof HttpError && err.status === 429
      );
    } finally {
      if (saved.LIVEKIT_URL === undefined) delete process.env.LIVEKIT_URL;
      else process.env.LIVEKIT_URL = saved.LIVEKIT_URL;
      if (saved.LIVEKIT_API_KEY === undefined) delete process.env.LIVEKIT_API_KEY;
      else process.env.LIVEKIT_API_KEY = saved.LIVEKIT_API_KEY;
      if (saved.LIVEKIT_API_SECRET === undefined) delete process.env.LIVEKIT_API_SECRET;
      else process.env.LIVEKIT_API_SECRET = saved.LIVEKIT_API_SECRET;
    }
  });

  await test('/api/gallery-token mints a hidden subscribe-only token only for a real current participant (#109)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aal')!;
    const room = livekitRoomName(credential);
    const service = {
      async listParticipants(requestedRoom: string) {
        assert.equal(requestedRoom, room);
        return [{ identity: ALICE_ID }, { identity: WEB_ID }];
      },
    } as unknown as RoomDiscoveryService;
    const response = await handleGalleryToken(
      { room: credential, baseIdentity: ALICE_ID },
      { service }
    );
    assert.equal(response.room, room);
    const payload = decodeJwtPayload(response.token);
    assert.equal(payload.sub, `${ALICE_ID}-gallery`);
    assert.equal(payload.video.hidden, true);
    assert.equal(payload.video.canPublish, false);
    assert.equal(payload.video.canSubscribe, true);
    assert.equal(payload.video.canPublishData, false);
  });

  await test('/api/gallery-token rejects a baseIdentity that is not a current participant', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aam')!;
    const service = {
      async listParticipants() {
        return [{ identity: WEB_ID }]; // ALICE_ID never joined
      },
    } as unknown as RoomDiscoveryService;
    await assert.rejects(
      () => handleGalleryToken({ room: credential, baseIdentity: ALICE_ID }, { service }),
      (err) => err instanceof HttpError && err.status === 403
    );
  });

  await test('/api/gallery-token rejects a room LiveKit cannot resolve, and rejects an already-suffixed baseIdentity', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aan')!;
    const throwingService = {
      async listParticipants() {
        throw new Error('twirp: room not found');
      },
    } as unknown as RoomDiscoveryService;
    await assert.rejects(
      () => handleGalleryToken({ room: credential, baseIdentity: ALICE_ID }, { service: throwingService }),
      (err) => err instanceof HttpError && err.status === 403
    );
    await assert.rejects(
      () =>
        handleGalleryToken(
          { room: credential, baseIdentity: `${ALICE_ID}-gallery` },
          { service: throwingService }
        ),
      (err) => err instanceof HttpError && err.status === 400
    );
  });

  console.log('');
  console.log('sentry capture invariants (#282):');

  const PII_MARKER_DISPLAY_NAME = 'Detective Alice Wonderland — do not leak';
  const PII_MARKER_ROOM_NAME = 'Executive Boardroom — do not leak';

  await test('absent SENTRY_DSN means fully off: zero captures, zero flush calls, even on a 5xx', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aao')!;
    const { captures, flushCalls } = await withSentry(undefined, async () => {
      await withMissingLiveKitEnv(async () => {
        const response = await callWithBody(tokenHandler, 'POST', {
          room: credential,
          identity: ALICE_ID,
          displayName: PII_MARKER_DISPLAY_NAME,
        });
        assert.equal(response.statusCode, 503);
      });
    });
    assert.deepEqual(captures, []);
    assert.deepEqual(flushCalls, []);
  });

  await test('/api/token: 4xx never captures, 5xx captures allowlisted fields only (no PII)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aat')!;

    const fourXx = await withSentry('https://test@example.invalid/1', async () => {
      const response = await callWithBody(tokenHandler, 'POST', { room: 'eng-sync', identity: 'mallory' });
      assert.equal(response.statusCode, 400);
    });
    assert.deepEqual(fourXx.captures, []);

    const fiveXx = await withSentry('https://test@example.invalid/1', async () => {
      await withMissingLiveKitEnv(async () => {
        const response = await callWithBody(tokenHandler, 'POST', {
          room: credential,
          identity: ALICE_ID,
          displayName: PII_MARKER_DISPLAY_NAME,
        });
        assert.equal(response.statusCode, 503);
        assert.deepEqual(response.body, { error: 'LiveKit not configured' });
      });
    });
    assert.equal(fiveXx.captures.length, 1);
    assert.deepEqual(fiveXx.captures[0].tags, {
      operation: '/api/token POST',
      route: '/api/token POST',
      statusCode: 503,
      errorType: 'LiveKitConfigError',
    });
    assert.equal(fiveXx.captures[0].message, '/api/token POST');
    assertNoLeakedValues(fiveXx.captures, [ALICE_ID, credential, PII_MARKER_DISPLAY_NAME]);
    assert.deepEqual(fiveXx.flushCalls, [2000]);
  });

  await test('/api/rooms: 4xx never captures, 5xx captures allowlisted fields only (no PII)', async () => {
    const fourXx = await withSentry('https://test@example.invalid/1', async () => {
      const response = await callWithBody(roomsHandler, 'POST', { name: 'x'.repeat(129) });
      assert.equal(response.statusCode, 400);
    });
    assert.deepEqual(fourXx.captures, []);

    const fiveXx = await withSentry('https://test@example.invalid/1', async () => {
      await withMissingLiveKitEnv(async () => {
        const response = await callWithBody(roomsHandler, 'POST', { name: PII_MARKER_ROOM_NAME });
        assert.equal(response.statusCode, 503);
        assert.deepEqual(response.body, { error: 'LiveKit not configured' });
      });
    });
    assert.equal(fiveXx.captures.length, 1);
    assert.deepEqual(fiveXx.captures[0].tags, {
      operation: '/api/rooms POST',
      route: '/api/rooms POST',
      statusCode: 503,
      errorType: 'LiveKitConfigError',
    });
    assertNoLeakedValues(fiveXx.captures, [PII_MARKER_ROOM_NAME]);
    assert.deepEqual(fiveXx.flushCalls, [2000]);
  });

  await test('/api/admin: 4xx never captures, 5xx captures allowlisted fields only (no room credential leak)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aau')!;
    const savedToken = process.env.PETAL_ADMIN_TOKEN;
    try {
      // assertAdminAuthorization's "not configured" (PETAL_ADMIN_TOKEN unset)
      // throws HttpError(503, ...) — a genuine operator misconfiguration, not
      // a routine caller error, so sendApiError's classifier (5xx == capture)
      // reports it like any other 5xx. Fields sent stay allowlist-only.
      delete process.env.PETAL_ADMIN_TOKEN;
      const notConfigured = await withSentry('https://test@example.invalid/1', async () => {
        const response = await callWithBody(adminHandler, 'POST', { action: 'close', room: credential });
        assert.equal(response.statusCode, 503);
      });
      assert.equal(notConfigured.captures.length, 1);
      assert.deepEqual(notConfigured.captures[0].tags, {
        operation: '/api/admin POST',
        route: '/api/admin POST',
        statusCode: 503,
        errorType: 'HttpError',
      });
      assertNoLeakedValues(notConfigured.captures, [credential]);

      process.env.PETAL_ADMIN_TOKEN = 'admin-secret';
      const wrongAuth = await withSentry('https://test@example.invalid/1', async () => {
        const response = await callWithBody(
          adminHandler,
          'POST',
          { action: 'close', room: credential },
          { authorization: 'Bearer wrong' }
        );
        assert.equal(response.statusCode, 403);
      });
      assert.deepEqual(wrongAuth.captures, []); // true 4xx: no capture

      const fiveXx = await withSentry('https://test@example.invalid/1', async () => {
        await withMissingLiveKitEnv(async () => {
          const response = await callWithBody(
            adminHandler,
            'POST',
            { action: 'close', room: credential },
            { authorization: 'Bearer admin-secret' }
          );
          assert.equal(response.statusCode, 503);
        });
      });
      assert.equal(fiveXx.captures.length, 1);
      assert.deepEqual(fiveXx.captures[0].tags, {
        operation: '/api/admin POST',
        route: '/api/admin POST',
        statusCode: 503,
        errorType: 'LiveKitConfigError',
      });
      assertNoLeakedValues(fiveXx.captures, [credential]);
      assert.deepEqual(fiveXx.flushCalls, [2000]);
    } finally {
      if (savedToken === undefined) delete process.env.PETAL_ADMIN_TOKEN;
      else process.env.PETAL_ADMIN_TOKEN = savedToken;
    }
  });

  await test('/api/gallery-token: 4xx never captures, 5xx captures allowlisted fields only (no PII)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aav')!;

    const fourXx = await withSentry('https://test@example.invalid/1', async () => {
      const response = await callWithBody(galleryTokenHandler, 'POST', { room: credential });
      assert.equal(response.statusCode, 400);
    });
    assert.deepEqual(fourXx.captures, []);

    const fiveXx = await withSentry('https://test@example.invalid/1', async () => {
      await withMissingLiveKitEnv(async () => {
        const response = await callWithBody(galleryTokenHandler, 'POST', {
          room: credential,
          baseIdentity: ALICE_ID,
          displayName: PII_MARKER_DISPLAY_NAME,
        });
        assert.equal(response.statusCode, 503);
      });
    });
    assert.equal(fiveXx.captures.length, 1);
    assert.deepEqual(fiveXx.captures[0].tags, {
      operation: '/api/gallery-token POST',
      route: '/api/gallery-token POST',
      statusCode: 503,
      errorType: 'LiveKitConfigError',
    });
    assertNoLeakedValues(fiveXx.captures, [ALICE_ID, credential, PII_MARKER_DISPLAY_NAME]);
    assert.deepEqual(fiveXx.flushCalls, [2000]);
  });

  await test('/api/ai-token: 4xx never captures, and the kill-switch 503 captures allowlisted fields only (no PII, no key)', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aaw')!;
    const GEMINI_KEY_MARKER = 'AIzaPRIVACYTESTdonotleak';

    // Configured but unauthenticated: a routine 401 must not burn Sentry quota.
    const fourXx = await withSentry('https://test@example.invalid/1', async () => {
      await withGeminiKey(GEMINI_KEY_MARKER, async () => {
        const response = await callWithBody(aiTokenHandler, 'POST', {
          room: credential,
          identity: ALICE_ID,
        });
        assert.equal(response.statusCode, 401);
      });
    });
    assert.deepEqual(fourXx.captures, []);

    // GEMINI_API_KEY unset is the documented global kill switch (#655): a
    // specific 503 clients can render as "AI chat temporarily unavailable".
    const fiveXx = await withSentry('https://test@example.invalid/1', async () => {
      await withGeminiKey(undefined, async () => {
        const response = await callWithBody(aiTokenHandler, 'POST', {
          room: credential,
          identity: ALICE_ID,
        });
        assert.equal(response.statusCode, 503);
        assert.deepEqual(response.body, { error: 'AI chat is not configured' });
      });
    });
    assert.equal(fiveXx.captures.length, 1);
    assert.deepEqual(fiveXx.captures[0].tags, {
      operation: '/api/ai-token POST',
      route: '/api/ai-token POST',
      statusCode: 503,
      errorType: 'GeminiConfigError',
    });
    assertNoLeakedValues(fiveXx.captures, [ALICE_ID, credential, GEMINI_KEY_MARKER]);
    assert.deepEqual(fiveXx.flushCalls, [2000]);
  });

  await test('/api/ai-token rejections log no room, identity, or key material', async () => {
    const credential = credentialForAccessCode('aaa-aaaa-aax')!;
    const GEMINI_KEY_MARKER = 'AIzaPRIVACYTESTdonotleak2';
    const messages = await captureConsole(async () => {
      await withGeminiKey(GEMINI_KEY_MARKER, async () => {
        // 401 unauthenticated, 400 malformed body, 400 spoofable identity.
        assert.equal(
          (await callWithBody(aiTokenHandler, 'POST', { room: credential, identity: ALICE_ID }))
            .statusCode,
          401
        );
        assert.equal((await callWithBody(aiTokenHandler, 'POST', {})).statusCode, 400);
        assert.equal(
          (await callWithBody(aiTokenHandler, 'POST', { room: credential, identity: 'Jane Doe' }))
            .statusCode,
          400
        );
      });
      await withGeminiKey(undefined, async () => {
        assert.equal(
          (await callWithBody(aiTokenHandler, 'POST', { room: credential, identity: ALICE_ID }))
            .statusCode,
          503
        );
      });
    });
    const serialized = messages.join('\n');
    for (const value of [credential, ALICE_ID, GEMINI_KEY_MARKER, 'Jane Doe']) {
      assert.ok(!serialized.includes(value), `ai-token logs leaked ${JSON.stringify(value)}`);
    }
  });

  await test('/api/download: unexpected failure captures the sanitized fallback only', async () => {
    const { captures, flushCalls } = await withSentry('https://test@example.invalid/1', async () => {
      await withMissingBlobToken(async () => {
        const response = await call(downloadHandler);
        assert.equal(response.statusCode, 502);
      });
    });
    assert.equal(captures.length, 1);
    assert.deepEqual(captures[0].tags, {
      operation: '/api/download GET',
      route: '/api/download GET',
      statusCode: 502,
      errorType: 'Error',
    });
    assert.equal(captures[0].message, '/api/download GET');
    assertNoLeakedValues(captures, ['BLOB_READ_WRITE_TOKEN']);
    assert.deepEqual(flushCalls, [2000]);
  });

  await test('/api/updater: masked failure (still 204) still reports to Sentry alongside the existing warn (#177 response contract unchanged)', async () => {
    const messages = await captureConsole(async () => {
      const { captures, flushCalls } = await withSentry('https://test@example.invalid/1', async () => {
        await withMissingBlobToken(async () => {
          const response = await call(updaterHandler);
          assert.equal(response.statusCode, 204); // #177 contract: unchanged
        });
      });
      assert.equal(captures.length, 1);
      assert.deepEqual(captures[0].tags, {
        operation: '/api/updater GET',
        route: '/api/updater GET',
        statusCode: 204,
        errorType: 'Error',
      });
      assertNoLeakedValues(captures, ['BLOB_READ_WRITE_TOKEN']);
      assert.deepEqual(flushCalls, [2000]);
    });
    assert.ok(messages.some((m) => m.startsWith('warn: updater: latest.json unavailable')));
  });

  await test('sendApiError genuinely awaits Sentry.flush before returning — not fire-and-forget', async () => {
    let flushResolvedBeforeReturn = false;
    const savedDsn = process.env.SENTRY_DSN;
    process.env.SENTRY_DSN = 'https://test@example.invalid/1';
    _setSentryClientForTest({
      init() {},
      captureException() {
        return 'mock-event-id';
      },
      async flush() {
        // Simulate a real network flush that takes a tick — if sendApiError
        // failed to await this, the assertion below would observe `false`.
        await new Promise((resolve) => setTimeout(resolve, 10));
        flushResolvedBeforeReturn = true;
        return true;
      },
    } as unknown as Parameters<typeof _setSentryClientForTest>[0]);
    try {
      const response = res();
      await sendApiError(response as unknown as VercelResponse, new Error('boom'), {
        operation: '/api/test POST',
        fallbackStatus: 502,
        fallbackMessage: 'test unavailable',
      });
      assert.equal(flushResolvedBeforeReturn, true, 'sendApiError returned before its flush() promise settled');
    } finally {
      _setSentryClientForTest(undefined);
      if (savedDsn === undefined) delete process.env.SENTRY_DSN;
      else process.env.SENTRY_DSN = savedDsn;
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
