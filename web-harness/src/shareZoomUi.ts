import type { HarnessContext } from './context.ts';
import type { PointLike, SizeLike } from './telepointer.ts';
import {
  SHARE_ZOOM_FIT,
  clampShareZoom,
  isShareZoomed,
  panShare,
  shareZoomClipInsets,
  shareZoomTransform,
  toggleShareFitFill,
  zoomShareAt,
  type ShareZoom,
} from './shareZoom.ts';

// ---------------------------------------------------------------------------
// Zoom and pan a shared window in View mode (#248). Listeners are delegated
// on the tile surface (like drawSender.ts), except the non-passive `wheel`
// and `touchmove`, which `bindTile` puts on each share tile so the rest of
// the surface still scrolls on the compositor:
//   - pinch (two touch pointers), Ctrl/⌘ + wheel, and a trackpad pinch
//     (Chromium/Firefox send it as Ctrl+wheel; Safari as gesture events) zoom
//     around the gesture point;
//   - a drag, or a plain wheel/two-finger scroll, pans while zoomed;
//   - a double-tap / double-click steps fit -> fill (at most 2.5x a tap) ->
//     fit;
//   - on a focused share tile (a press focuses it) `+`/`=` zoom in, `-`
//     zoom out, `0` fits, and the header's overflow menu has the same three
//     commands;
//   - a small chip shows the zoom and resets to fit.
// Control and Draw modes keep every gesture for the remote window. A zoom
// made in View mode stays in place when the mode changes -- all input mapping
// reads the transformed video rect, so a click or stroke still lands on the
// pixel under it -- and the chip stays, as the way back to fit.
// ---------------------------------------------------------------------------

const TAP_SLOP_PX = 8;
const DOUBLE_TAP_MS = 320;
const DOUBLE_TAP_DISTANCE_PX = 28;
/** Ctrl+wheel: zoom factor per wheel px. A trackpad pinch sends many small
 * deltas; a mouse notch (~100px) is capped so one notch is ~1.5x, not 3x. */
const WHEEL_ZOOM_PER_PX = 0.01;
const WHEEL_ZOOM_DELTA_CAP_PX = 40;
const WHEEL_LINE_PX = 16;
const WHEEL_PAGE_PX = WHEEL_LINE_PX * 20;
/** A wheel/trackpad zoom has no "end" event; it has ended once it is quiet. */
const WHEEL_SETTLE_MS = 250;
/** One `+` / `-` step. */
const KEY_ZOOM_STEP = 1.25;
/** Discrete changes (double-tap, key, menu, chip) ease; pinch/pan never do. */
const ZOOM_EASE_MS = 180;
/** Elements inside a share tile that own their own pointer input. */
const INTERACTIVE_SELECTOR =
  'button, a, input, textarea, select, label, summary, [contenteditable], .remote-window-header, .ai-chat-panel, .share-zoom-chip';
/** Fields that keep keyboard focus when a share tile is pressed. */
const TEXT_ENTRY_SELECTOR = 'input, textarea, select';

export type ShareZoomCommand = 'in' | 'out' | 'fit';

type WebKitGestureEvent = UIEvent & { scale: number; clientX: number; clientY: number };

interface ShareGeometry {
  video: HTMLVideoElement;
  box: SizeLike;
  media: SizeLike;
}

interface PointerGesture {
  tile: HTMLElement;
  pointers: Map<number, PointLike>;
  kind: 'press' | 'pan' | 'pinch';
  start: PointLike;
  last: PointLike;
  moved: boolean;
  pinch: { distance: number; mid: PointLike; origin: PointLike; scale: number; zoom: ShareZoom } | null;
}

interface ChipState {
  chip: HTMLButtonElement;
  value: HTMLSpanElement;
  label: string | null;
}

export function setupShareZoom(ctx: HarnessContext) {
  const { tilesEl } = ctx.dom;
  const zooms = new WeakMap<HTMLElement, ShareZoom>();
  const chips = new WeakMap<HTMLElement, ChipState>();
  const resizeObservers = new WeakMap<HTMLElement, ResizeObserver>();
  const watchedVideos = new WeakSet<HTMLVideoElement>();
  const wheelCommitTimers = new WeakMap<HTMLElement, ReturnType<typeof setTimeout>>();
  const easeTimers = new WeakMap<HTMLElement, ReturnType<typeof setTimeout>>();
  const boundTiles = new WeakSet<HTMLElement>();
  let gesture: PointerGesture | null = null;
  let webkitGesture: { tile: HTMLElement; anchor: PointLike; zoom: ShareZoom } | null = null;
  let lastTap: { tile: HTMLElement; at: number; point: PointLike } | null = null;
  let swallowClickOn: HTMLElement | null = null;
  let overlayFrame: number | null = null;
  let overlayFollowUntil = 0;

  function zoomFor(tile: HTMLElement): ShareZoom {
    return zooms.get(tile) ?? SHARE_ZOOM_FIT;
  }

  function geometry(tile: HTMLElement): ShareGeometry | null {
    const video = tile.querySelector<HTMLVideoElement>('video');
    if (!video) return null;
    const box = { width: video.clientWidth, height: video.clientHeight };
    const media = { width: video.videoWidth, height: video.videoHeight };
    if (box.width <= 0 || box.height <= 0 || media.width <= 0 || media.height <= 0) return null;
    return { video, box, media };
  }

  /**
   * Viewport px -> the video's own box, in its layout px. The painted rect is
   * the layout box moved and scaled by the zoom and, mid-FLIP, by the tile's
   * own (uniform) reflow transform -- so undo both. A tap during a reflow
   * (a double-tap whose first tap spotlighted the tile) therefore lands on
   * the picture point that was under the finger, in the tile's final box.
   */
  function boxPoint(tile: HTMLElement, geo: ShareGeometry, clientX: number, clientY: number): PointLike {
    const rect = geo.video.getBoundingClientRect();
    const transform = shareZoomTransform(geo.box, geo.media, zoomFor(tile));
    const ancestorScale = rect.width > 0 ? rect.width / (transform.scale * geo.box.width) : 1;
    const originX = rect.left - ancestorScale * transform.x;
    const originY = rect.top - ancestorScale * transform.y;
    return { x: (clientX - originX) / ancestorScale, y: (clientY - originY) / ancestorScale };
  }

  function inViewMode(tile: HTMLElement): boolean {
    return (
      !tilesEl.classList.contains('draw-mode-active') &&
      !tile.classList.contains('remote-control-active') &&
      !ctx.cb.activeRemoteControlForTile?.(tile as HTMLDivElement) &&
      !tile.classList.contains('is-spotlight-thumbnail')
    );
  }

  function shareTileFromEvent(event: Event): HTMLElement | null {
    const target = event.target as Element | null;
    if (!target || typeof target.closest !== 'function') return null;
    if (target.closest(INTERACTIVE_SELECTOR)) return null;
    const tile = target.closest<HTMLElement>('.tile.share-tile');
    return tile && tilesEl.contains(tile) ? tile : null;
  }

  function viewModeTileFromEvent(event: Event): HTMLElement | null {
    const tile = shareTileFromEvent(event);
    return tile && inViewMode(tile) ? tile : null;
  }

  /**
   * `+` / `-` / `0` act on the focused share tile, but a press on a zoomed
   * share is preventDefault-ed (no text selection, no image drag), which also
   * cancels the browser's focus-on-press -- so a mouse user could only reach
   * the keys with Tab. Focus the tile, as Control mode does; never away from
   * a field someone is typing in (the chat, a rename).
   */
  function focusShareTile(tile: HTMLElement) {
    const active = document.activeElement as HTMLElement | null | undefined;
    if (active && active !== tile && (active.isContentEditable || active.matches?.(TEXT_ENTRY_SELECTOR))) return;
    tile.focus?.({ preventScroll: true });
  }

  function prefersReducedMotion(): boolean {
    return globalThis.window?.matchMedia?.('(prefers-reduced-motion: reduce)').matches === true;
  }

  /** Telepointers and drawings read the transformed video rect, so re-place
   * them after every change -- every frame while a discrete change eases. */
  function scheduleOverlayReposition(followMs = 0) {
    if (followMs > 0) overlayFollowUntil = Math.max(overlayFollowUntil, Date.now() + followMs);
    if (overlayFrame !== null) return;
    const run = () => {
      overlayFrame = null;
      ctx.cb.repositionRemoteTelepointers();
      ctx.cb.repositionRemoteDraw();
      if (Date.now() < overlayFollowUntil && typeof requestAnimationFrame === 'function') {
        overlayFrame = requestAnimationFrame(run);
      }
    };
    if (typeof requestAnimationFrame !== 'function') {
      run();
      return;
    }
    overlayFrame = requestAnimationFrame(run);
  }

  function stopEasing(tile: HTMLElement) {
    const timer = easeTimers.get(tile);
    if (timer !== undefined) clearTimeout(timer);
    easeTimers.delete(tile);
    tile.classList.remove('is-share-zoom-easing');
  }

  function ensureChip(tile: HTMLElement): ChipState {
    const existing = chips.get(tile);
    if (existing && existing.chip.parentElement === tile) return existing;
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'share-zoom-chip';
    const value = document.createElement('span');
    value.className = 'share-zoom-chip__value';
    const action = document.createElement('span');
    action.className = 'share-zoom-chip__action';
    action.textContent = 'Fit';
    chip.append(value, action);
    // Its own control in every mode: never the start of a pan, a pin, a
    // stroke, or input forwarded to a controlled remote window (whose
    // listeners sit on the tile, above the chip).
    for (const type of ['pointerdown', 'pointermove', 'pointerup', 'wheel', 'keydown', 'keyup']) {
      chip.addEventListener(type, (event) => event.stopPropagation());
    }
    chip.addEventListener('click', (event) => {
      event.preventDefault();
      event.stopPropagation();
      applyCommand(tile, 'fit');
    });
    tile.appendChild(chip);
    const state: ChipState = { chip, value, label: null };
    chips.set(tile, state);
    return state;
  }

  function syncChip(tile: HTMLElement, zoom: ShareZoom) {
    const zoomed = isShareZoomed(zoom);
    const state = zoomed ? ensureChip(tile) : chips.get(tile);
    if (!state) return;
    if (state.chip.hidden === zoomed) state.chip.hidden = !zoomed;
    if (!zoomed) return;
    const label = `${zoom.scale.toFixed(1)}×`;
    if (state.label === label) return;
    state.label = label;
    state.value.textContent = label;
    state.chip.title = `Zoomed to ${label}. Reset to fit`;
    state.chip.setAttribute('aria-label', state.chip.title);
  }

  /** Re-derive the transform when the tile or the window changes size: the
   * zoom is stored relative to fit, so it stays on the same picture point. */
  function watchSize(tile: HTMLElement, geo: ShareGeometry) {
    if (!watchedVideos.has(geo.video)) {
      watchedVideos.add(geo.video);
      geo.video.addEventListener('resize', () => {
        const owner = geo.video.closest<HTMLElement>('.tile.share-tile');
        if (owner && zooms.has(owner)) setZoom(owner, zoomFor(owner));
      });
    }
    if (resizeObservers.has(tile) || typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(() => {
      // A rail thumbnail renders at fit (CSS); keep the zoom for the way back.
      if (zooms.has(tile) && !tile.classList.contains('is-spotlight-thumbnail')) setZoom(tile, zoomFor(tile));
    });
    observer.observe(tile);
    resizeObservers.set(tile, observer);
  }

  function setZoom(tile: HTMLElement, requested: ShareZoom) {
    const geo = geometry(tile);
    // No frame yet (or a republish gap): a reset still resets, but a zoom
    // cannot be placed without the picture's size -- keep what is painted.
    if (!geo && isShareZoomed(requested)) return;
    const zoom = geo ? clampShareZoom(geo.box, geo.media, requested) : SHARE_ZOOM_FIT;
    if (geo && isShareZoomed(zoom)) {
      zooms.set(tile, zoom);
      const transform = shareZoomTransform(geo.box, geo.media, zoom);
      const clip = shareZoomClipInsets(geo.box, transform);
      const video = geo.video;
      // Overlay layers cover the whole tile; clip them to the video's box so
      // a telepointer or stroke on an off-screen part of the window never
      // shows over the header strip or outside a menu-open tile.
      const mediaTop = video.offsetTop || 0;
      const mediaLeft = video.offsetLeft || 0;
      const mediaRight = Math.max(0, (tile.clientWidth || 0) - mediaLeft - (video.offsetWidth || 0));
      const mediaBottom = Math.max(0, (tile.clientHeight || 0) - mediaTop - (video.offsetHeight || 0));
      tile.style.setProperty(
        '--share-zoom-transform',
        `translate(${transform.x.toFixed(2)}px, ${transform.y.toFixed(2)}px) scale(${transform.scale.toFixed(4)})`
      );
      tile.style.setProperty(
        '--share-zoom-clip',
        `inset(${clip.top.toFixed(2)}px ${clip.right.toFixed(2)}px ${clip.bottom.toFixed(2)}px ${clip.left.toFixed(2)}px)`
      );
      tile.style.setProperty('--share-zoom-media-clip', `inset(${mediaTop}px ${mediaRight}px ${mediaBottom}px ${mediaLeft}px)`);
      tile.dataset.shareZoomPaintedScale = zoom.scale.toFixed(4);
      tile.classList.add('is-share-zoomed');
      watchSize(tile, geo);
    } else {
      zooms.delete(tile);
      tile.style.removeProperty('--share-zoom-transform');
      tile.style.removeProperty('--share-zoom-clip');
      tile.style.removeProperty('--share-zoom-media-clip');
      delete tile.dataset.shareZoomPaintedScale;
      tile.classList.remove('is-share-zoomed');
      resizeObservers.get(tile)?.disconnect();
      resizeObservers.delete(tile);
    }
    syncChip(tile, zoom);
    scheduleOverlayReposition();
  }

  /**
   * #248 viewer demand: a zoomed viewer shows the window bigger and asks the
   * sender for the pixels it now displays (viewerDemand.ts scales its rect by
   * `shareZoomDemandFactor`). Committed only when a gesture ENDS -- never per
   * pinch frame -- and published straight away rather than on the next 2s
   * heartbeat. Unchanged scale (a pan) publishes nothing.
   */
  function commitDemand(tile: HTMLElement) {
    const zoom = zoomFor(tile);
    const next = isShareZoomed(zoom) ? zoom.scale.toFixed(4) : undefined;
    if (tile.dataset.shareZoomDemandScale === next) return;
    if (next === undefined) delete tile.dataset.shareZoomDemandScale;
    else tile.dataset.shareZoomDemandScale = next;
    ctx.cb.publishViewerDemand?.(tile as HTMLDivElement, 'heartbeat');
  }

  /** A discrete change (double-tap, key, menu, chip): eases over ~180 ms
   * unless reduced motion is on, then commits its demand. */
  function setZoomEased(tile: HTMLElement, requested: ShareZoom) {
    stopEasing(tile);
    if (!prefersReducedMotion()) {
      tile.classList.add('is-share-zoom-easing');
      easeTimers.set(
        tile,
        setTimeout(() => stopEasing(tile), ZOOM_EASE_MS + 40)
      );
      scheduleOverlayReposition(ZOOM_EASE_MS + 40);
    }
    setZoom(tile, requested);
    commitDemand(tile);
  }

  function applyCommand(tile: HTMLElement, command: ShareZoomCommand) {
    if (command === 'fit') {
      setZoomEased(tile, SHARE_ZOOM_FIT);
      return;
    }
    const geo = geometry(tile);
    if (!geo) return;
    const centre = { x: geo.box.width / 2, y: geo.box.height / 2 };
    const factor = command === 'in' ? KEY_ZOOM_STEP : 1 / KEY_ZOOM_STEP;
    setZoomEased(tile, zoomShareAt(geo.box, geo.media, zoomFor(tile), centre, factor));
  }

  /** (Re)baseline a pinch on the first two tracked pointers -- whenever the
   * set of fingers changes, so a third finger landing or one lifting never
   * turns into a jump. */
  function startPinch(active: PointerGesture) {
    const geo = geometry(active.tile);
    const [a, b] = Array.from(active.pointers.values());
    if (!geo || !a || !b) return;
    const mid = boxPoint(active.tile, geo, (a.x + b.x) / 2, (a.y + b.y) / 2);
    const rect = geo.video.getBoundingClientRect();
    const transform = shareZoomTransform(geo.box, geo.media, zoomFor(active.tile));
    active.kind = 'pinch';
    active.moved = true;
    active.pinch = {
      distance: Math.max(1, Math.hypot(b.x - a.x, b.y - a.y)),
      mid,
      origin: { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 },
      scale: rect.width > 0 ? rect.width / (transform.scale * geo.box.width) : 1,
      zoom: zoomFor(active.tile),
    };
  }

  // Capture phase, so it runs even when the press lands on a control that
  // stops propagation (the chip): a touch pan produces no click to consume the
  // flag, and the NEXT press must never inherit it.
  function clearSwallowedClick() {
    swallowClickOn = null;
  }

  function handlePointerDown(event: PointerEvent) {
    const tile = viewModeTileFromEvent(event);
    if (!tile) return;
    if (event.pointerType === 'mouse' && event.button !== 0) return;
    focusShareTile(tile);
    if (!geometry(tile)) return;
    stopEasing(tile);
    const point = { x: event.clientX, y: event.clientY };
    // A release we never saw (an uncaptured mouse let go outside the tile
    // surface, or the tile was removed mid-gesture) must not leave a ghost
    // pointer behind to turn the next press into a pinch.
    if (
      gesture &&
      (gesture.pointers.has(event.pointerId) || event.pointerType === 'mouse' || !gesture.tile.isConnected)
    ) {
      gesture.tile.classList.remove('is-share-panning');
      gesture = null;
    }
    if (gesture && gesture.tile !== tile) return;
    if (!gesture) {
      gesture = { tile, pointers: new Map(), kind: 'press', start: point, last: point, moved: false, pinch: null };
    }
    gesture.pointers.set(event.pointerId, point);
    // The pinch follows the first two fingers; a third changes nothing until
    // one of the pair lifts (re-baselined in handlePointerEnd).
    if (gesture.pointers.size === 2) startPinch(gesture);
    if (gesture.kind === 'pinch' || isShareZoomed(zoomFor(tile))) {
      // Keep receiving the gesture if it leaves the tile, and keep the
      // browser from starting a text selection or image drag.
      event.preventDefault();
      try {
        tile.setPointerCapture(event.pointerId);
      } catch {
        // Pointer capture can fail for synthetic events; the gesture still tracks.
      }
    }
  }

  function handlePointerMove(event: PointerEvent) {
    const active = gesture;
    if (!active || !active.pointers.has(event.pointerId)) return;
    const point = { x: event.clientX, y: event.clientY };
    active.pointers.set(event.pointerId, point);
    const geo = geometry(active.tile);
    if (!geo) return;

    if (active.kind === 'pinch' && active.pinch && active.pointers.size >= 2) {
      const [a, b] = Array.from(active.pointers.values());
      const pinch = active.pinch;
      const distance = Math.max(1, Math.hypot(b.x - a.x, b.y - a.y));
      const zoomed = zoomShareAt(geo.box, geo.media, pinch.zoom, pinch.mid, distance / pinch.distance);
      const dx = ((a.x + b.x) / 2 - pinch.origin.x) / pinch.scale;
      const dy = ((a.y + b.y) / 2 - pinch.origin.y) / pinch.scale;
      setZoom(active.tile, panShare(geo.box, geo.media, zoomed, dx, dy));
      event.preventDefault();
      return;
    }

    if (!active.moved && Math.hypot(point.x - active.start.x, point.y - active.start.y) < TAP_SLOP_PX) return;
    active.moved = true;
    const zoom = zoomFor(active.tile);
    if (!isShareZoomed(zoom)) return;
    if (active.kind !== 'pan') {
      active.kind = 'pan';
      active.tile.classList.add('is-share-panning');
    }
    setZoom(active.tile, panShare(geo.box, geo.media, zoom, point.x - active.last.x, point.y - active.last.y));
    active.last = point;
    event.preventDefault();
  }

  function handlePointerEnd(event: PointerEvent) {
    const active = gesture;
    if (!active || !active.pointers.has(event.pointerId)) return;
    active.pointers.delete(event.pointerId);
    try {
      active.tile.releasePointerCapture?.(event.pointerId);
    } catch {
      // Safe to ignore: the pointer may not be captured.
    }
    if (active.pointers.size >= 2) {
      // Still pinching with the fingers that remain: re-baseline on them.
      startPinch(active);
      return;
    }
    if (active.pointers.size === 1 && active.kind === 'pinch') {
      // One finger lifted: keep panning with the other, from where it is.
      const remaining = Array.from(active.pointers.values())[0]!;
      active.kind = 'pan';
      active.pinch = null;
      active.last = remaining;
      return;
    }
    if (active.pointers.size > 0) return;

    gesture = null;
    active.tile.classList.remove('is-share-panning');
    if (active.kind !== 'press') {
      // A pan or pinch is not a click: do not let it pin the tile.
      swallowClickOn = active.tile;
      lastTap = null;
      commitDemand(active.tile);
      return;
    }
    if (active.moved || event.type !== 'pointerup') {
      lastTap = null;
      return;
    }
    const at = event.timeStamp;
    const point = { x: event.clientX, y: event.clientY };
    if (
      lastTap &&
      lastTap.tile === active.tile &&
      at - lastTap.at <= DOUBLE_TAP_MS &&
      Math.hypot(point.x - lastTap.point.x, point.y - lastTap.point.y) <= DOUBLE_TAP_DISTANCE_PX
    ) {
      lastTap = null;
      const geo = geometry(active.tile);
      if (!geo) return;
      // The first tap's click already pinned the tile (grid -> spotlight
      // hero, as before); the second is the zoom, not another pin.
      swallowClickOn = active.tile;
      const anchor = boxPoint(active.tile, geo, point.x, point.y);
      setZoomEased(active.tile, toggleShareFitFill(geo.box, geo.media, zoomFor(active.tile), anchor));
      return;
    }
    lastTap = { tile: active.tile, at, point };
  }

  function handleClickCapture(event: MouseEvent) {
    const tile = swallowClickOn;
    swallowClickOn = null;
    const target = event.target as Node | null;
    if (!tile || !target || !tile.contains(target)) return;
    event.preventDefault();
    event.stopPropagation();
  }

  function wheelPx(delta: number, mode: number): number {
    if (mode === 1) return delta * WHEEL_LINE_PX;
    if (mode === 2) return delta * WHEEL_PAGE_PX;
    return delta;
  }

  function scheduleWheelCommit(tile: HTMLElement) {
    const prior = wheelCommitTimers.get(tile);
    if (prior !== undefined) clearTimeout(prior);
    wheelCommitTimers.set(
      tile,
      setTimeout(() => {
        wheelCommitTimers.delete(tile);
        commitDemand(tile);
      }, WHEEL_SETTLE_MS)
    );
  }

  function handleWheel(event: WheelEvent) {
    const tile = viewModeTileFromEvent(event);
    if (!tile) return;
    const geo = geometry(tile);
    if (!geo) return;
    stopEasing(tile);
    const zoom = zoomFor(tile);
    if (event.ctrlKey || event.metaKey) {
      const delta = Math.max(-WHEEL_ZOOM_DELTA_CAP_PX, Math.min(WHEEL_ZOOM_DELTA_CAP_PX, wheelPx(event.deltaY, event.deltaMode)));
      const anchor = boxPoint(tile, geo, event.clientX, event.clientY);
      setZoom(tile, zoomShareAt(geo.box, geo.media, zoom, anchor, Math.exp(-delta * WHEEL_ZOOM_PER_PX)));
      scheduleWheelCommit(tile);
      event.preventDefault();
      return;
    }
    if (!isShareZoomed(zoom)) return;
    setZoom(
      tile,
      panShare(geo.box, geo.media, zoom, -wheelPx(event.deltaX, event.deltaMode), -wheelPx(event.deltaY, event.deltaMode))
    );
    event.preventDefault();
  }

  function handleKeyDown(event: KeyboardEvent) {
    const tile = event.target as HTMLElement | null;
    // Only the focused share tile itself: keys typed into its header, chat
    // panel or chip belong to them.
    if (!tile || !tile.classList?.contains('share-tile') || !tilesEl.contains(tile) || !inViewMode(tile)) return;
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    const command: ShareZoomCommand | null =
      event.key === '+' || event.key === '=' ? 'in' : event.key === '-' ? 'out' : event.key === '0' ? 'fit' : null;
    if (!command) return;
    event.preventDefault();
    applyCommand(tile, command);
  }

  // iOS Safari pinch-zooms the page from touches that CSS `touch-action`
  // alone does not always stop; refuse multi-finger moves over a View-mode
  // share outright. Non-passive, or preventDefault is ignored.
  function handleTouchMove(event: TouchEvent) {
    if ((event.touches?.length ?? 0) < 2) return;
    if (viewModeTileFromEvent(event)) event.preventDefault();
  }

  // Safari's trackpad pinch (and iOS pinch alongside pointer events). Always
  // prevent the page zoom over a View-mode share; only act on it when no
  // pointer pinch is already driving the same zoom.
  function handleGestureStart(event: Event) {
    const tile = viewModeTileFromEvent(event);
    if (!tile) return;
    event.preventDefault();
    const geo = geometry(tile);
    if (!geo || gesture) return;
    stopEasing(tile);
    const { clientX, clientY } = event as WebKitGestureEvent;
    webkitGesture = { tile, anchor: boxPoint(tile, geo, clientX, clientY), zoom: zoomFor(tile) };
  }

  function handleGestureChange(event: Event) {
    if (!webkitGesture && !viewModeTileFromEvent(event)) return;
    event.preventDefault();
    if (!webkitGesture || gesture) return;
    const geo = geometry(webkitGesture.tile);
    if (!geo) return;
    const { scale } = event as WebKitGestureEvent;
    setZoom(webkitGesture.tile, zoomShareAt(geo.box, geo.media, webkitGesture.zoom, webkitGesture.anchor, scale));
  }

  function handleGestureEnd(event: Event) {
    if (!webkitGesture && !viewModeTileFromEvent(event)) return;
    event.preventDefault();
    if (!webkitGesture) return;
    const tile = webkitGesture.tile;
    webkitGesture = null;
    commitDemand(tile);
  }

  tilesEl.addEventListener('pointerdown', clearSwallowedClick, { capture: true });
  tilesEl.addEventListener('pointerdown', handlePointerDown);
  tilesEl.addEventListener('pointermove', handlePointerMove);
  tilesEl.addEventListener('pointerup', handlePointerEnd);
  tilesEl.addEventListener('pointercancel', handlePointerEnd);
  tilesEl.addEventListener('click', handleClickCapture, { capture: true });
  tilesEl.addEventListener('keydown', handleKeyDown);
  tilesEl.addEventListener('gesturestart', handleGestureStart);
  tilesEl.addEventListener('gesturechange', handleGestureChange);
  tilesEl.addEventListener('gestureend', handleGestureEnd);

  /**
   * A non-passive `wheel` or `touchmove` listener makes the browser wait on
   * script before it scrolls whatever is under it, so these two sit on each
   * share tile (tiles.ts binds every one) -- not on the whole surface, whose
   * camera tiles and rail then scroll on the compositor. Idempotent.
   */
  function bindTile(tile: HTMLElement) {
    if (boundTiles.has(tile)) return;
    boundTiles.add(tile);
    tile.addEventListener('wheel', handleWheel, { passive: false });
    tile.addEventListener('touchmove', handleTouchMove, { passive: false });
  }

  return {
    /** Zoom in / out / back to fit (the header's overflow menu). */
    command: applyCommand,
    /** Give a share tile its non-passive wheel/touch listeners. */
    bindTile,
    /** The current zoom of a share tile -- for tests and the harness. */
    shareZoomFor: zoomFor,
  };
}
