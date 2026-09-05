import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { browserColorCorrectionMode, setupTiles } from '../src/tiles.ts';
import type { ActiveRemoteControl, HarnessContext } from '../src/context.ts';

type Listener = EventListenerOrEventListenerObject;

test('browser color correction uses the GPU for native video-range shares', () => {
  assert.equal(browserColorCorrectionMode('video'), 'video-range-css');
  assert.equal(browserColorCorrectionMode('full'), 'full-range-canvas');
  assert.equal(browserColorCorrectionMode(null), 'none');
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
  assert.match(css, /video\.video-range-source-video\s*\{[^}]*contrast\(1\.164383562\)/s);
});

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
    const values = new Set(this.values());
    values.add(name);
    this.element.className = Array.from(values).join(' ');
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

class FakeElement {
  id = '';
  className = '';
  textContent = '';
  value = '';
  dataset: Record<string, string | undefined> = {};
  readonly children: FakeElement[] = [];
  readonly classList = new FakeClassList(this);
  parentElement: FakeElement | null = null;
  autoplay = false;
  playsInline = false;
  muted = false;
  tabIndex = -1;
  srcObject: unknown = null;
  readyState = 0;
  videoWidth = 0;
  videoHeight = 0;
  readonly videoFrameCallbacks: VideoFrameRequestCallback[] = [];
  readonly animations: Keyframe[][] = [];
  private readonly listeners = new Map<string, Listener[]>();

  readonly tagName: string;

  constructor(tagName: string) {
    this.tagName = tagName;
  }

  get firstChild(): FakeElement | null {
    return this.children[0] ?? null;
  }

  appendChild<T extends FakeElement>(child: T): T {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  insertBefore<T extends FakeElement>(child: T, before: FakeElement | null): T {
    child.parentElement = this;
    if (!before) {
      this.children.push(child);
      return child;
    }
    const index = this.children.indexOf(before);
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

  requestVideoFrameCallback(callback: VideoFrameRequestCallback): number {
    this.videoFrameCallbacks.push(callback);
    return this.videoFrameCallbacks.length;
  }

  focus(_options?: FocusOptions) {}

  pause() {}

  animate(keyframes: Keyframe[] | PropertyIndexedKeyframes | null, _options?: KeyframeAnimationOptions) {
    this.animations.push(Array.isArray(keyframes) ? keyframes : []);
    return { cancel() {}, finished: Promise.resolve() } as unknown as Animation;
  }

  getBoundingClientRect(): DOMRect {
    if (!this.classList.contains('tile')) {
      return {
        x: 0,
        y: 0,
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
        width: 0,
        height: 0,
        toJSON: () => ({}),
      } as DOMRect;
    }

    const siblings = this.parentElement?.children.filter((child) => child.classList.contains('tile')) ?? [this];
    const index = Math.max(0, siblings.indexOf(this));
    const total = siblings.length;
    const width = total <= 1 ? 400 : 190;
    const height = total <= 1 ? 260 : 140;
    const left = total <= 1 ? 0 : (index % 2) * 210;
    const top = total <= 1 ? 0 : Math.floor(index / 2) * 156;
    return {
      x: left,
      y: top,
      left,
      top,
      right: left + width,
      bottom: top + height,
      width,
      height,
      toJSON: () => ({}),
    } as DOMRect;
  }

  setAttribute(name: string, value: string) {
    if (name === 'id') this.id = value;
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

  closest<T extends Element>(selector: string): T | null {
    let current: FakeElement | null = this;
    while (current) {
      if (matchesSelector(current, selector)) return current as unknown as T;
      current = current.parentElement;
    }
    return null;
  }
}

class FakeDocument {
  readonly body = new FakeElement('body');

  createElement(tagName: string): FakeElement {
    return new FakeElement(tagName.toLowerCase());
  }

  getElementById(id: string): FakeElement | null {
    return findById(this.body, id);
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    return this.body.querySelectorAll(selector);
  }
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  if (selector === 'video') return element.tagName === 'video';
  if (selector === '.tile:not(.share-tile)') {
    return element.classList.contains('tile') && !element.classList.contains('share-tile');
  }
  if (selector.startsWith('.')) return element.classList.contains(selector.slice(1));
  return false;
}

function findById(root: FakeElement, id: string): FakeElement | null {
  if (root.id === id) return root;
  for (const child of root.children) {
    const found = findById(child, id);
    if (found) return found;
  }
  return null;
}

function installFakeDom() {
  const originalDocument = globalThis.document;
  const originalHtmlDivElement = globalThis.HTMLDivElement;
  const originalWindow = globalThis.window;
  const originalRequestAnimationFrame = globalThis.requestAnimationFrame;
  const document = new FakeDocument();
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: FakeElement });
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: {
      matchMedia: () => ({ matches: false }),
    },
  });
  Object.defineProperty(globalThis, 'requestAnimationFrame', {
    configurable: true,
    value: (callback: FrameRequestCallback) => {
      callback(0);
      return 1;
    },
  });
  return {
    document,
    restore: () => {
      if (originalDocument === undefined) Reflect.deleteProperty(globalThis, 'document');
      else Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
      if (originalHtmlDivElement === undefined) Reflect.deleteProperty(globalThis, 'HTMLDivElement');
      else Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: originalHtmlDivElement });
      if (originalWindow === undefined) Reflect.deleteProperty(globalThis, 'window');
      else Object.defineProperty(globalThis, 'window', { configurable: true, value: originalWindow });
      if (originalRequestAnimationFrame === undefined) Reflect.deleteProperty(globalThis, 'requestAnimationFrame');
      else
        Object.defineProperty(globalThis, 'requestAnimationFrame', {
          configurable: true,
          value: originalRequestAnimationFrame,
        });
    },
  };
}

function createTilesHarness(graceMs = 20, shareReplacementGraceMs = graceMs) {
  const fakeDom = installFakeDom();
  const tilesEl = fakeDom.document.createElement('div') as unknown as HTMLDivElement;
  const participantCountEl = fakeDom.document.createElement('div') as unknown as HTMLElement;
  const displayNameInput = fakeDom.document.createElement('input') as unknown as HTMLInputElement;
  displayNameInput.value = 'Web Tester';
  fakeDom.document.body.appendChild(tilesEl as unknown as FakeElement);
  const stopReasons: string[] = [];
  const toasts: string[] = [];
  const firstDecoded: string[] = [];
  const firstPresented: string[] = [];
  let attachCount = 0;
  const state: {
    room: {
      localParticipant: { identity: string };
      remoteParticipants: Map<string, unknown>;
    };
    activeRemoteControl: ActiveRemoteControl | null;
    pinnedTileId: string | null;
  } = {
    room: {
      localParticipant: { identity: 'web-1' },
      remoteParticipants: new Map(),
    },
    activeRemoteControl: null,
    pinnedTileId: null,
  };
  const ctx = {
    dom: {
      tilesEl,
      participantCountEl,
      displayNameInput,
    },
    state,
    ui: {
      logEvent: () => {},
      showToast: (message: string) => toasts.push(message),
    },
    hook: {
      pipelineStats: {
        trackFirstDecoded: (_identity: string, _windowId: number, sid: string) => firstDecoded.push(sid),
        trackFirstPresented: (_identity: string, _windowId: number, sid: string) => firstPresented.push(sid),
      },
    },
    cb: {
      bindTileInteractions: () => {},
      applyTileLayout: () => {},
      shareTileCount: () => fakeDom.document.querySelectorAll('.share-tile').length,
      pinTile: (tile: HTMLDivElement) => {
        state.pinnedTileId = tile.id;
      },
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
      updateParticipantCount: () => {},
      stopRemoteControl: (reason?: string) => {
        stopReasons.push(reason ?? '');
        state.activeRemoteControl = null;
      },
    },
  } as unknown as HarnessContext;
  const tiles = setupTiles(ctx, {
    remoteControlShareRemovalGraceMs: graceMs,
    shareReplacementGraceMs,
  });
  const makeTrack = (id: string) => {
    const mediaStreamTrack = { id, kind: 'video' };
    const mediaStream = { getTracks: () => [mediaStreamTrack] };
    return {
      mediaStreamTrack,
      attach: (video: HTMLVideoElement) => {
        attachCount += 1;
        (video as unknown as FakeElement).srcObject = mediaStream;
      },
    };
  };
  const track = makeTrack('video-track-1');
  return {
    ctx,
    tiles,
    track,
    makeTrack,
    firstDecoded,
    firstPresented,
    attachCount: () => attachCount,
    stopReasons,
    toasts,
    baseTiles: () => fakeDom.document.querySelectorAll<HTMLDivElement>('.tile:not(.share-tile)'),
    shareTiles: () => fakeDom.document.querySelectorAll<HTMLDivElement>('.share-tile'),
    restore: fakeDom.restore,
  };
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test('same-window share tile replacement preserves active remote control during grace', async () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const oldTile = harness.shareTiles()[0]!;
    harness.ctx.state.activeRemoteControl = {
      tileId: oldTile.id,
      targetUserId: 'native-1',
      windowId: 42,
      pointerId: null,
      grantToken: null,
    };

    harness.tiles.removeShareTile('native-1', 'old-track');
    assert.deepEqual(harness.stopReasons, []);
    assert.equal(harness.ctx.state.activeRemoteControl?.tileId, oldTile.id);

    harness.tiles.addShareTile('native-1', false, 'new-track', harness.track as never, 'Native', 42);
    const newTile = harness.shareTiles()[0]!;

    assert.equal(harness.ctx.state.activeRemoteControl?.tileId, newTile.id);
    await wait(30);
    assert.deepEqual(harness.stopReasons, []);
    assert.deepEqual(harness.toasts, []);
  } finally {
    harness.restore();
  }
});

test('unsubscribe-before-replacement reuses the exact stable tile and video', async () => {
  const harness = createTilesHarness(50, 20);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const stableTile = harness.shareTiles()[0]!;
    const stableVideo = stableTile.querySelector<HTMLVideoElement>('video');

    harness.tiles.removeShareTile('native-1', 'old-track');
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0], stableTile);

    harness.tiles.addShareTile('native-1', false, 'new-track', harness.track as never, 'Native', 42);
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0], stableTile);
    assert.equal(stableTile.querySelector('video'), stableVideo);
    assert.equal(stableTile.dataset.trackSid, 'new-track');

    await wait(35);
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0], stableTile);
  } finally {
    harness.restore();
  }
});

test('genuine share end removes the pending tile after the replacement bound', async () => {
  const harness = createTilesHarness(50, 10);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    harness.tiles.removeShareTile('native-1', 'old-track');

    assert.equal(harness.shareTiles().length, 1);
    await wait(25);
    assert.equal(harness.shareTiles().length, 0);

    // A duplicate late unsubscribe after final retirement is idempotent.
    harness.tiles.removeShareTile('native-1', 'old-track');
    assert.equal(harness.shareTiles().length, 0);
  } finally {
    harness.restore();
  }
});

test('duplicate unsubscribe shares one cancellable pending-removal timer', async () => {
  const harness = createTilesHarness(50, 15);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const stableTile = harness.shareTiles()[0]!;

    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.addShareTile('native-1', false, 'new-track', harness.track as never, 'Native', 42);

    await wait(30);
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0], stableTile);
    assert.equal(stableTile.dataset.trackSid, 'new-track');
  } finally {
    harness.restore();
  }
});

test('pending removal never reuses a tile across window ids', async () => {
  const harness = createTilesHarness(50, 20);
  try {
    harness.tiles.addShareTile('native-1', false, 'window-42-old', harness.track as never, 'Native', 42);
    const window42Tile = harness.shareTiles()[0]!;
    harness.tiles.removeShareTile('native-1', 'window-42-old');

    harness.tiles.addShareTile('native-1', false, 'window-43', harness.track as never, 'Native', 43);
    const window43Tile = harness.shareTiles().find((tile) => tile.dataset.windowId === '43');
    assert.ok(window43Tile);
    assert.notEqual(window43Tile, window42Tile);

    harness.tiles.addShareTile('native-1', false, 'window-42-new', harness.track as never, 'Native', 42);
    assert.equal(harness.shareTiles().length, 2);
    assert.equal(
      harness.shareTiles().find((tile) => tile.dataset.windowId === '42'),
      window42Tile
    );
    await wait(30);
    assert.equal(harness.shareTiles().length, 2);
  } finally {
    harness.restore();
  }
});

test('unsubscribe then participant disconnect preserves the stable tile for a replacement', async () => {
  const harness = createTilesHarness(50, 30);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const stableTile = harness.shareTiles()[0]!;
    const stableVideo = stableTile.querySelector('video');
    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.removeParticipantTiles('native-1');
    assert.equal(harness.shareTiles()[0], stableTile);

    harness.tiles.addShareTile(
      'native-1',
      false,
      'new-track',
      harness.makeTrack('video-track-2') as never,
      'Native',
      42
    );
    assert.equal(harness.shareTiles()[0], stableTile);
    assert.equal(stableTile.querySelector('video'), stableVideo);
    assert.equal(stableTile.dataset.trackSid, 'new-track');
    await wait(45);
    assert.equal(harness.shareTiles()[0], stableTile);
  } finally {
    harness.restore();
  }
});

test('genuine participant departure removes its leased share after the replacement bound', async () => {
  const harness = createTilesHarness(50, 10);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.removeParticipantTiles('native-1');
    assert.equal(harness.shareTiles().length, 1);
    await wait(25);
    assert.equal(harness.shareTiles().length, 0);
  } finally {
    harness.restore();
  }
});

test('global clear remains immediate after participant disconnect leased a share', () => {
  const harness = createTilesHarness(50, 1_000);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.removeParticipantTiles('native-1');
    assert.equal(harness.shareTiles().length, 1);
    harness.tiles.clearTiles();
    assert.equal(harness.shareTiles().length, 0);
  } finally {
    harness.restore();
  }
});

test('clearTiles immediately clears all pending share tiles', () => {
  const harness = createTilesHarness(50, 1_000);
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const tile = harness.shareTiles()[0]!;
    harness.ctx.state.activeRemoteControl = {
      tileId: tile.id,
      targetUserId: 'native-1',
      windowId: 42,
      pointerId: null,
      grantToken: null,
    };
    harness.tiles.removeShareTile('native-1', 'old-track');
    assert.equal(harness.shareTiles().length, 1);

    harness.tiles.clearTiles();
    assert.equal(harness.shareTiles().length, 0);
    assert.deepEqual(harness.stopReasons, ['share ended']);
  } finally {
    harness.restore();
  }
});

test('replacement-before-old-unsubscribe keeps exactly one visible same-window tile', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const oldTile = harness.shareTiles()[0]!;

    // LiveKit may deliver the replacement subscription before the old
    // publication's unsubscribe. The receiver must replace in place rather
    // than briefly showing two windows for the same native window.
    harness.tiles.addShareTile('native-1', false, 'new-track', harness.track as never, 'Native', 42);
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0], oldTile);
    assert.equal(oldTile.dataset.trackSid, 'new-track');

    // The late old-track event must only retire its stale lookup entry; it
    // cannot remove the replacement tile.
    harness.tiles.removeShareTile('native-1', 'old-track');
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0]?.dataset.trackSid, 'new-track');
  } finally {
    harness.restore();
  }
});

test('same-window replacement remains one visible tile when old cleanup never arrives', () => {
  const harness = createTilesHarness();
  try {
    const replacementTrack = harness.makeTrack('video-track-2');
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    harness.tiles.addShareTile('native-1', false, 'new-track', replacementTrack as never, 'Native', 42);

    // This represents the bounded native old-unpublish timeout. There is no
    // old TrackUnsubscribed event, but the visible receiver state is already
    // converged on the replacement and must stay that way.
    assert.equal(harness.shareTiles().length, 1);
    assert.equal(harness.shareTiles()[0]?.dataset.trackSid, 'new-track');
    assert.equal(
      (harness.shareTiles()[0]?.querySelector<HTMLVideoElement>('video')?.srcObject as MediaStream)
        .getTracks()[0],
      replacementTrack.mediaStreamTrack
    );
  } finally {
    harness.restore();
  }
});

test('queued callbacks from the replaced track cannot report against the replacement lifecycle', () => {
  const harness = createTilesHarness();
  try {
    const replacementTrack = harness.makeTrack('video-track-2');
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const video = harness.shareTiles()[0]!.querySelector<HTMLVideoElement>('video') as unknown as FakeElement;
    const oldFrameCallback = video.videoFrameCallbacks[0]!;

    harness.tiles.addShareTile('native-1', false, 'new-track', replacementTrack as never, 'Native', 42);
    const newFrameCallback = video.videoFrameCallbacks[1]!;

    video.dispatchEvent(new Event('loadeddata'));
    video.dispatchEvent(new Event('playing'));
    oldFrameCallback(0, {} as VideoFrameCallbackMetadata);
    newFrameCallback(0, {} as VideoFrameCallbackMetadata);

    assert.deepEqual(harness.firstDecoded, ['new-track']);
    assert.deepEqual(harness.firstPresented, ['new-track']);
  } finally {
    harness.restore();
  }
});

test('missing old unsubscribe leaves no stale SID route for stream-state events', () => {
  const harness = createTilesHarness();
  try {
    const replacementTrack = harness.makeTrack('video-track-2');
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    harness.tiles.addShareTile('native-1', false, 'new-track', replacementTrack as never, 'Native', 42);
    const stableTile = harness.shareTiles()[0]!;
    const oldPublication = {
      kind: 'video',
      trackSid: 'old-track',
      trackName: 'petal-window-42',
      track: { streamState: 'paused' },
    };
    const replacementPublication = {
      kind: 'video',
      trackSid: 'new-track',
      trackName: 'petal-window-42',
      track: { streamState: 'paused' },
    };
    const participant = {
      identity: 'native-1',
      trackPublications: new Map([['old-track', oldPublication]]),
    };

    harness.tiles.syncStreamStates({ remoteParticipants: new Map([['native-1', participant]]) } as never);
    assert.equal(stableTile.classList.contains('stream-paused'), false);

    harness.tiles.setPublicationPaused(participant as never, replacementPublication as never, true);
    assert.equal(stableTile.classList.contains('stream-paused'), true);
  } finally {
    harness.restore();
  }
});

test('same window id is isolated by participant identity during replacement', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'native-1-track', harness.track as never, 'Native 1', 42);
    const native1Tile = harness.shareTiles()[0]!;
    harness.tiles.addShareTile(
      'native-2',
      false,
      'native-2-track',
      harness.makeTrack('video-track-2') as never,
      'Native 2',
      42
    );

    assert.equal(harness.shareTiles().length, 2);
    assert.notEqual(harness.shareTiles().find((tile) => tile.dataset.owner === 'native-2'), native1Tile);
    assert.equal(native1Tile.dataset.trackSid, 'native-1-track');
  } finally {
    harness.restore();
  }
});

test('replacement SID resolves pause, stream-state, and color updates to the stable window tile', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    harness.tiles.addShareTile('native-1', false, 'new-track', harness.track as never, 'Native', 42);
    const stableTile = harness.shareTiles()[0]!;
    const replacementPublication = {
      kind: 'video',
      trackSid: 'new-track',
      trackName: 'petal-window-42',
      track: { streamState: 'paused', mediaStreamTrack: { label: 'Window 42' } },
    };
    const participant = {
      identity: 'native-1',
      name: 'Native',
      metadata: JSON.stringify({ petalWindowColorProfiles: { '42': { range: 'video' } } }),
      trackPublications: new Map([['new-track', replacementPublication]]),
    };

    harness.tiles.setPublicationPaused(participant as never, replacementPublication as never, true);
    assert.equal(stableTile.classList.contains('stream-paused'), true);

    harness.tiles.syncStreamStates({ remoteParticipants: new Map([['native-1', participant]]) } as never);
    assert.equal(stableTile.classList.contains('stream-paused'), true);

    harness.tiles.updateParticipantShareColorProfiles(participant as never);
    assert.equal(
      stableTile.querySelector<HTMLVideoElement>('video')?.classList.contains('video-range-source-video'),
      true
    );
  } finally {
    harness.restore();
  }
});

test('local camera-off tile centers the clean display name without folding in the you suffix', () => {
  const harness = createTilesHarness();
  try {
    harness.ctx.state.room!.localParticipant.name = 'C (you)';
    harness.tiles.ensureBaseTile('web-1', true);

    const tile = harness.baseTiles()[0]!;
    const initials = tile.querySelector<HTMLSpanElement>('.initials');
    const chip = tile.querySelector<HTMLDivElement>('.name-chip');
    const label = chip?.querySelector<HTMLSpanElement>('.name-chip-label');

    assert.equal(tile.classList.contains('camera-off'), true);
    assert.equal(initials?.textContent, 'C');
    assert.equal(label?.textContent, 'C (you)');
    assert.notEqual(initials?.textContent, 'C(');
  } finally {
    harness.restore();
  }
});

test('camera tile skips LiveKit attach when the video track is unchanged', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.setTileCamera('native-1', false, harness.track as never);
    assert.equal(harness.attachCount(), 1);

    harness.tiles.setTileCamera('native-1', false, harness.track as never);
    assert.equal(harness.attachCount(), 1);
  } finally {
    harness.restore();
  }
});

test('camera tile draw ids do not expose remote-control window ids', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.setTileCamera('native-1', false, harness.track as never, 0x8000_1234);
    const tile = harness.baseTiles()[0]!;

    assert.equal(tile.dataset.drawWindowId, String(0x8000_1234));
    assert.equal(tile.dataset.windowId, undefined);

    harness.tiles.clearTileCamera('native-1');
    assert.equal(tile.dataset.drawWindowId, undefined);
    assert.equal(tile.dataset.windowId, undefined);
  } finally {
    harness.restore();
  }
});

test('camera tile keeps placeholder visible until the first renderable video frame', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.setTileCamera('native-1', false, harness.track as never);
    const tile = harness.baseTiles()[0]!;
    const video = tile.querySelector<HTMLVideoElement>('video') as unknown as FakeElement;
    const initials = tile.querySelector<HTMLSpanElement>('.initials');

    assert.equal(tile.classList.contains('camera-off'), true);
    assert.equal(tile.classList.contains('camera-starting'), true);
    assert.equal(video.classList.contains('camera-video-ready'), false);
    assert.equal(initials?.classList.contains('hidden'), false);

    video.readyState = 2;
    video.videoWidth = 1280;
    video.videoHeight = 720;
    video.dispatchEvent(new Event('loadeddata'));

    assert.equal(tile.classList.contains('camera-off'), false);
    assert.equal(tile.classList.contains('camera-starting'), false);
    assert.equal(tile.classList.contains('camera-ready'), true);
    assert.equal(video.classList.contains('camera-video-ready'), true);
    assert.equal(initials?.classList.contains('hidden'), false);
  } finally {
    harness.restore();
  }
});

test('web tile grid reflow animates existing tiles when participant count changes', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.ensureBaseTile('web-1', true);
    const localTile = harness.baseTiles()[0] as unknown as FakeElement;
    assert.equal(localTile.animations.length, 0);

    harness.tiles.ensureBaseTile('native-1', false);

    assert.equal(localTile.animations.length, 1);
    assert.match(String(localTile.animations[0]?.[0]?.transform), /translate\(0px,\s*0px\) scale\(/);
  } finally {
    harness.restore();
  }
});

test('camera-off keeps the stable video layer but hides it behind the placeholder', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.setTileCamera('native-1', false, harness.track as never);
    const tile = harness.baseTiles()[0]!;
    const video = tile.querySelector<HTMLVideoElement>('video') as unknown as FakeElement;
    video.readyState = 2;
    video.videoWidth = 1280;
    video.videoHeight = 720;
    video.dispatchEvent(new Event('loadeddata'));

    harness.tiles.clearTileCamera('native-1');

    assert.equal(tile.querySelector('video'), video);
    assert.equal(video.srcObject, null);
    assert.equal(tile.classList.contains('camera-off'), true);
    assert.equal(tile.classList.contains('camera-ready'), false);
    assert.equal(video.classList.contains('camera-video-ready'), false);
    assert.equal(tile.querySelector('.initials')?.classList.contains('hidden'), false);
  } finally {
    harness.restore();
  }
});

test('share tile skips LiveKit attach when the video track is unchanged', () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'window-track', harness.track as never, 'Native', 42);
    assert.equal(harness.attachCount(), 1);

    harness.tiles.addShareTile('native-1', false, 'window-track', harness.track as never, 'Native', 42);
    assert.equal(harness.attachCount(), 1);
  } finally {
    harness.restore();
  }
});

test('remote control release is delayed only until share-removal grace expires', async () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const oldTile = harness.shareTiles()[0]!;
    harness.ctx.state.activeRemoteControl = {
      tileId: oldTile.id,
      targetUserId: 'native-1',
      windowId: 42,
      pointerId: null,
      grantToken: null,
    };

    harness.tiles.removeShareTile('native-1', 'old-track');
    assert.deepEqual(harness.stopReasons, []);

    await wait(30);
    assert.deepEqual(harness.stopReasons, ['share ended']);
    assert.deepEqual(harness.toasts, ['Remote control ended because the shared window disappeared']);
  } finally {
    harness.restore();
  }
});

test('own-room disconnect still releases active remote control immediately', async () => {
  const harness = createTilesHarness();
  try {
    // No share tile for this identity at all: `removeParticipantTiles` has
    // nothing to grace-suspend, so it must still fall back to the immediate
    // release it always had -- this is a DIFFERENT code path from the
    // room-level disconnect asserted below, but exercises the same fallback.
    harness.ctx.state.activeRemoteControl = {
      tileId: 'nonexistent-tile',
      targetUserId: 'native-1',
      windowId: 42,
      pointerId: null,
      grantToken: null,
    };
    harness.tiles.removeParticipantTiles('native-1');
    assert.deepEqual(harness.stopReasons, ['participant left']);
  } finally {
    harness.restore();
  }

  // The controller's OWN room connection dying (`RoomEvent.Disconnected`) is
  // not a remote-participant signal at all -- there is no room left to grace-
  // confirm against, so this stays immediate and untouched by #820.
  const connection = readFileSync(new URL('../src/connection.ts', import.meta.url), 'utf8');
  assert.match(connection, /cb\.stopRemoteControl\('disconnected'\)/);
});

// #820 (web-harness twin of e0cf46bc): `ParticipantDisconnected` for the
// controlled identity used to kill `state.activeRemoteControl` immediately on
// event order alone. A resume aftershock produces a stale disconnect here
// too; symptom was the controller's next click going out tokenless
// ("dropping tokenless input ... no-active-request" host-side).
test('participant disconnect grace-confirms remote control instead of killing it immediately (#820)', async () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const oldTile = harness.shareTiles()[0]!;
    harness.ctx.state.activeRemoteControl = {
      tileId: oldTile.id,
      targetUserId: 'native-1',
      windowId: 42,
      pointerId: null,
      grantToken: null,
    };

    // Models the real LiveKit ordering the code comments document: an old
    // TrackUnsubscribed (-> `removeShareTile`) arrives before the
    // ParticipantDisconnected aftershock (-> `removeParticipantTiles`).
    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.removeParticipantTiles('native-1');

    // The grant must survive the disconnect event itself -- no immediate
    // stop, no cleared session.
    assert.deepEqual(harness.stopReasons, []);
    assert.equal(harness.ctx.state.activeRemoteControl?.targetUserId, 'native-1');

    await wait(30);
    // A genuine, un-rebound departure still ends control -- just after the
    // same share-removal grace window the disconnect-free case uses (#820's
    // "revoked, just up to the grace window later" posture), not instantly.
    assert.deepEqual(harness.stopReasons, ['share ended']);
  } finally {
    harness.restore();
  }
});

test('participant reconnecting within the grace window restores remote control with no gap (#820)', async () => {
  const harness = createTilesHarness();
  try {
    harness.tiles.addShareTile('native-1', false, 'old-track', harness.track as never, 'Native', 42);
    const oldTile = harness.shareTiles()[0]!;
    harness.ctx.state.activeRemoteControl = {
      tileId: oldTile.id,
      targetUserId: 'native-1',
      windowId: 42,
      pointerId: null,
      grantToken: 'grant-1',
    };

    harness.tiles.removeShareTile('native-1', 'old-track');
    harness.tiles.removeParticipantTiles('native-1');
    assert.deepEqual(harness.stopReasons, []);

    // The stale aftershock resolves: the same window's share reappears
    // (TrackSubscribed for the replacement publication) before the grace
    // window elapses.
    harness.tiles.addShareTile(
      'native-1',
      false,
      'new-track',
      harness.makeTrack('video-track-2') as never,
      'Native',
      42
    );

    await wait(30);
    // Never stopped, and the ORIGINAL grant token is untouched -- this is a
    // rebind, not a fresh grant.
    assert.deepEqual(harness.stopReasons, []);
    assert.equal(harness.ctx.state.activeRemoteControl?.grantToken, 'grant-1');
    assert.equal(harness.ctx.state.activeRemoteControl?.targetUserId, 'native-1');
  } finally {
    harness.restore();
  }
});
