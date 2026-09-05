import { browserStorage, clearFactoryResetStorage } from './storageKeys.ts';

export const PENDING_ROOM_DISPLAY_NAME_PREFIX = 'petal.pendingRoomDisplayName.';

const TCC_SERVICES = ['ScreenCapture', 'Accessibility', 'Microphone', 'Camera'] as const;

export function tccResetCommand(bundleIdentifier: string): string {
  const bundle = bundleIdentifier.trim();
  return TCC_SERVICES.map((service) => `tccutil reset ${service} ${bundle}`).join('\n');
}

export function clearPendingRoomDisplayNames(storage: Storage | undefined) {
  if (!storage) return;
  const keys: string[] = [];
  for (let i = 0; i < storage.length; i += 1) {
    const key = storage.key(i);
    if (key?.startsWith(PENDING_ROOM_DISPLAY_NAME_PREFIX)) keys.push(key);
  }
  for (const key of keys) storage.removeItem(key);
}

export function clearLocalFactoryResetState() {
  clearFactoryResetStorage(browserStorage());
  if (typeof sessionStorage !== 'undefined') clearPendingRoomDisplayNames(sessionStorage);
}
