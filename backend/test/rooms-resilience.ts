// #708: LiveKit Cloud's Twirp RPC transport occasionally returns a transient
// `503 Service Unavailable: no response from servers` (TwirpError with
// `status: 503` / `code: 'unavailable'`) for a single RoomServiceClient RPC.
// `handleListRooms` once fanned per-room `listParticipants` calls out over
// EVERY live room, so ONE room's transient failure 5xx'd the entire
// `/api/rooms GET` response (Sentry PETAL-BACKEND-3). That unbounded fan-out
// is gone for good: the room list is one `listRooms` RPC.
//
// #120 brought a per-room call back to `handleRoomStatus` alone, bounded by
// the credentials the caller presented (<= 64) rather than by the number of
// live rooms, because `numParticipants` counts hidden `-gallery` bridges and
// so cannot be shown to a user as a headcount. What this suite proves is
// therefore both halves: the room list is still one retried RPC, and the
// per-room call that came back carries #708's isolation with it -- one
// room's failure falls back to that room's `numParticipants` and never
// fails the batch.
//
// What is REAL here and what is mocked: everything -- this suite never
// touches a live LiveKit server. It exercises the real `withLiveKitRetry`
// helper and the real `handleListRooms`/`handleAdminControl` handler code
// against an injected `RoomListingService`/`RoomAdminService` mock (the
// `context.service` seam), so it runs in CI (`npm test`) without
// `livekit-server --dev`. `test/local.ts` still covers the live-server path
// separately.

import assert from 'node:assert/strict';
import { ParticipantInfo, ParticipantInfo_State, ParticipantPermission } from 'livekit-server-sdk';
import type { RoomAdminService, RoomDiscoveryService, RoomListingService } from '../lib/livekit.js';
import { withLiveKitRetry } from '../lib/livekit.js';
import {
  handleAdminControl,
  handleListRooms,
  handleRoomStatus,
  resetTokenRateLimitsForTest,
} from '../lib/handlers.js';
import { credentialForAccessCode, livekitRoomName } from '../lib/slug.js';

process.env.LIVEKIT_URL = 'ws://rooms-resilience-test.invalid';
process.env.LIVEKIT_API_KEY = 'rooms_resilience_test_key';
process.env.LIVEKIT_API_SECRET = 'rooms_resilience_test_secret';
process.env.PETAL_ADMIN_TOKEN = 'rooms_resilience_admin_token';

// Mirrors the real SDK's TwirpError shape (see
// node_modules/livekit-server-sdk/dist/TwirpRPC.js): `status` is the HTTP
// status, `code` is the Twirp error code.
class FakeTwirpError extends Error {
  status: number;
  code: string;
  constructor(status: number, code: string, message = 'no response from servers') {
    super(message);
    this.name = 'Unavailable';
    this.status = status;
    this.code = code;
  }
}

function transient503(): FakeTwirpError {
  return new FakeTwirpError(503, 'unavailable');
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
  console.log('withLiveKitRetry:');

  await test('retries once on a 503 status and returns the eventual success', async () => {
    let calls = 0;
    const result = await withLiveKitRetry(async () => {
      calls++;
      if (calls === 1) throw transient503();
      return 'ok';
    }, 2, 1);
    assert.equal(result, 'ok');
    assert.equal(calls, 2, 'must have retried exactly once');
  });

  await test("retries once on code: 'unavailable' even with a non-503 status", async () => {
    let calls = 0;
    const result = await withLiveKitRetry(async () => {
      calls++;
      if (calls === 1) throw new FakeTwirpError(500, 'unavailable');
      return 'ok';
    }, 2, 1);
    assert.equal(result, 'ok');
    assert.equal(calls, 2);
  });

  await test('does not retry a non-retryable error (e.g. 400) -- fails on the first attempt', async () => {
    let calls = 0;
    await assert.rejects(
      () =>
        withLiveKitRetry(async () => {
          calls++;
          throw new FakeTwirpError(400, 'invalid_argument');
        }, 2, 1),
      (err: unknown) => err instanceof FakeTwirpError && err.status === 400
    );
    assert.equal(calls, 1, 'a non-retryable error must not be retried');
  });

  await test('exhausts the attempt budget and rethrows the LAST error unchanged', async () => {
    let calls = 0;
    await assert.rejects(
      () =>
        withLiveKitRetry(async () => {
          calls++;
          throw transient503();
        }, 2, 1),
      (err: unknown) => err instanceof FakeTwirpError && err.status === 503
    );
    assert.equal(calls, 2, 'exactly `attempts` tries, no more');
  });

  await test('a single attempt (attempts=1) never retries even a retryable error', async () => {
    let calls = 0;
    await assert.rejects(
      () =>
        withLiveKitRetry(async () => {
          calls++;
          throw transient503();
        }, 1, 1),
      (err: unknown) => err instanceof FakeTwirpError && err.status === 503
    );
    assert.equal(calls, 1);
  });

  console.log('');
  console.log('handleListRooms -- one listRooms RPC, retried, no per-room fan-out (#708):');

  const ROOM_A = 'petal-room-alpha-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const ROOM_B = 'petal-room-bravo-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb';

  function metaFor(displayName: string): string {
    return JSON.stringify({ displayName, open: true });
  }

  await test('the ROOM LIST is still one RPC: handleListRooms keeps numParticipants and never fans out', async () => {
    // Deliberately still true after #120: `handleListRooms` is server-side
    // tooling (GET /api/rooms is 410), so it keeps the cheap room-level
    // number. The number a USER sees comes from `handleRoomStatus` below.
    let participantCalls = 0;
    const service = {
      async listRooms() {
        return [
          { name: ROOM_A, metadata: metaFor('Room A'), numParticipants: 2 },
          { name: ROOM_B, metadata: metaFor('Room B'), numParticipants: 1 },
        ] as never;
      },
      async listParticipants() {
        participantCalls++;
        return [] as never;
      },
    } as unknown as RoomListingService;
    const { rooms } = await handleListRooms({ nowMs: 1_000, service });
    assert.equal(rooms.length, 2);
    assert.equal(rooms.find((r) => r.name === 'Room A')!.occupancy, 2);
    assert.equal(rooms.find((r) => r.name === 'Room B')!.occupancy, 1);
    assert.equal(participantCalls, 0, 'the room list must cost exactly one upstream RPC');
  });

  await test('response shape is unchanged: each room view carries exactly id/name/open/occupancy', async () => {
    const service: RoomListingService = {
      async listRooms() {
        return [{ name: ROOM_A, metadata: metaFor('Shape Room'), numParticipants: 1 }] as never;
      },
    };
    const { rooms } = await handleListRooms({ nowMs: 1_000, service });
    assert.equal(rooms.length, 1);
    assert.deepEqual(Object.keys(rooms[0]!).sort(), ['id', 'name', 'occupancy', 'open']);
    assert.equal(typeof rooms[0]!.id, 'string');
    assert.equal(typeof rooms[0]!.open, 'boolean');
  });

  await test('listRooms failing after retry propagates as a real rejection', async () => {
    const service: RoomListingService = {
      async listRooms() {
        throw transient503();
      },
    };
    await assert.rejects(() => handleListRooms({ nowMs: 1_000, service }));
  });

  await test('listRooms itself is retried: a transient 503 on the room list recovers instead of failing the request', async () => {
    let listCalls = 0;
    const service: RoomListingService = {
      async listRooms() {
        listCalls++;
        if (listCalls === 1) throw transient503();
        return [{ name: ROOM_A, metadata: metaFor('Recovered'), numParticipants: 3 }] as never;
      },
    };
    const { rooms } = await handleListRooms({ nowMs: 1_000, service });
    assert.equal(rooms.length, 1);
    assert.equal(rooms[0]!.name, 'Recovered');
    assert.equal(rooms[0]!.occupancy, 3);
    assert.equal(listCalls, 2, 'listRooms was retried once');
  });

  console.log('');
  console.log('handleRoomStatus -- the #120 per-room count, bounded and isolated:');

  const STATUS_CODE_A = 'aaa-bbbb-ccc';
  const STATUS_CODE_B = 'ddd-eeee-fff';
  const STATUS_CRED_A = credentialForAccessCode(STATUS_CODE_A)!;
  const STATUS_CRED_B = credentialForAccessCode(STATUS_CODE_B)!;
  const STATUS_ROOM_A = livekitRoomName(STATUS_CRED_A);
  const STATUS_ROOM_B = livekitRoomName(STATUS_CRED_B);
  const HUMAN = '11111111-1111-4111-8111-111111111111';
  const OTHER_HUMAN = '22222222-2222-4222-8222-222222222222';

  function person(identity: string, hidden = false): ParticipantInfo {
    return new ParticipantInfo({
      identity,
      name: identity,
      state: ParticipantInfo_State.ACTIVE,
      permission: new ParticipantPermission({ hidden, canSubscribe: true, canPublish: !hidden }),
    });
  }

  // Two live rooms, each with one human plus that human's hidden bridge, so
  // `numParticipants` says 2 and the truth is 1.
  function statusService(overrides: {
    numParticipantsA?: number;
    numParticipantsB?: number;
    listParticipants: (room: string) => Promise<ParticipantInfo[]>;
  }) {
    const participantCalls: string[] = [];
    const service = {
      async listRooms() {
        return [
          { name: STATUS_ROOM_A, metadata: metaFor('Status A'), numParticipants: overrides.numParticipantsA ?? 2 },
          { name: STATUS_ROOM_B, metadata: metaFor('Status B'), numParticipants: overrides.numParticipantsB ?? 2 },
        ] as never;
      },
      async listParticipants(room: string) {
        participantCalls.push(room);
        return overrides.listParticipants(room);
      },
    } as unknown as RoomListingService & RoomDiscoveryService;
    return { service, participantCalls };
  }

  await test('occupancy is the visible headcount: the hidden -gallery bridge in numParticipants is not a person', async () => {
    const { service } = statusService({
      listParticipants: async (room) =>
        room === STATUS_ROOM_A
          ? [person(HUMAN), person(`${HUMAN}-gallery`, true)]
          : [person(OTHER_HUMAN), person(`${OTHER_HUMAN}-gallery`, true)],
    });
    const { rooms } = await handleRoomStatus(
      { rooms: [{ room: STATUS_CRED_A }, { room: STATUS_CRED_B }] },
      { nowMs: 1_000, service }
    );
    assert.equal(rooms.length, 2);
    for (const room of rooms) {
      assert.equal(room.occupancy, 1, `${room.name}: numParticipants says 2, one of them is a bridge`);
    }
  });

  await test('one room\'s listParticipants failure falls back to its numParticipants; the batch and its siblings are unaffected', async () => {
    const { service } = statusService({
      numParticipantsA: 7,
      listParticipants: async (room) => {
        if (room === STATUS_ROOM_A) throw transient503();
        return [person(OTHER_HUMAN), person(`${OTHER_HUMAN}-gallery`, true)];
      },
    });
    const { rooms } = await handleRoomStatus(
      { rooms: [{ room: STATUS_CRED_A }, { room: STATUS_CRED_B }] },
      { nowMs: 1_000, service }
    );
    assert.equal(rooms.length, 2, 'a failing room never fails the batch');
    const a = rooms.find((r) => r.name === 'Status A')!;
    const b = rooms.find((r) => r.name === 'Status B')!;
    assert.equal(a.occupancy, 7, 'falls back to numParticipants, not to 0 and not to an error');
    assert.equal(b.occupancy, 1, 'the healthy sibling is still exact');
  });

  await test('a transient 503 on listParticipants is retried, like every other LiveKit RPC', async () => {
    let attempts = 0;
    const { service } = statusService({
      listParticipants: async () => {
        attempts++;
        if (attempts === 1) throw transient503();
        return [person(HUMAN), person(`${HUMAN}-gallery`, true)];
      },
    });
    const { rooms } = await handleRoomStatus({ rooms: [{ room: STATUS_CRED_A }] }, { nowMs: 1_000, service });
    assert.equal(attempts, 2, 'retried once');
    assert.equal(rooms[0]!.occupancy, 1, 'the retry, not the fallback, produced this number');
  });

  await test('the fan-out is bounded: no participants call for a room the caller did not present, or for an empty one', async () => {
    const { service, participantCalls } = statusService({
      numParticipantsB: 0,
      listParticipants: async () => [person(HUMAN)],
    });
    const presentedOnly = await handleRoomStatus({ rooms: [{ room: STATUS_CRED_A }] }, { nowMs: 1_000, service });
    assert.equal(presentedOnly.rooms.length, 1);
    assert.deepEqual(participantCalls, [STATUS_ROOM_A], 'only the presented room costs an RPC');

    // Room B is live in the list but empty, and presented: a room the list
    // already calls empty cannot gain people, so it costs nothing.
    const both = await handleRoomStatus(
      { rooms: [{ room: STATUS_CRED_A }, { room: STATUS_CRED_B }] },
      { nowMs: 1_000 + 10_000, service }
    );
    assert.equal(both.rooms.find((r) => r.name === 'Status B')!.occupancy, 0);
    assert.deepEqual(
      participantCalls.filter((room) => room === STATUS_ROOM_B),
      [],
      'an empty room is never fanned out to'
    );
  });

  console.log('');
  console.log('handleAdminControl -- retry on the same transient LiveKit failure class (Sentry PETAL-BACKEND-2):');

  const ADMIN_CREDENTIAL = credentialForAccessCode('abc-defg-hij')!;

  await test('kick (removeParticipant) is retried once on a transient 503 before succeeding', async () => {
    let calls = 0;
    const service: RoomAdminService = {
      async listRooms() {
        return [] as never;
      },
      async updateRoomMetadata() {
        throw new Error('not exercised by this test (room absent)');
      },
      async removeParticipant() {
        calls++;
        if (calls === 1) throw transient503();
        return {} as never;
      },
      async deleteRoom() {
        throw new Error('not exercised by this test');
      },
    } as unknown as RoomAdminService;
    const result = await handleAdminControl(
      { action: 'kick', room: ADMIN_CREDENTIAL, identity: 'alice' },
      { authorization: `Bearer ${process.env.PETAL_ADMIN_TOKEN}`, service }
    );
    assert.equal(result.ok, true);
    assert.equal(calls, 2);
  });

  await test('close (deleteRoom) is retried once on a transient 503 before succeeding', async () => {
    let calls = 0;
    const service: RoomAdminService = {
      async listRooms() {
        throw new Error('not exercised by this test');
      },
      async updateRoomMetadata() {
        throw new Error('not exercised by this test');
      },
      async removeParticipant() {
        throw new Error('not exercised by this test');
      },
      async deleteRoom() {
        calls++;
        if (calls === 1) throw transient503();
        return {} as never;
      },
    } as unknown as RoomAdminService;
    const result = await handleAdminControl(
      { action: 'close', room: ADMIN_CREDENTIAL },
      { authorization: `Bearer ${process.env.PETAL_ADMIN_TOKEN}`, service }
    );
    assert.equal(result.ok, true);
    assert.equal(calls, 2);
  });

  await test('kick still surfaces a non-retryable failure unchanged after the retry helper gives up', async () => {
    const service: RoomAdminService = {
      async listRooms() {
        return [] as never;
      },
      async updateRoomMetadata() {
        throw new Error('not exercised by this test (room absent)');
      },
      async removeParticipant() {
        throw transient503();
      },
      async deleteRoom() {
        throw new Error('not exercised by this test');
      },
    } as unknown as RoomAdminService;
    await assert.rejects(
      () =>
        handleAdminControl(
          { action: 'kick', room: ADMIN_CREDENTIAL, identity: 'alice' },
          { authorization: `Bearer ${process.env.PETAL_ADMIN_TOKEN}`, service }
        ),
      (err: unknown) => err instanceof FakeTwirpError && err.status === 503
    );
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
