import assert from 'node:assert/strict';
import test from 'node:test';

import {
  consumePendingRoomDisplayName,
  rememberPendingRoomDisplayName,
  type PendingRoomLabelStorage
} from '../src/lib/data/pendingRoomLabels.ts';

class MemoryStorage implements PendingRoomLabelStorage {
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

test('pending room display names are keyed by credential and consumed once', () => {
  const storage = new MemoryStorage();
  const credential = 'room-8535e993a1b76ed8a9ee59b265f53dfc';

  rememberPendingRoomDisplayName(credential, ' Design Review! ', storage);

  assert.equal(consumePendingRoomDisplayName(credential.toUpperCase(), storage), 'Design Review!');
  assert.equal(consumePendingRoomDisplayName(credential, storage), null);
});

test('blank pending room display names clear any previous value', () => {
  const storage = new MemoryStorage();
  const credential = 'room-8535e993a1b76ed8a9ee59b265f53dfc';

  rememberPendingRoomDisplayName(credential, 'Design Review!', storage);
  rememberPendingRoomDisplayName(credential, '   ', storage);

  assert.equal(consumePendingRoomDisplayName(credential, storage), null);
});
