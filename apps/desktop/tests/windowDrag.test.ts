// PETAL-WORKAROUND(tao-drag-window): the macOS drag-region listener in
// src/lib/windowDrag.ts and its root-layout install.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { COMMANDS } from '../src/lib/ipc.ts';
import {
  installWindowDragWorkaround,
  isDragRegion,
  type DragMouseDown,
  type DragRegionElement,
  type MouseDownTarget
} from '../src/lib/windowDrag.ts';

class FakeElement implements DragRegionElement {
  constructor(
    readonly tagName: string,
    private readonly attrs: Record<string, string> = {}
  ) {}
  getAttribute(name: string): string | null {
    return name in this.attrs ? this.attrs[name]! : null;
  }
  hasAttribute(name: string): boolean {
    return name in this.attrs;
  }
}
const isFake = (el: unknown): el is DragRegionElement => el instanceof FakeElement;
const div = (attrs: Record<string, string> = {}) => new FakeElement('DIV', attrs);
const DRAG = 'data-tauri-drag-region';

test('a bare or "true" drag region drags only on a direct press', () => {
  const region = div({ [DRAG]: '' });
  assert.equal(isDragRegion([region], isFake), true);
  assert.equal(isDragRegion([div({ [DRAG]: 'true' })], isFake), true);
  assert.equal(isDragRegion([div(), region], isFake), false, 'a child of a bare region does not drag');
});

test('"deep" drags from its subtree, "false" and clickable elements block', () => {
  const deep = div({ [DRAG]: 'deep' });
  assert.equal(isDragRegion([div(), deep], isFake), true);
  assert.equal(isDragRegion([div({ [DRAG]: 'false' }), deep], isFake), false);
  // Tauri 2.12.2's full clickable tag and role lists, each blocking on its own.
  for (const tag of ['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'LABEL', 'SUMMARY']) {
    assert.equal(isDragRegion([new FakeElement(tag), deep], isFake), false, tag);
  }
  for (const role of ['button', 'link', 'menuitem', 'tab', 'checkbox', 'radio', 'switch', 'option']) {
    assert.equal(isDragRegion([div({ role }), deep], isFake), false, role);
  }
  assert.equal(isDragRegion([div({ role: 'presentation' }), deep], isFake), true);
  assert.equal(isDragRegion([div({ contenteditable: 'false' }), deep], isFake), true);
  assert.equal(isDragRegion([div({ [DRAG]: 'yes' }), deep], isFake), true, 'an unknown value keeps walking');
  assert.equal(isDragRegion([div({ tabindex: '0' }), deep], isFake), false);
  assert.equal(isDragRegion([div({ tabindex: '-1' }), deep], isFake), true);
  assert.equal(isDragRegion([div({ contenteditable: 'true' }), deep], isFake), false);
  assert.equal(isDragRegion([new FakeElement('BUTTON', { [DRAG]: '' })], isFake), true, 'a clickable region drags');
  // Non-HTML entries (an SVG node, document, window) are skipped.
  assert.equal(isDragRegion([{ tagName: 'svg' }, deep], isFake), true);
  assert.equal(isDragRegion([], isFake), false);
});

function fakeTarget() {
  const listeners = new Set<(event: DragMouseDown) => void>();
  const target: MouseDownTarget = {
    addEventListener: (_type, listener) => listeners.add(listener),
    removeEventListener: (_type, listener) => listeners.delete(listener)
  };
  return { target, listeners };
}

function mouseDown(path: unknown[], init: { button?: number; detail?: number } = {}) {
  const calls = { prevented: 0, stopped: 0 };
  const event: DragMouseDown = {
    button: init.button ?? 0,
    detail: init.detail ?? 1,
    composedPath: () => path,
    preventDefault: () => void calls.prevented++,
    stopPropagation: () => void calls.stopped++
  };
  return { event, calls };
}

test('the listener is not installed on Windows, other platforms or outside Tauri', () => {
  for (const [platform, hasBridge] of [
    ['windows', true],
    ['other', true],
    ['macos', false]
  ] as const) {
    const { target, listeners } = fakeTarget();
    installWindowDragWorkaround({ platform, hasBridge, target, startDrag: async () => {}, isElement: isFake });
    assert.equal(listeners.size, 0, `${platform}, bridge ${hasBridge}`);
  }
});

test('on macOS a single left press on a drag region is stopped and starts our drag', async () => {
  const { target, listeners } = fakeTarget();
  let drags = 0;
  const uninstall = installWindowDragWorkaround({
    platform: 'macos',
    hasBridge: true,
    target,
    startDrag: async () => void drags++,
    isElement: isFake
  });
  assert.equal(listeners.size, 1);
  const [listener] = listeners;
  const press = mouseDown([div({ [DRAG]: '' })]);
  listener!(press.event);
  assert.deepEqual(press.calls, { prevented: 1, stopped: 1 });
  assert.equal(drags, 1);
  uninstall();
  assert.equal(listeners.size, 0);
});

test('double clicks, other buttons and non-regions are left to Tauri and the page', () => {
  const { target, listeners } = fakeTarget();
  let drags = 0;
  installWindowDragWorkaround({
    platform: 'macos',
    hasBridge: true,
    target,
    startDrag: async () => void drags++,
    isElement: isFake
  });
  const [listener] = listeners;
  for (const press of [
    mouseDown([div({ [DRAG]: '' })], { detail: 2 }),
    mouseDown([div({ [DRAG]: '' })], { button: 2 }),
    mouseDown([new FakeElement('BUTTON')])
  ]) {
    listener!(press.event);
    assert.deepEqual(press.calls, { prevented: 0, stopped: 0 });
  }
  assert.equal(drags, 0);
});

test('the command is registered on both invoke handlers and installed from the root layout', () => {
  assert.equal(COMMANDS.startWindowDrag, 'start_window_drag');
  const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  const sections = lib.split('.invoke_handler(tauri::generate_handler![').slice(1);
  assert.equal(sections.length, 2);
  for (const section of sections) assert.match(section.split(']')[0]!, /window_drag::start_window_drag,/);
  // Comments stripped, so a commented-out install doesn't pass.
  const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8').replace(
    /\/\/.*$/gm,
    ''
  );
  const mountStart = layout.indexOf('onMount(() => {');
  const cleanupStart = layout.indexOf('return () => {', mountStart);
  const cleanupEnd = layout.indexOf('});', cleanupStart);
  assert.ok(mountStart > -1 && cleanupStart > mountStart && cleanupEnd > cleanupStart);
  assert.match(layout.slice(mountStart, cleanupStart), /const uninstallWindowDrag\s*=\s*installWindowDragWorkaround\(\)/);
  assert.match(layout.slice(cleanupStart, cleanupEnd), /uninstallWindowDrag\(\)/);
});
