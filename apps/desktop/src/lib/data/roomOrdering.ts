import type { RoomRecord } from './rooms';
import { browserStorage, STORAGE_KEYS, type StorageLike } from './storageKeys.ts';

export const FAVORITES_KEY = STORAGE_KEYS.favoriteRooms;

export function roomKey(name: string): string {
  return name.trim().toLowerCase();
}

export function parseFavoriteRooms(raw: string | null): string[] {
  if (!raw) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((value): value is string => typeof value === 'string');
  } catch {
    return [];
  }
}

export function loadFavoriteRooms(storage: StorageLike | undefined = browserStorage()): string[] {
  if (!storage) return [];
  try {
    return parseFavoriteRooms(storage.getItem(FAVORITES_KEY));
  } catch {
    return [];
  }
}

export function saveFavoriteRooms(
  next: string[],
  storage: StorageLike | undefined = browserStorage()
): boolean {
  if (!storage) return false;
  try {
    storage.setItem(FAVORITES_KEY, JSON.stringify(next));
    return true;
  } catch {
    // Browser storage unavailable/full; callers can keep their in-memory state.
    return false;
  }
}

export function orderRoomsForMenu(rooms: RoomRecord[], favoriteRooms: string[]): RoomRecord[] {
  const favoriteSet = new Set(favoriteRooms.map(roomKey));
  const recent = [...rooms].sort((a, b) => {
    const aRecentMs = a.lastJoinedMs ?? a.createdAtMs;
    const bRecentMs = b.lastJoinedMs ?? b.createdAtMs;
    return bRecentMs - aRecentMs || b.createdAtMs - a.createdAtMs;
  });
  const favorites = recent.filter((r) => favoriteSet.has(roomKey(r.name)));
  const others = recent.filter((r) => !favoriteSet.has(roomKey(r.name)));
  return [...favorites, ...others];
}
