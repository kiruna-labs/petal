// Pure text sanitization for the feedback message field (#292). Kept
// dependency-free (no `$lib/*` imports) so it's directly unit-testable
// under plain `node --test` -- `userDispatch.ts` imports and re-exports
// these for the rest of the feedback module.

export const FEEDBACK_MAX_MESSAGE_CHARS = 2_000;

const CONTROL_CHAR_PATTERN = new RegExp('[\\x00-\\x1F\\x7F]', 'g');

/** Trims, strips control characters, collapses internal whitespace, and
 * bounds length -- mirrors the `normalizedBoundedText` helper in the
 * approved web-harness parity reference (#293, `feedbackReport.ts`). */
export function normalizedFeedbackMessage(value: string): string {
  return value
    .normalize('NFKC')
    .replace(CONTROL_CHAR_PATTERN, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, FEEDBACK_MAX_MESSAGE_CHARS);
}
