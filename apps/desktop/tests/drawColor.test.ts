import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const draw = readFileSync(new URL('../src-tauri/src/draw.rs', import.meta.url), 'utf8');
const control = readFileSync(
  new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
  'utf8'
);

test('native draw local echo uses the RoomConnection palette getter', () => {
  assert.match(draw, /room_connection\.identity_palette_index\(\)/);
  assert.doesNotMatch(
    draw,
    /update_for_authenticated_sender\(message, drawer_identity, None, None\)/
  );
});

test('desktop draw cursor uses the selected session palette color', () => {
  assert.match(control, /penCursor\(identityColorCss\(session\.identity\)\)/);
  assert.doesNotMatch(control, /penCursor\(identityColorCss\(colorForIdentity\(drawerIdentity\)\)\)/);
});
