import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const roomRow = readFileSync(
  new URL('../src/lib/components/RoomRow.svelte', import.meta.url),
  'utf8'
);
const mainMenu = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');

test('main menu no longer shows a "created X ago" subtitle on room rows', () => {
  assert.doesNotMatch(mainMenu, /roomSubtitlesByName/);
  assert.doesNotMatch(mainRoute, /roomSubtitlesByName/);
  assert.doesNotMatch(mainRoute, /duplicateRoomSubtitle/);
  assert.doesNotMatch(mainRoute, /formatRoomAge/);
  assert.doesNotMatch(mainRoute, /created \$\{/);
});

test('remove and copy room affordances are hover/focus-only, matching the favorite star (#124)', () => {
  assert.doesNotMatch(roomRow, /\.remove-button\s*{[^}]*opacity:\s*0\.58/);
  assert.doesNotMatch(roomRow, /copy-button|copiedInvite|copyInvite|copyTimer/);
  assert.match(
    roomRow,
    /\.room-row-shell:hover \.remove-button,\s*\.room-row-shell:has\(:focus-visible\) \.remove-button\s*{[\s\S]*opacity:\s*1;/
  );
});

test('current room dot glow derives from live tokens', () => {
  assert.match(roomRow, /background: var\(--live\);/);
  assert.match(roomRow, /0 0 18px var\(--live-tint\);/);
  assert.doesNotMatch(roomRow, /rgba\(31,\s*209,\s*128/);
});
