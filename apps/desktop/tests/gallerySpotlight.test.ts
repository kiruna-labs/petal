// #785: the gallery's spotlight fallback used to read
// `sharing -> activeSpeaker -> LOCAL -> first`, so when a sharer stopped, the
// hero fell through to the user's own tile and they sat in spotlight staring
// at their own webcam. The ranking now lives in shared/logic/tileLayoutMode.ts
// (one source with the web client).
//
// Two halves, deliberately: the ranking itself, and the WIRING — a correct
// helper proves nothing if Gallery.svelte still hand-rolls the old chain, and
// this suite cannot mount a Svelte component (node --test + tsx, no DOM), so
// the wiring half asserts against the component source the way
// cameraOffCenteredName.test.ts and remoteWindowHeader.test.ts do.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  autoSpotlight,
  chooseSpotlightHero,
  dismissSpotlight,
  endAutoSpotlight,
  initialTileLayoutModeState,
  manualTileLayoutMode
} from '@petal/shared/logic/tileLayoutMode';

const gallery = readFileSync(new URL('../src/lib/components/Gallery.svelte', import.meta.url), 'utf8');

test('a remote participant outranks the local self-view, even one with video', () => {
  const hero = chooseSpotlightHero([
    { key: 'me', isLocal: true, hasVideo: true, isActiveSpeaker: true },
    { key: 'remote', hasVideo: false }
  ]);
  assert.equal(hero?.key, 'remote');
});

test('the self-view is still chosen when it is the only candidate', () => {
  const hero = chooseSpotlightHero([{ key: 'me', isLocal: true, hasVideo: true }]);
  assert.equal(hero?.key, 'me');
  assert.equal(chooseSpotlightHero([]), null);
});

test('a remote sharer, then a remote speaker, then remote video wins', () => {
  const candidates = [
    { key: 'quiet' },
    { key: 'video', hasVideo: true },
    { key: 'speaker', isActiveSpeaker: true, hasVideo: true },
    { key: 'sharer', isSharing: true }
  ];
  assert.equal(chooseSpotlightHero(candidates)?.key, 'sharer');
  assert.equal(chooseSpotlightHero(candidates.slice(0, 3))?.key, 'speaker');
  assert.equal(chooseSpotlightHero(candidates.slice(0, 2))?.key, 'video');
  assert.equal(chooseSpotlightHero(candidates.slice(0, 1))?.key, 'quiet');
});

test('my own tile does not get promoted just because I am the one sharing', () => {
  // Native shares are compositor NSWindows, never gallery tiles: `sharing` on
  // the local entry means "my webcam tile, while I share" — still self-view.
  const hero = chooseSpotlightHero([
    { key: 'me', isLocal: true, isSharing: true, hasVideo: true },
    { key: 'remote', hasVideo: true }
  ]);
  assert.equal(hero?.key, 'remote');
});

test('ties keep caller order, so an all-equal roster still spotlights its first entry', () => {
  const hero = chooseSpotlightHero([{ key: 'first' }, { key: 'second' }]);
  assert.equal(hero?.key, 'first');
});

test('Gallery.svelte routes its spotlight fallback through the shared ranking', () => {
  assert.match(gallery, /import \{ chooseSpotlightHero \} from '@petal\/shared\/logic\/tileLayoutMode'/);
  assert.match(gallery, /manualPinnedKey \?\? chooseSpotlightHero\(spotlightCandidates\)\?\.key/);
  // The old chain ended in `localEntry?.key ?? participantEntries[0]?.key`.
  // If it ever comes back, the ranking above is decorative.
  assert.doesNotMatch(gallery, /localEntry\?\.key/);
  assert.doesNotMatch(gallery, /spotlightKey = \$derived\([^)]*sharingEntry/s);
});

test('Gallery renders one keyed participant tree for grid and spotlight motion', () => {
  assert.match(gallery, /function transitionGalleryLayout\(mutate: \(\) => void\)/);
  assert.match(gallery, /\{#each tileEntries as p, index \(p\.key\)\}/);   // one keyed tree; tileEntries reorders participantEntries for spotlight (#918)
  assert.match(gallery, /data-participant-key=\{p\.key\}/);
  assert.match(gallery, /class:spotlight-main=\{spotlightActive && p\.key === spotlightEntry\?\.key\}/);
  assert.match(gallery, /class:spotlight-thumb=\{spotlightActive && p\.key !== spotlightEntry\?\.key\}/);
  assert.match(gallery, /tile\.animate\(/);
  assert.match(gallery, /duration, easing: 'cubic-bezier\(0\.2, 0, 0, 1\)', fill: 'none'/);
  assert.match(gallery, /animate:flip=\{\{ duration: suppressSvelteFlip \? 0 : tileLayoutDuration\(\) \}\}/);
  assert.doesNotMatch(gallery, /\{#key spotlightEntry\.key\}/);
  assert.equal((gallery.match(/<ParticipantTile\b/g) ?? []).length, 1);
});

test('an automatic spotlight records what to restore and never persists', () => {
  const auto = autoSpotlight(initialTileLayoutModeState('grid'));
  assert.deepEqual(auto, { state: { mode: 'spotlight', restoreMode: 'grid' }, persist: null });

  // Already spotlighted: nothing to record, nothing to restore later.
  const again = autoSpotlight(auto.state);
  assert.deepEqual(again.state, auto.state);
  assert.equal(again.persist, null);

  const restored = endAutoSpotlight(auto.state);
  assert.deepEqual(restored, { state: { mode: 'grid', restoreMode: null }, persist: null });
});

test('an explicit choice persists and discards a pending restore', () => {
  const auto = autoSpotlight(initialTileLayoutModeState('grid'));
  const manual = manualTileLayoutMode(auto.state, 'spotlight');
  assert.deepEqual(manual, { state: { mode: 'spotlight', restoreMode: null }, persist: 'spotlight' });
  // Nothing recorded any more, so the share ending leaves the user alone.
  assert.deepEqual(endAutoSpotlight(manual.state), { state: manual.state, persist: null });
});

test('endAutoSpotlight is a no-op when the user picked the mode themselves', () => {
  const chosen = manualTileLayoutMode(initialTileLayoutModeState('grid'), 'spotlight').state;
  assert.deepEqual(endAutoSpotlight(chosen), { state: chosen, persist: null });
});

test('dismissing an auto-spotlight returns to the previous mode; dismissing a chosen one is an explicit grid', () => {
  const auto = autoSpotlight(initialTileLayoutModeState('grid'));
  assert.deepEqual(dismissSpotlight(auto.state), {
    state: { mode: 'grid', restoreMode: null },
    persist: null
  });

  const chosen = manualTileLayoutMode(initialTileLayoutModeState('grid'), 'spotlight').state;
  assert.deepEqual(dismissSpotlight(chosen), {
    state: { mode: 'grid', restoreMode: null },
    persist: 'grid'
  });
});
