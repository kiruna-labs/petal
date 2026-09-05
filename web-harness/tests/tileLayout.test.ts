import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { HarnessContext } from '../src/context.ts';
import { TILE_REFLOW_ANIMATION_MS } from '../src/tileReflow.ts';
import { setupTileLayout } from '../src/tileLayout.ts';

type Listener = (event: Event) => void;

class FakeClassList {
  private readonly element: FakeElement;

  constructor(element: FakeElement) {
    this.element = element;
  }

  private values(): string[] {
    return this.element.className.split(/\s+/).filter(Boolean);
  }

  contains(name: string): boolean {
    return this.values().includes(name);
  }

  add(name: string) {
    this.element.className = Array.from(new Set([...this.values(), name])).join(' ');
  }

  remove(name: string) {
    this.element.className = this.values().filter((value) => value !== name).join(' ');
  }

  toggle(name: string, force?: boolean): boolean {
    const next = force ?? !this.contains(name);
    if (next) this.add(name);
    else this.remove(name);
    return next;
  }
}

class FakeStyle {
  readonly values = new Map<string, string>();

  setProperty(name: string, value: string) {
    this.values.set(name, value);
  }
}

class FakeElement {
  id = '';
  className = '';
  title = '';
  dataset: Record<string, string | undefined> = {};
  parentElement: FakeElement | null = null;
  readonly children: FakeElement[] = [];
  readonly classList = new FakeClassList(this);
  readonly style = new FakeStyle();
  readonly listeners = new Map<string, Listener[]>();
  readonly attributes = new Map<string, string>();
  readonly tagName: string;

  constructor(tagName: string) {
    this.tagName = tagName;
  }

  appendChild<T extends FakeElement>(child: T): T {
    child.remove();
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  prepend<T extends FakeElement>(child: T): T {
    child.remove();
    child.parentElement = this;
    this.children.unshift(child);
    return child;
  }

  remove() {
    if (!this.parentElement) return;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    if (index >= 0) siblings.splice(index, 1);
    this.parentElement = null;
  }

  contains(element: FakeElement): boolean {
    return element === this || this.children.some((child) => child.contains(element));
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }

  addEventListener(type: string, listener: Listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  click() {
    const event = {
      currentTarget: this,
      target: this,
    } as unknown as Event;
    for (const listener of this.listeners.get('click') ?? []) listener(event);
  }

  closest(): FakeElement | null {
    return null;
  }

  querySelector<T extends Element>(selector: string): T | null {
    return (this.querySelectorAll(selector)[0] as T | undefined) ?? null;
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    const matches: FakeElement[] = [];
    for (const child of this.children) {
      if (matchesSelector(child, selector)) matches.push(child);
      matches.push(...(child.querySelectorAll(selector) as unknown as FakeElement[]));
    }
    return matches as unknown as T[];
  }
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  if (selector === '.tile') return element.classList.contains('tile');
  if (selector === '.share-tile') return element.classList.contains('share-tile');
  if (selector === 'video') return element.tagName === 'video';
  if (selector === '.tile video') {
    return element.tagName === 'video' && element.parentElement?.classList.contains('tile') === true;
  }
  return false;
}

class FakeDocument {
  readonly root = new FakeElement('body');

  createElement(tagName: string): FakeElement {
    return new FakeElement(tagName.toLowerCase());
  }

  getElementById(id: string): FakeElement | null {
    return this.find(this.root, (element) => element.id === id);
  }

  private find(element: FakeElement, predicate: (candidate: FakeElement) => boolean): FakeElement | null {
    if (predicate(element)) return element;
    for (const child of element.children) {
      const match = this.find(child, predicate);
      if (match) return match;
    }
    return null;
  }
}

function installFakeDom() {
  const originalDocument = globalThis.document;
  const originalLocalStorage = globalThis.localStorage;
  const document = new FakeDocument();
  const storage = new Map<string, string>();
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
    },
  });
  return {
    document,
    restore: () => {
      if (originalDocument === undefined) Reflect.deleteProperty(globalThis, 'document');
      else Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
      if (originalLocalStorage === undefined) Reflect.deleteProperty(globalThis, 'localStorage');
      else Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: originalLocalStorage });
    },
  };
}

function installMotionApis() {
  const originalWindow = (globalThis as { window?: unknown }).window;
  const originalRaf = (globalThis as { requestAnimationFrame?: unknown }).requestAnimationFrame;
  const callbacks: FrameRequestCallback[] = [];
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { matchMedia: () => ({ matches: false }) },
  });
  Object.defineProperty(globalThis, 'requestAnimationFrame', {
    configurable: true,
    value: (callback: FrameRequestCallback) => {
      callbacks.push(callback);
      return callbacks.length;
    },
  });
  return {
    flush() {
      const pending = callbacks.splice(0);
      pending.forEach((callback) => callback(0));
    },
    setReducedMotion(reduced: boolean) {
      Object.defineProperty(globalThis, 'window', {
        configurable: true,
        value: { matchMedia: () => ({ matches: reduced }) },
      });
    },
    restore() {
      if (originalWindow === undefined) delete (globalThis as { window?: unknown }).window;
      else Object.defineProperty(globalThis, 'window', { configurable: true, value: originalWindow });
      if (originalRaf === undefined) delete (globalThis as { requestAnimationFrame?: unknown }).requestAnimationFrame;
      else Object.defineProperty(globalThis, 'requestAnimationFrame', { configurable: true, value: originalRaf });
    },
  };
}

test('spotlight mode keeps one hero and moves every other tile into the top strip', () => {
  const fakeDom = installFakeDom();
  try {
    const tilesEl = fakeDom.document.createElement('div');
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);

    const camera = fakeDom.document.createElement('div');
    camera.id = 'camera';
    camera.className = 'tile';
    camera.dataset.owner = 'Ada Lovelace';
    const share = fakeDom.document.createElement('div');
    share.id = 'share';
    share.className = 'tile share-tile';
    share.dataset.owner = 'Grace Hopper';
    const drawOverlay = fakeDom.document.createElement('svg');
    share.appendChild(drawOverlay);
    tilesEl.appendChild(camera);
    tilesEl.appendChild(share);

    const fitted: string[] = [];
    const logs: string[] = [];
    const state = {
      tileLayoutMode: 'grid',
      pinnedTileId: null,
      layoutModeButtons: null,
      speakerSmoothingTimer: null,
    };
    const ctx = {
      dom: { tilesEl, topbarRight },
      state,
      ui: { logEvent: (message: string) => logs.push(message) },
      cb: {
        activeRemoteControlForTile: () => null,
        fitTileLabels: (tile: HTMLDivElement) => fitted.push(tile.id),
      },
      speakerScores: new Map(),
      activeSpeakerTargets: new Set(),
    } as unknown as HarnessContext;
    const layout = setupTileLayout(ctx);
    layout.bindTileInteractions(camera as unknown as HTMLDivElement);
    layout.bindTileInteractions(share as unknown as HTMLDivElement);

    layout.pinTile(share as unknown as HTMLDivElement, 'auto');

    const strip = tilesEl.children[0]!;
    assert.equal(strip.classList.contains('spotlight-strip'), true);
    assert.deepEqual(strip.children, [camera]);
    assert.equal(tilesEl.children[1], share);
    assert.equal(camera.classList.contains('is-spotlight-thumbnail'), true);
    assert.equal(share.classList.contains('is-spotlight'), true);
    assert.equal(fitted.includes('camera'), true);
    assert.equal(drawOverlay.parentElement, share);
    assert.deepEqual(logs, []);

    camera.click();

    assert.deepEqual(strip.children, [share]);
    assert.equal(tilesEl.children[1], camera);
    assert.equal(camera.classList.contains('is-spotlight'), true);
    assert.equal(share.classList.contains('is-spotlight-thumbnail'), true);
    assert.equal(drawOverlay.parentElement, share);
    assert.deepEqual(logs, ['spotlight pinned: Ada Lovelace']);

    state.tileLayoutMode = 'grid';
    layout.applyTileLayout();

    assert.deepEqual(tilesEl.children, [camera, share]);
    assert.equal(camera.classList.contains('is-spotlight-thumbnail'), false);
    assert.equal(share.classList.contains('is-spotlight-thumbnail'), false);
    assert.equal(strip.parentElement, null);

    layout.pinTile(share as unknown as HTMLDivElement, 'auto');
    const detachedStrip = tilesEl.children[0]!;
    detachedStrip.remove();
    share.remove();

    const nextCamera = fakeDom.document.createElement('div');
    nextCamera.id = 'next-camera';
    nextCamera.className = 'tile';
    const nextShare = fakeDom.document.createElement('div');
    nextShare.id = 'next-share';
    nextShare.className = 'tile share-tile';
    tilesEl.appendChild(nextCamera);
    tilesEl.appendChild(nextShare);
    layout.bindTileInteractions(nextCamera as unknown as HTMLDivElement);
    layout.bindTileInteractions(nextShare as unknown as HTMLDivElement);
    layout.pinTile(nextShare as unknown as HTMLDivElement, 'auto');

    assert.notEqual(tilesEl.children[0], detachedStrip);
    assert.deepEqual(tilesEl.children[0]?.children, [nextCamera]);
    assert.equal(camera.parentElement, detachedStrip);
  } finally {
    fakeDom.restore();
  }
});

test('grid and spotlight layout changes FLIP persistent tiles and retarget the latest request', () => {
  const fakeDom = installFakeDom();
  const motion = installMotionApis();
  try {
    const tilesEl = fakeDom.document.createElement('div');
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);

    const camera = fakeDom.document.createElement('div');
    camera.id = 'camera';
    camera.className = 'tile';
    camera.dataset.owner = 'Ada Lovelace';
    camera.appendChild(fakeDom.document.createElement('video'));
    const share = fakeDom.document.createElement('div');
    share.id = 'share';
    share.className = 'tile share-tile';
    share.dataset.owner = 'Grace Hopper';
    const drawOverlay = fakeDom.document.createElement('svg');
    share.appendChild(drawOverlay);
    tilesEl.appendChild(camera);
    tilesEl.appendChild(share);

    const gridRects = new Map<FakeElement, DOMRect>([
      [camera, { left: 0, top: 0, width: 240, height: 135 } as DOMRect],
      [share, { left: 252, top: 0, width: 240, height: 135 } as DOMRect],
    ]);
    const heroRects = new Map<FakeElement, DOMRect>([
      [camera, { left: 0, top: 0, width: 492, height: 277 } as DOMRect],
      [share, { left: 0, top: 0, width: 492, height: 277 } as DOMRect],
    ]);
    const thumbnailRects = new Map<FakeElement, DOMRect>([
      [camera, { left: 0, top: 292, width: 156, height: 88 } as DOMRect],
      [share, { left: 168, top: 292, width: 156, height: 88 } as DOMRect],
    ]);
    const animationRecords: Array<{
      tile: FakeElement;
      keyframes: Keyframe[];
      options: KeyframeAnimationOptions;
      canceled: boolean;
      cancel: () => void;
    }> = [];
    const rectFor = (tile: FakeElement) => {
      if (tile.classList.contains('is-spotlight')) return heroRects.get(tile)!;
      if (tile.classList.contains('is-spotlight-thumbnail')) return thumbnailRects.get(tile)!;
      return gridRects.get(tile)!;
    };
    for (const tile of [camera, share]) {
      Object.defineProperty(tile, 'getBoundingClientRect', {
        configurable: true,
        value: () => rectFor(tile),
      });
      Object.defineProperty(tile, 'animate', {
        configurable: true,
        value: (keyframes: Keyframe[] | PropertyIndexedKeyframes, options: KeyframeAnimationOptions) => {
          const record = {
            tile,
            keyframes: keyframes as Keyframe[],
            options,
            canceled: false,
            cancel() {
              record.canceled = true;
            },
            finished: new Promise<Animation>(() => {}),
          };
          animationRecords.push(record);
          return record as unknown as Animation;
        },
      });
    }

    const state = {
      tileLayoutMode: 'grid' as const,
      pinnedTileId: null,
      autoSpotlightRestoreMode: null,
      layoutModeButtons: null,
      speakerSmoothingTimer: null,
    };
    const ctx = {
      dom: { tilesEl, topbarRight },
      state,
      ui: { logEvent: () => {} },
      cb: {
        activeRemoteControlForTile: () => null,
        fitTileLabels: () => {},
      },
      speakerScores: new Map(),
      activeSpeakerTargets: new Set(),
    } as unknown as HarnessContext;
    const layout = setupTileLayout(ctx);
    layout.bindTileInteractions(camera as unknown as HTMLDivElement);
    layout.bindTileInteractions(share as unknown as HTMLDivElement);

    layout.pinTile(share as unknown as HTMLDivElement, 'manual');
    motion.flush();
    assert.equal(animationRecords.length, 2);
    assert.ok(animationRecords.every((record) => record.options.duration === TILE_REFLOW_ANIMATION_MS));
    assert.ok(animationRecords.every((record) => record.options.fill === 'none'));
    assert.ok(animationRecords.every((record) => String(record.keyframes[0]?.transform).includes('translate')));
    assert.equal(drawOverlay.parentElement, share);

    // A second request before the first pair finishes cancels the old handles
    // and targets the latest hero/rail arrangement rather than queueing stale
    // geometry.
    layout.pinTile(camera as unknown as HTMLDivElement, 'manual');
    motion.flush();
    assert.equal(animationRecords.length, 4);
    assert.equal(animationRecords.slice(0, 2).every((record) => record.canceled), true);
    assert.equal(animationRecords.slice(2).every((record) => !record.canceled), true);
    assert.equal(drawOverlay.parentElement, share);

    // Reduced motion changes the layout immediately and schedules no WAAPI
    // work, while the persistent tile nodes remain available.
    motion.setReducedMotion(true);
    state.tileLayoutMode = 'grid';
    layout.applyTileLayout();
    motion.flush();
    assert.equal(animationRecords.length, 4);
    assert.equal(camera.parentElement, tilesEl);
    assert.equal(share.parentElement, tilesEl);
  } finally {
    motion.restore();
    fakeDom.restore();
  }
});

test('#785 the spotlight fallback skips the local self-view and takes a remote tile instead', () => {
  // Defect 2's actual site: applyTileLayout's missing-pin branch. The local
  // camera tile is seeded first and the spotlight strip is prepended, so the
  // old "first .tile video in DOM order" fallback landed on the user's own
  // webcam every time a share ended.
  const fakeDom = installFakeDom();
  try {
    const tilesEl = fakeDom.document.createElement('div');
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);

    const selfView = fakeDom.document.createElement('div');
    selfView.id = 'self-view';
    selfView.className = 'tile';
    selfView.dataset.owner = 'me';
    selfView.appendChild(fakeDom.document.createElement('video'));
    const remote = fakeDom.document.createElement('div');
    remote.id = 'remote';
    remote.className = 'tile';
    remote.dataset.owner = 'them';
    tilesEl.appendChild(selfView);
    tilesEl.appendChild(remote);

    const state = {
      room: { localParticipant: { identity: 'me' } },
      tileLayoutMode: 'spotlight',
      pinnedTileId: null,
      layoutModeButtons: null,
      speakerSmoothingTimer: null,
    };
    const ctx = {
      dom: { tilesEl, topbarRight },
      state,
      ui: { logEvent: () => {} },
      cb: { activeRemoteControlForTile: () => null, fitTileLabels: () => {} },
      speakerScores: new Map(),
      activeSpeakerTargets: new Set(),
    } as unknown as HarnessContext;
    const layout = setupTileLayout(ctx);

    layout.applyTileLayout();

    assert.equal(state.pinnedTileId, 'remote');

    // Alone in the room, the self-view is all there is -- spotlight it rather
    // than showing nothing.
    remote.remove();
    state.pinnedTileId = null;
    layout.applyTileLayout();
    assert.equal(state.pinnedTileId, 'self-view');
  } finally {
    fakeDom.restore();
  }
});

test('small spotlight layouts reserve at least three quarters of their height for the hero', async () => {
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const narrowMedia =
    /@media\s*\(max-width:\s*760px\)\s*\{(?<body>[\s\S]+?)@media\s*\(max-width:\s*560px\)/i.exec(
      css
    )?.groups?.body ?? '';
  const narrowSpotlight =
    /\.tiles\.layout-spotlight\s*\{(?<body>[^}]+)\}/i.exec(narrowMedia)?.groups?.body ?? '';

  assert.match(narrowSpotlight, /--spotlight-strip-height\s*:\s*min\(72px,\s*16%\)/i);
  assert.match(narrowSpotlight, /--spotlight-strip-gap\s*:\s*8px/i);

  for (const availableHeight of [160, 240, 400, 640]) {
    const stripHeight = Math.min(72, availableHeight * 0.16);
    const heroShare = (availableHeight - stripHeight - 8) / availableHeight;
    assert.ok(heroShare >= 0.75, `${availableHeight}px leaves only ${heroShare * 100}% for the hero`);
  }
});

test('spotlight thumbnails scroll horizontally and keep a fixed media aspect, never a label-driven width', async () => {
  // The 2026-07-30 E1 regression: `width: max-content` sized each thumbnail
  // to its NAME CHIP (the video is absolutely positioned and contributes no
  // intrinsic width), so tile shape was driven by name length instead of the
  // media — arbitrary black bands that read as stretched/wrong-aspect tiles.
  // Tiles keep a 16:9 box; the video stays `object-fit: contain` (letterbox
  // deliberately, never distort); labels stay fully visible through
  // fitNameChipLabel's compact swap, which needs a real, measurable overflow
  // (`overflow: hidden` + bounded max-width), not `overflow: visible`.
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const strip = /\.spotlight-strip\s*\{(?<body>[^}]+)\}/i.exec(css)?.groups?.body ?? '';
  const thumbnail =
    /\.spotlight-strip\s*>\s*\.tile\.is-spotlight-thumbnail\s*\{(?<body>[^}]+)\}/i.exec(css)?.groups
      ?.body ?? '';
  const chip =
    /\.spotlight-strip\s*>\s*\.tile\.is-spotlight-thumbnail \.name-chip\s*\{(?<body>[^}]+)\}/i.exec(
      css
    )?.groups?.body ?? '';
  const label =
    /\.spotlight-strip\s*>\s*\.tile\.is-spotlight-thumbnail \.name-chip-label\s*\{(?<body>[^}]+)\}/i.exec(
      css
    )?.groups?.body ?? '';
  const thumbnailInitials =
    /\.spotlight-strip\s*>\s*\.tile\.is-spotlight-thumbnail \.initials\s*\{(?<body>[^}]+)\}/i.exec(
      css
    )?.groups?.body ?? '';

  assert.match(strip, /display\s*:\s*flex/i);
  assert.match(strip, /overflow-x\s*:\s*auto/i);
  assert.match(strip, /overflow-y\s*:\s*hidden/i);
  assert.match(thumbnail, /aspect-ratio\s*:\s*16\s*\/\s*9/i);
  assert.doesNotMatch(thumbnail, /width\s*:\s*max-content/i);
  // #894: the camera-ON chip is positioned bottom-right by flex-end on the
  // tile — must stay, it's unrelated to (and does not fight) initials centering.
  assert.match(thumbnail, /justify-content\s*:\s*flex-end/i);
  assert.match(chip, /max-width\s*:\s*calc\(100% - 8px\)/i);
  assert.match(chip, /overflow\s*:\s*hidden/i);
  assert.match(label, /min-width\s*:\s*0/i);
  assert.match(label, /overflow\s*:\s*hidden/i);
  // The share video's contain rule is what guarantees "never distort".
  assert.match(css, /\.tile video,[\s\S]{0,200}?object-fit\s*:\s*contain/i);

  // #894: a camera-off thumbnail must inherit the base `.tile .initials`
  // absolute-centering rule (style.css:1666-1684) — only font-size may be
  // overridden here. The prior static/transform-none/max-width-none override
  // packed the name to the tile's flex-end edge instead of centering it.
  assert.ok(thumbnailInitials.length > 0, 'thumbnail .initials block should exist');
  assert.doesNotMatch(thumbnailInitials, /position\s*:\s*static/i);
  assert.doesNotMatch(thumbnailInitials, /transform\s*:\s*none/i);
  assert.doesNotMatch(thumbnailInitials, /max-width\s*:\s*none/i);
  assert.match(thumbnailInitials, /font-size/i);
});
