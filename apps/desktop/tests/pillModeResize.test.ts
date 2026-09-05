import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const meetingChromeSource = readFileSync(
  fileURLToPath(new URL('../src/lib/components/MeetingChrome.svelte', import.meta.url)),
  'utf8'
);
const pillWindowSource = readFileSync(
  fileURLToPath(new URL('../src/lib/meeting/pillWindow.svelte.ts', import.meta.url)),
  'utf8'
);

test('compact pill mode does not render MeetingChrome resize hit zones', () => {
  assert.doesNotMatch(meetingChromeSource, /class="resize-zones"/);
  assert.doesNotMatch(meetingChromeSource, /class="resize-zone/);
  assert.doesNotMatch(meetingChromeSource, /cursor:\s*(?:ns|ew|nesw|nwse)-resize/);
  assert.doesNotMatch(meetingChromeSource, /startResizeDragging/);
});

test('native window resizability is disabled only while in pill mode', () => {
  assert.match(pillWindowSource, /async function setCurrentWindowResizable/);
  assert.match(pillWindowSource, /setResizable\?: \(value: boolean\) => Promise<void>/);

  const enterGalleryWindow = pillWindowSource.match(
    /async function enterGalleryWindow[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(enterGalleryWindow, 'enterGalleryWindow should exist');
  assert.match(enterGalleryWindow, /await setCurrentWindowResizable\(win, true\)/);

  const enterPillWindow = pillWindowSource.match(/async function enterPillWindow[\s\S]*?\n  \}/)?.[0];
  assert.ok(enterPillWindow, 'enterPillWindow should exist');
  assert.match(enterPillWindow, /await setCurrentWindowResizable\(win, false\)/);

  const restoreHomeWindow = pillWindowSource.match(
    /async function restoreHomeWindow[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(restoreHomeWindow, 'restoreHomeWindow should exist');
  assert.match(restoreHomeWindow, /await setCurrentWindowResizable\(win, true\)/);
});
