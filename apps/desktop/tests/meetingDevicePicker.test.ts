import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const desktopSource = (relative: string) =>
  readFileSync(fileURLToPath(new URL(relative, import.meta.url)), 'utf8');

const meetingChromeSource = desktopSource('../src/lib/components/MeetingChrome.svelte');
const gallerySource = desktopSource('../src/lib/components/Gallery.svelte');
const pickerSource = desktopSource('../src/lib/components/DevicePicker.svelte');
const splitSource = desktopSource('../src/lib/components/MediaSplitControl.svelte');
const menubarSource = desktopSource('../src/routes/menubar-popover/+page.svelte');
const sharedControlsSource = readFileSync(
  fileURLToPath(new URL('../../../shared/ui/meeting-controls.css', import.meta.url)),
  'utf8'
);

function extractArrayLiteral(name: string): string[] {
  const match = meetingChromeSource.match(new RegExp(`const ${name} = \\[([^\\]]+)\\]`));
  assert.ok(match, `${name} array should exist`);
  return Array.from(match[1].matchAll(/'([^']+)'/g), (m) => m[1]);
}

test('the compact pill has a fixed essential hierarchy and stable More overflow', () => {
  assert.deepEqual(extractArrayLiteral('DISPLAY_ORDER'), ['mic', 'camera', 'screenshare']);
  assert.deepEqual(extractArrayLiteral('PILL_MORE_ORDER'), ['invite', 'region', 'remotecontrol']);
  assert.match(meetingChromeSource, /icon="more"/);
  assert.match(meetingChromeSource, /overflow: \[\.\.\.PILL_MORE_ORDER\]/);
});

test('desktop media controls use an attached options segment, not an overlay caret', () => {
  assert.match(splitSource, /class="meeting-split"/);
  assert.match(splitSource, /class="meeting-split-options"/);
  assert.match(splitSource, /aria-haspopup="dialog"/);
  assert.match(splitSource, /aria-expanded=\{optionsOpen\}/);
  assert.doesNotMatch(meetingChromeSource, /DeviceCaret/);
  assert.doesNotMatch(gallerySource, /DeviceCaret/);
});

test('gallery exposes stable primary labels and specialist More actions', () => {
  for (const label of ['Mic', 'Camera']) {
    assert.match(gallerySource, new RegExp(`visibleLabel="${label}"`));
  }
  for (const label of ['Share', 'Invite', 'More', 'Leave']) {
    assert.match(gallerySource, new RegExp(`meeting-control-label[^>]*>${label}<`));
  }
  assert.match(gallerySource, /Petal View/);
  assert.match(gallerySource, /Remote control (?:on|off)/);
  assert.match(gallerySource, /role="menuitemcheckbox"/);
});

test('device picker is a one-level direct-row panel with selected checks and feedback', () => {
  assert.match(pickerSource, /mode: 'audio' \| 'camera';/);
  assert.doesNotMatch(pickerSource, /DeviceSelect/);
  assert.match(pickerSource, /class="meeting-menu-row device-row"/);
  assert.match(pickerSource, /role="option"/);
  assert.match(pickerSource, /meeting-menu-row-check/);
  assert.match(pickerSource, /aria-live="polite"/);
  assert.match(pickerSource, /ArrowDown/);
  assert.match(pickerSource, /Saved — applies when you join a room/);
  assert.match(pickerSource, /max-height: var\(--device-menu-max-height, none\)/);
  assert.match(pickerSource, /overflow-y: auto;/);
});

test('device menus reserve action-bar space and keep split controls quiet at rest', () => {
  assert.match(sharedControlsSource, /\.meeting-split\s*\{[\s\S]*background: var\(--fill-strong\);/);
  assert.match(sharedControlsSource, /\.meeting-split > \.control-button:not\(:disabled\)[\s\S]*background: transparent;/);
  assert.match(meetingChromeSource, /trigger\.closest<HTMLElement>\('\.controlbar'\)/);
  assert.match(meetingChromeSource, /style:--device-menu-max-height=\{deviceMenuMaxHeight\}/);
  assert.match(meetingChromeSource, /\.devices-menu\s*\{[\s\S]*z-index: 21;/);
});

test('menubar popover reuses split controls and the shared picker', () => {
  assert.match(menubarSource, /<MediaSplitControl/);
  assert.match(menubarSource, /<DevicePicker/);
  assert.match(menubarSource, /menubar-device-panel/);
  assert.match(menubarSource, /ResizeObserver/);
});
