import { containedMediaRect, type PointLike, type RectLike, type SizeLike } from './telepointer.ts';

// ---------------------------------------------------------------------------
// Zoom and pan maths for a shared window in View mode (#248). Pure: every
// function takes the share video's layout box (untransformed, box-local px)
// and the video's intrinsic size, so tests and shareZoomUi.ts agree exactly.
//
// A share always FITS by default (`object-fit: contain`, never cropped). A
// zoom is kept relative to that fit -- `scale` 1 is fit, and `centerX/Y` is
// the picture point (0..1) shown at the middle of the box -- so it survives
// the tile being resized (grid <-> spotlight, window resize) without drifting.
// The rendered form is a `translate(x, y) scale(s)` on the video element from
// its top-left; because a transformed element's getBoundingClientRect() is
// the transformed box, every existing overlay mapping (telepointers, draw,
// remote control, all via telepointer.ts `mediaContentRect`) follows the zoom
// with no change of its own.
// ---------------------------------------------------------------------------

export interface ShareZoom {
  /** Magnification relative to fit: 1 = the whole window, letterboxed. */
  scale: number;
  /** Picture point (0..1 of the contained picture) at the box's centre. */
  centerX: number;
  centerY: number;
}

/** `translate(x px, y px) scale(scale)` from the video box's top-left. */
export interface ZoomTransform {
  scale: number;
  x: number;
  y: number;
}

export const SHARE_ZOOM_FIT: ShareZoom = Object.freeze({ scale: 1, centerX: 0.5, centerY: 0.5 });
/** Deepest zoom, unless fill itself needs more (a very tall window). */
export const SHARE_ZOOM_MAX = 5;
/** Double-tap on a window whose fit already fills its tile zooms this far. */
const SHARE_ZOOM_DOUBLE_TAP = 2;
/** One double-tap never zooms further than this: a tall window's "fill" can
 * be ~4x, too big a jump to keep your place. A further double-tap steps on to
 * fill. */
export const SHARE_ZOOM_DOUBLE_TAP_MAX = 2.5;
const SCALE_EPSILON = 1e-3;

function contentIn(box: SizeLike, media: SizeLike): RectLike {
  return containedMediaRect({ left: 0, top: 0, width: box.width, height: box.height }, media);
}

function usable(box: SizeLike, media: SizeLike): boolean {
  return box.width > 0 && box.height > 0 && media.width > 0 && media.height > 0;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function isShareZoomed(zoom: ShareZoom): boolean {
  return zoom.scale > 1 + SCALE_EPSILON;
}

/** The scale at which the fitted picture covers the whole box ("fill"). */
export function shareFillScale(box: SizeLike, media: SizeLike): number {
  if (!usable(box, media)) return 1;
  const content = contentIn(box, media);
  return Math.max(box.width / content.width, box.height / content.height);
}

export function shareZoomMaxScale(box: SizeLike, media: SizeLike): number {
  return Math.max(SHARE_ZOOM_MAX, shareFillScale(box, media));
}

/**
 * Keep a zoom legal: scale within [fit, max], and the picture covering the
 * box on every axis where it is larger than the box (no panning into black);
 * on an axis where it is still smaller, it stays centred like fit.
 */
export function clampShareZoom(box: SizeLike, media: SizeLike, zoom: ShareZoom): ShareZoom {
  if (!usable(box, media)) return SHARE_ZOOM_FIT;
  const scale = clamp(Number.isFinite(zoom.scale) ? zoom.scale : 1, 1, shareZoomMaxScale(box, media));
  if (scale <= 1 + SCALE_EPSILON) return SHARE_ZOOM_FIT;
  const content = contentIn(box, media);
  const axis = (center: number, visible: number, extent: number) => {
    const scaled = extent * scale;
    if (scaled <= visible + 1e-6) return 0.5;
    const half = visible / 2 / scaled;
    return clamp(Number.isFinite(center) ? center : 0.5, half, 1 - half);
  };
  return {
    scale,
    centerX: axis(zoom.centerX, box.width, content.width),
    centerY: axis(zoom.centerY, box.height, content.height),
  };
}

export function shareZoomTransform(box: SizeLike, media: SizeLike, zoom: ShareZoom): ZoomTransform {
  if (!usable(box, media)) return { scale: 1, x: 0, y: 0 };
  const content = contentIn(box, media);
  const pointX = content.left + zoom.centerX * content.width;
  const pointY = content.top + zoom.centerY * content.height;
  return {
    scale: zoom.scale,
    x: box.width / 2 - zoom.scale * pointX,
    y: box.height / 2 - zoom.scale * pointY,
  };
}

function zoomFromTransform(box: SizeLike, media: SizeLike, transform: ZoomTransform): ShareZoom {
  const content = contentIn(box, media);
  return {
    scale: transform.scale,
    centerX: ((box.width / 2 - transform.x) / transform.scale - content.left) / content.width,
    centerY: ((box.height / 2 - transform.y) / transform.scale - content.top) / content.height,
  };
}

/**
 * Multiply the zoom by `factor` keeping the picture point under `anchor`
 * (box-local px) where it is -- pinch, Ctrl/⌘+wheel and trackpad pinch all
 * zoom around the gesture, not the centre.
 */
export function zoomShareAt(
  box: SizeLike,
  media: SizeLike,
  zoom: ShareZoom,
  anchor: PointLike,
  factor: number
): ShareZoom {
  if (!usable(box, media) || !Number.isFinite(factor) || factor <= 0) return zoom;
  const before = shareZoomTransform(box, media, zoom);
  const local = { x: (anchor.x - before.x) / before.scale, y: (anchor.y - before.y) / before.scale };
  const scale = clamp(zoom.scale * factor, 1, shareZoomMaxScale(box, media));
  const after = { scale, x: anchor.x - scale * local.x, y: anchor.y - scale * local.y };
  return clampShareZoom(box, media, zoomFromTransform(box, media, after));
}

/** Move the picture by (dx, dy) screen px -- a drag pans with the finger. */
export function panShare(box: SizeLike, media: SizeLike, zoom: ShareZoom, dx: number, dy: number): ShareZoom {
  if (!usable(box, media)) return zoom;
  const transform = shareZoomTransform(box, media, zoom);
  return clampShareZoom(box, media, zoomFromTransform(box, media, { ...transform, x: transform.x + dx, y: transform.y + dy }));
}

/**
 * Double-tap, around the tapped point: at fit -> fill the tile (crop the
 * letterbox away), at most SHARE_ZOOM_DOUBLE_TAP_MAX per tap, so a tall
 * window steps 2.5x -> fill; once at (or past) fill -> back to fit. A window
 * whose fit already fills the tile has no letterbox to remove, so it zooms 2x.
 */
export function toggleShareFitFill(
  box: SizeLike,
  media: SizeLike,
  zoom: ShareZoom,
  anchor: PointLike = { x: box.width / 2, y: box.height / 2 }
): ShareZoom {
  const fill = shareFillScale(box, media);
  const hasLetterbox = fill > 1 + 0.02;
  if (isShareZoomed(zoom) && (!hasLetterbox || zoom.scale >= fill - SCALE_EPSILON)) return SHARE_ZOOM_FIT;
  const target = hasLetterbox ? Math.min(fill, zoom.scale * SHARE_ZOOM_DOUBLE_TAP_MAX) : SHARE_ZOOM_DOUBLE_TAP;
  return zoomShareAt(box, media, zoom, anchor, target / zoom.scale);
}

/** Box-local screen px -> picture point (0..1), or null off the picture.
 * Test oracle: the UI never needs it (overlays read the transformed rect);
 * tests use it to prove a pointer lands on the pixel it names. */
export function sharePointToPicture(
  box: SizeLike,
  media: SizeLike,
  zoom: ShareZoom,
  point: PointLike
): PointLike | null {
  if (!usable(box, media)) return null;
  const content = contentIn(box, media);
  const transform = shareZoomTransform(box, media, zoom);
  const x = ((point.x - transform.x) / transform.scale - content.left) / content.width;
  const y = ((point.y - transform.y) / transform.scale - content.top) / content.height;
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x, y };
}

/** Picture point (0..1) -> box-local screen px under this zoom. Test oracle,
 * like `sharePointToPicture`. */
export function sharePictureToPoint(box: SizeLike, media: SizeLike, zoom: ShareZoom, picture: PointLike): PointLike {
  const content = contentIn(box, media);
  const transform = shareZoomTransform(box, media, zoom);
  return {
    x: transform.x + transform.scale * (content.left + picture.x * content.width),
    y: transform.y + transform.scale * (content.top + picture.y * content.height),
  };
}

/**
 * The clip, in the video's own (untransformed) px, that keeps the zoomed
 * picture inside the video's layout box. The tile's overflow already clips
 * most of it, but not the strip under the docked header, and not at all while
 * the header's overflow menu has opened the tile (`remote-window-menu-open`).
 */
export function shareZoomClipInsets(
  box: SizeLike,
  transform: ZoomTransform
): { top: number; right: number; bottom: number; left: number } {
  const s = transform.scale;
  return {
    top: Math.max(0, -transform.y / s),
    right: Math.max(0, box.width - (box.width - transform.x) / s),
    bottom: Math.max(0, box.height - (box.height - transform.y) / s),
    left: Math.max(0, -transform.x / s),
  };
}

/**
 * The rect a browser reports from getBoundingClientRect() for the video once
 * the zoom transform is applied (origin top-left): what `mediaContentRect`
 * -- and so every telepointer, drawing and remote-control mapping -- sees.
 * Test oracle: lets node tests stand in for the browser's transformed rect.
 */
export function zoomedVideoBounds(layoutBounds: RectLike, transform: ZoomTransform): RectLike {
  return {
    left: layoutBounds.left + transform.x,
    top: layoutBounds.top + transform.y,
    width: layoutBounds.width * transform.scale,
    height: layoutBounds.height * transform.scale,
  };
}

// ---------------------------------------------------------------------------
// Small DOM reads shared with the overlay and viewer-demand modules. The zoom
// controller (shareZoomUi.ts) records its state on the tile:
//   data-share-zoom-painted-scale  the scale painted right now (every frame of
//                                  a pinch);
//   data-share-zoom-demand-scale   the scale committed when a gesture ENDS
//                                  (pinch/pan lift, wheel settle, double-tap,
//                                  key, menu, chip) -- what viewer demand uses.
// Both are absent at fit.
// ---------------------------------------------------------------------------

interface ZoomTileLike {
  classList?: { contains(name: string): boolean };
  dataset: DOMStringMap;
}

function datasetScale(value: string | undefined): number {
  const scale = Number(value);
  return Number.isFinite(scale) && scale >= 1 ? scale : 1;
}

/**
 * How much bigger than its painted rect a share's viewer demand should be:
 * the committed zoom over the painted one. Mid-pinch the painted scale moves
 * every frame while the committed one holds, so the demand stays put until
 * the gesture ends. A rail thumbnail always shows the whole window (CSS drops
 * the zoom there), so it asks for its plain box.
 */
export function shareZoomDemandFactor(tile: ZoomTileLike): number {
  if (!tile.classList?.contains('is-share-zoomed') || tile.classList.contains('is-spotlight-thumbnail')) return 1;
  return datasetScale(tile.dataset.shareZoomDemandScale) / datasetScale(tile.dataset.shareZoomPaintedScale);
}

interface MediaBoxTileLike extends ZoomTileLike {
  querySelector: HTMLElement['querySelector'];
}

/**
 * Whether a tile-relative point (an overlay's anchor) lies on the part of a
 * zoomed share that is actually visible -- the video's own layout box. At fit
 * everything an overlay can point at is visible, so this is always true.
 */
export function pointVisibleInZoomedShare(tile: MediaBoxTileLike, point: PointLike): boolean {
  if (!tile.classList?.contains('is-share-zoomed') || tile.classList.contains('is-spotlight-thumbnail')) return true;
  const video = tile.querySelector<HTMLVideoElement>('video');
  if (!video || !(video.offsetWidth > 0) || !(video.offsetHeight > 0)) return true;
  return (
    point.x >= video.offsetLeft &&
    point.x <= video.offsetLeft + video.offsetWidth &&
    point.y >= video.offsetTop &&
    point.y <= video.offsetTop + video.offsetHeight
  );
}
