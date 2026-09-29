// ---------------------------------------------------------------------------
// PII-scrub mechanism for Sentry error reporting (#283).
//
// Room names and participant identities are arbitrary user-chosen strings
// with no fixed shape -- a regex/substring PII scrub cannot pattern-match
// them. Instead this is an allowlist-first *registry*: the app registers the
// current room name and each known participant identity as they become
// known (mirroring how connection.ts already tracks them), and every
// breadcrumb/event string that reaches Sentry is passed through `scrub()`,
// which replaces every registered value with a stable redacted label before
// it can leave the browser. `scrubSensitiveStrings` is the pure, directly
// unit-testable core -- no Sentry import, no DOM.
// ---------------------------------------------------------------------------

import { accessCodeForCredential, slugify } from '@petal/shared/logic/meetingCode';

import { roomFallbackLabelForCredential } from './roomLabels.ts';

const ROOM_LABEL = '<redacted:room>';
/**
 * Shortest user-typed room name this registry will redact. Even matched whole,
 * a one- or two-character name ("a", "on") is an ordinary word that appears
 * throughout prose, so redacting it would destroy the report rather than
 * protect anything. Generated identifiers are unaffected: they go through
 * `registerRoom`, which has no floor.
 */
const MIN_FREE_TEXT_ROOM_LENGTH = 3;

const isWordChar = (ch: string): boolean => ch !== '' && /[A-Za-z0-9]/.test(ch);

/**
 * Replace `value` with `label`, but only where it stands alone -- not where it
 * is part of a longer word. Used for names a PERSON typed: those are ordinary
 * language, so a plain substring replace rewrites unrelated text ("test" would
 * turn "latest" into "la<redacted:room>").
 *
 * Written as a scan rather than a regex to avoid both escaping the value and
 * depending on lookbehind, which older Safari does not have.
 */
function replaceWhereBounded(text: string, value: string, label: string): string {
  let out = '';
  let from = 0;
  for (;;) {
    const at = text.indexOf(value, from);
    if (at === -1) return out + text.slice(from);
    const before = at === 0 ? '' : text[at - 1]!;
    const afterAt = at + value.length;
    const after = afterAt >= text.length ? '' : text[afterAt]!;
    out += text.slice(from, at) + (isWordChar(before) || isWordChar(after) ? value : label);
    from = afterAt;
  }
}

/**
 * Replaces every occurrence of every registered value in `text` with its
 * label. Longest values are replaced first so a shorter registered value
 * that happens to be a substring of a longer one never partially matches
 * inside an already-registered longer string.
 */
export function scrubSensitiveStrings(
  text: string,
  sensitiveValues: ReadonlyMap<string, string>,
  /**
   * Values that must match a whole word to be replaced. Everything a person
   * typed belongs here; generated identifiers do not need it and are safer
   * without it, since they can sit next to punctuation the scan would treat
   * as a word character.
   */
  wholeWordOnly: ReadonlySet<string> = new Set()
): string {
  if (!text) return text;
  let result = text;
  const entries = [...sensitiveValues.entries()]
    .filter(([value]) => value.length > 0)
    .sort((a, b) => b[0].length - a[0].length);
  for (const [value, label] of entries) {
    if (!result.includes(value)) continue;
    result = wholeWordOnly.has(value)
      ? replaceWhereBounded(result, value, label)
      : result.split(value).join(label);
  }
  return result;
}

export class SensitiveStringRegistry {
  private values = new Map<string, string>(); // raw value -> redacted label
  /** Values that must match a whole word; see `registerFreeTextRoom`. */
  private wholeWordOnly = new Set<string>();
  // Upload reporting must also scrub values that appeared earlier in the
  // session. The live Sentry map intentionally drops departed participants;
  // retaining this separate snapshot prevents a historic feedback message
  // from re-exposing a name that was present when its log line was created.
  private reportingValues = new Map<string, string>();
  private participantLabels = new Map<string, string>(); // identity -> label
  private participantCounter = 0;

  /**
   * Registers a room/meeting identifier under the shared room label. A
   * single logical room shows up in log text under several distinct string
   * forms in the same session -- the user-facing access code, the wire
   * LiveKit room name, and the backend-assigned room name
   * (connection.ts:189-192) -- so this is additive (all current variants
   * stay scrubbed at once) rather than replacing the previous value. Call
   * `reset()` when the session ends so stale values don't linger.
   */
  registerRoom(room: string | null | undefined): void {
    const trimmed = room?.trim();
    if (trimmed) {
      this.values.set(trimmed, ROOM_LABEL);
      this.reportingValues.set(trimmed, ROOM_LABEL);
    }
  }

  /**
   * Registers a room name a PERSON typed, which is ordinary language rather
   * than a generated identifier. Matched whole-word only, so a meeting called
   * "test" no longer rewrites "latest" everywhere in the log, and dropped
   * entirely below `MIN_FREE_TEXT_ROOM_LENGTH`, where even a whole-word match
   * would hit common prose.
   */
  registerFreeTextRoom(label: string | null | undefined): void {
    const trimmed = label?.trim();
    if (!trimmed || trimmed.length < MIN_FREE_TEXT_ROOM_LENGTH) return;
    this.values.set(trimmed, ROOM_LABEL);
    this.reportingValues.set(trimmed, ROOM_LABEL);
    this.wholeWordOnly.add(trimmed);
  }

  /** Registers a participant identity, assigning it a stable numbered label. */
  registerParticipant(identity: string | null | undefined): void {
    const trimmed = identity?.trim();
    if (!trimmed || this.participantLabels.has(trimmed)) return;
    this.participantCounter += 1;
    const label = `<redacted:participant-${this.participantCounter}>`;
    this.participantLabels.set(trimmed, label);
    this.values.set(trimmed, label);
    this.reportingValues.set(trimmed, label);
  }

  /**
   * Registers a display name, window-facing label, or other known session
   * value for redaction in BOTH scrub paths -- the Sentry-facing `scrub()`
   * (via `values`) and the local session-log download `scrubForReporting()`
   * (via `reportingValues`). #709: this used to write only to
   * `reportingValues`, so every registered display name was invisible to
   * `beforeBreadcrumb`/`beforeSend` and reached Sentry unredacted (e.g. a
   * "participant left: <name>" breadcrumb). Uses a shared generic label
   * rather than `registerParticipant`'s per-identity numbered scheme, since
   * these values are never individually unregistered.
   */
  registerReportingValue(value: string | null | undefined, label = '<redacted:session-value>'): void {
    const trimmed = value?.trim();
    if (!trimmed) return;
    // Never downgrade an already-registered, more specific label (e.g. a
    // room or a numbered participant label from `registerRoom`/
    // `registerParticipant`) to this generic one. This matters in practice:
    // the display-name fallback (`participantDisplayName`) returns the raw
    // identity itself when no real name is set, so `registerReportingValue`
    // is frequently called with a value that is ALREADY the exact string
    // `registerParticipant` just registered -- first registration wins, both
    // maps stay fully redacted either way.
    if (!this.values.has(trimmed)) this.values.set(trimmed, label);
    if (!this.reportingValues.has(trimmed)) this.reportingValues.set(trimmed, label);
  }

  /** Stops scrubbing a participant identity that has left the room. */
  unregisterParticipant(identity: string | null | undefined): void {
    const trimmed = identity?.trim();
    if (!trimmed) return;
    if (this.participantLabels.delete(trimmed)) this.values.delete(trimmed);
  }

  /**
   * Clears the live Sentry map -- called when the room session ends, so a
   * later session's breadcrumbs never keep an old room/identity around.
   *
   * The reporting snapshot is deliberately KEPT for the tab's lifetime
   * (#245): the session log outlives the meeting, so a feedback report sent
   * from the home screen afterwards still carries that meeting's lines, and
   * they must stay scrubbed.
   */
  reset(): void {
    this.values.clear();
    this.wholeWordOnly.clear();
    this.participantLabels.clear();
    // Not the counter: the kept snapshot still maps earlier identities to
    // their numbers, and a label must name one identity per report.
  }

  scrub(text: string): string {
    return scrubSensitiveStrings(text, this.values, this.wholeWordOnly);
  }

  /** Uses the retained reporting-session snapshot, including departed users. */
  scrubForReporting(text: string): string {
    return scrubSensitiveStrings(text, this.reportingValues, this.wholeWordOnly);
  }

  /** Test/debug introspection only. */
  get size(): number {
    return this.values.size;
  }
}

/**
 * Registers the user-facing forms of one meeting (#245) under the room label:
 * the joinable access code, and the room's display label plus the slug it
 * takes in an invite URL (`/<slug>/<access-code>`). The internal credential
 * and wire room names are registered by connection.ts itself. Registering a
 * value twice is harmless -- it is a map keyed by the value.
 */
export function registerMeetingAliases(
  registry: SensitiveStringRegistry,
  credential: string,
  displayLabel: string | null | undefined
): void {
  registry.registerRoom(accessCodeForCredential(credential));
  const label = displayLabel?.trim();
  // The friendly fallback and slugify's own fallback are Petal's words, not
  // the user's; redacting them would only mangle unrelated log text.
  if (!label || label === roomFallbackLabelForCredential(credential)) return;
  // The name a person typed: whole-word only, so an everyday word as a
  // meeting name cannot rewrite unrelated text in the report.
  registry.registerFreeTextRoom(label);
  const slug = slugify(label);
  if (slug !== 'room') registry.registerFreeTextRoom(slug);
}

// Single app-wide registry -- connection.ts registers/unregisters into this
// as the room and participant list change; sentryReporting.ts reads from it
// in beforeBreadcrumb/beforeSend.
export const sensitiveStringRegistry = new SensitiveStringRegistry();
