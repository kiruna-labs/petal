import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const mainMenuSource = readFileSync(resolve(__dirname, '../src/lib/components/MainMenu.svelte'), 'utf8');
const mainRouteSource = readFileSync(resolve(__dirname, '../src/routes/main/+page.svelte'), 'utf8');
const layoutSource = readFileSync(resolve(__dirname, '../src/routes/+layout.svelte'), 'utf8');
const tauriConfig = JSON.parse(
  readFileSync(resolve(__dirname, '../src-tauri/tauri.conf.json'), 'utf8')
);

test('main menu room refresh does not insert a layout-shifting loading row', () => {
  assert.doesNotMatch(mainMenuSource, />Loading rooms</);
  assert.doesNotMatch(mainMenuSource, /class="rooms-loading/);
  assert.doesNotMatch(mainMenuSource, /roomsLoading/);
  assert.doesNotMatch(mainMenuSource, /aria-busy=\{roomsLoading\}/);
  assert.doesNotMatch(mainRouteSource, /loadState = 'loading'/);
  assert.doesNotMatch(mainRouteSource, /roomsLoading=/);
});

test('desktop launch opens the final main menu route directly', () => {
  assert.equal(tauriConfig.app.windows[0].url, 'main.html');
  assert.match(layoutSource, /routePath = \$derived\.by/);
  assert.match(layoutSource, /p\.endsWith\('\.html'\)/);
  assert.match(mainRouteSource, /goto\('\/onboarding', \{ replaceState: true \}\)/);
});
