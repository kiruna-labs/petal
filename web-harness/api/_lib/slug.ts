// Access-code helpers for api/j.ts, copied from backend/lib/slug.ts (the
// canonical source — token/rooms minting still lives there).
//
// ⚠️ LOCKSTEP CONTRACT — this MUST stay byte-for-byte behaviorally identical to:
//   - native:  apps/desktop/src-tauri/src/rooms.rs
//   - web SPA: shared/logic/meetingCode.ts
//   - backend: backend/lib/slug.ts
// (shared/logic/meetingCode.ts's `internalCredentialForAccessCode` throws
// on an invalid code and has a client-only credential-cache side effect,
// neither of which fit this stateless server function — hence this separate,
// minimal, null-safe copy rather than importing that one.)

// Normalization is intentionally BROADER than generation: new codes never use
// visually ambiguous i/l (ACCESS_CODE_ALPHABET), but codes issued before that
// narrowing still contain them and must keep resolving. Narrowing this regex
// to the generator alphabet 400s every pre-2026-07-09 invite link.
const ACCESS_CODE_RE = /^[a-z]{3}-[a-z]{4}-[a-z]{3}$/;

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
