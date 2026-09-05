import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const participantTile = readFileSync(
  new URL('../src/lib/components/ParticipantTile.svelte', import.meta.url),
  'utf8'
);

const gallery = readFileSync(new URL('../src/lib/components/Gallery.svelte', import.meta.url), 'utf8');

test('desktop camera-off tile documents the approved centered-name deviation', () => {
  assert.match(participantTile, /intentionally deviates from the older canvas note/);
  assert.match(participantTile, /user approved the centered-name treatment in #137/);
  assert.match(
    participantTile,
    /<span class="camera-off-name" class:active=\{showCameraOffName\} aria-label=\{name\} title=\{name\}>\{centeredNameLabel\}<\/span>/
  );
});

test('desktop camera-off tile hides the bottom-left chip while video is off', () => {
  assert.match(participantTile, /\{#if videoOn\}\s*<div class="name-chip"/);
  assert.match(participantTile, /\.camera-off-name\s*{[\s\S]*left:\s*50%;[\s\S]*top:\s*50%;/);
  assert.match(participantTile, /\.camera-off-name\s*{[\s\S]*transform:\s*translate\(-50%, -50%\);/);
});

test('desktop participant tile keeps video and placeholder layers mounted for crossfade', () => {
  assert.doesNotMatch(participantTile, /\{#if videoOn && videoStream\}\s*<!-- Real video/);
  assert.match(participantTile, /<div class="video-fill" class:active=\{showVideoFill\}/);
  assert.match(participantTile, /<div class="off-fill" class:active=\{showCameraOffFill\}/);
  assert.match(participantTile, /<video[\s\S]*class:ready=\{videoReady\}[\s\S]*onloadeddata=\{\(\) => markVideoFrameReady\(\)\}/);
  assert.match(participantTile, /attachVideoStream\(videoEl, visibleVideoStream\)/);
});

test('desktop participant tile fades real video in only after decoded-frame readiness', () => {
  assert.match(participantTile, /requestVideoFrameCallback\?\.\(markReady\)/);
  assert.match(participantTile, /video\.addEventListener\('loadeddata', markReady, \{ once: true \}\)/);
  assert.match(participantTile, /const videoReady = \$derived\(videoOn && hasVisibleVideoStream && videoFrameReady\)/);
  assert.match(participantTile, /\.video-el\s*{[\s\S]*opacity:\s*0;[\s\S]*transition:\s*opacity var\(--motion-base\)/);
  assert.match(participantTile, /\.video-el\.ready\s*{[\s\S]*opacity:\s*1;/);
});

test('desktop participant tile debounces name measurement during animated resize', () => {
  assert.match(participantTile, /function scheduleMeasuredLabels\(\)/);
  assert.match(participantTile, /new ResizeObserver\(scheduleMeasuredLabels\)/);
  assert.match(participantTile, /cancelAnimationFrame\(measureFrame\)/);
});

// #676: the fixed `700 26px` camera-off-name font never scaled down for a
// ~100px-tall spotlight rail thumbnail. The fit itself is fixed in
// Gallery.svelte (the wrapper Gallery owns), not here.
test('camera-off name stays a fixed base size -- scaling happens in the spotlight-thumb wrapper', () => {
  assert.match(participantTile, /\.camera-off-name\s*{[\s\S]*font:\s*700 26px var\(--font-display\);/);
});

test('spotlight rail thumbnail scales the camera-off name relative to the real rendered thumb size, not another fixed px', () => {
  assert.match(
    gallery,
    /\.tile-wrap\.spotlight-thumb\s*:global\(\.camera-off-name\)\s*{\s*font-size:\s*clamp\([^)]*cqh[^)]*\);/
  );
  // The clamp must actually be capable of going below the unscaled 26px --
  // a clamp whose max still allows 26px+ would not be a fix.
  assert.doesNotMatch(
    gallery,
    /\.tile-wrap\.spotlight-thumb\s*:global\(\.camera-off-name\)\s*{\s*font-size:\s*(?:2[6-9]|[3-9]\d)px;/
  );
});

test('spotlight rail thumbnail establishes a query container so the font-size clamp above has something to scale against', () => {
  assert.match(gallery, /\.tile-wrap\.spotlight-thumb\s*{[\s\S]*container-type:\s*size;/);
});

test('spotlight rail thumbnail name-chip and muted-chip reuse the compact tier, not another one-off size', () => {
  assert.match(
    gallery,
    /\.tiles\.grid\.compact \.tile-wrap :global\(\.name-chip\),\s*\n\s*\.tile-wrap\.spotlight-thumb :global\(\.name-chip\) {/
  );
  assert.match(
    gallery,
    /\.tiles\.grid\.compact \.tile-wrap :global\(\.muted-chip\),\s*\n\s*\.tile-wrap\.spotlight-thumb :global\(\.muted-chip\) {/
  );
});

// #676 regression guard: the exact stale override that caused the overlap
// (0738c91f left `grid-auto-columns: minmax(118px, 150px)` on `.spotlight-rail`
// inside the `@media (max-width: 620px)` block after the base rule moved to
// an aspect-ratio-driven width) must never come back. A regex test alone is
// not sufficient evidence -- see scripts/verify-spotlight-rail.mjs for the
// real rendered-pixel check -- but it is a cheap, fast extra guard.
test('spotlight rail has no grid-auto-columns override left over from the pre-0738c91f grid layout', () => {
  assert.doesNotMatch(gallery, /\.spotlight-rail\s*{\s*grid-auto-columns/);
  assert.doesNotMatch(gallery, /grid-auto-columns:\s*minmax\(118px,\s*150px\)/);
});

test('spotlight rail and thumbnails use flex, not grid, so tracks cannot over-stretch at wide window widths', () => {
  assert.match(gallery, /\.spotlight-rail\s*{[\s\S]*display:\s*flex;/);
  assert.match(gallery, /\.tile-wrap\.spotlight-thumb\s*{[\s\S]*flex:\s*0 0 auto;/);
});
