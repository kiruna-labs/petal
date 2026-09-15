import assert from 'node:assert/strict';
import { test } from 'node:test';

// Desktop's Gallery.svelte gets its packing geometry from the shared module
// (galleryGeometry.test.ts in this same directory owns the full expected-
// results table); this file just pins the handful of shapes Gallery.svelte's
// own CSS-var contract and `shouldCenterTail` depend on.
import { computeGalleryLayout } from '@petal/shared/logic/galleryGeometry';

test('single participant fills one tile slot', () => {
  assert.deepEqual(
    pick(computeGalleryLayout(1, 900, 500)),
    { columns: 1, rows: 1, compact: false, tiny: false }
  );
});

test('two participants adapt to wide and tall containers', () => {
  assert.deepEqual(pick(computeGalleryLayout(2, 900, 420)), {
    columns: 2,
    rows: 1,
    compact: false,
    tiny: false
  });
  assert.deepEqual(pick(computeGalleryLayout(2, 320, 700)), {
    columns: 1,
    rows: 2,
    compact: false,
    tiny: false
  });
});

test('three and four participants prefer a stable two by two layout at a square-ish size', () => {
  assert.equal(computeGalleryLayout(3, 900, 500).columns, 2);
  assert.equal(computeGalleryLayout(3, 900, 500).rows, 2);
  assert.equal(computeGalleryLayout(4, 900, 500).columns, 2);
  assert.equal(computeGalleryLayout(4, 900, 500).rows, 2);
});

test('four participants in a tall narrow window pack into a single column, not a starved 2x2', () => {
  // #P0: the old hard-coded 2x2 filled 34% of a 1100x1750 window; a 1x4
  // column fills ~66%. The search (columns 1..count) must reach 1 column.
  assert.deepEqual(pick(computeGalleryLayout(4, 1100, 1750)), {
    columns: 1,
    rows: 4,
    compact: false,
    tiny: false
  });
});

test('larger groups optimize by container aspect', () => {
  assert.deepEqual(pick(computeGalleryLayout(6, 1100, 520)), {
    columns: 3,
    rows: 2,
    compact: false,
    tiny: false
  });
  assert.deepEqual(pick(computeGalleryLayout(6, 460, 900)), {
    columns: 1,
    rows: 6,
    compact: false,
    tiny: false
  });
});

test('compact flags protect overlays in cramped layouts', () => {
  assert.equal(computeGalleryLayout(4, 320, 210).compact, true);
  assert.equal(computeGalleryLayout(8, 300, 190).tiny, true);
});

function pick(layout: ReturnType<typeof computeGalleryLayout>) {
  return {
    columns: layout.columns,
    rows: layout.rows,
    compact: layout.compact,
    tiny: layout.tiny
  };
}
