// #875 part 4: the multi-share count pill on a participant's camera tile.
//
// These tests drive the REAL wiring -- setupTiles' addShareTile/
// removeShareTile/ensureBaseTile calling into the real pill rendering and
// click-resolution code, with setupTileLayout's REAL pinTile wired in too --
// rather than only the pure helpers in shareCountPill.ts (see
// shareCountPill.test.ts for those). A passing test on the pure resolver
// proves nothing about whether the pill is actually rendered, counted, or
// wired to a click; this file is what would have caught that gap.
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
  type = '';
  disabled = false;
  onclick: ((event: Event) => void) | null = null;
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
  readonly scrollIntoViewCalls: unknown[] = [];
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

  getAttribute(name: string): string | null {
    return this.attributes.get(name) ?? null;
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

  scrollIntoView(options?: unknown) {
    this.scrollIntoViewCalls.push(options);
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

function createHarness(options: { reducedMotion?: boolean } = {}) {
  const originalDocument = globalThis.document;
  const originalLocalStorage = globalThis.localStorage;
  const originalHtmlDivElement = globalThis.HTMLDivElement;
  const originalWindow = globalThis.window;
  const originalRequestAnimationFrame = globalThis.requestAnimationFrame;

  const document = new FakeDocument();
  const storage = new Map<string, string>();
  const setItemCalls: Array<{ key: string; value: string }> = [];
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: FakeElement });
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => {
        storage.set(key, value);
        setItemCalls.push({ key, value });
      },
    },
  });
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { matchMedia: () => ({ matches: options.reducedMotion ?? false }) },
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
    room: {
      localParticipant: { identity: 'web-1', metadata: undefined as string | undefined },
      remoteParticipants: new Map<string, { identity: string; name?: string; metadata?: string }>(),
    },
    tileLayoutMode: 'grid' as 'grid' | 'spotlight',
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
    setItemCalls,
    storedLayout: () => storage.get(HARNESS_TILE_LAYOUT_STORAGE_KEY) ?? null,
    tileById: (id: string) => document.getElementById(id) as unknown as FakeElement | null,
    pillFor: (identity: string) =>
      ((document.getElementById(`tile-p-${identity}`) as FakeElement | null)?.querySelector<HTMLButtonElement>(
        '.share-count-pill'
      ) as unknown as FakeElement | null) ?? null,
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

function windowZOrderMetadata(order: number[]): string {
  return JSON.stringify({ petalWindowZOrder: order });
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test('the pill is absent at 0-1 shares, appears at 2, and updates as more windows are shared', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    assert.equal(harness.pillFor('native-1'), null, 'no shares yet');

    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 10);
    assert.equal(harness.pillFor('native-1'), null, 'one share: existing indicators cover it, no pill');

    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 11);
    assert.equal(harness.pillFor('native-1')?.textContent, '2');

    harness.tiles.addShareTile('native-1', false, 'k3', harness.makeTrack('t3') as never, 'Native', 12);
    assert.equal(harness.pillFor('native-1')?.textContent, '3');
  } finally {
    harness.restore();
  }
});

test('the pill drops back below the threshold and disappears when a share is removed', async () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 10);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 11);
    assert.equal(harness.pillFor('native-1')?.textContent, '2');

    harness.tiles.removeShareTile('native-1', 'k2');
    await wait(30);

    assert.equal(harness.pillFor('native-1'), null, 'back to 1 share: pill must disappear, not read "1"');
  } finally {
    harness.restore();
  }
});

test('the pill caps display at 9+ rather than showing the real count once it exceeds the cap', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    for (let windowId = 1; windowId <= 10; windowId += 1) {
      harness.tiles.addShareTile(
        'native-1',
        false,
        `k${windowId}`,
        harness.makeTrack(`t${windowId}`) as never,
        'Native',
        windowId
      );
    }
    assert.equal(harness.pillFor('native-1')?.textContent, '9+');
  } finally {
    harness.restore();
  }
});

test('a republish of the same window (replacement SID) does not double-count or blip the pill', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 10);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 11);
    assert.equal(harness.pillFor('native-1')?.textContent, '2');

    // Same window id 11, new track SID -- a quality-switch/republish, not a
    // new share (mirrors the #679 "genuinely new" suppression it rides on).
    harness.tiles.addShareTile('native-1', false, 'k2-new', harness.makeTrack('t2b') as never, 'Native', 11);
    assert.equal(harness.pillFor('native-1')?.textContent, '2', 'still 2 windows, not 3');
  } finally {
    harness.restore();
  }
});

test('a click resolves the CORRECT owner\'s foremost window when two participants share a colliding window id (#678 class)', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('alice', false);
    harness.tiles.ensureBaseTile('bob', false);
    harness.state.room.remoteParticipants.set('alice', { identity: 'alice', metadata: windowZOrderMetadata([43, 42]) });
    harness.state.room.remoteParticipants.set('bob', { identity: 'bob', metadata: windowZOrderMetadata([42, 44]) });

    // Both share window id 42 (the collision), plus one more each.
    harness.tiles.addShareTile('alice', false, 'a1', harness.makeTrack('a1') as never, 'Alice', 42);
    harness.tiles.addShareTile('alice', false, 'a2', harness.makeTrack('a2') as never, 'Alice', 43);
    harness.tiles.addShareTile('bob', false, 'b1', harness.makeTrack('b1') as never, 'Bob', 42);
    harness.tiles.addShareTile('bob', false, 'b2', harness.makeTrack('b2') as never, 'Bob', 44);

    const alicePill = harness.pillFor('alice');
    assert.ok(alicePill);
    alicePill!.onclick?.({ stopPropagation: () => {} } as unknown as Event);

    const pinnedTile = harness.tileById(harness.state.pinnedTileId!);
    assert.equal(pinnedTile?.dataset.owner, 'alice');
    assert.equal(pinnedTile?.dataset.windowId, '43', "alice's zOrder says 43 is foremost, not the colliding 42");
  } finally {
    harness.restore();
  }
});

test('with no petalWindowZOrder metadata, the click falls back to the most-recently-added share', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    // No metadata published at all -- an older sharer.
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);

    const pill = harness.pillFor('native-1');
    pill!.onclick?.({ stopPropagation: () => {} } as unknown as Event);

    const pinnedTile = harness.tileById(harness.state.pinnedTileId!);
    assert.equal(pinnedTile?.dataset.windowId, '6', 'the most recently added share wins the fallback');
  } finally {
    harness.restore();
  }
});

test('the click pins the resolved tile as a MANUAL spotlight and scrolls it into view', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);

    const pill = harness.pillFor('native-1');
    pill!.onclick?.({ stopPropagation: () => {} } as unknown as Event);

    const pinnedTile = harness.tileById(harness.state.pinnedTileId!)!;
    assert.equal(pinnedTile.dataset.windowId, '6');
    // 'manual' (not 'auto') is load-bearing per tileLayout.ts: only a manual
    // pin forces spotlight mode and records the user's explicit preference.
    assert.equal(harness.state.tileLayoutMode, 'spotlight');
    assert.equal(pinnedTile.scrollIntoViewCalls.length, 1);
    assert.deepEqual(pinnedTile.scrollIntoViewCalls[0], { block: 'nearest', inline: 'nearest', behavior: 'smooth' });
  } finally {
    harness.restore();
  }
});

test('scroll-into-view respects prefers-reduced-motion', () => {
  const harness = createHarness({ reducedMotion: true });
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);

    const pill = harness.pillFor('native-1');
    pill!.onclick?.({ stopPropagation: () => {} } as unknown as Event);

    const pinnedTile = harness.tileById(harness.state.pinnedTileId!)!;
    assert.deepEqual(pinnedTile.scrollIntoViewCalls[0], { block: 'nearest', inline: 'nearest', behavior: 'auto' });
  } finally {
    harness.restore();
  }
});

test('the local participant\'s own pill shows the count but is non-interactive', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('web-1', true);
    harness.tiles.addShareTile('web-1', true, 'own-1', harness.makeTrack('o1') as never, 'Mine', 1);
    harness.tiles.addShareTile('web-1', true, 'own-2', harness.makeTrack('o2') as never, 'Mine', 2);

    const pill = harness.pillFor('web-1');
    assert.equal(pill?.textContent, '2');
    assert.equal(pill?.disabled, true);
    assert.equal(pill?.onclick, null);
  } finally {
    harness.restore();
  }
});

test('the pill carries an identity-tinted background/ink and a descriptive aria-label', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.state.room.remoteParticipants.set('native-1', { identity: 'native-1', name: 'Native' });
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);

    const pill = harness.pillFor('native-1')!;
    assert.match(pill.getAttribute('aria-label') ?? '', /2 windows shared by Native/);
    assert.ok(pill.style.values.get('--share-pill-bg'));
    assert.ok(pill.style.values.get('--share-pill-ink'));
  } finally {
    harness.restore();
  }
});

// #785: automatic layout-mode transitions must never persist a preference to
// localStorage. The pill's own bookkeeping (noteShareWindowAdded/Removed,
// firing on the exact same add/remove events `maybeAutoSpotlightFirstShare`
// reacts to) must not be a second, accidental path into that persisted key.
test('#785: adding and removing share tiles for the pill never writes to localStorage on its own', async () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);
    harness.tiles.removeShareTile('native-1', 'k1');
    harness.tiles.removeShareTile('native-1', 'k2');
    await wait(30);

    assert.deepEqual(harness.setItemCalls, [], 'auto add/remove bookkeeping must never persist a layout preference');
  } finally {
    harness.restore();
  }
});

// The other half of the same boundary: a MANUAL pill click is a real user
// choice, so -- unlike the automatic bookkeeping above -- it is SUPPOSED to
// persist, exactly like clicking any other tile does. This is not a #785
// violation; it is what proves the click reuses the real manual pin path
// rather than a parallel one that forgets to persist (or wrongly does).
test('#785 boundary: a manual pill click persists like any other manual spotlight pin', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);

    const pill = harness.pillFor('native-1');
    pill!.onclick?.({ stopPropagation: () => {} } as unknown as Event);

    assert.equal(harness.storedLayout(), 'spotlight');
  } finally {
    harness.restore();
  }
});

test('a click with no resolvable window (metadata present but nothing tiled overlaps) does nothing', () => {
  const harness = createHarness();
  try {
    harness.tiles.ensureBaseTile('native-1', false);
    harness.state.room.remoteParticipants.set('native-1', {
      identity: 'native-1',
      metadata: windowZOrderMetadata([999, 998]),
    });
    harness.tiles.addShareTile('native-1', false, 'k1', harness.makeTrack('t1') as never, 'Native', 5);
    harness.tiles.addShareTile('native-1', false, 'k2', harness.makeTrack('t2') as never, 'Native', 6);

    // The first share auto-spotlit itself (unrelated existing behavior) --
    // capture that state so the assertion below is specifically about the
    // CLICK doing nothing, not about spotlight being untouched from the start.
    const pinnedBeforeClick = harness.state.pinnedTileId;
    const storedBeforeClick = harness.storedLayout();

    const pill = harness.pillFor('native-1');
    pill!.onclick?.({ stopPropagation: () => {} } as unknown as Event);

    assert.equal(harness.state.pinnedTileId, pinnedBeforeClick, 'an unresolvable click must not touch the pin');
    assert.equal(harness.storedLayout(), storedBeforeClick);
  } finally {
    harness.restore();
  }
});
