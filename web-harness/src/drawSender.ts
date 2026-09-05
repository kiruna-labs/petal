import type { HarnessContext } from './context.ts';
import { drawPublishOptions, MAX_DRAW_TEXT_CHARS } from './draw.ts';
import { DRAW_TOPIC, identityPaletteIndexFromMetadata, type DrawMessage, type DrawPoint } from './trackNames.ts';
import { colorForIdentity, mediaContentRect, normalizedPointInContainedMedia } from './telepointer.ts';

export const MAX_DRAW_POINTS_PER_MESSAGE = 128;
export const DRAW_FLUSH_MS = 50;

export interface DrawTarget {
  ownerIdentity: string;
  windowId: number;
}

export interface ActiveDrawStroke {
  target: DrawTarget;
  strokeId: string;
}

interface DrawMessageBuilderOptions {
  createStrokeId?: (target: DrawTarget) => string;
}

const drawEncoder = new TextEncoder();

function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}

function clampDrawPoint(point: DrawPoint): DrawPoint {
  return {
    x: clamp01(point.x),
    y: clamp01(point.y),
  };
}

function defaultStrokeId(target: DrawTarget): string {
  const fallbackRandom = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  const random = globalThis.crypto?.randomUUID?.() ?? fallbackRandom;
  return `web-draw-${target.windowId}-${random}`;
}

export function chunkDrawPoints(points: DrawPoint[]): DrawPoint[][] {
  const chunks: DrawPoint[][] = [];
  for (let index = 0; index < points.length; index += MAX_DRAW_POINTS_PER_MESSAGE) {
    chunks.push(points.slice(index, index + MAX_DRAW_POINTS_PER_MESSAGE).map(clampDrawPoint));
  }
  return chunks;
}

export function penCursor(color: string): string {
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="28" height="28" viewBox="0 0 28 28"><path d="M5 23l2.3-7.1L18.9 4.3a2.6 2.6 0 0 1 3.7 3.7L11 19.6 5 23Z" fill="${color}" stroke="#071018" stroke-width="1.8" stroke-linejoin="round"/><path d="M16.8 6.4l4.8 4.8" stroke="#ffffff" stroke-width="1.4" stroke-linecap="round" opacity=".9"/></svg>`;
  return `url("data:image/svg+xml,${encodeURIComponent(svg)}") 5 23, crosshair`;
}

/**
 * #892: normalize against the tile's `<video>` content box, never the bare
 * tile -- a header-bearing tile insets its video 44px+1px, so the tile rect
 * sent every stroke offset on the sharer. Matches `(video ?? tile)` in
 * telepointerSender.ts:75. Exported (not closure-bound) so tests exercise
 * the real capture path.
 */
export function pointForTile(tile: HTMLDivElement, event: Pick<PointerEvent, 'clientX' | 'clientY'>): DrawPoint | null {
  const { bounds, media } = mediaContentRect(tile);
  return normalizedPointInContainedMedia(bounds, media, { x: event.clientX, y: event.clientY });
}

export function drawTargetFromTile(tile: HTMLDivElement): DrawTarget | null {
  const ownerIdentity = tile.dataset.owner?.trim() ?? '';
  const windowId = Number(tile.dataset.drawWindowId ?? tile.dataset.windowId);
  if (!ownerIdentity || !Number.isSafeInteger(windowId) || windowId < 1 || windowId > 0xffff_ffff) return null;
  return { ownerIdentity, windowId };
}

export const DRAWABLE_TILE_SELECTOR =
  '.tile[data-owner][data-draw-window-id], .share-tile[data-owner][data-window-id]';

export const DRAW_UNAVAILABLE_LABEL = 'Nothing to draw on';
export const DRAW_IDLE_TOOLTIP = 'Draw';
export const DRAW_ENABLE_LABEL = 'Enable drawing';
export const DRAW_DISABLE_LABEL = 'Disable drawing';

export function hasDrawableTarget(root: {
  querySelectorAll: (selector: string) => ArrayLike<Pick<HTMLDivElement, 'dataset'>>;
}): boolean {
  const tiles = root.querySelectorAll(DRAWABLE_TILE_SELECTOR);
  for (let index = 0; index < tiles.length; index++) {
    if (drawTargetFromTile(tiles[index] as HTMLDivElement)) return true;
  }
  return false;
}

export function drawControlCopy(
  available: boolean,
  drawMode: boolean
): { disabled: boolean; ariaLabel: string; title: string; tooltip: string } {
  if (!available) {
    return {
      disabled: true,
      ariaLabel: DRAW_UNAVAILABLE_LABEL,
      title: DRAW_UNAVAILABLE_LABEL,
      tooltip: DRAW_UNAVAILABLE_LABEL,
    };
  }
  if (drawMode) {
    return {
      disabled: false,
      ariaLabel: DRAW_DISABLE_LABEL,
      title: DRAW_DISABLE_LABEL,
      tooltip: DRAW_IDLE_TOOLTIP,
    };
  }
  return {
    disabled: false,
    ariaLabel: DRAW_ENABLE_LABEL,
    title: DRAW_ENABLE_LABEL,
    tooltip: DRAW_IDLE_TOOLTIP,
  };
}

export function createDrawMessageBuilder(options: DrawMessageBuilderOptions = {}) {
  let seq = 0;
  const createStrokeId = options.createStrokeId ?? defaultStrokeId;

  function nextSeq(): number {
    seq = seq >= Number.MAX_SAFE_INTEGER ? 1 : seq + 1;
    return seq;
  }

  function message(
    target: DrawTarget,
    type: 'begin' | 'points' | 'end',
    strokeId: string,
    points: DrawPoint[]
  ): DrawMessage {
    return {
      v: 1,
      type,
      ownerIdentity: target.ownerIdentity,
      windowId: target.windowId,
      seq: nextSeq(),
      strokeId,
      points: points.map(clampDrawPoint),
    };
  }

  function begin(target: DrawTarget, point: DrawPoint): ActiveDrawStroke & { message: DrawMessage } {
    const strokeId = createStrokeId(target).trim();
    return {
      target,
      strokeId,
      message: message(target, 'begin', strokeId, [point]),
    };
  }

  function points(stroke: ActiveDrawStroke, pendingPoints: DrawPoint[]): DrawMessage[] {
    return chunkDrawPoints(pendingPoints).map((chunk) => message(stroke.target, 'points', stroke.strokeId, chunk));
  }

  function end(stroke: ActiveDrawStroke, point: DrawPoint | null): DrawMessage {
    return message(stroke.target, 'end', stroke.strokeId, point ? [point] : []);
  }

  function text(target: DrawTarget, point: DrawPoint, value: string): DrawMessage {
    const trimmed = value.replace(/[\n\r\u2028\u2029]/gu, '').slice(0, MAX_DRAW_TEXT_CHARS);
    return {
      v: 1,
      type: 'text',
      ownerIdentity: target.ownerIdentity,
      windowId: target.windowId,
      seq: nextSeq(),
      strokeId: `text-${target.windowId}-${Date.now().toString(36)}`,
      points: [clampDrawPoint(point)],
      text: trimmed,
    };
  }

  return {
    begin,
    points,
    end,
    text,
  };
}

export function setupDrawSender(ctx: HarnessContext) {
  const { dom, state } = ctx;
  const { ctlDraw, ctlDrawLabel, tilesEl } = dom;
  const { logEvent, showToast } = ctx.ui;
  const builder = createDrawMessageBuilder();

  let drawMode = false;
  let activePointerId: number | null = null;
  let activeTile: HTMLDivElement | null = null;
  let activeStroke: ActiveDrawStroke | null = null;
  let pendingDrawPoints: DrawPoint[] = [];
  let drawFlushTimer: ReturnType<typeof setTimeout> | null = null;
  let textTarget: DrawTarget | null = null;
  let textAnchor: DrawPoint | null = null;
  let textDraft = '';
  let composing = false;

  function applyDrawControlCopy() {
    const copy = drawControlCopy(hasDrawableTarget(tilesEl), drawMode);
    ctlDraw.disabled = copy.disabled;
    ctlDraw.setAttribute('aria-label', copy.ariaLabel);
    ctlDraw.title = copy.title;
    ctlDrawLabel.textContent = copy.tooltip;
  }

  function setDrawMode(on: boolean) {
    const next = on && hasDrawableTarget(tilesEl);
    if (drawMode === next) {
      applyDrawControlCopy();
      return;
    }
    if (!next) {
      commitText();
      clearDrawInput();
    }
    drawMode = next;
    tilesEl.classList.toggle('draw-mode-active', drawMode);
    const drawerIdentity = state.room?.localParticipant.identity.trim() ?? '';
    if (drawMode && drawerIdentity) {
      tilesEl.style.setProperty(
        '--draw-cursor',
        penCursor(colorForIdentity(drawerIdentity, identityPaletteIndexFromMetadata(state.room?.localParticipant.metadata)))
      );
    } else {
      tilesEl.style.removeProperty('--draw-cursor');
    }
    ctlDraw.classList.toggle('draw-active', drawMode);
    ctlDrawLabel.classList.toggle('on', drawMode);
    ctlDraw.setAttribute('aria-pressed', drawMode ? 'true' : 'false');
    applyDrawControlCopy();
    if (drawMode) {
      ctx.cb.stopRemoteControl('drawing enabled');
      showToast('Draw mode on');
      logEvent('draw mode enabled', 'ok');
    } else {
      logEvent('draw mode disabled');
    }
  }

  function syncDrawAvailability() {
    if (drawMode && !hasDrawableTarget(tilesEl)) {
      setDrawMode(false);
      return;
    }
    applyDrawControlCopy();
  }

  function toggleDrawMode() {
    if (ctlDraw.disabled) return;
    setDrawMode(!drawMode);
  }

  function tileFromEvent(event: Event): HTMLDivElement | null {
    const target = event.target as Element | null;
    if (target?.closest('button, a, input, label, summary')) return null;
    return target?.closest<HTMLDivElement>(DRAWABLE_TILE_SELECTOR) ?? null;
  }

  function publishDrawMessage(message: DrawMessage) {
    const room = state.room;
    const drawerIdentity = room?.localParticipant.identity.trim();
    if (!room || !drawerIdentity) return;

    const bytes = drawEncoder.encode(JSON.stringify(message));
    ctx.cb.handleRemoteDrawPayload(bytes, drawerIdentity, DRAW_TOPIC);
    room.localParticipant.publishData(bytes, drawPublishOptions()).catch((err) => {
      if (message.type !== 'points') {
        logEvent(`draw publish failed: ${(err as Error).message ?? err}`, 'warn');
      }
    });
  }

  async function publishCockpitDrawStroke(): Promise<{ windowId: number }> {
    const room = state.room;
    const drawerIdentity = room?.localParticipant.identity.trim();
    if (!room || !drawerIdentity) throw new Error('draw requires an active room');
    const tile = dom.tilesEl.querySelector<HTMLDivElement>('.share-tile[data-owner][data-window-id]');
    if (!tile) throw new Error('draw requires a remote share tile');
    const target = drawTargetFromTile(tile);
    if (!target) throw new Error('draw target could not be resolved from remote share tile');
    const begin = builder.begin(target, { x: 0.25, y: 0.25 });
    const end = builder.end(begin, { x: 0.75, y: 0.75 });
    const messages = [begin.message, end];
    for (const message of messages) {
      const bytes = drawEncoder.encode(JSON.stringify(message));
      ctx.cb.handleRemoteDrawPayload(bytes, drawerIdentity, DRAW_TOPIC);
      await room.localParticipant.publishData(bytes, drawPublishOptions());
    }
    return { windowId: target.windowId };
  }

  function flushPendingDrawPoints() {
    if (drawFlushTimer) {
      clearTimeout(drawFlushTimer);
      drawFlushTimer = null;
    }
    const stroke = activeStroke;
    const points = pendingDrawPoints;
    pendingDrawPoints = [];
    if (!stroke || points.length === 0) return;
    for (const message of builder.points(stroke, points)) publishDrawMessage(message);
  }

  function scheduleDrawFlush() {
    if (drawFlushTimer) return;
    drawFlushTimer = setTimeout(() => {
      drawFlushTimer = null;
      flushPendingDrawPoints();
    }, DRAW_FLUSH_MS);
  }

  function clearDrawInput() {
    pendingDrawPoints = [];
    if (drawFlushTimer) {
      clearTimeout(drawFlushTimer);
      drawFlushTimer = null;
    }
    activePointerId = null;
    activeTile = null;
    activeStroke = null;
  }

  function clearText() {
    textTarget = null;
    textAnchor = null;
    textDraft = '';
  }

  function commitText() {
    if (textTarget && textAnchor && textDraft.trim()) {
      publishDrawMessage(builder.text(textTarget, textAnchor, textDraft));
    }
    clearText();
  }

  function appendText(value: string) {
    if (!drawMode || !textTarget || !textAnchor) return;
    textDraft = [...`${textDraft}${value}`]
      .filter((character) => !/[\n\r\u2028\u2029]/u.test(character))
      .slice(0, MAX_DRAW_TEXT_CHARS)
      .join('');
  }

  function handleKeyDown(event: KeyboardEvent) {
    if (!drawMode || composing || event.isComposing || !textAnchor) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === 'Enter') return;
    if (event.key === 'Escape') {
      clearText();
      return;
    }
    if (event.key === 'Backspace') {
      textDraft = [...textDraft].slice(0, -1).join('');
      if (!textDraft) clearText();
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || event.key.length !== 1) return;
    appendText(event.key);
  }

  function handleCompositionStart() {
    composing = true;
  }

  function handleCompositionEnd(event: CompositionEvent) {
    composing = false;
    appendText(event.data ?? '');
  }

  function handlePointerDown(event: PointerEvent) {
    if (!drawMode || !state.room || activeStroke) return;
    const tile = tileFromEvent(event);
    if (!tile) return;
    const target = drawTargetFromTile(tile);
    const point = pointForTile(tile, event);
    if (!target || !point) return;

    commitText();
    textTarget = target;
    textAnchor = point;
    event.preventDefault();
    event.stopPropagation();
    tile.focus({ preventScroll: true });
    activePointerId = event.pointerId;
    activeTile = tile;
    const begin = builder.begin(target, point);
    activeStroke = { target: begin.target, strokeId: begin.strokeId };
    try {
      tile.setPointerCapture(event.pointerId);
    } catch {
      // Pointer capture can fail for synthetic events; the active pointer id still gates input.
    }
    publishDrawMessage(begin.message);
  }

  function handlePointerMove(event: PointerEvent) {
    if (!drawMode) return;
    if (!activeStroke) {
      const tile = tileFromEvent(event);
      const target = tile ? drawTargetFromTile(tile) : null;
      const point = tile ? pointForTile(tile, event) : null;
      if (!tile || !target || !point) return;
      tile.focus({ preventScroll: true });
      if (textAnchor && textTarget && textTarget.windowId === target.windowId && textTarget.ownerIdentity === target.ownerIdentity) {
        // #892: pixelize the normalized delta against the video content box
        // (not the tile) -- cosmetic (only gates the re-commit threshold),
        // but a bare tile rect is wrong by the same header-inset margin.
        const { bounds } = mediaContentRect(tile);
        const moved = Math.hypot(
          (point.x - textAnchor.x) * bounds.width,
          (point.y - textAnchor.y) * bounds.height
        ) >= 6;
        if (moved) commitText();
      }
      textTarget = target;
      textAnchor = textAnchor && textDraft ? textAnchor : point;
      return;
    }
    if (activePointerId !== event.pointerId) return;
    const point = activeTile ? pointForTile(activeTile, event) : null;
    if (!point) return;

    event.preventDefault();
    event.stopPropagation();
    pendingDrawPoints.push(point);
    scheduleDrawFlush();
  }

  function handlePointerUp(event: PointerEvent) {
    if (!drawMode || !activeStroke || activePointerId !== event.pointerId) return;
    const stroke = activeStroke;
    const tile = activeTile;
    const point = tile ? pointForTile(tile, event) : null;

    event.preventDefault();
    event.stopPropagation();
    flushPendingDrawPoints();
    publishDrawMessage(builder.end(stroke, point));
    if (tile) {
      try {
        tile.releasePointerCapture(event.pointerId);
      } catch {
        // Safe to ignore: the pointer may not be captured anymore.
      }
    }
    clearDrawInput();
  }

  function handleClick(event: MouseEvent) {
    if (!drawMode || !tileFromEvent(event)) return;
    event.preventDefault();
    event.stopPropagation();
  }

  function installDrawSender() {
    ctlDraw.addEventListener('click', toggleDrawMode);
    tilesEl.addEventListener('pointerdown', handlePointerDown, { capture: true });
    tilesEl.addEventListener('pointermove', handlePointerMove, { capture: true });
    tilesEl.addEventListener('pointerup', handlePointerUp, { capture: true });
    tilesEl.addEventListener('pointercancel', handlePointerUp, { capture: true });
    tilesEl.addEventListener('click', handleClick, { capture: true });
    tilesEl.addEventListener('keydown', handleKeyDown, { capture: true });
    tilesEl.addEventListener('compositionstart', handleCompositionStart, { capture: true });
    tilesEl.addEventListener('compositionend', handleCompositionEnd, { capture: true });
  }

  setDrawMode(false);
  installDrawSender();
  syncDrawAvailability();

  return {
    setDrawMode,
    syncDrawAvailability,
    publishCockpitDrawStroke,
  };
}
