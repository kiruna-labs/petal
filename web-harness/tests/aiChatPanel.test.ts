// AI chat UI + session-controller behaviour (#657).
//
// The thing under test that actually matters here is the push-to-talk floor:
// a stuck-open PTT keeps the host streaming the room's microphone to a
// third-party API after the user believes they let go. Every release path is
// exercised — pointerup, pointerleave, pointercancel, blur, tab hidden,
// pagehide, window blur, disconnect and teardown.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createAiChatPanel } from '../src/aiChatPanel.ts';
import { setupAiChat } from '../src/aiChatSession.ts';
import { AI_CHAT_TOPIC, type AiChatMessage } from '../src/trackNames.ts';
import { aiChatEndReasonMessage, type AiChatSessionState } from '../src/aiChat.ts';
import type { HarnessContext } from '../src/context.ts';

type Listener = (event: unknown) => void;

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
  className = '';
  textContent = '';
  title = '';
  type = '';
  hidden = false;
  disabled = false;
  /** Only meaningful for `<input>`, but harmless elsewhere -- matches
   * HTMLInputElement.value defaulting to '', which the fake DOM otherwise
   * leaves undefined and every `.value.trim()` call would throw on. */
  value = '';
  maxLength = -1;
  dataset: Record<string, string | undefined> = {};
  readonly attributes = new Map<string, string>();
  readonly children: FakeElement[] = [];
  readonly classList = new FakeClassList(this);
  readonly listeners = new Map<string, Listener[]>();
  parentElement: FakeElement | null = null;
  readonly tagName: string;

  constructor(tagName: string) {
    this.tagName = tagName;
  }

  get firstChild(): FakeElement | null {
    return this.children[0] ?? null;
  }

  appendChild(child: FakeElement): FakeElement {
    child.parentElement?.removeChild(child);
    this.children.push(child);
    child.parentElement = this;
    return child;
  }

  removeChild(child: FakeElement) {
    const index = this.children.indexOf(child);
    if (index >= 0) this.children.splice(index, 1);
    child.parentElement = null;
  }

  remove() {
    this.parentElement?.removeChild(this);
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }

  getAttribute(name: string): string | null {
    return this.attributes.get(name) ?? null;
  }

  addEventListener(type: string, listener: Listener) {
    const existing = this.listeners.get(type) ?? [];
    existing.push(listener);
    this.listeners.set(type, existing);
  }

  dispatch(type: string, event: Record<string, unknown> = {}) {
    const payload = { preventDefault() {}, stopPropagation() {}, ...event };
    for (const listener of this.listeners.get(type) ?? []) listener(payload);
  }

  descendants(): FakeElement[] {
    return this.children.flatMap((child) => [child, ...child.descendants()]);
  }

  findByClass(name: string): FakeElement | null {
    return this.descendants().find((child) => child.classList.contains(name)) ?? null;
  }
}

// #847: the badge now has a persistent icon child (`.ai-chat-panel__badge-icon`,
// set via `innerHTML`, which this fake DOM does not model) plus a text child
// (`.ai-chat-panel__badge-text`) -- `badge.textContent` itself is never
// written to anymore, only the text child's is. Same recursive-aggregation
// helper as remoteWindowHeader.test.ts's `elementText`.
function elementText(element: FakeElement | null): string {
  if (!element) return '';
  return `${element.textContent}${element.children.map(elementText).join('')}`;
}

function installFakeDocument(): { restore: () => void; document: Record<string, unknown> } {
  const previousDocument = (globalThis as Record<string, unknown>).document;
  const fakeDocument = {
    createElement: (tagName: string) => new FakeElement(tagName) as unknown as HTMLElement,
    visibilityState: 'visible',
    listeners: new Map<string, Listener[]>(),
    addEventListener(type: string, listener: Listener) {
      const existing = this.listeners.get(type) ?? [];
      existing.push(listener);
      this.listeners.set(type, existing);
    },
    removeEventListener(type: string, listener: Listener) {
      this.listeners.set(type, (this.listeners.get(type) ?? []).filter((l) => l !== listener));
    },
    fire(type: string) {
      for (const listener of this.listeners.get(type) ?? []) listener({});
    },
  };
  (globalThis as Record<string, unknown>).document = fakeDocument;
  return {
    document: fakeDocument as unknown as Record<string, unknown>,
    restore: () => {
      if (previousDocument === undefined) delete (globalThis as Record<string, unknown>).document;
      else (globalThis as Record<string, unknown>).document = previousDocument;
    },
  };
}

function session(overrides: Partial<AiChatSessionState> = {}): AiChatSessionState {
  return {
    windowId: 42,
    ownerIdentity: 'owner-alice',
    active: true,
    startedBy: null,
    secondsLeft: null,
    activeSpeaker: null,
    error: null,
    lastStateAtMs: 0,
    turns: [],
    ...overrides,
  };
}

function mountPanel() {
  const fake = installFakeDocument();
  const tile = new FakeElement('div');
  const events: string[] = [];
  const options = {
    tile: tile as unknown as HTMLElement,
    windowId: 42,
    ownerIdentity: 'owner-alice',
    localIdentity: 'web-1',
    displayNameFor: (identity: string) => (identity === 'peer-bob' ? 'Bob Ó Súilleabháin' : identity),
    onStop: () => events.push('stop'),
    onPttStart: () => events.push('start'),
    onPttEnd: () => events.push('end'),
    onSendText: (text: string) => events.push(`text:${text}`),
  };
  const panel = createAiChatPanel(options);
  const root = tile.children[0];
  const ptt = root.findByClass('ai-chat-panel__ptt')!;
  return { fake, tile, panel, options, root, ptt, events };
}

test('push-to-talk sends start on pointerdown and end on pointerup', () => {
  const { fake, panel, options, ptt, events } = mountPanel();
  try {
    panel.update(options, session(), false);
    ptt.dispatch('pointerdown', { pointerId: 1 });
    assert.deepEqual(events, ['start']);
    assert.ok(ptt.classList.contains('is-holding'));

    ptt.dispatch('pointerup', { pointerId: 1 });
    assert.deepEqual(events, ['start', 'end']);
    assert.equal(ptt.classList.contains('is-holding'), false);
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('the live web panel has a reachable Stop control that releases PTT first', () => {
  const { fake, panel, options, root, ptt, events } = mountPanel();
  try {
    panel.update(options, session(), false);
    ptt.dispatch('pointerdown', { pointerId: 1 });
    const stop = root.findByClass('ai-chat-panel__stop')!;
    assert.equal(stop.hidden, false);
    assert.equal(stop.disabled, false);
    assert.equal(stop.getAttribute('aria-label'), 'Stop AI chat');
    stop.dispatch('click');
    assert.deepEqual(events, ['start', 'end', 'stop']);

    panel.update(options, session({ active: false }), false);
    assert.equal(stop.hidden, true);
    assert.equal(stop.disabled, true);
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('every way a press can end without a pointerup still releases the floor', () => {
  // A stuck-open PTT is the worst failure this surface can produce.
  for (const releaseEvent of ['pointerleave', 'pointercancel', 'lostpointercapture', 'blur']) {
    const { fake, panel, options, ptt, events } = mountPanel();
    try {
      panel.update(options, session(), false);
      ptt.dispatch('pointerdown', { pointerId: 1 });
      ptt.dispatch(releaseEvent, {});
      assert.deepEqual(events, ['start', 'end'], releaseEvent);
      // ...and a late pointerup must not send a SECOND release.
      ptt.dispatch('pointerup', { pointerId: 1 });
      assert.deepEqual(events, ['start', 'end'], `${releaseEvent} then pointerup`);
    } finally {
      panel.destroy();
      fake.restore();
    }
  }
});

test('destroying the panel while held releases the floor first', () => {
  const { fake, panel, options, ptt, events } = mountPanel();
  try {
    panel.update(options, session(), false);
    ptt.dispatch('pointerdown', { pointerId: 1 });
    panel.destroy();
    assert.deepEqual(events, ['start', 'end']);
  } finally {
    fake.restore();
  }
});

test('a second pointerdown while already held does not double-claim', () => {
  const { fake, panel, options, ptt, events } = mountPanel();
  try {
    panel.update(options, session(), false);
    ptt.dispatch('pointerdown', { pointerId: 1 });
    ptt.dispatch('pointerdown', { pointerId: 2 });
    assert.deepEqual(events, ['start']);
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('the floor holder is named and the control is disabled while they hold it', () => {
  const { fake, panel, options, root, ptt, events } = mountPanel();
  try {
    panel.update(options, session({ activeSpeaker: 'peer-bob' }), false);
    const status = root.findByClass('ai-chat-panel__status')!;
    assert.equal(status.textContent, 'Listening to Bob Ó Súilleabháin');
    assert.equal(ptt.disabled, true);
    // Two speakers interleaved corrupt the turn rather than mixing, so the
    // control refuses rather than silently losing the claim.
    ptt.dispatch('pointerdown', { pointerId: 1 });
    assert.deepEqual(events, []);
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('the disclosure badge is shown for exactly as long as the session is live', () => {
  const { fake, panel, options, root } = mountPanel();
  try {
    const disclosure = root.findByClass('ai-chat-panel__disclosure')!;
    const badge = root.findByClass('ai-chat-panel__badge')!;

    panel.update(options, session(), false);
    assert.equal(disclosure.hidden, false);
    assert.equal(elementText(badge), 'AI chat live');
    assert.ok(badge.classList.contains('is-live'));
    assert.match(disclosure.textContent, /window/i);
    assert.match(disclosure.textContent, /voice/i);

    panel.update(options, session({ active: false }), false);
    assert.equal(disclosure.hidden, true);
    assert.equal(badge.classList.contains('is-live'), false);
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('an error renders the shared user-facing copy, never a raw token', () => {
  const { fake, panel, options, root } = mountPanel();
  try {
    const status = root.findByClass('ai-chat-panel__status')!;
    for (const reason of ['busy', 'quota', 'time-limit'] as const) {
      panel.update(options, session({ active: false, error: reason }), false);
      assert.equal(status.textContent, aiChatEndReasonMessage(reason));
      // A sentence from the shared table, never the bare wire token.
      assert.notEqual(status.textContent, reason, `raw token rendered for ${reason}`);
      assert.match(status.textContent, /\.$/, `${reason} copy is not a sentence`);
    }
    // A normal end is not styled as a failure.
    assert.equal(status.classList.contains('is-warning'), false);
    panel.update(options, session({ active: false, error: 'quota' }), false);
    assert.ok(status.classList.contains('is-warning'));
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('transcript bubbles render coalesced turns in order, in full', () => {
  const { fake, panel, options, root } = mountPanel();
  try {
    const long = 'a sentence long enough that a fixed-width control would have to clip it '.repeat(3);
    panel.update(
      options,
      session({
        turns: [
          { id: 1, role: 'user', text: 'what broke?', final: true },
          { id: 2, role: 'assistant', text: long, final: false },
        ],
      }),
      false,
    );
    const transcript = root.findByClass('ai-chat-panel__transcript')!;
    assert.equal(transcript.hidden, false);
    assert.equal(transcript.children.length, 2);
    assert.deepEqual(
      transcript.children.map((bubble) => bubble.dataset.role),
      ['user', 'assistant'],
    );
    // The full text is present -- nothing is elided at render time. (Wrapping
    // rather than truncating is enforced in style.css.)
    const bubbleText = transcript.children[1].findByClass('ai-chat-panel__turn-text')!;
    assert.equal(bubbleText.textContent, long);
    assert.ok(transcript.children[1].classList.contains('is-open'));
    assert.equal(transcript.children[0].classList.contains('is-open'), false);

    panel.update(options, session(), false);
    assert.equal(transcript.hidden, true, 'no turns means no empty transcript box');
  } finally {
    panel.destroy();
    fake.restore();
  }
});

test('the countdown renders m:ss and hides when the owner reports none', () => {
  const { fake, panel, options, root } = mountPanel();
  try {
    const countdown = root.findByClass('ai-chat-panel__countdown')!;
    panel.update(options, session({ secondsLeft: 245 }), false);
    assert.equal(countdown.hidden, false);
    assert.equal(countdown.textContent, '4:05');
    panel.update(options, session(), false);
    assert.equal(countdown.hidden, true);
  } finally {
    panel.destroy();
    fake.restore();
  }
});

// ---------------------------------------------------------------------------
// Session controller
// ---------------------------------------------------------------------------

interface PublishedPacket {
  message: AiChatMessage;
  topic: string;
  reliable: boolean;
}

function makeController() {
  const fake = installFakeDocument();
  const previousGlobalAdd = (globalThis as Record<string, unknown>).addEventListener;
  const previousGlobalRemove = (globalThis as Record<string, unknown>).removeEventListener;
  const globalListeners = new Map<string, Listener[]>();
  (globalThis as Record<string, unknown>).addEventListener = (type: string, listener: Listener) => {
    globalListeners.set(type, [...(globalListeners.get(type) ?? []), listener]);
  };
  (globalThis as Record<string, unknown>).removeEventListener = (type: string, listener: Listener) => {
    globalListeners.set(type, (globalListeners.get(type) ?? []).filter((l) => l !== listener));
  };

  const published: PublishedPacket[] = [];
  const logs: string[] = [];
  const ctx = {
    state: {
      room: {
        localParticipant: {
          identity: 'web-1',
          publishData: (bytes: Uint8Array, options: { topic: string; reliable: boolean }) => {
            published.push({
              message: JSON.parse(new TextDecoder().decode(bytes)) as AiChatMessage,
              topic: options.topic,
              reliable: options.reliable,
            });
            return Promise.resolve();
          },
        },
      },
    },
    ui: { logEvent: (message: string) => logs.push(message) },
  } as unknown as HarnessContext;

  const controller = setupAiChat(ctx);
  return {
    controller,
    published,
    logs,
    fireGlobal: (type: string) => {
      for (const listener of globalListeners.get(type) ?? []) listener({});
    },
    fireDocument: (type: string) => (fake.document as unknown as { fire: (t: string) => void }).fire(type),
    setVisibility: (value: string) => {
      (fake.document as unknown as { visibilityState: string }).visibilityState = value;
    },
    restore: () => {
      controller.destroy();
      fake.restore();
      if (previousGlobalAdd === undefined) delete (globalThis as Record<string, unknown>).addEventListener;
      else (globalThis as Record<string, unknown>).addEventListener = previousGlobalAdd;
      if (previousGlobalRemove === undefined) delete (globalThis as Record<string, unknown>).removeEventListener;
      else (globalThis as Record<string, unknown>).removeEventListener = previousGlobalRemove;
    },
  };
}

function encode(message: AiChatMessage): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(message));
}

test('requests and floor claims publish reliably on the pinned topic', () => {
  const rig = makeController();
  try {
    rig.controller.requestStart(42, 'owner-alice');
    rig.controller.pttStart(42, 'owner-alice');
    rig.controller.pttEnd(42, 'owner-alice');
    assert.deepEqual(
      rig.published.map((packet) => packet.message.type),
      ['startRequest', 'pttStart', 'pttEnd'],
    );
    for (const packet of rig.published) {
      assert.equal(packet.topic, AI_CHAT_TOPIC);
      assert.equal(packet.reliable, true);
      assert.equal(packet.message.v, 1);
      assert.equal(packet.message.windowId, 42);
      assert.equal(packet.message.ownerIdentity, 'owner-alice');
    }
  } finally {
    rig.restore();
  }
});

test('stopping releases the floor before it asks the owner to stop', () => {
  const rig = makeController();
  try {
    rig.controller.pttStart(42, 'owner-alice');
    rig.published.length = 0;
    rig.controller.requestStop(42, 'owner-alice');
    assert.deepEqual(
      rig.published.map((packet) => packet.message.type),
      ['pttEnd', 'stopRequest'],
    );
  } finally {
    rig.restore();
  }
});

test('a hidden tab, a pagehide, and a window blur all release a held floor', () => {
  for (const trigger of ['visibility', 'pagehide', 'blur'] as const) {
    const rig = makeController();
    try {
      rig.controller.pttStart(42, 'owner-alice');
      assert.equal(rig.controller.localPttHeld(42, 'owner-alice'), true);
      rig.published.length = 0;

      if (trigger === 'visibility') {
        rig.setVisibility('hidden');
        rig.fireDocument('visibilitychange');
      } else {
        rig.fireGlobal(trigger);
      }

      assert.deepEqual(
        rig.published.map((packet) => packet.message.type),
        ['pttEnd'],
        trigger,
      );
      assert.equal(rig.controller.localPttHeld(42, 'owner-alice'), false, trigger);
    } finally {
      rig.restore();
    }
  }
});

test('becoming visible again does not release a floor that is not held', () => {
  const rig = makeController();
  try {
    rig.setVisibility('visible');
    rig.fireDocument('visibilitychange');
    assert.deepEqual(rig.published, []);
  } finally {
    rig.restore();
  }
});

test('teardown releases a held floor rather than stranding it', () => {
  const rig = makeController();
  try {
    rig.controller.pttStart(42, 'owner-alice');
    rig.published.length = 0;
    rig.controller.destroy();
    assert.deepEqual(
      rig.published.map((packet) => packet.message.type),
      ['pttEnd'],
    );
  } finally {
    rig.restore();
  }
});

test('inbound packets are authorized by the authenticated sender, not the payload', () => {
  const rig = makeController();
  try {
    const live: AiChatMessage = {
      v: 1,
      type: 'state',
      windowId: 42,
      ownerIdentity: 'owner-alice',
      active: true,
    };
    // A peer forging the owner's session state -- dropped, and logged.
    rig.controller.handlePayload(encode(live), 'peer-mallory', AI_CHAT_TOPIC);
    assert.equal(rig.controller.sessionFor(42, 'owner-alice'), null);
    assert.ok(rig.logs.some((line) => line.includes('peer-mallory')));

    rig.controller.handlePayload(encode(live), 'owner-alice', AI_CHAT_TOPIC);
    assert.equal(rig.controller.sessionFor(42, 'owner-alice')?.active, true);

    // Another topic's payload must never reach this handler's state.
    rig.controller.handlePayload(encode({ ...live, active: false }), 'owner-alice', 'petal.draw');
    assert.equal(rig.controller.sessionFor(42, 'owner-alice')?.active, true);
  } finally {
    rig.restore();
  }
});

test('an owner leaving clears their sessions and notifies listeners', () => {
  const rig = makeController();
  try {
    let notifications = 0;
    const unsubscribe = rig.controller.onChange(() => {
      notifications += 1;
    });
    rig.controller.handlePayload(
      encode({ v: 1, type: 'state', windowId: 42, ownerIdentity: 'owner-alice', active: true }),
      'owner-alice',
      AI_CHAT_TOPIC,
    );
    assert.equal(notifications, 1);

    rig.controller.ownerLeft('owner-alice');
    assert.equal(rig.controller.sessionFor(42, 'owner-alice'), null);
    assert.equal(notifications, 2);

    unsubscribe();
    rig.controller.ownerLeft('owner-alice');
    assert.equal(notifications, 2, 'unsubscribed listeners must stop firing');
  } finally {
    rig.restore();
  }
});

test('reset drops every session and releases every floor', () => {
  const rig = makeController();
  try {
    rig.controller.handlePayload(
      encode({ v: 1, type: 'state', windowId: 42, ownerIdentity: 'owner-alice', active: true }),
      'owner-alice',
      AI_CHAT_TOPIC,
    );
    rig.controller.pttStart(42, 'owner-alice');
    rig.published.length = 0;

    rig.controller.reset();
    assert.deepEqual(
      rig.published.map((packet) => packet.message.type),
      ['pttEnd'],
    );
    assert.equal(rig.controller.sessionFor(42, 'owner-alice'), null);
    assert.equal(rig.controller.localPttHeld(42, 'owner-alice'), false);
  } finally {
    rig.restore();
  }
});
