import { clearPluginStorage } from '@petal/shared/plugin-host/settingsModel';

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export const STORAGE_KEYS = {
  favoriteRooms: 'petal.favoriteRooms.v1',
  lastUpdateCheckMs: 'petal.lastUpdateCheckMs.v1',
  mainWindowGeometry: 'petal.windowGeometry.main.v1',
  meetingWindowGeometry: 'petal.windowGeometry.meeting.v1',
  pillWindowGeometry: 'petal.windowGeometry.pill.v1',
  windowPickerSnapshot: 'petal.window-picker.snapshot.v1',
  onboardingSession: 'petal:onboarding-session:v1',
  accessibilityRepairPending: 'petal:accessibility-repair:com.petal.app:v1'
} as const;

export const FACTORY_RESET_STORAGE_KEYS = [
  STORAGE_KEYS.onboardingSession,
  STORAGE_KEYS.accessibilityRepairPending,
  STORAGE_KEYS.favoriteRooms,
  STORAGE_KEYS.mainWindowGeometry,
  STORAGE_KEYS.meetingWindowGeometry,
  STORAGE_KEYS.pillWindowGeometry,
  STORAGE_KEYS.windowPickerSnapshot
] as const;

export function clearFactoryResetStorage(storage: Pick<StorageLike, 'removeItem'> | undefined) {
  if (!storage) return;
  for (const key of FACTORY_RESET_STORAGE_KEYS) storage.removeItem(key);
  // Plugin enabled map + per-plugin KV (dynamic keys) — plugins/README.md §2.2.
  clearPluginStorage(storage as StorageLike);
}

export function browserStorage(): StorageLike | undefined {
  return typeof localStorage === 'undefined' ? undefined : localStorage;
}
