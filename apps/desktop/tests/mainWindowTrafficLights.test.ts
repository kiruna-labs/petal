import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import test from 'node:test';

import { hideMainWindow, minimizeMainWindow } from '../src/lib/data/mainWindowControls.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (p: string) => readFileSync(resolve(__dirname, p), 'utf8');
// Assertions below are about CODE, not prose: these files deliberately explain
// the traps in comments, and a naive grep would match the explanation.
const stripComments = (src: string) =>
  src
    .split('\n')
    .filter((line) => !/^\s*(\/\/|\*|\/\*|<!--|-->)/.test(line))
    .join('\n');

const mainMenu = stripComments(read('../src/lib/components/MainMenu.svelte'));
const lib = stripComments(read('../src-tauri/src/lib.rs'));
const mainWindowRs = stripComments(read('../src-tauri/src/main_window.rs'));
const popover = stripComments(read('../src/routes/menubar-popover/+page.svelte'));
const deepLink = stripComments(read('../src-tauri/src/deep_link.rs'));
const capabilities = JSON.parse(read('../src-tauri/capabilities/default.json'));

// The expensive mistake this feature can make is red quitting the app instead of
// hiding the window. These two exercise the real module, not the markup.
test('red hides the window and does not minimize it', async () => {
  const calls: string[] = [];
  await hideMainWindow({
    hide: async () => {
      calls.push('hide');
    },
    minimize: async () => {
      calls.push('minimize');
    }
  });
  assert.deepEqual(calls, ['hide'], 'the red dot must call hide() exactly once and nothing else');
});

test('yellow minimizes the window and does not hide it', async () => {
  const calls: string[] = [];
  await minimizeMainWindow({
    hide: async () => {
      calls.push('hide');
    },
    minimize: async () => {
      calls.push('minimize');
    }
  });
  assert.deepEqual(calls, ['minimize'], 'the yellow dot must call minimize() exactly once');
});

test('a failing window call is swallowed, not thrown into the click handler', async () => {
  await assert.doesNotReject(
    () =>
      hideMainWindow({
        hide: async () => {
          throw new Error('denied');
        },
        minimize: async () => {}
      }),
    'an unhandled rejection in an onclick handler is a silent dead button'
  );
});

test('the main window traffic lights never quit the app or destroy the webview', () => {
  // close() destroys the webview and NOTHING can recreate label `main` -- there
  // is no WebviewWindowBuilder for it anywhere. quit_app leaves the room and
  // calls app.exit(0). Both are wrong for a window control.
  assert.doesNotMatch(
    mainMenu,
    /quitApp|quit_app/,
    'the traffic dots must never reach the quit command'
  );
  assert.doesNotMatch(
    mainMenu,
    /\.close\(\)/,
    'red must hide the window, never close it -- nothing can rebuild the main window'
  );
  assert.match(mainMenu, /hideMainWindow/, 'red must go through hideMainWindow');
  assert.match(mainMenu, /minimizeMainWindow/, 'yellow must go through minimizeMainWindow');
});

test('the dots are clickable: their own cluster, above the drag layer, before the brand', () => {
  const dotsAt = mainMenu.indexOf('class="window-controls"');
  const brandAt = mainMenu.indexOf('class="brand-cluster"');
  assert.ok(dotsAt > 0, 'the window-controls cluster must exist');
  assert.ok(brandAt > 0, 'the brand cluster must still exist');
  assert.ok(
    dotsAt < brandAt,
    '.brand-cluster is pointer-events: none -- dots inside or after it are unclickable'
  );
  assert.match(
    mainMenu,
    /\.window-dot\s*\{[^}]*pointer-events:\s*auto/,
    'the dots need pointer-events: auto to beat the absolute inset:0 topbar-drag-layer'
  );
  const dotButtons = mainMenu.match(/class="window-dot window-dot-\w+"/g) ?? [];
  assert.equal(dotButtons.length, 2, 'exactly two dots: hide (red) and minimize (yellow)');
  assert.equal(
    (mainMenu.match(/onmousedown=\{stopMouseDown\}/g) ?? []).length,
    2,
    'each dot needs onmousedown={stopMouseDown} or a native drag swallows its click'
  );
  assert.doesNotMatch(
    mainMenu,
    /traffic-close/,
    'do not revive the class name the codebase deliberately removed'
  );
});

test('the frontend is actually allowed to hide the window', () => {
  // capabilities/default.json granted allow-close and allow-minimize but not
  // allow-hide, so hide() was denied at runtime with no compile-time signal.
  assert.ok(
    capabilities.permissions.includes('core:window:allow-hide'),
    'without core:window:allow-hide the red dot is a no-op at runtime'
  );
  assert.ok(
    capabilities.permissions.includes('core:window:allow-minimize'),
    'yellow still needs allow-minimize'
  );
});

test('a Dock click can bring a hidden main window back', () => {
  // RunEvent::Reopen was discarded entirely: run() was a bare
  // .run(generate_context!()) with no handler, and tao's
  // applicationShouldHandleReopen returns has_visible_windows == NO while
  // hidden, which also suppresses AppKit's own unhide-and-front.
  assert.match(lib, /RunEvent::Reopen/, 'the Dock reopen event must be handled');
  assert.match(
    lib,
    /show_and_activate_main_window\(app_handle, "dock-reopen"\)/,
    'reopen must show and activate the main window'
  );
});

test('reopen does not go through the one-shot reveal gate', () => {
  // reveal_main_window is guarded by MAIN_WINDOW_REVEALED.swap(true), so it
  // fires exactly ONCE per process: wiring reopen to it makes the first reopen
  // pass and every later one silently do nothing.
  assert.doesNotMatch(
    lib,
    /Reopen[\s\S]{0,300}reveal_main_window/,
    'reopen must call show_and_activate_main_window, never the one-shot reveal'
  );
  assert.match(
    lib,
    /pub\(crate\) fn main_window_revealed\(\)[\s\S]{0,200}\.load\(/,
    'main_window_revealed must READ the flag, never swap it'
  );
});

test('the menubar popover offers a way back to the main window', () => {
  assert.match(popover, /label="Open Petal"/, 'the popover needs an explicit Open Petal row');
  assert.match(popover, /onOpenMainWindow/, 'that row must be wired to a handler');
});

test('Open Petal SHOWS the window instead of navigating away from a live meeting', () => {
  // openMainRoute('/main') hard-navigates the main webview. If the user is in a
  // meeting, that runs the meeting route's onDestroy -- stopLocalCamera() and
  // pill.restoreHomeWindow() -- while they are STILL in the room, because
  // leave_room is never called and /main does not redirect back. "Open Petal"
  // reads as "show me Petal" and will be clicked mid-meeting.
  // Extract the handler body precisely: a character-window regex here reaches
  // into the NEXT function (onOpenSettings, which legitimately navigates) and
  // reports a false failure.
  const start = popover.indexOf('function onOpenMainWindow()');
  assert.notEqual(start, -1, 'onOpenMainWindow must exist');
  const end = popover.indexOf('\n  }', start);
  assert.notEqual(end, -1, 'onOpenMainWindow must be a closed function body');
  const handler = popover.slice(start, end);

  assert.match(handler, /COMMANDS\.showMainWindow/, 'Open Petal must invoke the show-only command');
  assert.doesNotMatch(
    handler,
    /openMainRoute/,
    'Open Petal must not navigate -- it would cut the camera of a user still in a room'
  );
});

test('a deep link into a meeting cannot join with the window hidden', () => {
  // THE expensive one. tao's macOS set_focus is a no-op on a hidden window
  // (it checks is_visible first), so a petal:// join arriving while the user
  // has red-dot-hidden the window would navigate, mount, join_room -- live mic
  // -- with no visible UI at all. frontend_ready cannot save it: reveal is a
  // spent one-shot by then.
  assert.match(
    deepLink,
    /if crate::main_window_revealed\(\)\s*\{\s*crate::show_and_activate_main_window\(&app, "deep-link"\);/,
    'deep-link navigation must show a user-hidden window, gated on the reveal (#636 cold start)'
  );
});

test('a Dock click does not yank the main window over remote shares mid-meeting', () => {
  // Activating on EVERY reopen drags the 400px main window above arranged
  // remote share windows and steals key focus. Gate on the window's own
  // state -- NOT the event's has_visible_windows, which a visible pill or
  // share panel makes true while main is hidden, re-stranding the user.
  assert.match(
    lib,
    /RunEvent::Reopen[\s\S]{0,600}is_visible\(\)[\s\S]{0,200}is_minimized\(\)/,
    'reopen must restore only when main is actually hidden or minimized'
  );
  assert.doesNotMatch(
    lib,
    /RunEvent::Reopen\s*\{\s*has_visible_windows/,
    'has_visible_windows is the wrong signal -- a visible pill makes it true while main is hidden'
  );
});

test('the dots are macOS-only while Windows has no discoverable way back', () => {
  // Windows has no Reopen handler, no Open Petal popover row and no tray icon,
  // and hide() also removes the taskbar button. A second launch recovers, but
  // nothing on screen tells the user that.
  assert.match(
    mainMenu,
    /\{#if isMac\(\)\}[\s\S]{0,400}class="window-controls"/,
    'the dots must be gated behind isMac() until Windows has a way back'
  );
  assert.match(popover, /\{#if isMac\(\)\}[\s\S]{0,160}label="Open Petal"/);
});

test('open_main_route shows a user-hidden window without regressing the #636 cold start', () => {
  assert.match(
    mainWindowRs,
    /if crate::main_window_revealed\(\)\s*\{\s*crate::show_and_activate_main_window\(app, "open-main-route"\);/,
    'showing must be gated on the reveal having already happened, or it races first paint'
  );
});
