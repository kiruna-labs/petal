// ---------------------------------------------------------------------------
// #875 part 4: the multi-share count pill on a participant's camera tile.
//
// Pure logic only -- label formatting and foremost-window resolution -- kept
// out of tiles.ts so both can be unit tested directly without standing up the
// DOM harness. The DOM wiring (counting share tiles, rendering the pill,
// handling the click) lives in tiles.ts, which is the thing that actually
// calls these; a passing test on this module alone proves nothing is wired,
// see tests/shareCountPill.test.ts for the wiring-level assertions.
// ---------------------------------------------------------------------------

/** Counts above this render as `${SHARE_COUNT_PILL_CAP}+`, never a wider number. */
export const SHARE_COUNT_PILL_CAP = 9;

/**
 * The pill's label for a given share count. Empty string means "no pill" --
 * count < 2 (one window shared already has its own indicators; #875 only
 * adds a pill once there's an aggregate to show) collapses to the same "no
 * pill" result as count 0, so callers don't need a separate presence check.
 */
export function formatShareCountPillLabel(count: number): string {
  if (!Number.isFinite(count) || count < 2) return '';
  return count > SHARE_COUNT_PILL_CAP ? `${SHARE_COUNT_PILL_CAP}+` : String(Math.trunc(count));
}

/**
 * Resolve the sharer's FOREMOST currently-tiled window id for the pill click.
 *
 * `zOrder` is the decoded `petalWindowZOrder` participant-metadata value
 * (front-to-back, index 0 = frontmost) from
 * `trackNames.ts#sharedWindowZOrderFromMetadata` -- null covers BOTH an
 * absent key (older sharer) and a malformed one, and this function must not
 * try to tell those apart either, per that accessor's own contract.
 *
 * `tiledWindowIdsByAddOrder` is every window id that currently has a live
 * share tile for this owner, oldest-added first (the order `addShareTile`
 * calls arrive in for that identity). This is required even when `zOrder` is
 * present: a zOrder entry for a window that has already closed (or whose
 * tile hasn't finished attaching yet) cannot be spotlighted, so the result is
 * always scoped to windows that are actually resolvable to a tile right now.
 *
 * - With metadata: the first zOrder id that also has a live tile.
 * - Without metadata (older sharer): the MOST RECENTLY added tile (the last
 *   entry) -- the documented fallback, "treats the most-recently-started
 *   share instance as foremost".
 * - No candidate at all: null (caller does nothing rather than guessing).
 */
export function resolveForemostSharedWindowId(
  zOrder: readonly number[] | null,
  tiledWindowIdsByAddOrder: readonly number[]
): number | null {
  if (zOrder) {
    const tiled = new Set(tiledWindowIdsByAddOrder);
    for (const id of zOrder) {
      if (tiled.has(id)) return id;
    }
    return null;
  }
  return tiledWindowIdsByAddOrder.length > 0
    ? tiledWindowIdsByAddOrder[tiledWindowIdsByAddOrder.length - 1]!
    : null;
}
