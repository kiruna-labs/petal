import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (relativePath: string) =>
  readFileSync(new URL(`../${relativePath}`, import.meta.url), 'utf8');

const desktopPopupSources = [
  'src/lib/components/MeetingChrome.svelte',
  'src/lib/components/Gallery.svelte',
  'src/lib/components/MainMenu.svelte',
  'src/lib/components/DeviceSelect.svelte',
  'src/lib/components/ContextMenu.svelte',
  'src/routes/menubar-popover/+page.svelte'
];

for (const relativePath of desktopPopupSources) {
  test(`${relativePath} uses the shared non-modal dismissal seam`, () => {
    const source = read(relativePath);
    assert.match(source, /installDismissibleLayer/);
    assert.doesNotMatch(source, /(?:more|devices|gallery-more|profile)-backdrop/);
    assert.doesNotMatch(source, /class="backdrop"/);
    assert.doesNotMatch(source, /document\.addEventListener\(['"](?:pointerdown|mousedown)/);
  });
}

test('desktop modal backdrop remains the intentional modal exception', () => {
  const source = read('src/lib/components/Modal.svelte');
  assert.match(source, /class="modal-backdrop"/);
  assert.match(source, /role="presentation"/);
});

test('browser non-modal popup sources use the shared dismissal seam', () => {
  for (const relativePath of [
    'src/deviceMenu.ts',
    'src/homeScreen.ts',
    'src/remoteWindowHeader.ts'
  ]) {
    const source = readFileSync(new URL(`../../../web-harness/${relativePath}`, import.meta.url), 'utf8');
    assert.match(source, /installDismissibleLayer/, relativePath);
    assert.doesNotMatch(source, /(?:moreBackdrop|devicesBackdrop|more-backdrop|devices-backdrop)/, relativePath);
    assert.doesNotMatch(source, /document\.addEventListener\(['"]pointerdown/, relativePath);
  }
  // controls.ts hosts no dismissible popup since #893 removed the More menu;
  // it must still never roll its own dismissal wiring.
  const controls = readFileSync(new URL('../../../web-harness/src/controls.ts', import.meta.url), 'utf8');
  assert.doesNotMatch(controls, /(?:moreBackdrop|devicesBackdrop|more-backdrop|devices-backdrop)/, 'src/controls.ts');
  assert.doesNotMatch(controls, /document\.addEventListener\(['"]pointerdown/, 'src/controls.ts');
});

test('browser popup markup has no non-modal click-catcher and keeps modal backdrop', () => {
  const index = readFileSync(new URL('../../../web-harness/index.html', import.meta.url), 'utf8');
  const style = readFileSync(new URL('../../../web-harness/src/style.css', import.meta.url), 'utf8');
  assert.doesNotMatch(index, /(?:more-backdrop|devices-backdrop)/);
  assert.doesNotMatch(style, /\.(?:meeting-menu-backdrop|devices-backdrop)\b/);
  assert.match(style, /\.feedback-dialog::backdrop/);
});
