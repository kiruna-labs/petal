import { readFileSync } from 'node:fs';
import test from 'node:test';
import assert from 'node:assert/strict';

const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const controlsSource = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');

test('web room-name inputs disable mobile Safari autocapitalization', () => {
  assert.match(
    indexSource,
    /id="meeting-code"[\s\S]*?autocomplete="off"[\s\S]*?autocapitalize="off"/
  );
  assert.match(controlsSource, /input\.autocapitalize\s*=\s*'off'/);
});
