// SINGLE SOURCE OF TRUTH for the reply address every feedback form requires
// (#245), shared by the desktop app (apps/desktop/src/lib/feedback/email.ts
// re-exports this) and the web client (web-harness/src/feedbackReport.ts
// imports it directly) so the two forms can never disagree on what they accept
// or on what they remember.
//
// A FORMAT check only. Petal never sends a confirmation mail or otherwise
// verifies the address -- it exists so a report has somewhere to send a reply.
// Deliberately loose: real addresses take far more shapes than any short rule
// describes (internationalized domains, plus-addressing, quoted local parts),
// and a stricter one would only turn real people away.

/** The longest address a mail path can carry (RFC 5321's 256-octet path,
 * minus its angle brackets). */
export const FEEDBACK_EMAIL_MAX_CHARS = 254;

// Characters no deliverable address the form should accept contains: list
// separators and display-name/comment syntax (`a@b.org, c@d.org`,
// `Riley <riley@example.org>`, `riley(work)@example.org`), path separators,
// and invisible format characters (zero-width spaces pasted along with an
// address).
const FORBIDDEN_CHARS = /[<>,;/\\()]|\p{Cf}/u;

/** Unwraps what a pasted address commonly carries: one pair of surrounding
 * angle brackets and a leading `mailto:`. */
function unwrapped(value: string): string {
  let email = value.trim();
  if (email.startsWith('<') && email.endsWith('>')) email = email.slice(1, -1).trim();
  return email.replace(/^mailto:/i, '').trim();
}

/**
 * The normalized address when it is well-formed, otherwise `null`. This is
 * the value to send and to remember, never the raw field text. Well-formed
 * means: exactly one `@`, a non-empty local part, a domain of at least two
 * non-empty dot-separated labels, no whitespace or `FORBIDDEN_CHARS`
 * anywhere, and at most `FEEDBACK_EMAIL_MAX_CHARS` characters.
 */
export function normalizedFeedbackEmail(value: string | null | undefined): string | null {
  const email = unwrapped(value ?? '');
  if (email.length === 0 || email.length > FEEDBACK_EMAIL_MAX_CHARS) return null;
  if (/\s/.test(email) || FORBIDDEN_CHARS.test(email)) return null;
  const at = email.indexOf('@');
  if (at <= 0 || at !== email.lastIndexOf('@')) return null;
  return /^[^.]+(\.[^.]+)+$/.test(email.slice(at + 1)) ? email : null;
}

export function isValidFeedbackEmail(value: string | null | undefined): boolean {
  return normalizedFeedbackEmail(value) !== null;
}

/** Inline error for the field, or `null` when there is nothing to say. */
export function feedbackEmailError(value: string | null | undefined): string | null {
  const email = unwrapped(value ?? '');
  if (!email) return 'Enter your email address.';
  if (email.length > FEEDBACK_EMAIL_MAX_CHARS) return 'This email address is too long.';
  return isValidFeedbackEmail(email) ? null : 'Enter a valid email address, like name@example.com.';
}

/** Where a client remembers the last address sent (web `localStorage`, the
 * desktop webview's app storage). */
export interface FeedbackEmailStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/** The remembered address, or `''` when none (or no usable one) is stored.
 * Never throws: storage can be missing or blocked. */
export function rememberedFeedbackEmail(
  storage: Pick<FeedbackEmailStorage, 'getItem'> | null | undefined,
  key: string
): string {
  try {
    return normalizedFeedbackEmail(storage?.getItem(key)) ?? '';
  } catch {
    return '';
  }
}

/** Remembers the normalized address; anything malformed is ignored. Never
 * throws: the form still works, it just starts empty next time. */
export function rememberFeedbackEmail(
  storage: Pick<FeedbackEmailStorage, 'setItem'> | null | undefined,
  key: string,
  value: string
): void {
  const email = normalizedFeedbackEmail(value);
  if (!email) return;
  try {
    storage?.setItem(key, email);
  } catch {
    // Best-effort (quota, blocked site data).
  }
}
