export const HOVER_TAB_DRAG_THRESHOLD_PX = 6;
export const HOVER_TAB_DRAG_TAB_HEIGHT = 40;

export type HoverTabGesturePhase = 'pending' | 'dragging';

export interface HoverTabGesture {
  pointerId: number;
  startScreenX: number;
  startScreenY: number;
  originalOffset: number;
  phase: HoverTabGesturePhase;
}

export interface HoverTabGestureMove {
  gesture: HoverTabGesture;
  started: boolean;
  offset: number | null;
}

/** State for one drag's latest-wins preview pump. */
export interface HoverTabPreviewState {
  inFlight: boolean;
  pendingOffset: number | null;
}

export function clampHoverTabOffset(offset: number): number {
  return Number.isFinite(offset) ? Math.min(1, Math.max(0, offset)) : 0.5;
}

export function beginHoverTabGesture(
  pointerId: number,
  startScreenX: number,
  startScreenY: number,
  originalOffset: number
): HoverTabGesture {
  return {
    pointerId,
    startScreenX,
    startScreenY,
    originalOffset: clampHoverTabOffset(originalOffset),
    phase: 'pending'
  };
}

/**
 * Apply one global screen-coordinate pointer sample. Screen coordinates stay
 * stable while the native 40px tab moves; client coordinates would move with
 * the webview and feed the tab's own displacement back into this delta.
 * A pending gesture remains a click until the Euclidean movement reaches the
 * threshold; once dragging, vertical movement maps to source-relative travel.
 */
export function moveHoverTabGesture(
  gesture: HoverTabGesture,
  screenX: number,
  screenY: number,
  sourceHeight: number,
  tabHeight = HOVER_TAB_DRAG_TAB_HEIGHT
): HoverTabGestureMove {
  const distance = Math.hypot(screenX - gesture.startScreenX, screenY - gesture.startScreenY);
  if (gesture.phase === 'pending' && distance < HOVER_TAB_DRAG_THRESHOLD_PX) {
    return { gesture, started: false, offset: null };
  }

  const nextGesture: HoverTabGesture =
    gesture.phase === 'dragging' ? gesture : { ...gesture, phase: 'dragging' };
  const travel = Math.max(sourceHeight - tabHeight, 1);
  const offset = clampHoverTabOffset(
    gesture.originalOffset + (screenY - gesture.startScreenY) / travel
  );
  return { gesture: nextGesture, started: gesture.phase !== 'dragging', offset };
}

export function cancelHoverTabGesture(gesture: HoverTabGesture | null): number | null {
  return gesture ? gesture.originalOffset : null;
}

export function isHoverTabDragging(gesture: HoverTabGesture | null): boolean {
  return gesture?.phase === 'dragging';
}

export function createHoverTabPreviewState(): HoverTabPreviewState {
  return { inFlight: false, pendingOffset: null };
}

/**
 * Offer one preview to the pump. A command already in flight keeps only this
 * newest normalized offset; otherwise the caller receives the offset to send.
 */
export function offerHoverTabPreview(
  state: HoverTabPreviewState,
  offset: number
): number | null {
  const normalized = clampHoverTabOffset(offset);
  if (state.inFlight) {
    state.pendingOffset = normalized;
    return null;
  }
  state.inFlight = true;
  return normalized;
}

export function takeHoverTabPreview(state: HoverTabPreviewState): number | null {
  const offset = state.pendingOffset;
  state.pendingOffset = null;
  return offset;
}

/** Mark the current preview command settled and return its latest successor. */
export function settleHoverTabPreview(state: HoverTabPreviewState): number | null {
  state.inFlight = false;
  return takeHoverTabPreview(state);
}

/** Clear unsent work without pretending an in-flight IPC command has settled. */
export function clearHoverTabPreview(state: HoverTabPreviewState): void {
  state.pendingOffset = null;
}
