import { STORAGE_KEYS, browserStorage, type StorageLike } from '$lib/data/storageKeys';

export type AccessibilityRepairState = 'none' | 'restart-requested' | 'restart-failed';
// `blocked` is accepted because Onboarding shares this status type with its
// prerequisite gates; repair transitions themselves never produce it.
export type AccessibilityRepairStatus = 'up-next' | 'blocked' | 'denied' | 'repair' | 'enabled';

export interface AccessibilityRepairFlow {
  status: AccessibilityRepairStatus;
  settingsOpened: boolean;
  restartFailed: boolean;
}

export type AccessibilityRepairEvent =
  | { type: 'launch'; trusted: boolean }
  | { type: 'settings-opened' }
  | { type: 'explicit-recheck'; trusted: boolean }
  | { type: 'restart-completed'; restarted: boolean };

/**
 * Keep this marker inside Petal's own webview storage and name the bundle it
 * applies to. It is guidance state only: TCC remains the authority.
 */
export function accessibilityRepairPending(storage: Pick<StorageLike, 'getItem'> | undefined = browserStorage()): boolean {
  return storage?.getItem(STORAGE_KEYS.accessibilityRepairPending) === '1';
}

export function recordAccessibilityRepairPending(
  storage: Pick<StorageLike, 'setItem'> | undefined = browserStorage()
): void {
  storage?.setItem(STORAGE_KEYS.accessibilityRepairPending, '1');
}

export function clearAccessibilityRepairPending(
  storage: Pick<StorageLike, 'removeItem'> | undefined = browserStorage()
): void {
  storage?.removeItem(STORAGE_KEYS.accessibilityRepairPending);
}

/** Production transition table for the real onboarding route. */
export function transitionAccessibilityRepair(
  flow: AccessibilityRepairFlow,
  event: AccessibilityRepairEvent,
  storage: StorageLike | undefined = browserStorage()
): AccessibilityRepairFlow {
  switch (event.type) {
    case 'launch':
      if (event.trusted) {
        clearAccessibilityRepairPending(storage);
        return { status: 'enabled', settingsOpened: false, restartFailed: false };
      }
      return {
        status: accessibilityRepairPending(storage) ? 'repair' : 'up-next',
        settingsOpened: false,
        restartFailed: false
      };
    case 'settings-opened':
      // Opening Settings is not evidence of trust; retain the current status.
      return { ...flow, settingsOpened: true };
    case 'explicit-recheck':
      if (event.trusted) {
        clearAccessibilityRepairPending(storage);
        return { status: 'enabled', settingsOpened: false, restartFailed: false };
      }
      recordAccessibilityRepairPending(storage);
      return { status: 'repair', settingsOpened: true, restartFailed: false };
    case 'restart-completed':
      recordAccessibilityRepairPending(storage);
      return { ...flow, restartFailed: !event.restarted };
  }
}
