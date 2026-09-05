// LiveKit server-side helpers. The API secret lives ONLY here (server env),
// never in a client — this whole backend exists to keep it off user machines
// (issue #96). Do not add an endpoint that returns the secret.
//
// MINIMAL-BACKEND DESIGN (per product directive 2026-07-02): LiveKit is the
// ONLY state store. There is NO database. Room discovery uses LiveKit's own
// `listRooms()`, and the human-readable room name is carried in the LiveKit
// room's `metadata`. A named room exists on LiveKit while it is active (and for
// its emptyTimeout after the last person leaves); it is always re-joinable by
// its deterministic slug regardless. Favorites/recents live locally on the
// client, not here.

import { AccessToken, RoomServiceClient, type Room } from 'livekit-server-sdk';

export interface LiveKitEnv {
  url: string; // ws:// or wss:// signaling URL handed to clients
  apiKey: string;
  apiSecret: string;
}

export class LiveKitConfigError extends Error {
  constructor(message = 'LiveKit not configured') {
    super(message);
    this.name = 'LiveKitConfigError';
  }
}

export function loadLiveKitEnv(): LiveKitEnv {
  const url = process.env.LIVEKIT_URL;
  const apiKey = process.env.LIVEKIT_API_KEY;
  const apiSecret = process.env.LIVEKIT_API_SECRET;
  if (!url || !apiKey || !apiSecret) {
    throw new LiveKitConfigError(
      'LiveKit not configured: set LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET'
    );
  }
  return { url, apiKey, apiSecret };
}

// ws://host:port -> http://host:port ; wss://host/path?q -> https://host
// Mirrors the native `rooms::livekit_api_host_from_url` (strip path/query,
// swap the scheme) so the admin/Twirp host is derived identically.
export function httpHostFromUrl(signalUrl: string): string {
  const u = new URL(signalUrl);
  const scheme = u.protocol === 'wss:' ? 'https:' : u.protocol === 'ws:' ? 'http:' : u.protocol;
  return `${scheme}//${u.host}`;
}

export interface MintOptions {
  room: string; // the derived LiveKit room name (petal-room-<credential>)
  identity: string;
  displayName?: string;
  canPublish?: boolean;
  canSubscribe?: boolean;
  canPublishData?: boolean;
  hidden?: boolean;
  ttl?: string; // e.g. "24h"
}

export const DEFAULT_TOKEN_TTL = '24h';

// Mint a scoped LiveKit JWT. Grants mirror the native
// `transport::token::mint_access_token` exactly: room_join + this specific
// room, publish/subscribe as requested, and can_publish_data (telepointers).
export async function mintToken(env: LiveKitEnv, opts: MintOptions): Promise<string> {
  const at = new AccessToken(env.apiKey, env.apiSecret, {
    identity: opts.identity,
    name: opts.displayName ?? opts.identity,
    ttl: opts.ttl ?? DEFAULT_TOKEN_TTL,
  });
  at.addGrant({
    roomJoin: true,
    room: opts.room,
    canPublish: opts.canPublish ?? true,
    canSubscribe: opts.canSubscribe ?? true,
    canPublishData: opts.canPublishData ?? true,
    canUpdateOwnMetadata: true,
    hidden: opts.hidden ?? false,
  });
  return at.toJwt();
}

export function roomService(env: LiveKitEnv): RoomServiceClient {
  return new RoomServiceClient(httpHostFromUrl(env.url), env.apiKey, env.apiSecret);
}

// LiveKit Cloud's Twirp RPC transport occasionally returns a transient
// `503 Service Unavailable: no response from servers` (the SDK's TwirpError
// carries `status: 503` and `code: 'unavailable'`) for an otherwise-healthy
// RoomServiceClient call -- confirmed via Sentry breadcrumbs (#708,
// PETAL-BACKEND-3/2): `ListRooms` succeeds immediately before `ListParticipants`
// 503s for one room in the set. One short retry absorbs that blip instead of
// surfacing it to the caller.
const LIVEKIT_RETRYABLE_STATUS = 503;
const LIVEKIT_RETRYABLE_CODE = 'unavailable';
const LIVEKIT_RETRY_DELAY_MS = 150;

function isRetryableLiveKitError(err: unknown): boolean {
  const status = (err as { status?: unknown } | null | undefined)?.status;
  const code = (err as { code?: unknown } | null | undefined)?.code;
  return status === LIVEKIT_RETRYABLE_STATUS || code === LIVEKIT_RETRYABLE_CODE;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Bounded retry for a single RoomServiceClient RPC. Retries ONCE (by default
// -- `attempts` counts total tries, not extra retries) on a transient LiveKit
// Twirp 503/`unavailable`, waiting `delayMs` between attempts, then rethrows
// whatever the LAST attempt produced unchanged -- callers see the exact same
// error shape/status they always did, just after one more try first. Any
// non-retryable error rethrows immediately with no delay.
export async function withLiveKitRetry<T>(
  fn: () => Promise<T>,
  attempts = 2,
  delayMs = LIVEKIT_RETRY_DELAY_MS
): Promise<T> {
  let lastErr: unknown;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      return await fn();
    } catch (err) {
      lastErr = err;
      if (attempt >= attempts || !isRetryableLiveKitError(err)) {
        throw err;
      }
      await delay(delayMs);
    }
  }
  // Unreachable (the loop always returns or throws), but keeps the return
  // type total for TypeScript.
  throw lastErr;
}

export interface RoomMetadataService {
  createRoom: RoomServiceClient['createRoom'];
  listRooms: RoomServiceClient['listRooms'];
  updateRoomMetadata(room: string, metadata: string): Promise<Room>;
}

// Admin control needs the room list + metadata write as well as the two
// control RPCs: a kick is recorded in room metadata so it survives a rejoin
// (see `RoomMeta.removed`).
export type RoomAdminService = Pick<RoomServiceClient, 'deleteRoom' | 'removeParticipant' | 'listRooms'> & {
  updateRoomMetadata(room: string, metadata: string): Promise<Room>;
};
export type RoomDiscoveryService = Pick<RoomServiceClient, 'listParticipants'>;
// Seam for room DISCOVERY (`handleListRooms`). ONE RPC: `listRooms` already
// carries `numParticipants` per room (hidden participants excluded), so the
// per-room `listParticipants` fan-out this used to require is gone -- it made
// the cost of an unauthenticated GET scale with the number of live rooms and
// was the backend's cheapest DoS. Mirrors
// `CreateRoomContext.service`'s injection pattern for tests (#708).
export type RoomListingService = Pick<RoomServiceClient, 'listRooms'>;

// The metadata we stash on a LiveKit room so discovery can show a human name
// and the knock setting without a database.
export interface RoomMeta {
  displayName: string;
  open: boolean;
  // Identities an admin kicked from this room. `/api/token` refuses them for
  // as long as the LiveKit room (and so this metadata) exists. Bounded to
  // ROOM_META_REMOVED_LIMIT, oldest dropped first, so metadata stays small.
  removed?: string[];
}

export const ROOM_META_REMOVED_LIMIT = 64;

export function encodeRoomMeta(meta: RoomMeta): string {
  const removed = meta.removed?.slice(-ROOM_META_REMOVED_LIMIT);
  return JSON.stringify({
    displayName: meta.displayName,
    open: meta.open,
    ...(removed && removed.length ? { removed } : {}),
  });
}

// Pull the fields a metadata REWRITE must carry forward from an existing
// room: the removed-identity list is server-owned state no client ever sends,
// so any stamp that rebuilt metadata from the request alone would silently
// un-kick everyone.
export function preservedRoomMeta(existing: Partial<RoomMeta>): Pick<RoomMeta, 'removed'> {
  const removed = Array.isArray(existing.removed)
    ? existing.removed.filter((value): value is string => typeof value === 'string')
    : [];
  return removed.length ? { removed } : {};
}

export function decodeRoomMeta(raw: string | undefined): Partial<RoomMeta> {
  if (!raw) return {};
  try {
    return JSON.parse(raw) as Partial<RoomMeta>;
  } catch {
    return {};
  }
}

function isAlreadyExistsError(err: unknown): boolean {
  const message = err instanceof Error ? err.message : String(err);
  return /already exists|already_exist|already-exist/i.test(message);
}

async function findRoomMetadata(
  service: RoomMetadataService,
  livekitRoom: string
): Promise<string | undefined> {
  const rooms = await service.listRooms();
  return rooms.find((room) => room.name === livekitRoom)?.metadata;
}

export interface EnsureRoomOptions {
  preserveOpenOnExisting?: boolean;
}

// Create (or return the existing) LiveKit room, stamping the human display
// name + knock setting as metadata. Existing native-credential stamps refresh
// the display name but preserve the server-side knock gate, and EVERY
// rewrite of an existing room's metadata carries its removed-identity list
// forward (`preservedRoomMeta`).
export async function ensureRoom(
  env: LiveKitEnv,
  livekitRoom: string,
  meta: RoomMeta,
  service: RoomMetadataService = roomService(env),
  options: EnsureRoomOptions = {}
): Promise<Room> {
  const metadata = encodeRoomMeta(meta);
  const restamp = (existingRaw: string | undefined): string => {
    const existing = decodeRoomMeta(existingRaw);
    return encodeRoomMeta({
      ...meta,
      // #203: native rejoin stamps can carry stale local open=true. Once the
      // LiveKit room exists, keep its knock-gate flag and only refresh the
      // display label; use the request value only when metadata is absent.
      open: options.preserveOpenOnExisting ? existing.open ?? meta.open : meta.open,
      ...preservedRoomMeta(existing),
    });
  };
  try {
    const room = await service.createRoom({
      name: livekitRoom,
      metadata,
      // Keep an empty named room around briefly so a creator who hasn't joined
      // yet still shows in discovery; it is always re-joinable by slug anyway.
      emptyTimeout: 5 * 60,
    });
    if (room.metadata === metadata) return room;
    // Newer LiveKit servers return the EXISTING room from createRoom instead
    // of raising already-exists; treat its metadata as the authority to merge
    // from, exactly like the catch path below.
    const merged = restamp(room.metadata);
    return merged === room.metadata ? room : service.updateRoomMetadata(livekitRoom, merged);
  } catch (err) {
    if (!isAlreadyExistsError(err)) throw err;
    return service.updateRoomMetadata(
      livekitRoom,
      restamp(await findRoomMetadata(service, livekitRoom))
    );
  }
}

// All currently-active Petal rooms on the server (name starts with petal-room-).
// `service` defaults to a real client but is injectable so
// `handleListRooms` can share ONE service (real or mocked) across both this
// call and its per-room listParticipants calls.
export async function listPetalRooms(
  env: LiveKitEnv,
  service: Pick<RoomServiceClient, 'listRooms'> = roomService(env)
): Promise<Room[]> {
  const rooms = await withLiveKitRetry(() => service.listRooms());
  return rooms.filter((r) => r.name.startsWith('petal-room-'));
}
