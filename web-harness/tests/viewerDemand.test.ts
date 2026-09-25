import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Track, type RemoteTrackPublication } from 'livekit-client';

import {
  setupViewerDemand,
  shareMediaSize,
  viewerDemandContainedMediaGeometry,
  viewerDemandPixelGeometry,
  viewerDemandSubscriptionGeometry,
} from '../src/viewerDemand.ts';
import type { HarnessContext } from '../src/context.ts';

function makePublicationDemandHarness(localIdentity = 'web-riley') {
  const published: Array<{ payload: Uint8Array; options: { topic: string; reliable: boolean } }> = [];
  const ctx = {
    state: {
      room: {
        localParticipant: {
          identity: localIdentity,
          publishData: async (payload: Uint8Array, options: { topic: string; reliable: boolean }) => {
            published.push({ payload, options });
          },
        },
      },
      viewerDemandSeq: 0,
    },
  } as unknown as HarnessContext;
  return { published, viewerDemand: setupViewerDemand(ctx) };
}

function makeInboundRepairHarness(localIdentity = 'web-riley') {
  const repairCalls: Array<{ windowId: number; requesterIdentity: string }> = [];
  const ctx = {
    state: {
      room: {
        localParticipant: { identity: localIdentity },
      },
      viewerDemandSeq: 0,
    },
    cb: {
      repairScreenShareForWindow: async (windowId: number, requesterIdentity: string) => {
        repairCalls.push({ windowId, requesterIdentity });
        return true;
      },
    },
  } as unknown as HarnessContext;
  return { repairCalls, viewerDemand: setupViewerDemand(ctx) };
}

function repairRequestPayload(overrides: Partial<Record<string, unknown>> = {}): Uint8Array {
  const message = {
    v: 2,
    kind: 'heartbeat',
    targetUserId: 'web-riley',
    viewerId: 'native-quinn',
    windowId: 42,
    seq: 9,
    visible: true,
    width: 0,
    height: 0,
    scale: 1,
    pixelWidth: 0,
    pixelHeight: 0,
    needsRepublish: true,
    ...overrides,
  };
  return new TextEncoder().encode(JSON.stringify(message));
}

function publication(kind: Track.Kind, trackName: string): RemoteTrackPublication {
  return { kind, trackName } as RemoteTrackPublication;
}

test('viewer demand converts logical content size to receiver device pixels', () => {
  assert.deepEqual(viewerDemandPixelGeometry(1280, 720, 2), {
    width: 1280,
    height: 720,
    scale: 2,
    pixelWidth: 2560,
    pixelHeight: 1440,
  });
});

test('viewer demand sanitizes device scale and caps dimensions at the H264 guardrail', () => {
  assert.deepEqual(viewerDemandPixelGeometry(5000.4, 3000.4, Number.NaN), {
    width: 5000,
    height: 3000,
    scale: 1,
    pixelWidth: 4096,
    pixelHeight: 3000,
  });
  assert.deepEqual(viewerDemandPixelGeometry(1600, 900, 8), {
    width: 1600,
    height: 900,
    scale: 4,
    pixelWidth: 4096,
    pixelHeight: 3600,
  });
});

test('viewer demand measures wide video content inside horizontal letterboxing', () => {
  assert.deepEqual(
    viewerDemandContainedMediaGeometry(
      { left: 40, top: 80, width: 400, height: 300 },
      { width: 1600, height: 900 },
      2
    ),
    {
      width: 400,
      height: 225,
      scale: 2,
      pixelWidth: 800,
      pixelHeight: 450,
    }
  );
});

test('viewer demand measures tall video content inside vertical letterboxing', () => {
  assert.deepEqual(
    viewerDemandContainedMediaGeometry(
      { left: 40, top: 80, width: 400, height: 300 },
      { width: 600, height: 900 },
      2
    ),
    {
      width: 200,
      height: 300,
      scale: 2,
      pixelWidth: 400,
      pixelHeight: 600,
    }
  );
});

test('viewer demand rounds up to the smallest sufficient Petal simulcast layer', () => {
  assert.deepEqual(viewerDemandSubscriptionGeometry(1568, 980, { width: 1920, height: 1200 }), {
    width: 1920,
    height: 1200,
  });
  assert.deepEqual(viewerDemandSubscriptionGeometry(900, 560, { width: 1920, height: 1200 }), {
    width: 900,
    height: 560,
  });
  assert.deepEqual(viewerDemandSubscriptionGeometry(800, 500), {
    width: 800,
    height: 500,
  });
});

test('publication demand sends a PRESENCE-ONLY v2 open packet for the announced remote window', () => {
  const { published, viewerDemand } = makePublicationDemandHarness();

  viewerDemand.publishViewerDemandForPublication(
    'remote-sam',
    publication(Track.Kind.Video, 'petal-window-42')
  );

  // TrackPublished fires for EVERY announcement, including the one caused by
  // the owner's own republish. A resolution claim here (it used to advertise
  // viewport pixels) closes the republish feedback loop: downsize republish ->
  // announcement -> viewport-sized demand -> instant upsize republish, a pair
  // every 8s (shipped 0.8.1). Presence is allowed; geometry is not.
  assert.equal(published.length, 1);
  assert.deepEqual(JSON.parse(new TextDecoder().decode(published[0]!.payload)), {
    v: 2,
    kind: 'open',
    targetUserId: 'remote-sam',
    viewerId: 'web-riley',
    windowId: 42,
    seq: 1,
    visible: true,
    needsRepublish: false,
    width: 0,
    height: 0,
    scale: 1,
    pixelWidth: 0,
    pixelHeight: 0,
  });
  assert.deepEqual(published[0]!.options, {
    topic: 'petal.viewer-demand',
    reliable: true,
  });
});

test('publication demand ignores the local participant own publication', () => {
  const { published, viewerDemand } = makePublicationDemandHarness('web-riley');

  viewerDemand.publishViewerDemandForPublication(
    'web-riley',
    publication(Track.Kind.Video, 'petal-window-42')
  );

  assert.equal(published.length, 0);
});

test('publication demand ignores non-video publications', () => {
  const { published, viewerDemand } = makePublicationDemandHarness();

  viewerDemand.publishViewerDemandForPublication(
    'remote-sam',
    publication(Track.Kind.Audio, 'petal-window-42')
  );

  assert.equal(published.length, 0);
});

test('publication demand ignores track names without a window id', () => {
  const { published, viewerDemand } = makePublicationDemandHarness();

  viewerDemand.publishViewerDemandForPublication(
    'remote-sam',
    publication(Track.Kind.Video, 'screen-share-without-window-id')
  );

  assert.equal(published.length, 0);
});

// --- #627: the demand must not spike while a republished track lacks metadata ---

function makeTile(dataset: Record<string, string> = {}): HTMLDivElement {
  return { dataset: { ...dataset } } as unknown as HTMLDivElement;
}

test('shareMediaSize records the media size while it is known', () => {
  const tile = makeTile();
  const size = shareMediaSize(tile, { videoWidth: 1684, videoHeight: 1288 });
  assert.deepEqual(size, { width: 1684, height: 1288 });
  assert.equal(tile.dataset.shareMediaWidth, '1684');
  assert.equal(tile.dataset.shareMediaHeight, '1288');
});

test('shareMediaSize keeps the last known size across a metadata gap', () => {
  const tile = makeTile();
  shareMediaSize(tile, { videoWidth: 1684, videoHeight: 1288 });
  // Exactly what a republished track looks like for a few hundred ms.
  const duringSwap = shareMediaSize(tile, { videoWidth: 0, videoHeight: 0 });
  assert.deepEqual(duringSwap, { width: 1684, height: 1288 });
});

test('shareMediaSize reports unknown only when nothing has ever been seen', () => {
  assert.deepEqual(shareMediaSize(makeTile(), null), { width: 0, height: 0 });
  assert.deepEqual(shareMediaSize(makeTile(), { videoWidth: 0, videoHeight: 0 }), {
    width: 0,
    height: 0,
  });
});

function makeShareTileHarness(videoWidth: number, videoHeight: number) {
  const video = {
    videoWidth,
    videoHeight,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 600, height: 400 }),
  };
  const tile = {
    dataset: { owner: 'remote-sam', windowId: '42' } as Record<string, string>,
    isConnected: true,
    querySelector: () => video,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 600, height: 400 }),
  } as unknown as HTMLDivElement;
  const published: Array<{ payload: Uint8Array }> = [];
  const ctx = {
    state: {
      room: {
        localParticipant: {
          identity: 'web-riley',
          publishData: async (payload: Uint8Array) => {
            published.push({ payload });
          },
        },
        remoteParticipants: new Map(),
      },
      viewerDemandSeq: 0,
    },
    hook: {},
  } as unknown as HarnessContext;
  return { tile, published, viewerDemand: setupViewerDemand(ctx) };
}

test('a tile that has never presented a frame sends a presence-only demand, not its element box', () => {
  // The element box is a layout artifact, not a display size. A viewer with
  // no frame (e.g. right after a republish with nothing remembered, or a
  // subscription that never delivered -- Defect D in the 2026-07-30 session)
  // must not drive the sender's capture resolution: one box-sized packet is
  // enough to reverse a committed downsize and keep the republish loop alive.
  const { tile, published, viewerDemand } = makeShareTileHarness(0, 0);

  viewerDemand.publishViewerDemand(tile, 'heartbeat');

  assert.equal(published.length, 1);
  const message = JSON.parse(new TextDecoder().decode(published[0]!.payload));
  assert.equal(message.visible, true, 'presence must survive: the quality floor depends on it');
  assert.equal(message.pixelWidth, 0);
  assert.equal(message.pixelHeight, 0);
  assert.equal(message.width, 0);
  assert.equal(message.height, 0);
});

test('a tile with a presented frame still sends real contained geometry', () => {
  const { tile, published, viewerDemand } = makeShareTileHarness(1200, 800);

  viewerDemand.publishViewerDemand(tile, 'heartbeat');

  assert.equal(published.length, 1);
  const message = JSON.parse(new TextDecoder().decode(published[0]!.payload));
  assert.equal(message.visible, true);
  // 1200x800 media contained in a 600x400 box at scale 1.
  assert.equal(message.pixelWidth, 600);
  assert.equal(message.pixelHeight, 400);
});

test('#248: a zoomed share asks for its committed zoom, not the frame being painted', () => {
  // The share's video rect as the browser reports it mid-pinch: painted at
  // 3x (1800x1200), with 2x committed when the last gesture ended.
  const { tile, published, viewerDemand } = makeShareTileHarness(1200, 800);
  const video = tile.querySelector('video') as unknown as { getBoundingClientRect: () => DOMRect };
  video.getBoundingClientRect = () => ({ left: -600, top: -400, width: 1800, height: 1200 }) as DOMRect;
  Object.assign(tile, { classList: { contains: (name: string) => name === 'is-share-zoomed' } });
  tile.dataset.shareZoomPaintedScale = '3.0000';
  tile.dataset.shareZoomDemandScale = '2.0000';

  viewerDemand.publishViewerDemand(tile, 'heartbeat');

  const message = JSON.parse(new TextDecoder().decode(published[0]!.payload));
  // The 600x400 box at the committed 2x, not the painted 3x.
  assert.equal(message.pixelWidth, 1200);
  assert.equal(message.pixelHeight, 800);
});

test('#627: a metadata gap no longer inflates the demand above the steady-state value', () => {
  // A letterboxed share in a wide tile: the contained media rect is much
  // narrower than the element box, so the demand the sender acts on differs a
  // lot between "contained" and "raw box". Before the fix the gap produced the
  // raw box, the sender read it as a request to UPSIZE, and it instantly
  // reversed the downsize it had just committed -- republishing twice every
  // ~8s and blacking the tile each time.
  const bounds = { left: 0, top: 0, width: 1200, height: 400 };
  const tile = makeTile();

  const steady = viewerDemandContainedMediaGeometry(
    bounds,
    shareMediaSize(tile, { videoWidth: 1684, videoHeight: 1288 }),
    1
  );
  const duringSwap = viewerDemandContainedMediaGeometry(
    bounds,
    shareMediaSize(tile, { videoWidth: 0, videoHeight: 0 }),
    1
  );

  assert.deepEqual(
    duringSwap,
    steady,
    'the demand during a track swap must equal the steady-state demand'
  );

  // Guard the premise: without the memo the gap really would inflate the
  // demand, so this test would be vacuous if the fallback ever stopped
  // widening the rect.
  const unguarded = viewerDemandContainedMediaGeometry(bounds, { width: 0, height: 0 }, 1);
  assert.ok(
    unguarded.pixelWidth > steady.pixelWidth,
    `premise check: raw-box demand ${unguarded.pixelWidth} must exceed contained demand ${steady.pixelWidth}`
  );
});

// --- inbound side: a receiver's starvation-watchdog repair request -------
//
// A native receiver that gives up probing for HIGH sends
// `publish_window_repair_request` (needsRepublish: true) over this same
// topic. A web client must route that into `cb.repairScreenShareForWindow`
// so a stalled browser share actually recovers -- before this it was one of
// several packet kinds silently swallowed by `handleViewerDemandPayload`'s
// no-op registration in connection.ts.

test('a repair request for this client targets repairScreenShareForWindow exactly once', () => {
  const { repairCalls, viewerDemand } = makeInboundRepairHarness();

  viewerDemand.handleViewerDemandPayload(repairRequestPayload(), 'native-quinn');

  assert.equal(repairCalls.length, 1);
  assert.deepEqual(repairCalls[0], { windowId: 42, requesterIdentity: 'native-quinn' });
});

test('a repair request falls back to the payload viewerId when the SFU sender identity is missing', () => {
  const { repairCalls, viewerDemand } = makeInboundRepairHarness();

  viewerDemand.handleViewerDemandPayload(repairRequestPayload(), undefined);

  assert.equal(repairCalls.length, 1);
  assert.equal(repairCalls[0]!.requesterIdentity, 'native-quinn');
});

test('a demand packet without needsRepublish is not treated as a repair request', () => {
  const { repairCalls, viewerDemand } = makeInboundRepairHarness();

  viewerDemand.handleViewerDemandPayload(repairRequestPayload({ needsRepublish: false }), 'native-quinn');
  viewerDemand.handleViewerDemandPayload(repairRequestPayload({ needsRepublish: undefined }), 'native-quinn');

  assert.equal(repairCalls.length, 0);
});

test('a repair request addressed to a different participant is ignored', () => {
  const { repairCalls, viewerDemand } = makeInboundRepairHarness();

  viewerDemand.handleViewerDemandPayload(
    repairRequestPayload({ targetUserId: 'someone-else' }),
    'native-quinn'
  );

  assert.equal(repairCalls.length, 0);
});

test('a malformed repair payload is dropped without throwing', () => {
  const { repairCalls, viewerDemand } = makeInboundRepairHarness();

  assert.doesNotThrow(() => {
    viewerDemand.handleViewerDemandPayload(new TextEncoder().encode('not json'), 'native-quinn');
  });
  assert.equal(repairCalls.length, 0);
});
