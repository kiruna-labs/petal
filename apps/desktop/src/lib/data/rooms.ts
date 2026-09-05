// Real room-directory, presence, and join-flow client (SPEC.md §4.6). The
// frontend uses real Tauri calls for room listing, creation, joining, leaving,
// current-room lookup, and room presence; static room mocks have been retired.
import { invoke } from '@tauri-apps/api/core';
import type { IdentityColor } from '../components/Avatar.svelte';
import { IDENTITY_COLOR_HEX, PALETTE, colorForIdentity, identityColorCss } from './identityColor.ts';
import { roomAccessCode } from './roomAccessCode.ts';
import { COMMANDS } from '../ipc.ts';
import type {
  RemoteControlPolicy,
  PresentParticipant,
  PresenceUpdate,
  RoomOccupancy,
  RoomOccupancyParticipant,
  RoomRecord
} from '../ipc.ts';

export { PALETTE, colorForIdentity, identityColorCss, identityHeaderCss, identityInkCss } from './identityColor.ts';
export { paletteIndexForIdentityColor, identityColorFromPaletteIndex } from './identityColor.ts';
export type {
  PresentParticipant,
  PresenceUpdate,
  RoomOccupancy,
  RoomOccupancyParticipant,
  RoomRecord
} from '$lib/ipc';

/** List every durable room record known to this machine. */
export async function listRooms(): Promise<RoomRecord[]> {
  return invoke<RoomRecord[]>(COMMANDS.listRooms);
}

/**
 * Create a new durable room record. `open` defaults to `true` (SPEC.md
 * §4.6: "default open for internal eng rooms") -- callers that want a
 * knock-to-join room pass `false` explicitly.
 */
export async function createRoom(
  name: string,
  open = true,
  displayName: string | null = null
): Promise<RoomRecord> {
  return invoke<RoomRecord>(COMMANDS.createRoom, { name, open, displayName });
}

/** Set or clear a local-only display label. The room code/slug is unchanged. */
export async function renameRoom(
  idOrCode: string,
  displayName: string | null
): Promise<RoomRecord> {
  return invoke<RoomRecord>(COMMANDS.renameRoom, { idOrCode, displayName });
}

/** Forget one saved room from this machine's local room list. */
export async function forgetRoom(idOrCode: string): Promise<RoomRecord> {
  return invoke<RoomRecord>(COMMANDS.forgetRoom, { idOrCode });
}

/** The generic internal slug label pre-#42 code stamped as a real display
 * name for blank-created rooms (`RoomRecord.displayName === "room"`,
 * persisted to disk before the fix). Rooms created after the fix never get
 * this stamped; existing on-disk rooms still can, so the display layer must
 * keep treating it as "no name" rather than showing "room" as if it were a
 * real one. */
function isGenericRoomLabel(value: string | null | undefined): boolean {
  return (value?.trim().toLowerCase() ?? '') === 'room';
}

export function roomDisplayLabel(room: Pick<RoomRecord, 'name' | 'displayName' | 'accessCode'>): string {
  // Only a real display name; otherwise the friendly default. Never fall back
  // to the access code or the raw credential/slug — those read as technical IDs
  // (#42). A blank-created room has no display name and reads as "Petal meeting".
  const displayName = room.displayName?.trim();
  return displayName && !isGenericRoomLabel(displayName) ? displayName : 'Petal meeting';
}

/**
 * Return only a canonical, user-facing access code for a saved room. The
 * internal `room-<hex>` credential is deliberately never used as a fallback:
 * older records without a persisted access code simply have no code affordance
 * until the backend supplies one or this process still knows the runtime
 * mapping created during the current session.
 */
export { roomAccessCode } from './roomAccessCode.ts';

function hasLearnedRoomLabel(room: Pick<RoomRecord, 'displayName'>): boolean {
  const displayName = room.displayName?.trim();
  return Boolean(displayName && !isGenericRoomLabel(displayName));
}

/**
 * Query LiveKit server-side occupancy for every durable room this machine
 * knows about, without joining those rooms. When LiveKit API credentials or
 * the API itself are unavailable, each returned row is marked
 * `available: false` so callers can avoid falsely rendering "empty".
 */
export async function listRoomOccupancy(): Promise<RoomOccupancy[]> {
  return invoke<RoomOccupancy[]>(COMMANDS.listRoomOccupancy);
}

function normalizedKey(value: string | null | undefined): string | null {
  const normalized = value?.trim().toLowerCase();
  return normalized ? normalized : null;
}

function livekitRoomForCode(code: string | null | undefined): string | null {
  const normalized = normalizedKey(code);
  return normalized ? `petal-room-${normalized}` : null;
}

function roomCodeFromDirectoryRow(row: RoomOccupancy): string | null {
  return normalizedKey(row.slug);
}

function displayNameFromDirectoryRow(row: RoomOccupancy, code: string): string | null {
  const label = row.name?.trim();
  if (label && normalizedKey(label) !== normalizedKey(code)) return label;
  return null;
}

function indexRoom(index: Map<string, RoomRecord>, room: RoomRecord) {
  for (const key of [room.id, room.name, room.slug, livekitRoomForCode(room.name)]) {
    const normalized = normalizedKey(key);
    if (normalized && !index.has(normalized)) index.set(normalized, room);
  }
}

function directoryKeys(row: RoomOccupancy, code: string): string[] {
  return [row.id, row.roomName, row.slug, row.livekitRoom, code, livekitRoomForCode(code)]
    .map(normalizedKey)
    .filter((key): key is string => Boolean(key));
}

function localDirectoryKeys(row: RoomOccupancy): string[] {
  return [row.id, row.roomName, row.livekitRoom]
    .map(normalizedKey)
    .filter((key): key is string => Boolean(key));
}

function validLearnedDirectoryLabel(row: RoomOccupancy, room: RoomRecord): string | null {
  const label = row.name?.trim();
  if (!label || isGenericRoomLabel(label)) return null;
  const normalizedLabel = normalizedKey(label);
  const blocked = [
    room.id,
    room.name,
    room.slug,
    room.accessCode,
    row.id,
    row.slug,
    row.livekitRoom,
    livekitRoomForCode(room.name)
  ]
    .map(normalizedKey)
    .filter((key): key is string => Boolean(key));
  return normalizedLabel && !blocked.includes(normalizedLabel) ? label : null;
}

export interface RoomDisplayNameRepair {
  idOrCode: string;
  displayName: string;
}

export function roomDisplayNameRepairsFromDiscovery(
  localRooms: readonly RoomRecord[],
  occupancy: readonly RoomOccupancy[] | null
): RoomDisplayNameRepair[] {
  if (!occupancy) return [];

  const index = new Map<string, RoomRecord>();
  for (const room of localRooms) indexRoom(index, room);

  const repaired = new Set<string>();
  const repairs: RoomDisplayNameRepair[] = [];
  for (const row of occupancy) {
    if (row.available === false) continue;

    const room = localDirectoryKeys(row)
      .map((key) => index.get(key))
      .find((candidate): candidate is RoomRecord => Boolean(candidate));
    if (!room || hasLearnedRoomLabel(room)) continue;

    const repairKey = normalizedKey(room.id) ?? normalizedKey(room.name);
    if (!repairKey || repaired.has(repairKey)) continue;

    const displayName = validLearnedDirectoryLabel(row, room);
    if (!displayName) continue;

    repairs.push({ idOrCode: room.name, displayName });
    repaired.add(repairKey);
  }
  return repairs;
}

export async function persistRoomDisplayNameRepairsFromDiscovery(
  localRooms: readonly RoomRecord[],
  occupancy: readonly RoomOccupancy[] | null,
  persist: (idOrCode: string, displayName: string) => Promise<RoomRecord> = renameRoom,
  onError?: (error: unknown, repair: RoomDisplayNameRepair) => void
): Promise<RoomRecord[]> {
  const repairs = roomDisplayNameRepairsFromDiscovery(localRooms, occupancy);
  if (!repairs.length) return [...localRooms];

  const repairedByKey = new Map<string, RoomRecord>();
  for (const repair of repairs) {
    try {
      const updated = await persist(repair.idOrCode, repair.displayName);
      for (const key of [updated.id, updated.name, updated.slug, livekitRoomForCode(updated.name)]) {
        const normalized = normalizedKey(key);
        if (normalized) repairedByKey.set(normalized, updated);
      }
    } catch (error) {
      onError?.(error, repair);
    }
  }

  return localRooms.map((room) => {
    for (const key of [room.id, room.name, room.slug, livekitRoomForCode(room.name)]) {
      const normalized = normalizedKey(key);
      const repairedRoom = normalized ? repairedByKey.get(normalized) : undefined;
      if (repairedRoom) return repairedRoom;
    }
    return room;
  });
}

/**
 * Merge the local recents/favorites cache with live backend directory rows
 * returned by `list_room_occupancy`. Local records win on duplicate rooms. A
 * discovered-only row is joinable only when it carries an explicit credential;
 * public backend discovery no longer does (#83), so display-only rows are not
 * converted into clickable room records.
 */
export function mergeRoomsWithDiscovery(
  localRooms: readonly RoomRecord[],
  occupancy: readonly RoomOccupancy[] | null
): RoomRecord[] {
  const merged = [...localRooms];
  if (!occupancy) return merged;

  const index = new Map<string, RoomRecord>();
  for (const room of merged) indexRoom(index, room);

  for (const row of occupancy) {
    if (row.available === false) continue;

    const code = roomCodeFromDirectoryRow(row);
    if (!code) continue;

    const existing = directoryKeys(row, code).some((key) => index.has(key));
    if (existing) continue;

    const discovered: RoomRecord = {
      id: row.id?.trim() || `discovered:${row.livekitRoom || code}`,
      name: code,
      displayName: displayNameFromDirectoryRow(row, code),
      slug: row.slug?.trim() || code,
      createdAtMs: 0,
      lastJoinedMs: null,
      open: row.open ?? true
    };
    merged.push(discovered);
    indexRoom(index, discovered);
  }

  return merged;
}

/**
 * Join a room by name (SPEC.md §4.6's one-click join). `identity` should be
 * the real onboarding participant id (`session.participantId`), `displayName`
 * the real onboarding name -- see `session.svelte.ts`. Idempotent: joining a
 * room this process is already in is a clean no-op/reconnect on the Rust
 * side, never a duplicate membership or an error.
 */
export async function joinRoom(
  roomName: string,
  identity: string,
  displayName: string,
  remoteControlPolicy: RemoteControlPolicy = 'ask',
  identityPaletteIndex: number | null = null
): Promise<RoomRecord> {
  return invoke<RoomRecord>(
    COMMANDS.joinRoom,
    joinRoomCommandPayload(roomName, identity, displayName, remoteControlPolicy, identityPaletteIndex)
  );
}

/** `remoteControlPolicy` is authoritative; `remoteControlAllowed` is the
 * legacy boolean kept for an older Rust side (`off` -> false, else true). */
export function joinRoomCommandPayload(
  roomName: string,
  identity: string,
  displayName: string,
  remoteControlPolicy: RemoteControlPolicy = 'ask',
  identityPaletteIndex: number | null = null
) {
  return {
    roomName,
    identity,
    displayName,
    remoteControlAllowed: remoteControlPolicy !== 'off',
    remoteControlPolicy,
    identityPaletteIndex
  };
}

/** Leave the currently-joined room, if any. Idempotent. */
export async function leaveRoom(): Promise<void> {
  return invoke<void>(COMMANDS.leaveRoom);
}

/** The real durable room name this process is currently joined to, if any. */
export async function currentRoom(): Promise<string | null> {
  return invoke<string | null>(COMMANDS.currentRoom);
}

/** One-shot presence snapshot for whichever room is currently joined. */
export async function roomPresence(): Promise<PresentParticipant[]> {
  return invoke<PresentParticipant[]>(COMMANDS.roomPresence);
}

export interface MeetingColorParticipant {
  identity: string;
  baseColor?: IdentityColor | null;
}

interface HslColor {
  h: number;
  s: number;
  l: number;
}

const COLLISION_TINT_STEPS = [
  { h: 0, s: 0, l: 0 },
  { h: -4, s: 8, l: -14 },
  { h: 4, s: -4, l: 12 },
  { h: 7, s: 10, l: -24 },
  { h: -7, s: -8, l: 22 }
] as const;

/**
 * Resolve the concrete identity color every client should render for a
 * meeting roster. Participants sharing the same base color are ordered by the
 * stable shared identity, then assigned deterministic same-family tints.
 */
export function resolveMeetingColors(
  participants: readonly MeetingColorParticipant[]
): Map<string, string> {
  const byIdentity = new Map<string, { identity: string; baseColor: IdentityColor }>();
  for (const participant of participants) {
    if (!participant.identity || byIdentity.has(participant.identity)) continue;
    byIdentity.set(participant.identity, {
      identity: participant.identity,
      baseColor: participant.baseColor ?? colorForIdentity(participant.identity)
    });
  }

  const groups = new Map<IdentityColor, { identity: string; baseColor: IdentityColor }[]>();
  for (const participant of byIdentity.values()) {
    const group = groups.get(participant.baseColor) ?? [];
    group.push(participant);
    groups.set(participant.baseColor, group);
  }

  const usedBaseColors = new Set(groups.keys());
  const resolved = new Map<string, string>();
  for (const baseColor of PALETTE) {
    const group = groups.get(baseColor);
    if (!group) continue;
    group.sort(compareIdentity);
    for (let index = 0; index < group.length; index++) {
      resolved.set(
        group[index].identity,
        colorVariantForCollision(baseColor, index, usedBaseColors)
      );
    }
  }
  return resolved;
}

export function rosterFromPresence<
  T extends {
    identity: string;
    name: string;
    isLocal?: boolean;
    speaking?: boolean;
    micMuted?: boolean;
  }
>(
  participants: readonly T[],
  options: { markLocalName?: boolean; localMicMuted?: boolean } = {}
): Array<{
  name: string;
  identity: IdentityColor;
  resolvedColor: string;
  isYou?: boolean;
  muted?: boolean;
  speaking?: boolean;
}> {
  const resolvedColors = resolveMeetingColors(participants);
  return participants.map((participant) => {
    const identity = colorForIdentity(participant.identity);
    const muted =
      participant.isLocal && options.localMicMuted !== undefined
        ? options.localMicMuted
        : participant.micMuted;
    return {
      name:
        options.markLocalName && participant.isLocal
          ? `${participant.name} (you)`
          : participant.name,
      identity,
      resolvedColor: resolvedColors.get(participant.identity) ?? identityColorCss(identity),
      muted,
      speaking: participant.speaking && !muted,
      isYou: participant.isLocal
    };
  });
}

function compareIdentity(
  a: { identity: string },
  b: { identity: string }
): number {
  if (a.identity < b.identity) return -1;
  if (a.identity > b.identity) return 1;
  return 0;
}

function colorVariantForCollision(
  baseColor: IdentityColor,
  collisionIndex: number,
  usedBaseColors: ReadonlySet<IdentityColor>
): string {
  if (collisionIndex < COLLISION_TINT_STEPS.length) {
    return tintColor(baseColor, COLLISION_TINT_STEPS[collisionIndex]);
  }

  const fallbackBases = PALETTE.filter((color) => !usedBaseColors.has(color));
  if (fallbackBases.length === 0) {
    return tintColor(baseColor, COLLISION_TINT_STEPS[COLLISION_TINT_STEPS.length - 1]);
  }

  const overflowIndex = collisionIndex - COLLISION_TINT_STEPS.length;
  const fallbackBase = fallbackBases[overflowIndex % fallbackBases.length];
  const fallbackTintIndex = Math.floor(overflowIndex / fallbackBases.length);
  return tintColor(
    fallbackBase,
    COLLISION_TINT_STEPS[Math.min(fallbackTintIndex, COLLISION_TINT_STEPS.length - 1)]
  );
}

function tintColor(
  baseColor: IdentityColor,
  step: (typeof COLLISION_TINT_STEPS)[number]
): string {
  if (step.h === 0 && step.s === 0 && step.l === 0) return IDENTITY_COLOR_HEX[baseColor];
  const hsl = hexToHsl(IDENTITY_COLOR_HEX[baseColor]);
  return hslToHex({
    h: wrapHue(hsl.h + step.h),
    s: clamp(hsl.s + step.s, 42, 96),
    l: clamp(hsl.l + step.l, 38, 90)
  });
}

function hexToHsl(hex: string): HslColor {
  const r = Number.parseInt(hex.slice(1, 3), 16) / 255;
  const g = Number.parseInt(hex.slice(3, 5), 16) / 255;
  const b = Number.parseInt(hex.slice(5, 7), 16) / 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return { h: 0, s: 0, l: l * 100 };

  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h = 0;
  if (max === r) h = (g - b) / d + (g < b ? 6 : 0);
  else if (max === g) h = (b - r) / d + 2;
  else h = (r - g) / d + 4;
  return { h: (h / 6) * 360, s: s * 100, l: l * 100 };
}

function hslToHex({ h, s, l }: HslColor): string {
  const normalizedS = s / 100;
  const normalizedL = l / 100;
  const c = (1 - Math.abs(2 * normalizedL - 1)) * normalizedS;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = normalizedL - c / 2;
  let r = 0;
  let g = 0;
  let b = 0;

  if (h < 60) [r, g, b] = [c, x, 0];
  else if (h < 120) [r, g, b] = [x, c, 0];
  else if (h < 180) [r, g, b] = [0, c, x];
  else if (h < 240) [r, g, b] = [0, x, c];
  else if (h < 300) [r, g, b] = [x, 0, c];
  else [r, g, b] = [c, 0, x];

  return `#${toHexByte(r + m)}${toHexByte(g + m)}${toHexByte(b + m)}`;
}

function toHexByte(value: number): string {
  return Math.round(clamp(value, 0, 1) * 255)
    .toString(16)
    .padStart(2, '0');
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function wrapHue(value: number): number {
  return ((value % 360) + 360) % 360;
}
