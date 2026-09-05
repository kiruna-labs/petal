import assert from 'node:assert/strict';
import test from 'node:test';

import {
  containedMediaRect,
  normalizedControlPoint
} from '../src/lib/data/compositorControl.ts';

test('contained media rect fills bounds when media and bounds aspects match', () => {
  const bounds = { left: 10, top: 20, width: 640, height: 360 };

  assert.deepEqual(containedMediaRect(bounds, { width: 1920, height: 1080 }), bounds);
  assert.deepEqual(
    normalizedControlPoint(bounds, { width: 1920, height: 1080 }, { x: 330, y: 200 }),
    { x: 0.5, y: 0.5 }
  );
});

test('wide media normalizes clicks against the reduced content height', () => {
  const bounds = { left: 0, top: 0, width: 400, height: 400 };

  assert.deepEqual(containedMediaRect(bounds, { width: 1600, height: 900 }), {
    left: 0,
    top: 87.5,
    width: 400,
    height: 225
  });
  assert.deepEqual(
    normalizedControlPoint(bounds, { width: 1600, height: 900 }, { x: 200, y: 87.5 }),
    { x: 0.5, y: 0 }
  );
});

test('tall media normalizes clicks against the reduced content width', () => {
  const bounds = { left: 0, top: 0, width: 400, height: 400 };

  assert.deepEqual(containedMediaRect(bounds, { width: 900, height: 1600 }), {
    left: 87.5,
    top: 0,
    width: 225,
    height: 400
  });
  assert.deepEqual(
    normalizedControlPoint(bounds, { width: 900, height: 1600 }, { x: 87.5, y: 200 }),
    { x: 0, y: 0.5 }
  );
});

test('unknown source dimensions fall back to raw overlay bounds', () => {
  const bounds = { left: 10, top: 20, width: 300, height: 200 };

  assert.deepEqual(
    normalizedControlPoint(bounds, { width: 0, height: 1080 }, { x: 160, y: 120 }),
    { x: 0.5, y: 0.5 }
  );
  assert.deepEqual(
    normalizedControlPoint(bounds, { width: 1920, height: 0 }, { x: 160, y: 120 }),
    { x: 0.5, y: 0.5 }
  );
});
