import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { setupHomeScreen } from '../src/homeScreen.ts';
import { HARNESS_COLOR_STORAGE_KEY, HARNESS_RECENTS_STORAGE_KEY } from '../src/constants.ts';

const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const homeScreenSource = readFileSync(new URL('../src/homeScreen.ts', import.meta.url), 'utf8');
const controlsSource = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');

class FakeClassList {
  private readonly values = new Set<string>();

  add(...names: string[]) {
    names.forEach((name) => this.values.add(name));
  }

  remove(...names: string[]) {
    names.forEach((name) => this.values.delete(name));
  }

  toggle(name: string, force?: boolean): boolean {
    const next = force ?? !this.values.has(name);
    if (next) this.values.add(name);
    else this.values.delete(name);
    return next;
  }

  contains(name: string): boolean {
    return this.values.has(name);
  }
}

class FakeElement {
  readonly children: FakeElement[] = [];
  readonly dataset: Record<string, string> = {};
  readonly classList = new FakeClassList();
  readonly style = { setProperty(_name: string, _value: string) {} };
  parentElement: FakeElement | null = null;
  ownerDocument: FakeDocument | null = null;
  value = '';
  textContent = '';
  innerHTML = '';
  title = '';
  className = '';
  type = '';
  disabled = false;
  hidden = false;
  private readonly listeners = new Map<string, (event: Event) => void>();
  private readonly attributes = new Map<string, string>();

  append(...nodes: FakeElement[]) {
    nodes.forEach((node) => {
      node.parentElement = this;
      node.ownerDocument = this.ownerDocument;
      this.children.push(node);
    });
  }

  appendChild(node: FakeElement) {
    this.append(node);
    return node;
  }

  insertBefore(node: FakeElement, reference: FakeElement | null) {
    node.parentElement = this;
    const index = reference ? this.children.indexOf(reference) : -1;
    if (index < 0) this.children.push(node);
    else this.children.splice(index, 0, node);
    return node;
  }

  replaceChildren(...nodes: FakeElement[]) {
    this.children.length = 0;
    this.append(...nodes);
  }

  addEventListener(type: string, listener: (event: Event) => void) {
    this.listeners.set(type, listener);
  }

  dispatchEvent(event: Event & { type: string }) {
    this.listeners.get(event.type)?.(event);
    return true;
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }

  getAttribute(name: string) {
    return this.attributes.get(name) ?? null;
  }

  contains(node: FakeElement | null): boolean {
    return node === this || this.children.some((child) => child.contains(node));
  }

  querySelector(selector: string): FakeElement | null {
    const className = selector.startsWith('.') ? selector.slice(1) : null;
    for (const child of this.children) {
      if (className && child.className.split(' ').includes(className)) return child;
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }

  focus() {
    if (this.ownerDocument) this.ownerDocument.activeElement = this;
  }

  click() {
    this.listeners.get('click')?.({ preventDefault() {}, stopPropagation() {} } as Event);
  }
}

class FakeDocument {
  activeElement: FakeElement | null = null;
  private readonly listeners = new Map<string, (event: Event) => void>();

  createElement() {
    const element = new FakeElement();
    element.ownerDocument = this;
    return element;
  }

  addEventListener(type: string, listener: (event: Event) => void) {
    this.listeners.set(type, listener);
  }

  dispatchEvent(event: Event & { type: string }) {
    this.listeners.get(event.type)?.(event);
    return true;
  }
}

class MemoryStorage {
  private readonly values = new Map<string, string>();

  constructor(entries: Record<string, string> = {}) {
    Object.entries(entries).forEach(([key, value]) => this.values.set(key, value));
  }

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

function setupFixture(inputValue: string, storage = new MemoryStorage()) {
  const joinCard = new FakeElement();
  const connError = new FakeElement();
  connError.parentElement = joinCard;
  const meetingCodeInput = new FakeElement();
  meetingCodeInput.value = inputValue;
  const joinBtn = new FakeElement();
  const submitCalls: string[] = [];
  const submittedInputs: string[] = [];
  const priorDocument = globalThis.document;
  const priorStorage = globalThis.localStorage;
  const priorLocation = globalThis.location;

  (globalThis as unknown as { document: unknown }).document = {
    createElement: () => new FakeElement(),
  };
  (globalThis as unknown as { localStorage: unknown }).localStorage = storage;
  (globalThis as unknown as { location: unknown }).location = { hostname: 'meet.petal.live', origin: 'https://meet.petal.live' };

  const submitMeetingField = async () => {
    submitCalls.push('submit');
    submittedInputs.push(meetingCodeInput.value);
  };

  const api = setupHomeScreen({
    joinCard: joinCard as unknown as HTMLDivElement,
    displayNameInput: new FakeElement() as unknown as HTMLInputElement,
    profileAvatarInitial: null,
    meetingCodeInput: meetingCodeInput as unknown as HTMLInputElement,
    joinBtn: joinBtn as unknown as HTMLButtonElement,
    connError: connError as unknown as HTMLElement,
    joinHint: new FakeElement() as unknown as HTMLElement,
    submitMeetingField,
  });

  // Mirror the production controls.installControls listener so this fake
  // button exercises the same callback seam as the Enter handler above.
  joinBtn.addEventListener('click', () => {
    void submitMeetingField();
  });

  const restore = () => {
    (globalThis as unknown as { document: unknown }).document = priorDocument;
    (globalThis as unknown as { localStorage: unknown }).localStorage = priorStorage;
    (globalThis as unknown as { location: unknown }).location = priorLocation;
  };

  return { api, joinCard, joinBtn, meetingCodeInput, restore, submitCalls, submittedInputs };
}

test('clean startup keeps an empty meeting field and its Create/Join state', () => {
  const fixture = setupFixture('');
  try {
    assert.equal(fixture.meetingCodeInput.value, '');
    assert.equal(fixture.meetingCodeInput.dataset.petalRoomCredential, undefined);
    assert.equal(fixture.meetingCodeInput.dataset.petalRoomDisplayLabel, undefined);
    assert.equal(fixture.joinBtn.textContent, 'Create/Join');
  } finally {
    fixture.restore();
  }
});

test('the real meeting markup keeps the field empty, named, and placeholder-driven', () => {
  assert.match(indexSource, /<input id="meeting-code"[^>]*placeholder="Enter meeting name or Petal invite"/);
  assert.match(indexSource, /<input id="meeting-code"[^>]*aria-label="Meeting name, invite link, or meeting code"/);
  assert.doesNotMatch(indexSource, /<input id="meeting-code"[^>]*value=/);
  assert.doesNotMatch(indexSource, /<input id="meeting-code"[^>]*>Petal meeting/);
});

test('whitespace-only startup is normalized to an empty meeting field', () => {
  const fixture = setupFixture('   ');
  try {
    assert.equal(fixture.meetingCodeInput.value, '');
    assert.equal(fixture.meetingCodeInput.dataset.petalRoomCredential, undefined);
    assert.equal(fixture.meetingCodeInput.dataset.petalRoomDisplayLabel, undefined);
    assert.equal(fixture.joinBtn.textContent, 'Create/Join');
  } finally {
    fixture.restore();
  }
});

test('non-empty recent rooms still restore their label, credential, and Join state', () => {
  const fixture = setupFixture('', new MemoryStorage({
    [HARNESS_RECENTS_STORAGE_KEY]: JSON.stringify([{ code: 'room-123', lastJoinedAt: 1, joinCount: 1 }]),
  }));
  try {
    const recentRoomButton = fixture.joinCard.children[0]?.children[1]?.children[0];
    assert.ok(recentRoomButton, 'recent room button should be rendered');

    recentRoomButton.click();

    assert.equal(fixture.meetingCodeInput.value, 'Petal meeting');
    assert.equal(fixture.meetingCodeInput.dataset.petalRoomCredential, 'room-123');
    assert.equal(fixture.meetingCodeInput.dataset.petalRoomDisplayLabel, 'Petal meeting');
    assert.equal(fixture.joinBtn.textContent, 'Join');
    assert.deepEqual(fixture.submitCalls, ['submit']);
  } finally {
    fixture.restore();
  }
});

test('Enter and Create/Join use the same submitMeetingField seam for blank input', () => {
  assert.match(
    homeScreenSource,
    /meetingCodeInput\.addEventListener\('keydown',[\s\S]*if \(event\.key !== 'Enter'\)[\s\S]*void submitMeetingField\(\);/
  );
  assert.match(
    controlsSource,
    /joinBtn\.addEventListener\('click', \(\) => \{\s*void submitMeetingField\(\);\s*\}\);/
  );

  for (const input of ['', '   ']) {
    const enterFixture = setupFixture(input);
    const buttonFixture = setupFixture(input);
    try {
      enterFixture.meetingCodeInput.dispatchEvent({
        type: 'keydown',
        key: 'Enter',
        preventDefault() {},
      } as KeyboardEvent);
      buttonFixture.joinBtn.click();

      assert.deepEqual(enterFixture.submittedInputs, ['']);
      assert.deepEqual(buttonFixture.submittedInputs, enterFixture.submittedInputs);
    } finally {
      enterFixture.restore();
      buttonFixture.restore();
    }
  }
});

test('profile color chooser focuses, persists, and restores focus across escape and outside dismissal', async () => {
  const fakeDocument = new FakeDocument();
  const storage = new MemoryStorage({ [HARNESS_COLOR_STORAGE_KEY]: '1' });
  const priorDocument = globalThis.document;
  const priorStorage = globalThis.localStorage;
  const priorLocation = globalThis.location;
  (globalThis as unknown as { document: unknown }).document = fakeDocument;
  (globalThis as unknown as { localStorage: unknown }).localStorage = storage;
  (globalThis as unknown as { location: unknown }).location = { hostname: 'petal.test', origin: 'https://petal.test' };

  try {
    const joinCard = fakeDocument.createElement();
    const connError = fakeDocument.createElement();
    joinCard.append(connError);
    const picker = fakeDocument.createElement();
    const bubble = fakeDocument.createElement();
    const popover = fakeDocument.createElement();
    popover.className = 'profile-color-options';
    popover.hidden = true;
    picker.append(bubble, popover);
    const swatches = ['plum', 'blue', 'green', 'amber', 'lilac', 'slate'].map((name, index) => {
      const swatch = fakeDocument.createElement();
      swatch.dataset.colorIndex = String(index);
      swatch.dataset.colorName = name;
      popover.append(swatch);
      return swatch;
    });
    const onboarding = fakeDocument.createElement();
    const outside = fakeDocument.createElement();

    setupHomeScreen({
      joinCard: joinCard as unknown as HTMLDivElement,
      displayNameInput: fakeDocument.createElement() as unknown as HTMLInputElement,
      profileAvatarInitial: null,
      profileColorBubble: bubble as unknown as HTMLButtonElement,
      profileColorSwatches: swatches as unknown as HTMLButtonElement[],
      profileOnboarding: onboarding as unknown as HTMLElement,
      meetingCodeInput: fakeDocument.createElement() as unknown as HTMLInputElement,
      joinBtn: fakeDocument.createElement() as unknown as HTMLButtonElement,
      connError: connError as unknown as HTMLElement,
      joinHint: fakeDocument.createElement() as unknown as HTMLElement,
      submitMeetingField: async () => {},
    });

    bubble.click();
    assert.equal(popover.hidden, false);
    assert.equal(fakeDocument.activeElement, swatches[1], 'opening moves focus to the saved color');
    assert.equal(bubble.getAttribute('aria-expanded'), 'true');

    popover.dispatchEvent({ type: 'keydown', key: 'ArrowRight', preventDefault() {} } as KeyboardEvent);
    assert.equal(fakeDocument.activeElement, swatches[2], 'arrow navigation remains inside the chooser');

    swatches[4].click();
    assert.equal(storage.getItem(HARNESS_COLOR_STORAGE_KEY), '4');
    assert.equal(popover.hidden, true);
    assert.equal(fakeDocument.activeElement, bubble, 'selecting restores focus to the trigger');
    assert.match(bubble.getAttribute('aria-label') ?? '', /currently lilac/);
    assert.equal(onboarding.classList.contains('hidden'), false, 'selecting only dismisses the palette');

    bubble.click();
    popover.dispatchEvent({ type: 'keydown', key: 'Escape', preventDefault() {} } as KeyboardEvent);
    assert.equal(popover.hidden, true);
    assert.equal(fakeDocument.activeElement, bubble, 'Escape restores focus to the trigger');

    bubble.click();
    fakeDocument.dispatchEvent({ type: 'pointerdown', target: outside } as unknown as PointerEvent);
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(popover.hidden, true);
    assert.equal(fakeDocument.activeElement, bubble, 'outside dismissal restores focus to the trigger');
    assert.equal(storage.getItem(HARNESS_COLOR_STORAGE_KEY), '4', 'dismissal preserves the chosen color');
  } finally {
    (globalThis as unknown as { document: unknown }).document = priorDocument;
    (globalThis as unknown as { localStorage: unknown }).localStorage = priorStorage;
    (globalThis as unknown as { location: unknown }).location = priorLocation;
  }
});
