import { normalizedPointInContainedMedia } from './telepointer.ts';
import {
  REMOTE_CONTROL_TOPIC,
  type RemoteControlMessage,
  type RemoteControlModifiers,
  type RemoteControlStatusMessage
} from './trackNames.ts';

interface ModifierLike {
  altKey: boolean;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
}

export function remoteControlModifiers(event: ModifierLike): RemoteControlModifiers {
  return {
    alt: event.altKey,
    ctrl: event.ctrlKey,
    meta: event.metaKey,
    shift: event.shiftKey
  };
}

export type KeyChordLike = ModifierLike & {
  key?: string;
  code?: string;
};

/**
 * #375: does this key event match the Cmd+V paste chord? Mirrors the host's
 * `classify_text_shortcut` Paste branch (Rust, apps/desktop/src-tauri/src/
 * remote_control.rs) -- "meta only, no shift/ctrl/alt", matching the logical
 * `key` first (layout-independent) and falling back to the physical `code`
 * only when `key` is empty. When this matches, the controller pastes its OWN
 * clipboard as a `text` message instead of forwarding the raw key event --
 * forwarding the raw event too would double-paste, since the host's
 * classify_text_shortcut would ALSO fire its target-clipboard AX paste path.
 */
export function isPasteChord(event: KeyChordLike): boolean {
  if (!event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return false;
  const key = event.key ?? '';
  if (key) return key.toLowerCase() === 'v';
  return event.code === 'KeyV';
}

// #892: single implementation now lives in telepointer.ts (re-exported here
// so existing `from './remoteControl.ts'` imports, e.g. telepointerSender.ts
// and remoteControlUi.ts, keep working unchanged).
export { normalizedPointInContainedMedia };

function isV2DiscreteAttempt(message: RemoteControlMessage): boolean {
  return (
    message.controlSessionId !== undefined ||
    message.inputId !== undefined ||
    message.inputSeq !== undefined ||
    message.operationFingerprintVersion !== undefined ||
    message.operationFingerprint !== undefined ||
    message.targetKind !== undefined ||
    message.shareInstanceId !== undefined ||
    (message.controllerCapabilities?.length ?? 0) > 0 ||
    (message.hostCapabilities?.length ?? 0) > 0
  );
}

function isCapableWheel(message: RemoteControlMessage): boolean {
  return (
    message.kind === 'wheel' &&
    typeof message.controlSessionId === 'string' &&
    typeof message.inputId === 'string' &&
    typeof message.inputSeq === 'number' &&
    message.operationFingerprintVersion === 1 &&
    typeof message.operationFingerprint === 'string' &&
    (message.targetKind === 'window' || message.targetKind === 'display') &&
    typeof message.shareInstanceId === 'string'
  );
}

export function remoteControlPublishOptions(message: RemoteControlMessage): {
  reliable: boolean;
  topic: typeof REMOTE_CONTROL_TOPIC;
  destinationIdentities?: string[];
} {
  const capableWheel = isCapableWheel(message);
  return {
    topic: REMOTE_CONTROL_TOPIC,
    // Legacy hover/wheel streams are self-replacing. A capable wheel carries
    // one exactly-once operation envelope and must be ordered/reliable.
    reliable: !(
      (message.kind === 'pointer' && message.action === 'move' && message.buttons === 0) ||
      (message.kind === 'wheel' && !capableWheel)
    ),
    // #370 corrective pass (Bug B): scope LiveKit delivery to only the
    // intended recipient, mirroring the pattern `harnessApi.ts`'s
    // latency-probe publish already uses. This matters independently of
    // whatever the wire frame itself carries: without it, every room
    // participant receives every pointer/wheel/status/etc. packet and the
    // Rust receiver's `target_user_id != local_identity` JSON-path filter
    // (or, for binary frames -- which carry no `targetUserId` on the wire at
    // all -- the fact that the receiver just FABRICATES
    // `target_user_id = local_identity`) is the only thing standing between
    // "delivered to everyone" and "processed by everyone."
    ...(message.targetUserId ? { destinationIdentities: [message.targetUserId] } : {})
  };
}

export const REMOTE_CONTROL_BINARY_MAGIC = 0x50;
/**
 * #370 corrective pass: grew from 23 to 27 bytes to append a 4-byte
 * little-endian `tokenFingerprint` (FNV-1a32 of the sender's live grant
 * token) -- closes the bug where the original 23-byte frame had no room for
 * grant material, so the Rust receiver treated EVERY binary hot-path packet
 * as tokenless and let it through a compatibility window meant for old JSON
 * clients, not this wire variant. Keep in lockstep with
 * `apps/desktop/src-tauri/src/remote_control.rs`'s `BINARY_FRAME_LEN`.
 */
export const REMOTE_CONTROL_BINARY_LENGTH = 27;

/**
 * FNV-1a, 32-bit, over raw bytes. Pure/stateless so it can be reimplemented
 * identically in Rust (`apps/desktop/src-tauri/src/remote_control.rs::fnv1a32`)
 * -- keep both in lockstep; a pinned test vector on both sides
 * (`contracts/petal-contracts.json`'s `fnv1a32TestVectors`) guards against
 * silent divergence. Standard constants: offset basis `0x811c9dc5`, prime
 * `0x01000193`.
 */
export function fnv1a32(bytes: Uint8Array): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < bytes.length; i += 1) {
    hash ^= bytes[i];
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

export function fixedPointCoordinateKey(x: number, y: number): string {
  const quantize = (value: number) => Math.round(Math.min(1, Math.max(0, value)) * 0xffff);
  return `${quantize(x)}:${quantize(y)}`;
}

/**
 * Encodes only lossy pointer moves and legacy wheel packets; all discrete
 * input stays JSON. Returns null (falls back to JSON) for any v2 wheel or
 * whenever the message has no grant token to fingerprint: the 27-byte frame
 * cannot carry a v2 admission envelope or a real grant token.
 */
export function encodeRemoteControlHotPath(message: RemoteControlMessage): Uint8Array | null {
  const isPointerMove = message.kind === 'pointer' && message.action === 'move';
  const isWheel = message.kind === 'wheel';
  if ((!isPointerMove && !isWheel) || (isWheel && isV2DiscreteAttempt(message))) return null;
  if (typeof message.grantToken !== 'string' || message.grantToken.length === 0) return null;
  const hot = message as Extract<RemoteControlMessage, { kind: 'pointer' | 'wheel' }>;
  const out = new Uint8Array(REMOTE_CONTROL_BINARY_LENGTH);
  const view = new DataView(out.buffer);
  out[0] = REMOTE_CONTROL_BINARY_MAGIC; out[1] = 1;
  out[2] = isPointerMove ? 4 : 5; out[3] = isPointerMove ? 1 : 0;
  view.setUint32(4, message.seq >>> 0, true); view.setUint32(8, message.windowId >>> 0, true);
  view.setUint16(12, Math.round(Math.min(1, Math.max(0, hot.x)) * 0xffff), true);
  view.setUint16(14, Math.round(Math.min(1, Math.max(0, hot.y)) * 0xffff), true);
  out[16] = Math.min(255, isPointerMove ? (hot as Extract<RemoteControlMessage, { kind: 'pointer' }>).buttons ?? 0 : 0);
  out[17] = (hot.modifiers.alt ? 1 : 0) | (hot.modifiers.ctrl ? 2 : 0) |
    (hot.modifiers.meta ? 4 : 0) | (hot.modifiers.shift ? 8 : 0);
  view.setInt16(18, Math.max(-32768, Math.min(32767, Math.round(isWheel ? (hot as Extract<RemoteControlMessage, { kind: 'wheel' }>).deltaX : 0))), true);
  view.setInt16(20, Math.max(-32768, Math.min(32767, Math.round(isWheel ? (hot as Extract<RemoteControlMessage, { kind: 'wheel' }>).deltaY : 0))), true);
  out[22] = isWheel ? (hot as Extract<RemoteControlMessage, { kind: 'wheel' }>).deltaMode : 0;
  view.setUint32(23, fnv1a32(new TextEncoder().encode(message.grantToken)), true);
  return out;
}

/**
 * Decodes a binary hot-path frame. NOTE: unlike the Rust receiver, this does
 * NOT verify `tokenFingerprint` against an active grant -- web-harness never
 * acts as a remote-control HOST (it has no input-replay/injection surface;
 * `handleRemoteControlPayload` never dispatches on a decoded `pointer`/
 * `wheel` kind, only `status`/`result`), so there is no local authoritative
 * grant-token store to check it against. The decoded message's `grantToken`
 * is therefore left unset, matching the pre-#370 shape; only the REAL Rust
 * host, which does own that state (`active_grant_token`), is authoritative
 * for admitting a hot-path frame.
 */
export function decodeRemoteControlHotPath(payload: Uint8Array, targetUserId: string, controllerId: string): RemoteControlMessage | null {
  if (payload.length !== REMOTE_CONTROL_BINARY_LENGTH || payload[0] !== REMOTE_CONTROL_BINARY_MAGIC || payload[1] !== 1) return null;
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const kind = payload[2] === 4 ? 'pointer' : payload[2] === 5 ? 'wheel' : null;
  if (!kind) return null;
  const modifiers = { alt: !!(payload[17] & 1), ctrl: !!(payload[17] & 2), meta: !!(payload[17] & 4), shift: !!(payload[17] & 8) };
  const base = { v: 1 as const, targetUserId, controllerId, windowId: view.getUint32(8, true), seq: view.getUint32(4, true), x: view.getUint16(12, true) / 0xffff, y: view.getUint16(14, true) / 0xffff, modifiers };
  return kind === 'pointer'
    ? { ...base, kind, action: 'move', button: -1, buttons: payload[16] }
    : { ...base, kind, deltaX: view.getInt16(18, true), deltaY: view.getInt16(20, true), deltaMode: payload[22] as 0 | 1 | 2 };
}

/** Parses one JSON fallback packet; unknown enum values are ignored, not fatal to the stream. */
export function parseRemoteControlJson(payload: string): RemoteControlMessage | null {
  let value: Record<string, unknown>;
  try { value = JSON.parse(payload) as Record<string, unknown>; } catch { return null; }
  const kinds = ['request', 'release', 'status', 'pointer', 'wheel', 'key', 'text', 'result'];
  if (value.v !== 1 || typeof value.kind !== 'string' || !kinds.includes(value.kind)) return null;
  if (value.action !== undefined && !['move', 'down', 'up', 'click'].includes(String(value.action))) return null;
  if (value.targetKind !== undefined && !['window', 'display'].includes(String(value.targetKind))) {
    if (value.kind === 'request') delete value.targetKind;
    else return null;
  }
  const knownCapabilities = [
    'legacyControl',
    'discretePointerV1',
    'discreteScrollV1',
    'windowLocalPointer',
    'globalKeyboard',
    'uiaInvoke',
    'uiaScroll',
    'unicodeText'
  ];
  for (const field of ['controllerCapabilities', 'hostCapabilities'] as const) {
    if (value[field] === undefined) continue;
    if (!Array.isArray(value[field])) {
      delete value[field];
      continue;
    }
    value[field] = value[field].filter(
      (capability): capability is string =>
        typeof capability === 'string' && knownCapabilities.includes(capability)
    );
  }
  if (
    value.reason !== undefined &&
    !['controllerUpgradeRequired', 'requestEscalation', 'consentDenied', 'consentTimedOut'].includes(
      String(value.reason)
    )
  ) {
    delete value.reason;
  }
  if (
    value.kind === 'status' &&
    ![
      'active',
      'stopped',
      'disabled',
      'accessibilityDenied',
      'requestFailed',
      'targetPaused',
      'targetUnavailable',
      'requestUnavailable',
      'textTruncated',
      'notForeground',
      'occluded',
      'integrityBlocked',
      'secureField',
      'unsupportedRoute',
      'staleShareInstance',
      'injectionTimeout',
      'awaitingConsent',
      'denied'
    ].includes(String(value.status))
  )
    return null;
  if (value.kind === 'result') {
    // Delivery metadata is additive. Keep a correlated, known terminal
    // outcome usable when a newer peer sends a route/code this build does not
    // understand (#446).
    if (value.deliveryRoute !== undefined && !['admission', 'resolve', 'replay'].includes(String(value.deliveryRoute))) delete value.deliveryRoute;
    if (
      value.failureCode !== undefined &&
      ![
        'unauthorized',
        'accessibilityDenied',
        'grantExpired',
        'targetOffScreen',
        'targetUnavailable',
        'notForeground',
        'occluded',
        'integrityBlocked',
        'secureField',
        'unsupportedRoute',
        'staleShareInstance',
        'resolveFailed',
        'replayFailed',
        'injectionTimeout',
        'superseded',
        'malformed',
        'admissionOverloaded'
      ].includes(String(value.failureCode))
    )
      delete value.failureCode;
    // Successful dispositions cannot simultaneously carry a failure code.
    if (value.outcome === 'applied' || value.outcome === 'submitted') delete value.failureCode;
  }
  return value as unknown as RemoteControlMessage;
}

/** A status may update a grant only when its additive target envelope is whole. */
export function remoteControlGrantEnvelopeIsValid(message: RemoteControlMessage): boolean {
  const hasEnvelope =
    message.targetKind !== undefined ||
    message.shareInstanceId !== undefined ||
    message.hostCapabilities !== undefined;
  return (
    !hasEnvelope ||
    ((message.targetKind === 'window' || message.targetKind === 'display') &&
      typeof message.shareInstanceId === 'string' &&
      message.shareInstanceId.length > 0)
  );
}

/**
 * May an inbound `active` status RE-ESTABLISH a control session the controller
 * no longer has?
 *
 * #808: it must. A `stopped` for a superseded request clears
 * `state.activeRemoteControl`, and adoption of every later `active` was gated
 * on that same object being non-null -- so once cleared, nothing could restore
 * it and the controller sat permanently out of control while the host kept
 * granting. Measured live: the harness reported `{granted:false,
 * grantToken:null}` with `active=null`, two milliseconds after a `stopped`,
 * against a host that had just logged `status emitted (local+controller)
 * status='active'`. The same gap defeated #371's reconnect re-emit, whose
 * entire purpose is to restore a controller whose data channel was recreated.
 *
 * The sender check is preserved rather than skipped. `targetUserId` and
 * `windowId` on the wire are attacker-controlled and are NOT authentication;
 * the restore therefore requires the LiveKit-verified `senderIdentity` to own
 * the tile for that window, and requires a real grant token. A tokenless
 * `active`, a foreign sender, or an unknown window restores nothing.
 */
export function remoteControlStatusRestoresSession(
  message: RemoteControlStatusMessage,
  context: {
    hasActiveSession: boolean;
    localIdentity: string | undefined;
    senderIdentity: string | undefined;
    tileOwner: string | undefined;
  }
): boolean {
  if (context.hasActiveSession) return false;
  if (message.status !== 'active') return false;
  if (typeof message.grantToken !== 'string' || message.grantToken.length === 0) return false;
  if (!context.localIdentity || message.targetUserId !== context.localIdentity) return false;
  if (!context.senderIdentity || context.senderIdentity !== context.tileOwner) return false;
  return remoteControlGrantEnvelopeIsValid(message);
}

/**
 * Does an inbound `active` status carry a v2 grant envelope that matches the
 * request we made? Extracted from remoteControlUi.ts for #802: this is the
 * gate that silently discarded every macOS grant token, because a host
 * advertising no capabilities serializes no `hostCapabilities` key at all and
 * fails the non-empty check -- a check that applies exactly when
 * `controlSessionId` is set, i.e. exactly when a grant EXISTS. The host-side
 * defect is fixed; keep this pure and tested so the next regression is caught
 * in CI rather than in a 30/30 live failure with no error on either side.
 */
export function remoteControlNegotiatedGrantMatchesRequest(
  // Status-only: `resultCapability` exists on no other variant, and this gate
  // is only ever reached after `message.kind !== 'status'` has returned.
  message: RemoteControlStatusMessage,
  requested: {
    targetKind?: RemoteControlMessage['targetKind'];
    shareInstanceId?: string;
  }
): boolean {
  if (!message.controlSessionId) return true;
  return (
    requested.targetKind === message.targetKind &&
    requested.shareInstanceId === message.shareInstanceId &&
    Array.isArray(message.hostCapabilities) &&
    message.hostCapabilities.length > 0 &&
    message.resultCapability?.version === 2 &&
    message.resultCapability.retryEnabled === false
  );
}

export const EMPTY_REMOTE_CONTROL_MODIFIERS: RemoteControlModifiers = {
  alt: false,
  ctrl: false,
  meta: false,
  shift: false
};



// #373: mirrors remote_control.rs's `MAX_REPLAY_TEXT_CHARS` -- the host caps
// any single `text` wire message at this many Unicode scalar values
// (`capped_replay_text`) rather than chunking it itself, so a long IME
// composition commit sent as one message would silently lose everything past
// the cap. Chunk on the sender side the same way desktop's
// `remote_control_send` command chunks outbound drafts, so the web
// controller doesn't quietly drop part of a long composed string.
export const MAX_REMOTE_TEXT_CHARS = 1000;

export function chunkRemoteText(text: string): string[] {
  // Split on Unicode scalar values (code points), matching Rust's
  // `text.chars()` -- a naive UTF-16 slice could cut a surrogate pair (e.g.
  // an emoji) in half.
  const codePoints = Array.from(text);
  if (codePoints.length === 0) return [];
  const chunks: string[] = [];
  for (let i = 0; i < codePoints.length; i += MAX_REMOTE_TEXT_CHARS) {
    chunks.push(codePoints.slice(i, i + MAX_REMOTE_TEXT_CHARS).join(''));
  }
  return chunks;
}

/**
 * The v2 fingerprint is deliberately a binary record, never JSON. JSON field
 * order, number rendering, and omitted optional values are not a wire
 * contract. Keep this encoder in lockstep with remote_control.rs.
 */
function putU32(out: number[], value: number) {
  const n = value >>> 0;
  out.push(n & 0xff, (n >>> 8) & 0xff, (n >>> 16) & 0xff, (n >>> 24) & 0xff);
}

function putU64(out: number[], value: number) {
  const n = BigInt(value);
  for (let shift = 0n; shift < 64n; shift += 8n) out.push(Number((n >> shift) & 0xffn));
}

function putString(out: number[], value: string) {
  const bytes = new TextEncoder().encode(value);
  putU32(out, bytes.length);
  out.push(...bytes);
}

function putOptionalString(out: number[], value: string | undefined) {
  out.push(value === undefined ? 0 : 1);
  if (value !== undefined) putString(out, value);
}

function putOptionalNumber(out: number[], value: number | undefined, width: 1 | 2 | 8) {
  out.push(value === undefined ? 0 : 1);
  if (value === undefined) return;
  if (width === 8) {
    const view = new DataView(new ArrayBuffer(8));
    view.setFloat64(0, value, true);
    out.push(...new Uint8Array(view.buffer));
  } else if (width === 2) {
    out.push(value & 0xff, (value >>> 8) & 0xff);
  } else {
    out.push(value & 0xff);
  }
}

function kindCode(kind: RemoteControlMessage['kind']): number {
  return ({ request: 1, release: 2, status: 3, pointer: 4, wheel: 5, key: 6, text: 7, result: 8 } as const)[kind];
}

function actionCode(action: string | undefined): number {
  return ({ move: 1, down: 2, up: 3, click: 4 } as Record<string, number | undefined>)[action ?? ''] ?? 0;
}

/** Returns the exact bytes covered by a v2 discrete-operation fingerprint. */
export function canonicalRemoteControlOperationBytes(
  message: Extract<RemoteControlMessage, { kind: 'pointer' | 'wheel' | 'key' | 'text' }>,
  grant: { controlSessionId: string; inputId: string; inputSeq: number }
): Uint8Array {
  const out: number[] = [1, message.v, kindCode(message.kind), actionCode('action' in message ? message.action : undefined)];
  putString(out, message.targetUserId);
  putString(out, message.controllerId);
  putU32(out, message.windowId);
  putString(out, grant.controlSessionId);
  putString(out, grant.inputId);
  putU64(out, grant.inputSeq);
  putOptionalNumber(out, 'x' in message ? message.x : undefined, 8);
  putOptionalNumber(out, 'y' in message ? message.y : undefined, 8);
  putOptionalNumber(out, 'button' in message ? message.button : undefined, 2);
  putOptionalNumber(out, 'buttons' in message ? message.buttons : undefined, 2);
  putOptionalString(out, 'key' in message ? message.key : undefined);
  putOptionalString(out, 'code' in message ? message.code : undefined);
  out.push('repeat' in message && message.repeat ? 1 : 0);
  putOptionalNumber(out, 'location' in message ? (message.location as number | undefined) : undefined, 1);
  putOptionalString(out, 'text' in message ? message.text : undefined);
  const modifiers = 'modifiers' in message ? message.modifiers : undefined;
  out.push(modifiers?.alt ? 1 : 0, modifiers?.ctrl ? 1 : 0, modifiers?.meta ? 1 : 0, modifiers?.shift ? 1 : 0);
  if (message.kind === 'wheel') {
    out.push(3);
    putOptionalNumber(out, message.deltaX, 8);
    putOptionalNumber(out, message.deltaY, 8);
    putOptionalNumber(out, message.deltaMode, 1);
  }
  if (message.targetKind !== undefined || message.shareInstanceId !== undefined) {
    out.push(2);
    out.push(message.targetKind === 'display' ? 2 : 1);
    putOptionalString(out, message.shareInstanceId);
  }
  return new Uint8Array(out);
}

export async function canonicalRemoteControlFingerprint(
  message: Extract<RemoteControlMessage, { kind: 'pointer' | 'wheel' | 'key' | 'text' }>,
  grant: { controlSessionId: string; inputId: string; inputSeq: number }
): Promise<string> {
  const bytes = canonicalRemoteControlOperationBytes(message, grant);
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  const digest = await crypto.subtle.digest('SHA-256', copy as unknown as BufferSource);
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('');
}

export function newRemoteControlInputId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}
