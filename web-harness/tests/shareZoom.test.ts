import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  SHARE_ZOOM_DOUBLE_TAP_MAX,
  SHARE_ZOOM_FIT,
  SHARE_ZOOM_MAX,
  clampShareZoom,
  pointVisibleInZoomedShare,
  shareZoomDemandFactor,
  isShareZoomed,
  panShare,
  shareFillScale,
  sharePictureToPoint,
  sharePointToPicture,
  shareZoomClipInsets,
  shareZoomTransform,
  toggleShareFitFill,
  zoomShareAt,
  zoomedVideoBounds,
  type ShareZoom,
} from '../src/shareZoom.ts';
import {
  mediaContentRect,
  mediaContentRectRelativeToTile,
  normalizedPointInContainedMedia,
  telepointerPosition,
  type PointLike,
  type RectLike,
} from '../src/telepointer.ts';

// #248 PR 2: a shared window in View mode zooms and pans. The zoom is one
// transform on the video element, so every overlay (telepointers, drawings,
// remote-control input) keeps mapping through the video's reported rect --
// these pin that the maths and those mappings agree on the same content pixel.

// A 4:3 shared window in a 16:9 tile body below the 44px header: the window
// letterboxes left and right at fit.
const BOX = { width: 800, height: 450 };
const WINDOW = { width: 1600, height: 1200 };
const HEADER = 44;

function close(actual: number, expected: number, message?: string) {
  assert.ok(Math.abs(actual - expected) < 1e-6, `${message ?? ''} ${actual} vs ${expected}`);
}

function closePoint(actual: PointLike | null, expected: PointLike, message?: string) {
  assert.ok(actual, `${message ?? ''} expected a point`);
  close(actual.x, expected.x, `${message ?? ''} x`);
  close(actual.y, expected.y, `${message ?? ''} y`);
}

test('fit is the identity and fill removes the letterbox', () => {
  assert.deepEqual(shareZoomTransform(BOX, WINDOW, SHARE_ZOOM_FIT), { scale: 1, x: 0, y: 0 });
  // The 4:3 picture is 600 wide in an 800 box: fill is 800/600.
  close(shareFillScale(BOX, WINDOW), 4 / 3);
  const fill = toggleShareFitFill(BOX, WINDOW, SHARE_ZOOM_FIT);
  close(fill.scale, 4 / 3);
  const transform = shareZoomTransform(BOX, WINDOW, fill);
  // The picture now spans the full width (0..800) and is cropped top/bottom.
  close(transform.x + transform.scale * 100, 0, 'picture left edge');
  close(transform.x + transform.scale * 700, 800, 'picture right edge');
  // And a second double-tap goes back to fit.
  assert.deepEqual(toggleShareFitFill(BOX, WINDOW, fill), SHARE_ZOOM_FIT);
});

test('a window whose fit already fills its tile double-taps to 2x instead', () => {
  const wide = { width: 1920, height: 1080 };
  close(shareFillScale(BOX, wide), 1);
  close(toggleShareFitFill(BOX, wide, SHARE_ZOOM_FIT).scale, 2);
});

test('zooming keeps the picture point under the gesture where it was', () => {
  const anchor = { x: 520, y: 140 };
  const before = sharePointToPicture(BOX, WINDOW, SHARE_ZOOM_FIT, anchor)!;
  const zoomed = zoomShareAt(BOX, WINDOW, SHARE_ZOOM_FIT, anchor, 2.5);
  close(zoomed.scale, 2.5);
  closePoint(sharePointToPicture(BOX, WINDOW, zoomed, anchor), before, 'anchor');
  // Zooming back out around the same point returns to fit exactly.
  assert.deepEqual(zoomShareAt(BOX, WINDOW, zoomed, anchor, 1 / 2.5), SHARE_ZOOM_FIT);
});

test('screen and picture coordinates round-trip at 1x, 2x and after a pan', () => {
  const twoX = zoomShareAt(BOX, WINDOW, SHARE_ZOOM_FIT, { x: 400, y: 225 }, 2);
  const panned = panShare(BOX, WINDOW, twoX, -120, 60);
  assert.notDeepEqual(panned, twoX, 'the pan moved the view');
  for (const [label, zoom] of [
    ['1x', SHARE_ZOOM_FIT],
    ['2x', twoX],
    ['2x panned', panned],
  ] as Array<[string, ShareZoom]>) {
    for (const picture of [
      { x: 0.5, y: 0.5 },
      { x: 0.31, y: 0.62 },
      { x: 0.7, y: 0.4 },
    ]) {
      const screen = sharePictureToPoint(BOX, WINDOW, zoom, picture);
      closePoint(sharePointToPicture(BOX, WINDOW, zoom, screen), picture, `${label} ${picture.x},${picture.y}`);
    }
  }
});

test('panning stops at the picture edge and a letterboxed axis stays centred', () => {
  const twoX = zoomShareAt(BOX, WINDOW, SHARE_ZOOM_FIT, { x: 400, y: 225 }, 2);
  const far = panShare(BOX, WINDOW, twoX, 10_000, 10_000);
  const transform = shareZoomTransform(BOX, WINDOW, far);
  // Picture left/top edges pinned to the box's left/top edges (no black).
  close(transform.x + transform.scale * 100, 0);
  close(transform.y, 0);
  // At a zoom where the picture is still narrower than the box, x stays centred.
  const slight = clampShareZoom(BOX, WINDOW, { scale: 1.2, centerX: 0.05, centerY: 0.5 });
  close(slight.centerX, 0.5);
  // Scale is bounded to [fit, max].
  assert.deepEqual(clampShareZoom(BOX, WINDOW, { scale: 0.3, centerX: 0.2, centerY: 0.2 }), SHARE_ZOOM_FIT);
  close(clampShareZoom(BOX, WINDOW, { scale: 50, centerX: 0.5, centerY: 0.5 }).scale, SHARE_ZOOM_MAX);
  assert.equal(isShareZoomed(SHARE_ZOOM_FIT), false);
});

test('the media clip maps exactly onto the video box under any zoom', () => {
  const zoom = panShare(BOX, WINDOW, zoomShareAt(BOX, WINDOW, SHARE_ZOOM_FIT, { x: 600, y: 100 }, 3), 40, -30);
  const transform = shareZoomTransform(BOX, WINDOW, zoom);
  const clip = shareZoomClipInsets(BOX, transform);
  close(transform.x + transform.scale * clip.left, 0);
  close(transform.y + transform.scale * clip.top, 0);
  close(transform.x + transform.scale * (BOX.width - clip.right), BOX.width);
  close(transform.y + transform.scale * (BOX.height - clip.bottom), BOX.height);
});

/** A share tile as the browser reports it with the zoom transform applied to
 * its video (top-left origin), the video docked below the header. */
function zoomedShareTile(zoom: ShareZoom, applyTransformToRect = true) {
  const tileRect: RectLike = { left: 40, top: 30, width: BOX.width, height: BOX.height + HEADER };
  const layout: RectLike = { left: tileRect.left, top: tileRect.top + HEADER, width: BOX.width, height: BOX.height };
  const painted = applyTransformToRect ? zoomedVideoBounds(layout, shareZoomTransform(BOX, WINDOW, zoom)) : layout;
  const video = {
    videoWidth: WINDOW.width,
    videoHeight: WINDOW.height,
    dataset: {},
    getBoundingClientRect: () => ({ ...painted }),
  };
  return {
    tile: {
      querySelector: (selector: string) => (selector === 'video' ? video : null),
      getBoundingClientRect: () => ({ ...tileRect }),
    } as unknown as HTMLDivElement,
    layout,
  };
}

test('a telepointer lands on the same content pixel at 1x, at 2x, and after a pan', () => {
  const twoX = zoomShareAt(BOX, WINDOW, SHARE_ZOOM_FIT, { x: 250, y: 300 }, 2);
  const panned = panShare(BOX, WINDOW, twoX, 90, -40);
  const message = { x: 0.42, y: 0.37 };
  for (const [label, zoom] of [
    ['1x', SHARE_ZOOM_FIT],
    ['2x', twoX],
    ['2x panned', panned],
  ] as Array<[string, ShareZoom]>) {
    const { tile } = zoomedShareTile(zoom);
    // Exactly what telepointerDisplay.ts positionTelepointer does.
    const { bounds, media } = mediaContentRectRelativeToTile(tile);
    const pointer = telepointerPosition(bounds, media, message);
    // That tile-relative point, in the video box, is the same picture pixel.
    const inBox = { x: pointer.x, y: pointer.y - HEADER };
    closePoint(sharePointToPicture(BOX, WINDOW, zoom, inBox), message, label);
    closePoint(inBox, sharePictureToPoint(BOX, WINDOW, zoom, message), label);

    // Remote control / draw capture map a click there back to the same point.
    const viewport = mediaContentRect(tile);
    closePoint(
      normalizedPointInContainedMedia(viewport.bounds, viewport.media, { x: 40 + pointer.x, y: 30 + pointer.y }),
      message,
      `${label} input`
    );
  }
});

test('mutation guard: an overlay that ignored the zoom transform would miss the pixel', () => {
  const twoX = zoomShareAt(BOX, WINDOW, SHARE_ZOOM_FIT, { x: 250, y: 300 }, 2);
  const message = { x: 0.42, y: 0.37 };
  const { tile } = zoomedShareTile(twoX, false);
  const { bounds, media } = mediaContentRectRelativeToTile(tile);
  const pointer = telepointerPosition(bounds, media, message);
  const landed = sharePointToPicture(BOX, WINDOW, twoX, { x: pointer.x, y: pointer.y - HEADER });
  assert.ok(!landed || Math.hypot(landed.x - message.x, landed.y - message.y) > 0.05);
});

test('double-tap on a tall window steps 2.5x, then fill, then fit', () => {
  const tall = { width: 600, height: 1300 };
  const fill = shareFillScale(BOX, tall);
  assert.ok(fill > 3.8);
  const first = toggleShareFitFill(BOX, tall, SHARE_ZOOM_FIT);
  close(first.scale, SHARE_ZOOM_DOUBLE_TAP_MAX);
  const second = toggleShareFitFill(BOX, tall, first);
  close(second.scale, fill);
  assert.deepEqual(toggleShareFitFill(BOX, tall, second), SHARE_ZOOM_FIT);
  // The tapped point stays put on a step (x is clamped at the picture edge,
  // so check a point inside the letterboxed picture).
  const anchor = { x: 400, y: 140 };
  const before = sharePointToPicture(BOX, tall, SHARE_ZOOM_FIT, anchor)!;
  const stepped = toggleShareFitFill(BOX, tall, SHARE_ZOOM_FIT, anchor);
  closePoint(sharePointToPicture(BOX, tall, stepped, anchor), before, 'anchor');
});

test('a double-tap step that would stop just short of fill goes on to fill', () => {
  // A very tall window: fill is ~6.38x, so 2.5x -> 6.25x would leave a last
  // tap that moves 2%.
  const veryTall = { width: 300, height: 1077 };
  const fill = shareFillScale(BOX, veryTall);
  assert.ok(fill > 6.25 && fill < 6.25 * 1.1, `fill ${fill}`);
  const first = toggleShareFitFill(BOX, veryTall, SHARE_ZOOM_FIT);
  close(first.scale, SHARE_ZOOM_DOUBLE_TAP_MAX);
  const second = toggleShareFitFill(BOX, veryTall, first);
  close(second.scale, fill);
  assert.deepEqual(toggleShareFitFill(BOX, veryTall, second), SHARE_ZOOM_FIT);
  // The first tap too: a fill of 2.7x is one tap, not 2.5x and then 2.7x.
  const tallish = { width: 800, height: 1215 };
  const nearFill = shareFillScale(BOX, tallish);
  assert.ok(nearFill > 2.5 && nearFill < 2.5 * 1.1, `fill ${nearFill}`);
  close(toggleShareFitFill(BOX, tallish, SHARE_ZOOM_FIT).scale, nearFill);
});

function zoomTile(classes: string[], dataset: Record<string, string> = {}, video?: Partial<HTMLVideoElement>) {
  return {
    classList: { contains: (name: string) => classes.includes(name) },
    dataset,
    querySelector: () => video ?? null,
  } as unknown as HTMLElement;
}

test('viewer demand follows the committed zoom, not the frame being painted', () => {
  assert.equal(shareZoomDemandFactor(zoomTile(['share-tile'])), 1);
  // Mid-pinch at 3x with 2x committed: the painted rect is 3x the box, the
  // demand is 2x the box.
  const midPinch = zoomTile(['share-tile', 'is-share-zoomed'], {
    shareZoomPaintedScale: '3.0000',
    shareZoomDemandScale: '2.0000',
  });
  close(shareZoomDemandFactor(midPinch), 2 / 3);
  // Nothing committed yet (first pinch still in progress): the box itself.
  close(shareZoomDemandFactor(zoomTile(['share-tile', 'is-share-zoomed'], { shareZoomPaintedScale: '2.0000' })), 0.5);
  // A thumbnail paints no zoom and asks for its plain box.
  assert.equal(
    shareZoomDemandFactor(
      zoomTile(['share-tile', 'is-share-zoomed', 'is-spotlight-thumbnail'], { shareZoomDemandScale: '2.0000' })
    ),
    1
  );
});

test('overlays anchored outside the zoomed video box are reported hidden', () => {
  const video = { offsetLeft: 0, offsetTop: 44, offsetWidth: 800, offsetHeight: 450 };
  const zoomed = zoomTile(['share-tile', 'is-share-zoomed'], {}, video);
  assert.equal(pointVisibleInZoomedShare(zoomed, { x: 400, y: 200 }), true);
  assert.equal(pointVisibleInZoomedShare(zoomed, { x: 400, y: 20 }), false, 'under the header');
  assert.equal(pointVisibleInZoomedShare(zoomed, { x: -5, y: 200 }), false, 'off the left edge');
  assert.equal(pointVisibleInZoomedShare(zoomed, { x: 400, y: 600 }), false, 'below the tile');
  // At fit every anchor is on the (letterboxed) picture: never hidden.
  assert.equal(pointVisibleInZoomedShare(zoomTile(['share-tile'], {}, video), { x: 400, y: 20 }), true);
});
