// Petal wire-format contracts (track naming + data-channel topics), extracted
// into a DOM-free module so they can be unit-tested (see tests/) alongside
// meetingCode.ts. These MUST stay in lockstep with the native app:
//
// - `petal-window-<u32>`  -- apps/desktop/src-tauri/src/transport/publisher.rs
//   ::track_name_for_window, inverse window_id_from_track_name in
//   compositor.rs. Publishing under this exact name is what makes the native
//   receiver treat a track as a real shareable window.
// - `petal-camera-<slug>` -- publisher.rs::CAMERA_TRACK_PREFIX; the native
//   side derives a synthetic compositor-window id as
//   fnv1a(full track name) | 0x8000_0000, so the suffix only needs to be a
//   stable, collision-avoiding slug of our identity.
// - topic `petal.telepointer`, JSON {windowId, userId, x, y, visible,
//   activity?} -- apps/desktop/src-tauri/src/telepointer.rs (SPEC.md §4.5).
// - topic `petal.remote-control`, JSON RemoteControlMessage v1 -- native
//   remote-control receiver for replaying viewer input against the original
//   shared window.
// - topic `petal.viewer-demand`, JSON {v, kind, targetUserId, viewerId,
//   windowId, seq, visible, width, height, scale, pixelWidth, pixelHeight} -- passive viewer demand that
//   keeps watched remote windows at Full quality.
// - topic `petal.pipeline-stats`, JSON PipelineStatsMessage v1 -- low-rate
//   cross-peer local stage snapshots for Network Cockpit pipeline rows.
// - topic `petal.latency-probe`, JSON LatencyProbeMessage v1 -- data-channel
//   RTT ping/pong probes for the network cockpit.
// - topic `petal.draw`, JSON DrawMessage v1 -- reliable, batched drawing
//   annotations scoped to a shared window and authenticated LiveKit sender.
// - `petal-ai-window-<u32>` -- apps/desktop/src-tauri/src/ai_chat/wire.rs
//   ::ai_track_name. The assistant's VOICE, published by the window's owner.
//   It is not a human participant's microphone: every prefix-matching parser
//   must classify `petal-ai-*` explicitly instead of letting it fall through
//   to the camera/window/unknown branch, and it is excluded from
//   speaking-indicator and mic-mute logic.
// - topic `petal.ai-chat`, JSON AiChatMessage v1 -- apps/desktop/src-tauri/
//   src/ai_chat/wire.rs. Reliable; per-message-kind authorization lives in
//   aiChat.ts::authorizeAiChatMessage (the whole security boundary of the
//   topic), mirroring wire.rs::authorize.
// - topic `petal.cockpit`, JSON CockpitReportMessage v1 -- test-cockpit
//   walking-skeleton (#254): unattended `?auto=<scenarioId>` runs self-report
//   step-by-step liveness/results here. Native side (`cockpit_topic.rs`)
//   only receives/logs it this phase; no verdict consumption yet.

export function trackNameForWindow(windowId: number): string {
  return `petal-window-${windowId}`;
}

export function trackNameForCamera(identity: string): string {
  const slug =
    identity
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '') || 'anon';
  return `petal-camera-${slug}`;
}

/**
 * Track-name prefix for the assistant's published voice (`petal.ai-chat`,
 * #657). Mirrors wire.rs's `AI_TRACK_PREFIX`.
 *
 * Deliberately NOT a sub-namespace of `petal-window-`/`petal-camera-`: those
 * two prefixes already drive tile creation, window-id parsing and camera
 * classification, and an assistant track that fell into either would surface
 * as a bogus participant tile or a phantom shared window.
 */
export const AI_TRACK_PREFIX = 'petal-ai-';

/** The assistant's audio track name for a shared window. */
export function aiTrackName(windowId: number): string {
  return `${AI_TRACK_PREFIX}window-${windowId}`;
}

/**
 * Is this the assistant's voice rather than a human participant's mic? Every
 * surface that classifies a track by prefix must ask this FIRST -- the
 * assistant is not the sharer, so it must never light a speaking indicator
 * and muting your own microphone must never mute it.
 */
export function isAiTrackName(trackName: string | null | undefined): boolean {
  return typeof trackName === 'string' && trackName.startsWith(AI_TRACK_PREFIX);
}

/** The window a `petal-ai-window-<id>` track belongs to, or null. */
export function aiTrackWindowId(trackName: string | null | undefined): number | null {
  if (!isAiTrackName(trackName)) return null;
  const raw = (trackName as string).slice(`${AI_TRACK_PREFIX}window-`.length);
  if (!(trackName as string).startsWith(`${AI_TRACK_PREFIX}window-`) || !/^\d+$/.test(raw)) return null;
  const id = Number(raw);
  if (!Number.isSafeInteger(id) || id < 1 || id > 0xffff_ffff) return null;
  return id;
}

export function cameraWindowId(trackName: string): number {
  let hash = 0x811c_9dc5;
  for (let index = 0; index < trackName.length; index += 1) {
    hash ^= trackName.charCodeAt(index) & 0xff;
    hash = Math.imul(hash, 0x0100_0193) >>> 0;
  }
  return (hash | 0x8000_0000) >>> 0;
}

/**
 * Fresh random window_id per REAL screen share (getDisplayMedia). Kept in the
 * positive-i32 range (high bit clear) deliberately: the native compositor
 * derives synthetic window ids for `petal-camera-*` tracks as
 * `fnv1a(track_name) | 0x8000_0000` (high bit SET), so staying below
 * 0x8000_0000 guarantees a real share's id can never collide with a
 * camera-derived synthetic id on the receiving side.
 */
export function randomWindowId(): number {
  return (Math.floor(Math.random() * 0x7fff_ffff) + 1) >>> 0;
}

export const TELEPOINTER_TOPIC = 'petal.telepointer';

/** Exact telepointer wire shape (SPEC.md §4.5 / telepointer.rs). */
export interface TelepointerMessage {
  windowId: number;
  userId: string;
  x: number;
  y: number;
  visible: boolean;
  activity?: 'click' | 'type';
  /** Optional owner (sharer) of the shared surface the cursor is over. */
  surfaceOwnerId?: string;
}

export const REMOTE_CONTROL_TOPIC = 'petal.remote-control';
export const VIEWER_DEMAND_TOPIC = 'petal.viewer-demand';
export const PIPELINE_STATS_TOPIC = 'petal.pipeline-stats';
export const LATENCY_PROBE_TOPIC = 'petal.latency-probe';
export const DRAW_TOPIC = 'petal.draw';
export const AI_CHAT_TOPIC = 'petal.ai-chat';
export const COCKPIT_TOPIC = 'petal.cockpit';

/** Wire version of `petal.ai-chat`. A mismatch is rejected outright. */
export const AI_CHAT_VERSION = 1;

/**
 * How often the owner republishes `state` while a session is live, and how
 * many consecutive misses a receiver tolerates before declaring the session
 * gone. Mirrors wire.rs's STATE_HEARTBEAT_SECONDS /
 * STATE_MISSED_HEARTBEATS_BEFORE_STALE: long enough to ride out a hiccup,
 * short enough that a crashed host cannot leave a phantom "AI active" badge.
 */
export const AI_CHAT_STATE_HEARTBEAT_MS = 5_000;
export const AI_CHAT_MISSED_HEARTBEATS_BEFORE_STALE = 3;
export const AI_CHAT_STALE_AFTER_MS =
  AI_CHAT_STATE_HEARTBEAT_MS * AI_CHAT_MISSED_HEARTBEATS_BEFORE_STALE;

/**
 * Closed set. Freeform strings are not permitted on this topic: every surface
 * renders copy from these tokens (see aiChat.ts's message table, which mirrors
 * `EndReason::user_message()`), so a new one must be added to
 * contracts/petal-contracts.json and both implementations together.
 */
export type AiChatEndReason =
  | 'stopped'
  | 'time-limit'
  | 'disabled'
  | 'not-shared'
  | 'busy'
  | 'rate-limited'
  | 'hosted-unavailable'
  | 'offline'
  | 'mint-failed'
  | 'model-unavailable'
  | 'quota'
  | 'error';

export type AiChatTranscriptRole = 'user' | 'assistant';

/**
 * Every message is keyed by `{windowId, ownerIdentity}` -- a raw CGWindowID is
 * only unique on the machine that produced it.
 */
interface AiChatBase {
  v: 1;
  windowId: number;
  ownerIdentity: string;
}

export type AiChatMessage =
  | (AiChatBase & {
      /** Any participant may ASK; the owner decides whether to act. */
      type: 'startRequest' | 'stopRequest';
    })
  | (AiChatBase & {
      /**
       * Owner-authored session truth, and the liveness heartbeat. Absent
       * optionals are omitted, never sent as null (the Rust side skips
       * serializing `None`).
       */
      type: 'state';
      active: boolean;
      startedBy?: string;
      secondsLeft?: number;
      /** Who currently holds the single push-to-talk floor, if anyone. */
      activeSpeaker?: string;
      error?: AiChatEndReason;
    })
  | (AiChatBase & {
      /** Claim/release the floor -- for the sender themselves only. */
      type: 'pttStart' | 'pttEnd';
    })
  | (AiChatBase & {
      type: 'transcript';
      role: AiChatTranscriptRole;
      text: string;
      final: boolean;
    })
  | (AiChatBase & {
      /**
       * A typed turn. Any participant may send one; unlike pttStart/pttEnd
       * it never touches the floor -- a typed message has no "who's
       * speaking" ambiguity to arbitrate.
       */
      type: 'sendText';
      text: string;
    });

/** Mirrors `wire::MAX_USER_TEXT_CHARS` on the Rust side -- the real
 * enforcement point. This copy only bounds the input box's own UI. */
export const AI_CHAT_TEXT_MAX_CHARS = 600;

/** One self-reported step from an unattended `?auto=<scenarioId>` cockpit run. */
export interface CockpitReportMessage {
  v: 1;
  reporterId: string;
  scenarioId: string;
  step: string;
  ok: boolean;
  detail: string;
  sentAtMs: number;
  fps?: number;
  width?: number;
  height?: number;
  cameraPublished?: boolean;
  cameraDisappeared?: boolean;
  audioPublished?: boolean;
  strokeDelivered?: boolean;
  telepointerMoved?: boolean;
  heartbeatCount?: number;
  heartbeatOk?: boolean;
  stallWatchOk?: boolean;
  participantCount?: number;
  remoteParticipantCount?: number;
  /** Additive #261 MULTI-3 browser-roster correlation proof; never raw identities. */
  rosterFingerprint?: string;
  rosterFingerprintAlgorithm?: 'sha-256';
  rosterIncludesReporter?: boolean;
  rosterUnique?: boolean;
  trackName?: string;
  windowId?: number;
  /**
   * #812 AUD-04: post-decode audio energy the WEB listener measured on the
   * native mic track (`totalAudioEnergy / totalSamplesDuration` from
   * inbound-rtp). RMS of the samples the decoder produced -- never a packet
   * or byte count, which #787 proved can look healthy through total silence.
   */
  remoteAudioAudible?: boolean;
  remoteAudioRms?: number;
  remoteAudioEnergyDelta?: number;
  remoteAudioDurationDelta?: number;
  remoteAudioPublisher?: string;
  /**
   * CAM-N2W (journey CAM-05, #815): what the web viewer saw of the NATIVE
   * camera. `remoteCameraVisible` is the marker the native arm requires, and
   * it means advancing, non-black, CHANGING pixels read back off the tile --
   * not that a track was subscribed, which #806 proved can be green while
   * nothing is on screen.
   */
  remoteCameraVisible?: boolean;
  remoteCameraFps?: number;
  remoteCameraWidth?: number;
  remoteCameraHeight?: number;
  remoteCameraFramesDecodedDelta?: number;
  remoteCameraNonBlackRatio?: number;
  remoteCameraInterFrameDiff?: number;
  remoteCameraPublisher?: string;
  /**
   * Set to `INFRA-FAIL` when the scenario could not be measured at all (the
   * instrument failed), as opposed to the product failing. The native side
   * maps this to an InfraFail verdict; without it on the wire, an instrument
   * failure is indistinguishable from a product failure at the only layer
   * anyone acts on (#821).
   */
  classification?: 'PASS' | 'TEST-FAIL' | 'INFRA-FAIL';
  /**
   * #819 RC-N2W: what this peer RECEIVED from a native controller while acting
   * as an emulated control host. A DELIVERY record only -- a browser cannot
   * inject OS input, so nothing here says an input was applied, and the
   * emulator never sends a `result`. See remoteControlHostLedger.ts.
   */
  controlGranted?: boolean;
  receivedControlKinds?: string[];
  receivedControlCount?: number;
}

export interface LatencyProbeMessage {
  v: 1;
  kind: 'ping' | 'pong';
  probeId: number;
  senderId: string;
  sendTimeMs: number;
  receiverReceiveTimeMs?: number;
  receiverSendTimeMs?: number;
}

export type SharedSourceKind = 'window' | 'display';

const PETAL_WINDOW_KINDS_METADATA_KEY = 'petalWindowKinds';
const PETAL_WINDOW_SCALES_METADATA_KEY = 'petalWindowScales';
const PETAL_WINDOW_COLOR_PROFILES_METADATA_KEY = 'petalWindowColorProfiles';
const PETAL_WINDOW_TITLES_METADATA_KEY = 'petalWindowTitles';
const PETAL_WINDOW_URLS_METADATA_KEY = 'petalWindowUrls';
const PETAL_WINDOW_SHARE_INSTANCES_METADATA_KEY = 'petalWindowShareInstances';
export const PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY = 'petalIdentityPaletteIndex';
// #875: the sharer's currently-shared window ids, front-to-back (index 0 =
// frontmost), as a JSON array. Older sharers omit this key entirely --
// `sharedWindowZOrderFromMetadata` returns null for both an absent key and a
// malformed value; callers must not distinguish those two cases.
export const PETAL_WINDOW_Z_ORDER_METADATA_KEY = 'petalWindowZOrder';
// Per-share remote-control permission (mirrors publisher.rs's
// PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY). Only `false` is ever written, so
// absence means "allowed" and pre-key sharers are unaffected.
export const PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY = 'petalWindowRemoteControl';
const IDENTITY_PALETTE_SIZE = 6;

export interface WindowColorProfile {
  range: 'full' | 'video';
}

export function mergeSharedSourceMetadata(
  currentMetadata: string | undefined | null,
  windowId: number,
  kind: SharedSourceKind | null,
  // Whether a remote peer may CONTROL this share. A browser cannot inject OS
  // input, so a real getDisplayMedia share must publish `false`: without it
  // the scale entry below makes native receivers offer a Control button that
  // can only ever time out. The cockpit test-pattern share passes `true`
  // because RC-N2W's host emulation does answer control requests.
  options: { remoteControllable?: boolean } = {},
): string {
  const root = parseMetadataObject(currentMetadata);
  const kinds = metadataChildObject(root, PETAL_WINDOW_KINDS_METADATA_KEY);
  // petalWindowScales rides along with the kind entry: the native receiver's
  // `remote_control_available` requires a positive scale entry for the shared
  // window (subscriber.rs -> shared_window_scale_from_metadata), so a share
  // that omits it can never be remote-controlled -- RC-N2W's preflight
  // refuses every run against such a share (#819 review). Canvas/display
  // captures in this harness are 1:1, so the scale is 1.
  const scales = metadataChildObject(root, PETAL_WINDOW_SCALES_METADATA_KEY);
  if (kind) {
    kinds[String(windowId)] = kind;
    scales[String(windowId)] = 1;
  } else {
    delete kinds[String(windowId)];
    delete scales[String(windowId)];
  }
  const remoteControl = metadataChildObject(root, PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY);
  if (kind && options.remoteControllable === false) {
    remoteControl[String(windowId)] = false;
  } else {
    delete remoteControl[String(windowId)];
  }
  root[PETAL_WINDOW_KINDS_METADATA_KEY] = kinds;
  root[PETAL_WINDOW_SCALES_METADATA_KEY] = scales;
  if (Object.keys(remoteControl).length > 0) {
    root[PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY] = remoteControl;
  } else {
    delete root[PETAL_WINDOW_REMOTE_CONTROL_METADATA_KEY];
  }
  return JSON.stringify(root);
}

export function mergeIdentityPaletteIndexMetadata(
  currentMetadata: string | undefined | null,
  paletteIndex: number | null | undefined,
): string {
  const root = parseMetadataObject(currentMetadata);
  if (validIdentityPaletteIndex(paletteIndex)) {
    root[PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY] = paletteIndex;
  } else {
    delete root[PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY];
  }
  return JSON.stringify(root);
}

export function identityPaletteIndexFromMetadata(metadata: string | undefined | null): number | null {
  const root = parseMetadataObject(metadata);
  const raw = root[PETAL_IDENTITY_PALETTE_INDEX_METADATA_KEY];
  return validIdentityPaletteIndex(raw) ? raw : null;
}

export function sharedSourceKindFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): SharedSourceKind {
  const root = parseMetadataObject(metadata);
  const raw = metadataChildObject(root, PETAL_WINDOW_KINDS_METADATA_KEY)[String(windowId)];
  return raw === 'display' || raw === 'screen' ? 'display' : 'window';
}

export function sharedWindowShareInstanceFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): string | null {
  const root = parseMetadataObject(metadata);
  const raw = metadataChildObject(root, PETAL_WINDOW_SHARE_INSTANCES_METADATA_KEY)[String(windowId)];
  return typeof raw === 'string' && raw.length > 0 ? raw : null;
}

export function sharedWindowScaleFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): number | null {
  const root = parseMetadataObject(metadata);
  const raw = metadataChildObject(root, PETAL_WINDOW_SCALES_METADATA_KEY)[String(windowId)];
  return typeof raw === 'number' && Number.isFinite(raw) && raw > 0 ? raw : null;
}

export function sharedWindowTitleFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): string | null {
  const root = parseMetadataObject(metadata);
  const raw = metadataChildObject(root, PETAL_WINDOW_TITLES_METADATA_KEY)[String(windowId)];
  return typeof raw === 'string' && raw.trim() ? raw.trim() : null;
}

export function sharedWindowUrlFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): string | null {
  const root = parseMetadataObject(metadata);
  const raw = metadataChildObject(root, PETAL_WINDOW_URLS_METADATA_KEY)[String(windowId)];
  return typeof raw === 'string' ? privacyMinimizedOpenableUrl(raw) : null;
}

export function colorProfileFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): WindowColorProfile | null {
  const root = parseMetadataObject(metadata);
  const raw = metadataChildObject(root, PETAL_WINDOW_COLOR_PROFILES_METADATA_KEY)[String(windowId)];
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const range = (raw as Record<string, unknown>).range;
  return range === 'full' || range === 'video' ? { range } : null;
}

/**
 * #875: decode `petalWindowZOrder` -- the sharer's currently-shared window
 * ids, front-to-back (index 0 = frontmost). Returns null when the key is
 * absent (an older sharer, or none published yet) OR malformed (not an
 * array, or containing a non-integer/negative entry) -- callers must treat
 * both the same way ("no rank data"), never surfacing a partial/best-effort
 * order. An explicitly-published empty array is valid and returns `[]`.
 */
export function sharedWindowZOrderFromMetadata(
  metadata: string | undefined | null,
): number[] | null {
  const root = parseMetadataObject(metadata);
  const raw = root[PETAL_WINDOW_Z_ORDER_METADATA_KEY];
  if (!Array.isArray(raw)) return null;
  const order: number[] = [];
  for (const entry of raw) {
    if (typeof entry !== 'number' || !Number.isInteger(entry) || entry < 0) return null;
    order.push(entry);
  }
  return order;
}

/**
 * This window's front-to-back rank within `sharedWindowZOrderFromMetadata`'s
 * order (0 = frontmost), or null if the key is absent/malformed or this
 * window id is not present in the order.
 */
export function sharedWindowZRankFromMetadata(
  metadata: string | undefined | null,
  windowId: number,
): number | null {
  const order = sharedWindowZOrderFromMetadata(metadata);
  if (!order) return null;
  const index = order.indexOf(windowId);
  return index === -1 ? null : index;
}

function parseMetadataObject(metadata: string | undefined | null): Record<string, unknown> {
  if (!metadata) return {};
  try {
    const parsed = JSON.parse(metadata);
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

function metadataChildObject(root: Record<string, unknown>, key: string): Record<string, unknown> {
  const value = root[key];
  return value && typeof value === 'object' && !Array.isArray(value)
    ? { ...(value as Record<string, unknown>) }
    : {};
}

function validIdentityPaletteIndex(index: unknown): index is number {
  return Number.isInteger(index) && Number(index) >= 0 && Number(index) < IDENTITY_PALETTE_SIZE;
}

function privacyMinimizedOpenableUrl(url: string): string | null {
  const trimmed = url.trim();
  if (!trimmed.startsWith('http://') && !trimmed.startsWith('https://')) return null;
  const queryIndex = trimmed.indexOf('?');
  const hashIndex = trimmed.indexOf('#');
  const cutPoints = [queryIndex, hashIndex].filter((index) => index >= 0);
  const end = cutPoints.length ? Math.min(...cutPoints) : trimmed.length;
  return trimmed.slice(0, end);
}

export interface ViewerDemandMessage {
  v: 2;
  kind: 'open' | 'closed' | 'heartbeat';
  targetUserId: string;
  viewerId: string;
  windowId: number;
  seq: number;
  visible: boolean;
  width: number;
  height: number;
  scale: number;
  pixelWidth: number;
  pixelHeight: number;
  needsRepublish?: boolean;
}

export interface PipelineStageMetrics {
  width: number | null;
  height: number | null;
  fps: number | null;
  kbps: number | null;
}

export type CaptureStateKind = 'live' | 'idle' | 'occluded' | 'wedged';

export interface CaptureCpuMetrics {
  lockCopyMs: number | null;
  convertMs: number | null;
  captureFrameReturnMs: number | null;
}

export interface CaptureStateReport {
  state: CaptureStateKind;
  fps: number | null;
  dirtyRectCount: number | null;
  dirtyAreaPx: number | null;
  occlusionPct: number | null;
  cpu: CaptureCpuMetrics;
}

export interface ReceiverFreezeMetrics {
  freezeCount: number;
  framesDropped: number;
  qualityLimitationReason: string | null;
}

export type PipelineStatsRole = 'sender' | 'receiver';

/**
 * A coarse lifecycle fact observed by the reporter itself.  It is deliberately
 * not a cross-peer assertion: a sender never reports receiver presentation,
 * and a receiver never reports capture/publication.
 */
export type PipelineLifecycle =
  | 'captureReady'
  | 'published'
  | 'subscribed'
  | 'firstDecoded'
  | 'firstPresented'
  | 'unsubscribed'
  | 'unpublished'
  | 'terminalFailure';

export interface PipelineStatsMessage {
  v: 1;
  role: PipelineStatsRole;
  reporterId: string;
  ownerIdentity: string;
  windowId: number;
  seq: number;
  sentAtMs: number;
  grabbed: PipelineStageMetrics | null;
  encodedSent: PipelineStageMetrics | null;
  received: PipelineStageMetrics | null;
  decoded: PipelineStageMetrics | null;
  captureState: CaptureStateReport | null;
  receiverFreeze: ReceiverFreezeMetrics | null;
  /** Additive v1 correlation fields. Omitted by already-shipped senders. */
  publicationSid?: string | null;
  shareEpoch?: string | null;
  lifecycle?: PipelineLifecycle | null;
}

export interface DrawPoint {
  x: number;
  y: number;
}

interface DrawBase {
  v: 1;
  type: 'begin' | 'points' | 'end' | 'clear' | 'text';
  ownerIdentity: string;
  windowId: number;
  seq: number;
  points: DrawPoint[];
}

export type DrawMessage =
  | (DrawBase & {
      type: 'begin' | 'points' | 'end';
      strokeId: string;
    })
  | (DrawBase & {
      type: 'text';
      strokeId: string;
      points: [DrawPoint];
      text: string;
    })
  | (DrawBase & {
      type: 'clear';
      strokeId: null;
      points: [];
    });

export interface RemoteControlModifiers {
  alt: boolean;
  ctrl: boolean;
  meta: boolean;
  shift: boolean;
}
export type RemoteControlTargetKind = 'window' | 'display';

export type RemoteControlCapability =
  | 'legacyControl'
  | 'discretePointerV1'
  | 'discreteScrollV1'
  | 'windowLocalPointer'
  | 'globalKeyboard'
  | 'uiaInvoke'
  | 'uiaScroll'
  | 'unicodeText';

export type RemoteControlReason =
  | 'controllerUpgradeRequired'
  | 'requestEscalation'
  | 'consentDenied'
  | 'consentTimedOut';


interface RemoteControlBase {
  v: 1;
  targetUserId: string;
  controllerId: string;
  windowId: number;
  seq: number;
  /** Missing means a legacy window target; it never implies a display. */
  targetKind?: RemoteControlTargetKind;
  /** Opaque identity of one live capture/publication instance. */
  shareInstanceId?: string;
  /** Sent by controllers on requests. Unknown values are ignored. */
  controllerCapabilities?: RemoteControlCapability[];
  /** Returned by hosts on accepted grants. Unknown values are ignored. */
  hostCapabilities?: RemoteControlCapability[];
  /** Optional additive status metadata. */
  reason?: RemoteControlReason;
  /** Capability returned by an active grant; omitted by request/release and old peers. */
  grantToken?: string;
  /** Additive v2 fields; absent means the peer is legacy/result-incompatible. */
  controlSessionId?: string;
  inputId?: string;
  inputSeq?: number;
  operationFingerprintVersion?: 1;
  operationFingerprint?: string;
}

export type RemoteControlMessage =
  | (RemoteControlBase & {
      kind: 'request' | 'release';
    })
  | (RemoteControlBase & {
      kind: 'status';
      status:
        | 'active'
        | 'stopped'
        | 'disabled'
        | 'accessibilityDenied'
        | 'targetPaused'
        | 'targetUnavailable'
        | 'requestUnavailable'
        | 'requestFailed'
        | 'textTruncated'
        | 'notForeground'
        | 'occluded'
        | 'integrityBlocked'
        | 'secureField'
        | 'unsupportedRoute'
        | 'staleShareInstance'
        | 'injectionTimeout'
        | 'awaitingConsent'
        | 'denied'
      message: string;
      controlSessionId?: string;
      resultCapability?: {
        version: 2;
        retryEnabled: boolean;
        retryDeadlineMs: number;
        dedupGuaranteeWindowMs: number;
      };
      /** #370 corrective pass: present and true ONLY on an "active" status
       * packet from a host running the corrective-pass code -- its mere
       * presence is the capability signal a controller uses to decide
       * whether it may switch pointer/wheel sends to the binary hot path
       * for this (windowId, targetUserId) session. Absent on any packet
       * from a not-yet-upgraded host. */
      supportsBinaryHotPath?: boolean;
    })
  | (RemoteControlBase & {
      kind: 'pointer';
      action: 'move' | 'down' | 'up' | 'click';
      x: number;
      y: number;
      button: number;
      buttons: number;
      /** #373: authoritative multi-click count (mirrors DOM `detail`), additive/optional. */
      clickCount?: number;
      modifiers: RemoteControlModifiers;
    })
  | (RemoteControlBase & {
      kind: 'wheel';
      x: number;
      y: number;
      deltaX: number;
      deltaY: number;
      deltaMode: 0 | 1 | 2;
      modifiers: RemoteControlModifiers;
    })
  | (RemoteControlBase & {
      kind: 'key';
      action: 'down' | 'up';
      key: string;
      code: string;
      repeat: boolean;
      location?: number;
      modifiers: RemoteControlModifiers;
    })
  | (RemoteControlBase & {
      kind: 'text';
      text: string;
      modifiers: RemoteControlModifiers;
    })
  | (RemoteControlBase & {
      kind: 'result';
      controlSessionId: string;
      inputId: string;
      inputSeq: number;
      operationFingerprintVersion: 1;
      operationFingerprint: string;
      outcome:
        | 'applied'
        | 'unauthorized'
        | 'submitted'
        | 'grantExpired'
        | 'targetUnavailable'
        | 'targetOffScreen'
        | 'accessibilityDenied'
        | 'resolveFailed'
        | 'replayFailed'
        | 'superseded'
        | 'malformed'
        | 'admissionOverloaded';
      /** Optional host stage for a correlated v2 terminal result. */
      deliveryRoute?: 'admission' | 'resolve' | 'replay';
      /** Optional privacy-safe reason code. Unknown future values are ignored. */
      failureCode?:
        | 'unauthorized'
        | 'accessibilityDenied'
        | 'grantExpired'
        | 'targetOffScreen'
        | 'targetUnavailable'
        | 'notForeground'
        | 'occluded'
        | 'integrityBlocked'
        | 'secureField'
        | 'unsupportedRoute'
        | 'staleShareInstance'
        | 'resolveFailed'
        | 'replayFailed'
        | 'injectionTimeout'
        | 'superseded'
        | 'malformed'
        | 'admissionOverloaded';
    });

/**
 * The `status` variant alone. It is the only one carrying the grant and
 * capability-negotiation fields (`grantToken`, `controlSessionId`,
 * `hostCapabilities`, `resultCapability`), so anything reasoning about a grant
 * takes this rather than the full union.
 */
export type RemoteControlStatusMessage = Extract<RemoteControlMessage, { kind: 'status' }>;
