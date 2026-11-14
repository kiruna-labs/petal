// #372: which LiveKit edge this client is on, without ever sending the raw
// node id. Mirrors `apps/desktop/src-tauri/src/sfu_edge.rs`; both are pinned
// by `contracts/petal-contracts.json` -> `lkEdgeTelemetry`. Used by the web
// client (web-harness/src/connection.ts, remoteControlUi.ts).

export const LK_NODE_HASH_SALT = 'petal-lk-node:';
const LK_NODE_HASH_PATTERN = /^[0-9a-f]{8}$/;

export type EdgeFlag = 'true' | 'false' | 'unknown';

/** First 8 hex chars of SHA-256(salt + nodeId); undefined for no node id. */
export async function lkNodeHash(nodeId: string | undefined): Promise<string | undefined> {
  // `crypto.subtle` exists only in secure contexts; without it, no hash.
  const subtle = globalThis.crypto?.subtle;
  if (!nodeId || !subtle) return undefined;
  try {
    const bytes = new TextEncoder().encode(LK_NODE_HASH_SALT + nodeId);
    const digest = new Uint8Array(await subtle.digest('SHA-256', bytes));
    return Array.from(digest.subarray(0, 4), (byte) => byte.toString(16).padStart(2, '0')).join('');
  } catch {
    return undefined;
  }
}

/** A peer-supplied hash is untrusted: only 8 lowercase hex chars count. */
export function isLkNodeHash(value: unknown): value is string {
  return typeof value === 'string' && LK_NODE_HASH_PATTERN.test(value);
}

export function sameEdge(local: unknown, peer: unknown): EdgeFlag {
  if (!isLkNodeHash(local) || !isLkNodeHash(peer)) return 'unknown';
  return local === peer ? 'true' : 'false';
}

/** `false` only for a `*.livekit.cloud` host; `unknown` with no parseable host. */
export function selfHostedSfu(url: string): EdgeFlag {
  const schemeEnd = url.indexOf('://');
  if (schemeEnd < 0) return 'unknown';
  const authority = url.slice(schemeEnd + 3).split(/[/?#]/)[0] ?? '';
  const hostPort = authority.slice(authority.lastIndexOf('@') + 1);
  const host = (
    hostPort.startsWith('[') ? hostPort.slice(1).split(']')[0] ?? '' : hostPort.split(':')[0] ?? ''
  )
    .replace(/\.+$/, '')
    .toLowerCase();
  if (!host) return 'unknown';
  return host === 'livekit.cloud' || host.endsWith('.livekit.cloud') ? 'false' : 'true';
}

/** `lk_region` token: only a LiveKit Cloud region is sent; else `unknown`. */
export function lkRegionToken(region: string | undefined, selfHosted: EdgeFlag): string {
  if (selfHosted !== 'false') return 'unknown';
  // ASCII-only lowering per code point, like Rust's `to_ascii_lowercase`
  // (`toLowerCase` can change a non-ASCII string's length).
  const token = Array.from((region ?? '').trim())
    .map((c) => (/^[A-Z]$/.test(c) ? c.toLowerCase() : c))
    .map((c) => (/^[a-z0-9_-]$/.test(c) ? c : '_'))
    .slice(0, 32)
    .join('');
  return /[a-z0-9]/.test(token) ? token : 'unknown';
}

export interface LocalEdge {
  /** Absent when the server sent no server info (omitted, never guessed). */
  lkRegion?: string;
  selfHostedSfu: EdgeFlag;
  lkNodeHash?: string;
}

/** This client's edge from livekit-client's `room.serverInfo` and the room URL. */
export async function localEdge(
  serverInfo: { region?: string; nodeId?: string } | undefined,
  url: string
): Promise<LocalEdge> {
  const selfHosted = selfHostedSfu(url);
  const hash = await lkNodeHash(serverInfo?.nodeId);
  return {
    ...(serverInfo ? { lkRegion: lkRegionToken(serverInfo.region, selfHosted) } : {}),
    selfHostedSfu: selfHosted,
    ...(hash ? { lkNodeHash: hash } : {}),
  };
}

type ServerInfoLike = { region?: string; nodeId?: string };

/** livekit-client sets `room.serverInfo` only on the first connect; a full
 * reconnect's join response lives on `room.engine.latestJoinResponse`. */
export function currentServerInfo(room: {
  serverInfo?: ServerInfoLike;
  engine?: { latestJoinResponse?: { serverInfo?: ServerInfoLike } };
}): ServerInfoLike | undefined {
  // A join response without server info means none, never the stale
  // first-connect value (matches native `Room::server_info`).
  const latest = room.engine?.latestJoinResponse;
  return latest ? latest.serverInfo : room.serverInfo;
}

/** The edge dimensions of a control session's latency summary (#372); empty
 * while the local edge is unknown or being refreshed. ICE is native-only. */
export function edgeDimensions(
  local: LocalEdge | null | undefined,
  peerHash: string | undefined
): Partial<Record<'lk_region' | 'self_hosted_sfu' | 'same_edge', string>> {
  if (!local) return {};
  return {
    ...(local.lkRegion ? { lk_region: local.lkRegion } : {}),
    self_hosted_sfu: local.selfHostedSfu,
    same_edge: sameEdge(local.lkNodeHash, peerHash),
  };
}
