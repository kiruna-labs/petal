import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  clearPendingRoomDisplayNames,
  PENDING_ROOM_DISPLAY_NAME_PREFIX,
  tccResetCommand
} from '../src/lib/data/factoryReset.ts';
import { FACTORY_RESET_STORAGE_KEYS, STORAGE_KEYS, clearFactoryResetStorage } from '../src/lib/data/storageKeys.ts';

class MemoryStorage {
  values = new Map<string, string>();

  get length(): number {
    return this.values.size;
  }

  key(index: number): string | null {
    return Array.from(this.values.keys())[index] ?? null;
  }

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

test('factory reset clears every durable frontend key', () => {
  assert.deepEqual(FACTORY_RESET_STORAGE_KEYS, [
    STORAGE_KEYS.onboardingSession,
    STORAGE_KEYS.accessibilityRepairPending,
    STORAGE_KEYS.favoriteRooms,
    STORAGE_KEYS.mainWindowGeometry,
    STORAGE_KEYS.meetingWindowGeometry,
    STORAGE_KEYS.pillWindowGeometry,
    STORAGE_KEYS.windowPickerSnapshot
  ]);

  const storage = new MemoryStorage();
  for (const key of FACTORY_RESET_STORAGE_KEYS) storage.setItem(key, 'value');
  storage.setItem('unrelated', 'keep');

  clearFactoryResetStorage(storage);

  for (const key of FACTORY_RESET_STORAGE_KEYS) assert.equal(storage.getItem(key), null);
  assert.equal(storage.getItem('unrelated'), 'keep');
});

test('factory reset clears pending room labels by prefix only', () => {
  const storage = new MemoryStorage();
  storage.setItem(`${PENDING_ROOM_DISPLAY_NAME_PREFIX}room-a`, 'A');
  storage.setItem(`${PENDING_ROOM_DISPLAY_NAME_PREFIX}room-b`, 'B');
  storage.setItem('petal.other', 'keep');

  clearPendingRoomDisplayNames(storage as unknown as Storage);

  assert.equal(storage.getItem(`${PENDING_ROOM_DISPLAY_NAME_PREFIX}room-a`), null);
  assert.equal(storage.getItem(`${PENDING_ROOM_DISPLAY_NAME_PREFIX}room-b`), null);
  assert.equal(storage.getItem('petal.other'), 'keep');
});

test('permission reset command uses the four Petal TCC services and runtime bundle id', () => {
  assert.equal(
    tccResetCommand('com.example.PetalQA'),
    [
      'tccutil reset ScreenCapture com.example.PetalQA',
      'tccutil reset Accessibility com.example.PetalQA',
      'tccutil reset Microphone com.example.PetalQA',
      'tccutil reset Camera com.example.PetalQA'
    ].join('\n')
  );
});

test('resetOnboarding resets the full stored session, not only onboardingComplete', () => {
  const source = readFileSync(new URL('../src/lib/stores/session.svelte.ts', import.meta.url), 'utf8');

  assert.match(source, /Object\.assign\(session,\s*\{\s*\.{3}defaults,\s*participantId:\s*newParticipantId\(\)\s*\}\)/);
});
