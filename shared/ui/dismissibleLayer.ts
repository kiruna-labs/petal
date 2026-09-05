/**
 * Framework-neutral outside-pointer dismissal for non-modal DOM popups.
 *
 * Callers keep popup state, keyboard behavior, and placement. This module owns
 * the event seam that must not depend on backdrop stacking: a capture-phase
 * pointerdown listener closes only when the event is outside the popup and its
 * triggers. The opener is restored only when the pointer did not leave focus on
 * a live outside control.
 */

export interface DismissibleLayerOptions {
  /** Read lazily so one installed listener can survive reactive state changes. */
  isOpen: () => boolean;
  /** Popup and trigger nodes whose pointer events belong to this layer. */
  getInsideNodes: () => readonly (Node | null | undefined)[];
  /** Popup panel nodes used only to decide whether focus was stranded. */
  getPopupNodes?: () => readonly (Node | null | undefined)[];
  /** Called once for an outside pointerdown. */
  onDismiss: () => void;
  /** Focus fallback for blank-area dismissal and removed popup focus. */
  getOpener?: () => HTMLElement | null | undefined;
  /** Optional document seam for tests and embedded documents. */
  document?: Document;
}

export type DismissibleLayerCleanup = () => void;

function nodeContains(container: Node, target: EventTarget | null): boolean {
  if (!target) return false;
  if (container === target) return true;
  return typeof (container as Node & { contains?: unknown }).contains === 'function'
    ? container.contains(target as Node)
    : false;
}

function nodeIsInside(node: Node | null, containers: readonly (Node | null | undefined)[]): boolean {
  if (!node) return false;
  return containers.some((container) => container !== null && container !== undefined && nodeContains(container, node));
}

function eventIsInside(
  event: PointerEvent,
  containers: readonly (Node | null | undefined)[]
): boolean {
  const path = typeof event.composedPath === 'function' ? event.composedPath() ?? [] : [];
  return containers.some((container) => {
    if (!container) return false;
    if (path.length > 0 && path.includes(container)) return true;
    return nodeContains(container, event.target);
  });
}

function scheduleAfterPointer(documentTarget: Document, callback: () => void): void {
  const view = documentTarget.defaultView;
  if (view && typeof view.requestAnimationFrame === 'function') {
    view.requestAnimationFrame(callback);
    return;
  }
  if (typeof globalThis.requestAnimationFrame === 'function') {
    globalThis.requestAnimationFrame(callback);
    return;
  }
  globalThis.setTimeout(callback, 0);
}

export function installDismissibleLayer(
  options: DismissibleLayerOptions
): DismissibleLayerCleanup {
  const documentTarget = options.document ?? globalThis.document;
  if (!documentTarget || typeof documentTarget.addEventListener !== 'function') return () => {};

  const onPointerDown = (event: PointerEvent) => {
    if (!options.isOpen() || eventIsInside(event, options.getInsideNodes())) return;

    const popupNodes = options.getPopupNodes?.() ?? options.getInsideNodes();
    const opener = options.getOpener?.();
    options.onDismiss();

    // Pointerdown runs before the browser moves focus to a clicked button. Wait
    // one paint so an outside action keeps focus; restore only for a blank
    // target, a removed menu item, or a document-body fallback.
    scheduleAfterPointer(documentTarget, () => {
      const active = documentTarget.activeElement;
      const focusStranded =
        !active ||
        active === documentTarget.body ||
        nodeIsInside(active, popupNodes) ||
        (active as Node & { isConnected?: boolean }).isConnected === false;
      if (
        focusStranded &&
        opener &&
        opener !== active &&
        opener.isConnected !== false &&
        typeof opener.focus === 'function'
      ) {
        opener.focus();
      }
    });
  };

  documentTarget.addEventListener('pointerdown', onPointerDown, true);
  return () => {
    if (typeof documentTarget.removeEventListener === 'function') {
      documentTarget.removeEventListener('pointerdown', onPointerDown, true);
    }
  };
}
