// Canonical room credential -> LiveKit-room-name mapping for the Petal backend.
// See docs/CONTRACTS.md; keep in sync with rooms.rs and shared/logic/meetingCode.ts.
//
// ⚠️ LOCKSTEP CONTRACT — this MUST stay byte-for-byte behaviorally identical to:
//   - native: apps/desktop/src-tauri/src/rooms.rs
//   - web:    shared/logic/meetingCode.ts
// A credential is internal-only `room-<32 lowercase hex chars>`. The copied
// invite carries only the short access code; clients derive this hidden room id.
//
// Algorithm: trim, lowercase, collapse every run of non-[a-z0-9] to a single
// '-', trim leading/trailing '-', empty result falls back to "room".

import { randomBytes } from 'node:crypto';

export function slugify(name: string): string {
  const slug = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return slug === '' ? 'room' : slug;
}

const CREDENTIAL_RE = /^room-[0-9a-f]{32}$/;
const ACCESS_CODE_RE = /^[a-z]{3}-[a-z]{4}-[a-z]{3}$/;
// Keep generated codes in lockstep with native + web clients. Normalization
// remains intentionally broader for compatibility with previously-issued
// codes, but new codes never use visually ambiguous i/l.
export const ACCESS_CODE_ALPHABET = 'abcdefghjkmnopqrstuvwxyz';

export function normalizeRoomCredential(code: string): string | null {
  const normalized = code.trim().toLowerCase();
  return CREDENTIAL_RE.test(normalized) ? normalized : null;
}

export function roomLabelFromCredential(code: string): string | null {
  const credential = normalizeRoomCredential(code);
  if (!credential) return null;
  return 'room';
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

export function normalizeAccessCode(input: string): string | null {
  const compact = input.trim().toLowerCase().replace(/-/g, '');
  if (!/^[a-z]{10}$/.test(compact)) return null;
  const code = `${compact.slice(0, 3)}-${compact.slice(3, 7)}-${compact.slice(7)}`;
  return ACCESS_CODE_RE.test(code) ? code : null;
}

export function credentialForAccessCode(accessCode: string): string | null {
  const code = normalizeAccessCode(accessCode);
  return code ? `room-${fnv1a128Hex(code)}` : null;
}

export function generateAccessCode(existing: Iterable<string> = []): string {
  const bytes = randomBytes(10);
  const used = new Set(Array.from(existing, normalizeAccessCode).filter((value): value is string => value !== null));
  for (let attempt = 0; attempt < 100; attempt++) {
    if (attempt > 0) randomBytes(10).copy(bytes);
    const chars = Array.from(bytes, (b) => ACCESS_CODE_ALPHABET[b % ACCESS_CODE_ALPHABET.length]!);
    const code = `${chars.slice(0, 3).join('')}-${chars.slice(3, 7).join('')}-${chars.slice(7).join('')}`;
    if (!used.has(code)) return code;
  }
  throw new Error('could not generate a unique access code');
}

export function generateRoomCredential(_label: string, existingAccessCodes: Iterable<string> = []): string {
  return credentialForAccessCode(generateAccessCode(existingAccessCodes))!;
}

export function livekitRoomName(code: string): string {
  const credential = normalizeRoomCredential(code);
  if (!credential) {
    throw new Error('room credential must include a 128-bit capability suffix');
  }
  return `petal-room-${credential}`;
}
