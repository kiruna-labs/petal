// The meeting window and Petal View take the first click while Petal is
// inactive (macOS `acceptFirstMouse`). Sharing hands focus back to the source
// app, so without it the first click only activates Petal.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

test('the meeting window accepts the first mouse', () => {
  const conf = JSON.parse(read('../src-tauri/tauri.conf.json'));
  const main = conf.app.windows.find((w: { url?: string }) => w.url === 'main.html');
  assert.ok(main, 'tauri.conf.json declares the main.html window');
  assert.equal(main.acceptFirstMouse, true);
});

test('Petal View accepts the first mouse', () => {
  const src = read('../src-tauri/src/region_window.rs').replace(/\/\/.*$/gm, '');
  const start = src.indexOf('WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(route');
  const end = src.indexOf('builder.build()', start);
  assert.ok(start > -1 && end > start, 'open_region_window builds Petal View with one builder chain');
  assert.match(src.slice(start, end), /\.accept_first_mouse\(true\)/);
});
