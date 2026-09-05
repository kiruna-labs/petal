import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PresentationSourceHost, PRESENTATION_SOURCE_CLASS, PRESENTATION_SOURCE_HOST_ID } from '../src/presentationSourceHost.ts';

class FakeClassList {
  private readonly values = new Set<string>();
  add(value: string) { this.values.add(value); }
  remove(value: string) { this.values.delete(value); }
  contains(value: string) { return this.values.has(value); }
}

class FakeElement {
  readonly children: FakeElement[] = [];
  readonly style: Record<string, string> = {};
  readonly classList = new FakeClassList();
  parentNode: FakeElement | null = null;
  id = '';
  readonly tagName: string;

  constructor(tagName: string) { this.tagName = tagName; }

  get parentElement() { return this.parentNode; }
  get nextSibling(): FakeElement | null {
    if (!this.parentNode) return null;
    return this.parentNode.children[this.parentNode.children.indexOf(this) + 1] ?? null;
  }
  get isConnected() { return this.parentNode !== null; }
  appendChild(child: FakeElement) {
    child.remove(); child.parentNode = this; this.children.push(child); return child;
  }
  insertBefore(child: FakeElement, before: FakeElement | null) {
    child.remove(); child.parentNode = this;
    const index = before ? this.children.indexOf(before) : -1;
    if (index < 0) this.children.push(child); else this.children.splice(index, 0, child);
    return child;
  }
  contains(candidate: FakeElement): boolean { return candidate === this || this.children.some((child) => child.contains(candidate)); }
  remove() {
    if (!this.parentNode) return;
    const index = this.parentNode.children.indexOf(this);
    if (index >= 0) this.parentNode.children.splice(index, 1);
    this.parentNode = null;
  }
  getBoundingClientRect() {
    return { width: Number.parseFloat(this.style.width ?? '0'), height: Number.parseFloat(this.style.height ?? '0') };
  }
}

class FakeDocument {
  readonly body = new FakeElement('BODY');
  constructor() { this.body.parentNode = this.body; }
  createElement(tagName: string) { return new FakeElement(tagName.toUpperCase()); }
  getElementById(id: string): FakeElement | null {
    const visit = (element: FakeElement): FakeElement | null => element.id === id ? element : element.children.map(visit).find(Boolean) ?? null;
    return visit(this.body);
  }
}

test('presentation source host moves the exact capture canvas into a visible nonzero top-level box, then restores it on stop', () => {
  const document = new FakeDocument();
  const panel = document.createElement('section');
  const canvas = document.createElement('canvas'); canvas.id = 'test-canvas';
  document.body.appendChild(panel); panel.appendChild(canvas);
  const originalCanvas = canvas;

  const sourceHost = new PresentationSourceHost(canvas as unknown as HTMLCanvasElement, document as unknown as Document);
  const host = sourceHost.mount() as unknown as FakeElement;

  assert.equal(host.id, PRESENTATION_SOURCE_HOST_ID);
  assert.equal(host.parentNode, document.body);
  assert.equal(host.children.length, 1);
  assert.equal(host.children[0], originalCanvas, 'the captureStream canvas is moved, never copied');
  assert.equal(originalCanvas.parentNode, host);
  assert.equal(originalCanvas.classList.contains(PRESENTATION_SOURCE_CLASS), true);
  assert.equal(host.style.position, 'fixed');
  assert.equal(host.style.display, 'block');
  assert.equal(host.style.visibility, 'visible');
  assert.ok(host.getBoundingClientRect().width > 0 && host.getBoundingClientRect().height > 0, 'host layout is nonzero while sharing');

  sourceHost.unmount();
  assert.equal(document.getElementById(PRESENTATION_SOURCE_HOST_ID), null);
  assert.equal(originalCanvas.parentNode, panel, 'the exact source canvas is restored after sharing');
  assert.equal(originalCanvas.classList.contains(PRESENTATION_SOURCE_CLASS), false);
});
