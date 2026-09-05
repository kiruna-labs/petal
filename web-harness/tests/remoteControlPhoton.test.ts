import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  CORNER_CALIBRATION_SQUARES,
  GRAY_CODE_BLOCK_RECTS,
  TEST_PATTERN_HEIGHT,
  TEST_PATTERN_WIDTH,
  frameCounterToGrayBlocks,
  type Rect
} from '../src/testPattern.ts';
import {
  decodePhotonSentinelFrame,
  matchesExpectedPhotonGeneration,
  nextPhotonGeneration
} from '../src/remoteControlPhoton.ts';

function parseHexColor(hex: string): [number, number, number] {
  const value = Number.parseInt(hex.slice(1), 16);
  return [(value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff];
}

function paintRect(
  data: Uint8ClampedArray,
  width: number,
  height: number,
  rect: Rect,
  color: [number, number, number]
) {
  const left = Math.floor((rect.x / TEST_PATTERN_WIDTH) * width);
  const top = Math.floor((rect.y / TEST_PATTERN_HEIGHT) * height);
  const right = Math.ceil(((rect.x + rect.w) / TEST_PATTERN_WIDTH) * width);
  const bottom = Math.ceil(((rect.y + rect.h) / TEST_PATTERN_HEIGHT) * height);
  for (let y = top; y < bottom; y += 1) {
    for (let x = left; x < right; x += 1) {
      const offset = (y * width + x) * 4;
      data[offset] = color[0];
      data[offset + 1] = color[1];
      data[offset + 2] = color[2];
      data[offset + 3] = 255;
    }
  }
}

function sentinelFrame(generation: number, scale = 1) {
  const width = Math.round(TEST_PATTERN_WIDTH * scale);
  const height = Math.round(TEST_PATTERN_HEIGHT * scale);
  const data = new Uint8ClampedArray(width * height * 4);
  for (let offset = 0; offset < data.length; offset += 4) {
    data[offset] = 27;
    data[offset + 1] = 16;
    data[offset + 2] = 51;
    data[offset + 3] = 255;
  }
  for (const square of CORNER_CALIBRATION_SQUARES) {
    paintRect(data, width, height, square, parseHexColor(square.color));
  }
  const bits = frameCounterToGrayBlocks(generation);
  for (let index = 0; index < bits.length; index += 1) {
    const value = bits[index] ? 235 : 16;
    paintRect(data, width, height, GRAY_CODE_BLOCK_RECTS[index], [value, value, value]);
  }
  return { data, width, height };
}

test('photon sentinel decoder recovers static generations at full and half resolution', () => {
  for (const generation of [0, 1, 42, 0xffff]) {
    assert.equal(decodePhotonSentinelFrame(sentinelFrame(generation))?.generation, generation);
    assert.equal(decodePhotonSentinelFrame(sentinelFrame(generation, 0.5))?.generation, generation);
  }
});

test('photon sentinel decoder rejects ambiguous bits and missing calibration', () => {
  const ambiguous = sentinelFrame(7);
  paintRect(ambiguous.data, ambiguous.width, ambiguous.height, GRAY_CODE_BLOCK_RECTS[3], [128, 128, 128]);
  assert.equal(decodePhotonSentinelFrame(ambiguous), null);

  const uncalibrated = sentinelFrame(7);
  paintRect(uncalibrated.data, uncalibrated.width, uncalibrated.height, CORNER_CALIBRATION_SQUARES[0], [0, 0, 0]);
  assert.equal(decodePhotonSentinelFrame(uncalibrated), null);
});

test('photon generation matching rejects stale frames and handles wraparound', () => {
  const baseline = decodePhotonSentinelFrame(sentinelFrame(42));
  const stale = decodePhotonSentinelFrame(sentinelFrame(42));
  const expected = decodePhotonSentinelFrame(sentinelFrame(43));
  assert.ok(baseline);
  assert.equal(nextPhotonGeneration(baseline.generation), 43);
  assert.equal(matchesExpectedPhotonGeneration(stale, 43), false);
  assert.equal(matchesExpectedPhotonGeneration(expected, 43), true);
  assert.equal(nextPhotonGeneration(0xffff), 0);
});
