import { test } from 'node:test';
import assert from 'node:assert/strict';

import { setupTelepointerDisplay } from '../src/telepointerDisplay.ts';
import { TELEPOINTER_TOPIC } from '../src/trackNames.ts';

// #892 regression: telepointerDisplay.ts's positionTelepointer used to
// normalize against the bare tile rect (videoBoundsForTile), the same
// defect class as the draw-offset bug -- a native sharer's cursor rendered
// ~22px high in every header-bearing web tile. This drives the REAL
// handleRemoteTelepointerPayload -> renderRemoteTelepointer ->
// positionTelepointer path through a DOM double whose <video> is inset
// under a docked header (rect != tile rect), the shape
// tests/telepointer.test.ts's FakeTile (tile/video rects coincide) cannot
// catch.

class FakeClassList {
  private readonly element: FakeElement;
  constructor(element: FakeElement) {
    this.element = element;
  }
  private values(): string[] {
    return this.element.className.split(/\s+/).filter(Boolean);
  }
  add(...names: string[]) {
    const values = new Set(this.values());
    for (const name of names) values.add(name);
    this.element.className = Array.from(values).join(' ');
  }
  remove(...names: string[]) {
    const values = new Set(this.values());
    for (const name of names) values.delete(name);
    this.element.className = Array.from(values).join(' ');
  }
  contains(name: string): boolean {
    return this.values().includes(name);
  }
}

class FakeStyle {
  private readonly values = new Map<string, string>();
  // telepointerDisplay.ts assigns `.style.transform` directly rather than
  // via setProperty -- a plain field mirrors that real CSSStyleDeclaration behavior.
  transform = '';
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

  append(...children: FakeElement[]) {
    for (const child of children) this.appendChild(child);
  }

  remove() {
    if (!this.parentElement) return;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    if (index !== -1) siblings.splice(index, 1);
    this.parentElement = null;
  }

  setAttribute() {
    // unused by the assertions below
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

  querySelector<T extends Element>(selector: string): T | null {
    return this.body.querySelector(selector);
  }

  querySelectorAll<T extends Element>(selector: string): T[] {
    return this.body.querySelectorAll(selector);
  }
}

function matchesSelector(element: FakeElement, selector: string): boolean {
  if (selector === 'video') return element.tagName === 'video';
  const shareTileMatch = selector.match(/^\.share-tile\[data-window-id="(\d+)"\]$/);
  if (shareTileMatch) {
    return element.classList.contains('share-tile') && element.dataset.windowId === shareTileMatch[1];
  }
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

function makeHeaderBearingShareTile(owner: string, windowId: number) {
  // Tile at viewport (50,20), 400x300 -- off-origin so absolute/relative helper confusion cannot cancel out. Video inset top:45px (44px docked
  // header + 1px border) / left:1px (border), matching
  // .tile.has-remote-window-header video { top: 44px } + .tile's 1px border.
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

test('telepointerDisplay positions the receive-side cursor against the video content box, not the bare tile (#892)', () => {
  const fakeDom = installFakeDom();
  try {
    const tile = makeHeaderBearingShareTile('native-1', 42);
    fakeDom.document.body.appendChild(tile);

    const display = setupTelepointerDisplay({
      remoteTelepointers: new Map(),
      handshakeCooldowns: new Map(),
      state: {
        room: {
          localParticipant: { identity: 'web-1', metadata: undefined },
          remoteParticipants: new Map([['native-1', { name: 'Native Sharer', metadata: undefined }]]),
        },
      },
      ui: { logEvent: () => undefined },
    } as never);

    try {
      display.handleRemoteTelepointerPayload(
        encode({ windowId: 42, userId: 'native-1', x: 0.5, y: 0.5, visible: true }),
        'native-1',
        TELEPOINTER_TOPIC
      );

      const pointer = tile.querySelector('.remote-telepointer') as unknown as FakeElement | null;
      assert.ok(pointer, 'expected a rendered telepointer element');
      // video content box (tile-relative): left=1,top=45,width=398,height=254
      // over 1600x900 media -> letterbox top inset 15.0625 -> content
      // {left:1, top:60.0625, width:398, height:223.875}. Center (0.5,0.5)
      // projects to (200.0, 172.0). The old bare-tile-rect bug instead
      // projected against {0,0,400,300} -> (200.0, 150.0): same X (the tile
      // and video share the same horizontal center here), wrong Y by
      // exactly the header inset -- pin the number, not just "differs".
      assert.equal(pointer!.style.transform, 'translate3d(200.0px, 172.0px, 0)');
    } finally {
      display.clearRemoteTelepointers();
    }
  } finally {
    fakeDom.restore();
  }
});
