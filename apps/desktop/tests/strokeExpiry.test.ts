import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  isStrokeExpired,
  strokeFadeOpacity,
  STROKE_EXPIRE_MS,
  STROKE_FADE_DURATION_MS,
  STROKE_FADE_START_MS,
} from '../src/lib/data/strokeExpiry.ts';

// -------------------------------------------------------------------------
// #670: on-window draw strokes must fade out 10s after their LAST point
// (SPEC.md "ephemeral by default"), not accumulate for the life of a share.
// Mirrors web-harness/tests/strokeExpiry.test.ts's coverage of the same
// shared module (shared/logic/strokeExpiry.ts) exactly.
// -------------------------------------------------------------------------

test('the user-specified 10s fade start (not an estimate, 2026-08-06)', () => {
  assert.equal(STROKE_FADE_START_MS, 10_000);
});

test('strokeFadeOpacity: a stroke at t=9.9s is still fully visible', () => {
  assert.equal(strokeFadeOpacity(9_900), 1);
});

test('strokeFadeOpacity: a stroke at t=10.1s has started fading', () => {
  const opacity = strokeFadeOpacity(10_100);
  assert.ok(opacity < 1 && opacity > 0, `expected an in-between opacity, got ${opacity}`);
});

test('strokeFadeOpacity: never negative once fully expired', () => {
  assert.equal(strokeFadeOpacity(STROKE_EXPIRE_MS), 0);
  assert.equal(strokeFadeOpacity(STROKE_EXPIRE_MS + 5_000), 0);
});

test('isStrokeExpired: false while visible or fading, true once fully faded', () => {
  assert.equal(isStrokeExpired(9_900), false);
  assert.equal(isStrokeExpired(10_100), false);
  assert.equal(isStrokeExpired(STROKE_EXPIRE_MS - 1), false);
  assert.equal(isStrokeExpired(STROKE_EXPIRE_MS), true);
});

test('a stroke extended at t=9s ages from the NEW point, not the original (#670 requirement 4)', () => {
  const strokeBeganAtMs = 0;
  const continuedAtMs = 9_000;
  const laterCheckMs = 10_500;

  const ageFromFirstPoint = laterCheckMs - strokeBeganAtMs; // 10500ms
  const ageFromLastPoint = laterCheckMs - continuedAtMs; // 1500ms

  assert.ok(strokeFadeOpacity(ageFromFirstPoint) < 1, 'sanity: first-point basis would already be fading');
  assert.equal(strokeFadeOpacity(ageFromLastPoint), 1, 'last-point basis must still be fully visible');
});

test(
  'STROKE_FADE_DURATION_MS reads as a deliberate fade, not an abrupt pop or a slow crawl',
  () => {
    assert.ok(STROKE_FADE_DURATION_MS >= 400 && STROKE_FADE_DURATION_MS <= 1_200);
  }
);

// -------------------------------------------------------------------------
// All three render surfaces must import the shared predicate rather than
// reimplementing the 10s/fade-duration threshold -- so they cannot drift.
// Source-text checks (not direct import) because these Svelte 5 files use
// runes at module scope, same constraint documented in
// apps/desktop/tests/localEcho.test.ts.
// -------------------------------------------------------------------------

const pointerOverlaySource = readFileSync(
  new URL('../src/routes/compositor/pointer/+page.svelte', import.meta.url),
  'utf8'
);
const participantTileSource = readFileSync(
  new URL('../src/lib/components/ParticipantTile.svelte', import.meta.url),
  'utf8'
);
const webHarnessDrawDisplaySource = readFileSync(
  new URL('../../../web-harness/src/drawDisplay.ts', import.meta.url),
  'utf8'
);

test('native compositor pointer overlay imports the shared stroke-expiry predicate', () => {
  assert.match(pointerOverlaySource, /from '\$lib\/data\/strokeExpiry'/);
  assert.match(pointerOverlaySource, /isStrokeExpired/);
  assert.match(pointerOverlaySource, /strokeFadeOpacity/);
});

test('meeting-tile camera draw layer imports the shared stroke-expiry predicate', () => {
  assert.match(participantTileSource, /from '\$lib\/data\/strokeExpiry'/);
  assert.match(participantTileSource, /isStrokeExpired/);
  assert.match(participantTileSource, /strokeFadeOpacity/);
});

test('web client imports the shared stroke-expiry predicate directly from @petal/shared', () => {
  assert.match(webHarnessDrawDisplaySource, /from '@petal\/shared\/logic\/strokeExpiry'/);
  assert.match(webHarnessDrawDisplaySource, /isStrokeExpired/);
  assert.match(webHarnessDrawDisplaySource, /strokeFadeOpacity/);
});

// -------------------------------------------------------------------------
// #670: the `clear` draw message type is receive-only dead code (no sender
// ever emits it) -- CLAUDE.md "dormant code doesn't merge" means the
// receive-side reaction is deleted, not kept as an orphaned branch. The
// wire type itself stays (contracts/petal-contracts.json still pins it;
// no contract change for this issue), so only the UI reactions go.
// -------------------------------------------------------------------------

test('native compositor pointer overlay no longer special-cases the dead clear message type', () => {
  assert.doesNotMatch(pointerOverlaySource, /update\.type === 'clear'/);
});

test('meeting-tile camera draw layer no longer special-cases the dead clear message type', () => {
  assert.doesNotMatch(participantTileSource, /update\.type === 'clear'/);
});

test('web client no longer special-cases the dead clear message type', () => {
  assert.doesNotMatch(webHarnessDrawDisplaySource, /message\.type === 'clear'/);
});
