// Backend abuse hardening. Everything here runs
// against injected service mocks -- no live livekit-server -- and covers:
//
//   1. rate-limit stores evict by TTL and enforce a hard key cap
//   2. room status lookup is one listRooms RPC, cached per instance, and
//      returns only rooms the caller proves possession of
//   3. /api/token refuses a closed (`open: false`) room without its access code
//   4. room creation has its own small per-source bucket + a global ceiling
//   5. a kick is recorded in room metadata, /api/token refuses the identity,
//      and a later native re-stamp of that room preserves the record

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { MemoryRateLimitStore } from '../lib/ratelimit.js';
import {
  decodeRoomMeta,
  encodeRoomMeta,
  ensureRoom,
  ROOM_META_REMOVED_LIMIT,
  type RoomAdminService,
  type RoomListingService,
  type RoomMetadataService,
} from '../lib/livekit.js';
import {
  handleAdminControl,
  handleCreateRoom,
  handleListRooms,
  handleRoomStatus,
  handleToken,
  HttpError,
  rateLimitStoreSizesForTest,
  resetTokenRateLimitsForTest,
  ROOM_CREATE_BUCKET_CAPACITY,
  ROOM_CREATE_BUCKET_REFILL_MS,
  ROOM_CREATE_GLOBAL_CAPACITY,
  ROOMS_LIST_CACHE_MS,
  ROOM_STATUS_MAX_ROOMS,
} from '../lib/handlers.js';
import { credentialForAccessCode, livekitRoomName } from '../lib/slug.js';

process.env.LIVEKIT_URL = 'ws://hardening-test.invalid';
process.env.LIVEKIT_API_KEY = 'hardening_test_key';
process.env.LIVEKIT_API_SECRET = 'hardening_test_secret_at_least_32_chars_long';
process.env.PETAL_ADMIN_TOKEN = 'hardening_admin_token';

const ADMIN = { authorization: `Bearer ${process.env.PETAL_ADMIN_TOKEN}` };
const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = credentialForAccessCode(ACCESS_CODE)!;
const LIVEKIT_ROOM = livekitRoomName(CREDENTIAL);
const ALICE = 'web-11111111-1111-4111-8111-111111111111';
const BOB = 'web-22222222-2222-4222-8222-222222222222';

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

// A room store mock that behaves like one LiveKit room's metadata: every
// service type in lib/livekit.ts is satisfied by the same object so one mock
// can flow through create -> kick -> token.
function roomMock(initial?: { metadata: string; numParticipants?: number }) {
  const state = { exists: !!initial, metadata: initial?.metadata ?? '', numParticipants: initial?.numParticipants ?? 0 };
  const calls: string[] = [];
  const service = {
    async createRoom(request: { name: string; metadata?: string }) {
      calls.push('createRoom');
      if (state.exists) throw new Error('room already exists');
      state.exists = true;
      state.metadata = request.metadata ?? '';
      return { name: request.name, metadata: state.metadata, numParticipants: 0 } as never;
    },
    async listRooms() {
      calls.push('listRooms');
      return state.exists
        ? ([{ name: LIVEKIT_ROOM, metadata: state.metadata, numParticipants: state.numParticipants }] as never)
        : ([] as never);
    },
    async updateRoomMetadata(room: string, metadata: string) {
      calls.push('updateRoomMetadata');
      state.metadata = metadata;
      return { name: room, metadata, numParticipants: state.numParticipants } as never;
    },
    async removeParticipant(_room: string, identity: string) {
      calls.push(`removeParticipant:${identity}`);
    },
    async deleteRoom() {
      calls.push('deleteRoom');
    },
    async listParticipants() {
      calls.push('listParticipants');
      return [] as never;
    },
  };
  return { service: service as RoomMetadataService & RoomAdminService & RoomListingService, state, calls };
}

async function main() {
  console.log('1. rate-limit store bounds (TTL + cap):');

  await test('entries older than the TTL are swept; fresh ones survive', () => {
    const store = new MemoryRateLimitStore({ ttlMs: 1_000, sweepEvery: 1_000_000 });
    store.set('old', { tokens: 1, updatedAt: 0 }, 0);
    store.set('fresh', { tokens: 1, updatedAt: 900 }, 900);
    assert.equal(store.size, 2);
    store.sweep(1_000);
    assert.equal(store.size, 1);
    assert.ok(store.get('fresh'));
    assert.equal(store.get('old'), undefined);
  });

  await test('the key cap evicts least-recently-touched keys first, never the just-written one', () => {
    const store = new MemoryRateLimitStore({ ttlMs: 60_000, maxKeys: 3, sweepEvery: 1_000_000 });
    store.set('a', { tokens: 1, updatedAt: 1 }, 1);
    store.set('b', { tokens: 1, updatedAt: 2 }, 2);
    store.set('c', { tokens: 1, updatedAt: 3 }, 3);
    store.set('a', { tokens: 1, updatedAt: 4 }, 4); // touch a -> b is now the oldest
    store.set('d', { tokens: 1, updatedAt: 5 }, 5); // over cap -> immediate sweep
    assert.equal(store.size, 3);
    assert.equal(store.get('b'), undefined, 'b was least recently touched');
    assert.ok(store.get('a') && store.get('c') && store.get('d'));
  });

  await test('periodic sweep fires on the configured cadence without a cap breach', () => {
    const store = new MemoryRateLimitStore({ ttlMs: 10, sweepEvery: 4 });
    for (let i = 0; i < 3; i++) store.set(`k${i}`, { tokens: 1, updatedAt: 0 }, 0);
    assert.equal(store.size, 3, 'no sweep yet');
    store.set('late', { tokens: 1, updatedAt: 100 }, 100); // 4th set -> sweep at t=100
    assert.equal(store.size, 1, 'the three expired keys were swept on the 4th set');
    assert.ok(store.get('late'));
  });

  await test('the handlers are wired to the bounded stores (residency is reported, not a bare Map)', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'R', open: true }) });
    assert.equal(rateLimitStoreSizesForTest().token, 0);
    await handleToken({ room: CREDENTIAL, identity: ALICE }, { service, rateLimitKey: '203.0.113.1', nowMs: 1 });
    assert.equal(rateLimitStoreSizesForTest().token, 1);
    await handleToken({ room: CREDENTIAL, identity: ALICE }, { service, rateLimitKey: '203.0.113.2', nowMs: 2 });
    assert.equal(rateLimitStoreSizesForTest().token, 2);
    // Every store reports a number: none of the six limits fell back to an
    // unbounded map.
    for (const [name, size] of Object.entries(rateLimitStoreSizesForTest())) {
      assert.equal(typeof size, 'number', `${name} store is not a MemoryRateLimitStore`);
    }
  });

  console.log('');
  console.log('2. POST /api/rooms/status: proof-of-possession, one RPC, cached:');

  await test('repeated lookups inside the cache window cost one listRooms call', async () => {
    const { service, calls } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Cached', open: true }), numParticipants: 2 });
    const body = { rooms: [{ room: CREDENTIAL }] };
    const first = await handleRoomStatus(body, { service, nowMs: 10_000, rateLimitKey: 'a' });
    const second = await handleRoomStatus(body, { service, nowMs: 10_000 + ROOMS_LIST_CACHE_MS - 1, rateLimitKey: 'b' });
    assert.equal(calls.filter((c) => c === 'listRooms').length, 1);
    assert.deepEqual(second, first);
    assert.equal(first.rooms.length, 1);
    assert.equal(first.rooms[0]!.occupancy, 2, 'occupancy is numParticipants straight from listRooms');
    assert.deepEqual(Object.keys(first.rooms[0]!).sort(), ['id', 'name', 'occupancy', 'open']);
    assert.ok(!JSON.stringify(first).includes(CREDENTIAL), 'the view never echoes the credential');
    assert.ok(!calls.includes('listParticipants'), 'no per-room fan-out');
  });

  await test('the cache expires after ROOMS_LIST_CACHE_MS and refreshes from LiveKit', async () => {
    const { service, state, calls } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Cached', open: true }), numParticipants: 1 });
    const body = { rooms: [{ room: CREDENTIAL }] };
    await handleRoomStatus(body, { service, nowMs: 10_000 });
    state.numParticipants = 5;
    const stale = await handleRoomStatus(body, { service, nowMs: 10_000 + ROOMS_LIST_CACHE_MS - 1 });
    assert.equal(stale.rooms[0]!.occupancy, 1, 'still served from cache');
    const fresh = await handleRoomStatus(body, { service, nowMs: 10_000 + ROOMS_LIST_CACHE_MS });
    assert.equal(fresh.rooms[0]!.occupancy, 5, 'refreshed');
    assert.equal(calls.filter((c) => c === 'listRooms').length, 2);
  });

  await test('a credential the caller does not present is omitted (no enumeration)', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Secret', open: true }), numParticipants: 3 });
    const other = credentialForAccessCode('zzz-zzzz-zzz')!;
    const { rooms } = await handleRoomStatus({ rooms: [{ room: other }] }, { service, nowMs: 1 });
    assert.deepEqual(rooms, []);
    const empty = await handleRoomStatus({ rooms: [] }, { service, nowMs: 2 });
    assert.deepEqual(empty.rooms, []);
  });

  await test('an unknown or malformed credential is silently omitted, never 404d', async () => {
    const { service, calls } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Live', open: true }) });
    const { rooms } = await handleRoomStatus(
      { rooms: [{ room: 'room-00000000000000000000000000000000' }, { room: 'not-a-credential' }, { room: CREDENTIAL }] },
      { service, nowMs: 1 }
    );
    assert.equal(rooms.length, 1);
    assert.equal(rooms[0]!.name, 'Live');
    assert.equal(calls.filter((c) => c === 'listRooms').length, 1);
  });

  await test('a closed room is omitted without its access code and returned with it', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Closed', open: false }), numParticipants: 4 });
    const without = await handleRoomStatus({ rooms: [{ room: CREDENTIAL }] }, { service, nowMs: 1 });
    assert.deepEqual(without.rooms, [], 'credential alone does not reveal a closed room');
    const wrong = await handleRoomStatus({ rooms: [{ room: CREDENTIAL, accessCode: 'zzz-zzzz-zzz' }] }, { service, nowMs: 2 });
    assert.deepEqual(wrong.rooms, []);
    const right = await handleRoomStatus({ rooms: [{ room: CREDENTIAL, accessCode: ACCESS_CODE }] }, { service, nowMs: 3 });
    assert.equal(right.rooms.length, 1);
    assert.equal(right.rooms[0]!.open, false);
    assert.equal(right.rooms[0]!.occupancy, 4);
  });

  await test('the request is bounded: shape errors and the entry cap are 400', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Live', open: true }) });
    for (const bad of [undefined, null, {}, { rooms: 'x' }, { rooms: ['room-abc'] }, { rooms: [{}] }]) {
      await assert.rejects(
        () => handleRoomStatus(bad as never, { service, nowMs: 1 }),
        (err) => err instanceof HttpError && err.status === 400,
        `expected 400 for ${JSON.stringify(bad)}`
      );
    }
    const tooMany = { rooms: Array.from({ length: ROOM_STATUS_MAX_ROOMS + 1 }, () => ({ room: CREDENTIAL })) };
    await assert.rejects(
      () => handleRoomStatus(tooMany, { service, nowMs: 1 }),
      (err) => err instanceof HttpError && err.status === 400
    );
    const atCap = { rooms: Array.from({ length: ROOM_STATUS_MAX_ROOMS }, () => ({ room: CREDENTIAL })) };
    const { rooms } = await handleRoomStatus(atCap, { service, nowMs: 1 });
    assert.equal(rooms.length, 1, 'duplicates collapse to one view');
  });

  await test('the rate limit is still charged on cache hits (a cached 200 is not free for an abuser)', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Cached', open: true }) });
    const body = { rooms: [{ room: CREDENTIAL }] };
    for (let i = 0; i < 60; i++) await handleRoomStatus(body, { service, nowMs: 1_000, rateLimitKey: '198.51.100.9' });
    await assert.rejects(
      () => handleRoomStatus(body, { service, nowMs: 1_000, rateLimitKey: '198.51.100.9' }),
      (err) => err instanceof HttpError && err.status === 429
    );
  });

  await test('contract fixture: roomStatusRequest shape, cap and response keys are pinned', async () => {
    const contracts = JSON.parse(readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')) as {
      roomStatusRequest: {
        maxRooms: number;
        request: { rooms: { room: string; accessCode?: string }[] };
        responseKeys: string[];
        directoryGetStatus: number;
      };
    };
    const fixture = contracts.roomStatusRequest;
    assert.equal(fixture.maxRooms, ROOM_STATUS_MAX_ROOMS);
    assert.equal(fixture.directoryGetStatus, 410);
    assert.equal(fixture.request.rooms[0]!.room, CREDENTIAL, 'fixture shares the roomCredentials access code');
    assert.equal(credentialForAccessCode(fixture.request.rooms[0]!.accessCode!), CREDENTIAL);
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Closed', open: false }), numParticipants: 1 });
    const { rooms } = await handleRoomStatus(fixture.request, { service, nowMs: 1 });
    assert.equal(rooms.length, 1, 'the closed fixture room is returned with its code; the unknown one is omitted');
    assert.deepEqual(Object.keys(rooms[0]!).sort(), fixture.responseKeys);
  });

  await test('the full directory view still exists server-side (tooling) and shares the cache', async () => {
    const { service, calls } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Cached', open: true }), numParticipants: 2 });
    const listed = await handleListRooms({ service, nowMs: 10_000 });
    const status = await handleRoomStatus({ rooms: [{ room: CREDENTIAL }] }, { service, nowMs: 10_000 + 1 });
    assert.deepEqual(status.rooms, listed.rooms);
    assert.equal(calls.filter((c) => c === 'listRooms').length, 1);
  });

  console.log('3. /api/token enforces open:false:');

  await test('a closed room refuses a token request carrying only the credential', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Closed', open: false }) });
    await assert.rejects(
      () => handleToken({ room: CREDENTIAL, identity: ALICE }, { service, nowMs: 1 }),
      (err) => err instanceof HttpError && err.status === 403 && /closed/.test(err.message)
    );
  });

  await test('a closed room refuses a wrong access code, including one for a different room', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Closed', open: false }) });
    for (const accessCode of ['zzz-zzzz-zzz', 'not-a-code', '']) {
      await assert.rejects(
        () => handleToken({ room: CREDENTIAL, identity: ALICE, accessCode }, { service, nowMs: 1 }),
        (err) => err instanceof HttpError && err.status === 403
      );
    }
  });

  await test('a closed room mints when the access code hashes to the credential (any casing / hyphenation)', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Closed', open: false }) });
    for (const accessCode of [ACCESS_CODE, ACCESS_CODE.toUpperCase(), ACCESS_CODE.replace(/-/g, '')]) {
      const response = await handleToken({ room: CREDENTIAL, identity: ALICE, accessCode }, { service, nowMs: 1 });
      assert.equal(response.room, LIVEKIT_ROOM);
      assert.equal(response.displayName, 'Closed');
    }
  });

  await test('an open room ignores accessCode entirely (present, absent, or wrong)', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Open', open: true }) });
    for (const accessCode of [undefined, ACCESS_CODE, 'zzz-zzzz-zzz']) {
      const response = await handleToken({ room: CREDENTIAL, identity: ALICE, accessCode }, { service, nowMs: 1 });
      assert.equal(response.room, LIVEKIT_ROOM);
    }
  });

  await test('a room with no metadata (older client / not live on LiveKit) stays joinable by credential', async () => {
    const { service } = roomMock();
    const response = await handleToken({ room: CREDENTIAL, identity: ALICE }, { service, nowMs: 1 });
    assert.equal(response.room, LIVEKIT_ROOM);
    assert.equal(response.displayName, undefined);
  });

  console.log('');
  console.log('4. room creation has its own bucket + a global ceiling:');

  await test('per-source create cap is ROOM_CREATE_BUCKET_CAPACITY, separate from the 60/min discovery bucket', async () => {
    const { service } = roomMock();
    let created = 0;
    const freshService = () => {
      const m = roomMock();
      return m.service;
    };
    for (let i = 0; i < ROOM_CREATE_BUCKET_CAPACITY; i++) {
      await handleCreateRoom({ name: `Spam ${i}` }, { service: freshService(), rateLimitKey: '198.51.100.20', nowMs: 5_000 });
      created++;
    }
    assert.equal(created, ROOM_CREATE_BUCKET_CAPACITY);
    await assert.rejects(
      () => handleCreateRoom({ name: 'One more' }, { service, rateLimitKey: '198.51.100.20', nowMs: 5_000 }),
      (err) => err instanceof HttpError && err.status === 429
    );
    // Discovery from the same source is unaffected: different bucket.
    const listing = roomMock({ metadata: encodeRoomMeta({ displayName: 'X', open: true }) });
    await handleListRooms({ service: listing.service, rateLimitKey: '198.51.100.20', nowMs: 5_000 });
  });

  await test('the create bucket refills over ROOM_CREATE_BUCKET_REFILL_MS', async () => {
    for (let i = 0; i < ROOM_CREATE_BUCKET_CAPACITY; i++) {
      await handleCreateRoom({ name: `Spam ${i}` }, { service: roomMock().service, rateLimitKey: '198.51.100.21', nowMs: 0 });
    }
    await assert.rejects(
      () => handleCreateRoom({ name: 'Blocked' }, { service: roomMock().service, rateLimitKey: '198.51.100.21', nowMs: 0 }),
      (err) => err instanceof HttpError && err.status === 429
    );
    await handleCreateRoom(
      { name: 'Allowed again' },
      { service: roomMock().service, rateLimitKey: '198.51.100.21', nowMs: ROOM_CREATE_BUCKET_REFILL_MS }
    );
  });

  await test('rotating sources hits the instance-wide ceiling of ROOM_CREATE_GLOBAL_CAPACITY creates', async () => {
    for (let i = 0; i < ROOM_CREATE_GLOBAL_CAPACITY; i++) {
      await handleCreateRoom(
        { name: `Rotating ${i}` },
        { service: roomMock().service, rateLimitKey: `10.0.${Math.floor(i / 250)}.${i % 250}`, nowMs: 7_000 }
      );
    }
    await assert.rejects(
      () => handleCreateRoom({ name: 'Over the top' }, { service: roomMock().service, rateLimitKey: '10.9.9.9', nowMs: 7_000 }),
      (err) => err instanceof HttpError && err.status === 429 && /temporarily unavailable/.test(err.message)
    );
  });

  console.log('');
  console.log('5. kick sticks:');

  await test('kick writes the identity into room metadata BEFORE removing the participant', async () => {
    const { service, state, calls } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Team', open: true }) });
    await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    const meta = decodeRoomMeta(state.metadata);
    assert.deepEqual(meta.removed, [ALICE]);
    assert.equal(meta.displayName, 'Team');
    assert.equal(meta.open, true);
    assert.ok(
      calls.indexOf('updateRoomMetadata') < calls.indexOf(`removeParticipant:${ALICE}`),
      `metadata must be written before the disconnect, got ${calls.join(',')}`
    );
  });

  await test('/api/token refuses a removed identity; everyone else still mints', async () => {
    const { service } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Team', open: true }) });
    await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    await assert.rejects(
      () => handleToken({ room: CREDENTIAL, identity: ALICE }, { service, nowMs: 1 }),
      (err) => err instanceof HttpError && err.status === 403 && /removed/.test(err.message)
    );
    const bob = await handleToken({ room: CREDENTIAL, identity: BOB }, { service, nowMs: 1 });
    assert.equal(bob.room, LIVEKIT_ROOM);
  });

  await test('kicking the same identity twice is idempotent (one metadata write, no duplicate entry)', async () => {
    const { service, state, calls } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Team', open: true }) });
    await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    assert.deepEqual(decodeRoomMeta(state.metadata).removed, [ALICE]);
    assert.equal(calls.filter((c) => c === 'updateRoomMetadata').length, 1);
    assert.equal(calls.filter((c) => c.startsWith('removeParticipant')).length, 2);
  });

  await test('a kick whose identity already left still succeeds (record written, "does not exist" tolerated)', async () => {
    const { service, state } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Team', open: true }) });
    const gone = Object.assign(new Error('twirp error not_found: participant does not exist'), { code: 'not_found', status: 404 });
    service.removeParticipant = async () => {
      throw gone;
    };
    const result = await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    assert.equal(result.ok, true);
    assert.deepEqual(decodeRoomMeta(state.metadata).removed, [ALICE], 'the kick still sticks');
    // Any other failure still surfaces to the admin.
    service.removeParticipant = async () => {
      throw Object.assign(new Error('permission denied'), { code: 'permission_denied', status: 403 });
    };
    await assert.rejects(
      () => handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: BOB }, { ...ADMIN, service }),
      /permission denied/
    );
  });

  await test('a kick on a room LiveKit no longer holds still removes the participant (nothing to record)', async () => {
    const { service, calls } = roomMock();
    await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    assert.ok(!calls.includes('updateRoomMetadata'));
    assert.ok(calls.includes(`removeParticipant:${ALICE}`));
  });

  await test('a native re-stamp (ensureRoom on an existing room) preserves the removed list and knock gate', async () => {
    const { service, state } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Team', open: false, removed: [ALICE] }) });
    const env = { url: 'ws://x', apiKey: 'k', apiSecret: 's' };
    await ensureRoom(env, LIVEKIT_ROOM, { displayName: 'Team renamed', open: true }, service, { preserveOpenOnExisting: true });
    const meta = decodeRoomMeta(state.metadata);
    assert.equal(meta.displayName, 'Team renamed', 'display label refreshes');
    assert.equal(meta.open, false, '#203: server-side knock gate preserved');
    assert.deepEqual(meta.removed, [ALICE], 'kick record survives the re-stamp');
  });

  await test('a web create that lands on an existing room (no preserveOpen) still carries removed forward', async () => {
    const { service, state } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Team', open: true, removed: [ALICE] }) });
    const env = { url: 'ws://x', apiKey: 'k', apiSecret: 's' };
    await ensureRoom(env, LIVEKIT_ROOM, { displayName: 'Team', open: false }, service);
    const meta = decodeRoomMeta(state.metadata);
    assert.equal(meta.open, false, 'request value wins without preserveOpenOnExisting (unchanged behaviour)');
    assert.deepEqual(meta.removed, [ALICE]);
  });

  await test('the removed list is bounded to ROOM_META_REMOVED_LIMIT, dropping the oldest', async () => {
    const removed = Array.from({ length: ROOM_META_REMOVED_LIMIT }, (_, i) => `web-${i.toString(16).padStart(8, '0')}-0000-4000-8000-000000000000`);
    const { service, state } = roomMock({ metadata: encodeRoomMeta({ displayName: 'Big', open: true, removed }) });
    await handleAdminControl({ action: 'kick', room: CREDENTIAL, identity: ALICE }, { ...ADMIN, service });
    const meta = decodeRoomMeta(state.metadata);
    assert.equal(meta.removed!.length, ROOM_META_REMOVED_LIMIT);
    assert.equal(meta.removed![meta.removed!.length - 1], ALICE);
    assert.ok(!meta.removed!.includes(removed[0]!), 'the oldest entry was dropped');
  });

  await test('contract fixture: closedRoomTokenRequest refuses without the code / for the removed identity, mints with both right', async () => {
    const contracts = JSON.parse(readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')) as {
      closedRoomTokenRequest: {
        metadata: string;
        request: { room: string; identity: string; displayName: string; accessCode: string };
        removedIdentity: string;
        refusedStatus: number;
      };
    };
    const fixture = contracts.closedRoomTokenRequest;
    assert.equal(fixture.request.room, CREDENTIAL, 'fixture shares the roomCredentials access code');
    const { service } = roomMock({ metadata: fixture.metadata });
    const { accessCode, ...withoutCode } = fixture.request;
    await assert.rejects(
      () => handleToken(withoutCode, { service, nowMs: 1 }),
      (err) => err instanceof HttpError && err.status === fixture.refusedStatus
    );
    await assert.rejects(
      () => handleToken({ ...fixture.request, identity: fixture.removedIdentity }, { service, nowMs: 1 }),
      (err) => err instanceof HttpError && err.status === fixture.refusedStatus
    );
    const minted = await handleToken(fixture.request, { service, nowMs: 1 });
    assert.equal(minted.room, LIVEKIT_ROOM);
    assert.equal(minted.displayName, 'Eng meeting');
    assert.ok(accessCode);
  });

  await test('encodeRoomMeta omits an empty removed list so older metadata stays byte-identical', () => {
    assert.equal(encodeRoomMeta({ displayName: 'Eng meeting', open: false }), '{"displayName":"Eng meeting","open":false}');
    assert.equal(encodeRoomMeta({ displayName: 'Eng meeting', open: false, removed: [] }), '{"displayName":"Eng meeting","open":false}');
  });

  console.log('');
  if (failures === 0) {
    console.log('ALL PASSED');
  } else {
    console.error(`${failures} CHECK(S) FAILED`);
    process.exit(1);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
