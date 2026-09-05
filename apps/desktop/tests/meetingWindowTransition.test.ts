import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const pillWindowSource = readFileSync(
  fileURLToPath(new URL('../src/lib/meeting/pillWindow.svelte.ts', import.meta.url)),
  'utf8'
);

test('meeting gallery and home transitions resize directly without native animation', () => {
  assert.doesNotMatch(pillWindowSource, /animateMainWindowResize/);
  assert.doesNotMatch(pillWindowSource, /animate_main_window_resize/);
  // The file DOES import @tauri-apps/api/core — for the Windows native
  // corner-radius toggle (set_main_pill_mode), not for resizing. The
  // no-invoke guarantee that matters is per-helper below: the resize
  // helpers themselves must only ever call win.setSize directly.

  const directGalleryResize = pillWindowSource.match(
    /async function setCurrentWindowSize[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(directGalleryResize, 'setCurrentWindowSize helper should exist');
  assert.match(directGalleryResize, /win\.setSize\(new LogicalSize\(target\.width, target\.height\)\)/);
  assert.doesNotMatch(directGalleryResize, /invoke|COMMANDS|animate/i);

  const directPillResize = pillWindowSource.match(
    /async function resizePillHostWindow[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(directPillResize, 'resizePillHostWindow helper should exist');
  assert.match(directPillResize, /win\.setSize\(new LogicalSize\(target\.width, target\.height\)\)/);
  assert.doesNotMatch(directPillResize, /invoke|COMMANDS|animate/i);

  const enterGalleryWindow = pillWindowSource.match(
    /async function enterGalleryWindow[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(enterGalleryWindow, 'enterGalleryWindow should exist');
  // The gallery geometry chain moved to the shared applyMeetingWindowGeometry
  // helper (used by both enterGalleryWindow and prepareMeetingWindow);
  // enterGalleryWindow applies it directly, never via a native animation.
  assert.match(
    enterGalleryWindow,
    /remembered = await applyMeetingWindowGeometry\(win, first \? null : remembered\)/
  );

  const restoreHomeWindow = pillWindowSource.match(
    /async function restoreHomeWindow[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(restoreHomeWindow, 'restoreHomeWindow should exist');
  assert.match(restoreHomeWindow, /await setCurrentWindowSize\(win, target\)/);
});

test('meeting gallery restores full frames and centers only when no frame exists', () => {
  assert.match(pillWindowSource, /loadMeetingWindowFrame/);
  assert.match(pillWindowSource, /centerCurrentWindowOnMonitor/);

  const applyMeetingGeometry = pillWindowSource.match(
    /async function applyMeetingWindowGeometry[\s\S]*?\n\}/
  )?.[0];
  assert.ok(applyMeetingGeometry, 'applyMeetingWindowGeometry should exist');
  assert.match(applyMeetingGeometry, /const savedFrame = loadMeetingWindowFrame\(\)/);
  assert.match(applyMeetingGeometry, /if \(savedFrame\)/);
  assert.match(applyMeetingGeometry, /await safePositionForLogicalFrame\(win, savedFrame, target\)/);
  assert.match(applyMeetingGeometry, /await centerCurrentWindowOnMonitor\(win, saved\)/);
  assert.match(applyMeetingGeometry, /await centerCurrentWindowOnMonitor\(win, target\)/);
});

test('all window-geometry controllers share one programmatic resize guard', () => {
  const windowGeometrySource = readFileSync(
    fileURLToPath(new URL('../src/lib/data/windowGeometry.ts', import.meta.url)),
    'utf8'
  );
  const mainPageSource = readFileSync(
    fileURLToPath(new URL('../src/routes/main/+page.svelte', import.meta.url)),
    'utf8'
  );
  // The pre-navigation meeting pre-size runs while /main is still mounted.
  // If the menu route kept its own private guard, its onResized/onMoved
  // persistence would record the meeting geometry as the main-window frame
  // while the resize was in flight — the next leave then restores the home
  // window to the meeting size. Both controllers must bind the SAME guard.
  assert.match(
    windowGeometrySource,
    /export const programmaticResizeGuard = createProgrammaticGuard\(\)/
  );
  assert.match(pillWindowSource, /const programmatic = programmaticResizeGuard;/);
  assert.match(mainPageSource, /const programmatic = programmaticResizeGuard;/);
});

test('main page normalizes the window to the persisted home geometry at mount', () => {
  const mainPageSource = readFileSync(
    fileURLToPath(new URL('../src/routes/main/+page.svelte', import.meta.url)),
    'utf8'
  );
  // A /main mount can arrive with the window still at the meeting geometry
  // (pill-mode leaves restore after the swap; the meeting route's onDestroy
  // restore races the mount). The mount must shrink-or-grow back to the
  // persisted home frame so the meeting size is never re-persisted as the
  // main-window frame — the "keep whatever size you arrived with" behavior
  // was the leave-to-meeting-size bug.
  assert.match(mainPageSource, /loadMainWindowFrame\(\)/);
  assert.match(mainPageSource, /arrivingAtLaunchDefault/);
  assert.match(mainPageSource, /await resizeWindow\(home\)/);
  assert.match(mainPageSource, /safePositionForPhysicalFrame\(\s*\{ x: savedFrame\.x, y: savedFrame\.y \}/);
});

test('main page never persists the meeting geometry as the main-window frame', () => {
  const mainPageSource = readFileSync(
    fileURLToPath(new URL('../src/routes/main/+page.svelte', import.meta.url)),
    'utf8'
  );
  // The pre-navigation meeting pre-size resizes the window BEFORE /main
  // unmounts. The route's last-chance unmount save must not persist that
  // meeting-sized window as the main frame — the next leave would then
  // restore the meeting size onto /main. The setup's post-normalize save
  // is also gated on the route still being active (the async setup can
  // outlive the unmount and read the already-resized window).
  assert.match(mainPageSource, /saveCurrentMainFrameIfHome\(/);
  assert.match(mainPageSource, /void saveCurrentMainFrameIfHome\(getCurrentWindow\(\)\)/);
  assert.match(mainPageSource, /if \(routeActive\) await saveCurrentMainFrame\(win\)/);
});

test('restoreHomeWindow never clobbers the meeting frame with the home size', () => {
  // On a normal leave the pre-restore resizes the window to HOME before the
  // swap, so the meeting route's onDestroy safety-net restore runs with the
  // window already home-sized. Its remember step must not save that home
  // geometry as the MEETING frame — the next join would open the meeting at
  // the home size (clamped to the gallery minimum), which was the
  // "second join opens smaller, expanding only to the right" bug.
  assert.match(pillWindowSource, /const atHome =/);
  assert.match(pillWindowSource, /if \(!atHome\) \{\n\s*await rememberCurrentViewFrame\(win\);/);
});

test('pill mode restores saved pill frame without replacing first shrink fallback', () => {
  assert.match(pillWindowSource, /loadPillWindowFrame/);
  assert.match(pillWindowSource, /let rememberedPill: WindowFrame \| null = null/);

  const restoredPillPosition = pillWindowSource.match(
    /async function restoredPillPosition[\s\S]*?\n  \}/
  )?.[0];
  assert.ok(restoredPillPosition, 'restoredPillPosition should exist');
  assert.match(restoredPillPosition, /rememberedPill \?\? loadPillWindowFrame\(\)/);
  assert.match(restoredPillPosition, /if \(!saved\) return undefined/);

  const enterPillWindow = pillWindowSource.match(/async function enterPillWindow[\s\S]*?\n  \}/)?.[0];
  assert.ok(enterPillWindow, 'enterPillWindow should exist');
  assert.match(
    enterPillWindow,
    /resizePillHostWindow\(win, target, true, await restoredPillPosition\(win, target\)\)/
  );
});
