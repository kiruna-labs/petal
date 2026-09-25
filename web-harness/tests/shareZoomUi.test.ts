import { mock, test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import type { HarnessContext } from '../src/context.ts';
import { setupShareZoom } from '../src/shareZoomUi.ts';
import { SHARE_ZOOM_FIT, sharePointToPicture } from '../src/shareZoom.ts';

// #248 PR 2: the gesture layer. Zoom/pan/fit-fill only in View mode; Control
// and Draw modes and rail thumbnails are left to their own handlers.

type Listener = (event: FakeEvent) => void;

interface FakeEvent {
  type: string;
  target: FakeElement;
  clientX: number;
  clientY: number;
  pointerId: number;
  pointerType: string;
  button: number;
  timeStamp: number;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
  key: string;
  deltaX: number;
  deltaY: number;
  deltaMode: number;
  scale: number;
  touches: unknown[];
  defaultPrevented: boolean;
  propagationStopped: boolean;
  preventDefault(): void;
  stopPropagation(): void;
}

class FakeElement {
  className = '';
  hidden = false;
  textContent = '';
  title = '';
  type = '';
  clientWidth = 0;
  clientHeight = 0;
  offsetTop = 0;
  offsetLeft = 0;
  offsetWidth = 0;
  offsetHeight = 0;
  videoWidth = 0;
  videoHeight = 0;
  isConnected = true;
  parentElement: FakeElement | null = null;
  readonly dataset: Record<string, string | undefined> = {};
  readonly children: FakeElement[] = [];
  readonly listeners = new Map<string, Array<{ listener: Listener; capture: boolean; passive: boolean | undefined }>>();
  readonly styles = new Map<string, string>();
  readonly attributes = new Map<string, string>();
  attributeWrites = 0;
  readonly focusOptions: unknown[] = [];
  readonly style = {
    setProperty: (name: string, value: string) => this.styles.set(name, value),
    removeProperty: (name: string) => this.styles.delete(name),
  };
  readonly classList = {
    contains: (name: string) => this.className.split(/\s+/).includes(name),
    add: (name: string) => {
      if (!this.classList.contains(name)) this.className = `${this.className} ${name}`.trim();
    },
    remove: (name: string) => {
      this.className = this.className
        .split(/\s+/)
        .filter((value) => value && value !== name)
        .join(' ');
    },
  };
  rect = { left: 0, top: 0, width: 0, height: 0 };
  readonly tagName: string;

  constructor(tagName: string) {
    this.tagName = tagName;
  }

  appendChild(child: FakeElement) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  append(...children: FakeElement[]) {
    children.forEach((child) => this.appendChild(child));
  }

  contains(other: FakeElement | null): boolean {
    for (let node = other; node; node = node.parentElement) if (node === this) return true;
    return false;
  }

  matches(selector: string): boolean {
    return selector.split(',').some((part) => {
      const trimmed = part.trim();
      if (trimmed === this.tagName) return true;
      if (trimmed.startsWith('.')) return trimmed.split('.').filter(Boolean).every((name) => this.classList.contains(name));
      return false;
    });
  }

  closest(selector: string): FakeElement | null {
    for (let node: FakeElement | null = this; node; node = node.parentElement) if (node.matches(selector)) return node;
    return null;
  }

  querySelector(selector: string): FakeElement | null {
    for (const child of this.children) {
      if (child.matches(selector)) return child;
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }

  setAttribute(name: string, value: string) {
    this.attributeWrites += 1;
    this.attributes.set(name, value);
  }

  addEventListener(type: string, listener: Listener, options?: boolean | { capture?: boolean; passive?: boolean }) {
    const capture = typeof options === 'object' ? options.capture === true : options === true;
    const passive = typeof options === 'object' ? options.passive : undefined;
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), { listener, capture, passive }]);
  }

  focus(options?: unknown) {
    this.focusOptions.push(options);
    (globalThis.document as unknown as { activeElement: FakeElement }).activeElement = this;
  }

  fire(type: string) {
    for (const { listener } of this.listeners.get(type) ?? []) listener(makeEvent(type, this));
  }

  setPointerCapture() {}
  releasePointerCapture() {}

  getBoundingClientRect() {
    return { ...this.rect, right: this.rect.left + this.rect.width, bottom: this.rect.top + this.rect.height };
  }
}

function makeEvent(type: string, target: FakeElement, init: Partial<FakeEvent> = {}): FakeEvent {
  return {
    type,
    target,
    clientX: 0,
    clientY: 0,
    pointerId: 1,
    pointerType: 'mouse',
    button: 0,
    timeStamp: 0,
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    key: '',
    deltaX: 0,
    deltaY: 0,
    deltaMode: 0,
    scale: 1,
    touches: [],
    defaultPrevented: false,
    propagationStopped: false,
    preventDefault() {
      this.defaultPrevented = true;
    },
    stopPropagation() {
      this.propagationStopped = true;
    },
    ...init,
  };
}

/** Capture-phase listeners on the surface run first, then those of the
 * target and each ancestor up to the surface (the chip, the share tile's own
 * wheel/touchmove, a pinning click handler standing in for tileLayout.ts),
 * then the surface's bubble listeners -- unless something stopped
 * propagation. An event outside the surface never reaches it. */
function dispatch(surface: FakeElement, event: FakeEvent, onTileClick?: () => void) {
  const inside = surface.contains(event.target);
  if (inside) {
    for (const { listener } of (surface.listeners.get(event.type) ?? []).filter((entry) => entry.capture)) listener(event);
  }
  if (event.propagationStopped) return event;
  for (let node: FakeElement | null = event.target; node && node !== surface; node = node.parentElement) {
    for (const { listener } of node.listeners.get(event.type) ?? []) listener(event);
    if (event.propagationStopped) return event;
  }
  if (!inside) return event;
  if (event.type === 'click') onTileClick?.();
  for (const { listener } of (surface.listeners.get(event.type) ?? []).filter((entry) => !entry.capture)) listener(event);
  return event;
}

function setup(options: { activeRemoteControl?: boolean; media?: { width: number; height: number } } = {}) {
  const originalDocument = globalThis.document;
  const originalRaf = (globalThis as { requestAnimationFrame?: unknown }).requestAnimationFrame;
  const body = new FakeElement('body');
  const fakeDocument = { createElement: (tag: string) => new FakeElement(tag.toLowerCase()), activeElement: body };
  Object.defineProperty(globalThis, 'document', { configurable: true, value: fakeDocument });
  delete (globalThis as { requestAnimationFrame?: unknown }).requestAnimationFrame;

  const surface = new FakeElement('div');
  surface.className = 'tiles layout-spotlight';
  const tile = surface.appendChild(new FakeElement('div'));
  tile.className = 'tile share-tile is-spotlight has-remote-window-header';
  tile.rect = { left: 0, top: 0, width: 800, height: 494 };
  tile.clientWidth = 800;
  tile.clientHeight = 494;
  const header = tile.appendChild(new FakeElement('div'));
  header.className = 'remote-window-header';
  const video = tile.appendChild(new FakeElement('video'));
  // By default a 4:3 window in the 800x450 video box under the 44px header.
  video.clientWidth = video.offsetWidth = 800;
  video.clientHeight = video.offsetHeight = 450;
  video.offsetTop = 44;
  video.videoWidth = options.media?.width ?? 1600;
  video.videoHeight = options.media?.height ?? 1200;
  video.getBoundingClientRect = () => {
    // Report the transform the controller applied, as a browser would.
    const match = /translate\(([-\d.]+)px, ([-\d.]+)px\) scale\(([\d.]+)\)/.exec(
      tile.styles.get('--share-zoom-transform') ?? ''
    );
    const [x, y, s] = match ? [Number(match[1]), Number(match[2]), Number(match[3])] : [0, 0, 1];
    return { left: x, top: 44 + y, width: 800 * s, height: 450 * s, right: x + 800 * s, bottom: 44 + y + 450 * s };
  };

  let repositions = 0;
  const demands: Array<{ scale: string | undefined; painted: string | undefined }> = [];
  const ctx = {
    dom: { tilesEl: surface },
    cb: {
      activeRemoteControlForTile: () => (options.activeRemoteControl ? {} : null),
      repositionRemoteTelepointers: () => {
        repositions += 1;
      },
      repositionRemoteDraw: () => {},
      publishViewerDemand: () => {
        demands.push({ scale: tile.dataset.shareZoomDemandScale, painted: tile.dataset.shareZoomPaintedScale });
      },
    },
  } as unknown as HarnessContext;
  const zoom = setupShareZoom(ctx);
  // As tiles.ts does for every share tile.
  zoom.bindTile(tile as unknown as HTMLElement);
  const h = {
    surface,
    body,
    focused: () => fakeDocument.activeElement,
    tile,
    header,
    video,
    zoom,
    demands,
    repositions: () => repositions,
    chip: () => tile.children.find((child) => child.className === 'share-zoom-chip') ?? null,
    scale: () => zoom.shareZoomFor(tile as unknown as HTMLElement).scale,
    translate: () => {
      const match = /translate\(([-\d.]+)px, ([-\d.]+)px\)/.exec(tile.styles.get('--share-zoom-transform') ?? '');
      return match ? { x: Number(match[1]), y: Number(match[2]) } : { x: 0, y: 0 };
    },
    wheel: (init: Partial<FakeEvent>) => dispatch(surface, makeEvent('wheel', video, { clientX: 400, clientY: 269, ...init })),
    pointer: (type: string, init: Partial<FakeEvent>) => dispatch(surface, makeEvent(type, video, init)),
    tap: (at: number, init: Partial<FakeEvent> = {}) => {
      dispatch(surface, makeEvent('pointerdown', video, { pointerType: 'touch', clientX: 400, clientY: 260, timeStamp: at, ...init }));
      dispatch(surface, makeEvent('pointerup', video, { pointerType: 'touch', clientX: 400, clientY: 260, timeStamp: at + 60, ...init }));
    },
    restore() {
      if (originalDocument === undefined) Reflect.deleteProperty(globalThis, 'document');
      else Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
      if (originalRaf !== undefined) {
        Object.defineProperty(globalThis, 'requestAnimationFrame', { configurable: true, value: originalRaf });
      }
    },
  };
  return h;
}

function close(actual: number, expected: number, message = '') {
  assert.ok(Math.abs(actual - expected) < 1e-6, `${message} ${actual} vs ${expected}`);
}

test('Ctrl/Cmd + wheel zooms a share around the cursor and shows the reset chip', () => {
  const h = setup();
  try {
    const wheel = h.wheel({ ctrlKey: true, deltaY: -40, clientX: 600, clientY: 200 });
    assert.equal(wheel.defaultPrevented, true, 'the page must not zoom instead');
    close(h.scale(), Math.exp(0.4));
    assert.equal(h.tile.classList.contains('is-share-zoomed'), true);
    assert.match(h.tile.styles.get('--share-zoom-transform') ?? '', /^translate\(-?[\d.]+px, -?[\d.]+px\) scale\([\d.]+\)$/);
    assert.match(h.tile.styles.get('--share-zoom-clip') ?? '', /^inset\(/);
    assert.equal(h.tile.styles.get('--share-zoom-media-clip'), 'inset(44px 0px 0px 0px)');
    assert.ok(h.repositions() > 0, 'telepointers and drawings are re-placed');
    const chip = h.chip();
    assert.ok(chip && !chip.hidden);
    assert.match(chip.attributes.get('aria-label') ?? '', /^Zoomed to 1\.5×\. Reset to fit$/);

    // Cmd works the same (macOS), and a mouse notch is capped.
    h.wheel({ metaKey: true, deltaY: -500, clientX: 600, clientY: 200 });
    close(h.scale(), Math.exp(0.8));

    // The chip resets to fit.
    dispatch(h.surface, makeEvent('click', chip));
    assert.equal(h.scale(), 1);
    assert.equal(h.tile.classList.contains('is-share-zoomed'), false);
    assert.equal(h.tile.styles.has('--share-zoom-transform'), false);
    assert.equal(chip.hidden, true);
  } finally {
    h.restore();
  }
});

test('wheel deltas in lines and pages are converted before zooming or panning', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -1, deltaMode: 1 }); // 1 line = 16px
    close(h.scale(), Math.exp(0.16));
    h.wheel({ ctrlKey: true, deltaY: -1, deltaMode: 2 }); // 1 page = 320px, capped at 40
    close(h.scale(), Math.exp(0.16 + 0.4));
    const before = h.translate();
    h.wheel({ deltaX: 1, deltaMode: 1 }); // pans 16px left
    close(h.translate().x, before.x - 16);
  } finally {
    h.restore();
  }
});

test('a plain wheel pans only while zoomed; otherwise it scrolls the page as before', () => {
  const h = setup();
  try {
    const unzoomed = h.wheel({ deltaY: 30 });
    assert.equal(unzoomed.defaultPrevented, false);
    h.wheel({ ctrlKey: true, deltaY: -40 });
    const before = h.tile.styles.get('--share-zoom-transform');
    const pan = h.wheel({ deltaY: 30 });
    assert.equal(pan.defaultPrevented, true);
    assert.notEqual(h.tile.styles.get('--share-zoom-transform'), before);
  } finally {
    h.restore();
  }
});

test('double-tap toggles fit and fill, and its second click does not pin again', () => {
  const h = setup();
  try {
    let pins = 0;
    const click = () => dispatch(h.surface, makeEvent('click', h.video), () => (pins += 1));
    h.tap(1000);
    click();
    assert.equal(h.scale(), 1, 'one tap is a click (pin), not a zoom');
    assert.equal(pins, 1);
    h.tap(1200);
    click();
    close(h.scale(), 4 / 3, 'fill: the 4:3 window widens to the 16:9 box');
    assert.equal(pins, 1, 'the zooming tap is swallowed, not a second pin');
    assert.equal(h.tile.classList.contains('is-share-zoom-easing'), true, 'a double-tap eases');
    h.tap(3000, { pointerType: 'mouse' });
    h.tap(3200, { pointerType: 'mouse' });
    assert.equal(h.scale(), 1, 'a double-click goes back to fit');
    // Too slow is two single taps.
    h.tap(5000);
    h.tap(5600);
    assert.equal(h.scale(), 1);
    // Too far apart is two single taps.
    h.tap(7000);
    h.tap(7150, { clientX: 440 });
    assert.equal(h.scale(), 1);
  } finally {
    h.restore();
  }
});

test('a double-tap during the spotlight reflow fills around the picture point under the finger', () => {
  const h = setup();
  try {
    // The first tap pinned the tile; mid-FLIP the tile paints at half size,
    // offset -- the same tap lands on a different screen point than at rest.
    const flipScale = 0.5;
    const flip = { x: 100, y: 50 };
    h.video.getBoundingClientRect = () => {
      const match = /translate\(([-\d.]+)px, ([-\d.]+)px\) scale\(([\d.]+)\)/.exec(
        h.tile.styles.get('--share-zoom-transform') ?? ''
      );
      const [x, y, s] = match ? [Number(match[1]), Number(match[2]), Number(match[3])] : [0, 0, 1];
      const left = flip.x + flipScale * x;
      const top = flip.y + flipScale * (44 + y);
      return { left, top, width: 800 * s * flipScale, height: 450 * s * flipScale, right: 0, bottom: 0 };
    };
    // Box point (600, 200) of the final layout, as painted mid-FLIP.
    const screen = { clientX: flip.x + flipScale * 600, clientY: flip.y + flipScale * (44 + 200) };
    const pictureBefore = sharePointToPicture({ width: 800, height: 450 }, { width: 1600, height: 1200 }, SHARE_ZOOM_FIT, {
      x: 600,
      y: 200,
    })!;
    h.tap(1000, screen);
    h.tap(1150, screen);
    close(h.scale(), 4 / 3);
    const after = sharePointToPicture(
      { width: 800, height: 450 },
      { width: 1600, height: 1200 },
      h.zoom.shareZoomFor(h.tile as unknown as HTMLElement),
      { x: 600, y: 200 }
    )!;
    // At fill the 4:3 picture spans the box's width exactly (x is pinned);
    // vertically it overflows, and the tapped row stays under the finger.
    close(after.y, pictureBefore.y, 'y');
    // Mutation guard: had the mid-FLIP transform not been undone, the anchor
    // would have been taken at box point (400, 128) instead.
    assert.ok(Math.abs(pictureBefore.y - 128 / 450) > 0.1);
  } finally {
    h.restore();
  }
});

test('a tall window double-taps to 2.5x first, then on to fill, then back to fit', () => {
  // A 600x1300 portrait window in the 800x450 box: fill is ~3.85x.
  const h = setup({ media: { width: 600, height: 1300 } });
  try {
    const fill = 800 / ((450 * 600) / 1300);
    h.tap(1000);
    h.tap(1150);
    close(h.scale(), 2.5);
    h.tap(3000);
    h.tap(3150);
    close(h.scale(), fill);
    h.tap(5000);
    h.tap(5150);
    assert.equal(h.scale(), 1);
  } finally {
    h.restore();
  }
});

test('dragging a zoomed share pans it, and the drag does not also pin the tile', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40 });
    h.wheel({ ctrlKey: true, deltaY: -40 });
    const before = h.tile.styles.get('--share-zoom-transform') ?? '';
    const down = h.pointer('pointerdown', { clientX: 400, clientY: 260 });
    assert.equal(down.defaultPrevented, true);
    h.pointer('pointermove', { clientX: 380, clientY: 250 });
    h.pointer('pointermove', { clientX: 340, clientY: 240 });
    assert.equal(h.tile.classList.contains('is-share-panning'), true);
    h.pointer('pointerup', { clientX: 340, clientY: 240 });
    assert.equal(h.tile.classList.contains('is-share-panning'), false);
    const after = h.tile.styles.get('--share-zoom-transform') ?? '';
    assert.notEqual(after, before);
    const x = (value: string) => Number(/translate\(([-\d.]+)px/.exec(value)?.[1]);
    assert.ok(Math.abs(x(after) - x(before) + 60) < 0.02, 'the picture follows the pointer');

    let pinned = 0;
    const click = dispatch(h.surface, makeEvent('click', h.video), () => (pinned += 1));
    assert.equal(click.propagationStopped, true);
    assert.equal(pinned, 0);
    // The NEXT plain click pins as usual.
    h.pointer('pointerdown', { clientX: 300, clientY: 200, timeStamp: 10_000 });
    h.pointer('pointerup', { clientX: 300, clientY: 200, timeStamp: 10_050 });
    dispatch(h.surface, makeEvent('click', h.video), () => (pinned += 1));
    assert.equal(pinned, 1);
  } finally {
    h.restore();
  }
});

test('a pointercancel mid-pan ends the gesture cleanly', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40 });
    h.pointer('pointerdown', { pointerType: 'touch', pointerId: 4, clientX: 400, clientY: 260 });
    h.pointer('pointermove', { pointerType: 'touch', pointerId: 4, clientX: 360, clientY: 260 });
    assert.equal(h.tile.classList.contains('is-share-panning'), true);
    h.pointer('pointercancel', { pointerType: 'touch', pointerId: 4, clientX: 360, clientY: 260 });
    assert.equal(h.tile.classList.contains('is-share-panning'), false);
    // A fresh finger starts a fresh press: a wiggle inside the tap slop does
    // not pan from the cancelled one.
    const before = h.translate();
    h.pointer('pointerdown', { pointerType: 'touch', pointerId: 5, clientX: 100, clientY: 100 });
    h.pointer('pointermove', { pointerType: 'touch', pointerId: 5, clientX: 103, clientY: 101 });
    assert.deepEqual(h.translate(), before);
  } finally {
    h.restore();
  }
});

test('after a touch pan (which fires no click) the reset chip still works', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40 });
    const touch = (type: string, clientX: number) =>
      h.pointer(type, { pointerType: 'touch', pointerId: 7, clientX, clientY: 260 });
    touch('pointerdown', 400);
    touch('pointermove', 360);
    touch('pointerup', 360);
    const chip = h.chip()!;
    // The chip stops its own pointerdown from bubbling (it is not a pan).
    dispatch(h.surface, makeEvent('pointerdown', chip, { pointerType: 'touch', pointerId: 8 }));
    dispatch(h.surface, makeEvent('click', chip));
    assert.equal(h.scale(), 1);
  } finally {
    h.restore();
  }
});

test('a mouse release the surface never saw does not haunt the next press', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40 });
    h.wheel({ ctrlKey: true, deltaY: -40 });
    // Pressed far to the left, released outside the tile surface (no pointerup here).
    h.pointer('pointerdown', { clientX: 100, clientY: 260 });
    const before = h.tile.styles.get('--share-zoom-transform') ?? '';
    // A fresh press: its first small move is a tap-slop wiggle, not a pan
    // measured from the stale press 300px away.
    h.pointer('pointerdown', { clientX: 400, clientY: 260 });
    h.pointer('pointermove', { clientX: 402, clientY: 261 });
    assert.equal(h.tile.styles.get('--share-zoom-transform'), before);
    assert.equal(h.tile.classList.contains('is-share-panning'), false);
  } finally {
    h.restore();
  }
});

test('a two-finger pinch zooms by the finger spread; lifting one finger keeps panning; no click pins', () => {
  const h = setup();
  try {
    const touch = (type: string, pointerId: number, clientX: number, clientY: number) =>
      h.pointer(type, { pointerType: 'touch', pointerId, clientX, clientY });
    touch('pointerdown', 1, 350, 250);
    touch('pointerdown', 2, 450, 250);
    const move = touch('pointermove', 2, 550, 250);
    assert.equal(move.defaultPrevented, true);
    // Spread 100px -> 200px, but the midpoint moved 50px right too.
    close(h.scale(), 2);
    // Lift finger 2: finger 1 carries on as a pan from where it is.
    touch('pointerup', 2, 550, 250);
    const before = h.translate();
    touch('pointermove', 1, 330, 240);
    close(h.scale(), 2);
    close(h.translate().x, before.x - 20);
    touch('pointerup', 1, 330, 240);
    let pinned = 0;
    dispatch(h.surface, makeEvent('click', h.video), () => (pinned += 1));
    assert.equal(pinned, 0, 'a pinch is not a click');
  } finally {
    h.restore();
  }
});

test('a third finger landing or lifting re-baselines the pinch instead of jumping', () => {
  const h = setup();
  try {
    const touch = (type: string, pointerId: number, clientX: number, clientY: number) =>
      h.pointer(type, { pointerType: 'touch', pointerId, clientX, clientY });
    touch('pointerdown', 1, 350, 250);
    touch('pointerdown', 2, 450, 250);
    touch('pointermove', 2, 550, 250);
    close(h.scale(), 2);
    touch('pointerdown', 3, 300, 400);
    touch('pointerup', 1, 350, 250);
    // Fingers 2 and 3 are now 250px apart; a 1px nudge must not rescale by
    // the ratio against the OLD pair's 200px.
    touch('pointermove', 3, 301, 400);
    assert.ok(Math.abs(h.scale() - 2) < 0.02, `scale ${h.scale()}`);
  } finally {
    h.restore();
  }
});

test('Draw mode, Control mode and rail thumbnails keep every gesture for themselves', () => {
  for (const variant of ['draw', 'control-class', 'control-active', 'thumbnail'] as const) {
    const h = setup({ activeRemoteControl: variant === 'control-active' });
    try {
      if (variant === 'draw') h.surface.classList.add('draw-mode-active');
      if (variant === 'control-class') h.tile.classList.add('remote-control-active');
      if (variant === 'thumbnail') h.tile.classList.add('is-spotlight-thumbnail');
      const wheel = h.wheel({ ctrlKey: true, deltaY: -40 });
      assert.equal(wheel.defaultPrevented, false, `${variant}: Ctrl+wheel reaches the remote window`);
      h.tap(100);
      h.tap(250);
      assert.equal(h.scale(), 1, `${variant}: no zoom`);
      const key = dispatch(h.surface, makeEvent('keydown', h.tile, { key: '+' }));
      assert.equal(key.defaultPrevented, false, `${variant}: keys reach the remote window`);
      assert.equal(h.scale(), 1);
    } finally {
      h.restore();
    }
  }
});

test('the chip survives into Control and Draw, resets there, and never leaks input', () => {
  for (const variant of ['draw', 'control'] as const) {
    const h = setup();
    try {
      h.wheel({ ctrlKey: true, deltaY: -40 });
      if (variant === 'draw') h.surface.classList.add('draw-mode-active');
      else h.tile.classList.add('remote-control-active');
      const chip = h.chip()!;
      assert.equal(chip.hidden, false);
      for (const type of ['pointerdown', 'pointermove', 'pointerup', 'wheel', 'keydown', 'keyup']) {
        const event = makeEvent(type, chip);
        for (const { listener } of chip.listeners.get(type) ?? []) listener(event);
        assert.equal(event.propagationStopped, true, `${variant}: ${type} stops at the chip`);
      }
      dispatch(h.surface, makeEvent('click', chip));
      assert.equal(h.scale(), 1, `${variant}: the chip resets`);
    } finally {
      h.restore();
    }
  }
});

test('gestures on the header, its menu and other controls are not zoom gestures', () => {
  const h = setup();
  try {
    const wheel = dispatch(h.surface, makeEvent('wheel', h.header, { ctrlKey: true, deltaY: -40 }));
    assert.equal(wheel.defaultPrevented, false);
    assert.equal(h.scale(), 1);
  } finally {
    h.restore();
  }
});

test('keyboard on a focused share tile: + / = zoom in, - zooms out, 0 fits', () => {
  const h = setup();
  try {
    const key = (k: string, init: Partial<FakeEvent> = {}, target = h.tile) =>
      dispatch(h.surface, makeEvent('keydown', target, { key: k, ...init }));
    assert.equal(key('+').defaultPrevented, true);
    close(h.scale(), 1.25);
    key('=');
    close(h.scale(), 1.5625);
    key('-');
    close(h.scale(), 1.25);
    // Browser zoom shortcuts and keys typed elsewhere in the tile pass through.
    assert.equal(key('+', { ctrlKey: true }).defaultPrevented, false);
    assert.equal(key('+', {}, h.header).defaultPrevented, false);
    close(h.scale(), 1.25);
    key('0');
    assert.equal(h.scale(), 1);
  } finally {
    h.restore();
  }
});

test('a press on a View-mode share focuses it, so + / - / 0 work without Tab', () => {
  const h = setup();
  try {
    // Zoomed, a press is preventDefault-ed, which cancels the browser's own
    // focus-on-press: the tile has to take focus itself.
    h.wheel({ ctrlKey: true, deltaY: -40 });
    h.wheel({ ctrlKey: true, deltaY: -40 });
    const down = h.pointer('pointerdown', { clientX: 400, clientY: 260 });
    assert.equal(down.defaultPrevented, true);
    h.pointer('pointerup', { clientX: 400, clientY: 260 });
    assert.equal(h.focused(), h.tile, 'the pressed share tile has focus');
    assert.deepEqual(h.tile.focusOptions, [{ preventScroll: true }]);
    // Keys go to the focused element.
    dispatch(h.surface, makeEvent('keydown', h.focused(), { key: '0' }));
    assert.equal(h.scale(), 1);
    dispatch(h.surface, makeEvent('keydown', h.focused(), { key: '+' }));
    close(h.scale(), 1.25);
  } finally {
    h.restore();
  }
});

test('a share press never takes focus from a text field, the header, or Control and Draw', () => {
  const h = setup();
  try {
    // Typing in the chat: panning the share keeps the draft focused.
    const chatInput = h.body.appendChild(new FakeElement('input'));
    chatInput.focus();
    h.wheel({ ctrlKey: true, deltaY: -40 });
    h.pointer('pointerdown', { clientX: 400, clientY: 260 });
    h.pointer('pointerup', { clientX: 400, clientY: 260 });
    assert.equal(h.focused(), chatInput);
    h.body.focus();
    dispatch(h.surface, makeEvent('pointerdown', h.header, { clientX: 400, clientY: 20 }));
    assert.equal(h.focused(), h.body, 'the header owns its own presses');
    for (const mode of ['draw', 'control'] as const) {
      if (mode === 'draw') h.surface.classList.add('draw-mode-active');
      else h.tile.classList.add('remote-control-active');
      h.pointer('pointerdown', { clientX: 400, clientY: 260, timeStamp: 5000 });
      h.pointer('pointerup', { clientX: 400, clientY: 260, timeStamp: 5050 });
      assert.equal(h.focused(), h.body, `${mode}: not the zoom controller's press`);
      h.surface.classList.remove('draw-mode-active');
      h.tile.classList.remove('remote-control-active');
    }
  } finally {
    h.restore();
  }
});

test('wheel and touchmove are non-passive on share tiles only, never on the whole surface', () => {
  const h = setup();
  try {
    // Bound once however often tiles.ts re-binds a reused share tile.
    h.zoom.bindTile(h.tile as unknown as HTMLElement);
    for (const type of ['wheel', 'touchmove']) {
      assert.deepEqual(
        (h.tile.listeners.get(type) ?? []).map((entry) => entry.passive),
        [false],
        `${type}: one non-passive listener on the share tile`
      );
      assert.deepEqual(
        (h.surface.listeners.get(type) ?? []).filter((entry) => entry.passive !== true),
        [],
        `${type}: the surface's camera tiles and rail keep compositor scrolling`
      );
    }
    // Every share tile gets them, wherever tiles.ts creates or reuses one.
    const tilesSource = readFileSync(new URL('../src/tiles.ts', import.meta.url), 'utf8');
    assert.match(tilesSource, /cb\.bindTileInteractions\(tile\);\n\s*cb\.bindShareZoomTile\?\.\(tile\);/);
    const mainSource = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');
    assert.match(mainSource, /bindShareZoomTile: shareZoom\.bindTile/);
  } finally {
    h.restore();
  }
});

test('the header menu command zooms in, out and back to fit', () => {
  const h = setup();
  try {
    h.zoom.command(h.tile as unknown as HTMLElement, 'in');
    close(h.scale(), 1.25);
    h.zoom.command(h.tile as unknown as HTMLElement, 'in');
    h.zoom.command(h.tile as unknown as HTMLElement, 'out');
    close(h.scale(), 1.25);
    h.zoom.command(h.tile as unknown as HTMLElement, 'fit');
    assert.equal(h.scale(), 1);
  } finally {
    h.restore();
  }
});

test("Safari's gesture events zoom a View-mode share and never the page", () => {
  const h = setup();
  try {
    const start = dispatch(h.surface, makeEvent('gesturestart', h.video, { clientX: 400, clientY: 269 }));
    assert.equal(start.defaultPrevented, true);
    const change = dispatch(h.surface, makeEvent('gesturechange', h.video, { scale: 2, clientX: 400, clientY: 269 }));
    assert.equal(change.defaultPrevented, true);
    close(h.scale(), 2);
    const end = dispatch(h.surface, makeEvent('gestureend', h.video, { scale: 2 }));
    assert.equal(end.defaultPrevented, true);
    assert.deepEqual(h.demands.at(-1), { scale: '2.0000', painted: '2.0000' });
    // iOS: a gesturechange with no gesturestart seen is still refused.
    const stray = dispatch(h.surface, makeEvent('gesturechange', h.video, { scale: 3 }));
    assert.equal(stray.defaultPrevented, true);
    close(h.scale(), 2);
    // And a two-finger touchmove over the share never scrolls/zooms the page.
    assert.equal(dispatch(h.surface, makeEvent('touchmove', h.video, { touches: [{}, {}] })).defaultPrevented, true);
    assert.equal(dispatch(h.surface, makeEvent('touchmove', h.video, { touches: [{}] })).defaultPrevented, false);
  } finally {
    h.restore();
  }
});

test('a new window size re-derives the zoom and keeps it inside the picture', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40, clientX: 780, clientY: 440 });
    h.wheel({ ctrlKey: true, deltaY: -40, clientX: 780, clientY: 440 });
    const before = h.tile.styles.get('--share-zoom-transform');
    // The shared window is resized to 16:9: the same zoom now spans a wider
    // picture, so the transform is recomputed (and re-clamped).
    h.video.videoWidth = 1920;
    h.video.videoHeight = 1080;
    h.video.fire('resize');
    assert.notEqual(h.tile.styles.get('--share-zoom-transform'), before);
    const { x, y } = h.translate();
    const s = h.scale();
    assert.ok(x <= 0 && x + 800 * s >= 800 - 1e-6, 'no black at the sides');
    assert.ok(y <= 0 && y + 450 * s >= 450 - 1e-6, 'no black top or bottom');
  } finally {
    h.restore();
  }
});

test('viewer demand is committed when a gesture ends, never per pinch frame', () => {
  mock.timers.enable({ apis: ['setTimeout'] });
  const h = setup();
  try {
    const touch = (type: string, pointerId: number, clientX: number) =>
      h.pointer(type, { pointerType: 'touch', pointerId, clientX, clientY: 250 });
    touch('pointerdown', 1, 350);
    touch('pointerdown', 2, 450);
    touch('pointermove', 2, 500);
    touch('pointermove', 2, 550);
    assert.equal(h.demands.length, 0, 'nothing published mid-pinch');
    assert.equal(h.tile.dataset.shareZoomPaintedScale, '2.0000');
    assert.equal(h.tile.dataset.shareZoomDemandScale, undefined);
    touch('pointerup', 2, 550);
    touch('pointerup', 1, 350);
    assert.deepEqual(h.demands, [{ scale: '2.0000', painted: '2.0000' }]);

    // A pan keeps the scale: nothing new to ask for.
    touch('pointerdown', 3, 400);
    touch('pointermove', 3, 360);
    touch('pointerup', 3, 360);
    assert.equal(h.demands.length, 1);

    // A wheel zoom commits once it settles.
    h.wheel({ ctrlKey: true, deltaY: 20 });
    h.wheel({ ctrlKey: true, deltaY: 20 });
    assert.equal(h.demands.length, 1);
    mock.timers.tick(260);
    assert.equal(h.demands.length, 2);
    assert.equal(h.demands[1]!.scale, h.tile.dataset.shareZoomPaintedScale);

    // Back to fit: the demand scale is dropped and published.
    h.zoom.command(h.tile as unknown as HTMLElement, 'fit');
    assert.deepEqual(h.demands.at(-1), { scale: undefined, painted: undefined });
  } finally {
    h.restore();
    mock.timers.reset();
  }
});

test('only discrete changes ease; reduced motion never does', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40 });
    assert.equal(h.tile.classList.contains('is-share-zoom-easing'), false, 'a wheel zoom tracks exactly');
    h.zoom.command(h.tile as unknown as HTMLElement, 'in');
    assert.equal(h.tile.classList.contains('is-share-zoom-easing'), true);
    // A gesture starting mid-ease snaps to it first.
    h.pointer('pointerdown', { clientX: 400, clientY: 260 });
    assert.equal(h.tile.classList.contains('is-share-zoom-easing'), false);
    h.pointer('pointerup', { clientX: 400, clientY: 260 });
  } finally {
    h.restore();
  }
  const originalWindow = (globalThis as { window?: unknown }).window;
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { matchMedia: () => ({ matches: true }) },
  });
  const reduced = setup();
  try {
    reduced.zoom.command(reduced.tile as unknown as HTMLElement, 'in');
    assert.equal(reduced.tile.classList.contains('is-share-zoom-easing'), false);
  } finally {
    reduced.restore();
    if (originalWindow === undefined) delete (globalThis as { window?: unknown }).window;
    else Object.defineProperty(globalThis, 'window', { configurable: true, value: originalWindow });
  }
});

test('the chip is only rewritten when its label changes', () => {
  const h = setup();
  try {
    h.wheel({ ctrlKey: true, deltaY: -40 });
    const chip = h.chip()!;
    const writes = chip.attributeWrites;
    h.wheel({ deltaY: 20 }); // a pan: same zoom, same label
    h.wheel({ deltaX: 20 });
    assert.equal(chip.attributeWrites, writes);
  } finally {
    h.restore();
  }
});

// --- CSS contract --------------------------------------------------------

const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

function rule(selector: RegExp): string {
  return new RegExp(`${selector.source}\\s*\\{(?<body>[^}]+)\\}`).exec(css)?.groups?.body ?? '';
}

test('CSS: the zoom transform reaches every media layer of a share, never a thumbnail', () => {
  for (const layer of ['video', 'canvas\\.full-range-canvas', 'canvas\\.share-hold-canvas']) {
    assert.match(
      css,
      new RegExp(`\\.tile\\.share-tile\\.is-share-zoomed:not\\(\\.is-spotlight-thumbnail\\) ${layer}[,\\s]`),
      `${layer} is transformed`
    );
  }
  const body = rule(/\.tile\.share-tile\.is-share-zoomed:not\(\.is-spotlight-thumbnail\) canvas\.share-hold-canvas/);
  assert.match(body, /transform:\s*var\(--share-zoom-transform\)/);
  assert.match(body, /transform-origin:\s*0 0/);
  assert.match(body, /clip-path:\s*var\(--share-zoom-clip\)/);
});

test('CSS: while zoomed, overlays are clipped to the video box and off-screen anchors hidden', () => {
  assert.match(
    rule(/\.tile\.share-tile\.is-share-zoomed:not\(\.is-spotlight-thumbnail\) \.remote-draw-layer/),
    /clip-path:\s*var\(--share-zoom-media-clip\)/
  );
  assert.match(css, /\.tile\.share-tile\.is-share-zoomed:not\(\.is-spotlight-thumbnail\) \.telepointer-layer,/);
  assert.match(rule(/\.remote-draw-text\.is-outside-media/), /visibility:\s*hidden/);
  assert.match(css, /\.remote-telepointer\.is-outside-media,/);
});

test('CSS: touch-action is taken only on View-mode share tiles', () => {
  const body = rule(
    /\.tiles:not\(\.draw-mode-active\) \.tile\.share-tile:not\(\.remote-control-active\):not\(\.is-spotlight-thumbnail\)/
  );
  assert.match(body, /touch-action:\s*none/);
  const touchRules = [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/(?<selector>[^{}]+)\{[^}]*touch-action\s*:[^}]*\}/g)]
    .map((match) => (match.groups?.selector ?? '').trim())
    .filter((selector) => /\.tile\b/.test(selector));
  for (const selector of touchRules) {
    assert.ok(/share-tile|draw-mode-active/.test(selector), `unexpected tile touch-action: ${selector}`);
  }
});

test('CSS: the chip hides on thumbnails only, and has a finger-sized target', () => {
  assert.match(rule(/\.tile\.is-spotlight-thumbnail \.share-zoom-chip/), /display:\s*none/);
  assert.doesNotMatch(css, /\.tiles\.draw-mode-active \.share-zoom-chip/);
  assert.doesNotMatch(css, /\.tile\.remote-control-active \.share-zoom-chip/);
  const coarse = /@media \(pointer: coarse\)\s*\{\s*\.share-zoom-chip::after\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';
  assert.match(coarse, /inset:\s*-12px -8px/, 'a ~21px chip grows to a ~45px target');
});

test('CSS: discrete zoom changes ease, not under reduced motion', () => {
  assert.match(rule(/\.tile\.share-tile\.is-share-zoom-easing canvas\.share-hold-canvas/), /transition:[\s\S]*transform 180ms/);
  assert.match(
    css,
    /@media \(prefers-reduced-motion: reduce\)\s*\{\s*\.tile\.share-tile\.is-share-zoom-easing video,[\s\S]*?transition:\s*none/
  );
  assert.match(css, /@media \(pointer: fine\)\s*\{[\s\S]*?cursor:\s*zoom-in/);
});
