// Layout lab (#P0): the lab exists to PROVE the web and native panes render
// the shared packer's own output, not two copies of the algorithm that could
// silently drift. Two halves: a source-grep that the module actually imports
// the shared function and never reimplements its row math (the tell would be
// a stray `Math.ceil(`), and a behavioral check that the lab's own pane
// layout call agrees with calling the shared function directly.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import { computeGalleryLayout } from '@petal/shared/logic/galleryGeometry';
import { CAMERA_TILE_ASPECT_RANGE } from '@petal/shared/logic/cameraCrop';
import { computePaneLayout, paneContentSize } from '../src/layoutLab.ts';

const source = readFileSync(new URL('../src/layoutLab.ts', import.meta.url), 'utf8');

test('layoutLab.ts imports the shared packer', () => {
  assert.match(source, /from '@petal\/shared\/logic\/galleryGeometry'/);
});

test('layoutLab.ts never reimplements the row math', () => {
  assert.doesNotMatch(source, /Math\.ceil\(/);
});

test("a pane's layout call agrees with calling the shared function directly", () => {
  const webPane = { pad: 20 };
  const nativePane = { pad: 28 };
  const cases: Array<{ count: number; frameWidth: number; frameHeight: number }> = [
    { count: 4, frameWidth: 1100 + 40, frameHeight: 1750 + 40 }, // table case: 4@1100x1750 -> 1x4
    { count: 9, frameWidth: 1600 + 56, frameHeight: 900 + 56 } // table case: 9@1600x900 -> 3x3
  ];

  for (const { count, frameWidth, frameHeight } of cases) {
    const controls = {
      count,
      frameWidth,
      frameHeight,
      arrangement: 'auto' as const,
      gap: 18,
      tileAspect: 16 / 9
    };

    const webLayout = computePaneLayout(webPane, controls);
    const webContent = paneContentSize(frameWidth, frameHeight, webPane.pad);
    const directWeb = computeGalleryLayout(count, webContent.width, webContent.height, {
      gap: controls.gap,
      tileAspect: controls.tileAspect,
      arrangement: controls.arrangement
    });
    assert.deepEqual(
      [webLayout.columns, webLayout.rows],
      [directWeb.columns, directWeb.rows],
      `web pane disagreed with the shared function for ${count}@${frameWidth}x${frameHeight}`
    );

    const nativeLayout = computePaneLayout(nativePane, controls);
    const nativeContent = paneContentSize(frameWidth, frameHeight, nativePane.pad);
    const directNative = computeGalleryLayout(count, nativeContent.width, nativeContent.height, {
      gap: controls.gap,
      tileAspect: controls.tileAspect,
      arrangement: controls.arrangement
    });
    assert.deepEqual(
      [nativeLayout.columns, nativeLayout.rows],
      [directNative.columns, directNative.rows],
      `native pane disagreed with the shared function for ${count}@${frameWidth}x${frameHeight}`
    );
  }
});

test('#248: the camera crop toggle packs exactly like the camera-only web grid', () => {
  // Landscape-phone-sized frame: 2 cameras fill the height only with the range.
  const pane = { pad: 20 };
  const controls = {
    count: 2,
    frameWidth: 780 + 40,
    frameHeight: 300 + 40,
    arrangement: 'auto' as const,
    gap: 18,
    tileAspect: 16 / 9,
  };
  const fixed = computePaneLayout(pane, controls);
  const cropped = computePaneLayout(pane, { ...controls, cameraCrop: true });
  const direct = computeGalleryLayout(2, 780, 300, { gap: 18, tileAspectRange: CAMERA_TILE_ASPECT_RANGE });
  assert.deepEqual(
    [cropped.columns, cropped.rows, cropped.tileWidth, cropped.tileHeight],
    [direct.columns, direct.rows, direct.tileWidth, direct.tileHeight]
  );
  assert.equal(cropped.tileHeight, 300);
  assert.ok(fixed.tileHeight < 300);
  assert.match(source, /tileAspectRange: controls\.cameraCrop \? CAMERA_TILE_ASPECT_RANGE : null/);
});
