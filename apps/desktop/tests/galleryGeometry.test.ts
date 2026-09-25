import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  computeGalleryLayout,
  GAP_COMPACT,
  GAP_TINY,
  scoreGalleryCandidate,
  tierGap
} from '@petal/shared/logic/galleryGeometry';

// #P0: apps/desktop/src/lib/galleryLayout.ts used to hard-code a 2x2 grid for
// 3-4 participants and start its column search at 2, so a single column was
// unreachable for any count -- 4 participants in a 1100x1750 window filled
// 34% of the area instead of the 66% a 1x4 column gets. This table is the
// orchestrator-specified expected shape at gap 18 / 16:9 for each case; every
// number below was computed by calling the real function, not asserted from
// the spec text.
const TABLE: Array<[count: number, width: number, height: number, columns: number, rows: number]> = [
  [4, 1100, 1750, 1, 4],
  [4, 900, 500, 2, 2],
  [3, 1600, 900, 2, 2],
  [6, 460, 900, 1, 6],
  [9, 1600, 900, 3, 3],
  [7, 1100, 520, 3, 3],
  [8, 1100, 1750, 2, 4],
  [2, 320, 700, 1, 2],
  [2, 900, 420, 2, 1],
  [3, 2400, 300, 3, 1],
  [5, 1100, 520, 3, 2]
];

test('expected-results table', () => {
  for (const [count, width, height, columns, rows] of TABLE) {
    const layout = computeGalleryLayout(count, width, height);
    assert.equal(
      layout.columns,
      columns,
      `${count}@${width}x${height}: expected ${columns} columns, got ${layout.columns}`
    );
    assert.equal(
      layout.rows,
      rows,
      `${count}@${width}x${height}: expected ${rows} rows, got ${layout.rows}`
    );
  }
});

test('4@1100x1750 fills far more area as a single column than the old hard-coded 2x2 did', () => {
  const layout = computeGalleryLayout(4, 1100, 1750);
  const old2x2Fill = scoreGalleryCandidate(4, 2, 1100, 1750, 18, 16 / 9).fill;
  assert.ok(layout.fill > old2x2Fill, `1x4 fill ${layout.fill} should beat 2x2 fill ${old2x2Fill}`);
  assert.ok(layout.fill > 0.6, `expected roughly 0.66 fill, got ${layout.fill}`);
});

test('a single reachable column (1xN) wins when the container is tall and narrow', () => {
  const layout = computeGalleryLayout(3, 1100, 1750);
  assert.deepEqual([layout.columns, layout.rows], [1, 3]);
});

test('a single reachable row (Nx1) wins when the container is short and wide', () => {
  const layout = computeGalleryLayout(3, 2400, 300);
  assert.deepEqual([layout.columns, layout.rows], [3, 1]);
});

test('empty-cell penalty prefers a fuller 3x2 over a 4x2 with three dead cells', () => {
  // 5 tiles in 3x2 leaves 1 empty cell; in 4x2 it leaves 3. Both are
  // candidates the raw fill-only search would consider; the penalty is what
  // picks 3x2.
  const layout = computeGalleryLayout(5, 1100, 520);
  assert.deepEqual([layout.columns, layout.rows], [3, 2]);
  const threeByTwo = scoreGalleryCandidate(5, 3, 1100, 520, 18, 16 / 9);
  const fourByTwo = scoreGalleryCandidate(5, 4, 1100, 520, 18, 16 / 9);
  assert.ok(threeByTwo.score > fourByTwo.score);
});

test('empty-cell penalty actually flips the winner, not just correlates with it', () => {
  // At 5@300x460, 2x3 (1 empty cell) has the HIGHER raw fill (0.4052) than
  // 1x5 (0 empty cells, fill 0.3879) -- so the 3x2-vs-4x2 case above would
  // still pass even with the penalty deleted (3x2 already wins on raw fill
  // alone there). This case only passes if the penalty is applied: it knocks
  // 2x3's score to 0.3728, below 1x5's 0.3879, flipping the winner.
  const layout = computeGalleryLayout(5, 300, 460);
  assert.deepEqual([layout.columns, layout.rows], [1, 5]);
  const oneByFive = scoreGalleryCandidate(5, 1, 300, 460, 18, 16 / 9);
  const twoByThree = scoreGalleryCandidate(5, 2, 300, 460, 18, 16 / 9);
  assert.ok(twoByThree.fill > oneByFive.fill, 'sanity: 2x3 must win on raw fill for this case to test the penalty');
  assert.ok(oneByFive.score > twoByThree.score, 'the penalty must flip the ranking versus raw fill');
});

test('hysteresis: a near-tied previous shape is kept instead of flipping', () => {
  // At 525x450, 3 participants naturally best-fit 1x3 (score ~0.4299) with
  // 2x2 close behind (~0.4223, ~1.8% worse) -- inside the 8% switch
  // threshold, so a previous 2x2 should stick rather than reflow to 1x3.
  const layout = computeGalleryLayout(3, 525, 450, {
    previous: { count: 3, columns: 2, rows: 2 }
  });
  assert.deepEqual([layout.columns, layout.rows], [2, 2]);
});

test('hysteresis: a previous shape far behind the best candidate is replaced', () => {
  // At 900x500, 4 participants best-fit 2x2 (score ~0.918); a previous 1x4
  // (score ~0.196) is nowhere near the 8% band, so the layout must repack.
  const layout = computeGalleryLayout(4, 900, 500, {
    previous: { count: 4, columns: 1, rows: 4 }
  });
  assert.deepEqual([layout.columns, layout.rows], [2, 2]);
});

test('hysteresis: a previous shape for a different participant count is ignored', () => {
  const layout = computeGalleryLayout(4, 900, 500, {
    previous: { count: 3, columns: 1, rows: 3 }
  });
  assert.deepEqual([layout.columns, layout.rows], [2, 2]);
});

test("arrangement 'column' forces a single column and reports overflow once tiles hit the floor", () => {
  const layout = computeGalleryLayout(20, 400, 600, { arrangement: 'column' });
  assert.equal(layout.columns, 1);
  assert.equal(layout.rows, 20);
  assert.equal(layout.overflow, true);
  assert.equal(layout.tileHeight, 96);
});

test("arrangement 'row' is the shape transpose of 'column'", () => {
  // Tile aspect stays 16:9 (width > height) regardless of arrangement, so
  // this only transposes the GRID shape (columns/rows), not tile pixels.
  const column = computeGalleryLayout(5, 600, 1200, { arrangement: 'column' });
  const row = computeGalleryLayout(5, 1200, 600, { arrangement: 'row' });
  assert.deepEqual([column.columns, column.rows], [1, 5]);
  assert.deepEqual([row.columns, row.rows], [5, 1]);
});

test('count <= 1 keeps the wider density thresholds', () => {
  assert.equal(computeGalleryLayout(1, 240, 165).compact, true);
  assert.equal(computeGalleryLayout(1, 260, 175).compact, false);
  assert.equal(computeGalleryLayout(1, 180, 125).tiny, true);
});

// Owner feedback: tighten the gap as tiles get small. The orchestrator's
// suggested "4@380x360 (compact)" case does NOT land in the compact tier
// under the real algorithm -- 4 participants there resolve to 2x2 with
// cellWidth=181/cellHeight=171 (both above the 170/105 compact thresholds),
// so it stays at the base gap. Verified with the real function and swapped
// for 4@290x250, which genuinely lands compact-but-not-tiny (cell
// 136x116 at the base gap). Not tuning the module to fit the suggested
// numbers -- this file's own header already commits to that discipline.
test('a compact (but not tiny) layout tightens the gap to GAP_COMPACT', () => {
  const layout = computeGalleryLayout(4, 290, 250);
  assert.deepEqual([layout.columns, layout.rows], [2, 2]);
  assert.equal(layout.compact, true);
  assert.equal(layout.tiny, false);
  assert.equal(layout.gap, GAP_COMPACT);
  // The tighter gap must actually enlarge the tile versus the base gap,
  // not just report a different number -- pin the real before/after pixels.
  const atBaseGap = scoreGalleryCandidate(4, 2, 290, 250, 18, 16 / 9);
  assert.ok(
    layout.tileWidth > atBaseGap.tileWidth,
    `tile should grow once the gap tightens: ${layout.tileWidth} should exceed ${atBaseGap.tileWidth}`
  );
});

test('a tiny layout tightens the gap all the way to GAP_TINY', () => {
  const layout = computeGalleryLayout(9, 380, 360);
  assert.deepEqual([layout.columns, layout.rows], [3, 3]);
  assert.equal(layout.tiny, true);
  assert.equal(layout.gap, GAP_TINY);
});

test('a roomy layout keeps the base gap', () => {
  const layout = computeGalleryLayout(4, 900, 500);
  assert.equal(layout.compact, false);
  assert.equal(layout.tiny, false);
  assert.equal(layout.gap, 18);
});

test("gap tiering applies to a forced 'column' arrangement too", () => {
  const layout = computeGalleryLayout(20, 400, 600, { arrangement: 'column' });
  assert.equal(layout.gap, GAP_TINY);
  // Unchanged from the pre-gap-tiering behavior: the minTileHeight clamp
  // still wins regardless of which gap tier fed into it.
  assert.equal(layout.overflow, true);
  assert.equal(layout.tileHeight, 96);
});

// #239: the web client's phone breakpoints pass a 10px/9px base gap. The
// tiers used to return their constant unconditionally, so a compact phone
// cell was WIDENED to 12px -- the opposite of the header comment's promise.
test('gap tiering only ever tightens the base gap, never widens it', () => {
  for (const baseGap of [0, 4, 8, 9, 10, 12, 16, 18, 24]) {
    for (const flags of [
      { compact: false, tiny: false },
      { compact: true, tiny: false },
      { compact: true, tiny: true }
    ]) {
      assert.ok(
        tierGap(baseGap, flags) <= baseGap,
        `tierGap(${baseGap}, ${JSON.stringify(flags)}) = ${tierGap(baseGap, flags)} exceeds the base gap`
      );
    }
  }
  // The same compact phone layout the web client packs at its 560px
  // breakpoint: the cells are compact, and the 10px gap it asked for holds.
  const phone = computeGalleryLayout(4, 290, 250, { gap: 10 });
  assert.equal(phone.compact, true);
  assert.equal(phone.gap, 10);
  const tinyPhone = computeGalleryLayout(9, 380, 360, { gap: 9 });
  assert.equal(tinyPhone.tiny, true);
  assert.equal(tinyPhone.gap, GAP_TINY, 'a base gap above GAP_TINY still tightens to it');
});

// #248: camera-only layouts may crop, so the packer scores tiles whose aspect
// runs from ~7:6 to 16:9 (`tileAspectRange`). The fixed-16:9 default above is
// unchanged -- every case in the table still packs identically.
const CAMERA_RANGE = { min: (16 / 9) * (2 / 3), max: 16 / 9 };

test('2 participants on a landscape phone fill the tile area height once tiles may crop', () => {
  // A landscape phone's tile surface: wide and short.
  const fixed = computeGalleryLayout(2, 780, 300, { gap: 10 });
  const ranged = computeGalleryLayout(2, 780, 300, { gap: 10, tileAspectRange: CAMERA_RANGE });
  assert.deepEqual([ranged.columns, ranged.rows], [2, 1]);
  assert.ok(fixed.tileHeight < 0.75 * 300, `16:9 letterboxes the height (${fixed.tileHeight}px of 300)`);
  assert.equal(ranged.tileHeight, 300, 'the cropping tiles fill the whole height');
  assert.ok(ranged.fill > 0.95, `fill ${ranged.fill}`);
  assert.ok(ranged.fill > fixed.fill + 0.2);
});

test('2 participants on a portrait phone fill the height as a column of cropping tiles', () => {
  const ranged = computeGalleryLayout(2, 360, 560, { gap: 10, tileAspectRange: CAMERA_RANGE });
  assert.deepEqual([ranged.columns, ranged.rows], [1, 2]);
  assert.equal(ranged.tileWidth, 360);
  assert.equal(ranged.tileHeight, (560 - 10) / 2);
});

test('a ranged tile takes its cell shape clamped into the range, never outside it', () => {
  // Wide cells clamp at 16:9 (no top/bottom crop is ever planned)...
  const wide = computeGalleryLayout(2, 2000, 300, { gap: 10, tileAspectRange: CAMERA_RANGE });
  assert.ok(Math.abs(wide.tileWidth / wide.tileHeight - 16 / 9) < 1e-9);
  // ...tall cells clamp at the side-crop cap (~7:6).
  const tall = computeGalleryLayout(1, 360, 800, { tileAspectRange: CAMERA_RANGE });
  assert.ok(Math.abs(tall.tileWidth / tall.tileHeight - CAMERA_RANGE.min) < 1e-9);
  assert.equal(tall.tileWidth, 360);
  // The candidate scorer takes the range too (the lab and tests use it).
  const scored = scoreGalleryCandidate(2, 2, 780, 300, 10, CAMERA_RANGE);
  assert.equal(scored.tileHeight, 300);
});

test('the range never packs the table worse, and repacks where a cropping shape fills more', () => {
  for (const [count, width, height] of TABLE) {
    const fixed = computeGalleryLayout(count, width, height);
    const ranged = computeGalleryLayout(count, width, height, { tileAspectRange: CAMERA_RANGE });
    assert.ok(ranged.fill >= fixed.fill - 1e-9, `${count}@${width}x${height}: ${ranged.fill} < ${fixed.fill}`);
  }
  // 6 in a tall narrow 460x900 window: 16:9 needs a 1x6 column (47% fill);
  // cropping lets a 2x3 grid of ~7:6 tiles fill ~60%.
  const six = computeGalleryLayout(6, 460, 900, { tileAspectRange: CAMERA_RANGE });
  assert.deepEqual([six.columns, six.rows], [2, 3]);
  assert.ok(six.fill > computeGalleryLayout(6, 460, 900).fill + 0.1);
});

test('hysteresis scores the previous shape with the same range', () => {
  // 2@560x400: a ranged 1x2 (score ~0.579) narrowly beats a ranged 2x1
  // (~0.553, inside the 8% band) -- so a previous 2x1 must stick. Scored at a
  // fixed 16:9 instead, that 2x1 would drop to ~0.369 and be thrown away.
  const fresh = computeGalleryLayout(2, 560, 400, { tileAspectRange: CAMERA_RANGE });
  assert.deepEqual([fresh.columns, fresh.rows], [1, 2]);
  const kept = computeGalleryLayout(2, 560, 400, {
    tileAspectRange: CAMERA_RANGE,
    previous: { count: 2, columns: 2, rows: 1 }
  });
  assert.deepEqual([kept.columns, kept.rows], [2, 1]);
});

test("a forced line with a range keeps the clamped tile's own aspect when it overflows", () => {
  const layout = computeGalleryLayout(20, 400, 600, { arrangement: 'column', tileAspectRange: CAMERA_RANGE });
  assert.equal(layout.overflow, true);
  assert.equal(layout.tileHeight, 96);
  assert.ok(Math.abs(layout.tileWidth - 96 * (16 / 9)) < 1e-9);
});
