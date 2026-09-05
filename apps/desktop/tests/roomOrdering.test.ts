import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  FAVORITES_KEY,
  loadFavoriteRooms,
  orderRoomsForMenu,
  parseFavoriteRooms,
  roomKey,
  saveFavoriteRooms,
  type StorageLike
} from '../src/lib/data/roomOrdering.ts';
import type { RoomRecord } from '../src/lib/data/rooms.ts';

function room(name: string, createdAtMs: number, lastJoinedMs?: number | null): RoomRecord {
  return {
    id: `${name}-id`,
    name,
    slug: name.trim().toLowerCase(),
    createdAtMs,
    lastJoinedMs,
    open: true
  };
}

class MemoryStorage implements StorageLike {
  values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

test('roomKey normalizes casing and incidental whitespace', () => {
  assert.equal(roomKey(' WebTest '), 'webtest');
  assert.equal(roomKey('RIDGE-vivid-reef'), 'ridge-vivid-reef');
});

test('orderRoomsForMenu returns recent favorites first, then recent others', () => {
  const rooms = [
    room('old-favorite', 100),
    room('new-other', 400),
    room('new-favorite', 300),
    room('old-other', 200)
  ];

  assert.deepEqual(
    orderRoomsForMenu(rooms, ['NEW-FAVORITE', 'old-favorite']).map((r) => r.name),
    ['new-favorite', 'old-favorite', 'new-other', 'old-other']
  );
});

test('orderRoomsForMenu sorts by last joined timestamp with created timestamp fallback', () => {
  const rooms = [
    room('created-newer', 500),
    room('joined-most-recent', 100, 900),
    room('joined-middle', 700, 800)
  ];

  assert.deepEqual(
    orderRoomsForMenu(rooms, []).map((r) => r.name),
    ['joined-most-recent', 'joined-middle', 'created-newer']
  );
});

test('orderRoomsForMenu is immutable and preserves original records', () => {
  const rooms = [room('a', 1), room('b', 2)];
  const ordered = orderRoomsForMenu(rooms, ['a']);

  assert.deepEqual(rooms.map((r) => r.name), ['a', 'b']);
  assert.equal(ordered[0], rooms[0]);
  assert.equal(ordered[1], rooms[1]);
});

test('parseFavoriteRooms tolerates missing, invalid, and mixed storage values', () => {
  assert.deepEqual(parseFavoriteRooms(null), []);
  assert.deepEqual(parseFavoriteRooms('not json'), []);
  assert.deepEqual(parseFavoriteRooms('{"not":"an array"}'), []);
  assert.deepEqual(parseFavoriteRooms('["alpha", 42, "beta", null]'), ['alpha', 'beta']);
});

test('loadFavoriteRooms and saveFavoriteRooms use injected storage', () => {
  const storage = new MemoryStorage();

  assert.equal(saveFavoriteRooms(['alpha', 'beta'], storage), true);
  assert.equal(storage.getItem(FAVORITES_KEY), '["alpha","beta"]');
  assert.deepEqual(loadFavoriteRooms(storage), ['alpha', 'beta']);
});

test('storage failures are contained so menu rendering can continue', () => {
  const throwingStorage: StorageLike = {
    getItem() {
      throw new Error('read failed');
    },
    setItem() {
      throw new Error('write failed');
    },
    removeItem() {
      throw new Error('remove failed');
    }
  };

  assert.deepEqual(loadFavoriteRooms(throwingStorage), []);
  assert.equal(saveFavoriteRooms(['alpha'], throwingStorage), false);
});
