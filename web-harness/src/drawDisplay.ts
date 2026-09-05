import type { HarnessContext } from './context.ts';
import { DRAW_TOPIC, identityPaletteIndexFromMetadata, type DrawMessage, type DrawPoint } from './trackNames.ts';
import { colorForIdentity, containedMediaRect, mediaContentRectRelativeToTile } from './telepointer.ts';
import { parseDrawPayload } from './draw.ts';
import { isStrokeExpired, strokeFadeOpacity } from '@petal/shared/logic/strokeExpiry';

/** #670: how often the fade/expiry sweep re-checks stroke ages -- matches
 * the native compositor pointer overlay's telepointer/draw sweep cadence
 * (apps/desktop/src/routes/compositor/pointer/+page.svelte). */
const DRAW_FADE_SWEEP_MS = 250;

interface RemoteDrawStroke {
  drawerIdentity: string;
  ownerIdentity: string;
  windowId: number;
  strokeId: string;
  points: DrawPoint[];
  path: SVGPathElement | null;
  /** Date.now() of the last point received for this stroke (#670) -- ages
   * from the LAST point, not the first, so a stroke still being actively
   * extended never starts fading mid-draw. Client-side only; no wire-format
   * change. */
  lastPointAtMs: number;
}

interface RemoteDrawText {
  drawerIdentity: string;
  ownerIdentity: string;
  windowId: number;
  annotationId: string;
  anchor: DrawPoint;
  text: string;
  element: HTMLDivElement | null;
  lastPointAtMs: number;
}

function drawKey(message: Pick<DrawMessage, 'ownerIdentity' | 'windowId'>, drawerIdentity: string, strokeId: string) {
  return `${drawerIdentity}:${message.ownerIdentity}:${message.windowId}:${strokeId}`;
}

function tileMatchesDrawTarget(tile: HTMLDivElement, ownerIdentity: string, windowId: number): boolean {
  return tile.dataset.owner === ownerIdentity && Number(tile.dataset.drawWindowId ?? tile.dataset.windowId) === windowId;
}

function tileForDrawTarget(ownerIdentity: string, windowId: number): HTMLDivElement | null {
  return (
    Array.from(document.querySelectorAll<HTMLDivElement>('.tile[data-owner]')).find((tile) =>
      tileMatchesDrawTarget(tile, ownerIdentity, windowId)
    ) ?? null
  );
}

function pointInTile(tile: HTMLDivElement, point: DrawPoint): DrawPoint {
  // #892: bounds must be the video's content box relative to the tile (its
  // real offset under a docked header), not `{0,0,tileW,tileH}` -- that bare
  // tile box is what sent the local echo offset the same wrong way as the
  // outgoing capture, so the two errors cancelled and only the sharer saw it.
  const { bounds, media } = mediaContentRectRelativeToTile(tile);
  const content = containedMediaRect(bounds, media);
  return {
    x: content.left + content.width * point.x,
    y: content.top + content.height * point.y,
  };
}

function strokePathData(tile: HTMLDivElement, points: DrawPoint[]): string {
  return points
    .map((point, index) => {
      const projected = pointInTile(tile, point);
      return `${index === 0 ? 'M' : 'L'} ${projected.x.toFixed(1)} ${projected.y.toFixed(1)}`;
    })
    .join(' ');
}

function ensureDrawLayer(tile: HTMLDivElement): SVGSVGElement {
  let layer = tile.querySelector<SVGSVGElement>('.remote-draw-layer');
  if (!layer) {
    layer = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    layer.classList.add('remote-draw-layer');
    layer.setAttribute('aria-hidden', 'true');
    tile.appendChild(layer);
  }
  const rect = tile.getBoundingClientRect();
  layer.setAttribute('viewBox', `0 0 ${Math.max(1, rect.width).toFixed(1)} ${Math.max(1, rect.height).toFixed(1)}`);
  return layer;
}

export function setupDrawDisplay(ctx: HarnessContext) {
  const strokes = new Map<string, RemoteDrawStroke>();
  const texts = new Map<string, RemoteDrawText>();

  function makePath(stroke: RemoteDrawStroke, tile: HTMLDivElement): SVGPathElement {
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.classList.add('remote-draw-stroke');
    path.dataset.drawer = stroke.drawerIdentity;
    path.dataset.owner = stroke.ownerIdentity;
    path.dataset.windowId = String(stroke.windowId);
    path.dataset.strokeId = stroke.strokeId;
    path.setAttribute('d', strokePathData(tile, stroke.points));
    return path;
  }

  function paletteIndexForIdentity(identity: string): number | null {
    const room = ctx.state?.room;
    if (!room) return null;
    if (room.localParticipant.identity === identity) {
      return identityPaletteIndexFromMetadata(room.localParticipant.metadata);
    }
    return identityPaletteIndexFromMetadata(room.remoteParticipants.get(identity)?.metadata);
  }

  function renderText(text: RemoteDrawText) {
    const tile = tileForDrawTarget(text.ownerIdentity, text.windowId);
    if (!tile) return;
    const point = pointInTile(tile, text.anchor);
    if (!text.element || text.element.parentNode !== tile) {
      text.element?.remove();
      text.element = document.createElement('div');
      text.element.className = 'remote-draw-text';
      text.element.dataset.drawer = text.drawerIdentity;
      text.element.dataset.owner = text.ownerIdentity;
      text.element.dataset.windowId = String(text.windowId);
      text.element.dataset.annotationId = text.annotationId;
      tile.appendChild(text.element);
    }
    text.element.textContent = text.text;
    text.element.style.setProperty('left', `${point.x}px`);
    text.element.style.setProperty('top', `${point.y}px`);
    const rect = tile.getBoundingClientRect();
    const alignRight = text.anchor.x > 0.62;
    const available = Math.max(24, alignRight ? point.x : rect.width - point.x);
    const estimatedTextWidth = Math.max(8, [...text.text].length * 8.5 + 14);
    const horizontalScale = Math.min(1, available / estimatedTextWidth);
    text.element.style.setProperty(
      'transform',
      `translate(${alignRight ? '-100%' : '0'}, -50%) scaleX(${horizontalScale})`
    );
    text.element.style.setProperty('transform-origin', `${alignRight ? 'right' : 'left'} center`);
    text.element.style.setProperty('color', colorForIdentity(text.drawerIdentity, paletteIndexForIdentity(text.drawerIdentity)));
    text.element.style.setProperty('opacity', String(strokeFadeOpacity(Date.now() - text.lastPointAtMs)));
  }

  function renderStroke(stroke: RemoteDrawStroke) {
    if (stroke.points.length === 0) return;
    const tile = tileForDrawTarget(stroke.ownerIdentity, stroke.windowId);
    if (!tile) return;
    const layer = ensureDrawLayer(tile);
    if (!stroke.path || stroke.path.parentNode !== layer) {
      stroke.path?.remove();
      stroke.path = makePath(stroke, tile);
      layer.appendChild(stroke.path);
    } else {
      stroke.path.setAttribute('d', strokePathData(tile, stroke.points));
    }
    stroke.path.style.setProperty('--draw-color', colorForIdentity(stroke.drawerIdentity, paletteIndexForIdentity(stroke.drawerIdentity)));
    applyStrokeFadeOpacity(stroke);
  }

  /** #670: set the stroke's rendered opacity from its current age (time
   * since its last point). Called both on every render (new points) and by
   * the periodic sweep below (idle strokes still need to keep fading). */
  function applyStrokeFadeOpacity(stroke: RemoteDrawStroke) {
    if (!stroke.path) return;
    const age = Date.now() - stroke.lastPointAtMs;
    stroke.path.style.opacity = String(strokeFadeOpacity(age));
  }

  function removeText(key: string) {
    const text = texts.get(key);
    text?.element?.remove();
    texts.delete(key);
  }

  /** #670: age out drawn strokes 10s after their LAST point (SPEC.md
   * "ephemeral by default"). Runs independently of new draw traffic so an
   * idle stroke still fades and is eventually removed. */
  function sweepExpiredStrokes() {
    const now = Date.now();
    Array.from(strokes.entries()).forEach(([key, stroke]) => {
      const age = now - stroke.lastPointAtMs;
      if (isStrokeExpired(age)) {
        removeStroke(key);
      } else {
        applyStrokeFadeOpacity(stroke);
      }
    });
    Array.from(texts.entries()).forEach(([key, text]) => {
      const age = now - text.lastPointAtMs;
      if (isStrokeExpired(age)) {
        removeText(key);
      } else if (text.element) {
        text.element.style.setProperty('opacity', String(strokeFadeOpacity(age)));
      }
    });
  }

  if (typeof setInterval === 'function') {
    const sweepTimer = setInterval(sweepExpiredStrokes, DRAW_FADE_SWEEP_MS);
    // setupDrawDisplay() has no destroy()/teardown hook (it's a page-lifetime
    // singleton in the real app, same as setupTelepointerDisplay), so this
    // timer would otherwise run forever. unref() (a no-op on a real
    // browser's numeric timer handle) keeps it from alone keeping a Node
    // process alive -- same fix as remoteWindowHeader.ts's freshnessTimer /
    // tiles.ts's cameraDecodeHealthPoll, which hung the test suite the same
    // way before they picked it up.
    (sweepTimer as unknown as { unref?: () => void })?.unref?.();
  }

  function renderDrawForWindow(windowId: number, ownerIdentity?: string) {
    strokes.forEach((stroke) => {
      if (stroke.windowId !== windowId) return;
      if (ownerIdentity && stroke.ownerIdentity !== ownerIdentity) return;
      renderStroke(stroke);
    });
    texts.forEach((text) => {
      if (text.windowId !== windowId) return;
      if (ownerIdentity && text.ownerIdentity !== ownerIdentity) return;
      renderText(text);
    });
  }

  function removeStroke(key: string) {
    const stroke = strokes.get(key);
    stroke?.path?.remove();
    strokes.delete(key);
  }

  // #670: the `clear` message type is receive-only dead code -- no sender
  // (native or web) ever emits it (a 10s auto-fade below replaces the need
  // for an explicit clear). `message.strokeId` is `null` for a clear
  // message and non-null for every other type (DrawMessage's discriminated
  // union), so this drops it the same way an unrecognized/no-op message
  // always would, without a dedicated branch to keep in sync.
  function applyDrawMessage(message: DrawMessage, drawerIdentity: string) {
    if (!message.strokeId) return;

    const key = drawKey(message, drawerIdentity, message.strokeId);
    if (message.type === 'text') {
      texts.set(key, {
        drawerIdentity,
        ownerIdentity: message.ownerIdentity,
        windowId: message.windowId,
        annotationId: message.strokeId,
        anchor: message.points[0],
        text: message.text,
        element: null,
        lastPointAtMs: Date.now(),
      });
      renderText(texts.get(key)!);
      return;
    }
    let stroke = strokes.get(key);
    if (!stroke) {
      stroke = {
        drawerIdentity,
        ownerIdentity: message.ownerIdentity,
        windowId: message.windowId,
        strokeId: message.strokeId,
        points: [],
        path: null,
        lastPointAtMs: Date.now(),
      };
      strokes.set(key, stroke);
    }

    if (message.type === 'begin') stroke.points = [];
    stroke.points.push(...message.points);
    // Restart the age-out clock on every continuation (#670 requirement: a
    // stroke still being drawn must not begin fading mid-draw).
    stroke.lastPointAtMs = Date.now();
    renderStroke(stroke);
  }

  function handleRemoteDrawPayload(payload: Uint8Array, senderIdentity?: string, topic?: string) {
    if (topic !== DRAW_TOPIC) return;
    const drawerIdentity = senderIdentity?.trim();
    if (!drawerIdentity) {
      ctx.ui.logEvent('ignored draw payload without authenticated sender identity', 'warn');
      return;
    }
    const message = parseDrawPayload(payload);
    if (!message) {
      ctx.ui.logEvent('ignored malformed draw payload', 'warn');
      return;
    }
    applyDrawMessage(message, drawerIdentity);
  }

  function repositionRemoteDraw() {
    strokes.forEach((stroke) => {
      if (stroke.path) renderStroke(stroke);
    });
    texts.forEach(renderText);
  }

  function removeDrawForWindow(windowId: number, ownerIdentity?: string) {
    Array.from(strokes.entries()).forEach(([key, stroke]) => {
      if (stroke.windowId !== windowId) return;
      if (ownerIdentity && stroke.ownerIdentity !== ownerIdentity) return;
      removeStroke(key);
    });
    Array.from(texts.entries()).forEach(([key, text]) => {
      if (text.windowId !== windowId) return;
      if (ownerIdentity && text.ownerIdentity !== ownerIdentity) return;
      removeText(key);
    });
  }

  function removeDrawForParticipant(identity: string) {
    Array.from(strokes.entries()).forEach(([key, stroke]) => {
      if (stroke.ownerIdentity === identity || stroke.drawerIdentity === identity) removeStroke(key);
    });
    Array.from(texts.entries()).forEach(([key, text]) => {
      if (text.ownerIdentity === identity || text.drawerIdentity === identity) removeText(key);
    });
  }

  function clearRemoteDraw() {
    Array.from(strokes.keys()).forEach(removeStroke);
    Array.from(texts.keys()).forEach(removeText);
  }

  return {
    handleRemoteDrawPayload,
    renderDrawForWindow,
    repositionRemoteDraw,
    removeDrawForWindow,
    removeDrawForParticipant,
    clearRemoteDraw,
  };
}
