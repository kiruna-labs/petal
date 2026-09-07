import { test } from 'node:test';
import assert from 'node:assert/strict';

import { PLUGIN_LIMITS, createRateLimiter, jsonByteLength } from '@petal/shared/plugin-host/rateLimit';

test('token bucket: burst then refill at perSecond', () => {
  let now = 0;
  const limiter = createRateLimiter({ perSecond: 10, now: () => now });
  for (let i = 0; i < 10; i++) assert.ok(limiter.tryTake('a'), `take ${i}`);
  assert.ok(!limiter.tryTake('a'), 'bucket empty');
  assert.ok(limiter.tryTake('b'), 'keys are independent');
  now = 100; // 0.1 s -> one token
  assert.ok(limiter.tryTake('a'));
  assert.ok(!limiter.tryTake('a'));
  now = 5000;
  for (let i = 0; i < 10; i++) assert.ok(limiter.tryTake('a'), 'never above capacity');
  assert.ok(!limiter.tryTake('a'));
  limiter.reset('a');
  assert.ok(limiter.tryTake('a'));
});

test('fractional rates: one toast per two seconds', () => {
  let now = 0;
  const limiter = createRateLimiter({ perSecond: PLUGIN_LIMITS.toastPerSecond, burst: 1, now: () => now });
  assert.ok(limiter.tryTake('p'));
  assert.ok(!limiter.tryTake('p'));
  now = 1999;
  assert.ok(!limiter.tryTake('p'));
  now = 2000;
  assert.ok(limiter.tryTake('p'));
});

test('jsonByteLength counts UTF-8 bytes of the serialised value', () => {
  assert.equal(jsonByteLength('é'), 4); // quotes + 2-byte char
  assert.equal(jsonByteLength({ a: 1 }), 7);
  assert.equal(jsonByteLength(undefined), 0);
  const cyclic: Record<string, unknown> = {};
  cyclic.self = cyclic;
  assert.equal(jsonByteLength(cyclic), Number.POSITIVE_INFINITY);
});
