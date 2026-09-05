import assert from 'node:assert/strict';
import test from 'node:test';
import {
  accessibilityRepairPending,
  transitionAccessibilityRepair,
  type AccessibilityRepairFlow
} from '../src/lib/data/accessibilityRepair.ts';
import { STORAGE_KEYS, type StorageLike } from '../src/lib/data/storageKeys.ts';

class MemoryStorage implements StorageLike {
  values = new Map<string, string>();
  getItem(key: string) { return this.values.get(key) ?? null; }
  setItem(key: string, value: string) { this.values.set(key, value); }
  removeItem(key: string) { this.values.delete(key); }
}

const denied: AccessibilityRepairFlow = {
  status: 'denied',
  settingsOpened: false,
  restartFailed: false
};

test('route interaction keeps denied after Settings and enters repair only after an explicit failed recheck', () => {
  const storage = new MemoryStorage();
  const opened = transitionAccessibilityRepair(denied, { type: 'settings-opened' }, storage);
  assert.deepEqual(opened, { ...denied, settingsOpened: true });
  assert.equal(accessibilityRepairPending(storage), false);

  const repair = transitionAccessibilityRepair(opened, { type: 'explicit-recheck', trusted: false }, storage);
  assert.equal(repair.status, 'repair');
  assert.equal(accessibilityRepairPending(storage), true);
  assert.equal(storage.getItem(STORAGE_KEYS.accessibilityRepairPending), '1');
});

test('repair settings, restart failure, and false next launch preserve the pending fallback', () => {
  const storage = new MemoryStorage();
  const repair: AccessibilityRepairFlow = { status: 'repair', settingsOpened: false, restartFailed: false };
  const settings = transitionAccessibilityRepair(repair, { type: 'settings-opened' }, storage);
  assert.equal(settings.settingsOpened, true);
  const failedRestart = transitionAccessibilityRepair(settings, { type: 'restart-completed', restarted: false }, storage);
  assert.equal(failedRestart.restartFailed, true);
  assert.equal(accessibilityRepairPending(storage), true);
  assert.equal(
    transitionAccessibilityRepair(denied, { type: 'launch', trusted: false }, storage).status,
    'repair'
  );
});

test('a trusted next launch clears the persisted repair marker', () => {
  const storage = new MemoryStorage();
  storage.setItem(STORAGE_KEYS.accessibilityRepairPending, '1');
  const restored = transitionAccessibilityRepair(denied, { type: 'launch', trusted: true }, storage);
  assert.equal(restored.status, 'enabled');
  assert.equal(accessibilityRepairPending(storage), false);
});
