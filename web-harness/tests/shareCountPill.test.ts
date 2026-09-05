import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  formatShareCountPillLabel,
  resolveForemostSharedWindowId,
  SHARE_COUNT_PILL_CAP,
} from '../src/shareCountPill.ts';

test('the pill label is empty below 2 shares -- count 1 gets no aggregate cue, count 0 nothing to show', () => {
  assert.equal(formatShareCountPillLabel(0), '');
  assert.equal(formatShareCountPillLabel(1), '');
});

test('the pill label shows the exact count from 2 up to the cap', () => {
  assert.equal(formatShareCountPillLabel(2), '2');
  assert.equal(formatShareCountPillLabel(3), '3');
  assert.equal(formatShareCountPillLabel(SHARE_COUNT_PILL_CAP), '9');
});

test('the pill label caps at 9+ rather than ever showing a wider number', () => {
  assert.equal(formatShareCountPillLabel(SHARE_COUNT_PILL_CAP + 1), '9+');
  assert.equal(formatShareCountPillLabel(42), '9+');
});

test('resolveForemostSharedWindowId with metadata present picks the first zOrder id that has a live tile', () => {
  // zOrder id 10 has no tile (already closed, or a fraction ahead of its
  // tile finishing setup) -- 20 is the first entry that resolves.
  assert.equal(resolveForemostSharedWindowId([10, 20, 30], [30, 20]), 20);
});

test('resolveForemostSharedWindowId with metadata present but no overlap resolves to null, never guesses', () => {
  // The zOrder is authoritative once published -- it must not silently fall
  // back to "most recently added" just because none of its entries are
  // currently tiled.
  assert.equal(resolveForemostSharedWindowId([10, 11], [99]), null);
});

test('resolveForemostSharedWindowId treats an explicit empty zOrder as authoritative (no tile to resolve)', () => {
  assert.equal(resolveForemostSharedWindowId([], [7, 8]), null);
});

test('resolveForemostSharedWindowId falls back to the most-recently-added tile only when metadata is absent', () => {
  // Older sharer: no petalWindowZOrder published at all.
  assert.equal(resolveForemostSharedWindowId(null, [7, 8, 9]), 9);
});

test('resolveForemostSharedWindowId returns null when there is nothing tiled at all', () => {
  assert.equal(resolveForemostSharedWindowId(null, []), null);
  assert.equal(resolveForemostSharedWindowId([1, 2], []), null);
});
