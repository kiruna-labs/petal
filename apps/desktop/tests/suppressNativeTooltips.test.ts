import assert from 'node:assert/strict';
import test from 'node:test';
import {
  stripNativeTooltipTitles,
  type TitleAttrElement
} from '../src/lib/data/suppressNativeTooltips.ts';

/** Minimal DOM-free stand-in for the ancestry walk. */
class StubElement implements TitleAttrElement {
  attributes = new Set<string>();
  parentElement: StubElement | null = null;

  constructor(title?: string, parent: StubElement | null = null) {
    if (title !== undefined) this.attributes.add('title');
    this.parentElement = parent;
  }

  hasAttribute(name: string): boolean {
    return this.attributes.has(name);
  }

  removeAttribute(name: string): void {
    this.attributes.delete(name);
  }
}

test('strips title from the hovered element itself', () => {
  const button = new StubElement('Mute');
  stripNativeTooltipTitles(button);
  assert.equal(button.hasAttribute('title'), false);
});

test('strips title from titled ancestors (Chromium shows the nearest titled ancestor)', () => {
  const root = new StubElement('Connection stats');
  const cell = new StubElement(undefined, root);
  const button = new StubElement(undefined, cell);

  stripNativeTooltipTitles(button);
  assert.equal(button.hasAttribute('title'), false);
  assert.equal(root.hasAttribute('title'), false);
});

test('strips titles along the whole chain, not just the nearest titled ancestor', () => {
  const grandparent = new StubElement('Grandparent');
  const parent = new StubElement('Parent', grandparent);
  const leaf = new StubElement(undefined, parent);

  stripNativeTooltipTitles(leaf);
  assert.equal(leaf.hasAttribute('title'), false);
  assert.equal(parent.hasAttribute('title'), false);
  assert.equal(grandparent.hasAttribute('title'), false);
});

test('preserves a marked native tooltip while stripping unmarked titled ancestors', () => {
  const root = new StubElement('container tooltip');
  const button = new StubElement('Share this window — right-click for options', root);
  button.attributes.add('data-allow-native-tooltip');

  stripNativeTooltipTitles(button);
  assert.equal(button.hasAttribute('title'), true);
  assert.equal(root.hasAttribute('title'), false);
});

test('does not preserve an unmarked title next to the native-tooltip marker', () => {
  const button = new StubElement('Share this window');

  stripNativeTooltipTitles(button);
  assert.equal(button.hasAttribute('title'), false);
});

test('leaves unrelated elements untouched', () => {
  const button = new StubElement('Mute');
  const sibling = new StubElement('Unmute');

  stripNativeTooltipTitles(button);
  assert.equal(button.hasAttribute('title'), false);
  assert.equal(sibling.hasAttribute('title'), true);
});

test('is a no-op on an already-stripped element (repeated hovers)', () => {
  const button = new StubElement('Mute');
  stripNativeTooltipTitles(button);
  stripNativeTooltipTitles(button);
  assert.equal(button.hasAttribute('title'), false);
});

test('is a no-op on null', () => {
  stripNativeTooltipTitles(null);
  assert.ok(true);
});
