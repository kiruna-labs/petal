import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  frameCounterToGrayBlocks,
  grayBlocksToFrameCounter,
  GRAY_CODE_BITS,
  prepareTestPatternCanvas,
  TEST_PATTERN_HEIGHT,
  TEST_PATTERN_WIDTH,
} from '../src/testPattern.ts';

function bitDifference(a: boolean[], b: boolean[]): number {
  return a.reduce((count, bit, i) => count + (bit === b[i] ? 0 : 1), 0);
}

test('Gray-code frame blocks round-trip 16-bit counters', () => {
  const counters = [
    ...Array.from({ length: 2048 }, (_, i) => i),
    0x7ffe,
    0x7fff,
    0x8000,
    0xfffe,
    0xffff,
    0x10000,
    0x10001,
  ];
  for (const counter of counters) {
    const blocks = frameCounterToGrayBlocks(counter);
    assert.equal(blocks.length, GRAY_CODE_BITS);
    assert.equal(grayBlocksToFrameCounter(blocks), counter & 0xffff);
  }
});

test('Gray-code frame blocks change one bit between adjacent frames', () => {
  for (let counter = 0; counter < 4096; counter += 1) {
    const current = frameCounterToGrayBlocks(counter);
    const next = frameCounterToGrayBlocks(counter + 1);
    assert.equal(bitDifference(current, next), 1);
  }
  assert.equal(bitDifference(frameCounterToGrayBlocks(0xffff), frameCounterToGrayBlocks(0)), 1);
});

test('Gray-code decoder rejects wrong bit counts', () => {
  assert.throws(() => grayBlocksToFrameCounter([true, false]), /expected 16/);
});

test('test-pattern canvas allocation is idempotent across animation ticks', () => {
  let width = 0;
  let height = 0;
  let widthWrites = 0;
  let heightWrites = 0;
  const canvas = {
    get width() { return width; },
    set width(value: number) { widthWrites += 1; width = value; },
    get height() { return height; },
    set height(value: number) { heightWrites += 1; height = value; },
  } as HTMLCanvasElement;

  prepareTestPatternCanvas(canvas);
  assert.equal(width, TEST_PATTERN_WIDTH);
  assert.equal(height, TEST_PATTERN_HEIGHT);
  assert.equal(widthWrites, 1);
  assert.equal(heightWrites, 1);

  prepareTestPatternCanvas(canvas);
  assert.equal(widthWrites, 1, 'steady-state draws must not reset the canvas backing store');
  assert.equal(heightWrites, 1, 'steady-state draws must not reset the canvas backing store');
});
