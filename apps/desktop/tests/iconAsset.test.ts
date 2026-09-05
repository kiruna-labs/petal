import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const source = readFileSync(new URL('../src-tauri/icons/icon-source.svg', import.meta.url), 'utf8');

test('desktop app icon keeps the approved geometry and flat two-color treatment', () => {
  assert.match(source, /<svg width="1024" height="1024" viewBox="0 0 1024 1024"/);
  // Apple's app-icon grid: the squircle is 824x824 CENTERED in the 1024
  // canvas with ~100px transparent margin. A full-bleed rect renders
  // visibly LARGER than every neighboring Dock icon (live report,
  // 2026-08-20, shipped to users via the 0.8.8 auto-update).
  assert.match(source, /<rect x="100" y="100" width="824" height="824" rx="185" ry="185" fill="#303238"\/>/);
  assert.match(source, /<g transform="translate\(189\.3,180\.5\) scale\(0\.6899\)" fill="#F5F6F7"/);
  assert.doesNotMatch(source, /linearGradient|radialGradient|stroke-opacity|<rect[^>]+stroke=/);
});

test('the icon squircle never extends to the canvas edge (the "giant icon" regression class)', () => {
  // Whatever the exact geometry becomes, the background rect must keep a
  // real transparent margin on every side: x/y >= 60 and x+width/y+height
  // <= 964 (Apple's grid uses 100/824; allow modest redesign slack).
  const m = source.match(/<rect x="(\d+(?:\.\d+)?)" y="(\d+(?:\.\d+)?)" width="(\d+(?:\.\d+)?)" height="(\d+(?:\.\d+)?)"/);
  assert.ok(m, 'icon-source.svg must contain the background rect');
  const [x, y, w, h] = m.slice(1).map(Number);
  assert.ok(x >= 60 && y >= 60, `background rect must keep a transparent margin (got x=${x}, y=${y})`);
  assert.ok(x + w <= 964 && y + h <= 964, `background rect must not approach the canvas edge (got ${x + w}x${y + h})`);
});
