// A measured fit ladder: level 0 shows everything, each level above it
// shows less (a control moved into a menu, labels dropped, ...). The gallery
// steps one level per measurement -- up while the row does not fit, back down
// as soon as the level below fits again -- for its control bar and its top
// bar (Gallery.svelte). Measured, never by breakpoint, like the web client's
// control overflow (#247): a plugin adding a button or a renamed room re-fits.

/** Sub-pixel slack, so a row that fits exactly is not rounded into overflow. */
const FIT_EPSILON = 0.5;

/**
 * One step. `needed` is the width the row at `level` needs right now, in the
 * same terms as `available` -- the whole bar's width, its OWN padding
 * included, because levels may pad differently (comparing a lower level's
 * content width with this level's content box once flipped the gallery's
 * labels on and off every two frames). `widthAtLevel` remembers what each
 * level needed when it last showed; the step records into it.
 */
export function stepFitLadder(
  level: number,
  needed: number,
  available: number,
  widthAtLevel: Array<number | undefined>,
  maxLevel: number
): number {
  widthAtLevel[level] = needed;
  if (needed > available + FIT_EPSILON) return Math.min(maxLevel, level + 1);
  const wider = level > 0 ? widthAtLevel[level - 1] : undefined;
  return wider !== undefined && wider <= available + FIT_EPSILON ? level - 1 : level;
}
