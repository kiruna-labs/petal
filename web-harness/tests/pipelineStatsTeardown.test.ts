import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Track } from 'livekit-client';

import { setupPipelineStats } from '../src/pipelineStats.ts';

// #164: the stats tick is async. A disconnect that clears `state.room` while a
// receiver stats report is still being awaited must turn that tick into a
// quiet no-op, not a TypeError on `null.localParticipant` (Sentry
// PETAL-WEB-HARNESS-4).
function harnessWithOneRemoteShare(releaseStats: () => Promise<void>) {
  let resolveStats: (report: RTCStatsReport) => void = () => {};
  const statsGate = new Promise<RTCStatsReport>((resolve) => { resolveStats = resolve; });
  const remoteTrack = {
    kind: Track.Kind.Video,
    getRTCStatsReport: async () => {
      await releaseStats();
      return statsGate;
    },
  };
  const publication = { track: remoteTrack, trackName: 'petal-window-42', trackSid: 'TR_remote' };
  const remoteParticipant = { identity: 'owner', trackPublications: new Map([['TR_remote', publication]]) };
  const room = {
    localParticipant: { identity: 'viewer', trackPublications: new Map(), publishData: async () => {} },
    remoteParticipants: new Map([['owner', remoteParticipant]]),
  };
  const ctx = {
    state: { room, sharing: false, localVideoTrack: null, screenSharing: false, screenTrack: null, screenWindowId: null },
    hook: {}, windowId: 7,
  } as any;
  return { ctx, resolveStats: () => resolveStats(new Map() as unknown as RTCStatsReport) };
}

test('a receiver stats tick in flight at disconnect is a no-op, not a throw', async () => {
  let disconnect: () => void = () => {};
  const { ctx, resolveStats } = harnessWithOneRemoteShare(async () => {
    // The room goes away while the stats report is being awaited.
    disconnect();
  });
  const setup = setupPipelineStats(ctx);
  disconnect = () => {
    setup.stopPipelineStats();
    ctx.state.room = null;
  };
  const tick = setup.publishPipelineStats();
  resolveStats();
  const messages = await tick;
  assert.deepEqual(messages, [], 'a sample after teardown has nothing to report');
});

test('a receiver stats tick that completes with the room still present reports as before', async () => {
  const { ctx, resolveStats } = harnessWithOneRemoteShare(async () => {});
  const setup = setupPipelineStats(ctx);
  const tick = setup.publishPipelineStats();
  resolveStats();
  const messages = await tick;
  // An empty stats report carries no signal, so no message is produced --
  // but the tick must have run to completion without touching a null room.
  assert.ok(Array.isArray(messages));
  assert.notEqual(ctx.state.room, null);
});
