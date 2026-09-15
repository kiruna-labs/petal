import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  computeGalleryLayout,
  GAP_COMPACT,
  GAP_TINY,
  scoreGalleryCandidate
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
