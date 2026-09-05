import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const tiles = readFileSync(new URL('../src/tiles.ts', import.meta.url), 'utf8');
const drawSender = readFileSync(new URL('../src/drawSender.ts', import.meta.url), 'utf8');
const index = readFileSync(new URL('../index.html', import.meta.url), 'utf8');

test('tile lifecycle hooks resync the meeting-bar Draw button', () => {
  assert.match(tiles, /const notifyDrawAvailability = \(\) => cb\.syncDrawAvailability\?\.\(\);/);
  const notifyCount = tiles.split('notifyDrawAvailability();').length - 1;
  assert.ok(notifyCount >= 6, `expected Draw resync at share/camera/clear hooks, found ${notifyCount}`);
  assert.match(tiles, /function addShareTile[\s\S]*notifyDrawAvailability\(\);/);
  assert.match(tiles, /function setTileCamera[\s\S]*notifyDrawAvailability\(\);/);
  assert.match(tiles, /function clearTileCamera[\s\S]*notifyDrawAvailability\(\);/);
  assert.match(tiles, /function finalizeShareTileRemoval[\s\S]*notifyDrawAvailability\(\);/);
  assert.match(tiles, /function clearTiles[\s\S]*notifyDrawAvailability\(\);/);
});

test('Draw starts disabled and refuses to enter draw mode without a target', () => {
  assert.match(index, /id="ctl-draw"[^>]*\sdisabled/);
  assert.match(drawSender, /if \(ctlDraw\.disabled\) return;/);
  assert.match(drawSender, /const next = on && hasDrawableTarget\(tilesEl\);/);
  assert.match(drawSender, /if \(drawMode && !hasDrawableTarget\(tilesEl\)\) \{\s*setDrawMode\(false\);/);
});
