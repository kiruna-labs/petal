// SINGLE SOURCE OF TRUTH for meeting-code / credential logic, shared by the
// desktop app (apps/desktop/src/lib/data/meetingCode.ts re-exports this) and
// the web client (web-harness imports it directly).
//
// LOCKSTEP: keep in sync with apps/desktop/src-tauri/src/rooms.rs and
// backend/lib/slug.ts — docs/CONTRACTS.md. `slugify` must match
// `rooms.rs::slugify`, `livekitRoomName` must match
// `rooms.rs::livekit_room_name_for`, and `normalizeAccessCode` /
// `normalizeRoomCredential` must match the backend's. The access-code
// alphabet excludes visually ambiguous i/l for NEW codes, but normalization
// intentionally accepts them for compatibility with previously-issued codes
// (backend/lib/slug.ts's `ACCESS_CODE_RE` is `/^[a-z]{3}-[a-z]{4}-[a-z]{3}$/`).

const ACCESS_CODE_ATTEMPT_RE = /^[a-z]{3}-?[a-z]{4}-?[a-z]{3}$/i;
export const ACCESS_CODE_ALPHABET = 'abcdefghjkmnopqrstuvwxyz';
const accessCodesByCredential = new Map<string, string>();

const CREDENTIAL_RE = /^room-[0-9a-f]{32}$/;
const CREDENTIAL_ATTEMPT_RE = /^room-[a-z0-9]{32}$/i;

function randomAccessCode(): string {
  const bytes = new Uint8Array(10);
  crypto.getRandomValues(bytes);
  const chars = Array.from(bytes, (b) => ACCESS_CODE_ALPHABET[b % ACCESS_CODE_ALPHABET.length]!);
  return `${chars.slice(0, 3).join('')}-${chars.slice(3, 7).join('')}-${chars.slice(7).join('')}`;
}

function fnv1a128Hex(input: string): string {
  let hash = 0x6c62272e07bb014262b821756295c58dn;
  const prime = 0x0000000001000000000000000000013bn;
  const mask = (1n << 128n) - 1n;
  for (const byte of new TextEncoder().encode(input)) {
    hash ^= BigInt(byte);
    hash = (hash * prime) & mask;
  }
  return hash.toString(16).padStart(32, '0');
}

function safeDecodeURIComponent(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/** Generates a new internal room credential backed by a short access code. */
export function generateMeetingCode(_nameOrLabel?: string): string {
  const accessCode = generateAccessCode(accessCodesByCredential.values());
  const credential = internalCredentialForAccessCode(accessCode);
  accessCodesByCredential.set(credential, accessCode);
  return credential;
}

export function generateAccessCode(existing: Iterable<string> = []): string {
  const used = new Set(Array.from(existing, normalizeAccessCode).filter((v): v is string => v !== null));
  for (let i = 0; i < 100; i++) {
    const code = randomAccessCode();
    if (!used.has(code)) return code;
  }
  throw new Error('could not generate a unique access code');
}

/**
 * Normalizes a user-typed access code ("abc-defg-hjk", "abcdefghjk", with
 * surrounding whitespace / uppercase). Accepts any lowercase letter —
 * including i/l — for compatibility with previously-issued codes; new codes
 * never contain them (see ACCESS_CODE_ALPHABET). Returns null when the input
 * is not 3-4-3 shaped letters.
 */
export function normalizeAccessCode(input: string): string | null {
  const compact = input.trim().toLowerCase().replace(/-/g, '');
  if (!/^[a-z]{10}$/.test(compact)) return null;
  return `${compact.slice(0, 3)}-${compact.slice(3, 7)}-${compact.slice(7)}`;
}

export function internalCredentialForAccessCode(accessCode: string): string {
  const code = normalizeAccessCode(accessCode);
  if (!code) throw new Error('invalid access code');
  const credential = `room-${fnv1a128Hex(code)}`;
  accessCodesByCredential.set(credential, code);
  return credential;
}

export function accessCodeForCredential(credential: string): string | null {
  const normalizedCredential = normalizeRoomCredential(credential);
  return normalizedCredential ? accessCodesByCredential.get(normalizedCredential) ?? null : null;
}

/**
 * Re-seeds the in-memory credential -> access-code map from a previously
 * persisted pair (e.g. a stored recent-room record). The map only lives for
 * one page load and is otherwise populated only by generating a fresh code
 * or parsing a typed one -- without this, rejoining a room via the recent-
 * rooms list (which passes the internal credential directly, never re-typing
 * the code) leaves the one-way credential hash unrecoverable, so any invite
 * link built from it silently degrades to the bare origin.
 */
export function registerAccessCodeForCredential(credential: string, accessCode: string): void {
  const normalizedCredential = normalizeRoomCredential(credential);
  const normalizedCode = normalizeAccessCode(accessCode);
  if (!normalizedCredential || !normalizedCode) return;
  accessCodesByCredential.set(normalizedCredential, normalizedCode);
}

/** Normalizes user-typed codes: trims whitespace, lowercases. */
export function normalizeMeetingCode(input: string): string {
  return input.trim().toLowerCase();
}

/**
 * Slugify an arbitrary human label into a stable, LiveKit-safe prefix. MUST
 * stay byte-for-byte identical in behavior to the native app's `rooms::slugify`
 * (apps/desktop/src-tauri/src/rooms.rs): lowercase, every run of non-[a-z0-9]
 * collapsed to a single '-', leading/trailing '-' trimmed, and an empty result
 * falls back to "room". It is only the readable part of a credential; the full
 * join value must also include the 32-hex capability suffix.
 */
export function slugify(name: string): string {
  const slug = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return slug === '' ? 'room' : slug;
}

/** Desktop app's historical name for `slugify` (same behavior). */
export function slugifyMeetingCode(name: string): string {
  return slugify(name);
}

export function normalizeRoomCredential(input: string): string | null {
  const normalized = normalizeMeetingCode(input);
  return CREDENTIAL_RE.test(normalized) ? normalized : null;
}

/** Desktop app's historical name for `normalizeRoomCredential`. */
export function normalizeMeetingCredential(input: string): string | null {
  return normalizeRoomCredential(input);
}

export function looksLikeRoomCredentialInput(input: string): boolean {
  const trimmed = input.trim();
  return ACCESS_CODE_ATTEMPT_RE.test(trimmed) || CREDENTIAL_ATTEMPT_RE.test(trimmed);
}

/** Desktop app's historical name for `looksLikeRoomCredentialInput`. */
export function looksLikeMeetingCredentialInput(input: string): boolean {
  return looksLikeRoomCredentialInput(input);
}

export function meetingDisplayLabelFromCredential(input: string): string | null {
  const credential = normalizeRoomCredential(input);
  return credential ? null : null;
}

function credentialFromPetalUrl(input: string): string | null {
  let url: URL;
  try {
    url = new URL(input);
  } catch {
    return null;
  }
  if (url.protocol.toLowerCase() !== 'petal:' || url.hostname.toLowerCase() !== 'join') {
    return null;
  }
  const segments = url.pathname.split('/').filter(Boolean).map(safeDecodeURIComponent);
  const accessCode = segments.length === 1 ? normalizeAccessCode(segments[0]!) : null;
  return accessCode ? internalCredentialForAccessCode(accessCode) : null;
}

function credentialFromWebUrl(url: URL): string | null {
  const segments = url.pathname.split('/').filter(Boolean).map(safeDecodeURIComponent);
  if (segments.length === 1 || segments.length === 2) {
    const accessCode = normalizeAccessCode(segments[segments.length - 1]!);
    return accessCode ? internalCredentialForAccessCode(accessCode) : null;
  }
  const code = url.searchParams.get('code');
  if (code !== null) {
    const accessCode = normalizeAccessCode(code);
    return accessCode ? internalCredentialForAccessCode(accessCode) : null;
  }
  const hashMatch = /^#\/join\/([^/?#]+)\/?(?:[?#].*)?$/i.exec(url.hash);
  if (!hashMatch) return null;
  const accessCode = normalizeAccessCode(safeDecodeURIComponent(hashMatch[1]!));
  return accessCode ? internalCredentialForAccessCode(accessCode) : null;
}

function credentialFromRelativeInvitePath(input: string): string | null {
  const path = input.startsWith('/') ? input : `/${input}`;
  const segments = path.split('/').filter(Boolean).map(safeDecodeURIComponent);
  if (segments.length === 1 || segments.length === 2) {
    const accessCode = normalizeAccessCode(segments[segments.length - 1]!);
    return accessCode ? internalCredentialForAccessCode(accessCode) : null;
  }
  return null;
}

/**
 * Extracts the internal credential from supported invite inputs:
 * - raw "abc-defg-hjk" access code
 * - "https://.../<name-or-label>/<access-code>" (name segment is display-only)
 * - "petal://join/<access-code>"
 * - legacy web "?code=<access-code>" or "#/join/<access-code>" URLs
 */
export function meetingCredentialFromInviteInput(input: string): string | null {
  const trimmed = input.trim();
  if (!trimmed) return null;

  if (/^petal:/i.test(trimmed)) {
    return credentialFromPetalUrl(trimmed);
  }

  if (/^https?:\/\//i.test(trimmed)) {
    let url: URL;
    try {
      url = new URL(trimmed);
    } catch {
      return null;
    }
    return credentialFromWebUrl(url);
  }

  if (trimmed.startsWith('/')) {
    return credentialFromRelativeInvitePath(trimmed);
  }

  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed)) {
    return null;
  }

  const accessCode = normalizeAccessCode(trimmed);
  return accessCode ? internalCredentialForAccessCode(accessCode) : null;
}

export function accessCodeFromInviteInput(input: string): string | null {
  const trimmed = input.trim();
  if (!trimmed) return null;

  if (/^petal:/i.test(trimmed)) {
    try {
      const url = new URL(trimmed);
      if (url.protocol.toLowerCase() !== 'petal:' || url.hostname.toLowerCase() !== 'join') return null;
      const segments = url.pathname.split('/').filter(Boolean).map(safeDecodeURIComponent);
      return segments.length === 1 ? normalizeAccessCode(segments[0]!) : null;
    } catch {
      return null;
    }
  }

  if (/^https?:\/\//i.test(trimmed)) {
    try {
      const url = new URL(trimmed);
      const segments = url.pathname.split('/').filter(Boolean).map(safeDecodeURIComponent);
      if (segments.length === 1 || segments.length === 2) return normalizeAccessCode(segments[segments.length - 1]!);
      const code = url.searchParams.get('code');
      if (code !== null) return normalizeAccessCode(code);
      const hashMatch = /^#\/join\/([^/?#]+)\/?(?:[?#].*)?$/i.exec(url.hash);
      return hashMatch ? normalizeAccessCode(safeDecodeURIComponent(hashMatch[1]!)) : null;
    } catch {
      return null;
    }
  }

  if (trimmed.startsWith('/')) {
    const segments = trimmed.split('/').filter(Boolean).map(safeDecodeURIComponent);
    if (segments.length === 1 || segments.length === 2) return normalizeAccessCode(segments[segments.length - 1]!);
  }

  return normalizeAccessCode(trimmed);
}

export function buildMeetingInvitePath(
  nameOrLabel: string | null | undefined,
  accessCode: string | null | undefined
): string | null {
  if (!accessCode) return null;
  const normalizedAccessCode = normalizeAccessCode(accessCode);
  if (!normalizedAccessCode) return null;

  const label = nameOrLabel?.trim() ? slugifyMeetingCode(nameOrLabel) : null;
  return label
    ? `/${encodeURIComponent(label)}/${encodeURIComponent(normalizedAccessCode)}`
    : `/${encodeURIComponent(normalizedAccessCode)}`;
}

/**
 * The canonical credential -> LiveKit-room-name mapping. MUST match the native
 * app's `rooms::livekit_room_name_for` exactly (`petal-room-<credential>`), or
 * the native app and this harness will connect to different LiveKit rooms.
 */
export function livekitRoomName(code: string): string {
  const credential = normalizeRoomCredential(code);
  if (!credential) throw new Error('room credential must include a capability suffix');
  return `petal-room-${credential}`;
}
