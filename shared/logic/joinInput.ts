// SINGLE SOURCE OF TRUTH for join-input parsing, shared by the desktop app
// (apps/desktop/src/lib/data/joinInput.ts re-exports this) and the web client
// (web-harness imports it directly). The web client's `parseJoinInput` is the
// canonical form (resolves the internal credential); the desktop app keeps its
// historical access-code-returning variant under `parseJoinInputAccessCode`.
//
// The join field accepts a full room access code or a pasted invite link, and
// must resolve from:
//   - a bare access code ("abc-defg-hjk")
//   - petal://join/<access-code>
//   - https://<host>/<cosmetic-label>/<access-code>
//   - legacy web URLs carrying ?code=<access-code> or #/join/<access-code>
// Labels are never sufficient.
import {
  accessCodeFromInviteInput,
  looksLikeRoomCredentialInput,
  meetingCredentialFromInviteInput,
  normalizeRoomCredential
} from './meetingCode';

export type JoinInputResult = { ok: true; code: string } | { ok: false; error: string };
export const INVALID_JOIN_INPUT_ERROR = 'Paste a full invite link or meeting code.';

export function looksLikeJoinAttempt(value: string): boolean {
  const trimmed = value.trim();
  if (looksLikeRoomCredentialInput(trimmed)) return true;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed)) return true;
  if (/^https?:/i.test(trimmed) || /^petal:/i.test(trimmed)) return true;
  if (/^\/?j\//i.test(trimmed)) return true;
  if (/[?#&]code=/i.test(trimmed) || /#\/join\//i.test(trimmed)) return true;
  return false;
}

export function parseJoinInput(raw: string): JoinInputResult {
  const trimmed = raw.trim();
  if (!trimmed) {
    return { ok: false, error: 'Enter a meeting code or paste an invite link.' };
  }

  const credential = meetingCredentialFromInviteInput(trimmed);
  return credential ? { ok: true, code: credential } : { ok: false, error: INVALID_JOIN_INPUT_ERROR };
}

/** Desktop client's historical result shape (returns the short access code). */
export type DesktopJoinInputResult = { ok: true; room: string } | { ok: false; error: string };

/** Desktop variant: resolves to the short ACCESS CODE, not the internal credential. */
export function parseJoinInputAccessCode(raw: string): DesktopJoinInputResult {
  if (!raw.trim()) {
    return { ok: false, error: 'Enter a meeting code or paste an invite link.' };
  }

  const accessCode = accessCodeFromInviteInput(raw);
  if (!accessCode) {
    return { ok: false, error: 'Paste a full invite link or meeting code.' };
  }

  return { ok: true, room: accessCode };
}

export function normalizeJoinRoomName(name: string): string | null {
  return normalizeRoomCredential(name);
}
