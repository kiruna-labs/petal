import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  alignByCalibrationSquares,
  edgeSharpnessRatio,
  laplacianEnergy,
  lumaSsim,
  resolutionMatches,
  type PixelBuffer,
} from '../src/crispness.ts';

function buffer(width: number, height: number, fill: [number, number, number] = [0, 0, 0]): PixelBuffer {
  const data = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      setPixel({ width, height, data }, x, y, fill);
    }
  }
  return { width, height, data };
}

function setPixel(buf: PixelBuffer, x: number, y: number, rgb: [number, number, number]) {
  const offset = (y * buf.width + x) * 4;
  buf.data[offset] = rgb[0];
  buf.data[offset + 1] = rgb[1];
  buf.data[offset + 2] = rgb[2];
  buf.data[offset + 3] = 255;
}

function checker(width: number, height: number, cell = 2): PixelBuffer {
  const buf = buffer(width, height, [0, 0, 0]);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const v = (Math.floor(x / cell) + Math.floor(y / cell)) % 2 === 0 ? 255 : 0;
      setPixel(buf, x, y, [v, v, v]);
    }
  }
  return buf;
}

function blur(source: PixelBuffer): PixelBuffer {
  const out = buffer(source.width, source.height);
  for (let y = 0; y < source.height; y += 1) {
    for (let x = 0; x < source.width; x += 1) {
      let r = 0;
      let g = 0;
      let b = 0;
      let count = 0;
      for (let yy = Math.max(0, y - 1); yy <= Math.min(source.height - 1, y + 1); yy += 1) {
        for (let xx = Math.max(0, x - 1); xx <= Math.min(source.width - 1, x + 1); xx += 1) {
          const offset = (yy * source.width + xx) * 4;
          r += source.data[offset];
          g += source.data[offset + 1];
          b += source.data[offset + 2];
          count += 1;
        }
      }
      setPixel(out, x, y, [Math.round(r / count), Math.round(g / count), Math.round(b / count)]);
    }
  }
  return out;
}

test('resolutionMatches is a direct decoded/source equality check', () => {
  assert.equal(resolutionMatches(960, 600, 960, 600), true);
  assert.equal(resolutionMatches(960, 540, 960, 600), false);
});

test('laplacian sharpness ratio accepts identical patterns and rejects blurred ones', () => {
  const reference = checker(32, 32, 2);
  const received = checker(32, 32, 2);
  const softened = blur(reference);

  assert.ok(laplacianEnergy(reference) > laplacianEnergy(softened));
  const identical = edgeSharpnessRatio(received, reference, 1);
  assert.ok(Math.abs(identical.ratio - 1) < 0.0001);
  assert.equal(identical.pass, true);

  const blurred = edgeSharpnessRatio(softened, reference, 1);
  assert.ok(blurred.ratio < 0.75);
  assert.equal(blurred.pass, false);
});

test('alignByCalibrationSquares finds local corner offsets and rejects absent squares', () => {
  const colors = ['#ff2d55', '#00ff88', '#2d7dff', '#ffd400'] as [string, string, string, string];
  const buf = buffer(960, 600, [27, 16, 51]);
  const centers = [
    [28, 28],
    [932, 28],
    [28, 572],
    [932, 572],
  ] as const;
  const offset = { dx: 3, dy: -2 };
  const rgbs: Array<[number, number, number]> = [
    [255, 45, 85],
    [0, 255, 136],
    [45, 125, 255],
    [255, 212, 0],
  ];
  centers.forEach(([cx, cy], i) => {
    setPixel(buf, cx + offset.dx, cy + offset.dy, rgbs[i]);
  });

  assert.deepEqual(alignByCalibrationSquares(buf, colors), offset);
  assert.equal(alignByCalibrationSquares(buffer(960, 600, [27, 16, 51]), colors), null);
});

test('luma SSIM passes matching buffers and fails clearly different images under strict baseline', () => {
  const reference = checker(32, 32, 4);
  const matching = checker(32, 32, 4);
  const solid = buffer(32, 32, [128, 128, 128]);

  const same = lumaSsim(matching, reference, { dx: 0, dy: 0 }, 0.99);
  assert.ok(same.ssim > 0.99);
  assert.equal(same.pass, true);

  const different = lumaSsim(solid, reference, { dx: 0, dy: 0 }, 0.99);
  assert.ok(different.ssim < 0.5);
  assert.equal(different.pass, false);
});
