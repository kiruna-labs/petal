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

// The layout lab's window shapes: a column of faces and a bar of faces stay
// the gallery; small in BOTH directions is the pill. The wiring, not just
// windowGeometry.ts's pure rule (ENGINEERING.md: native window lifecycle).
test('the gallery collapses to the pill only when small both ways, and never remembers the collapsing frame', () => {
  const handleSize = pillWindowSource.match(/function handleLogicalSize\(w: number, h: number\)[\s\S]*?\n  \}/)?.[0];
  assert.ok(handleSize, 'handleLogicalSize should exist');
  assert.match(handleSize, /if \(galleryCollapsesToPill\(\{ width: w, height: h \}\)\) expanded = false;/);
  assert.doesNotMatch(pillWindowSource, /handleLogicalWidth|GALLERY_BREAKPOINT - 1/);
  // Both resize paths feed width AND height.
  assert.match(pillWindowSource, /handleLogicalSize\(window\.innerWidth, window\.innerHeight\)/);
  assert.match(pillWindowSource, /handleLogicalSize\(logical\.width, logical\.height\)/);
  // A drag into the pill zone does not overwrite the remembered gallery
  // frame, on either path that saves it, so expanding restores the window
  // the person had.
  assert.match(pillWindowSource, /if \(expanded && !galleryCollapsesToPill\(logical\)\) \{/);
  const rememberGallery = pillWindowSource.match(/async function rememberGalleryFrame[\s\S]*?\n  \}/)?.[0];
  assert.ok(rememberGallery, 'rememberGalleryFrame should exist');
  assert.match(rememberGallery, /if \(remembered && galleryCollapsesToPill\(frame\)\) return;/);
  // Every gallery entry lowers the minimum to the gallery's own BEFORE sizing.
  const enterGallery = pillWindowSource.match(/async function enterGalleryWindow[\s\S]*?\n  \}/)?.[0];
  assert.ok(enterGallery, 'enterGalleryWindow should exist');
  assert.match(enterGallery, /setMinSize\(new LogicalSize\(GALLERY_MIN\.width, GALLERY_MIN\.height\)\);\s*remembered = await applyMeetingWindowGeometry/);
  const prepare = pillWindowSource.match(/export async function prepareMeetingWindow[\s\S]*?\n\}/)?.[0];
  assert.ok(prepare, 'prepareMeetingWindow should exist');
  assert.match(prepare, /setMinSize\(new LogicalSize\(GALLERY_MIN\.width, GALLERY_MIN\.height\)\);\s*const applied = await applyMeetingWindowGeometry/);
});
