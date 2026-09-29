import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  CAMERA_MAX_SIDE_CROP,
  CAMERA_MAX_VERTICAL_CROP,
  CAMERA_TILE_ASPECT_RANGE,
  cameraFit,
  coverCropFractions,
  renderedMediaRect
} from '@petal/shared/logic/cameraCrop';
import { computeGalleryLayout } from '@petal/shared/logic/galleryGeometry';

// #248: camera tiles crop to fill their box -- up to about a third of the
// width off the sides, at most 10% off the top and bottom -- and letterbox
// past either cap. Shares are never cropped. These pin the caps themselves,
// the packer range derived from them, and both clients' wiring.

const HD = { width: 1280, height: 720 };
const PORTRAIT_PHONE = { width: 720, height: 1280 };

function box(aspect: number, height = 360) {
  return { width: aspect * height, height };
}

test('a 16:9 camera in a 4:3 cell crops its sides (cover)', () => {
  const crop = coverCropFractions(HD, box(4 / 3));
  assert.ok(Math.abs(crop.sides - 0.25) < 1e-9, `4:3 of a 16:9 frame drops a quarter of the width, got ${crop.sides}`);
  assert.equal(crop.vertical, 0);
  assert.equal(cameraFit(HD, box(4 / 3)), 'cover');
});

test('a 9:16 portrait camera in a 16:9 cell letterboxes (contain) instead of losing the face', () => {
  const crop = coverCropFractions(PORTRAIT_PHONE, box(16 / 9));
  assert.ok(crop.vertical > 0.6, `cover would cut ${crop.vertical} of the height`);
  assert.equal(cameraFit(PORTRAIT_PHONE, box(16 / 9)), 'contain');
  // Also in the narrowest camera cell the packer can produce.
  assert.equal(cameraFit(PORTRAIT_PHONE, box(CAMERA_TILE_ASPECT_RANGE.min)), 'contain');
  // A portrait cell of its own shape is not a crop at all.
  assert.equal(cameraFit(PORTRAIT_PHONE, box(9 / 16)), 'cover');
});

test('the side cap is about a third of the width and the vertical cap 10%', () => {
  assert.equal(CAMERA_MAX_SIDE_CROP, 1 / 3);
  assert.equal(CAMERA_MAX_VERTICAL_CROP, 0.1);
  // 16:9 into a square cell would drop 44% of the width: past the side cap.
  assert.equal(cameraFit(HD, box(1)), 'contain');
  // A 4:3 webcam in a 16:9 cell would drop 25% of its height: past 10%.
  assert.equal(cameraFit({ width: 640, height: 480 }, box(16 / 9)), 'contain');
  // 16:9 into a 1.9:1 cell drops ~6% of the height: inside the cap.
  assert.equal(cameraFit(HD, box(1.9)), 'cover');
  // 16:9 into 2.1:1 drops ~15%: past it (heads live up there).
  assert.equal(cameraFit(HD, box(2.1)), 'contain');
});

test('an unknown video size never crops', () => {
  assert.equal(cameraFit({ width: 0, height: 0 }, box(16 / 9)), 'contain');
  assert.equal(cameraFit(HD, { width: 0, height: 0 }), 'contain');
  assert.equal(cameraFit({ width: Number.NaN, height: 720 }, box(16 / 9)), 'contain');
});

test('the packer range is the side cap applied to a 16:9 camera (about 7:6 up to 16:9)', () => {
  assert.ok(Math.abs(CAMERA_TILE_ASPECT_RANGE.min - 32 / 27) < 1e-9);
  assert.ok(Math.abs(CAMERA_TILE_ASPECT_RANGE.min - 7 / 6) < 0.02, 'about 7:6');
  assert.equal(CAMERA_TILE_ASPECT_RANGE.max, 16 / 9);
  // Exactly at the narrow end a 16:9 camera still covers -- and a tile laid
  // out half a pixel narrower than the packer asked must not flip to a
  // letterbox on the rounding.
  const narrowest = box(CAMERA_TILE_ASPECT_RANGE.min, 300);
  assert.equal(cameraFit(HD, narrowest), 'cover');
  assert.equal(cameraFit(HD, { width: narrowest.width - 0.5, height: narrowest.height }), 'cover');
});

test('every camera-only packed tile shows a 16:9 camera with cover, never a letterbox', () => {
  for (const count of [1, 2, 3, 4, 5, 6, 9]) {
    for (const [width, height] of [
      [780, 300],
      [360, 560],
      [1240, 640],
      [700, 940],
      [2400, 300],
      [320, 700]
    ]) {
      const layout = computeGalleryLayout(count, width, height, { tileAspectRange: CAMERA_TILE_ASPECT_RANGE });
      const aspect = layout.tileWidth / layout.tileHeight;
      assert.ok(
        aspect >= CAMERA_TILE_ASPECT_RANGE.min - 1e-9 && aspect <= CAMERA_TILE_ASPECT_RANGE.max + 1e-9,
        `${count}@${width}x${height}: tile aspect ${aspect} left the range`
      );
      assert.equal(
        cameraFit(HD, { width: layout.tileWidth, height: layout.tileHeight }),
        'cover',
        `${count}@${width}x${height}: a planned 16:9 camera letterboxed`
      );
    }
  }
});

test('renderedMediaRect: cover overhangs the box, contain sits inside it, both centred', () => {
  const tile = { left: 10, top: 20, width: 300, height: 300 };
  const cover = renderedMediaRect(tile, HD, 'cover');
  const coverWidth = (1280 * 300) / 720;
  assert.ok(Math.abs(cover.width - coverWidth) < 1e-9);
  assert.equal(cover.height, 300);
  assert.ok(Math.abs(cover.left - (10 - (coverWidth - 300) / 2)) < 1e-9);
  assert.equal(cover.top, 20);
  const contain = renderedMediaRect(tile, HD, 'contain');
  assert.equal(contain.width, 300);
  assert.equal(contain.height, 168.75);
  assert.equal(contain.left, 10);
  assert.equal(contain.top, 20 + (300 - 168.75) / 2);
  // Unknown media: the box itself.
  assert.deepEqual(renderedMediaRect(tile, { width: 0, height: 0 }, 'cover'), tile);
});

const participantTile = readFileSync(
  new URL('../src/lib/components/ParticipantTile.svelte', import.meta.url),
  'utf8'
);
const gallery = readFileSync(new URL('../src/lib/components/Gallery.svelte', import.meta.url), 'utf8');

test('desktop camera tiles decide cover/contain from the shared caps, not a blanket cover', () => {
  assert.match(participantTile, /import \{ cameraFit, renderedMediaRect, type CameraFit \} from '@petal\/shared\/logic\/cameraCrop'/);
  assert.match(participantTile, /const next = cameraFit\(media, box\);/);
  assert.match(participantTile, /class:contain=\{videoFit === 'contain'\}/);
  assert.match(participantTile, /\.video-el\.contain \{\s*object-fit: contain;/);
  // Re-decided on a new track / rotated camera and on every tile resize.
  assert.match(participantTile, /video\.addEventListener\('loadedmetadata', sync\)/);
  assert.match(participantTile, /video\.addEventListener\('resize', sync\)/);
  assert.match(participantTile, /new ResizeObserver\(sync\)/);
});

test('desktop camera drawings sit on the rendered picture, not the bare tile', () => {
  assert.match(participantTile, /renderedMediaRect\(box, videoIntrinsicSize, videoFit\)/);
  assert.match(participantTile, /style:left=\{drawLayerStyle\?\.left\}/);
  assert.match(participantTile, /style:height=\{drawLayerStyle\?\.height\}/);
});

test('desktop letterbox bars are near-black, like the web client', () => {
  assert.match(participantTile, /\.video-el\.contain \{[^}]*object-fit: contain;[^}]*background: var\(--bg-base\);/);
});

test('the desktop spotlight hero is its area clamped to the camera range, centred', () => {
  // The rail is the size container the hero measures against.
  assert.match(gallery, /\.tiles\.spotlight \.spotlight-rail \{[^}]*container-type: size;/);
  const hero = /\.tiles\.spotlight \.spotlight-main \{(?<body>[^}]+)\}/.exec(gallery)?.groups?.body ?? '';
  assert.match(hero, /--hero-width: min\(100cqw, calc\(var\(--hero-area-height\) \* 16 \/ 9\)\);/);
  assert.match(hero, /width: var\(--hero-width\);/);
  assert.match(hero, /height: min\(var\(--hero-area-height\), calc\(var\(--hero-width\) \/ var\(--camera-tile-min-aspect, 1\.185\)\)\);/);
  assert.match(hero, /left: calc\(\(100% - var\(--hero-width\)\) \/ 2\);/);
  assert.match(hero, /margin: 0 auto 14px;/);
  assert.doesNotMatch(hero, /width: 100%;/);
  // The narrow end comes from the shared constant, not a second copy.
  assert.match(gallery, /--camera-tile-min-aspect: \$\{CAMERA_TILE_ASPECT_RANGE\.min\};/);
  // A 16:9 camera covers both ends of that box within the caps.
  for (const aspect of [16 / 9, CAMERA_TILE_ASPECT_RANGE.min]) {
    assert.equal(cameraFit(HD, box(aspect)), 'cover');
  }
});

test('the desktop gallery packs its camera tiles with the shared crop range', () => {
  assert.match(gallery, /import \{ CAMERA_TILE_ASPECT_RANGE \} from '@petal\/shared\/logic\/cameraCrop'/);
  assert.match(gallery, /tileAspectRange: CAMERA_TILE_ASPECT_RANGE/);
});
