import assert from 'node:assert/strict';
import test from 'node:test';

import { nextDeviceOptionIndex } from '../src/lib/components/deviceSelect';

test('device menu keyboard navigation wraps and supports endpoints', () => {
  assert.equal(nextDeviceOptionIndex(0, 'ArrowDown', 3), 1);
  assert.equal(nextDeviceOptionIndex(2, 'ArrowDown', 3), 0);
  assert.equal(nextDeviceOptionIndex(0, 'ArrowUp', 3), 2);
  assert.equal(nextDeviceOptionIndex(1, 'Home', 3), 0);
  assert.equal(nextDeviceOptionIndex(1, 'End', 3), 2);
  assert.equal(nextDeviceOptionIndex(1, 'Escape', 3), null);
});
