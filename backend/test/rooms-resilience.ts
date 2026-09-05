// #708: LiveKit Cloud's Twirp RPC transport occasionally returns a transient
// `503 Service Unavailable: no response from servers` (TwirpError with
// `status: 503` / `code: 'unavailable'`) for a single RoomServiceClient RPC.
// `handleListRooms` once fanned per-room `listParticipants` calls out, so ONE
// room's transient failure 5xx'd the entire `/api/rooms GET` response (Sentry
// PETAL-BACKEND-3). The fan-out is gone entirely now (occupancy comes from
// `listRooms` itself), so what remains to prove
// is that the single remaining RPC is retried and nothing else is called.
//
// What is REAL here and what is mocked: everything -- this suite never
// touches a live LiveKit server. It exercises the real `withLiveKitRetry`
// helper and the real `handleListRooms`/`handleAdminControl` handler code
// against an injected `RoomListingService`/`RoomAdminService` mock (the
// `context.service` seam), so it runs in CI (`npm test`) without
// `livekit-server --dev`. `test/local.ts` still covers the live-server path
// separately.

import assert from 'node:assert/strict';
import type { RoomAdminService, RoomListingService } from '../lib/livekit.js';
import { withLiveKitRetry } from '../lib/livekit.js';
import {
  handleAdminControl,
  handleListRooms,
  resetTokenRateLimitsForTest,
} from '../lib/handlers.js';
import { credentialForAccessCode } from '../lib/slug.js';

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
  console.log('handleListRooms -- one listRooms RPC, retried, no per-room fan-out (#708 superseded):');

  const ROOM_A = 'petal-room-alpha-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const ROOM_B = 'petal-room-bravo-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb';

  function metaFor(displayName: string): string {
    return JSON.stringify({ displayName, open: true });
  }

  await test('occupancy comes from listRooms numParticipants -- listParticipants is never called', async () => {
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
    assert.equal(participantCalls, 0, 'discovery must cost exactly one upstream RPC');
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
