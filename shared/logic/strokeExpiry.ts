// SINGLE SOURCE OF TRUTH for on-window drawing (annotation) stroke expiry
// (#670): SPEC.md says drawing is "ephemeral by default," but strokes never
// expired -- they accumulated for the life of a share. Shared by the desktop
// app's native compositor pointer overlay (apps/desktop/src/routes/compositor/
// pointer/+page.svelte, re-exported via apps/desktop/src/lib/data/strokeExpiry.ts)
// and its meeting-tile camera-draw layer (ParticipantTile.svelte), and by the
// web client (web-harness/src/drawDisplay.ts imports this directly) -- so the
// three independent render surfaces cannot drift on the fade threshold.
//
// Mirrors the telepointer idle-fade pattern one component over
// (apps/desktop/src/routes/compositor/pointer/+page.svelte's IDLE_MS/STALE_MS
// + the ~250ms sweep interval + Pointer.svelte's CSS opacity transition):
// client-side-only received timestamps, no wire-format change. A stroke ages
// from its LAST point, not its first -- a stroke still being actively
// extended must never start fading mid-draw, so every caller must restart
// the clock (re-stamp "now") on every new point delivered for a stroke, not
// just at "begin".

/** A stroke stays fully visible until this many ms after its last point
 * (the user's own specified value, 2026-08-06 -- not an estimate). */
export const STROKE_FADE_START_MS = 10_000;

/** Then fades out over this long -- long enough to read as a deliberate
 * fade rather than an abrupt pop, short enough that the stroke is fully
 * gone shortly after the 10s mark (matches the "~0.5-1s" guidance). */
export const STROKE_FADE_DURATION_MS = 800;

/** Total age at which a stroke has fully faded and can be dropped from
 * state entirely. */
export const STROKE_EXPIRE_MS = STROKE_FADE_START_MS + STROKE_FADE_DURATION_MS;

/**
 * Opacity multiplier in [0,1] for a stroke, given `ageMs` -- the time since
 * its LAST point (not its first). 1 until STROKE_FADE_START_MS, then a
 * linear ramp to 0 over STROKE_FADE_DURATION_MS.
 *
 * Pure function: callers supply their own `ageMs` (computed from whichever
 * clock they already use -- `performance.now()` natively, `Date.now()` in
 * the web harness) so this module never needs to know which surface is
 * calling it.
 */
export function strokeFadeOpacity(ageMs: number): number {
  if (ageMs <= STROKE_FADE_START_MS) return 1;
  if (ageMs >= STROKE_EXPIRE_MS) return 0;
  const fadeElapsedMs = ageMs - STROKE_FADE_START_MS;
  return 1 - fadeElapsedMs / STROKE_FADE_DURATION_MS;
}

/** True once a stroke has fully faded and its render/tracking state can be
 * dropped entirely. */
export function isStrokeExpired(ageMs: number): boolean {
  return ageMs >= STROKE_EXPIRE_MS;
}
