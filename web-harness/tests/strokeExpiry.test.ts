import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  isStrokeExpired,
  strokeFadeOpacity,
  STROKE_EXPIRE_MS,
  STROKE_FADE_DURATION_MS,
  STROKE_FADE_START_MS,
} from '@petal/shared/logic/strokeExpiry';

// -------------------------------------------------------------------------
// #670: on-window draw strokes must fade out 10s after their LAST point
// (SPEC.md "ephemeral by default"). This is the single shared predicate
// consumed by all three render surfaces -- see drawDisplay.ts (this
// package), apps/desktop/src/routes/compositor/pointer/+page.svelte, and
// apps/desktop/src/lib/components/ParticipantTile.svelte, all of which
// import `strokeFadeOpacity`/`isStrokeExpired` rather than reimplementing
// the threshold. Mirrors apps/desktop/tests/strokeExpiry.test.ts's coverage
// of the same shared module exactly.
// -------------------------------------------------------------------------

test('the user-specified 10s fade start (not an estimate, 2026-08-06)', () => {
  assert.equal(STROKE_FADE_START_MS, 10_000);
});

test('strokeFadeOpacity: a stroke at t=9.9s is still fully visible', () => {
  assert.equal(strokeFadeOpacity(9_900), 1);
});

test('strokeFadeOpacity: a stroke at t=10.1s has started fading', () => {
  const opacity = strokeFadeOpacity(10_100);
  assert.ok(opacity < 1, `expected < 1, got ${opacity}`);
  assert.ok(opacity > 0, `expected > 0, got ${opacity}`);
});

test('strokeFadeOpacity: exactly at the fade start is still fully visible (inclusive boundary)', () => {
  assert.equal(strokeFadeOpacity(STROKE_FADE_START_MS), 1);
});

test('strokeFadeOpacity: ramps linearly to 0 across the fade duration', () => {
  const midway = STROKE_FADE_START_MS + STROKE_FADE_DURATION_MS / 2;
  assert.ok(Math.abs(strokeFadeOpacity(midway) - 0.5) < 1e-9);
});

test('strokeFadeOpacity: never negative once fully expired', () => {
  assert.equal(strokeFadeOpacity(STROKE_EXPIRE_MS), 0);
  assert.equal(strokeFadeOpacity(STROKE_EXPIRE_MS + 5_000), 0);
});

test('isStrokeExpired: false while visible or fading, true once fully faded', () => {
  assert.equal(isStrokeExpired(0), false);
  assert.equal(isStrokeExpired(9_900), false);
  assert.equal(isStrokeExpired(10_100), false);
  assert.equal(isStrokeExpired(STROKE_EXPIRE_MS - 1), false);
  assert.equal(isStrokeExpired(STROKE_EXPIRE_MS), true);
  assert.equal(isStrokeExpired(STROKE_EXPIRE_MS + 1_000), true);
});

test('a stroke extended at t=9s ages from the NEW point, not the original (#670 requirement 4)', () => {
  // Simulates: a stroke begins at t=0, is still being drawn, and receives a
  // new point at t=9000ms. Every caller re-stamps "last point" on
  // continuation, so `ageMs` passed to this module is always relative to
  // the newest point -- here, checking again at t=9500ms means the real
  // age since the t=9000ms point is only 500ms, still fully visible, even
  // though 9500ms have passed since the stroke's FIRST point.
  const strokeBeganAtMs = 0;
  const continuedAtMs = 9_000;
  const checkedAtMs = 9_500;

  const ageFromFirstPoint = checkedAtMs - strokeBeganAtMs; // 9500ms -- still < 10s, coincidentally fine
  const ageFromLastPoint = checkedAtMs - continuedAtMs; // 500ms -- the correct basis

  assert.equal(strokeFadeOpacity(ageFromLastPoint), 1);
  assert.equal(strokeFadeOpacity(ageFromFirstPoint), 1);

  // Push the check past 10s since the FIRST point but still well within
  // 10s of the continuation -- this is the case that actually
  // distinguishes "ages from last point" from "ages from first point".
  const laterCheckMs = 10_500;
  const ageFromFirstPointLate = laterCheckMs - strokeBeganAtMs; // 10500ms -> would be fading if measured from the first point
  const ageFromLastPointLate = laterCheckMs - continuedAtMs; // 1500ms -> still fully visible

  assert.ok(strokeFadeOpacity(ageFromFirstPointLate) < 1, 'sanity: first-point basis would already be fading');
  assert.equal(strokeFadeOpacity(ageFromLastPointLate), 1, 'last-point basis must still be fully visible');
});
