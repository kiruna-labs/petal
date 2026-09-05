import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const style = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

test('web create/join placeholder drops optional copy and uses the larger fitted size', () => {
  assert.match(html, /placeholder="Enter meeting name or Petal invite"/);
  assert.doesNotMatch(html, /Enter meeting name or Petal invite \(optional\)/);
  assert.match(style, /\.meeting-field input\[type='text'\]\s*{[\s\S]*font-size:\s*14px;/);
});

test('web create/join header is about twenty percent shorter', () => {
  assert.match(style, /\.hero-quiet\s*{[\s\S]*height:\s*122px;/);
});
