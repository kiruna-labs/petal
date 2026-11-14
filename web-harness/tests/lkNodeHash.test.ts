// #372: the edge classifiers shared with the native app. Native counterpart:
// apps/desktop/src-tauri/src/sfu_edge.rs; both are pinned by
// contracts/petal-contracts.json -> lkEdgeTelemetry.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  LK_NODE_HASH_SALT,
  currentServerInfo,
  isLkNodeHash,
  lkNodeHash,
  lkRegionToken,
  localEdge,
  sameEdge,
  selfHostedSfu,
  type EdgeFlag,
} from '@petal/shared/logic/lkNodeHash';

const contract = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
) as {
  lkEdgeTelemetry: {
    nodeHashSalt: string;
    nodeHashVectors: Array<{ nodeId: string; lkNodeHash: string | null }>;
    sameEdge: Array<{ local: string | null; peer: string | null; expected: EdgeFlag }>;
    selfHostedSfu: Array<{ url: string; expected: EdgeFlag }>;
    regionTokens: Array<{ region: string; selfHostedSfu: EdgeFlag; expected: string }>;
  };
};
const edge = contract.lkEdgeTelemetry;

test('#372: lkNodeHash matches the shared vectors and never contains the node id', async () => {
  assert.equal(edge.nodeHashSalt, LK_NODE_HASH_SALT);
  assert.ok(edge.nodeHashVectors.length >= 4);
  for (const vector of edge.nodeHashVectors) {
    const hash = await lkNodeHash(vector.nodeId);
    assert.equal(hash, vector.lkNodeHash ?? undefined, vector.nodeId);
    if (hash) {
      assert.ok(isLkNodeHash(hash));
      assert.ok(!hash.includes(vector.nodeId));
    }
  }
  assert.equal(await lkNodeHash(undefined), undefined);
});

test('#372: same_edge truth table: equal, different, missing or malformed on either side', () => {
  const outcomes = new Set(edge.sameEdge.map((v) => v.expected));
  assert.deepEqual([...outcomes].sort(), ['false', 'true', 'unknown']);
  for (const v of edge.sameEdge) {
    assert.equal(sameEdge(v.local ?? undefined, v.peer ?? undefined), v.expected, `${v.local} vs ${v.peer}`);
  }
});

test('#372: self_hosted_sfu classification matches the contract', () => {
  for (const v of edge.selfHostedSfu) assert.equal(selfHostedSfu(v.url), v.expected, v.url);
});

test('#372: lk_region tokens match the contract and the summary token rule', () => {
  for (const v of edge.regionTokens) {
    const token = lkRegionToken(v.region, v.selfHostedSfu);
    assert.equal(token, v.expected, `${v.region}/${v.selfHostedSfu}`);
    assert.match(token, /^[a-z0-9_-]{1,32}$/);
  }
});

test('#372: localEdge reads livekit-client serverInfo and keeps only the hash', async () => {
  const cloud = await localEdge(
    { region: 'us-east-1', nodeId: 'ND_7xKqM2pLz9Rb' },
    'wss://petal-x.livekit.cloud'
  );
  assert.deepEqual(cloud, { lkRegion: 'us-east-1', selfHostedSfu: 'false', lkNodeHash: '69e94d0a' });
  assert.ok(!JSON.stringify(cloud).includes('ND_7xKqM2pLz9Rb'));
  assert.deepEqual(await localEdge({ region: 'acme', nodeId: 'localnode' }, 'ws://localhost:7880'), {
    lkRegion: 'unknown',
    selfHostedSfu: 'true',
    lkNodeHash: '02adc8a7',
  });
  // No server info: no region at all, so meeting_joined omits it.
  assert.deepEqual(await localEdge(undefined, 'wss://petal-x.livekit.cloud'), {
    selfHostedSfu: 'false',
  });
});

test('#372: a full reconnect\'s join response wins over the first-connect serverInfo', () => {
  const first = { region: 'us-east-1', nodeId: 'ND_7xKqM2pLz9Rb' };
  const later = { region: 'eu-west-1', nodeId: 'ND_Hq4cV8nTw3Ej' };
  assert.equal(currentServerInfo({ serverInfo: first }), first);
  assert.equal(currentServerInfo({ serverInfo: first, engine: { latestJoinResponse: { serverInfo: later } } }), later);
  assert.equal(currentServerInfo({ serverInfo: first, engine: {} }), first);
  assert.equal(currentServerInfo({}), undefined);
  assert.equal(currentServerInfo({ serverInfo: first, engine: { latestJoinResponse: {} } }), undefined);
});
