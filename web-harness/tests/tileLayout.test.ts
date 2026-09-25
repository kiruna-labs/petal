import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { HarnessContext } from '../src/context.ts';
import { TILE_REFLOW_ANIMATION_MS } from '../src/tileReflow.ts';
import { computeSpotlightGeometry, setupTileLayout, spotlightHeroMedia } from '../src/tileLayout.ts';
import { cameraFit, coverCropFractions } from '@petal/shared/logic/cameraCrop';

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
  clientWidth = 0;
  clientHeight = 0;
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

test('spotlight mode keeps one hero and moves every other tile into the strip', () => {
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
    // #248: the inverted frame scales uniformly -- `scale(s)`, never
    // `scale(sx, sy)`, which would squash live video when a tile changes shape.
    assert.ok(
      animationRecords.every((record) => /scale\([\d.]+\)$/.test(String(record.keyframes[0]?.transform))),
      animationRecords.map((record) => String(record.keyframes[0]?.transform)).join(' | ')
    );
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

test('#239 your own camera leads the spotlight strip, ahead of tiles that joined before it', () => {
  // Remote tiles usually exist before the local one (they are in the room
  // when you join), so join order put the self-view LAST -- below the fold
  // of a side strip with seven people on a landscape phone.
  const fakeDom = installFakeDom();
  try {
    const tilesEl = fakeDom.document.createElement('div');
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);
    const make = (id: string, owner: string, share = false) => {
      const tile = fakeDom.document.createElement('div');
      tile.id = id;
      tile.className = share ? 'tile share-tile' : 'tile';
      tile.dataset.owner = owner;
      tilesEl.appendChild(tile);
      return tile;
    };
    const alice = make('alice', 'alice');
    const bob = make('bob', 'bob');
    const myShare = make('my-share', 'me', true);
    const selfView = make('self-view', 'me');
    const share = make('share', 'carol', true);

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

    assert.equal(tilesEl.children[1], share, 'the remote share is the hero');
    const strip = tilesEl.children[0]!;
    // Only the camera self-view moves up; your own share keeps its place.
    assert.deepEqual(strip.children, [selfView, alice, bob, myShare]);
  } finally {
    fakeDom.restore();
  }
});

// #239: the strip used to be a horizontal band above the hero whose height
// had no floor (min(72px, 16%)), so on a landscape phone it collapsed to
// ~24px and showed one thumbnail. Surfaces below are the tiles' content box
// the real client measures on each device (viewport minus chrome and pad).
test('#239 spotlight puts the strip beside the hero on wide surfaces and under it on tall ones', () => {
  const landscapePhone = computeSpotlightGeometry(3, 794, 348, 6); // Pixel 8, toolbar showing
  assert.equal(landscapePhone.placement, 'side');
  assert.ok(Math.abs(landscapePhone.heroHeight - 348) < 0.5, 'the hero takes the full height');
  assert.ok(landscapePhone.stripSize >= 112, 'the strip never collapses below the thumbnail floor');
  assert.ok(landscapePhone.heroWidth + 6 + landscapePhone.stripSize <= 794 + 0.5, 'hero + strip fit the width');

  for (const [label, width, height] of [
    ['desktop 1280x800', 1240, 634],
    ['desktop 1920x1080', 1880, 914],
    ['iPad mini landscape', 992, 610],
    ['iPhone SE landscape', 598, 363],
  ] as const) {
    assert.equal(computeSpotlightGeometry(3, width, height, 14).placement, 'side', label);
  }
  for (const [label, width, height] of [
    ['Pixel 8 portrait', 392, 689],
    ['iPhone SE portrait', 355, 520],
    ['iPad mini portrait', 736, 866],
  ] as const) {
    const portrait = computeSpotlightGeometry(3, width, height, 8);
    assert.equal(portrait.placement, 'below', label);
    assert.ok(Math.abs(portrait.heroWidth - width) < 0.5, `${label}: the hero takes the full width`);
    assert.ok(portrait.thumbnailWidth <= width * 0.7 + 0.5, `${label}: the hero stays the biggest tile`);
    assert.ok(portrait.heroHeight + 8 + portrait.stripSize <= height + 0.5, `${label}: hero + rows fit`);
  }
});

test('#239 under the hero, thumbnails grow into the room they have instead of staying a sliver', () => {
  // Pixel 8 portrait: three thumbnails stack one per row at 262px -- the
  // self-view is nearly twice the area of a fixed two-per-row 191px grid.
  const three = computeSpotlightGeometry(3, 392, 689, 8);
  assert.ok(three.thumbnailWidth > 250, `${three.thumbnailWidth}px`);
  assert.ok(3 * three.thumbnailHeight + 2 * 8 <= three.stripSize, 'all three fit without scrolling');
  // Six only fit two per row; the block stays compact, never a tower.
  const six = computeSpotlightGeometry(6, 392, 689, 8);
  assert.ok(six.thumbnailWidth * 2 + 8 <= 392 && six.thumbnailWidth > 180, `${six.thumbnailWidth}px`);
});

test('#239 spotlight thumbnails share one size with a floor, and a crowded strip scrolls instead of shrinking', () => {
  const few = computeSpotlightGeometry(2, 794, 348, 6);
  const many = computeSpotlightGeometry(12, 794, 348, 6);
  // One width for every thumbnail, whatever the count: the self-view is
  // never smaller than anyone else's, with or without a shared window hero.
  assert.equal(many.thumbnailWidth, few.thumbnailWidth);
  assert.ok(many.thumbnailWidth >= 110, `thumbnails keep a usable size (${many.thumbnailWidth}px)`);
  assert.ok(Math.abs(many.thumbnailHeight * (16 / 9) - many.thumbnailWidth) < 0.01, '16:9');

  // Tall surface, many thumbnails: the rows get what the hero leaves, and
  // past the floor the rest is reachable by scrolling the strip -- the
  // thumbnails never shrink below it.
  const portraitFew = computeSpotlightGeometry(2, 392, 689, 8);
  const portraitMany = computeSpotlightGeometry(20, 392, 689, 8);
  assert.ok(portraitMany.thumbnailWidth >= 112, `floor holds (${portraitMany.thumbnailWidth}px)`);
  assert.ok(portraitMany.stripSize <= 689 - 8 - portraitMany.heroHeight + 0.5);
  const perRow = Math.floor((392 + 8) / (portraitMany.thumbnailWidth + 8));
  const rowsNeeded = Math.ceil(20 / perRow) * (portraitMany.thumbnailHeight + 8);
  assert.ok(rowsNeeded > portraitMany.stripSize, 'the strip scrolls for the rest');
  assert.ok(portraitFew.stripSize < portraitMany.stripSize, 'a short strip only takes the rows it needs');

  // Alone with the hero: no strip at all, the hero fits the whole surface.
  const solo = computeSpotlightGeometry(0, 800, 450, 6);
  assert.equal(solo.stripSize, 0);
  assert.equal(solo.heroWidth, 800);
});

// Surfaces the real client measures (tiles' content box), and the shapes a
// hero really shows: cameras either way up, shared windows of any shape
// with their 44px docked header.
const SURFACES = [
  ['Pixel 8 landscape', 794, 352, 6],
  ['iPhone SE landscape', 606, 363, 6],
  ['Pixel 8 portrait', 392, 689, 8],
  ['iPad mini portrait', 736, 866, 10],
  ['iPad mini landscape', 992, 610, 14],
  ['desktop 1280x800', 1240, 634, 14],
] as const;
const HEROES = [
  ['16:9 camera', { aspect: 16 / 9, header: 0 }],
  ['9:16 phone camera', { aspect: 9 / 16, header: 0 }],
  ['16:10 window', { aspect: 16 / 10, header: 44 }],
  ['4:3 window', { aspect: 4 / 3, header: 44 }],
  ['phone-shaped window', { aspect: 9 / 19.5, header: 44 }],
  ['ultrawide window', { aspect: 32 / 9, header: 44 }],
] as const;

test('#239 the spotlight hero is always the biggest picture: its video beats every thumbnail', () => {
  // The review case: on a Pixel 8 in portrait a 16:9 hero BOX left a shared
  // window ~392x177 of video while thumbnails were 260x146. The hero now
  // takes its media's own shape, and thumbnails are capped against it.
  for (const [surface, width, height, gap] of SURFACES) {
    for (const [shape, hero] of HEROES) {
      for (const count of [1, 3, 4, 6]) {
        const g = computeSpotlightGeometry(count, width, height, gap, hero);
        const video = g.heroWidth * (g.heroHeight - hero.header);
        const thumbnail = g.thumbnailWidth * g.thumbnailHeight;
        assert.ok(
          video >= 2 * thumbnail,
          `${surface}, ${shape}, ${count} thumbnails: hero video ${Math.round(video)} vs thumbnail ${Math.round(thumbnail)}`
        );
        assert.ok(g.heroWidth <= width + 0.5 && g.heroHeight <= height + 0.5, `${surface}, ${shape}: the hero fits`);
        assert.ok(Math.abs(g.heroWidth / (g.heroHeight - hero.header) - hero.aspect) < 0.01, `${surface}, ${shape}: no letterbox`);
      }
    }
  }
});

test('#239 a phone camera held upright is a tall hero, not a sliver in a 16:9 box', () => {
  const upright = { aspect: 9 / 16, header: 0 };
  // Portrait phone: all the height but one floor row of thumbnails (the
  // strip scrolls for the rest). It was a 124px-wide sliver in a 16:9 box.
  const portrait = computeSpotlightGeometry(4, 392, 689, 8, upright);
  assert.ok(portrait.heroWidth > 330, `${portrait.heroWidth}px wide`);
  assert.ok(portrait.heroHeight > 600, `${portrait.heroHeight}px tall`);
  // Landscape phone: the full height, the strip beside it.
  const landscape = computeSpotlightGeometry(4, 794, 352, 6, upright);
  assert.equal(landscape.placement, 'side');
  assert.ok(Math.abs(landscape.heroHeight - 352) < 0.5);
  // A shared window's header is part of its box, over its video.
  const share = computeSpotlightGeometry(4, 392, 689, 8, { aspect: 16 / 10, header: 44 });
  assert.ok(Math.abs(share.heroHeight - (392 / 1.6 + 44)) < 0.5, `${share.heroHeight}`);
});

test('#248 a camera spotlight hero shows its whole frame: covered, never cropped, never letterboxed', () => {
  for (const [shape, video] of [
    ['16:9 camera', { width: 1280, height: 720 }],
    ['9:16 phone camera', { width: 720, height: 1280 }],
    ['4:3 webcam', { width: 640, height: 480 }],
  ] as const) {
    const tile = {
      classList: { contains: () => false },
      querySelector: () => ({ videoWidth: video.width, videoHeight: video.height }),
    } as unknown as HTMLElement;
    const hero = spotlightHeroMedia(tile);
    assert.ok(Math.abs(hero.aspect - video.width / video.height) < 1e-9, `${shape}: the aspect it shows`);
    for (const [surface, width, height, gap] of SURFACES) {
      for (const count of [0, 1, 3, 6]) {
        const g = computeSpotlightGeometry(count, width, height, gap, hero);
        const label = `${surface}, ${shape}, ${count} thumbnails`;
        const box = { width: g.heroWidth, height: g.heroHeight };
        assert.equal(cameraFit(video, box), 'cover', `${label}: fills its box`);
        const crop = coverCropFractions(video, box);
        assert.ok(crop.sides < 0.01 && crop.vertical < 0.01, `${label}: crops nothing (${JSON.stringify(crop)})`);
      }
    }
  }
});

test('#239 the spotlight hero has an explicit box, so a centred grid cell cannot collapse it', async () => {
  // Root cause of the empty spotlight hero (0.9.27-0.9.29): #204 gave
  // `.tiles` `place-items: center`, and the hero -- `width: 100%` but
  // `height: auto`, with only absolutely positioned children -- shrank to its
  // 2px border. Only the strip rendered ("I only see Bob").
  const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');
  const hero = /\.tile\.is-spotlight\s*\{(?<body>[^}]+)\}/i.exec(css)?.groups?.body ?? '';
  assert.match(hero, /height\s*:\s*min\(100%,\s*var\(--spotlight-hero-height,\s*100%\)\)/i);
  assert.match(hero, /width\s*:\s*min\(100%,\s*var\(--spotlight-hero-width,\s*100%\)\)/i);
  const spotlight = /\.tiles\.layout-spotlight\s*\{(?<body>[^}]+)\}/i.exec(css)?.groups?.body ?? '';
  assert.match(spotlight, /place-items\s*:\s*stretch/i, 'the strip fills its track');
  const side = /\.tiles\.layout-spotlight\.spotlight-side\s*\{(?<body>[^}]+)\}/i.exec(css)?.groups?.body ?? '';
  assert.match(side, /grid-template-columns\s*:[^;]*var\(--spotlight-strip-size/i, 'the side strip is a column');
});

test('spotlight thumbnails scroll inside the strip and keep a fixed media aspect, never a label-driven width', async () => {
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
  // #239: one scroll axis whatever the placement (a side column, or rows
  // under the hero), and a fling at its end never drags the page along.
  assert.match(strip, /overflow-x\s*:\s*hidden/i);
  assert.match(strip, /overflow-y\s*:\s*auto/i);
  assert.match(strip, /overscroll-behavior\s*:\s*contain/i);
  assert.match(thumbnail, /aspect-ratio\s*:\s*16\s*\/\s*9/i);
  assert.match(thumbnail, /width\s*:\s*min\(100%,\s*var\(--spotlight-thumbnail-width/i);
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

// ---------------------------------------------------------------------------
// #204 PR2: the web grid packs with the shared geometry. A fake
// ResizeObserver hands the callback back so a resize can be fired, and a
// fake getComputedStyle supplies the padding/gap the packer subtracts.
// ---------------------------------------------------------------------------

function installFakeLayoutEngine(surface: FakeElement, pad: number, gap: number) {
  const originalRO = (globalThis as { ResizeObserver?: unknown }).ResizeObserver;
  const originalGCS = (globalThis as { getComputedStyle?: unknown }).getComputedStyle;
  let resizeCallback: (() => void) | null = null;
  class FakeResizeObserver {
    constructor(callback: () => void) {
      resizeCallback = callback;
    }
    observe() {}
    disconnect() {}
  }
  Object.defineProperty(globalThis, 'ResizeObserver', { configurable: true, value: FakeResizeObserver });
  Object.defineProperty(globalThis, 'getComputedStyle', {
    configurable: true,
    value: (el: unknown) => {
      if (el !== surface) return { paddingLeft: '0px', paddingRight: '0px', paddingTop: '0px', paddingBottom: '0px', getPropertyValue: () => '' };
      return {
        paddingLeft: `${pad}px`,
        paddingRight: `${pad}px`,
        paddingTop: `${pad}px`,
        paddingBottom: `${pad}px`,
        getPropertyValue: (name: string) => (name === '--tile-gap' ? `${gap}px` : ''),
      };
    },
  });
  return {
    fireResize() {
      resizeCallback?.();
    },
    restore() {
      if (originalRO === undefined) delete (globalThis as { ResizeObserver?: unknown }).ResizeObserver;
      else Object.defineProperty(globalThis, 'ResizeObserver', { configurable: true, value: originalRO });
      if (originalGCS === undefined) delete (globalThis as { getComputedStyle?: unknown }).getComputedStyle;
      else Object.defineProperty(globalThis, 'getComputedStyle', { configurable: true, value: originalGCS });
    },
  };
}

test('#239 a short last row is centred under the full rows, and spotlight clears the offset', () => {
  const fakeDom = installFakeDom();
  const tilesEl = fakeDom.document.createElement('div');
  const engine = installFakeLayoutEngine(tilesEl, 20, 16);
  try {
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);
    const tiles = Array.from({ length: 7 }, (_, index) => {
      const tile = fakeDom.document.createElement('div');
      tile.id = `tile-${index}`;
      tile.className = 'tile';
      tile.dataset.owner = `Peer ${index}`;
      tilesEl.appendChild(tile);
      return tile;
    });
    const ctx = {
      dom: { tilesEl, topbarRight },
      state: { tileLayoutMode: 'grid', pinnedTileId: null, layoutModeButtons: null, speakerSmoothingTimer: null },
      ui: { logEvent: () => {} },
      cb: { activeRemoteControlForTile: () => null, fitTileLabels: () => {} },
      speakerScores: new Map(),
      activeSpeakerTargets: new Set(),
    } as unknown as HarnessContext;
    const layout = setupTileLayout(ctx);
    // Seven cameras on a wide surface pack 4x2 (#248: camera tiles may crop,
    // and 4x2 of ~7:6 tiles beats 3x3 of 16:9): the last row holds three
    // tiles, which start on half-track 2 of 8 -- centred under the four.
    tilesEl.clientWidth = 1240 + 40;
    tilesEl.clientHeight = 634 + 40;
    layout.applyTileLayout();
    assert.equal(tilesEl.style.values.get('--gallery-cols'), '4');
    // The half tracks the start counts in, handed to CSS as a plain integer:
    // older WebKit rejects `repeat(calc(...), ...)` and drops the template.
    assert.equal(tilesEl.style.values.get('--gallery-half-tracks'), '8');
    const starts = () => tiles.map((tile) => tile.style.values.get('grid-column-start') ?? '');
    assert.deepEqual(starts(), ['', '', '', '', '2', '', '']);

    (ctx.state as { tileLayoutMode: string }).tileLayoutMode = 'spotlight';
    layout.applyTileLayout();
    assert.deepEqual(starts(), ['', '', '', '', '', '', ''], 'spotlight places its own tiles');
  } finally {
    engine.restore();
    fakeDom.restore();
  }
});

test('#204 the web grid packs tiles with the shared geometry and repacks on resize', () => {
  const fakeDom = installFakeDom();
  const tilesEl = fakeDom.document.createElement('div');
  const engine = installFakeLayoutEngine(tilesEl, 20, 16);
  try {
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);
    for (let index = 0; index < 4; index += 1) {
      const tile = fakeDom.document.createElement('div');
      tile.id = `tile-${index}`;
      tile.className = 'tile';
      tile.dataset.owner = `Peer ${index}`;
      tilesEl.appendChild(tile);
    }
    const ctx = {
      dom: { tilesEl, topbarRight },
      state: { tileLayoutMode: 'grid', pinnedTileId: null, layoutModeButtons: null, speakerSmoothingTimer: null },
      ui: { logEvent: () => {} },
      cb: { activeRemoteControlForTile: () => null, fitTileLabels: () => {} },
      speakerScores: new Map(),
      activeSpeakerTargets: new Set(),
    } as unknown as HarnessContext;
    const layout = setupTileLayout(ctx);

    // The issue's headline case: four tiles in a tall 1100x1750 window pack
    // as one column (66% fill), not the 2x2 the old auto-fit produced.
    tilesEl.clientWidth = 1100 + 40;
    tilesEl.clientHeight = 1750 + 40;
    layout.applyTileLayout();
    const vars = tilesEl.style.values;
    assert.equal(vars.get('--gallery-cols'), '1');
    assert.equal(vars.get('--gallery-half-tracks'), '2');
    assert.equal(vars.get('--gallery-rows'), '4');
    assert.match(vars.get('--gallery-tile-width') ?? '', /^\d+(\.\d+)?px$/);
    assert.equal(vars.get('--gallery-gap'), '16px', 'the packer keeps the CSS gap when tiles are not compact');

    // A drag-resize to a wide window repacks without a mode/count change.
    tilesEl.clientWidth = 900 + 40;
    tilesEl.clientHeight = 500 + 40;
    engine.fireResize();
    assert.equal(vars.get('--gallery-cols'), '2');
    assert.equal(vars.get('--gallery-half-tracks'), '4');
    assert.equal(vars.get('--gallery-rows'), '2');

    // Spotlight owns its own template: the packer leaves it alone.
    (ctx.state as { tileLayoutMode: string; pinnedTileId: string | null }).tileLayoutMode = 'spotlight';
    (ctx.state as { pinnedTileId: string | null }).pinnedTileId = 'tile-0';
    vars.delete('--gallery-cols');
    layout.applyTileLayout();
    assert.equal(vars.get('--gallery-cols'), undefined);

    // #239: it has a geometry of its own instead -- on this wide surface the
    // strip goes beside the hero, one thumbnail width for all.
    assert.equal(tilesEl.classList.contains('spotlight-side'), true);
    assert.match(vars.get('--spotlight-strip-size') ?? '', /^\d+(\.\d+)?px$/);
    assert.match(vars.get('--spotlight-thumbnail-width') ?? '', /^\d+(\.\d+)?px$/);
    assert.match(vars.get('--spotlight-hero-height') ?? '', /^\d+(\.\d+)?px$/);
    // Rotating to a tall surface moves the strip under the hero, without a
    // mode change.
    tilesEl.clientWidth = 400 + 40;
    tilesEl.clientHeight = 800 + 40;
    engine.fireResize();
    assert.equal(tilesEl.classList.contains('spotlight-side'), false);
    // And back to grid, the placement class leaves with the strip.
    tilesEl.clientWidth = 900 + 40;
    tilesEl.clientHeight = 500 + 40;
    layout.applyTileLayout();
    assert.equal(tilesEl.classList.contains('spotlight-side'), true);
    (ctx.state as { tileLayoutMode: string }).tileLayoutMode = 'grid';
    layout.applyTileLayout();
    assert.equal(tilesEl.classList.contains('spotlight-side'), false);
  } finally {
    engine.restore();
    fakeDom.restore();
  }
});

test('#248 a camera-only grid packs cropping tiles that fill a landscape phone; a share keeps 16:9', () => {
  const fakeDom = installFakeDom();
  const tilesEl = fakeDom.document.createElement('div');
  const engine = installFakeLayoutEngine(tilesEl, 12, 10);
  try {
    const topbarRight = fakeDom.document.createElement('div');
    fakeDom.document.root.appendChild(tilesEl);
    fakeDom.document.root.appendChild(topbarRight);
    for (let index = 0; index < 2; index += 1) {
      const tile = fakeDom.document.createElement('div');
      tile.id = `tile-${index}`;
      tile.className = 'tile';
      tile.dataset.owner = `Peer ${index}`;
      tilesEl.appendChild(tile);
    }
    const ctx = {
      dom: { tilesEl, topbarRight },
      state: { tileLayoutMode: 'grid', pinnedTileId: null, layoutModeButtons: null, speakerSmoothingTimer: null },
      ui: { logEvent: () => {} },
      cb: { activeRemoteControlForTile: () => null, fitTileLabels: () => {} },
      speakerScores: new Map(),
      activeSpeakerTargets: new Set(),
    } as unknown as HarnessContext;
    const layout = setupTileLayout(ctx);

    // A landscape phone's tile surface: 780x300 inside 12px padding.
    tilesEl.clientWidth = 780 + 24;
    tilesEl.clientHeight = 300 + 24;
    layout.applyTileLayout();
    const vars = tilesEl.style.values;
    assert.equal(vars.get('--gallery-cols'), '2');
    assert.equal(vars.get('--gallery-rows'), '1');
    // Each 385x300 cell is filled by a ~1.28:1 tile (16:9 would be 385x217).
    assert.equal(vars.get('--gallery-tile-width'), '385px');
    assert.equal(vars.get('--gallery-tile-height'), '300px', 'two cameras fill the whole height');

    // A share joins: nothing in the grid may crop now, so every cell is 16:9
    // -- 3x1 cells of 253.33x300 hold 253.33x142.5 tiles.
    const share = fakeDom.document.createElement('div');
    share.id = 'share';
    share.className = 'tile share-tile';
    share.dataset.owner = 'Peer 0';
    tilesEl.appendChild(share);
    layout.applyTileLayout();
    assert.equal(vars.get('--gallery-cols'), '3');
    assert.equal(vars.get('--gallery-tile-height'), '142.5px');
    assert.ok(Math.abs(parseFloat(vars.get('--gallery-tile-width') ?? '') - 760 / 3) < 1e-9);
  } finally {
    engine.restore();
    fakeDom.restore();
  }
});
