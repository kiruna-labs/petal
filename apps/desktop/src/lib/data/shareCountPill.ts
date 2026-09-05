// #875: pure logic for the multi-share count pill on gallery/filmstrip
// participant tiles. Framework-import-free (same pattern as
// cameraFreezeWatchdog.ts) so it's directly unit-testable without mounting
// any Svelte component.
//
// Product decisions locked with the user (2026-08-23): caps its DISPLAYED
// digit at "9+" (never truncates --
// the label is always the full "9+" glyph, never a clipped "9"), and is
// hidden entirely at the `tiny` gallery tile size (drop, never shrink past
// legibility -- the CLAUDE.md "UI text must never truncate" rule).

/** Count includes ALL of a participant's `petal-window-*` publications --
 * display shares and viewer-hidden windows included. This is a raw
 * publication count, not a "windows currently visible to me" count. */
// #875 revision (2026-08-26): was 2, on the premise that "one shared window
// is already covered by the existing `sharing` indicator". That premise was
// wrong twice over -- the `.sharing` class styled NOTHING (fixed alongside
// this), and gating at 2 meant a single-window sharer had no clickable
// affordance at all, so "click their portrait to raise their windows" simply
// did not exist in the most common case. Owner report: "clicking portrait
// also regressed, because it doesn't seem to work for me". At 1 the pill is
// always present while sharing, so the raise affordance always exists.
export const SHARE_COUNT_PILL_MIN = 1;
export const SHARE_COUNT_PILL_CAP = 9;

/** True only when the pill should render at all -- i.e. whenever the
 * participant is sharing at least one window, so the click-to-raise
 * affordance is always available. */
export function shouldShowSharePill(count: number): boolean {
  return Number.isFinite(count) && count >= SHARE_COUNT_PILL_MIN;
}

/** Digit label for the pill: the exact count up to the cap, then "9+".
 * Never returns a clipped/truncated form of a larger number. */
export function shareCountPillLabel(count: number): string {
  const safeCount = Number.isFinite(count) ? Math.max(0, Math.trunc(count)) : 0;
  return safeCount > SHARE_COUNT_PILL_CAP ? `${SHARE_COUNT_PILL_CAP}+` : String(safeCount);
}

/** Full aria-label for the interactive (remote) pill -- states the real
 * count (not the capped display label) so an assistive-tech user with 12
 * shared windows hears "12", not "9+". */
export function shareCountPillAriaLabel(count: number, name: string): string {
  const safeCount = Number.isFinite(count) ? Math.max(0, Math.trunc(count)) : 0;
  const noun = safeCount === 1 ? 'window' : 'windows';
  return `${safeCount} ${noun} shared by ${name} — bring to front`;
}

/** Aria-label for the non-interactive local tile's pill: no "bring to
 * front" action language, since clicking it does nothing this iteration. */
export function localShareCountPillAriaLabel(count: number): string {
  const safeCount = Number.isFinite(count) ? Math.max(0, Math.trunc(count)) : 0;
  const noun = safeCount === 1 ? 'window' : 'windows';
  return `Sharing ${safeCount} ${noun}`;
}
