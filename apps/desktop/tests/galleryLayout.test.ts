import assert from 'node:assert/strict';
import { test } from 'node:test';

import { computeSmartGalleryLayout } from '../src/lib/galleryLayout.ts';

test('single participant fills one tile slot', () => {
  assert.deepEqual(
    pick(computeSmartGalleryLayout(1, 900, 500)),
    { columns: 1, rows: 1, compact: false, tiny: false }
  );
});

test('two participants adapt to wide and tall containers', () => {
  assert.deepEqual(pick(computeSmartGalleryLayout(2, 900, 420)), {
    columns: 2,
    rows: 1,
    compact: false,
    tiny: false
  });
  assert.deepEqual(pick(computeSmartGalleryLayout(2, 320, 700)), {
    columns: 1,
    rows: 2,
    compact: false,
    tiny: false
  });
});

test('three and four participants prefer a stable two by two layout', () => {
  assert.equal(computeSmartGalleryLayout(3, 900, 500).columns, 2);
  assert.equal(computeSmartGalleryLayout(3, 900, 500).rows, 2);
  assert.equal(computeSmartGalleryLayout(4, 900, 500).columns, 2);
  assert.equal(computeSmartGalleryLayout(4, 900, 500).rows, 2);
});

test('larger groups optimize by container aspect', () => {
  assert.deepEqual(pick(computeSmartGalleryLayout(6, 1100, 520)), {
    columns: 3,
    rows: 2,
    compact: false,
    tiny: false
  });
  assert.deepEqual(pick(computeSmartGalleryLayout(6, 460, 900)), {
    columns: 2,
    rows: 3,
    compact: false,
    tiny: false
  });
});

test('compact flags protect overlays in cramped layouts', () => {
  assert.equal(computeSmartGalleryLayout(4, 320, 210).compact, true);
  assert.equal(computeSmartGalleryLayout(8, 300, 190).tiny, true);
});

function pick(layout: ReturnType<typeof computeSmartGalleryLayout>) {
  return {
    columns: layout.columns,
    rows: layout.rows,
    compact: layout.compact,
    tiny: layout.tiny
  };
}
