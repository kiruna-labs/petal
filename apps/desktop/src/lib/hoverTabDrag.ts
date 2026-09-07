import type { HoverTabSide } from '$lib/ipc';

export const HOVER_TAB_DRAG_THRESHOLD_PX = 6;
export const HOVER_TAB_DRAG_TAB_HEIGHT = 40;
export const HOVER_TAB_DRAG_TAB_WIDTH = 40;
const PERIMETER_SEGMENTS = 4;
export const HOVER_TAB_CORNER_INSET = 4;
const DEFAULT_HOVER_TAB_POSITION = 3 / 8;

type Point = { x: number; y: number };

export type HoverTabGesturePhase = 'pending' | 'dragging';

export interface HoverTabGesture {
  pointerId: number;
  startScreenX: number;
  startScreenY: number;
  originalPosition: number;
  startTabCenter: Point;
  sourceFrame: { x: number; y: number; width: number; height: number };
  tabWidth: number;
  tabHeight: number;
  grabOffset: Point;
  phase: HoverTabGesturePhase;
  /** Backend identity for this pointer gesture; absent before threshold. */
  dragToken?: number;
  /** Native attachment at pointer-down, used for terminal rollback. */
  originalAttachment: 'outside' | 'inset';
  /** Edge currently owning the drag; retained through the corner hysteresis. */
  currentSide: HoverTabSide;
}

export interface HoverTabGestureMove {
  gesture: HoverTabGesture;
  started: boolean;
  position: number | null;
}

/** State for one drag's latest-wins preview pump. */
export interface HoverTabPreviewState {
  inFlight: boolean;
  pendingPosition: number | null;
}

export function normalizeHoverTabPosition(position: number): number {
  if (!Number.isFinite(position)) return DEFAULT_HOVER_TAB_POSITION;
  const wrapped = position % 1;
  const normalized = wrapped < 0 ? wrapped + 1 : wrapped;
  return Math.round(normalized * 1e12) / 1e12;
}

export function hoverTabPositionForSide(side: HoverTabSide): number {
  const segment = side === 'top' ? 0 : side === 'right' ? 1 : side === 'bottom' ? 2 : 3;
  return (segment + 0.5) / PERIMETER_SEGMENTS;
}

/** Compatibility display values for the native route's edge-aware styling. */
export function hoverTabSideOffsetForPosition(position: number): {
  side: HoverTabSide;
  offset: number;
} {
  const scaled = normalizeHoverTabPosition(position) * PERIMETER_SEGMENTS;
  const segment = Math.min(PERIMETER_SEGMENTS - 1, Math.floor(scaled));
  const local = Math.min(1, Math.max(0, scaled - segment));
  if (segment === 0) return { side: 'top', offset: local };
  if (segment === 1) return { side: 'right', offset: local };
  if (segment === 2) return { side: 'bottom', offset: 1 - local };
  return { side: 'left', offset: 1 - local };
}

export function hoverTabSideForPosition(position: number): HoverTabSide {
  return hoverTabSideOffsetForPosition(position).side;
}

export function beginHoverTabGesture(
  pointerId: number,
  startScreenX: number,
  startScreenY: number,
  originalPosition: number,
  tabX: number,
  tabY: number,
  sourceFrame: { x: number; y: number; width: number; height: number },
  tabWidth = HOVER_TAB_DRAG_TAB_WIDTH,
  tabHeight = HOVER_TAB_DRAG_TAB_HEIGHT,
  originalAttachment: 'outside' | 'inset' = 'outside'
): HoverTabGesture {
  const width = Number.isFinite(tabWidth) && tabWidth > 0 ? tabWidth : HOVER_TAB_DRAG_TAB_WIDTH;
  const height = Number.isFinite(tabHeight) && tabHeight > 0 ? tabHeight : HOVER_TAB_DRAG_TAB_HEIGHT;
  const startTabCenter = { x: tabX + width / 2, y: tabY + height / 2 };
  return {
    pointerId,
    startScreenX,
    startScreenY,
    originalPosition: normalizeHoverTabPosition(originalPosition),
    startTabCenter,
    sourceFrame,
    tabWidth: width,
    tabHeight: height,
    grabOffset: {
      x: startScreenX - startTabCenter.x,
      y: startScreenY - startTabCenter.y
    },
    phase: 'pending',
    originalAttachment,
    currentSide: hoverTabSideForPosition(originalPosition)
  };
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

type EdgeCandidate = {
  side: HoverTabSide;
  local: number;
  center: Point;
  distance: number;
};

function edgeCandidate(
  point: Point,
  side: HoverTabSide,
  start: Point,
  end: Point
): EdgeCandidate {
  const dx = end.x - start.x;
  const dy = end.y - start.y;
  const lengthSquared = dx * dx + dy * dy;
  const local = lengthSquared > 0
    ? clamp(((point.x - start.x) * dx + (point.y - start.y) * dy) / lengthSquared, 0, 1)
    : 0;
  const center = { x: start.x + dx * local, y: start.y + dy * local };
  return {
    side,
    local,
    center,
    distance: Math.hypot(point.x - center.x, point.y - center.y)
  };
}

function edgeCandidates(
  point: Point,
  sourceFrame: { x: number; y: number; width: number; height: number },
  tabWidth: number,
  tabHeight: number
): EdgeCandidate[] {
  const left = sourceFrame.x;
  const top = sourceFrame.y;
  const right = left + sourceFrame.width;
  const bottom = top + sourceFrame.height;
  const inset = HOVER_TAB_CORNER_INSET * Math.max(tabWidth / 40, tabHeight / 40);
  const horizontalTravel = Math.max(0, sourceFrame.width - tabWidth - inset * 2);
  const verticalTravel = Math.max(0, sourceFrame.height - tabHeight - inset * 2);
  const halfWidth = tabWidth / 2;
  const halfHeight = tabHeight / 2;
  return [
    edgeCandidate(
      point,
      'top',
      { x: left + inset + halfWidth, y: top - halfHeight },
      { x: left + inset + halfWidth + horizontalTravel, y: top - halfHeight }
    ),
    edgeCandidate(
      point,
      'right',
      { x: right + halfWidth, y: top + inset + halfHeight },
      { x: right + halfWidth, y: top + inset + halfHeight + verticalTravel }
    ),
    edgeCandidate(
      point,
      'bottom',
      { x: right - inset - halfWidth, y: bottom + halfHeight },
      { x: right - inset - halfWidth - horizontalTravel, y: bottom + halfHeight }
    ),
    edgeCandidate(
      point,
      'left',
      { x: left - halfWidth, y: bottom - inset - halfHeight },
      { x: left - halfWidth, y: bottom - inset - halfHeight - verticalTravel }
    )
  ];
}

function positionForCandidate(candidate: EdgeCandidate): number {
  const segment = candidate.side === 'top' ? 0 : candidate.side === 'right' ? 1 : candidate.side === 'bottom' ? 2 : 3;
  return normalizeHoverTabPosition((segment + candidate.local) / PERIMETER_SEGMENTS);
}

export interface HoverTabProjection {
  position: number;
  side: HoverTabSide;
}

/** Project onto cardinal edge spans, retaining the current edge near corners. */
export function projectHoverTabCenterWithSide(
  point: Point,
  sourceFrame: { x: number; y: number; width: number; height: number },
  currentSide?: HoverTabSide,
  tabWidth = HOVER_TAB_DRAG_TAB_WIDTH,
  tabHeight = HOVER_TAB_DRAG_TAB_HEIGHT
): HoverTabProjection | null {
  if (
    !Number.isFinite(point.x) || !Number.isFinite(point.y) ||
    !Number.isFinite(sourceFrame.x) || !Number.isFinite(sourceFrame.y) ||
    !Number.isFinite(sourceFrame.width) || !Number.isFinite(sourceFrame.height) ||
    sourceFrame.width <= 0 || sourceFrame.height <= 0
  ) return null;
  const width = Number.isFinite(tabWidth) && tabWidth > 0 ? tabWidth : HOVER_TAB_DRAG_TAB_WIDTH;
  const height = Number.isFinite(tabHeight) && tabHeight > 0 ? tabHeight : HOVER_TAB_DRAG_TAB_HEIGHT;
  const candidates = edgeCandidates(point, sourceFrame, width, height);
  const closest = candidates.reduce((best, candidate) => candidate.distance < best.distance ? candidate : best);
  let selected = closest;
  if (currentSide && closest.side !== currentSide) {
    const current = candidates.find((candidate) => candidate.side === currentSide);
    if (current && closest.distance + HOVER_TAB_DRAG_THRESHOLD_PX >= current.distance) selected = current;
  }
  return { position: positionForCandidate(selected), side: selected.side };
}

export function projectHoverTabCenter(
  point: Point,
  sourceFrame: { x: number; y: number; width: number; height: number },
  tabWidth = HOVER_TAB_DRAG_TAB_WIDTH,
  tabHeight = HOVER_TAB_DRAG_TAB_HEIGHT
): number | null {
  return projectHoverTabCenterWithSide(point, sourceFrame, undefined, tabWidth, tabHeight)?.position ?? null;
}

/**
 * Apply one global screen-coordinate pointer sample. The desired tab center
 * follows the pointer's original grab point, then gets projected onto the
 * current cardinal edge. The current edge is retained until the adjacent
 * edge is closer by the drag hysteresis, preventing corner chatter.
 */
export function moveHoverTabGesture(
  gesture: HoverTabGesture,
  screenX: number,
  screenY: number
): HoverTabGestureMove {
  const distance = Math.hypot(screenX - gesture.startScreenX, screenY - gesture.startScreenY);
  if (gesture.phase === 'pending' && distance < HOVER_TAB_DRAG_THRESHOLD_PX) {
    return { gesture, started: false, position: null };
  }

  const nextGesture: HoverTabGesture =
    gesture.phase === 'dragging' ? gesture : { ...gesture, phase: 'dragging' };
  const desiredCenter = {
    x: screenX - gesture.grabOffset.x,
    y: screenY - gesture.grabOffset.y
  };
  const projection = projectHoverTabCenterWithSide(
    desiredCenter,
    gesture.sourceFrame,
    gesture.currentSide,
    gesture.tabWidth,
    gesture.tabHeight
  );
  const projectedGesture = projection
    ? { ...nextGesture, currentSide: projection.side }
    : nextGesture;
  return {
    gesture: projectedGesture,
    started: gesture.phase !== 'dragging',
    position: projection?.position ?? null
  };
}

export function cancelHoverTabGesture(gesture: HoverTabGesture | null): number | null {
  return gesture ? gesture.originalPosition : null;
}

export function isHoverTabDragging(gesture: HoverTabGesture | null): boolean {
  return gesture?.phase === 'dragging';
}

export function createHoverTabPreviewState(): HoverTabPreviewState {
  return { inFlight: false, pendingPosition: null };
}

/** Offer one perimeter preview to the latest-wins IPC pump. */
export function offerHoverTabPreview(
  state: HoverTabPreviewState,
  position: number
): number | null {
  const normalized = normalizeHoverTabPosition(position);
  if (state.inFlight) {
    state.pendingPosition = normalized;
    return null;
  }
  state.inFlight = true;
  return normalized;
}

export function takeHoverTabPreview(state: HoverTabPreviewState): number | null {
  const position = state.pendingPosition;
  state.pendingPosition = null;
  return position;
}

/** Mark the current preview command settled and return its latest successor. */
export function settleHoverTabPreview(state: HoverTabPreviewState): number | null {
  state.inFlight = false;
  return takeHoverTabPreview(state);
}

/** Clear unsent work without pretending an in-flight IPC command has settled. */
export function clearHoverTabPreview(state: HoverTabPreviewState): void {
  state.pendingPosition = null;
}

/** Serialize native drag commands while preserving rejection isolation. */
export function createSerializedHoverTabCommandQueue<T, R>(
  run: (command: T) => Promise<R>
): (command: T) => Promise<R> {
  let tail: Promise<unknown> = Promise.resolve();
  return (command: T) => {
    const next = tail.then(() => run(command));
    tail = next.then(
      () => undefined,
      () => undefined
    );
    return next;
  };
}
