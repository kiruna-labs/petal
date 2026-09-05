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

const ROOM_LABEL = '<redacted:room>';

/**
 * Replaces every occurrence of every registered value in `text` with its
 * label. Longest values are replaced first so a shorter registered value
 * that happens to be a substring of a longer one never partially matches
 * inside an already-registered longer string.
 */
export function scrubSensitiveStrings(text: string, sensitiveValues: ReadonlyMap<string, string>): string {
  if (!text) return text;
  let result = text;
  const entries = [...sensitiveValues.entries()]
    .filter(([value]) => value.length > 0)
    .sort((a, b) => b[0].length - a[0].length);
  for (const [value, label] of entries) {
    if (!result.includes(value)) continue;
    result = result.split(value).join(label);
  }
  return result;
}

export class SensitiveStringRegistry {
  private values = new Map<string, string>(); // raw value -> redacted label
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

  /** Clears everything -- called when the room session ends. */
  reset(): void {
    this.values.clear();
    this.reportingValues.clear();
    this.participantLabels.clear();
    this.participantCounter = 0;
  }

  scrub(text: string): string {
    return scrubSensitiveStrings(text, this.values);
  }

  /** Uses the retained reporting-session snapshot, including departed users. */
  scrubForReporting(text: string): string {
    return scrubSensitiveStrings(text, this.reportingValues);
  }

  /** Test/debug introspection only. */
  get size(): number {
    return this.values.size;
  }
}

// Single app-wide registry -- connection.ts registers/unregisters into this
// as the room and participant list change; sentryReporting.ts reads from it
// in beforeBreadcrumb/beforeSend.
export const sensitiveStringRegistry = new SensitiveStringRegistry();
