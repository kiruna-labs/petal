import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  friendlyTelepointerName,
  isTechnicalIdentity
} from '../src/lib/data/telepointerDisplay.ts';

test('friendlyTelepointerName prefers LiveKit display name metadata', () => {
  assert.equal(
    friendlyTelepointerName('  Ada   Lovelace  ', '4f7b59e8-3d18-467c-985b-f8d477307b33'),
    'Ada Lovelace'
  );
});

test('friendlyTelepointerName never exposes UUID participant identities', () => {
  assert.equal(
    friendlyTelepointerName(null, '4f7b59e8-3d18-467c-985b-f8d477307b33'),
    'Guest'
  );
});

test('friendlyTelepointerName hides generated fallback participant identities', () => {
  assert.equal(friendlyTelepointerName('', 'p-m1h4z9-a8y3v2'), 'Guest');
});

test('friendlyTelepointerName can make readable non-technical ids friendly', () => {
  assert.equal(friendlyTelepointerName(null, 'web-tester'), 'Web Tester');
});

test('isTechnicalIdentity detects empty, UUID, generated, and long hex ids', () => {
  assert.equal(isTechnicalIdentity(''), true);
  assert.equal(isTechnicalIdentity('4f7b59e8-3d18-467c-985b-f8d477307b33'), true);
  assert.equal(isTechnicalIdentity('p-m1h4z9-a8y3v2'), true);
  assert.equal(isTechnicalIdentity('abcdef1234567890abcdef12'), true);
  assert.equal(isTechnicalIdentity('alice'), false);
});

test('native telepointer chrome has no permanent glyph halo or name-pill outline', () => {
  const pointer = readFileSync(new URL('../src/lib/components/Pointer.svelte', import.meta.url), 'utf8');
  const namePill = readFileSync(new URL('../src/lib/components/NamePill.svelte', import.meta.url), 'utf8');

  assert.doesNotMatch(pointer, /class="pointer-halo"|\.pointer-halo/);
  assert.doesNotMatch(namePill, /box-shadow\s*:/);
});

test('NamePill never uppercases the telepointer name — must render exactly as provided', () => {
  const namePill = readFileSync(new URL('../src/lib/components/NamePill.svelte', import.meta.url), 'utf8');

  // Regression guard: `.name-pill` previously had `text-transform: uppercase`,
  // which silently forced every remote participant's name to caps regardless
  // of how they entered/were given it. The approved mock
  // (the approved design canvas telepointer board) renders "Priya" /
  // "Chantelle" in mixed case, and web-harness's equivalent
  // .remote-telepointer__label already sets text-transform: none for the
  // same reason — keep both sides in lockstep.
  assert.doesNotMatch(namePill, /text-transform\s*:\s*uppercase/);
  assert.match(namePill, /text-transform\s*:\s*none/);
});
