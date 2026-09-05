export interface PendingRoomLabelStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

const KEY_PREFIX = 'petal.pendingRoomDisplayName.';

function storageKey(roomName: string): string {
  return `${KEY_PREFIX}${roomName.trim().toLowerCase()}`;
}

function browserSessionStorage(): PendingRoomLabelStorage | null {
  try {
    return globalThis.sessionStorage ?? null;
  } catch {
    return null;
  }
}

function cleanDisplayName(displayName: string | null | undefined): string | null {
  const cleaned = displayName?.trim();
  return cleaned ? cleaned : null;
}

export function rememberPendingRoomDisplayName(
  roomName: string,
  displayName: string | null | undefined,
  storage: PendingRoomLabelStorage | null = browserSessionStorage()
): void {
  if (!storage) return;
  const cleaned = cleanDisplayName(displayName);
  const key = storageKey(roomName);
  try {
    if (cleaned) storage.setItem(key, cleaned);
    else storage.removeItem(key);
  } catch {
    // Best-effort polish only; persistence and auth do not depend on this.
  }
}

export function consumePendingRoomDisplayName(
  roomName: string,
  storage: PendingRoomLabelStorage | null = browserSessionStorage()
): string | null {
  if (!storage) return null;
  const key = storageKey(roomName);
  try {
    const value = cleanDisplayName(storage.getItem(key));
    storage.removeItem(key);
    return value;
  } catch {
    return null;
  }
}
