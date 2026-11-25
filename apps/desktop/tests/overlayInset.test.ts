import assert from 'node:assert/strict';
import test from 'node:test';

import { overlayStage, parseOverlayInset, stagePoint } from '../src/lib/data/overlayInset.ts';

test('an inset overlay draws over the whole shared region', () => {
  // A 213x120 Petal View, overlay panel inset 8 pt: the window is 197x104.
  const stage = overlayStage(8, { width: 197, height: 104 });
  assert.deepEqual(stage, { left: -8, top: -8, width: 213, height: 120 });

  // The region's center lands where it did without the inset.
  assert.deepEqual(stagePoint(stage, 0.5, 0.5), { x: 98.5, y: 52 });
  // The region's corners fall outside the window, so the window clips them.
  assert.deepEqual(stagePoint(stage, 0, 0), { x: -8, y: -8 });
  assert.deepEqual(stagePoint(stage, 1, 1), { x: 205, y: 112 });
});

test('without an inset the stage is the window', () => {
  const stage = overlayStage(0, { width: 640, height: 360 });
  assert.deepEqual(stage, { left: 0, top: 0, width: 640, height: 360 });
  assert.deepEqual(stagePoint(stage, 0.25, 0.5), { x: 160, y: 180 });
});

test('the inset query value is a whole, non-negative number of points', () => {
  assert.equal(parseOverlayInset('8'), 8);
  assert.equal(parseOverlayInset(null), 0);
  assert.equal(parseOverlayInset(''), 0);
  assert.equal(parseOverlayInset('-3'), 0);
  assert.equal(parseOverlayInset('abc'), 0);
  assert.equal(parseOverlayInset('7.6'), 8);
});
