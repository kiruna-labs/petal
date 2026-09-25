import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const mainMenu = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);

test('desktop create/join placeholder drops optional copy and stays fitted at larger size', () => {
  assert.match(mainMenu, /placeholder="Enter meeting name or Petal invite"/);
  assert.doesNotMatch(mainMenu, /Enter meeting name or Petal invite \(optional\)/);
  assert.match(mainMenu, /14px: the shortened placeholder/);
  assert.match(mainMenu, /~189px in Albert Sans\) fits the ~238px input area/);
  assert.match(mainMenu, /\.join-input\s*{[\s\S]*font-size:\s*14px;/);
});

test('desktop create/join header is about twenty percent shorter', () => {
  assert.match(mainMenu, /\.hero-quiet\s*{[\s\S]*height:\s*122px;/);
});

// #243: the maintainer asked for the eyebrow to go from every version.
test('desktop quiet hero drops the "Ready to collaborate?" eyebrow', () => {
  assert.doesNotMatch(mainMenu, /Ready to collaborate/);
  assert.doesNotMatch(mainMenu, /quiet-eyebrow|quiet-ring/);
  assert.match(
    mainMenu,
    /<section class="hero-quiet">\s*<div class="quiet-bloom" aria-hidden="true"><\/div>\s*<span class="quiet-title">Start a meeting<\/span>\s*<\/section>/
  );
  // Still painted above the absolutely positioned bloom.
  assert.match(mainMenu, /\.quiet-title\s*\{\s*position:\s*relative;/);
});
