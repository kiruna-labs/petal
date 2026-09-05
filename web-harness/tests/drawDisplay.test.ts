import { test } from 'node:test';
import assert from 'node:assert/strict';

import { setupDrawDisplay } from '../src/drawDisplay.ts';
import { DRAW_TOPIC } from '../src/trackNames.ts';
import { colorForIdentity } from '../src/telepointer.ts';

class FakeClassList {
  private readonly element: FakeElement;

  constructor(element: FakeElement) {
    this.element = element;
  }

  private values(): string[] {
    return this.element.className.split(/\s+/).filter(Boolean);
  }

  add(name: string) {
    const values = new Set(this.values());
    values.add(name);
    this.element.className = Array.from(values).join(' ');
  }

  contains(name: string): boolean {
    return this.values().includes(name);
  }
}

class FakeStyle {
  private readonly values = new Map<string, string>();

  setProperty(name: string, value: string) {
    this.values.set(name, value);
  }

  getPropertyValue(name: string): string {
    return this.values.get(name) ?? '';
  }
}

class FakeElement {
  className = '';
  textContent = '';
  dataset: Record<string, string | undefined> = {};
  readonly attributes = new Map<string, string>();
  readonly children: FakeElement[] = [];
  readonly classList = new FakeClassList(this);
  readonly style = new FakeStyle();
  parentElement: FakeElement | null = null;
  videoWidth = 0;
  videoHeight = 0;

  readonly tagName: string;
  private readonly rect: { left: number; top: number; width: number; height: number };

  constructor(tagName: string, rect = { left: 0, top: 0, width: 400, height: 300 }) {
    this.tagName = tagName;
    this.rect = rect;
  }

  get parentNode(): FakeElement | null {
    return this.parentElement;
  }

  appendChild<T extends FakeElement>(child: T): T {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  remove() {
    if (!this.parentElement) return;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    if (index !== -1) siblings.splice(index, 1);
    this.parentElement = null;
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }

  getAttribute(name: string): string | null {
    return this.attributes.get(name) ?? null;
  }

  querySelector<T extends Element>(selector: string): T | null {
    return (this.querySelectorAll(selector)[0] as unknown as T | undefined) ?? null;
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    const matches: FakeElement[] = [];
    for (const child of this.children) {
      if (matchesSelector(child, selector)) matches.push(child);
      matches.push(...(child.querySelectorAll(selector) as unknown as FakeElement[]));
    }
    return matches as unknown as T[];
  }

  getBoundingClientRect() {
    return this.rect as DOMRect;
  }
}

class FakeDocument {
  readonly body = new FakeElement('body');

  createElement(tagName: string): FakeElement {
    return new FakeElement(tagName.toLowerCase());
  }

  createElementNS(_namespace: string, tagName: string): FakeElement {
    return new FakeElement(tagName.toLowerCase());
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    return this.body.querySelectorAll(selector);
  }
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  if (selector === 'video') return element.tagName === 'video';
  if (selector === '.share-tile') return element.classList.contains('share-tile');
  if (selector === '.tile[data-owner]') return element.classList.contains('tile') && Boolean(element.dataset.owner);
  if (selector.startsWith('.')) return element.classList.contains(selector.slice(1));
  return false;
}

function installFakeDom() {
  const originalDocument = globalThis.document;
  const document = new FakeDocument();
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  return {
    document,
    restore: () => {
      if (originalDocument === undefined) Reflect.deleteProperty(globalThis, 'document');
      else Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
    },
  };
}

function makeShareTile(owner: string, windowId: number, rect = { left: 0, top: 0, width: 400, height: 300 }) {
  const tile = new FakeElement('div', rect);
  tile.className = 'tile share-tile';
  tile.dataset.owner = owner;
  tile.dataset.windowId = String(windowId);
  const video = new FakeElement('video');
  video.videoWidth = 1600;
  video.videoHeight = 900;
  tile.appendChild(video);
  return tile;
}

function makeCameraTile(owner: string, windowId: number, rect = { left: 0, top: 0, width: 400, height: 300 }) {
  const tile = new FakeElement('div', rect);
  tile.className = 'tile camera-ready';
  tile.dataset.owner = owner;
  tile.dataset.drawWindowId = String(windowId);
  const video = new FakeElement('video');
  video.videoWidth = 1280;
  video.videoHeight = 720;
  tile.appendChild(video);
  return tile;
}

/** #892: a share tile whose <video> is inset under a docked remote-window
 * header (top:44px + 1px border), unlike makeShareTile's default where the
 * video has no explicit rect and silently coincides with the tile's --
 * exactly the shape that let the offset bug ship with strokes rendering
 * "correctly" in every existing fixture. */
function makeHeaderBearingShareTile(owner: string, windowId: number) {
  const tile = new FakeElement('div', { left: 50, top: 20, width: 400, height: 300 });
  tile.className = 'tile share-tile has-remote-window-header';
  tile.dataset.owner = owner;
  tile.dataset.windowId = String(windowId);
  const video = new FakeElement('video', { left: 51, top: 65, width: 398, height: 254 });
  video.videoWidth = 1600;
  video.videoHeight = 900;
  tile.appendChild(video);
  return tile;
}

function encode(message: unknown): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(message));
}

test('draw display (local echo + receive path) renders strokes against the video content box, not the bare tile, under a docked header (#892)', () => {
  const fakeDom = installFakeDom();
  try {
    const target = makeHeaderBearingShareTile('native-1', 42);
    fakeDom.document.body.appendChild(target);
    const display = setupDrawDisplay({
      ui: { logEvent: () => undefined },
    } as never);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'begin',
        ownerIdentity: 'native-1',
        windowId: 42,
        seq: 1,
        strokeId: 'stroke-header',
        points: [{ x: 0.5, y: 0.5 }],
      }),
      'drawer-1',
      DRAW_TOPIC
    );

    const path = target.querySelector('.remote-draw-stroke') as unknown as FakeElement | null;
    assert.ok(path);
    // video content box relative to the tile: {left:1, top:45, width:398,
    // height:254} over 1600x900 media letterboxes (top inset 15.0625) to
    // {left:1, top:60.0625, width:398, height:223.875}; (0.5,0.5) projects
    // to (200.0, 172.0). The old bare-tile-rect bug instead used
    // {0,0,400,300} -> (200.0, 150.0): same X, wrong Y by the header inset.
    assert.equal(path!.getAttribute('d'), 'M 200.0 172.0');
  } finally {
    fakeDom.restore();
  }
});

test('draw display renders authenticated strokes on the matching owner/window share tile', () => {
  const fakeDom = installFakeDom();
  try {
    const target = makeShareTile('native-1', 42);
    const sameWindowDifferentOwner = makeShareTile('native-2', 42);
    fakeDom.document.body.appendChild(target);
    fakeDom.document.body.appendChild(sameWindowDifferentOwner);
    const logs: Array<{ message: string; kind?: string }> = [];
    const display = setupDrawDisplay({
      ui: { logEvent: (message: string, kind?: string) => logs.push({ message, kind }) },
    } as never);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'begin',
        ownerIdentity: 'native-1',
        windowId: 42,
        seq: 1,
        strokeId: 'stroke-a',
        points: [{ x: 0.5, y: 0.5 }],
      }),
      ' drawer-1 ',
      DRAW_TOPIC
    );

    const targetPath = target.querySelector('.remote-draw-stroke') as unknown as FakeElement | null;
    assert.ok(targetPath);
    assert.equal(targetPath.getAttribute('d'), 'M 200.0 150.0');
    assert.equal(targetPath.style.getPropertyValue('--draw-color'), colorForIdentity('drawer-1'));
    assert.equal(
      (target.querySelector('.remote-draw-layer') as unknown as FakeElement | null)?.getAttribute('viewBox'),
      '0 0 400.0 300.0'
    );
    assert.equal(sameWindowDifferentOwner.querySelector('.remote-draw-stroke'), null);
    assert.deepEqual(logs, []);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'points',
        ownerIdentity: 'native-1',
        windowId: 42,
        seq: 2,
        strokeId: 'stroke-a',
        points: [{ x: 0.75, y: 1 }],
      }),
      'drawer-1',
      DRAW_TOPIC
    );

    assert.equal(targetPath.getAttribute('d'), 'M 200.0 150.0 L 300.0 262.5');
  } finally {
    fakeDom.restore();
  }
});

test('draw display renders anchored text annotations with authenticated color', () => {
  const fakeDom = installFakeDom();
  try {
    const target = makeShareTile('native-1', 42);
    fakeDom.document.body.appendChild(target);
    const display = setupDrawDisplay({
      ui: { logEvent: () => undefined },
    } as never);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'text',
        ownerIdentity: 'native-1',
        windowId: 42,
        seq: 1,
        strokeId: 'text-a',
        points: [{ x: 0.25, y: 0.5 }],
        text: 'Hello Petal',
      }),
      'drawer-1',
      DRAW_TOPIC
    );

    const annotation = target.querySelector('.remote-draw-text') as unknown as FakeElement | null;
    assert.ok(annotation);
    assert.equal(annotation.textContent, 'Hello Petal');
    assert.equal(annotation.style.getPropertyValue('left'), '100px');
    assert.equal(annotation.style.getPropertyValue('top'), '150px');
    assert.equal(annotation.style.getPropertyValue('color'), colorForIdentity('drawer-1'));
  } finally {
    fakeDom.restore();
  }
});

test('draw display renders a sharer-originated stroke when drawer and owner match', () => {
  const fakeDom = installFakeDom();
  try {
    const target = makeShareTile('native-sharer', 42);
    fakeDom.document.body.appendChild(target);
    const display = setupDrawDisplay({
      ui: { logEvent: () => undefined },
    } as never);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'begin',
        ownerIdentity: 'native-sharer',
        windowId: 42,
        seq: 1,
        strokeId: 'sharer-stroke',
        points: [{ x: 0.25, y: 0.5 }],
      }),
      'native-sharer',
      DRAW_TOPIC
    );

    const path = target.querySelector('.remote-draw-stroke') as unknown as FakeElement | null;
    assert.ok(path);
    assert.equal(path.dataset.drawer, 'native-sharer');
    assert.equal(path.dataset.owner, 'native-sharer');
    assert.equal(path.getAttribute('d'), 'M 100.0 150.0');
  } finally {
    fakeDom.restore();
  }
});

test('draw display renders authenticated strokes on camera draw targets without data-window-id', () => {
  const fakeDom = installFakeDom();
  try {
    const cameraWindowId = 0x8000_1234;
    const target = makeCameraTile('web-1', cameraWindowId);
    const sameSyntheticIdDifferentOwner = makeCameraTile('web-2', cameraWindowId);
    fakeDom.document.body.appendChild(target);
    fakeDom.document.body.appendChild(sameSyntheticIdDifferentOwner);
    const display = setupDrawDisplay({
      ui: { logEvent: () => undefined },
    } as never);

    assert.equal(target.dataset.windowId, undefined);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'begin',
        ownerIdentity: 'web-1',
        windowId: cameraWindowId,
        seq: 1,
        strokeId: 'camera-stroke',
        points: [{ x: 0.5, y: 0.5 }],
      }),
      'drawer-1',
      DRAW_TOPIC
    );

    const targetPath = target.querySelector('.remote-draw-stroke') as unknown as FakeElement | null;
    assert.ok(targetPath);
    assert.equal(targetPath.getAttribute('d'), 'M 200.0 150.0');
    assert.equal(sameSyntheticIdDifferentOwner.querySelector('.remote-draw-stroke'), null);
  } finally {
    fakeDom.restore();
  }
});

test('draw display still renders share targets via data-window-id fallback', () => {
  const fakeDom = installFakeDom();
  try {
    const target = makeShareTile('native-1', 42);
    fakeDom.document.body.appendChild(target);
    const display = setupDrawDisplay({
      ui: { logEvent: () => undefined },
    } as never);

    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'begin',
        ownerIdentity: 'native-1',
        windowId: 42,
        seq: 1,
        strokeId: 'share-stroke',
        points: [{ x: 0.5, y: 0.5 }],
      }),
      'drawer-1',
      DRAW_TOPIC
    );

    assert.ok(target.querySelector('.remote-draw-stroke'));
  } finally {
    fakeDom.restore();
  }
});

test('draw display ignores the dead clear message type and logs rejected payloads (#670)', () => {
  const fakeDom = installFakeDom();
  try {
    const target = makeShareTile('native-1', 42);
    fakeDom.document.body.appendChild(target);
    const logs: Array<{ message: string; kind?: string }> = [];
    const display = setupDrawDisplay({
      ui: { logEvent: (message: string, kind?: string) => logs.push({ message, kind }) },
    } as never);
    const begin = {
      v: 1,
      type: 'begin',
      ownerIdentity: 'native-1',
      windowId: 42,
      seq: 1,
      strokeId: 'stroke-a',
      points: [{ x: 0.5, y: 0.5 }],
    };

    display.handleRemoteDrawPayload(encode(begin), 'drawer-1', DRAW_TOPIC);
    display.handleRemoteDrawPayload(encode({ ...begin, strokeId: 'stroke-b' }), 'drawer-2', DRAW_TOPIC);
    assert.equal(target.querySelectorAll('.remote-draw-stroke').length, 2);

    // #670: `clear` is receive-only dead code -- no sender (native or web)
    // ever emits it, since a 10s auto-fade (strokeExpiry.test.ts) replaced
    // the need for an explicit clear. Still a structurally valid wire
    // message (the contract fixture pins it), but it must now be a no-op:
    // strokes stay exactly as they were.
    display.handleRemoteDrawPayload(
      encode({
        v: 1,
        type: 'clear',
        ownerIdentity: 'native-1',
        windowId: 42,
        seq: 2,
        strokeId: null,
        points: [],
      }),
      'drawer-1',
      DRAW_TOPIC
    );
    assert.equal(target.querySelectorAll('.remote-draw-stroke').length, 2);

    display.handleRemoteDrawPayload(encode(begin), ' ', DRAW_TOPIC);
    display.handleRemoteDrawPayload(encode({ ...begin, points: [{ x: 2, y: 0 }] }), 'drawer-1', DRAW_TOPIC);
    assert.deepEqual(logs, [
      { message: 'ignored draw payload without authenticated sender identity', kind: 'warn' },
      { message: 'ignored malformed draw payload', kind: 'warn' },
    ]);
  } finally {
    fakeDom.restore();
  }
});
