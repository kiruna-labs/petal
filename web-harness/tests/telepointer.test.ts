import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  containedMediaRect,
  colorForIdentity,
  inkForIdentity,
  mediaContentRect,
  mediaContentRectRelativeToTile,
  normalizedPointInContainedMedia,
  parseTelepointerPayload,
  telepointerKey,
  telepointerPosition,
  windowIdFromTrackName,
  type MediaTileLike
} from '../src/telepointer.ts';
import { setupTelepointerDisplay } from '../src/telepointerDisplay.ts';
import {
  createTelepointerSender,
  hoverTelepointerPointForTile,
  hoverTelepointerTargetFromTile,
  telepointerMessage,
  telepointerPublishOptions
} from '../src/telepointerSender.ts';
import { TELEPOINTER_TOPIC } from '../src/trackNames.ts';

type Listener = (event: PointerEvent) => void;

class FakeVideo {
  videoWidth = 1600;
  videoHeight = 900;

  getBoundingClientRect() {
    return { left: 0, top: 0, width: 400, height: 300 } as DOMRect;
  }
}

class FakeTile {
  dataset: Record<string, string | undefined> = { owner: 'native-1', windowId: '42' };
  video: FakeVideo | null = new FakeVideo();
  listeners = new Map<string, Listener[]>();

  addEventListener(type: string, listener: EventListenerOrEventListenerObject) {
    const fn = typeof listener === 'function' ? listener : listener.handleEvent.bind(listener);
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn as Listener]);
  }

  querySelector<T extends Element>(selector: string): T | null {
    return selector === 'video' ? (this.video as unknown as T) : null;
  }

  getBoundingClientRect() {
    return { left: 0, top: 0, width: 400, height: 300 } as DOMRect;
  }

  dispatchPointer(type: string, clientX: number, clientY: number) {
    const event = { currentTarget: this, clientX, clientY } as unknown as PointerEvent;
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

/** #892: a tile whose <video> rect differs from the tile's own rect -- the
 * header-inset shape every real share tile has, unlike FakeTile above (whose
 * tile/video rects coincide and so cannot catch a wrong-rect regression). */
class FakeTileWithVideoAt {
  private readonly tileRect: { left: number; top: number; width: number; height: number };
  private readonly video: { rect: { left: number; top: number; width: number; height: number }; width: number; height: number } | null;

  constructor(
    tileRect: { left: number; top: number; width: number; height: number },
    video: { rect: { left: number; top: number; width: number; height: number }; width: number; height: number } | null
  ) {
    this.tileRect = tileRect;
    this.video = video;
  }

  querySelector<T extends Element>(selector: string): T | null {
    if (selector !== 'video' || !this.video) return null;
    const { rect, width, height } = this.video;
    return { videoWidth: width, videoHeight: height, getBoundingClientRect: () => rect as DOMRect } as unknown as T;
  }

  getBoundingClientRect() {
    return this.tileRect as DOMRect;
  }
}

function decodeTelepointerPublish(publish: { data: Uint8Array; options: unknown }) {
  return {
    message: JSON.parse(new TextDecoder().decode(publish.data)),
    options: publish.options
  };
}

test('windowIdFromTrackName parses only petal window tracks', () => {
  assert.equal(windowIdFromTrackName('petal-window-42'), 42);
  assert.equal(windowIdFromTrackName('petal-window-4294967295'), 0xffff_ffff);
  assert.equal(windowIdFromTrackName('petal-camera-alice'), null);
  assert.equal(windowIdFromTrackName('petal-window-0'), null);
  assert.equal(windowIdFromTrackName('petal-window--1'), null);
  assert.equal(windowIdFromTrackName('petal-window-1.5'), null);
  assert.equal(windowIdFromTrackName(undefined), null);
});

test('parseTelepointerPayload accepts the native wire shape and trims user ids', () => {
  const payload = new TextEncoder().encode(
    JSON.stringify({ windowId: 42, userId: ' alice ', x: 0.25, y: 0.75, visible: true })
  );

  assert.deepEqual(parseTelepointerPayload(payload), {
    windowId: 42,
    userId: 'alice',
    x: 0.25,
    y: 0.75,
    visible: true
  });
});

test('parseTelepointerPayload preserves empty user id for receiver normalization', () => {
  assert.deepEqual(parseTelepointerPayload('{"windowId":7,"userId":"","x":0,"y":1,"visible":false}'), {
    windowId: 7,
    userId: '',
    x: 0,
    y: 1,
    visible: false
  });
});

test('parseTelepointerPayload accepts optional click and type activity markers', () => {
  assert.deepEqual(
    parseTelepointerPayload('{"windowId":7,"userId":"alice","x":0.4,"y":0.6,"visible":true,"activity":"click"}'),
    {
      windowId: 7,
      userId: 'alice',
      x: 0.4,
      y: 0.6,
      visible: true,
      activity: 'click'
    }
  );
  assert.deepEqual(
    parseTelepointerPayload('{"windowId":7,"userId":"alice","x":0.4,"y":0.6,"visible":true,"activity":"type"}')
      ?.activity,
    'type'
  );
});

test('parseTelepointerPayload rejects unknown activity markers', () => {
  assert.equal(
    parseTelepointerPayload('{"windowId":1,"userId":"a","x":0,"y":0,"visible":true,"activity":"dance"}'),
    null
  );
});

test('parseTelepointerPayload rejects malformed payloads', () => {
  assert.equal(parseTelepointerPayload('not json'), null);
  assert.equal(parseTelepointerPayload('{"windowId":0,"userId":"a","x":0,"y":0,"visible":true}'), null);
  assert.equal(parseTelepointerPayload('{"windowId":1,"userId":"a","x":"0","y":0,"visible":true}'), null);
  assert.equal(parseTelepointerPayload('{"windowId":1,"userId":"a","x":0,"y":0,"visible":"yes"}'), null);
});

test('containedMediaRect returns the object-fit contain video content rect', () => {
  assert.deepEqual(containedMediaRect({ left: 0, top: 0, width: 400, height: 300 }, { width: 1600, height: 900 }), {
    left: 0,
    top: 37.5,
    width: 400,
    height: 225
  });

  assert.deepEqual(containedMediaRect({ left: 0, top: 0, width: 400, height: 300 }, { width: 600, height: 900 }), {
    left: 100,
    top: 0,
    width: 200,
    height: 300
  });
});

test('normalizedPointInContainedMedia (#892 consolidated letterbox helper) covers letterbox, pillarbox, and mixed geometries', () => {
  // Letterbox: media wider than bounds -> bars top/bottom, center maps to (0.5,0.5).
  assert.deepEqual(
    normalizedPointInContainedMedia({ left: 0, top: 0, width: 400, height: 300 }, { width: 1600, height: 900 }, { x: 200, y: 150 }),
    { x: 0.5, y: 0.5 }
  );
  assert.deepEqual(
    normalizedPointInContainedMedia({ left: 0, top: 0, width: 400, height: 300 }, { width: 1600, height: 900 }, { x: 0, y: 37.5 }),
    { x: 0, y: 0 }
  );

  // Pillarbox: media narrower than bounds -> bars left/right.
  assert.deepEqual(
    normalizedPointInContainedMedia({ left: 0, top: 0, width: 400, height: 300 }, { width: 600, height: 900 }, { x: 200, y: 150 }),
    { x: 0.5, y: 0.5 }
  );
  assert.deepEqual(
    normalizedPointInContainedMedia({ left: 0, top: 0, width: 400, height: 300 }, { width: 600, height: 900 }, { x: 100, y: 0 }),
    { x: 0, y: 0 }
  );

  // Mixed (#892): the phantom tile rect (400x300, aspect 1.333) letterboxes
  // media aspect 1.45, but the real header-inset video box (398x254, aspect
  // 1.567) PILLARBOXES the same media -- the regime where the draw-offset
  // bug was wrong on BOTH axes, not just a constant Y shift.
  const media = { width: 1450, height: 1000 }; // aspect 1.45
  const tileBounds = { left: 0, top: 0, width: 400, height: 300 }; // aspect 1.333
  const videoBounds = { left: 1, top: 45, width: 398, height: 254 }; // aspect 1.567
  assert.ok(containedMediaRect(tileBounds, media).top > tileBounds.top, 'phantom tile rect letterboxes (top inset)');
  assert.ok(containedMediaRect(videoBounds, media).left > videoBounds.left, 'real video box pillarboxes (left inset)');
  const tileBasis = normalizedPointInContainedMedia(tileBounds, media, { x: 200, y: 150 });
  const videoBasis = normalizedPointInContainedMedia(videoBounds, media, { x: 200, y: 150 });
  assert.notDeepEqual(tileBasis, videoBasis, 'tile-rect and video-rect bases must diverge on the same raw point');
});

test('mediaContentRect and mediaContentRectRelativeToTile pick the video rect over the tile rect, falling back to the tile when no video is present (#892)', () => {
  const headerInsetTile = new FakeTileWithVideoAt(
    { left: 0, top: 0, width: 400, height: 300 },
    { rect: { left: 1, top: 45, width: 398, height: 254 }, width: 1600, height: 900 }
  );

  const absolute = mediaContentRect(headerInsetTile as unknown as MediaTileLike);
  assert.deepEqual(absolute.bounds, { left: 1, top: 45, width: 398, height: 254 });
  assert.deepEqual(absolute.media, { width: 1600, height: 900 });

  // Tile happens to sit at viewport (0,0) here, so relative-to-tile equals
  // the absolute video rect; the two differ only when the tile itself is
  // offset in the viewport.
  const relative = mediaContentRectRelativeToTile(headerInsetTile as unknown as MediaTileLike);
  assert.deepEqual(relative.bounds, { left: 1, top: 45, width: 398, height: 254 });

  const offsetTile = new FakeTileWithVideoAt(
    { left: 50, top: 20, width: 400, height: 300 },
    { rect: { left: 51, top: 65, width: 398, height: 254 }, width: 1600, height: 900 }
  );
  assert.deepEqual(mediaContentRectRelativeToTile(offsetTile as unknown as MediaTileLike).bounds, {
    left: 1,
    top: 45,
    width: 398,
    height: 254
  });

  const noVideoTile = new FakeTileWithVideoAt({ left: 10, top: 20, width: 400, height: 300 }, null);
  const fallback = mediaContentRect(noVideoTile as unknown as MediaTileLike);
  assert.deepEqual(fallback.bounds, { left: 10, top: 20, width: 400, height: 300 });
  assert.deepEqual(fallback.media, { width: 0, height: 0 });
});

test('telepointerPosition clamps normalized coordinates inside letterboxed media', () => {
  const point = telepointerPosition(
    { left: 0, top: 0, width: 400, height: 300 },
    { width: 1600, height: 900 },
    { x: 1.4, y: -0.2 }
  );

  assert.deepEqual(point, { x: 400, y: 37.5 });
});

test('telepointerKey is scoped by user and window', () => {
  assert.equal(telepointerKey({ userId: 'alice', windowId: 42 }), 'alice:42');
});

test('identity colors expose contrast ink for active sharing controls', () => {
  assert.equal(colorForIdentity('web-alpha'), '#6e8bff');
  assert.equal(colorForIdentity('native-user-42'), '#d6b8f0');
  assert.equal(colorForIdentity('web-alpha', 3), '#e8b84b');
  assert.equal(inkForIdentity('web-alpha'), '#081129');
  assert.equal(inkForIdentity('native-user-42'), '#1f102b');
});

test('telepointer display records spoofed payload userId as authenticated sender', () => {
  const originalDocument = globalThis.document;
  Object.defineProperty(globalThis, 'document', {
    configurable: true,
    value: {
      querySelector: () => null,
      querySelectorAll: () => []
    }
  });

  try {
    const remoteTelepointers = new Map();
    const logs: Array<{ message: string; kind?: string }> = [];
    const display = setupTelepointerDisplay({
      remoteTelepointers,
      handshakeCooldowns: new Map(),
      state: { room: null },
      ui: {
        logEvent: (message: string, kind?: string) => logs.push({ message, kind })
      }
    } as never);

    display.handleRemoteTelepointerPayload(
      new TextEncoder().encode(
        JSON.stringify({ windowId: 42, userId: 'victim-user', x: 0.25, y: 0.75, visible: true })
      ),
      ' authenticated-sender ',
      TELEPOINTER_TOPIC
    );

    assert.equal(remoteTelepointers.has('victim-user:42'), false);
    assert.equal(remoteTelepointers.has('authenticated-sender:42'), true);
    assert.equal(remoteTelepointers.get('authenticated-sender:42')?.message.userId, 'authenticated-sender');
    assert.deepEqual(logs, []);
  } finally {
    if (originalDocument === undefined) {
      Reflect.deleteProperty(globalThis, 'document');
    } else {
      Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
    }
  }
});

test('telepointer sender helpers preserve the native hover wire contract', () => {
  assert.deepEqual(telepointerPublishOptions(), { reliable: false, topic: TELEPOINTER_TOPIC });
  assert.deepEqual(telepointerMessage(42, 'web-1', { x: 0.25, y: 0.75 }, true), {
    windowId: 42,
    userId: 'web-1',
    x: 0.25,
    y: 0.75,
    visible: true
  });
  assert.deepEqual(telepointerMessage(42, 'web-1', { x: 1, y: 0 }, false), {
    windowId: 42,
    userId: 'web-1',
    x: 1,
    y: 0,
    visible: false
  });
});

test('hover telepointer target parsing accepts only remote share window tiles', () => {
  assert.deepEqual(hoverTelepointerTargetFromTile({ dataset: { owner: 'native-1', windowId: '42' } }), {
    targetUserId: 'native-1',
    windowId: 42
  });
  assert.equal(hoverTelepointerTargetFromTile({ dataset: { owner: 'native-1', windowId: '0' } }), null);
  assert.equal(hoverTelepointerTargetFromTile({ dataset: { owner: '', windowId: '42' } }), null);
  assert.equal(hoverTelepointerTargetFromTile({ dataset: { owner: 'native-1', windowId: 'petal-window-42' } }), null);
});

test('hover telepointer coordinates normalize within the contained video rect', () => {
  const tile = new FakeTile();

  assert.deepEqual(
    hoverTelepointerPointForTile(tile as unknown as Parameters<typeof hoverTelepointerPointForTile>[0], {
      clientX: 200,
      clientY: 150
    }),
    { x: 0.5, y: 0.5 }
  );

  assert.deepEqual(
    hoverTelepointerPointForTile(tile as unknown as Parameters<typeof hoverTelepointerPointForTile>[0], {
      clientX: 200,
      clientY: 0
    }),
    { x: 0.5, y: 0 }
  );
});

test('hover telepointer binding publishes visible enter and hidden leave updates', async () => {
  const publishes: Array<{ data: Uint8Array; options: unknown }> = [];
  const room = {
    localParticipant: {
      identity: 'web-1',
      publishData: (data: Uint8Array, options: unknown) => {
        publishes.push({ data, options });
        return Promise.resolve();
      }
    }
  };
  const tile = new FakeTile();
  const { bindHoverTelepointer } = createTelepointerSender({
    windowId: 99,
    getRoom: () => room as never
  });

  bindHoverTelepointer(tile as never);
  bindHoverTelepointer(tile as never);
  assert.equal(tile.listeners.get('pointerenter')?.length, 1);

  tile.dispatchPointer('pointerenter', 200, 150);
  tile.dispatchPointer('pointerleave', 390, 260);
  await Promise.resolve();

  assert.equal(publishes.length, 2);
  assert.deepEqual(decodeTelepointerPublish(publishes[0]!), {
    options: { reliable: false, topic: TELEPOINTER_TOPIC },
    message: { windowId: 42, userId: 'web-1', x: 0.5, y: 0.5, visible: true }
  });
  assert.deepEqual(decodeTelepointerPublish(publishes[1]!), {
    options: { reliable: false, topic: TELEPOINTER_TOPIC },
    message: {
      windowId: 42,
      userId: 'web-1',
      x: 0.975,
      y: 0.9888888888888889,
      visible: false
    }
  });
});

test('cockpit telepointer publishes to the remote share tile window id, not the local harness id', async () => {
  const originalDocument = globalThis.document;
  const publishes: Array<{ data: Uint8Array; options: unknown }> = [];
  const room = {
    localParticipant: {
      identity: 'web-1',
      publishData: (data: Uint8Array, options: unknown) => {
        publishes.push({ data, options });
        return Promise.resolve();
      }
    }
  };
  const tile = new FakeTile();
  const fakeDocument = {
    querySelectorAll: (selector: string) =>
      selector === '.share-tile[data-owner][data-window-id]' ? [tile] : []
  } as unknown as Document;
  Object.defineProperty(globalThis, 'document', { configurable: true, value: fakeDocument });
  try {
    const { publishCockpitTelepointer } = createTelepointerSender({
      windowId: 99,
      getRoom: () => room as never
    });

    const result = await publishCockpitTelepointer();

    assert.equal(result.windowId, 42);
    assert.equal(publishes.length, 1);
    assert.deepEqual(decodeTelepointerPublish(publishes[0]!), {
      options: { reliable: false, topic: TELEPOINTER_TOPIC },
      message: { windowId: 42, userId: 'web-1', x: 0.42, y: 0.58, visible: true }
    });
  } finally {
    Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
  }
});

test('web telepointer chrome has no permanent glyph halo or name-pill outline', () => {
  const display = readFileSync(new URL('../src/telepointerDisplay.ts', import.meta.url), 'utf8');
  const styles = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

  assert.doesNotMatch(display, /remote-telepointer__arrow-halo/);
  assert.doesNotMatch(styles, /\.remote-telepointer__arrow-halo/);
  assert.doesNotMatch(styles, /\.remote-telepointer__label\s*\{[^}]*box-shadow\s*:/s);
});
