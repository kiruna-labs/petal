// Pure request handlers (framework-agnostic) so they can be unit-tested
// directly (test/local.ts) AND wrapped by thin Vercel adapters (api/*.ts).
//
// No database: rooms live entirely on LiveKit (see lib/livekit.ts). Discovery
// reads LiveKit's active-room list; the human name rides in room metadata.

import { timingSafeEqual } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { ParticipantInfo_State, TokenVerifier, type ParticipantInfo } from 'livekit-server-sdk';
import {
  credentialForAccessCode,
  generateRoomCredential,
  livekitRoomName,
  normalizeRoomCredential,
  roomLabelFromCredential,
} from './slug.js';
import { MemoryRateLimitStore, type RateBucket, type RateLimitStore } from './ratelimit.js';
import {
  loadLiveKitEnv,
  mintToken,
  ensureRoom,
  listPetalRooms,
  decodeRoomMeta,
  encodeRoomMeta,
  preservedRoomMeta,
  roomService,
  withLiveKitRetry,
  ROOM_META_REMOVED_LIMIT,
  type LiveKitEnv,
  type RoomAdminService,
  type RoomDiscoveryService,
  type RoomListingService,
  type RoomMetadataService,
} from './livekit.js';
import {
  AI_TOKEN_LIFETIME_MS,
  AI_TOKEN_NEW_SESSION_WINDOW_MS,
  AI_TOKEN_RESPONSE_MODALITY,
  AI_TOKEN_USES,
  loadGeminiEnv,
  mintGeminiEphemeralToken,
  type GeminiTokenMinter,
} from './gemini.js';

export interface TokenRequest {
  room: string; // full room credential: <human-slug>-<128-bit hex capability>
  identity: string;
  displayName?: string;
  // The short invite access code (`abc-defg-hij`). REQUIRED when the room's
  // metadata says `open: false` (knock-to-join): the credential is a one-way
  // hash of this code and is visible in LiveKit room names, JWT claims and
  // logs, while the code only ever travels inside an invite — so for a closed
  // room the backend demands the pre-image, not just the hash. Ignored for
  // open rooms (see docs/CONTRACTS.md "Closed rooms and removed participants").
  accessCode?: string;
  // Accepted only for backwards-compatible JSON parsing. The public endpoint
  // always clamps grants below; hidden/subscribe-only tokens need a trusted
  // server-owned path, not caller-controlled request fields (#100).
  canPublish?: boolean;
  canSubscribe?: boolean;
  canPublishData?: boolean;
  hidden?: boolean;
}

export interface TokenResponse {
  url: string; // signaling URL for the client to connect to
  token: string;
  room: string; // the derived LiveKit room name actually joined
  displayName?: string; // human room name from LiveKit room metadata
}

export interface TokenRequestContext {
  rateLimitKey?: string;
  nowMs?: number;
  service?: RoomMetadataService;
}

// Trusted, server-owned path for the desktop app's hidden "gallery bridge"
// participant (#109/#26) -- a SECOND, hidden/subscribe-only LiveKit connection
// the webview uses to receive remote camera video (native compositor handles
// only petal-window-* share tracks). The public /api/token endpoint always
// clamps hidden/grant fields (#100) and rejects non-generated identities, so
// the `<identity>-gallery` bridge identity can never get a usable token
// there -- this endpoint is the intentional, narrowly-scoped alternative.
export interface GalleryTokenRequest {
  room: string; // full room credential
  // The caller's OWN visible-participant identity (no suffix). The bridge
  // identity is derived server-side, never caller-supplied.
  baseIdentity: string;
  displayName?: string;
}

export interface GalleryTokenContext {
  rateLimitKey?: string;
  nowMs?: number;
  service?: RoomDiscoveryService;
}

export interface RequestContext {
  rateLimitKey?: string;
  nowMs?: number;
  // Test seam mirroring CreateRoomContext.service (#708): production always
  // uses a real RoomServiceClient via roomService(env).
  service?: RoomListingService;
}

export interface AdminControlRequest {
  action: 'kick' | 'close';
  room: string;
  identity?: string;
}

export interface AdminControlContext {
  authorization?: string;
  service?: RoomAdminService;
}

const TOKEN_FIELD_LIMIT = 128;
const TOKEN_BUCKET_CAPACITY = 20;
const TOKEN_BUCKET_REFILL_MS = 60_000;
const ROOMS_BUCKET_CAPACITY = 60;
const ROOMS_BUCKET_REFILL_MS = 60_000;
// Room CREATION gets its own, much smaller bucket than discovery: a create is
// unauthenticated (see docs/CONTRACTS.md "Room directory access") and each one
// materialises a LiveKit room that sits in the public directory for its
// emptyTimeout, so it is the cheapest way to spam every client's room list.
// Discovery reads are cached (ROOMS_LIST_CACHE_MS) and cost nothing upstream.
export const ROOM_CREATE_BUCKET_CAPACITY = 20;
export const ROOM_CREATE_BUCKET_REFILL_MS = 10 * 60_000;
// Instance-wide ceiling on creates regardless of source: bounds directory
// spam from a caller rotating addresses. Per-instance like every other bucket
// (see lib/ratelimit.ts for the shared-store seam).
export const ROOM_CREATE_GLOBAL_CAPACITY = 120;
const ROOM_CREATE_GLOBAL_KEY = '\u0000global';
// How long one instance serves a cached /api/rooms GET before asking LiveKit
// again. Short enough that occupancy feels live, long enough that a burst of
// clients (or an abuser) costs one upstream listRooms per window, not one per
// request.
export const ROOMS_LIST_CACHE_MS = 3_000;

// AI-chat token minting costs real money upstream, so it gets its OWN buckets
// rather than sharing /api/token's — an AI-chat burst must never be able to
// lock a participant out of joining a meeting (#655).
//
// THREE buckets, two different jobs, because "how much did you spend" and "how
// hard are you hammering us" are different questions and one bucket answered
// both badly:
//
//  * The two SPEND buckets below are charged only when a mint actually
//    succeeded. Charging them up front billed a caller for work they never
//    received — a failed liveness check or a Google outage silently ate hourly
//    slots and then answered "Too many AI chat sessions just now", which was
//    both false and unactionable.
//  * The ATTEMPT bucket is charged on entry for EVERY request, successes and
//    failures alike. It is what keeps "always fail" from being an unlimited
//    free probe now that failures cost no spend slot. Deliberately much larger
//    than the spend caps: it must never be the thing a human meets when their
//    mints are merely failing, only a flood.
export const AI_TOKEN_IDENTITY_BUCKET_CAPACITY = 6;
export const AI_TOKEN_IP_BUCKET_CAPACITY = 60;
export const AI_TOKEN_ATTEMPT_BUCKET_CAPACITY = 120;
const AI_TOKEN_BUCKET_REFILL_MS = 60 * 60_000;

// One store per limit; each store's TTL is its limit's refill window (a bucket
// untouched that long is full again, i.e. identical to absent). Replace these
// via `configureRateLimitStores` to back them with a shared store.
interface RateLimitStores {
  token: RateLimitStore;
  rooms: RateLimitStore;
  roomCreate: RateLimitStore;
  aiTokenIp: RateLimitStore;
  aiTokenIdentity: RateLimitStore;
  aiTokenAttempt: RateLimitStore;
}

function defaultRateLimitStores(): RateLimitStores {
  return {
    token: new MemoryRateLimitStore({ ttlMs: TOKEN_BUCKET_REFILL_MS }),
    rooms: new MemoryRateLimitStore({ ttlMs: ROOMS_BUCKET_REFILL_MS }),
    roomCreate: new MemoryRateLimitStore({ ttlMs: ROOM_CREATE_BUCKET_REFILL_MS }),
    aiTokenIp: new MemoryRateLimitStore({ ttlMs: AI_TOKEN_BUCKET_REFILL_MS }),
    aiTokenIdentity: new MemoryRateLimitStore({ ttlMs: AI_TOKEN_BUCKET_REFILL_MS }),
    aiTokenAttempt: new MemoryRateLimitStore({ ttlMs: AI_TOKEN_BUCKET_REFILL_MS }),
  };
}

let stores: RateLimitStores = defaultRateLimitStores();

// Swap in shared-store implementations (all or a subset). Intended to be
// called once at module init by a deployment that wires Vercel KV / Redis.
export function configureRateLimitStores(overrides: Partial<RateLimitStores>): void {
  stores = { ...stores, ...overrides };
}

// Test-only view of the in-memory stores' residency, so tests can prove
// eviction actually happens rather than trusting the comment above.
export function rateLimitStoreSizesForTest(): Record<keyof RateLimitStores, number | undefined> {
  const size = (store: RateLimitStore) =>
    store instanceof MemoryRateLimitStore ? store.size : undefined;
  return {
    token: size(stores.token),
    rooms: size(stores.rooms),
    roomCreate: size(stores.roomCreate),
    aiTokenIp: size(stores.aiTokenIp),
    aiTokenIdentity: size(stores.aiTokenIdentity),
    aiTokenAttempt: size(stores.aiTokenAttempt),
  };
}

const GENERATED_PARTICIPANT_ID =
  /^(?:web-)?[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$|^p-[a-z0-9]+-[a-z0-9]+$/i;

function assertBoundedString(
  value: unknown,
  field: string,
  { required }: { required: boolean }
): string | undefined {
  if (typeof value !== 'string') {
    if (required) throw new HttpError(400, `${field} is required`);
    return undefined;
  }
  const trimmed = value.trim();
  if (!trimmed) {
    if (required) throw new HttpError(400, `${field} is required`);
    return undefined;
  }
  if (trimmed.length > TOKEN_FIELD_LIMIT) {
    throw new HttpError(400, `${field} must be ${TOKEN_FIELD_LIMIT} characters or fewer`);
  }
  return trimmed;
}

export async function resetTokenRateLimitsForTest() {
  await Promise.all(Object.values(stores).map((store) => store.clear()));
  resetRoomsListCacheForTest();
}

// Best-effort limiter: per-instance unless a shared store is configured (see
// lib/ratelimit.ts). Bucket keys stay in the store and are NEVER logged or
// returned, so the ai-token identity+room key below does not weaken the
// #128/#218 privacy posture.
async function refillBucket(
  store: RateLimitStore,
  key: string,
  nowMs: number,
  capacity: number,
  refillMs: number
): Promise<RateBucket> {
  const current = await store.get(key);
  const bucket = current ?? { tokens: capacity, updatedAt: nowMs };
  const elapsed = Math.max(0, nowMs - bucket.updatedAt);
  bucket.tokens = Math.min(capacity, bucket.tokens + (elapsed / refillMs) * capacity);
  bucket.updatedAt = nowMs;
  await store.set(key, bucket, nowMs);
  return bucket;
}

// Admission check that spends NOTHING. Pairs with `spendRateLimit` below for
// work that must only be charged once it has actually been delivered.
async function assertRateLimitAvailable(
  store: RateLimitStore,
  key: string | undefined,
  nowMs: number,
  capacity: number,
  refillMs: number,
  message = 'rate limit exceeded'
): Promise<void> {
  if (!key) return;
  if ((await refillBucket(store, key, nowMs, capacity, refillMs)).tokens < 1) {
    throw new HttpError(429, message);
  }
}

// Record the charge. Never throws: admission was already decided and the work
// is already done, so refusing here would only lose the accounting. Clamped at
// zero so two requests racing the same peek overshoot by at most one slot
// instead of driving the bucket negative and locking the caller out for longer
// than the stated capacity.
async function spendRateLimit(
  store: RateLimitStore,
  key: string | undefined,
  nowMs: number,
  capacity: number,
  refillMs: number
): Promise<void> {
  if (!key) return;
  const bucket = await refillBucket(store, key, nowMs, capacity, refillMs);
  bucket.tokens = Math.max(0, bucket.tokens - 1);
  await store.set(key, bucket, nowMs);
}

async function enforceRateLimit(
  store: RateLimitStore,
  key: string | undefined,
  nowMs: number,
  capacity: number,
  refillMs: number,
  message = 'rate limit exceeded'
): Promise<void> {
  if (!key) return;
  await assertRateLimitAvailable(store, key, nowMs, capacity, refillMs, message);
  await spendRateLimit(store, key, nowMs, capacity, refillMs);
}

function enforceTokenRateLimit(key: string | undefined, nowMs: number): Promise<void> {
  return enforceRateLimit(stores.token, key, nowMs, TOKEN_BUCKET_CAPACITY, TOKEN_BUCKET_REFILL_MS);
}

function enforceRoomsRateLimit(key: string | undefined, nowMs: number): Promise<void> {
  return enforceRateLimit(stores.rooms, key, nowMs, ROOMS_BUCKET_CAPACITY, ROOMS_BUCKET_REFILL_MS);
}

async function enforceRoomCreateRateLimit(key: string | undefined, nowMs: number): Promise<void> {
  await enforceRateLimit(
    stores.roomCreate,
    key,
    nowMs,
    ROOM_CREATE_BUCKET_CAPACITY,
    ROOM_CREATE_BUCKET_REFILL_MS,
    'room creation rate limit exceeded'
  );
  // The global ceiling is keyed on a constant so it applies even to callers
  // without a source key (never the case in production; keeps tests honest).
  await enforceRateLimit(
    stores.roomCreate,
    ROOM_CREATE_GLOBAL_KEY,
    nowMs,
    ROOM_CREATE_GLOBAL_CAPACITY,
    ROOM_CREATE_BUCKET_REFILL_MS,
    'room creation is temporarily unavailable'
  );
}

function assertGeneratedParticipantIdentity(identity: string): string {
  if (!GENERATED_PARTICIPANT_ID.test(identity)) {
    throw new HttpError(400, 'identity must be a generated participant id');
  }
  return identity;
}

function assertAdminAuthorization(authorization: string | undefined): void {
  const expected = process.env.PETAL_ADMIN_TOKEN?.trim();
  if (!expected) {
    throw new HttpError(503, 'admin control is not configured');
  }
  const match = authorization?.match(/^Bearer\s+(.+)$/i);
  if (!match?.[1]) {
    throw new HttpError(401, 'admin authorization is required');
  }
  const received = Buffer.from(match[1]);
  const target = Buffer.from(expected);
  if (received.length !== target.length || !cryptoSafeEqual(received, target)) {
    throw new HttpError(403, 'admin authorization failed');
  }
}

// livekit-server answers `removeParticipant` for an unknown identity with a
// Twirp `not_found` ("participant does not exist"); LiveKit Cloud phrases it
// the same way. Match both the code and the message so neither transport's
// wording alone is load-bearing.
function isParticipantMissingError(err: unknown): boolean {
  const code = (err as { code?: unknown } | null | undefined)?.code;
  const message = err instanceof Error ? err.message : String(err);
  return code === 'not_found' || /participant does not exist|participant not found/i.test(message);
}

function cryptoSafeEqual(a: Buffer, b: Buffer): boolean {
  try {
    return timingSafeEqual(a, b);
  } catch {
    return false;
  }
}

export async function handleToken(
  body: Partial<TokenRequest>,
  context: TokenRequestContext = {}
): Promise<TokenResponse> {
  if (!body || typeof body.room !== 'string' || !body.room.trim()) {
    throw new HttpError(400, 'room is required');
  }
  await enforceTokenRateLimit(context.rateLimitKey, context.nowMs ?? Date.now());
  const requestedRoom = assertBoundedString(body.room, 'room', { required: true })!;
  const identity = assertGeneratedParticipantIdentity(
    assertBoundedString(body.identity, 'identity', { required: true })!
  );
  const displayName = assertBoundedString(body.displayName, 'displayName', { required: false });
  const accessCode = assertBoundedString(body.accessCode, 'accessCode', { required: false });
  const env = loadLiveKitEnv();
  const credential = normalizeRoomCredential(requestedRoom);
  if (!credential) {
    throw new HttpError(400, 'room credential is required');
  }
  const room = livekitRoomName(credential);

  // Room metadata is the authority for two refusals, and for the display name
  // the browser needs before it can render the meeting header:
  //   * `open: false` -- knock-to-join. The credential alone is not enough:
  //     demand the invite's access code, whose one-way hash the credential is.
  //   * `removed` -- identities an admin kicked:
  //     a kick used to be a one-shot `removeParticipant`, and the same token
  //     request rejoined immediately.
  // Lookup stays best-effort for AVAILABILITY (rooms created by older clients
  // carry no metadata; a room past its emptyTimeout has none either) -- a
  // room with no readable metadata is treated as open with nobody removed.
  let roomMeta: Partial<import('./livekit.js').RoomMeta> = {};
  try {
    const service = context.service ?? roomService(env);
    const rooms = await withLiveKitRetry(() => service.listRooms());
    roomMeta = decodeRoomMeta(rooms.find((candidate) => candidate.name === room)?.metadata);
  } catch {
    roomMeta = {};
  }
  if (roomMeta.open === false) {
    const provenCredential = accessCode ? credentialForAccessCode(accessCode) : null;
    if (!provenCredential || provenCredential !== credential) {
      throw new HttpError(403, 'room is closed: a valid access code is required');
    }
  }
  if (Array.isArray(roomMeta.removed) && roomMeta.removed.includes(identity)) {
    throw new HttpError(403, 'participant was removed from this room');
  }

  const token = await mintToken(env, {
    room,
    identity,
    displayName,
    canPublish: true,
    canSubscribe: true,
    canPublishData: true,
    hidden: false,
  });
  const roomDisplayName = roomMeta.displayName?.trim() || undefined;
  return {
    url: env.url,
    token,
    room,
    ...(roomDisplayName ? { displayName: roomDisplayName } : {}),
  };
}

const GALLERY_IDENTITY_SUFFIX = '-gallery';

export async function handleGalleryToken(
  body: Partial<GalleryTokenRequest>,
  context: GalleryTokenContext = {}
): Promise<TokenResponse> {
  if (!body || typeof body.room !== 'string' || !body.room.trim()) {
    throw new HttpError(400, 'room is required');
  }
  await enforceTokenRateLimit(context.rateLimitKey, context.nowMs ?? Date.now());
  const requestedRoom = assertBoundedString(body.room, 'room', { required: true })!;
  const baseIdentity = assertGeneratedParticipantIdentity(
    assertBoundedString(body.baseIdentity, 'baseIdentity', { required: true })!
  );
  if (baseIdentity.endsWith(GALLERY_IDENTITY_SUFFIX)) {
    // Reject the request rather than silently double-suffixing -- a caller
    // passing an already-bridged identity is either confused or probing.
    throw new HttpError(400, 'baseIdentity must be the visible participant identity');
  }
  const displayName = assertBoundedString(body.displayName, 'displayName', { required: false });
  const env = loadLiveKitEnv();
  const credential = normalizeRoomCredential(requestedRoom);
  if (!credential) {
    throw new HttpError(400, 'room credential is required');
  }
  const room = livekitRoomName(credential);

  // Trust anchor (#109): only mint a hidden bridge token for a caller who is
  // ALREADY a real, currently-connected participant in THIS exact room. A
  // caller can't get one for a room they haven't legitimately joined, or
  // impersonating someone else's identity -- their own live connection is
  // what proves the identity, not this request. Any failure (room doesn't
  // exist, identity not present, LiveKit unreachable) collapses to the same
  // 403 so the response never leaks which case it was.
  const service = context.service ?? roomService(env);
  const authorized = await withLiveKitRetry(() => service.listParticipants(room))
    .then((participants) => participants.some((p) => p.identity === baseIdentity))
    .catch(() => false);
  if (!authorized) {
    throw new HttpError(403, 'not currently a participant in this room');
  }

  const identity = `${baseIdentity}${GALLERY_IDENTITY_SUFFIX}`;
  const token = await mintToken(env, {
    room,
    identity,
    displayName: displayName ?? identity,
    // Least privilege, hardcoded -- never caller-controlled (mirrors the
    // public endpoint's #100 clamp, just with the grants a hidden bridge
    // actually needs instead of a visible participant's).
    canPublish: false,
    canSubscribe: true,
    canPublishData: false,
    hidden: true,
  });
  return { url: env.url, token, room };
}

// ---------------------------------------------------------------------------
// AI chat: Gemini Live ephemeral tokens (#655)
// ---------------------------------------------------------------------------

export interface AiTokenRequest {
  room: string; // full room credential
  identity: string; // the caller's OWN visible-participant identity
}

export interface AiTokenResponse {
  token: string; // Gemini `authTokens/…` resource name, used verbatim
  // RFC3339 expiry AS REPORTED BY GOOGLE on the created token. Omitted entirely
  // when Google's create response carried none — a measured value or nothing,
  // never our own request wearing Google's name. Absence means "unknown", not
  // "never expires"; `requestedExpireTime` is the ceiling we asked for.
  expireTime?: string;
  requestedExpireTime: string; // RFC3339; what we ASKED Google to cut the session off at
  model: string; // resolved model id — clients MUST use this, not a constant
}

export interface LiveKitTokenClaims {
  identity?: string;
  room?: string;
  roomJoin?: boolean;
}

export type LiveKitTokenVerifier = (
  env: LiveKitEnv,
  jwt: string
) => Promise<LiveKitTokenClaims>;

export interface AiTokenContext {
  authorization?: string; // the caller's own LiveKit access token, `Bearer <jwt>`
  rateLimitKey?: string;
  nowMs?: number;
  service?: RoomDiscoveryService;
  // Test seams. Production always uses the real verifier/minter below.
  verifyLiveKitToken?: LiveKitTokenVerifier;
  mintEphemeralToken?: GeminiTokenMinter;
  upstreamTimeoutMs?: number;
}

// ONE budget, SHARED by both upstream calls (liveness, then mint) — not one
// each. This route's worst case has to be a number a caller can wait out,
// because a mint is NOT idempotent: a caller that gives up early and retries
// pays for every abandoned attempt that succeeded upstream anyway.
//
// It used to be 4s per call, so the route could legitimately take 8s while the
// desktop client abandoned each attempt at 5s and retried three times. One
// click bought FOUR real Gemini tokens, burned four of the user's six hourly
// slots, and still ended in "Could not reach the AI chat service" (#655 cost
// review). Two rules keep that closed, and both are load-bearing:
//   1. the total below must stay under vercel.json's maxDuration (10s), so the
//      route always answers rather than being killed mid-flight, AND
//   2. every caller must wait at least AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS for
//      ONE attempt and must not retry.
export const AI_TOKEN_UPSTREAM_BUDGET_MS = 6_000;

// The liveness check is a cheap LiveKit round trip. Capping its slice keeps the
// remainder of the shared budget for the call that actually costs money.
export const AI_TOKEN_LIVENESS_TIMEOUT_MS = 2_000;

// The contract with every client of this route — mirrored by the desktop app's
// `AI_TOKEN_REQUEST_TIMEOUT` in apps/desktop/src-tauri/src/ai_chat/commands.rs.
// A client MUST allow at least this long for a single attempt and MUST NOT
// retry on timeout or 5xx: a retried mint is a second billable token, not a
// second chance at the first one. It deliberately exceeds vercel.json's
// maxDuration so even a platform kill is observed as a response rather than
// raced by a client that already gave up.
export const AI_TOKEN_CLIENT_ATTEMPT_TIMEOUT_MS = 12_000;

export class UpstreamTimeoutError extends Error {
  constructor(message = 'upstream timed out') {
    super(message);
    this.name = 'UpstreamTimeoutError';
  }
}

async function withUpstreamTimeout<T>(work: Promise<T>, timeoutMs: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    // Promise.race subscribes to `work`, so a late rejection after a timeout
    // is still handled and never surfaces as an unhandled rejection.
    return await Promise.race([
      work,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new UpstreamTimeoutError()), timeoutMs);
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

function bearerToken(authorization: string | undefined): string | undefined {
  const match = authorization?.match(/^Bearer\s+(.+)$/i);
  return match?.[1]?.trim() || undefined;
}

const verifyLiveKitAccessToken: LiveKitTokenVerifier = async (env, jwt) => {
  const claims = await new TokenVerifier(env.apiKey, env.apiSecret).verify(jwt);
  return {
    identity: typeof claims.sub === 'string' ? claims.sub : undefined,
    room: claims.video?.room,
    roomJoin: claims.video?.roomJoin,
  };
};

// Upstream HTTP status only — never the upstream message, which for Google is
// the raw JSON error body and for LiveKit can echo request material.
function upstreamStatusSuffix(err: unknown): string {
  const status = (err as { status?: unknown })?.status;
  return typeof status === 'number' && status >= 400 && status < 600 ? ` (upstream ${status})` : '';
}

// Mint a short-lived, single-use Gemini Live ephemeral token for one live
// meeting participant (#655). The real GEMINI_API_KEY never leaves this
// backend; the client takes the returned resource name straight to Google's
// constrained Live WebSocket, so no media proxies through here.
//
// TWO auth layers, both required, in this order:
//
//  1. CRYPTOGRAPHIC — `Authorization: Bearer <the caller's own LiveKit access
//     token>`, verified against LIVEKIT_API_SECRET. This is strictly stronger
//     than handleGalleryToken's anchor: that one proves such an identity is in
//     the room, this one proves the CALLER IS that identity. Without it one
//     member could mint against another member's identity, draining their rate
//     bucket and misattributing abuse.
//  2. LIVENESS — the verified identity must be currently connected to the room
//     (the #109 anchor). A 24h-TTL join token long outliving the meeting must
//     not keep buying AI sessions.
//
// Cost bounds that survive a modified client: uses:1, a 30s window to open the
// session, a 12-minute hard expiry, the rate buckets, and Google-side quota.
// Ephemeral-token constraints bind model + response modality but NOT prompt
// content — accepted, documented residual risk (#655).
//
// TWO cost rules that are easy to break by accident, both learned the expensive
// way in #655's review:
//
//  * ONE CALL, ONE MINT. This route is not idempotent — every completed call
//    is a token Google bills for. Everything upstream shares a single
//    AI_TOKEN_UPSTREAM_BUDGET_MS deadline so the route always answers inside
//    the window a client is required to wait, and clients must not retry it.
//  * CHARGE FOR WHAT WAS DELIVERED. The spend buckets are checked on the way in
//    and committed only after a token exists. Failed attempts are bounded by
//    the separate attempt bucket instead, so nobody pays an hourly slot for a
//    token they never got.
//
// Deliberately NOT closed (product decision, #655): a caller who churns
// generated identities gets the IP cap of AI_TOKEN_IP_BUCKET_CAPACITY rather
// than the per-identity AI_TOKEN_IDENTITY_BUCKET_CAPACITY. Anyone able to do
// that already holds the room credential, which IS the capability to be in the
// meeting at all, so the churn buys nothing they were not already invited to.
export async function handleAiToken(
  body: Partial<AiTokenRequest>,
  context: AiTokenContext = {}
): Promise<AiTokenResponse> {
  if (!body || typeof body.room !== 'string' || !body.room.trim()) {
    throw new HttpError(400, 'room is required');
  }
  const nowMs = context.nowMs ?? Date.now();
  // Attempt bucket first: cheapest possible cap on an unauthenticated flood,
  // before any crypto or upstream work, and the ONLY ai-token bucket a failed
  // request charges. Own map, never /api/token's — an AI-chat burst must not be
  // able to lock this IP out of joining meetings.
  await enforceRateLimit(
    stores.aiTokenAttempt,
    context.rateLimitKey,
    nowMs,
    AI_TOKEN_ATTEMPT_BUCKET_CAPACITY,
    AI_TOKEN_BUCKET_REFILL_MS
  );

  const requestedRoom = assertBoundedString(body.room, 'room', { required: true })!;
  const identity = assertGeneratedParticipantIdentity(
    assertBoundedString(body.identity, 'identity', { required: true })!
  );
  const credential = normalizeRoomCredential(requestedRoom);
  if (!credential) {
    throw new HttpError(400, 'room credential is required');
  }
  const room = livekitRoomName(credential);

  // Kill switch, checked before any upstream call: unsetting GEMINI_API_KEY in
  // Vercel turns hosted AI chat off globally and this returns 503 without
  // touching LiveKit or Google.
  const geminiEnv = loadGeminiEnv();
  const livekitEnv = loadLiveKitEnv();

  const jwt = bearerToken(context.authorization);
  if (!jwt) {
    throw new HttpError(401, 'livekit authorization is required');
  }
  const verify = context.verifyLiveKitToken ?? verifyLiveKitAccessToken;
  let claims: LiveKitTokenClaims;
  try {
    claims = await verify(livekitEnv, jwt);
  } catch {
    // Bad signature, expired, malformed — one response for all of them.
    throw new HttpError(401, 'livekit authorization failed');
  }
  if (claims.roomJoin !== true || claims.identity !== identity || claims.room !== room) {
    throw new HttpError(403, 'livekit authorization does not cover this room and identity');
  }

  // Admission only — nothing is charged yet. Both spend buckets are committed
  // at the very bottom, once a token really exists: a caller who fails the
  // liveness check or loses to a Google outage must not lose an hourly slot for
  // a token they never received, and must not then be told "too many AI chat
  // sessions", which is both false and unactionable. Only a cryptographically
  // proven caller can reach their own identity bucket — the whole point of
  // layer 1. The key never leaves process memory.
  const identityBucketKey = `${room}\u0000${identity}`;
  await assertRateLimitAvailable(
    stores.aiTokenIp,
    context.rateLimitKey,
    nowMs,
    AI_TOKEN_IP_BUCKET_CAPACITY,
    AI_TOKEN_BUCKET_REFILL_MS,
    'ai token rate limit exceeded'
  );
  await assertRateLimitAvailable(
    stores.aiTokenIdentity,
    identityBucketKey,
    nowMs,
    AI_TOKEN_IDENTITY_BUCKET_CAPACITY,
    AI_TOKEN_BUCKET_REFILL_MS,
    'ai token rate limit exceeded'
  );

  // ONE deadline across BOTH upstream calls. Measured on the monotonic clock,
  // never `nowMs` — that one is the rate limiter's logical clock and tests pin
  // it to a fixed instant.
  const budgetMs = context.upstreamTimeoutMs ?? AI_TOKEN_UPSTREAM_BUDGET_MS;
  const startedAt = performance.now();
  const remainingMs = () => Math.max(0, budgetMs - (performance.now() - startedAt));

  const service = context.service ?? roomService(livekitEnv);
  let connected = false;
  try {
    const participants = await withUpstreamTimeout(
      withLiveKitRetry(() => service.listParticipants(room)),
      Math.min(remainingMs(), AI_TOKEN_LIVENESS_TIMEOUT_MS)
    );
    connected = participants.some((participant) => participant.identity === identity);
  } catch (err) {
    if (err instanceof UpstreamTimeoutError) {
      throw new HttpError(503, 'participant directory timed out');
    }
    // Every other failure (room gone, LiveKit unreachable) collapses to the
    // same 403 as handleGalleryToken so the response never leaks which case.
    connected = false;
  }
  if (!connected) {
    throw new HttpError(403, 'not currently a participant in this room');
  }

  // Never START a billable mint there is no budget left to receive: a token
  // minted after our own deadline is paid for and thrown away.
  const mintBudgetMs = remainingMs();
  if (mintBudgetMs <= 0) {
    throw new HttpError(503, 'ai token service timed out');
  }

  const requestedExpireTime = new Date(nowMs + AI_TOKEN_LIFETIME_MS).toISOString();
  const mint = context.mintEphemeralToken ?? mintGeminiEphemeralToken;
  let minted: Awaited<ReturnType<GeminiTokenMinter>>;
  try {
    minted = await withUpstreamTimeout(
      mint(geminiEnv, {
        uses: AI_TOKEN_USES,
        newSessionExpireTime: new Date(nowMs + AI_TOKEN_NEW_SESSION_WINDOW_MS).toISOString(),
        expireTime: requestedExpireTime,
        responseModality: AI_TOKEN_RESPONSE_MODALITY,
      }),
      mintBudgetMs
    );
  } catch (err) {
    if (err instanceof UpstreamTimeoutError) {
      throw new HttpError(503, 'ai token service timed out');
    }
    // Never rethrow the SDK's own error: @google/genai's ApiError carries a
    // `status` that lib/http.ts would pass through as OUR status, and a
    // `message` that is the raw upstream JSON body. Status number only.
    throw new HttpError(502, `ai token service unavailable${upstreamStatusSuffix(err)}`);
  }

  // A token now exists and Google will bill for it. THIS is the moment to
  // charge, and the only one.
  await spendRateLimit(
    stores.aiTokenIp,
    context.rateLimitKey,
    nowMs,
    AI_TOKEN_IP_BUCKET_CAPACITY,
    AI_TOKEN_BUCKET_REFILL_MS
  );
  await spendRateLimit(
    stores.aiTokenIdentity,
    identityBucketKey,
    nowMs,
    AI_TOKEN_IDENTITY_BUCKET_CAPACITY,
    AI_TOKEN_BUCKET_REFILL_MS
  );

  return {
    token: minted.token,
    ...(minted.expireTime ? { expireTime: minted.expireTime } : {}),
    requestedExpireTime,
    model: minted.model,
  };
}

export async function handleAdminControl(
  body: Partial<AdminControlRequest>,
  context: AdminControlContext = {}
): Promise<{ ok: true; action: 'kick' | 'close'; room: string }> {
  assertAdminAuthorization(context.authorization);
  const action = body.action;
  if (action !== 'kick' && action !== 'close') {
    throw new HttpError(400, 'action must be kick or close');
  }
  const requestedRoom = assertBoundedString(body.room, 'room', { required: true })!;
  const credential = normalizeRoomCredential(requestedRoom);
  if (!credential) {
    throw new HttpError(400, 'room credential is required');
  }
  const room = livekitRoomName(credential);
  const service = context.service ?? roomService(loadLiveKitEnv());
  if (action === 'kick') {
    const identity = assertBoundedString(body.identity, 'identity', { required: true })!;
    // Record the kick in room metadata FIRST so a client racing to rejoin on
    // the disconnect it is about to receive already finds itself refused by
    // /api/token; then drop the live connection. A metadata failure is a
    // real failure of the admin's intent ("kick" that does not stick) and is
    // surfaced rather than swallowed.
    const rooms = await withLiveKitRetry(() => service.listRooms());
    const existing = rooms.find((candidate) => candidate.name === room);
    if (existing) {
      const meta = decodeRoomMeta(existing.metadata);
      const removed = preservedRoomMeta(meta).removed ?? [];
      if (!removed.includes(identity)) {
        const next = [...removed, identity].slice(-ROOM_META_REMOVED_LIMIT);
        await withLiveKitRetry(() =>
          service.updateRoomMetadata(
            room,
            encodeRoomMeta({
              displayName: meta.displayName ?? roomLabelFromCredential(credential) ?? 'room',
              open: meta.open ?? true,
              removed: next,
            })
          )
        );
      }
    }
    try {
      await withLiveKitRetry(() => service.removeParticipant(room, identity));
    } catch (err) {
      // The identity already left (or never connected): the record above is
      // what makes the kick stick, so "nothing to disconnect" is success, not
      // an error the admin has to retry. Every other failure still surfaces.
      if (!isParticipantMissingError(err)) throw err;
    }
  } else {
    await withLiveKitRetry(() => service.deleteRoom(room));
  }
  return { ok: true, action, room };
}

export interface RoomParticipantView {
  identity: string;
  name: string;
}

export interface RoomView {
  id: string; // opaque public identifier; not a join credential
  name: string; // human display name (from room metadata; falls back to slug)
  open: boolean;
  occupancy: number; // live participant count
  /**
   * #122: WHO is in the room -- display names ONLY, never identities. Present
   * only on `POST /api/rooms/status`, only for a room whose `listParticipants`
   * call actually succeeded, and OMITTED (not `[]`) when it failed, so a
   * caller can tell "nobody visible" from "we could not ask". The same
   * visible-participant filter `occupancy` uses produces it, so the count and
   * the list can never disagree.
   *
   * An identity is a stable per-install value and would be a cross-room,
   * cross-day correlation handle for every invite holder; a name is the thing
   * the owner decided to expose here (see PRIVACY.md). Never log either.
   */
  participants?: RoomOccupantView[];
}

/**
 * One visible person in a room, as `/api/rooms/status` reports them. Deliberately
 * NAME-ONLY -- `RoomParticipantView` (identity + name) is the creator-only
 * shape and must never reach this view. An empty string is a real value: the
 * participant published no display name, and the client renders "Someone".
 */
export interface RoomOccupantView {
  name: string;
}

export interface CreatedRoomView extends RoomView {
  slug: string; // full join credential returned only to the creator
  livekitRoom: string;
  participants: RoomParticipantView[];
}

export interface CreateRoomRequest {
  name?: string;
  open?: boolean;
  room?: string; // optional existing credential to stamp, used by native-created rooms
}

export interface CreateRoomContext {
  service?: RoomMetadataService;
  rateLimitKey?: string;
  nowMs?: number;
}

const FNV_128_OFFSET = 0x6c62272e07bb014262b821756295c58dn;
const FNV_128_PRIME = 0x1000000000000000000013bn;
const U128_MASK = (1n << 128n) - 1n;

export function publicRoomIdForLiveKitRoom(livekitRoom: string): string {
  let hash = FNV_128_OFFSET;
  for (const byte of Buffer.from(livekitRoom, 'utf8')) {
    hash ^= BigInt(byte);
    hash = (hash * FNV_128_PRIME) & U128_MASK;
  }
  return `room_${hash.toString(16).padStart(32, '0')}`;
}

export function roomDiscoveryView(
  livekitRoom: string,
  metadata: string | undefined,
  visibleOccupancy: number
): RoomView {
  const credential = livekitRoom.replace(/^petal-room-/, '');
  const meta = decodeRoomMeta(metadata);
  return {
    id: publicRoomIdForLiveKitRoom(livekitRoom),
    name: meta.displayName ?? roomLabelFromCredential(credential) ?? 'room',
    open: meta.open ?? true,
    occupancy: visibleOccupancy,
  };
}

// ONE upstream RPC per refresh, shared by every caller for ROOMS_LIST_CACHE_MS.
// `numParticipants` is the ROOM-LEVEL count and it COUNTS HIDDEN participants
// (#120, measured 2026-09-09 on livekit-server 1.13.2 AND on the hosted
// LiveKit Cloud deployment: 1 visible peer + 1 hidden `-gallery` bridge reads
// 2). Every desktop user opens such a bridge, so this number is N + k, not N.
// It is still the room list's number -- `handleListRooms` is server-side
// tooling and keeps it -- but `handleRoomStatus`, whose answer reaches the
// user as "N people in the room", refines it per room with `listParticipants`
// (see `cachedVisibleOccupancy`). That fan-out is bounded by the caller's
// presented credentials (<= ROOM_STATUS_MAX_ROOMS), which is what made the
// old unauthenticated-GET fan-out over every live room unacceptable (#708).
interface RoomsListCacheEntry {
  at: number;
  rooms: CachedRoomEntry[];
}
interface CachedRoomEntry {
  livekitRoom: string;
  meta: Partial<import('./livekit.js').RoomMeta>;
  view: RoomView;
}
// Keyed by the service object so an injected test service never shares a
// cache with production; production uses one stable key per process.
const roomsListCache = new WeakMap<object, RoomsListCacheEntry>();
const PRODUCTION_ROOMS_LIST_CACHE_KEY = {};
let roomsListInFlight: Promise<CachedRoomEntry[]> | null = null;

export function resetRoomsListCacheForTest(): void {
  roomsListCache.delete(PRODUCTION_ROOMS_LIST_CACHE_KEY);
  roomsListInFlight = null;
  participantsCache.delete(PRODUCTION_ROOMS_LIST_CACHE_KEY);
  participantsInFlight.delete(PRODUCTION_ROOMS_LIST_CACHE_KEY);
}

// #120: the count a human reads off the room card. `numParticipants` counts
// every LiveKit participant including hidden ones, and every desktop user
// brings a hidden `-gallery` bridge, so the room-level number is N + k. Count
// the people instead.
//
// The `-gallery` suffix check and the `hidden` permission check are
// deliberately both here: they are the same participant today, but a hidden
// participant from any other source is not a person either, and the suffix
// still holds if a server ever stops populating `permission`.
//
// The DISCONNECTED filter is NOT the ghost fix. A peer that drops without
// disconnecting stays `ACTIVE` for the SFU's ~20-30 s reconnect grace and is
// counted for that whole window -- accepted, see docs/CONTRACTS.md. This only
// drops the SDK's terminal state.
// Deduped by identity so the count and the name list (#122) are computed from
// literally the same rows and cannot disagree. A live LiveKit roster is
// identity-keyed, so this is defensive, not a fix for an observed duplicate.
export function visibleParticipants(participants: ParticipantInfo[]): ParticipantInfo[] {
  const seen = new Set<string>();
  return participants.filter((p) => {
    if (p.permission?.hidden) return false;
    if (p.identity.endsWith(GALLERY_IDENTITY_SUFFIX)) return false;
    if (p.state === ParticipantInfo_State.DISCONNECTED) return false;
    if (seen.has(p.identity)) return false;
    seen.add(p.identity);
    return true;
  });
}

/**
 * #122: an identity must never travel as a NAME either. LiveKit sets
 * `participant.name` from the client's `displayName` and falls back to the
 * raw identity when there is none, so the default roster row for a
 * display-name-less peer literally carries the identity in the name field --
 * publishing it would hand every invite holder the correlation handle this
 * whole field shape exists to withhold.
 *
 * Both rules are needed: `name === identity` catches LiveKit's own fallback,
 * and the shape check catches a client that set `displayName` to some other
 * opaque token. The three shapes are the identity formats this file mints and
 * accepts (an opaque UUID, `web-<uuid>`, and a bare 32-hex credential-like
 * token) and are kept in sync with shared/logic/participantNames.ts's
 * `looksLikeTechnicalIdentity`, which the CLIENTS use to render "Someone".
 * They are duplicated rather than imported because `backend/` deploys as its
 * own self-contained Vercel root and cannot reach the repo-root shared/
 * package (#662).
 */
const TECHNICAL_IDENTITY_SHAPES = [
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
  /^web-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
  /^[0-9a-f]{32}$/i,
];
function publishableName(p: ParticipantInfo): string {
  const name = (p.name ?? '').trim();
  if (!name) return '';
  if (name === p.identity.trim()) return '';
  if (TECHNICAL_IDENTITY_SHAPES.some((shape) => shape.test(name))) return '';
  return name;
}

export function visibleParticipantCount(participants: ParticipantInfo[]): number {
  return visibleParticipants(participants).length;
}

/** At most this many names travel in one room's status view (#122). */
export const ROOM_STATUS_MAX_PARTICIPANT_NAMES = 32;

/**
 * #122: the names behind `occupancy`, for the row's hover tooltip. The same
 * (already identity-deduped) `visibleParticipants` rows, then:
 *   - reduced to `{ name }` -- the identity is dropped here and never leaves
 *     the backend, in the `name` field or anywhere else (`publishableName`),
 *   - sorted by name so the tooltip is stable across polls, and
 *   - capped, because this is a hover tooltip, not a directory.
 *
 * The trim is deliberate and an empty result is kept: the client renders a
 * nameless participant as "Someone" rather than the endpoint inventing a
 * label, and NEVER substitutes the identity.
 */
export function visibleParticipantNames(participants: ParticipantInfo[]): RoomOccupantView[] {
  return visibleParticipants(participants)
    .map(publishableName)
    .sort((a, b) => a.localeCompare(b))
    .slice(0, ROOM_STATUS_MAX_PARTICIPANT_NAMES)
    .map((name) => ({ name }));
}

interface RoomOccupancyReading {
  count: number;
  participants: RoomOccupantView[];
}
interface ParticipantsCacheEntry extends RoomOccupancyReading {
  at: number;
}
// Same keying discipline as roomsListCache: an injected test service never
// shares cached counts with production, and concurrent misses for one room
// coalesce onto a single upstream call.
const participantsCache = new WeakMap<object, Map<string, ParticipantsCacheEntry>>();
const participantsInFlight = new WeakMap<object, Map<string, Promise<RoomOccupancyReading>>>();
const PARTICIPANTS_CACHE_MAX_ROOMS = 512;

function perServiceMap<T>(store: WeakMap<object, Map<string, T>>, key: object): Map<string, T> {
  const existing = store.get(key);
  if (existing) return existing;
  const created = new Map<string, T>();
  store.set(key, created);
  return created;
}

// Entries are keyed by room, so a long-lived instance would otherwise
// accumulate one per room it has ever answered for. Expired first, then
// oldest-inserted (Map preserves insertion order).
function pruneParticipantsCache(cache: Map<string, ParticipantsCacheEntry>, nowMs: number): void {
  if (cache.size <= PARTICIPANTS_CACHE_MAX_ROOMS) return;
  for (const [room, entry] of cache) {
    if (nowMs - entry.at >= ROOMS_LIST_CACHE_MS) cache.delete(room);
  }
  while (cache.size > PARTICIPANTS_CACHE_MAX_ROOMS) {
    const oldest = cache.keys().next();
    if (oldest.done) break;
    cache.delete(oldest.value);
  }
}

// ONE `listParticipants` read answers both questions (#122): the count and
// the names come out of the same roster, cached and coalesced together, so
// adding the names costs no extra upstream call.
async function cachedRoomOccupancy(
  service: RoomDiscoveryService,
  cacheKey: object,
  livekitRoom: string,
  nowMs: number
): Promise<RoomOccupancyReading> {
  const cache = perServiceMap(participantsCache, cacheKey);
  const cached = cache.get(livekitRoom);
  if (cached && nowMs - cached.at < ROOMS_LIST_CACHE_MS && nowMs >= cached.at) {
    return { count: cached.count, participants: cached.participants };
  }
  const inFlight = perServiceMap(participantsInFlight, cacheKey);
  const existing = inFlight.get(livekitRoom);
  if (existing) return existing;
  const pending = withLiveKitRetry(() => service.listParticipants(livekitRoom))
    .then((participants) => {
      const reading: RoomOccupancyReading = {
        count: visibleParticipantCount(participants),
        participants: visibleParticipantNames(participants),
      };
      cache.set(livekitRoom, { at: nowMs, ...reading });
      pruneParticipantsCache(cache, nowMs);
      return reading;
    })
    .finally(() => {
      inFlight.delete(livekitRoom);
    });
  inFlight.set(livekitRoom, pending);
  return pending;
}

// Replaces each presented room's room-level `numParticipants` with its count
// of visible, connected participants. Bounded by the caller's presented set
// (<= ROOM_STATUS_MAX_ROOMS) intersected with the rooms the list already says
// are non-empty, so an empty or unknown credential costs nothing extra.
async function withVisibleOccupancy(
  matched: CachedRoomEntry[],
  context: RoomStatusContext,
  nowMs: number
): Promise<RoomView[]> {
  // `numParticipants === 0` needs no RPC: nothing this function could learn
  // would raise the count, and a hidden-only room reports >= 1 here, so the
  // gate never skips a room whose visible count it would have lowered.
  const live = matched.filter((e) => e.view.occupancy > 0);
  if (live.length === 0) return matched.map((e) => e.view);
  const service: RoomDiscoveryService = context.service ?? roomService(loadLiveKitEnv());
  const cacheKey: object = context.service ?? PRODUCTION_ROOMS_LIST_CACHE_KEY;
  const settled = await Promise.allSettled(
    live.map((entry) => cachedRoomOccupancy(service, cacheKey, entry.livekitRoom, nowMs))
  );
  const refined = new Map<string, RoomOccupancyReading>();
  live.forEach((entry, index) => {
    const result = settled[index]!;
    // One room's failure never fails the batch and never reads as 0: it falls
    // back to the room list's number, which is the pre-#120 behaviour and
    // still the best available answer for that room. This is #708's per-room
    // isolation, back where a per-room call actually exists again.
    if (result.status === 'fulfilled') refined.set(entry.livekitRoom, result.value);
  });
  // Never mutate `e.view` -- it is the cached object handleListRooms returns.
  return matched.map((entry) => {
    const reading = refined.get(entry.livekitRoom);
    // #122: `participants` is OMITTED, never `[]`, for a room whose roster we
    // could not read (the failed-RPC fallback above) and for a room the room
    // list already says is empty. `[]` would claim "nobody is here", which is
    // exactly the thing we do not know in the failure case.
    if (!reading) return entry.view;
    return { ...entry.view, occupancy: reading.count, participants: reading.participants };
  });
}

async function cachedRoomEntries(context: RequestContext, nowMs: number): Promise<CachedRoomEntry[]> {
  const env = loadLiveKitEnv();
  const cacheKey: object = context.service ?? PRODUCTION_ROOMS_LIST_CACHE_KEY;
  const cached = roomsListCache.get(cacheKey);
  if (cached && nowMs - cached.at < ROOMS_LIST_CACHE_MS && nowMs >= cached.at) {
    return cached.rooms;
  }
  // Coalesce concurrent misses onto one upstream call (production key only;
  // injected services are per-test and must not share a promise).
  const refresh = async (): Promise<CachedRoomEntry[]> => {
    const service = context.service ?? roomService(env);
    const rooms = await listPetalRooms(env, service);
    const entries = rooms.map((r) => ({
      livekitRoom: r.name,
      meta: decodeRoomMeta(r.metadata),
      view: roomDiscoveryView(r.name, r.metadata, r.numParticipants),
    }));
    roomsListCache.set(cacheKey, { at: nowMs, rooms: entries });
    return entries;
  };
  if (context.service) {
    return refresh();
  }
  if (!roomsListInFlight) {
    roomsListInFlight = refresh().finally(() => {
      roomsListInFlight = null;
    });
  }
  return roomsListInFlight;
}

/**
 * The full directory view. NOT exposed over HTTP any more (`GET /api/rooms`
 * is 410): listing every room's name and headcount to anyone on the internet
 * was an enumeration leak, and the cross-machine discovery it existed for
 * (#98/#155) has been inert since #83 stripped join credentials from the
 * view. Kept as the shared cache primitive behind `handleRoomStatus` and for
 * server-side tooling/tests.
 */
export async function handleListRooms(context: RequestContext = {}): Promise<{ rooms: RoomView[] }> {
  const nowMs = context.nowMs ?? Date.now();
  await enforceRoomsRateLimit(context.rateLimitKey, nowMs);
  const entries = await cachedRoomEntries(context, nowMs);
  return { rooms: entries.map((e) => e.view) };
}

// Proof-of-possession status lookup (replaces the public directory).
export const ROOM_STATUS_MAX_ROOMS = 64;

export interface RoomStatusRequestEntry {
  room: string; // room credential, `room-<32hex>`
  accessCode?: string; // required only for rooms stamped open:false
}
export interface RoomStatusRequest {
  rooms: RoomStatusRequestEntry[];
}

// The status seam needs BOTH RPCs: `listRooms` for the shared room list and
// metadata, `listParticipants` for the per-room visible count (#120).
export interface RoomStatusContext extends RequestContext {
  service?: RoomListingService & RoomDiscoveryService;
}

/**
 * `POST /api/rooms/status { rooms: [{ room, accessCode? }] }` ->
 * `{ rooms: [{ id, name, open, occupancy }] }` for ONLY the rooms whose
 * credential the caller presented. Unknown, expired, and malformed entries
 * are silently omitted -- never 404'd -- so the endpoint is not an oracle for
 * whether a guessed credential exists. A room stamped `open:false` is
 * additionally omitted unless `accessCode` hashes to its credential, the same
 * rule `handleToken` applies at mint. One cached `listRooms` RPC serves every
 * caller (see `cachedRoomEntries`); the rooms rate-limit bucket is charged.
 *
 * `occupancy` is the number of VISIBLE, connected participants, counted with
 * one cached `listParticipants` per presented non-empty room (#120) -- NOT
 * `numParticipants`, which counts the hidden `-gallery` bridge every desktop
 * user opens and so reads N + k.
 *
 * `participants` (#122) is WHO those people are, by DISPLAY NAME ONLY, from
 * that same roster read -- no second fan-out. Owner decision 2026-09-09: an
 * invite holder sees the names without joining. Identities never appear, and
 * nothing here logs a name (`handlers.ts` has no `console.*` at all; asserted
 * by backend/test/privacy.ts). The key is omitted, never `[]`, for any room
 * whose roster this call did not actually read.
 */
export async function handleRoomStatus(
  body: Partial<RoomStatusRequest> | null | undefined,
  context: RoomStatusContext = {}
): Promise<{ rooms: RoomView[] }> {
  if (!body || !Array.isArray(body.rooms)) {
    throw new HttpError(400, 'rooms must be an array');
  }
  if (body.rooms.length > ROOM_STATUS_MAX_ROOMS) {
    throw new HttpError(400, `rooms must have at most ${ROOM_STATUS_MAX_ROOMS} entries`);
  }
  const requested = new Map<string, { credential: string; accessCode?: string }>();
  for (const entry of body.rooms) {
    if (!entry || typeof entry !== 'object' || typeof (entry as RoomStatusRequestEntry).room !== 'string') {
      throw new HttpError(400, 'each rooms entry must be { room, accessCode? }');
    }
    const room = assertBoundedString((entry as RoomStatusRequestEntry).room, 'room', { required: true })!;
    const accessCode = assertBoundedString((entry as RoomStatusRequestEntry).accessCode, 'accessCode', {
      required: false,
    });
    const credential = normalizeRoomCredential(room);
    if (!credential) continue; // malformed: omit, don't reveal
    const livekitRoom = livekitRoomName(credential);
    if (!requested.has(livekitRoom)) requested.set(livekitRoom, { credential, accessCode });
  }
  const nowMs = context.nowMs ?? Date.now();
  await enforceRoomsRateLimit(context.rateLimitKey, nowMs);
  if (requested.size === 0) return { rooms: [] };
  const entries = await cachedRoomEntries(context, nowMs);
  const matched: CachedRoomEntry[] = [];
  for (const entry of entries) {
    const asked = requested.get(entry.livekitRoom);
    if (!asked) continue;
    if (entry.meta.open === false) {
      const proven = asked.accessCode ? credentialForAccessCode(asked.accessCode) : null;
      if (!proven || proven !== asked.credential) continue;
    }
    matched.push(entry);
  }
  if (matched.length === 0) return { rooms: [] };
  return { rooms: await withVisibleOccupancy(matched, context, nowMs) };
}

export async function handleCreateRoom(
  body: Partial<CreateRoomRequest>,
  context: CreateRoomContext = {}
): Promise<{ room: CreatedRoomView }> {
  if (!body || typeof body.name !== 'string' || !body.name.trim()) {
    throw new HttpError(400, 'name is required');
  }
  await enforceRoomCreateRateLimit(context.rateLimitKey, context.nowMs ?? Date.now());
  const displayName = assertBoundedString(body.name, 'name', { required: true })!;
  const open = body.open ?? true;
  const requestedCredential = typeof body.room === 'string' && body.room.trim()
    ? normalizeRoomCredential(body.room)
    : null;
  if (typeof body.room === 'string' && body.room.trim() && !requestedCredential) {
    throw new HttpError(400, 'room credential is required');
  }
  const credential = requestedCredential ?? generateRoomCredential(displayName);
  const livekitRoom = livekitRoomName(credential);
  const env = loadLiveKitEnv();
  const room = await ensureRoom(
    env,
    livekitRoom,
    { displayName, open },
    context.service,
    { preserveOpenOnExisting: requestedCredential !== null }
  );
  const roomMeta = decodeRoomMeta(room.metadata);
  const actualOpen = roomMeta.open ?? open;
  return {
    room: {
      id: publicRoomIdForLiveKitRoom(livekitRoom),
      name: displayName,
      slug: credential,
      livekitRoom,
      open: actualOpen,
      occupancy: room.numParticipants,
      participants: [],
    },
  };
}

export class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}
