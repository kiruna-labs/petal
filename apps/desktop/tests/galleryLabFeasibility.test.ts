// Unit tests for galleryGridFeasible (src/lib/dev/galleryLabMeasure.ts): the
// exact "can every tile still be a face" check the lab judge uses in place of
// an area estimate. Pure maths, no browser.

import assert from 'node:assert/strict';
import test from 'node:test';
import { galleryGridFeasible } from '../src/lib/dev/galleryLabMeasure.ts';

// Gap the narrow (compact) gallery packs with, see Gallery.svelte.
const COMPACT_GAP = 8;

test('a 240x250 surface cannot hold nine 80x54 faces (the n9 narrow cell)', () => {
  // 3 columns: cells 74.7 wide. 2 columns: 5 rows of 43.6 tall. 1 column: 9 rows.
  assert.equal(galleryGridFeasible({ width: 240, height: 250 }, 9, COMPACT_GAP), false);
});

test('a roomy surface holds nine 80x54 faces', () => {
  assert.equal(galleryGridFeasible({ width: 840, height: 560 }, 9, 18), true);
});

test('borderline: two faces side by side fit exactly at 168x54, one pixel less does not', () => {
  // 2 columns of 80 at a 8px gap is 168 wide; each 80x54 cell sits at the
  // 16:9-side camera aspect range, so the tile is exactly 80x54.
  assert.equal(galleryGridFeasible({ width: 168, height: 54 }, 2, COMPACT_GAP), true);
  assert.equal(galleryGridFeasible({ width: 167, height: 54 }, 2, COMPACT_GAP), false);
});

test('an area estimate would wrongly call the n9 narrow cell feasible', () => {
  // 240 * 250 = 60000 >= 9 * 80 * 54 * 1.3 = 50544, yet the grid is infeasible.
  const area = 240 * 250;
  assert.ok(area >= 9 * 80 * 54 * 1.3);
  assert.equal(galleryGridFeasible({ width: 240, height: 250 }, 9, COMPACT_GAP), false);
});
