import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const mainMenu = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
const layout = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const tauriConfig = JSON.parse(
  readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8')
);

test('Windows main window remains frameless without an extra Close control', () => {
  const mainWindow = tauriConfig.app.windows[0];
  assert.equal(mainWindow.decorations, false);
  assert.equal(mainWindow.transparent, true);

  assert.doesNotMatch(layout, /windows-close-button/);
  assert.doesNotMatch(layout, /aria-label="Close Petal"/);
  assert.doesNotMatch(layout, /closeWindowsMainWindow/);
});

test('the profile menu remains the clean quit path', () => {
  assert.match(mainMenu, /MenuItem label="Quit Petal"[\s\S]*onclick=\{quitFromProfile\}/);
  assert.match(mainRoute, /await invoke\(COMMANDS\.quitApp\)/);
  assert.match(mainRoute, /onQuit=\{handleQuit\}/);
});
