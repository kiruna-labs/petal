// #372: keeps `state.lkEdge` on the current connection's edge. Shared
// classifiers live in shared/logic/lkNodeHash.ts.
import type { Room } from 'livekit-client';
import { currentServerInfo, localEdge, type LocalEdge } from '@petal/shared/logic/lkNodeHash';

type EdgeRoom = Parameters<typeof currentServerInfo>[0];

export function createLkEdgeRefresher(deps: {
  state: { room: unknown; lkEdge?: LocalEdge | null };
  logEvent: (message: string) => void;
  computeEdge?: typeof localEdge;
}): (room: Room | EdgeRoom, url: string) => Promise<void> {
  const computeEdge = deps.computeEdge ?? localEdge;
  let seq = 0;
  return async (room, url) => {
    const mine = ++seq;
    // Until the new hash is ready a request or status carries none, never
    // the previous connection's.
    if (deps.state.room === room) deps.state.lkEdge = null;
    const edge = await computeEdge(currentServerInfo(room as EdgeRoom), url);
    // A stale room's or an overtaken refresh's answer is dropped.
    if (deps.state.room !== room || mine !== seq) return;
    deps.state.lkEdge = edge;
    deps.logEvent(
      `sfu edge: lk_region=${edge.lkRegion ?? 'none'} self_hosted_sfu=${edge.selfHostedSfu} lk_node_hash=${edge.lkNodeHash ?? 'none'}`
    );
  };
}
