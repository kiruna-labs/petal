/**
 * PETAL-WORKAROUND(tao-drag-window): macOS drag-region presses go to Petal's
 * own `start_window_drag` instead of Tauri's `start_dragging`.
 *
 * On macOS, tao's `drag_window` drags from `NSApp.currentEvent`, which can be
 * a stale AppKitDefined event at the window's corner by the time the IPC call
 * arrives (type 13 at (0, 400) in our test logs). So dragging Petal View by its
 * title bar could make it jump so its top-left corner landed under the pointer
 * (tauri-apps/tauri#6568). `src-tauri/src/window_drag.rs` explains the fix
 * and when to remove it.
 *
 * Tauri's own handler (`drag.js`) listens on `document` in the bubble phase.
 * This one listens on `document.documentElement`, which the event reaches
 * first, and stops it there for single clicks only. Double clicks
 * (`detail === 2`) still reach Tauri, which toggles maximize on mouseup.
 */
import { invoke } from '@tauri-apps/api/core';
import { COMMANDS, hasTauriBridge } from '#lib/ipc.js';
import { platformKey, type PlatformKey } from '#lib/platform.js';

/** The element surface `isDragRegion` reads. */
export interface DragRegionElement {
  tagName: string;
  getAttribute(name: string): string | null;
  hasAttribute(name: string): boolean;
}

const TAURI_DRAG_REGION_ATTR = 'data-tauri-drag-region';

// Copied from Tauri 2.12.2 `crates/tauri/src/window/scripts/drag.js`
// (Apache-2.0 OR MIT, Copyright 2019-2024 Tauri Programme within The Commons
// Conservancy), typed. `window_drag.rs`'s drift test fails when the resolved
// Tauri copy changes these semantics.
const CLICKABLE_TAGS = new Set(['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'LABEL', 'SUMMARY']);
const INTERACTIVE_ROLES = new Set(['button', 'link', 'menuitem', 'tab', 'checkbox', 'radio', 'switch', 'option']);

function isClickableElement(el: DragRegionElement): boolean {
  return (
    CLICKABLE_TAGS.has(el.tagName) ||
    (el.hasAttribute('contenteditable') && el.getAttribute('contenteditable') !== 'false') ||
    (el.hasAttribute('tabindex') && el.getAttribute('tabindex') !== '-1') ||
    INTERACTIVE_ROLES.has(el.getAttribute('role') ?? '')
  );
}

function isHtmlElement(el: unknown): el is DragRegionElement {
  return typeof HTMLElement !== 'undefined' && el instanceof HTMLElement;
}

/**
 * Tauri's drag-region test over an event's composed path: bare/"true" = only
 * a direct press on that element, "deep" = anywhere in its subtree, "false"
 * blocks, and a clickable element without the attribute blocks.
 */
export function isDragRegion(
  composedPath: readonly unknown[],
  isElement: (el: unknown) => el is DragRegionElement = isHtmlElement
): boolean {
  for (const el of composedPath) {
    if (!isElement(el)) continue;
    const attr = el.getAttribute(TAURI_DRAG_REGION_ATTR);
    if (isClickableElement(el) && attr === null) return false;
    if (attr === null) continue;
    if (attr === 'false') return false;
    if (attr === 'deep') return true;
    if (attr === '' || attr === 'true') return el === composedPath[0];
  }
  return false;
}

/** The mouse-down surface the handler reads and stops. */
export interface DragMouseDown {
  button: number;
  detail: number;
  composedPath(): readonly unknown[];
  preventDefault(): void;
  stopPropagation(): void;
}

/** Where the listener goes: `document.documentElement` in the app. */
export interface MouseDownTarget {
  addEventListener(type: 'mousedown', listener: (event: DragMouseDown) => void): void;
  removeEventListener(type: 'mousedown', listener: (event: DragMouseDown) => void): void;
}

/** `document.documentElement` behind the narrow `MouseDownTarget` surface. */
function documentRoot(): MouseDownTarget {
  const root = document.documentElement;
  return {
    addEventListener: (type, listener) => root.addEventListener(type, listener),
    removeEventListener: (type, listener) => root.removeEventListener(type, listener)
  };
}

export interface WindowDragDeps {
  platform?: PlatformKey;
  hasBridge?: boolean;
  target?: MouseDownTarget;
  startDrag?: () => Promise<unknown>;
  isElement?: (el: unknown) => el is DragRegionElement;
}

/**
 * Install the macOS listener once per webview (the root layout's `onMount`).
 * Returns the uninstaller. On every other platform, and outside Tauri, it
 * installs nothing and drags keep going through Tauri's `start_dragging`.
 */
export function installWindowDragWorkaround(deps: WindowDragDeps = {}): () => void {
  const platform = deps.platform ?? platformKey();
  const hasBridge = deps.hasBridge ?? hasTauriBridge();
  if (platform !== 'macos' || !hasBridge) return () => {};
  const target = deps.target ?? documentRoot();
  const startDrag = deps.startDrag ?? (() => invoke(COMMANDS.startWindowDrag));
  const isElement = deps.isElement ?? isHtmlElement;
  const onMouseDown = (e: DragMouseDown) => {
    if (e.button !== 0 || e.detail !== 1 || !isDragRegion(e.composedPath(), isElement)) return;
    e.preventDefault();
    e.stopPropagation();
    void startDrag().catch((err) => console.warn('windowDrag: start_window_drag failed', err));
  };
  target.addEventListener('mousedown', onMouseDown);
  return () => target.removeEventListener('mousedown', onMouseDown);
}
