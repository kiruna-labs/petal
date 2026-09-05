// #785: stopping a share left the web client stuck in spotlight with the
// user's OWN webcam promoted as hero, and the automatic switch had already
// overwritten the persisted layout preference so the wrong mode survived a
// reload.
//
// These tests drive the REAL path -- setupTiles' addShareTile/removeShareTile
// wired to the REAL setupTileLayout -- rather than the pure helpers those
// modules delegate to. The defect lived in the wiring (who records the mode,
// who restores it, which tile the fallback lands on), so a unit test on
// shared/logic/tileLayoutMode.ts alone could not have caught it.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { setupTiles } from '../src/tiles.ts';
import { setupTileLayout } from '../src/tileLayout.ts';
import { HARNESS_TILE_LAYOUT_STORAGE_KEY } from '../src/constants.ts';
import type { HarnessContext } from '../src/context.ts';

type Listener = EventListenerOrEventListenerObject;

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
  textContent = '';
  value = '';
  autoplay = false;
  playsInline = false;
  muted = false;
  tabIndex = -1;
  srcObject: unknown = null;
  readyState = 0;
  videoWidth = 0;
  videoHeight = 0;
  dataset: Record<string, string | undefined> = {};
  parentElement: FakeElement | null = null;
  readonly children: FakeElement[] = [];
  readonly classList = new FakeClassList(this);
  readonly style = new FakeStyle();
  readonly attributes = new Map<string, string>();
  private readonly listeners = new Map<string, Listener[]>();
  readonly tagName: string;

  constructor(tagName: string) {
    this.tagName = tagName;
  }

  get firstChild(): FakeElement | null {
    return this.children[0] ?? null;
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

  insertBefore<T extends FakeElement>(child: T, before: FakeElement | null): T {
    child.remove();
    child.parentElement = this;
    const index = before ? this.children.indexOf(before) : -1;
    if (index === -1) this.children.push(child);
    else this.children.splice(index, 0, child);
    return child;
  }

  remove() {
    const parent = this.parentElement;
    if (!parent) return;
    const index = parent.children.indexOf(this);
    if (index !== -1) parent.children.splice(index, 1);
    this.parentElement = null;
  }

  contains(element: FakeElement): boolean {
    return element === this || this.children.some((child) => child.contains(element));
  }

  setAttribute(name: string, value: string) {
    if (name === 'id') this.id = value;
    this.attributes.set(name, value);
  }

  addEventListener(type: string, listener: Listener, _options?: unknown) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  dispatchEvent(event: Event): boolean {
    for (const listener of this.listeners.get(event.type) ?? []) {
      if (typeof listener === 'function') listener.call(this, event);
      else listener.handleEvent(event);
    }
    return true;
  }

  click() {
    this.dispatchEvent({ type: 'click', currentTarget: this, target: this } as unknown as Event);
  }

  requestVideoFrameCallback(_callback: unknown): number {
    return 1;
  }

  focus() {}

  pause() {}

  animate() {
    return { cancel() {}, finished: Promise.resolve() } as unknown as Animation;
  }

  getBoundingClientRect(): DOMRect {
    return {
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 160,
      bottom: 90,
      width: 160,
      height: 90,
      toJSON: () => ({}),
    } as DOMRect;
  }

  closest<T extends Element>(selector: string): T | null {
    let current: FakeElement | null = this;
    while (current) {
      if (matchesSelector(current, selector)) return current as unknown as T;
      current = current.parentElement;
    }
    return null;
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
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  if (selector === 'video') return element.tagName === 'video';
  if (selector === '.tile video') {
    return element.tagName === 'video' && element.parentElement?.classList.contains('tile') === true;
  }
  if (selector === '.tile:not(.share-tile)') {
    return element.classList.contains('tile') && !element.classList.contains('share-tile');
  }
  if (selector.startsWith('.')) return element.classList.contains(selector.slice(1));
  return false;
}

class FakeDocument {
  readonly body = new FakeElement('body');

  createElement(tagName: string): FakeElement {
    return new FakeElement(tagName.toLowerCase());
  }

  getElementById(id: string): FakeElement | null {
    return findById(this.body, id);
  }

  querySelector<T extends Element>(selector: string): T | null {
    return this.body.querySelector(selector);
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    return this.body.querySelectorAll(selector);
  }
}

function findById(root: FakeElement, id: string): FakeElement | null {
  if (root.id === id) return root;
  for (const child of root.children) {
    const found = findById(child, id);
    if (found) return found;
  }
  return null;
}

/** The layout the user had saved before any of this ran. */
function createHarness(storedMode: 'grid' | 'spotlight') {
  const originalDocument = globalThis.document;
  const originalLocalStorage = globalThis.localStorage;
  const originalHtmlDivElement = globalThis.HTMLDivElement;
  const originalWindow = globalThis.window;
  const originalRequestAnimationFrame = globalThis.requestAnimationFrame;

  const document = new FakeDocument();
  const storage = new Map<string, string>([[HARNESS_TILE_LAYOUT_STORAGE_KEY, storedMode]]);
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: FakeElement });
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
    },
  });
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { matchMedia: () => ({ matches: false }) },
  });
  Object.defineProperty(globalThis, 'requestAnimationFrame', {
    configurable: true,
    value: (callback: FrameRequestCallback) => {
      callback(0);
      return 1;
    },
  });

  const tilesEl = document.createElement('div') as unknown as HTMLDivElement;
  const topbarRight = document.createElement('div') as unknown as HTMLDivElement;
  const participantCountEl = document.createElement('div') as unknown as HTMLElement;
  const displayNameInput = document.createElement('input') as unknown as HTMLInputElement;
  displayNameInput.value = 'Web Tester';
  document.body.appendChild(tilesEl as unknown as FakeElement);
  document.body.appendChild(topbarRight as unknown as FakeElement);

  const state = {
    room: { localParticipant: { identity: 'web-1' }, remoteParticipants: new Map() },
    tileLayoutMode: storedMode,
    pinnedTileId: null as string | null,
    layoutModeButtons: null,
    speakerSmoothingTimer: null,
    activeRemoteControl: null,
  };
  const cb: Record<string, unknown> = {
    activeRemoteControlForTile: () => null,
    bindHoverTelepointer: () => {},
    ensureRemoteControlAffordance: () => {},
    publishViewerDemand: () => {},
    renderTelepointersForWindow: () => {},
    renderDrawForWindow: () => {},
    removeTelepointersForWindow: () => {},
    removeTelepointersForParticipant: () => {},
    removeDrawForWindow: () => {},
    removeDrawForParticipant: () => {},
    repositionRemoteDraw: () => {},
    repositionRemoteTelepointers: () => {},
    updateParticipantCount: () => {},
    setPublicationPaused: () => {},
    stopRemoteControl: () => {},
    syncHarnessHook: () => {},
  };
  const ctx = {
    dom: { tilesEl, topbarRight, participantCountEl, displayNameInput },
    state,
    ui: { logEvent: () => {}, showToast: () => {} },
    cb,
    speakerScores: new Map(),
    activeSpeakerTargets: new Set(),
  } as unknown as HarnessContext;

  const layout = setupTileLayout(ctx);
  const tiles = setupTiles(ctx, {
    remoteControlShareRemovalGraceMs: 10,
    shareReplacementGraceMs: 10,
  });
  Object.assign(cb, {
    applyTileLayout: layout.applyTileLayout,
    pinTile: layout.pinTile,
    shareTileCount: layout.shareTileCount,
    bindTileInteractions: layout.bindTileInteractions,
    fitTileLabels: tiles.fitTileLabels,
  });

  const makeTrack = (id: string) => {
    const mediaStreamTrack = { id, kind: 'video' };
    const mediaStream = { getTracks: () => [mediaStreamTrack] };
    return {
      mediaStreamTrack,
      attach: (video: HTMLVideoElement) => {
        (video as unknown as FakeElement).srcObject = mediaStream;
      },
    };
  };

  return {
    ctx,
    state,
    tiles,
    layout,
    makeTrack,
    storedLayout: () => storage.get(HARNESS_TILE_LAYOUT_STORAGE_KEY) ?? null,
    tileById: (id: string) => document.getElementById(id),
    shareTiles: () => document.querySelectorAll<HTMLDivElement>('.share-tile'),
    restore: () => {
      const put = (key: string, original: unknown) => {
        if (original === undefined) Reflect.deleteProperty(globalThis, key);
        else Object.defineProperty(globalThis, key, { configurable: true, value: original });
      };
      put('document', originalDocument);
      put('localStorage', originalLocalStorage);
      put('HTMLDivElement', originalHtmlDivElement);
      put('window', originalWindow);
      put('requestAnimationFrame', originalRequestAnimationFrame);
    },
  };
}

/** Local camera tile + one remote participant camera tile, both with video. */
function seedTwoParticipantGrid(harness: ReturnType<typeof createHarness>) {
  harness.tiles.setTileCamera('web-1', true, harness.makeTrack('local-cam') as never);
  harness.tiles.setTileCamera('native-1', false, harness.makeTrack('remote-cam') as never);
  return {
    localTile: harness.tileById('tile-p-web-1')!,
    remoteTile: harness.tileById('tile-p-native-1')!,
  };
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test('#785 a remote share auto-spotlights without touching the saved preference, and stopping it returns to grid', async () => {
  const harness = createHarness('grid');
  try {
    const { localTile } = seedTwoParticipantGrid(harness);

    harness.tiles.addShareTile('native-1', false, 'share-track', harness.makeTrack('share') as never, 'Native', 42);
    const shareTile = harness.shareTiles()[0]!;
    assert.equal(harness.state.tileLayoutMode, 'spotlight', 'the share should auto-spotlight');
    assert.equal(harness.state.pinnedTileId, shareTile.id);
    // Defect 3: an automatic switch must never overwrite the user's preference.
    assert.equal(harness.storedLayout(), 'grid', 'auto-spotlight must not persist');

    harness.tiles.removeShareTile('native-1', 'share-track');
    await wait(30);

    assert.equal(harness.shareTiles().length, 0);
    // Defects 1+2: back to the mode the user was actually in, and emphatically
    // NOT spotlighting the local self-view.
    assert.equal(harness.state.tileLayoutMode, 'grid', 'stopping the last share must restore grid');
    assert.notEqual(harness.state.pinnedTileId, localTile.id, 'never promote the self-view');
    assert.equal(harness.state.pinnedTileId, null);
    assert.equal(harness.storedLayout(), 'grid');
  } finally {
    harness.restore();
  }
});

test('#785 a manual spotlight taken during a share survives the share ending, and never lands on the self-view', async () => {
  const harness = createHarness('grid');
  try {
    const { localTile, remoteTile } = seedTwoParticipantGrid(harness);

    harness.tiles.addShareTile('native-1', false, 'share-track', harness.makeTrack('share') as never, 'Native', 42);
    assert.equal(harness.state.tileLayoutMode, 'spotlight');

    // The user clicks a participant tile: an explicit choice of spotlight that
    // must outlive the automatic switch it replaced.
    harness.layout.pinTile(remoteTile as unknown as HTMLDivElement, 'manual');
    assert.equal(harness.storedLayout(), 'spotlight', 'a manual pin is an explicit preference');

    harness.tiles.removeShareTile('native-1', 'share-track');
    await wait(30);

    assert.equal(harness.state.tileLayoutMode, 'spotlight', 'a manual choice is never auto-restored');
    assert.equal(harness.state.pinnedTileId, remoteTile.id);
    assert.notEqual(harness.state.pinnedTileId, localTile.id);
  } finally {
    harness.restore();
  }
});

test('#785 a share ending while the user chose spotlight themselves falls back to a remote tile, never the self-view', async () => {
  const harness = createHarness('spotlight');
  try {
    const { localTile, remoteTile } = seedTwoParticipantGrid(harness);

    harness.tiles.addShareTile('native-1', false, 'share-track', harness.makeTrack('share') as never, 'Native', 42);
    const shareTile = harness.shareTiles()[0]!;
    assert.equal(harness.state.pinnedTileId, shareTile.id);

    harness.tiles.removeShareTile('native-1', 'share-track');
    await wait(30);

    // Started in spotlight, so nothing to restore -- but the hero must be the
    // remote participant, not the user's own webcam (#785's headline symptom).
    assert.equal(harness.state.tileLayoutMode, 'spotlight');
    assert.equal(harness.state.pinnedTileId, remoteTile.id, 'hero should be the remote participant');
    assert.notEqual(harness.state.pinnedTileId, localTile.id);
    assert.equal(harness.storedLayout(), 'spotlight');
  } finally {
    harness.restore();
  }
});

test('#785 the local share tile is a legitimate hero, and stopping it still restores the previous mode', async () => {
  const harness = createHarness('grid');
  try {
    seedTwoParticipantGrid(harness);

    // Sharing your OWN window: the share tile carries content, so it is a fine
    // hero -- unlike the self-view camera tile.
    harness.tiles.addShareTile('web-1', true, 'own-share', harness.makeTrack('own') as never, 'Mine', 7);
    const ownShare = harness.shareTiles()[0]!;
    assert.equal(harness.state.pinnedTileId, ownShare.id);
    assert.equal(harness.storedLayout(), 'grid');

    harness.tiles.removeShareTile('web-1', 'own-share');
    await wait(30);

    assert.equal(harness.state.tileLayoutMode, 'grid');
    assert.equal(harness.storedLayout(), 'grid');
  } finally {
    harness.restore();
  }
});

test('#785 only the LAST share leaving restores the previous mode', async () => {
  const harness = createHarness('grid');
  try {
    seedTwoParticipantGrid(harness);

    harness.tiles.addShareTile('native-1', false, 'share-a', harness.makeTrack('a') as never, 'Native', 42);
    harness.tiles.addShareTile('native-1', false, 'share-b', harness.makeTrack('b') as never, 'Native', 43);
    assert.equal(harness.shareTiles().length, 2);
    assert.equal(harness.state.tileLayoutMode, 'spotlight');

    harness.tiles.removeShareTile('native-1', 'share-a');
    await wait(30);
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.state.tileLayoutMode, 'spotlight', 'one share left: stay spotlighted');

    harness.tiles.removeShareTile('native-1', 'share-b');
    await wait(30);
    assert.equal(harness.shareTiles().length, 0);
    assert.equal(harness.state.tileLayoutMode, 'grid');
  } finally {
    harness.restore();
  }
});
