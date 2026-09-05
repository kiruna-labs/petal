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
