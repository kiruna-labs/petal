import { HARNESS_ROOM_DISPLAY_NAMES_STORAGE_KEY } from './constants.ts';
type RoomLabelStorage = Pick<Storage, 'getItem' | 'setItem'>;

function defaultStorage(): RoomLabelStorage | null {
  return typeof localStorage === 'undefined' ? null : localStorage;
}

function normalizedCredential(code: string): string {
  return code.trim().toLowerCase();
}

function readLabelMap(storage: RoomLabelStorage | null): Record<string, string> {
  if (!storage) return {};
  try {
    const parsed = JSON.parse(storage.getItem(HARNESS_ROOM_DISPLAY_NAMES_STORAGE_KEY) ?? '{}');
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed as Record<string, string> : {};
  } catch {
    return {};
  }
}

function writeLabelMap(storage: RoomLabelStorage | null, labels: Record<string, string>) {
  if (!storage) return;
  storage.setItem(HARNESS_ROOM_DISPLAY_NAMES_STORAGE_KEY, JSON.stringify(labels));
}

export function roomFallbackLabelForCredential(_code: string): string {
  // Friendly default; never the raw credential/technical ID (#42). Mirrors the
  // desktop `roomDisplayLabel` default.
  return 'Petal meeting';
}

export function roomDisplayLabelForCredential(code: string, storage: RoomLabelStorage | null = defaultStorage()): string {
  const key = normalizedCredential(code);
  const label = readLabelMap(storage)[key]?.trim();
  return label || roomFallbackLabelForCredential(code);
}

export function roomDisplayNameFromMetadata(metadata: string | null | undefined): string | null {
  if (!metadata) return null;
  try {
    const parsed = JSON.parse(metadata);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
    const displayName = (parsed as { displayName?: unknown }).displayName;
    return typeof displayName === 'string' && displayName.trim() ? displayName.trim() : null;
  } catch {
    return null;
  }
}

export function roomDisplayLabelForCredentialWithMetadata(
  code: string,
  metadata: string | null | undefined,
  storage: RoomLabelStorage | null = defaultStorage()
): string {
  return roomDisplayNameFromMetadata(metadata) || roomDisplayLabelForCredential(code, storage);
}

export function roomDisplayLabelForCredentialWithDisplayName(
  code: string,
  displayName: string | null | undefined,
  storage: RoomLabelStorage | null = defaultStorage()
): string {
  const cleaned = displayName?.trim();
  return cleaned || roomDisplayLabelForCredential(code, storage);
}

export function setRoomDisplayLabel(
  code: string,
  displayName: string | null,
  storage: RoomLabelStorage | null = defaultStorage()
): string {
  const key = normalizedCredential(code);
  if (!key) return roomFallbackLabelForCredential(code);

  const labels = readLabelMap(storage);
  const cleaned = displayName?.trim() ?? '';
  if (cleaned && cleaned !== roomFallbackLabelForCredential(code)) {
    labels[key] = cleaned;
  } else {
    delete labels[key];
  }
  writeLabelMap(storage, labels);
  return roomDisplayLabelForCredential(code, storage);
}
