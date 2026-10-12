// stepFitLadder: one level per measurement, no flip-flop (the gallery's
// control bar once toggled labels on and off every two frames).
import test from 'node:test';
import assert from 'node:assert/strict';
import { stepFitLadder } from '../src/lib/meeting/fitLadder.ts';

test('steps up one level while the row does not fit, capped at the max level', () => {
  const memo: Array<number | undefined> = [];
  assert.equal(stepFitLadder(0, 700, 500, memo, 3), 1);
  assert.equal(stepFitLadder(1, 640, 500, memo, 3), 2);
  assert.equal(stepFitLadder(3, 400, 100, memo, 3), 3);
});

test('steps back down only when the wider level is known to fit', () => {
  const memo: Array<number | undefined> = [];
  stepFitLadder(0, 700, 500, memo, 2);
  assert.equal(stepFitLadder(1, 480, 500, memo, 2), 1, 'level 0 needed 700, still too wide');
  assert.equal(stepFitLadder(1, 480, 700, memo, 2), 0, 'level 0 now fits');
});

test('a row that fits exactly (sub-pixel slack) does not collapse', () => {
  const memo: Array<number | undefined> = [];
  assert.equal(stepFitLadder(0, 500.4, 500, memo, 2), 0);
});

test('never flips between two levels at a steady width', () => {
  const memo: Array<number | undefined> = [];
  const need = [700, 560, 420];
  let level = 0;
  const seen: number[] = [];
  for (let i = 0; i < 8; i++) {
    level = stepFitLadder(level, need[level], 600, memo, 2);
    seen.push(level);
  }
  assert.deepEqual(seen, [1, 1, 1, 1, 1, 1, 1, 1]);
});
