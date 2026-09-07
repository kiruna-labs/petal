// Which remote share tile a cockpit scenario acts on.
//
// #919: DRAW-N drew on the FIRST `.share-tile` in the DOM. Quick tier runs
// SHARE-W2N-Q right before it, and that peer's headless Chrome is killed, not
// disconnected, so the SFU keeps its publication alive for ~25s -- long enough
// for its tile to be first when DRAW-N's peer joins. The stroke then targeted a
// window the native cockpit did not own, native routed it to a remote pointer
// overlay (trace-level only), and the scenario failed on "no native evidence"
// while the web side had asserted `strokeDelivered: true`.
//
// The native cockpit now names its own LiveKit identity in the launch URL
// (`&owner=`); scenarios that draw on / point at / measure the native share
// must use it. Without it (hand-driven `?auto=` runs) the first tile still
// wins, as before.

export interface CockpitShareTileLike {
  dataset: {
    owner?: string;
    windowId?: string;
    [key: string]: string | undefined;
  };
}

export function cockpitOwnerFromSearch(search: string): string | undefined {
  const owner = new URLSearchParams(search).get('owner')?.trim();
  return owner ? owner : undefined;
}

export function selectCockpitShareTile<T extends CockpitShareTileLike>(
  tiles: readonly T[],
  ownerIdentity?: string
): T | null {
  const owner = ownerIdentity?.trim();
  if (owner) return tiles.find((tile) => tile.dataset.owner?.trim() === owner) ?? null;
  return tiles[0] ?? null;
}

export function cockpitShareTileMissingDetail(
  action: string,
  tiles: readonly CockpitShareTileLike[],
  ownerIdentity?: string
): string {
  const present = tiles.map((tile) => `${tile.dataset.owner ?? '?'}:${tile.dataset.windowId ?? '?'}`);
  const owner = ownerIdentity?.trim();
  return owner
    ? `${action} requires the native cockpit owner's share tile (owner=${owner}); share tiles present: [${present.join(', ')}]`
    : `${action} requires a remote share tile with owner and window id; share tiles present: [${present.join(', ')}]`;
}
