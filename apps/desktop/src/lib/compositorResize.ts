import { invoke } from '@tauri-apps/api/core';
import { COMMANDS } from '$lib/ipc';

export type CompositorResizeDirection =
  | 'North'
  | 'East'
  | 'South'
  | 'West'
  | 'NorthEast'
  | 'NorthWest'
  | 'SouthEast'
  | 'SouthWest';

export interface CompositorResizeFrame {
  x: number;
  y: number;
  width: number;
  height: number;
}

type PendingDelta = {
  deltaX: number;
  deltaY: number;
};

export async function beginCompositorResizeDrag(
  event: PointerEvent,
  windowId: number,
  ownerIdentity: string | undefined,
  direction: CompositorResizeDirection
) {
  if (event.button !== 0 || !Number.isFinite(windowId) || windowId <= 0) return;

  event.preventDefault();
  event.stopPropagation();

  const target = event.currentTarget instanceof HTMLElement ? event.currentTarget : null;
  try {
    target?.setPointerCapture(event.pointerId);
  } catch {
    // Best effort: pointer capture can fail for synthetic events.
  }

  let cancelledBeforeStart = false;
  function cancelBeforeStart(pointerEvent: PointerEvent) {
    cancelledBeforeStart = true;
    pointerEvent.preventDefault();
    pointerEvent.stopPropagation();
    window.removeEventListener('pointerup', cancelBeforeStart);
    window.removeEventListener('pointercancel', cancelBeforeStart);
    try {
      target?.releasePointerCapture(pointerEvent.pointerId);
    } catch {
      // Safe to ignore: capture may already be released.
    }
  }

  window.addEventListener('pointerup', cancelBeforeStart);
  window.addEventListener('pointercancel', cancelBeforeStart);

  let startFrame: CompositorResizeFrame;
  try {
    startFrame = await invoke<CompositorResizeFrame>(COMMANDS.compositorBeginResize, { windowId, ownerIdentity });
  } catch {
    window.removeEventListener('pointerup', cancelBeforeStart);
    window.removeEventListener('pointercancel', cancelBeforeStart);
    return;
  }
  window.removeEventListener('pointerup', cancelBeforeStart);
  window.removeEventListener('pointercancel', cancelBeforeStart);
  if (cancelledBeforeStart) {
    // compositor_begin_resize already marked the backend's resize gesture
    // active before we could learn the pointer was released/cancelled.
    // Without this, that flag would suppress source-size reconciliation
    // until the native backstop timeout (#416 review finding) -- send an
    // immediate no-op finalize so it clears right away instead.
    invoke(COMMANDS.compositorResizeWindow, {
      windowId,
      ownerIdentity,
      direction,
      startX: startFrame.x,
      startY: startFrame.y,
      startWidth: startFrame.width,
      startHeight: startFrame.height,
      deltaX: 0,
      deltaY: 0,
      finalize: true
    }).catch(() => {});
    return;
  }

  const startScreenX = event.screenX;
  const startScreenY = event.screenY;
  let pending: PendingDelta | null = null;
  let lastDelta: PendingDelta = { deltaX: 0, deltaY: 0 };
  let frame = 0;

  function flushResize(finalize = false) {
    frame = 0;
    const delta = pending;
    pending = null;
    if (delta) lastDelta = delta;
    if (!delta && !finalize) return;
    const appliedDelta = delta ?? lastDelta;
    invoke(COMMANDS.compositorResizeWindow, {
      windowId,
      ownerIdentity,
      direction,
      startX: startFrame.x,
      startY: startFrame.y,
      startWidth: startFrame.width,
      startHeight: startFrame.height,
      deltaX: appliedDelta.deltaX,
      deltaY: appliedDelta.deltaY,
      finalize
    }).catch(() => {});
  }

  function scheduleResize(pointerEvent: PointerEvent) {
    pointerEvent.preventDefault();
    pointerEvent.stopPropagation();
    pending = {
      deltaX: pointerEvent.screenX - startScreenX,
      deltaY: pointerEvent.screenY - startScreenY
    };
    if (!frame) frame = requestAnimationFrame(() => flushResize());
  }

  function stopResize(pointerEvent: PointerEvent) {
    pointerEvent.preventDefault();
    pointerEvent.stopPropagation();
    if (frame) {
      cancelAnimationFrame(frame);
      frame = 0;
    }
    flushResize(true);
    window.removeEventListener('pointermove', scheduleResize);
    window.removeEventListener('pointerup', stopResize);
    window.removeEventListener('pointercancel', stopResize);
    try {
      target?.releasePointerCapture(pointerEvent.pointerId);
    } catch {
      // Safe to ignore: capture may already be released.
    }
  }

  window.addEventListener('pointermove', scheduleResize);
  window.addEventListener('pointerup', stopResize);
  window.addEventListener('pointercancel', stopResize);
}
