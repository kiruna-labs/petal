// SINGLE SOURCE OF TRUTH for the controller-side "local echo" feature
// (opt-in, default OFF via session.localEchoEnabled / HARNESS_LOCAL_ECHO_STORAGE_KEY),
// shared by the desktop app (apps/desktop/src/lib/data/localEcho.ts re-exports
// this) and the web client (web-harness imports it directly). Previously
// duplicated in both apps/desktop/src/lib/data/localEcho.ts and
// web-harness/src/localEcho.ts with identical behavior — merged here so the
// two can never drift.
//
// Hard rule this file exists to uphold (truth-over-appearance): every value
// rendered from this module is a LOCAL PREDICTION of "input was sent", never
// a claim that the remote effect happened. Phase 1 (gesture echo) draws
// nothing that could be mistaken for real app content. Phase 2 (text echo)
// is bounded and always visually distinct (a translucent "pending" strip),
// and MUST be cleared -- either by a real confirmation signal once one
// exists (no per-input ack protocol is wired yet; that's #288, tracked
// separately) or, always, by the hard-capped timeout below.

/** Ripple fade duration for Phase 1 gesture echo (click/wheel feedback). */
export const LOCAL_ECHO_RIPPLE_FADE_MS = 150;

/**
 * Hard cap on how long the Phase 2 pending-text strip may show unconfirmed
 * text before it clears itself. Restarted on every echoed keystroke, so
 * continuous typing keeps the strip alive; it clears ~2s after typing pauses
 * since there is no real per-input receipt (#288) to confirm against yet.
 */
export const LOCAL_ECHO_TEXT_TIMEOUT_MS = 2000;

export interface LocalEchoRipple {
  id: number;
  /** Overlay-local pixel coordinates -- where THIS user clicked/scrolled,
   * not the normalized remote-content coordinates used on the wire. */
  x: number;
  y: number;
}

/** Monotonic id generator matching the `seq`-style wraparound used
 * elsewhere in the remote-control surface (nextSeq/nextDrawSeq). */
export function nextLocalEchoRippleId(current: number): number {
  return current >= Number.MAX_SAFE_INTEGER ? 1 : current + 1;
}

export interface EchoKeyLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
}

/**
 * Decides how a keydown should affect the Phase 2 pending-text strip.
 * Returns the new pending string, or `null` when the key is not
 * text-composition-relevant (shortcuts, arrows, function keys, Escape,
 * etc.) and the existing pending text should be left untouched.
 *
 * - Any Ctrl/Meta/Alt combo is treated as a shortcut, never text.
 * - Enter is treated as "submitted" and clears the strip -- lingering
 *   predicted text after a submit would misrepresent what's still pending.
 * - Backspace pops the last predicted character.
 * - A single-codepoint `key` (the DOM convention for printable characters,
 *   including Unicode/emoji) is appended.
 */
export function applyLocalEchoKey(pending: string, event: EchoKeyLike): string | null {
  if (event.ctrlKey || event.metaKey || event.altKey) return null;
  if (event.key === 'Enter') return '';
  if (event.key === 'Backspace') return pending.length > 0 ? pending.slice(0, -1) : pending;
  if (Array.from(event.key).length === 1) return pending + event.key;
  return null;
}

export interface RectSize {
  width: number;
  height: number;
}

export interface EchoPoint {
  x: number;
  y: number;
}

/**
 * Keeps the pending-text strip's anchor (proxied from the most recent local
 * click point, since the real remote caret position is never known to the
 * controller) within the visible overlay/tile, so it can never render
 * off-screen or get clipped.
 */
export function clampLocalEchoAnchor(anchor: EchoPoint, bounds: RectSize, margin = 12): EchoPoint {
  const width = Math.max(0, bounds.width);
  const height = Math.max(0, bounds.height);
  const maxX = Math.max(margin, width - margin);
  const maxY = Math.max(margin, height - margin);
  return {
    x: Math.min(Math.max(anchor.x, margin), maxX),
    y: Math.min(Math.max(anchor.y, margin), maxY)
  };
}
