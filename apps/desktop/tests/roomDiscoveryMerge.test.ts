import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  mergeRoomsWithDiscovery,
  persistRoomDisplayNameRepairsFromDiscovery,
  roomDisplayNameRepairsFromDiscovery,
  type RoomRecord
} from '../src/lib/data/rooms.ts';
import type { RoomOccupancy } from '../src/lib/ipc.ts';

function localRoom(name: string, displayName: string | null = null): RoomRecord {
  return {
    id: `local:${name}`,
    name,
    accessCode: 'abc-defg-hjk',
    displayName,
    slug: name,
    createdAtMs: 100,
    open: true
  };
}

test('public room discovery rows without credentials are not converted into joinable rooms', () => {
  const merged = mergeRoomsWithDiscovery([], [
    {
      id: 'room_1234',
      roomName: 'Private Standup',
      livekitRoom: 'room_1234',
      available: true,
      name: 'Private Standup',
      open: true,
      occupancy: 2
    } satisfies RoomOccupancy
  ]);

  assert.deepEqual(merged, []);
});

test('explicit credential discovery rows remain joinable for trusted callers', () => {
  const credential = 'room-8535e993a1b76ed8a9ee59b265f53dfc';
  const merged = mergeRoomsWithDiscovery([], [
    {
      id: 'room_1234',
      roomName: credential,
      slug: credential,
      livekitRoom: `petal-room-${credential}`,
      available: true,
      name: 'Private Standup',
      open: true,
      occupancy: 2
    } satisfies RoomOccupancy
  ]);

  assert.equal(merged.length, 1);
  assert.equal(merged[0]?.name, credential);
  assert.equal(merged[0]?.slug, credential);
  assert.equal(merged[0]?.displayName, 'Private Standup');
});

test('opaque room ids match existing local rooms without exposing credentials', () => {
  const room = localRoom('room-8535e993a1b76ed8a9ee59b265f53dfc');
  const merged = mergeRoomsWithDiscovery([room], [
    {
      id: room.id,
      roomName: 'Private Standup',
      livekitRoom: 'room_1234',
      available: true,
      name: 'Private Standup',
      open: true,
      occupancy: 2
    } satisfies RoomOccupancy
  ]);

  assert.deepEqual(merged, [room]);
});

test('slug-less occupancy rows plan fill-only display-name self-heal before the credential gate', () => {
  const room = localRoom('room-8535e993a1b76ed8a9ee59b265f53dfc');
  const repairs = roomDisplayNameRepairsFromDiscovery([room], [
    {
      id: 'public-hash',
      roomName: room.name,
      livekitRoom: `petal-room-${room.name}`,
      available: true,
      name: 'Design sync',
      open: true,
      occupancy: 1
    } satisfies RoomOccupancy
  ]);

  assert.deepEqual(repairs, [{ idOrCode: room.name, displayName: 'Design sync' }]);
});

test('display-name self-heal persists slug-less directory labels without clobbering user labels', async () => {
  const unnamed = localRoom('room-8535e993a1b76ed8a9ee59b265f53dfc');
  const userNamed = localRoom('room-0d11c0ffee0d11c0ffee0d11c0ffee0d', 'My local label');
  const calls: [string, string][] = [];

  const repaired = await persistRoomDisplayNameRepairsFromDiscovery(
    [unnamed, userNamed],
    [
      {
        id: 'public-hash-unnamed',
        roomName: unnamed.name,
        livekitRoom: `petal-room-${unnamed.name}`,
        available: true,
        name: 'Design sync',
        open: true,
        occupancy: 1
      } satisfies RoomOccupancy,
      {
        id: 'public-hash-user-named',
        roomName: userNamed.name,
        livekitRoom: `petal-room-${userNamed.name}`,
        available: true,
        name: 'Server rename',
        open: true,
        occupancy: 1
      } satisfies RoomOccupancy
    ],
    async (idOrCode, displayName) => {
      calls.push([idOrCode, displayName]);
      return { ...unnamed, displayName };
    }
  );

  assert.deepEqual(calls, [[unnamed.name, 'Design sync']]);
  assert.equal(repaired[0]?.displayName, 'Design sync');
  assert.equal(repaired[1]?.displayName, 'My local label');
});

test('display-name self-heal treats legacy generic room labels as empty', async () => {
  const room = localRoom('room-8535e993a1b76ed8a9ee59b265f53dfc', 'room');

  const repaired = await persistRoomDisplayNameRepairsFromDiscovery(
    [room],
    [
      {
        id: 'public-hash',
        roomName: room.name,
        livekitRoom: `petal-room-${room.name}`,
        available: true,
        name: 'Design sync',
        open: true,
        occupancy: 1
      } satisfies RoomOccupancy
    ],
    async (_idOrCode, displayName) => ({ ...room, displayName })
  );

  assert.equal(repaired[0]?.displayName, 'Design sync');
});
