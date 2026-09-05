import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createRemoteWindowHeader, isIdleKeepaliveFps, latestCaptureStateFor } from '../src/remoteWindowHeader.ts';
import type { ActiveRemoteControl, HarnessContext } from '../src/context.ts';
import type { CaptureStateReport, PipelineStatsMessage } from '../src/trackNames.ts';
import type { AiChatSessionState } from '../src/aiChat.ts';

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

class FakeStyle {
  readonly values = new Map<string, string>();

  setProperty(name: string, value: string) {
    this.values.set(name, value);
  }

  getPropertyValue(name: string): string {
    return this.values.get(name) ?? '';
  }
}

class FakeElement {
  id = '';
  className = '';
  textContent = '';
  innerHTML = '';
  title = '';
  type = '';
  hidden = false;
  disabled = false;
  videoWidth = 0;
  videoHeight = 0;
  isConnected = true;
  /** Only meaningful for `<input>`, but harmless elsewhere -- matches
   * HTMLInputElement.value defaulting to '', which the fake DOM otherwise
   * leaves undefined and every `.value.trim()` call would throw on. */
  value = '';
  maxLength = -1;
  dataset: Record<string, string | undefined> = {};
  readonly attributes = new Map<string, string>();
  readonly children: FakeElement[] = [];
  readonly classList = new FakeClassList(this);
  readonly style = new FakeStyle();
  readonly listeners = new Map<string, Listener[]>();
  parentElement: FakeElement | null = null;
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

  remove() {
    const parent = this.parentElement;
    if (!parent) return;
    const index = parent.children.indexOf(this);
    if (index !== -1) parent.children.splice(index, 1);
    this.parentElement = null;
    this.isConnected = false;
  }

  contains(element: FakeElement): boolean {
    if (element === this) return true;
    return this.children.some((child) => child.contains(element));
  }

  addEventListener(type: string, listener: Listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  click() {
    if (this.disabled) return;
    this.dispatch('click');
  }

  dispatch(type: string) {
    const event = {
      preventDefault() {},
      stopPropagation() {},
    } as Event;
    for (const listener of this.listeners.get(type) ?? []) {
      if (typeof listener === 'function') listener.call(this, event);
      else listener.handleEvent(event);
    }
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
    if (name === 'id') this.id = value;
    if (name === 'class') this.className = value;
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
}

class FakeDocument {
  readonly body = new FakeElement('body');
  readonly listeners = new Map<string, Listener[]>();

  createElement(tagName: string): FakeElement {
    return new FakeElement(tagName.toLowerCase());
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    return this.body.querySelectorAll(selector);
  }

  addEventListener(type: string, listener: Listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: Listener) {
    const listeners = this.listeners.get(type) ?? [];
    this.listeners.set(type, listeners.filter((candidate) => candidate !== listener));
  }

  dispatch(type: string, target: FakeElement) {
    const event = { target } as unknown as Event;
    for (const listener of this.listeners.get(type) ?? []) {
      if (typeof listener === 'function') listener.call(this, event);
      else listener.handleEvent(event);
    }
  }
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  if (selector.startsWith('.')) return element.classList.contains(selector.slice(1));
  const aria = /^\[aria-label="(.+)"\]$/.exec(selector);
  if (aria) return element.getAttribute('aria-label') === aria[1];
  return element.tagName === selector.toLowerCase();
}

function elementText(element: FakeElement | null): string {
  if (!element) return '';
  return `${element.textContent}${element.children.map(elementText).join('')}`;
}

function installFakeDom() {
  const originalDocument = globalThis.document;
  const originalHtmlDivElement = globalThis.HTMLDivElement;
  const originalMutationObserver = globalThis.MutationObserver;
  const document = new FakeDocument();
  Object.defineProperty(globalThis, 'document', { configurable: true, value: document });
  Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: FakeElement });
  Object.defineProperty(globalThis, 'MutationObserver', { configurable: true, value: undefined });
  return {
    document,
    restore: () => {
      if (originalDocument === undefined) Reflect.deleteProperty(globalThis, 'document');
      else Object.defineProperty(globalThis, 'document', { configurable: true, value: originalDocument });
      if (originalHtmlDivElement === undefined) Reflect.deleteProperty(globalThis, 'HTMLDivElement');
      else Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: originalHtmlDivElement });
      if (originalMutationObserver === undefined) Reflect.deleteProperty(globalThis, 'MutationObserver');
      else Object.defineProperty(globalThis, 'MutationObserver', { configurable: true, value: originalMutationObserver });
    },
  };
}

function createHarness(
  overrides: Partial<Parameters<typeof createRemoteWindowHeader>[0]> = {},
  // Extra ctx callbacks. The header treats an absent callback as "that feature
  // is not wired here" (see aiChatAvailable), so a test that wants a feature
  // has to supply it explicitly rather than get it by default.
  cbOverrides: Record<string, unknown> = {},
) {
  const fakeDom = installFakeDom();
  const tilesEl = fakeDom.document.createElement('div') as unknown as HTMLDivElement;
  const tile = fakeDom.document.createElement('div') as unknown as HTMLDivElement;
  const video = fakeDom.document.createElement('video') as unknown as HTMLVideoElement;
  (tile as unknown as FakeElement).id = 'tile-s-native-1-track';
  (tile as unknown as FakeElement).className = 'tile share-tile';
  (tile as unknown as FakeElement).dataset.owner = 'native-1';
  (tile as unknown as FakeElement).dataset.windowId = '42';
  (tilesEl as unknown as FakeElement).appendChild(tile as unknown as FakeElement);

  const state: {
    room: { localParticipant: { identity: string }; remoteParticipants: Map<string, unknown> };
    activeRemoteControl: ActiveRemoteControl | null;
  } = {
    room: {
      localParticipant: { identity: 'web-1' },
      remoteParticipants: new Map([['native-1', {}]]),
    },
    activeRemoteControl: null,
  };

  const ctx = {
    dom: { tilesEl },
    state,
    cb: {
      setDrawMode: (on: boolean) => {
        (tilesEl as unknown as FakeElement).classList.toggle('draw-mode-active', on);
      },
      startRemoteControl: (target: HTMLDivElement) => {
        state.activeRemoteControl = {
          tileId: target.id,
          targetUserId: 'native-1',
          windowId: 42,
          pointerId: null,
          grantToken: null,
        };
      },
      stopRemoteControl: () => {
        state.activeRemoteControl = null;
      },
      activeRemoteControlForTile: (target: HTMLDivElement) =>
        state.activeRemoteControl?.tileId === target.id ? state.activeRemoteControl : null,
      ...cbOverrides,
    },
  } as unknown as HarnessContext;

  const track = {
    mediaStreamTrack: { label: 'Example & Specs — Chrome' },
  };
  (video as unknown as { getVideoPlaybackQuality: () => { totalVideoFrames: number } }).getVideoPlaybackQuality = () => ({
    totalVideoFrames: 1,
  });
  const controller = createRemoteWindowHeader({
    ctx,
    tile,
    ownerIdentity: 'native-1',
    ownerName: 'Ada',
    isLocal: false,
    track: track as never,
    video,
    windowId: 42,
    sourceUrl: 'https://example.com/spec',
    autoHide: false,
    ...overrides,
  });
  const root = (tile as unknown as FakeElement).querySelector('.remote-window-header') as unknown as FakeElement;

  return {
    controller,
    ctx,
    fakeDom,
    root,
    tile: tile as unknown as FakeElement,
    restore: fakeDom.restore,
  };
}

function buttonByAria(root: FakeElement, label: string): FakeElement {
  const button = root.querySelector(`[aria-label="${label}"]`) as unknown as FakeElement | null;
  assert.ok(button, `missing button ${label}`);
  return button;
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test('remote window header uses web-appropriate shared-window actions', () => {
  const harness = createHarness();
  try {
    assert.ok(harness.root);
    assert.equal(harness.root.querySelector('.remote-window-header__accent'), null);
    assert.equal(harness.root.querySelectorAll('.remote-window-header__traffic-dot').length, 0);
    assert.equal(harness.root.querySelectorAll('.remote-window-header__window-actions').length, 1);
    assert.equal(harness.root.querySelectorAll('.remote-window-header__icon-control').length, 1);
    const sizeToggle = buttonByAria(harness.root, 'Expand remote window');
    assert.ok(sizeToggle.classList.contains('control-button'));
    assert.equal(sizeToggle.getAttribute('aria-expanded'), 'false');
    // The source-app avatar/logo was removed per user request.
    assert.equal(harness.root.querySelector('.remote-window-header__avatar'), null);
    assert.equal(elementText(harness.root.querySelector('.remote-window-header__source-label') as unknown as FakeElement), 'Chrome');
    // Leading space preserved so it renders "Chrome by Ada", not "Chromeby Ada".
    assert.equal(elementText(harness.root.querySelector('.remote-window-header__owner-label') as unknown as FakeElement), ' by Ada');
    assert.equal(harness.root.querySelectorAll('.remote-window-header__active-indicator').length, 1);
    assert.equal(harness.root.querySelectorAll('.remote-window-header__segment').length, 3);
    assert.deepEqual(
      harness.root
        .querySelectorAll('.remote-window-header__button-label')
        .map((element) => elementText(element as unknown as FakeElement)),
      [
        'Debug',
        'Open URL',
        // #657: asks the window's OWNER to start a session -- this client
        // never hosts one.
        'AI chat',
        'View',
        'Control',
        'Draw',
        'View shared window',
        'Request remote control',
        'Draw on shared window',
      ],
    );
    assert.equal(harness.root.dataset.mode, 'view');
    assert.equal(harness.root.style.getPropertyValue('--active-mode-index'), '0');
    assert.equal(harness.root.style.getPropertyValue('--identity-header-bg'), '#6e8bff');
    assert.equal(harness.root.style.getPropertyValue('--identity-header-ink'), '#081129');
    assert.equal(buttonByAria(harness.root, 'Open URL').disabled, false);
    assert.match(buttonByAria(harness.root, 'Show debug stats').title, /^live · updated 0s ago$/);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('remote window header size toggle follows actual tile state and fires the next action', () => {
  const calls: string[] = [];
  const harness = createHarness({
    onMinimizeWindow: () => {
      calls.push('minimize');
      harness.tile.classList.remove('is-spotlight');
    },
    onExpandWindow: () => {
      calls.push('expand');
      harness.tile.classList.add('is-spotlight');
    },
    onOpenSourceUrl: (url) => calls.push(`open:${url}`),
  });
  try {
    buttonByAria(harness.root, 'Expand remote window').click();
    assert.equal(buttonByAria(harness.root, 'Minimize remote window').getAttribute('aria-expanded'), 'true');
    buttonByAria(harness.root, 'Minimize remote window').click();
    buttonByAria(harness.root, 'Open URL').click();

    assert.deepEqual(calls, ['expand', 'minimize', 'open:https://example.com/spec']);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('remote window header size toggle resyncs after an external layout change', () => {
  const harness = createHarness({
    onMinimizeWindow: () => {},
    onExpandWindow: () => {},
  });
  try {
    harness.tile.classList.add('is-spotlight');
    harness.controller.syncMode();

    const minimize = buttonByAria(harness.root, 'Minimize remote window');
    assert.equal(minimize.title, 'Minimize remote window');
    assert.equal(minimize.getAttribute('aria-expanded'), 'true');
    assert.match(
      (minimize.querySelector('.remote-window-header__icon') as unknown as FakeElement).innerHTML,
      /M5 12h14/,
    );

    harness.tile.classList.remove('is-spotlight');
    harness.controller.syncMode();

    const expand = buttonByAria(harness.root, 'Expand remote window');
    assert.equal(expand.title, 'Expand remote window');
    assert.equal(expand.getAttribute('aria-expanded'), 'false');
    assert.match(
      (expand.querySelector('.remote-window-header__icon') as unknown as FakeElement).innerHTML,
      /M8 3H3v5/,
    );

    const source = readFileSync(new URL('../src/remoteWindowHeader.ts', import.meta.url), 'utf8');
    assert.match(source, /REMOTE_CONTROL_STATUS_DATA_ATTRS = \[[\s\S]*'class'/);
    assert.match(
      source,
      /observer\.observe\(current\.tile, \{ attributes: true, attributeFilter: REMOTE_CONTROL_STATUS_DATA_ATTRS \}\)/,
    );
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('remote window header shows requesting-control state before settling active', async () => {
  const harness = createHarness();
  try {
    buttonByAria(harness.root, 'Request remote control').click();

    assert.equal(harness.ctx.state.activeRemoteControl?.tileId, 'tile-s-native-1-track');
    assert.equal(harness.root.dataset.mode, 'control');
    assert.equal(harness.root.style.getPropertyValue('--active-mode-index'), '1');
    const status = harness.root.querySelector('.remote-window-header__status-chip') as unknown as FakeElement;
    assert.equal(status.hidden, false);
    assert.equal(elementText(status), 'Requesting control');
    const control = buttonByAria(harness.root, 'Requesting control');
    assert.equal(control.disabled, true);
    assert.equal(control.classList.contains('requesting'), true);

    await wait(470);
    assert.equal(status.hidden, true);
    assert.equal(harness.root.dataset.mode, 'control');
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('#497: transient target failure keeps amber warning feedback', () => {
  const harness = createHarness();
  try {
    harness.tile.dataset.remoteControlStatus = 'targetUnavailable';
    harness.tile.dataset.remoteControlStatusMessage = 'Pointer injection failed on the shared Mac.';
    harness.controller.syncMode();

    const status = harness.root.querySelector('.remote-window-header__status-chip') as unknown as FakeElement;
    assert.equal(status.hidden, false);
    assert.equal(status.classList.contains('warning'), true);
    assert.equal(elementText(status), 'Unavailable');
    assert.equal(status.title, 'Pointer injection failed on the shared Mac.');
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('operation refusal uses concise controller feedback', () => {
  const harness = createHarness();
  try {
    harness.tile.dataset.remoteControlStatus = 'occluded';
    harness.tile.dataset.remoteControlStatusMessage = 'Remote input was ignored because the target point is covered.';
    harness.controller.syncMode();

    const status = harness.root.querySelector('.remote-window-header__status-chip') as unknown as FakeElement;
    assert.equal(status.hidden, false);
    assert.equal(status.classList.contains('warning'), true);
    assert.equal(elementText(status), 'Covered');
    assert.match(status.title, /ignored/);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('#497: structural request unavailability uses the neutral paused treatment', () => {
  const harness = createHarness();
  try {
    harness.tile.dataset.remoteControlStatus = 'requestUnavailable';
    harness.tile.dataset.remoteControlStatusMessage = 'This window is not being shared.';
    harness.controller.syncMode();

    const status = harness.root.querySelector('.remote-window-header__status-chip') as unknown as FakeElement;
    assert.equal(status.hidden, false);
    assert.equal(status.classList.contains('warning'), false);
    assert.equal(status.classList.contains('paused'), true);
    assert.equal(elementText(status), 'Unavailable');
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('#497: small-tile overflow menu keeps full labels and invokes all three modes', () => {
  const harness = createHarness();
  try {
    const overflow = buttonByAria(harness.root, 'More remote window modes');
    const menu = harness.root.querySelector('.remote-window-header__overflow-menu') as unknown as FakeElement;
    const items = menu.querySelectorAll('.remote-window-header__overflow-item') as unknown as FakeElement[];

    overflow.click();
    assert.equal(menu.hidden, false);
    assert.equal(overflow.getAttribute('aria-expanded'), 'true');
    assert.equal(harness.tile.classList.contains('remote-window-menu-open'), true);
    assert.deepEqual(items.map(elementText), [
      'View shared window',
      'Request remote control',
      'Draw on shared window',
    ]);

    items[2].click();
    assert.equal(harness.root.dataset.mode, 'draw');
    assert.equal(menu.hidden, true);
    assert.equal(harness.tile.classList.contains('remote-window-menu-open'), false);

    overflow.click();
    items[0].click();
    assert.equal(harness.root.dataset.mode, 'view');

    overflow.click();
    items[1].click();
    assert.equal(harness.root.dataset.mode, 'control');
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('#497: small-tile overflow menu escapes tile clipping and closes on outside click', () => {
  const harness = createHarness();
  try {
    const overflow = buttonByAria(harness.root, 'More remote window modes');
    const menu = harness.root.querySelector('.remote-window-header__overflow-menu') as unknown as FakeElement;
    const outside = harness.fakeDom.document.createElement('button');

    overflow.click();
    harness.fakeDom.document.dispatch('pointerdown', outside);

    assert.equal(menu.hidden, true);
    assert.equal(overflow.getAttribute('aria-expanded'), 'false');
    assert.equal(harness.tile.classList.contains('remote-window-menu-open'), false);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('remote window header CSS pins native silhouette and responsive contract', () => {
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
  const header = /\.remote-window-header\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';
  const switcher = /\.remote-window-header__mode-switcher\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';
  const indicator = /\.remote-window-header__active-indicator\s*\{(?<body>[^}]+)\}/.exec(css)?.groups?.body ?? '';

  assert.match(header, /top\s*:\s*0/i);
  assert.match(header, /height\s*:\s*44px/i);
  assert.match(header, /background\s*:\s*var\(--identity-header-bg/i);
  assert.match(css, /--identity-header-ink/);
  assert.doesNotMatch(css, /\.remote-window-header__accent/);
  assert.doesNotMatch(css, /traffic-(?:lights|dot|close|hide|fit)/i);
  assert.doesNotMatch(css, /#(?:ff5f57|febc2e|28c840)/i);
  // Full-label switcher remains the wide-window control; #497 swaps the
  // whole component for an overflow menu at the compact breakpoint.
  assert.match(switcher, /--segment-width\s*:\s*86px/i);
  assert.match(indicator, /width\s*:\s*var\(--segment-width\)/i);
  assert.match(indicator, /transform\s*:\s*translateX\(calc\(var\(--active-mode-index\) \* var\(--segment-width\)\)\)/i);

  for (const width of [720, 640, 560, 470, 300]) {
    assert.match(css, new RegExp(`@container\\s*\\(max-width:\\s*${width}px\\)`, 'i'));
    assert.match(css, new RegExp(`@media\\s*\\(max-width:\\s*${width}px\\)`, 'i'));
  }
});

test('#497: small tiles replace the segmented switcher with a full-label overflow menu', () => {
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

  for (const rule of ['@container', '@media']) {
    const narrow = new RegExp(
      `${rule} \\(max-width: 470px\\) \\{([\\s\\S]*?)\\n\\}`
    ).exec(css)?.[1] ?? '';
    assert.match(narrow, /\.remote-window-header__mode-switcher\s*\{\s*display:\s*none;/);
    assert.match(narrow, /\.remote-window-header__overflow-button\s*\{\s*display:\s*inline-flex;/);
  }
  assert.match(css, /\.remote-window-header__overflow-item\s*\{[\s\S]*white-space:\s*normal/);
  assert.match(css, /\.remote-window-header__overflow-menu\s*\{[\s\S]*width:\s*214px/);
  assert.match(css, /max-width:\s*calc\(100cqw - 8px\)/);
  assert.match(css, /\.tile\.remote-window-menu-open\s*\{[\s\S]*overflow:\s*visible/);
});

test('#376 item 2: unavailable control reads as transient "preparing", not a flat dead end', () => {
  const source = readFileSync(new URL('../src/remoteWindowHeader.ts', import.meta.url), 'utf8');
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

  assert.match(source, /const preparing = !canControl && !current\.isLocal;/);
  assert.match(source, /Preparing remote control/);
  assert.doesNotMatch(source, /Remote control unavailable for this window/);
  assert.match(source, /controlButton\.classList\.toggle\('preparing', preparing\)/);
  assert.match(css, /\.remote-window-header__segment\.preparing\s*\{[\s\S]*animation:\s*remote-window-header-preparing-pulse/);
});

test('#376 item 3: focus-loss during an active control session shows a resume cue', () => {
  const source = readFileSync(new URL('../src/remoteControlUi.ts', import.meta.url), 'utf8');
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

  assert.match(source, /tile\.addEventListener\('focusin', \(\) => setFocusHintVisible\(tile, false\)\)/);
  assert.match(source, /tile\.addEventListener\('focusout', \(\) => \{[\s\S]*setFocusHintVisible\(tile, true\)/);
  assert.match(source, /textContent = 'Click to resume control'/);
  // Passive cue: must not block the click that's supposed to refocus.
  assert.match(css, /\.remote-control-focus-hint\s*\{[\s\S]*pointer-events:\s*none;/);
});

test('remote window header reserves 44px above the video instead of overlaying it', () => {
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
  const reserved =
    // Selector list is allowed to grow (canvas.share-hold-canvas joined it for
    // #627) -- what must not change is that the video and every overlay stacked
    // on it get the SAME 44px reservation. `[^{}]*` keeps this from matching
    // across a rule boundary.
    /\.tile\.has-remote-window-header video,[^{}]*canvas\.full-range-canvas[^{}]*\{(?<body>[^}]+)\}/.exec(
      css,
    )?.groups?.body ?? '';
  assert.match(reserved, /top\s*:\s*44px/i);
  assert.match(reserved, /height\s*:\s*calc\(100%\s*-\s*44px\)/i);
});

test('#466: spotlight rail hides only inactive headers and restores the video/chip layout', () => {
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
  const remoteControlUi = readFileSync(new URL('../src/remoteControlUi.ts', import.meta.url), 'utf8');
  const railRule =
    /\.tiles\.layout-spotlight \.tile:not\(\.is-spotlight\):not\(\.remote-control-active\)\.has-remote-window-header[\s\S]*?\.remote-window-header\s*\{(?<body>[^}]+)\}/.exec(
      css,
    )?.groups?.body ?? '';
  const railVideoRule =
    /\.tiles\.layout-spotlight\s+\.tile:not\(\.is-spotlight\):not\(\.remote-control-active\)\.has-remote-window-header video,[^{}]*\.full-range-canvas[^{}]*\{(?<body>[^}]+)\}/.exec(
      css,
    )?.groups?.body ?? '';

  assert.match(railRule, /display\s*:\s*none/i);
  assert.match(css, /\.tiles\.layout-spotlight \.tile:not\(\.is-spotlight\):not\(\.remote-control-active\)\.has-remote-window-header[\s\S]*?\.name-chip\s*\{[\s\S]*?display\s*:\s*flex/i);
  assert.match(railVideoRule, /top\s*:\s*0/i);
  assert.match(railVideoRule, /height\s*:\s*100%/i);
  assert.match(css, /not\(\.remote-control-active\)/);
  assert.match(remoteControlUi, /!active[\s\S]*has-remote-window-header[\s\S]*fitTileLabels\(tile\)/);
});

function pipelineMessage(overrides: Partial<PipelineStatsMessage> = {}): PipelineStatsMessage {
  return {
    v: 1,
    role: 'sender',
    reporterId: 'native-1',
    ownerIdentity: 'native-1',
    windowId: 42,
    seq: 1,
    sentAtMs: Date.now(),
    grabbed: null,
    encodedSent: null,
    received: null,
    decoded: null,
    captureState: null,
    receiverFreeze: null,
    ...overrides,
  };
}

function captureState(state: CaptureStateReport['state']): CaptureStateReport {
  return { state, fps: null, dirtyRectCount: null, dirtyAreaPx: null, occlusionPct: null, cpu: { lockCopyMs: null, convertMs: null, captureFrameReturnMs: null } };
}

function ctxWithReceived(
  received: Array<{ message: PipelineStatsMessage; senderIdentity?: string; receivedAt: number }>,
): HarnessContext {
  return {
    hook: {
      pipelineStats: {
        metrics: () => ({ sent: [], received }),
        resetMetrics: () => {},
        publish: async () => [],
      },
    },
  } as unknown as HarnessContext;
}

test('latestCaptureStateFor matches on reporter identity and window id, ignoring other windows/owners', () => {
  const ctx = ctxWithReceived([
    { message: pipelineMessage({ windowId: 7, captureState: captureState('live') }), receivedAt: 1000 },
    { message: pipelineMessage({ reporterId: 'someone-else', captureState: captureState('idle') }), receivedAt: 1000 },
    { message: pipelineMessage({ captureState: captureState('occluded') }), receivedAt: 1000 },
  ]);
  assert.equal(latestCaptureStateFor(ctx, 'native-1', 42)?.state, 'occluded');
  assert.equal(latestCaptureStateFor(ctx, 'native-1', 7)?.state, 'live');
  assert.equal(latestCaptureStateFor(ctx, 'native-1', 999), null);
});

test('latestCaptureStateFor picks the most recently received report for that track', () => {
  const ctx = ctxWithReceived([
    { message: pipelineMessage({ captureState: captureState('live') }), receivedAt: 1000 },
    { message: pipelineMessage({ captureState: captureState('idle') }), receivedAt: 5000 },
  ]);
  assert.equal(latestCaptureStateFor(ctx, 'native-1', 42)?.state, 'idle');
});

test('latestCaptureStateFor returns null without a pipelineStats API (no cross-peer signal yet)', () => {
  const ctx = { hook: { pipelineStats: null } } as unknown as HarnessContext;
  assert.equal(latestCaptureStateFor(ctx, 'native-1', 42), null);
});

test('isIdleKeepaliveFps only flags a positive fps reading during a reported idle capture state', () => {
  const idleCtx = ctxWithReceived([{ message: pipelineMessage({ captureState: captureState('idle') }), receivedAt: 1000 }]);
  const liveCtx = ctxWithReceived([{ message: pipelineMessage({ captureState: captureState('live') }), receivedAt: 1000 }]);

  // The reported bug: fps reads ~1 while the sender says content is idle.
  assert.equal(isIdleKeepaliveFps(idleCtx, 'native-1', 42, 1), true);
  // Genuinely live content at any fps is never mislabeled as keepalive.
  assert.equal(isIdleKeepaliveFps(liveCtx, 'native-1', 42, 1), false);
  // No activity at all (fps 0/null) isn't "keepalive" -- nothing to explain.
  assert.equal(isIdleKeepaliveFps(idleCtx, 'native-1', 42, 0), false);
  assert.equal(isIdleKeepaliveFps(idleCtx, 'native-1', 42, null), false);
});

// ---------------------------------------------------------------------------
// AI chat (#657). These drive the REAL header/panel event chain -- the button
// click, the data-channel-driven refresh, and the pointer gesture -- not the
// pure helpers underneath. The bugs in this class live in the wiring.
// ---------------------------------------------------------------------------

interface AiChatHarnessRig {
  sessions: Map<string, AiChatSessionState>;
  calls: string[];
  notify: () => void;
  cb: Record<string, unknown>;
  setHeld: (value: boolean) => void;
}

function aiChatRig(): AiChatHarnessRig {
  const sessions = new Map<string, AiChatSessionState>();
  const calls: string[] = [];
  const listeners = new Set<() => void>();
  let held = false;
  return {
    sessions,
    calls,
    notify: () => listeners.forEach((listener) => listener()),
    setHeld: (value: boolean) => {
      held = value;
    },
    cb: {
      aiChatSessionFor: (windowId: number, owner: string) => sessions.get(`${owner}:${windowId}`) ?? null,
      startAiChat: (windowId: number, owner: string) => calls.push(`start ${owner}/${windowId}`),
      stopAiChat: (windowId: number, owner: string) => calls.push(`stop ${owner}/${windowId}`),
      aiChatPttStart: (windowId: number, owner: string) => {
        held = true;
        calls.push(`pttStart ${owner}/${windowId}`);
      },
      aiChatPttEnd: (windowId: number, owner: string) => {
        held = false;
        calls.push(`pttEnd ${owner}/${windowId}`);
      },
      aiChatLocalPttHeld: () => held,
      onAiChatChange: (listener: () => void) => {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
    },
  };
}

function liveSession(overrides: Partial<AiChatSessionState> = {}): AiChatSessionState {
  return {
    windowId: 42,
    ownerIdentity: 'native-1',
    active: true,
    startedBy: 'web-1',
    secondsLeft: 240,
    activeSpeaker: null,
    error: null,
    lastStateAtMs: 0,
    turns: [],
    ...overrides,
  };
}

test('the AI chat button asks the owner and never claims to host the session itself', () => {
  const rig = aiChatRig();
  const harness = createHarness({}, rig.cb);
  try {
    const button = buttonByAria(harness.root, 'Start AI chat on this window');
    assert.equal(button.disabled, false);
    assert.equal(button.classList.contains('is-hidden'), false);

    button.click();
    assert.deepEqual(rig.calls, ['start native-1/42']);
    // No local session state is invented by clicking: the owner answers with
    // `state`, and until it does there is nothing to show.
    assert.equal(harness.tile.querySelector('.ai-chat-panel'), null);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('AI chat is not offered for your own shared window', () => {
  // This browser client has neither the window pixels nor its accessibility
  // tree, so it can never host a session for a window it is sharing.
  const rig = aiChatRig();
  const harness = createHarness({ isLocal: true }, rig.cb);
  try {
    const button = buttonByAria(harness.root, 'Start AI chat on this window');
    assert.equal(button.classList.contains('is-hidden'), true);
    assert.equal(button.disabled, true);
    button.click();
    assert.deepEqual(rig.calls, []);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('an owner state message brings up the disclosure panel and flips the button to stop', () => {
  const rig = aiChatRig();
  const harness = createHarness({}, rig.cb);
  try {
    rig.sessions.set('native-1:42', liveSession());
    rig.notify();

    const panel = harness.tile.querySelector('.ai-chat-panel') as unknown as FakeElement | null;
    assert.ok(panel, 'a live session must show the panel on the tile');
    assert.equal(elementText(panel.querySelector('.ai-chat-panel__badge') as unknown as FakeElement), 'AI chat live');
    assert.equal(elementText(panel.querySelector('.ai-chat-panel__countdown') as unknown as FakeElement), '4:00');
    const disclosure = panel.querySelector('.ai-chat-panel__disclosure') as unknown as FakeElement;
    assert.equal((disclosure as unknown as { hidden: boolean }).hidden, false);
    assert.match(elementText(disclosure), /window/i);
    assert.match(elementText(disclosure), /voice/i);

    const stopButton = buttonByAria(harness.root, 'Stop AI chat');
    assert.equal(elementText(stopButton.querySelector('.remote-window-header__button-label') as unknown as FakeElement), 'Stop AI chat');
    stopButton.click();
    assert.deepEqual(rig.calls, ['stop native-1/42']);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('holding the panel button claims the floor and releasing it lets go', () => {
  const rig = aiChatRig();
  const harness = createHarness({}, rig.cb);
  try {
    rig.sessions.set('native-1:42', liveSession());
    rig.notify();
    const ptt = harness.tile.querySelector('.ai-chat-panel__ptt') as unknown as FakeElement;
    assert.ok(ptt);

    ptt.dispatch('pointerdown');
    assert.deepEqual(rig.calls, ['pttStart native-1/42']);
    ptt.dispatch('pointerup');
    assert.deepEqual(rig.calls, ['pttStart native-1/42', 'pttEnd native-1/42']);
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('a stale/expired session leaves no phantom AI badge behind', () => {
  // Staleness expiry deletes the session outright; the header must follow it
  // down, or a crashed host leaves an "AI chat live" badge for the meeting.
  const rig = aiChatRig();
  const harness = createHarness({}, rig.cb);
  try {
    rig.sessions.set('native-1:42', liveSession());
    rig.notify();
    assert.ok(harness.tile.querySelector('.ai-chat-panel'));

    rig.sessions.delete('native-1:42');
    rig.notify();
    assert.equal(harness.tile.querySelector('.ai-chat-panel'), null);
    assert.equal(elementText(buttonByAria(harness.root, 'Start AI chat on this window')), 'AI chat');
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});

test('destroying the header while the floor is held releases it', () => {
  const rig = aiChatRig();
  const harness = createHarness({}, rig.cb);
  try {
    rig.sessions.set('native-1:42', liveSession());
    rig.notify();
    (harness.tile.querySelector('.ai-chat-panel__ptt') as unknown as FakeElement).dispatch('pointerdown');
    rig.calls.length = 0;

    harness.controller.destroy();
    // Once from the panel's own teardown, once from the header's scoped
    // backstop -- both idempotent on the controller side, and both scoped to
    // THIS window so another tile's turn is never cut short.
    assert.ok(rig.calls.every((call) => call === 'pttEnd native-1/42'), rig.calls.join(','));
    assert.ok(rig.calls.length >= 1);
    assert.equal(harness.tile.querySelector('.ai-chat-panel'), null);
  } finally {
    harness.restore();
  }
});

test('an owner refusal renders the shared copy on the control and the panel', () => {
  const rig = aiChatRig();
  const harness = createHarness({}, rig.cb);
  try {
    rig.sessions.set('native-1:42', liveSession({ active: false, secondsLeft: null, error: 'busy' }));
    rig.notify();

    const button = buttonByAria(harness.root, 'Start AI chat on this window');
    assert.equal(button.title, 'An AI chat is already running for this window.');
    assert.ok(button.classList.contains('is-warning'));
    const status = harness.tile.querySelector('.ai-chat-panel__status') as unknown as FakeElement;
    assert.equal(elementText(status), 'An AI chat is already running for this window.');
  } finally {
    harness.controller.destroy();
    harness.restore();
  }
});
