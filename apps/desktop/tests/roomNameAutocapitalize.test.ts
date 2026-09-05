import { readFileSync } from 'node:fs';
import test from 'node:test';
import assert from 'node:assert/strict';

const mainMenuSource = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);
const gallerySource = readFileSync(
  new URL('../src/lib/components/Gallery.svelte', import.meta.url),
  'utf8'
);

test('desktop room-name inputs disable browser autocapitalization', () => {
  assert.match(
    mainMenuSource,
    /id="meeting-code"[\s\S]*?name="meeting-code"[\s\S]*?autocapitalize="off"/
  );
  assert.match(
    gallerySource,
    /class="room-name-input"[\s\S]*?autocapitalize="off"[\s\S]*?bind:value=\{roomNameDraft\}/
  );
});
