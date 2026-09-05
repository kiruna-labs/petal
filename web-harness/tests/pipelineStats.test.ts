import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Track } from 'livekit-client';

import { actualInboundRid, parsePipelineStatsPayload, pipelineStatsCorrelationKey, prunePipelineReducerMaps, pruneStartupStatsCache, resetLifecycleTransitions, setupPipelineStats } from '../src/pipelineStats.ts';
import { StartupTimelineRecorder, classifyStartup } from '../src/startupTimeline.ts';

function baseV1Message(): Record<string, unknown> {
  return {
    v: 1,
    role: 'sender',
    reporterId: 'reporter-1',
    ownerIdentity: 'owner-1',
    windowId: 42,
    seq: 7,
    sentAtMs: 1_000,
    grabbed: null,
    encodedSent: null,
    received: null,
    decoded: null,
  };
}

test('parses a v1 message that has never had captureState/receiverFreeze (older sender build)', () => {
  // Regression test for issue #180 B1: a message from an already-shipped
  // desktop build predates the captureState/receiverFreeze fields entirely,
  // so they are absent (`undefined`), not merely `null`. The parser must
  // treat "field absent" the same as "field explicitly null" -- not as a
  // parse failure that drops the whole message.
  const payload = JSON.stringify(baseV1Message());
  const parsed = parsePipelineStatsPayload(payload);
  assert.notEqual(parsed, null);
  assert.equal(parsed?.captureState, null);
  assert.equal(parsed?.receiverFreeze, null);
});

test('parses a v1 message with captureState/receiverFreeze explicitly null', () => {
  const payload = JSON.stringify({
    ...baseV1Message(),
    captureState: null,
    receiverFreeze: null,
  });
  const parsed = parsePipelineStatsPayload(payload);
  assert.notEqual(parsed, null);
  assert.equal(parsed?.captureState, null);
  assert.equal(parsed?.receiverFreeze, null);
});

test('parses a v1 message with populated captureState and receiverFreeze', () => {
  const payload = JSON.stringify({
    ...baseV1Message(),
    captureState: {
      state: 'live',
      fps: 30,
      dirtyRectCount: 2,
      dirtyAreaPx: 1200,
      occlusionPct: 0,
      cpu: { lockCopyMs: 1.2, convertMs: 0.8, captureFrameReturnMs: 0.3 },
    },
    receiverFreeze: {
      freezeCount: 0,
      framesDropped: 0,
      qualityLimitationReason: null,
    },
  });
  const parsed = parsePipelineStatsPayload(payload);
  assert.notEqual(parsed, null);
  assert.equal(parsed?.captureState?.state, 'live');
  assert.equal(parsed?.receiverFreeze?.freezeCount, 0);
});

test('parses additive opaque correlation and lifecycle fields while preserving v1', () => {
  const parsed = parsePipelineStatsPayload(JSON.stringify({
    ...baseV1Message(),
    publicationSid: 'TR_private_id',
    shareEpoch: 'e7',
    lifecycle: 'firstPresented',
  }));
  assert.equal(parsed?.publicationSid, 'TR_private_id');
  assert.equal(parsed?.shareEpoch, 'e7');
  assert.equal(parsed?.lifecycle, 'firstPresented');
});

test('rejects malformed additive lifecycle fields without rejecting older v1 packets', () => {
  assert.equal(parsePipelineStatsPayload(JSON.stringify({ ...baseV1Message(), lifecycle: 'pixelsSent' })), null);
  assert.equal(parsePipelineStatsPayload(JSON.stringify({ ...baseV1Message(), shareEpoch: '' })), null);
});

test('replacement publications have isolated reducer identities', () => {
  const first = { ...baseV1Message(), publicationSid: 'TR_old', shareEpoch: 'e1' } as never;
  const replacement = { ...baseV1Message(), publicationSid: 'TR_new', shareEpoch: 'e2' } as never;
  assert.notEqual(pipelineStatsCorrelationKey(first, 'owner-1'), pipelineStatsCorrelationKey(replacement, 'owner-1'));
});

test('unsubscribe clears one-shot receiver lifecycle gates so resubscribe is observable', () => {
  const delivered = new Set([
    'receiver:owner:42:TR_same:legacy:subscribed',
    'receiver:owner:42:TR_same:legacy:firstDecoded',
    'receiver:owner:42:TR_same:legacy:firstPresented',
    'receiver:owner:42:TR_same:legacy:unsubscribed',
    'receiver:owner:42:TR_other:legacy:firstPresented',
  ]);
  resetLifecycleTransitions(delivered, 'owner', 42, 'TR_same');
  assert.deepEqual([...delivered], [
    'receiver:owner:42:TR_same:legacy:unsubscribed',
    'receiver:owner:42:TR_other:legacy:firstPresented',
  ]);
});

test('pipeline reducer prunes stale entries and evicts oldest beyond its 200-key cap', () => {
  const highWater = new Map<string, { seq: number; receivedAt: number }>();
  const terminals = new Map<string, number>();
  highWater.set('stale', { seq: 1, receivedAt: 0 });
  terminals.set('stale', 0);
  for (let index = 0; index < 201; index += 1) {
    highWater.set(`h${index}`, { seq: index, receivedAt: 10_000 });
    terminals.set(`t${index}`, 10_000);
  }
  prunePipelineReducerMaps(highWater, terminals, 10_000);
  assert.equal(highWater.size, 200);
  assert.equal(terminals.size, 200);
  assert.equal(highWater.has('stale'), false);
  assert.equal(terminals.has('stale'), false);
  assert.equal(highWater.has('h0'), false);
  assert.equal(terminals.has('t0'), false);
});

test('local sender collector pins a publication SID and epoch, then distinguishes a replacement', async () => {
  const makeTrack = () => ({
    mediaStreamTrack: { readyState: 'live', muted: false, getSettings: () => ({ width: 1280, height: 720, frameRate: 30 }) },
    sender: { getStats: async () => new Map() },
  });
  const first = makeTrack();
  const publications = new Map([['TR_old', { track: first, trackSid: 'TR_old' }]]);
  const room = {
    localParticipant: { identity: 'owner', trackPublications: publications, publishData: async () => {} },
    remoteParticipants: new Map(),
  };
  const ctx = {
    state: { room, sharing: true, localVideoTrack: first, screenSharing: false, screenTrack: null, screenWindowId: null },
    hook: {}, windowId: 42,
  } as any;
  const setup = setupPipelineStats(ctx);
  const firstMessage = (await setup.publishPipelineStats())[0];
  assert.equal(firstMessage.publicationSid, 'TR_old');
  assert.equal(firstMessage.lifecycle, 'published');
  assert.ok(firstMessage.shareEpoch);
  setup.stopPipelineStats();
  const afterRestart = (await setup.publishPipelineStats())[0];
  assert.equal(afterRestart.publicationSid, 'TR_old');
  assert.equal(afterRestart.shareEpoch, firstMessage.shareEpoch);
  const second = makeTrack();
  publications.clear();
  publications.set('TR_new', { track: second, trackSid: 'TR_new' });
  ctx.state.localVideoTrack = second;
  const replacement = (await setup.publishPipelineStats())[0];
  assert.equal(replacement.publicationSid, 'TR_new');
  assert.notEqual(replacement.shareEpoch, firstMessage.shareEpoch);
});

test('rejects a v1 message with a malformed (not merely absent) captureState', () => {
  const payload = JSON.stringify({
    ...baseV1Message(),
    captureState: { state: 'not-a-real-state' },
  });
  assert.equal(parsePipelineStatsPayload(payload), null);
});

test('rejects malformed JSON payload', () => {
  assert.equal(parsePipelineStatsPayload('{not json'), null);
});

test('startup waterfall is bounded, monotonic, privacy-safe, and explicit about cross-clock uncertainty', () => {
  let now = 100;
  const recorder = new StartupTimelineRecorder({ now: () => now, maxEventsPerTimeline: 4 });
  const correlation = { ownerIdentity: 'private-user-identity', windowId: 42, publicationSid: 'TR_one' };
  recorder.record(correlation, 'trackPublished');
  now = 180;
  recorder.record(correlation, 'trackSubscribed');
  now = 195;
  recorder.record(correlation, 'viewerDemand', {
    requestedSubscription: 'high', requestedWidth: 1920, requestedHeight: 1200,
  });
  now = 200;
  recorder.record(correlation, 'viewerDemand', {
    requestedSubscription: 'high', requestedWidth: 1920, requestedHeight: 1200,
  });
  now = 245;
  recorder.record(correlation, 'firstPresented');

  const [timeline] = recorder.snapshot();
  assert.equal(timeline.correlation.ownerAlias, 'peer-1');
  assert.equal(JSON.stringify(timeline).includes('private-user-identity'), false);
  assert.deepEqual(timeline.events.map((event) => [event.kind, event.elapsedMs]), [
    ['trackPublished', 0], ['trackSubscribed', 80], ['viewerDemand', 95], ['firstPresented', 145],
  ]);
  assert.deepEqual(timeline.clock, {
    basis: 'receiver-performance-now',
    crossPeerComparable: false,
    uncertainty: 'uncalibrated-cross-peer-clocks',
  });
});

test('startup recorder caps timelines and expires room-local state', () => {
  let now = 0;
  const recorder = new StartupTimelineRecorder({ now: () => now, maxTimelines: 2, ttlMs: 50 });
  recorder.record({ ownerIdentity: 'one', windowId: 1, publicationSid: 'TR_1' }, 'trackPublished');
  recorder.record({ ownerIdentity: 'two', windowId: 2, publicationSid: 'TR_2' }, 'trackPublished');
  recorder.record({ ownerIdentity: 'three', windowId: 3, publicationSid: 'TR_3' }, 'trackPublished');
  assert.equal(recorder.snapshot().length, 2);
  now = 100;
  assert.equal(recorder.snapshot().length, 0);

  // Expired/cap-evicted owners must not remain in the alias map. Reappearing
  // owners receive a fresh monotonic alias rather than retaining hidden state.
  recorder.record({ ownerIdentity: 'one', windowId: 4, publicationSid: 'TR_4' }, 'trackPublished');
  assert.equal(recorder.snapshot()[0].correlation.ownerAlias, 'peer-4');
});

test('six-fps classifier names proven causes and refuses to guess without sender evidence', () => {
  assert.equal(classifyStartup({ selectedMode: 'dataSaver', decodedFps: 15 }).cause, 'data-saver');
  assert.equal(classifyStartup({ capturePath: 'occluded-snapshot', decodedFps: 6 }).cause, 'occluded-snapshot-backoff');
  assert.equal(classifyStartup({ capturePath: 'static-idle', decodedFps: 0 }).cause, 'static-idle-source');
  assert.equal(classifyStartup({ capturePath: 'visible-raw', captureFps: 6, decodedFps: 6 }).cause, 'source-throttling');
  assert.equal(classifyStartup({ capturePath: 'visible-raw', captureFps: 30, decodedFps: 6 }).cause, 'visible-raw-cadence-shortfall');
  assert.equal(classifyStartup({ capturePath: 'visible-raw', decodedFps: 0 }).cause, 'visible-raw-cadence-shortfall');
  const unknown = classifyStartup({ decodedFps: 6 });
  assert.equal(unknown.cause, 'receiver-transport-unknown');
  assert.equal(unknown.evidenceComplete, false);
});

test('startup stats dedupe cache prunes TTL entries and caps retained publications', () => {
  const cache = new Map<string, { signature: string; updatedAt: number }>();
  cache.set('stale', { signature: 'old', updatedAt: 0 });
  for (let index = 0; index < 201; index += 1) {
    cache.set(`active-${index}`, { signature: `${index}`, updatedAt: 600_001 });
  }
  pruneStartupStatsCache(cache, 600_001);
  assert.equal(cache.size, 200);
  assert.equal(cache.has('stale'), false);
  assert.equal(cache.has('active-0'), false);
});

test('existing-track reconnect reconciliation records trackSubscribed in the startup timeline', () => {
  const remoteTrack = {
    kind: Track.Kind.Video,
    getRTCStatsReport: async () => new Map(),
  };
  const publication = {
    track: remoteTrack,
    trackName: 'petal-window-42',
    trackSid: 'TR_existing',
  };
  const room = {
    localParticipant: { identity: 'web-1', trackPublications: new Map(), publishData: async () => {} },
    remoteParticipants: new Map([
      ['owner-1', { identity: 'owner-1', trackPublications: new Map([['TR_existing', publication]]) }],
    ]),
  };
  const ctx = {
    state: {
      room, pipelineStatsTimer: null, sharing: false, localVideoTrack: null,
      screenSharing: false, screenTrack: null, screenWindowId: null,
    },
    hook: {}, windowId: 1,
  } as any;
  const setup = setupPipelineStats(ctx);
  setup.startPipelineStats();
  setup.stopPipelineStats();
  const timeline = ctx.hook.pipelineStats.startupTimeline()[0];
  assert.equal(timeline.correlation.publicationSid, 'TR_existing');
  assert.equal(timeline.events.some((event: { kind: string }) => event.kind === 'trackSubscribed'), true);
});

test('unsubscribe and unpublish clear per-publication startup stats dedupe', async () => {
  const report = new Map([
    ['inbound', { type: 'inbound-rtp', kind: 'video', bytesReceived: 100, framesPerSecond: 6, frameWidth: 480, frameHeight: 300 }],
  ]);
  const remoteTrack = { kind: Track.Kind.Video, getRTCStatsReport: async () => report };
  const publication = { track: remoteTrack, trackName: 'petal-window-42', trackSid: 'TR_cleanup' };
  const room = {
    localParticipant: { identity: 'web-1', trackPublications: new Map(), publishData: async () => {} },
    remoteParticipants: new Map([
      ['owner-1', { identity: 'owner-1', trackPublications: new Map([['TR_cleanup', publication]]) }],
    ]),
  };
  const ctx = {
    state: {
      room, pipelineStatsTimer: null, sharing: false, localVideoTrack: null,
      screenSharing: false, screenTrack: null, screenWindowId: null,
    },
    hook: {}, windowId: 1,
  } as any;
  const setup = setupPipelineStats(ctx);
  await setup.publishPipelineStats();
  await setup.publishPipelineStats();
  const statsCount = () => ctx.hook.pipelineStats.startupTimeline()[0].events
    .filter((event: { kind: string }) => event.kind === 'statsTransition').length;
  assert.equal(statsCount(), 1);
  ctx.hook.pipelineStats.trackUnsubscribed('owner-1', 42, 'TR_cleanup');
  await setup.publishPipelineStats();
  assert.equal(statsCount(), 2);
  ctx.hook.pipelineStats.trackUnpublished('owner-1', 42, 'TR_cleanup');
  await setup.publishPipelineStats();
  assert.equal(statsCount(), 3);
});

test('startup classifier identifies published-not-subscribed and post-subscription layer upgrade', () => {
  assert.equal(classifyStartup({ publishedElapsedMs: 1, observationElapsedMs: 2_100 }).cause, 'published-but-unsubscribed');
  assert.equal(classifyStartup({
    subscribedElapsedMs: 20, demandElapsedMs: 30,
    requestedWidth: 1920, requestedHeight: 1200,
    initialDecodedWidth: 480, initialDecodedHeight: 300,
    decodedWidth: 1920, decodedHeight: 1200,
  }).cause, 'quarter-bootstrap-layer-upgrade');
});

test('RID readback uses only an explicitly exposed inbound field', () => {
  const unavailable = new Map<string, unknown>([
    ['inbound', { type: 'inbound-rtp', kind: 'video', bytesReceived: 100 }],
  ]) as unknown as RTCStatsReport;
  assert.equal(actualInboundRid(unavailable), 'unavailable');
  const explicit = new Map<string, unknown>([
    ['low', { type: 'inbound-rtp', kind: 'video', bytesReceived: 100, rid: 'q' }],
    ['high', { type: 'inbound-rtp', kind: 'video', bytesReceived: 200, rid: 'h' }],
  ]) as unknown as RTCStatsReport;
  assert.equal(actualInboundRid(explicit), 'h');
});
