import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { EVENTS } from '../src/lib/ipc.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));

const ipcSource = readFileSync(resolve(__dirname, '../src/lib/ipc.ts'), 'utf8');
const pickerSource = readFileSync(
  resolve(__dirname, '../src/lib/components/WindowPicker.svelte'),
  'utf8'
);
const watcherRs = readFileSync(
  resolve(__dirname, '../src-tauri/src/window_change_watcher.rs'),
  'utf8'
);
const pickerRs = readFileSync(resolve(__dirname, '../src-tauri/src/window_picker.rs'), 'utf8');
const sessionRs = readFileSync(resolve(__dirname, '../src-tauri/src/session/room.rs'), 'utf8');
const sessionStubRs = readFileSync(
  resolve(__dirname, '../src-tauri/src/session_stub.rs'),
  'utf8'
);
const windowSourceRs = readFileSync(
  resolve(__dirname, '../src-tauri/src/window_source.rs'),
  'utf8'
);
const libRs = readFileSync(resolve(__dirname, '../src-tauri/src/lib.rs'), 'utf8');

// ---- event registry --------------------------------------------------------

test('desktop-windows-changed event is registered with a void payload', () => {
  assert.equal(EVENTS.desktopWindowsChanged, 'desktop-windows-changed');
  assert.match(ipcSource, /\[EVENTS\.desktopWindowsChanged\]: void;/);
});

// ---- Rust emitter ----------------------------------------------------------

test('the watcher emits the exact registered event name and busts the list cache', () => {
  assert.match(
    watcherRs,
    /DESKTOP_WINDOWS_CHANGED_EVENT: &str = "desktop-windows-changed"/
  );
  assert.match(watcherRs, /app\.emit\(DESKTOP_WINDOWS_CHANGED_EVENT, \(\)\)/);
  // The fire must invalidate the cached enumeration so the picker's refresh
  // sees the CURRENT window set, not a pre-event one.
  assert.match(watcherRs, /crate::window_source::invalidate_list_cache\(\)/);
  assert.match(windowSourceRs, /pub\(crate\) fn invalidate_list_cache\(\)/);
});

test('the watcher is picker-scoped, not always-on', () => {
  // Started when the picker becomes visible (rebuild or re-show of the
  // singleton window), from window_picker.rs — not at app setup.
  assert.match(pickerRs, /window_change_watcher::start\(app\.clone\(\)\)/);
  assert.doesNotMatch(libRs, /window_change_watcher::start/);
  assert.match(libRs, /mod window_change_watcher;/);
  // And it self-terminates when the picker is no longer visible
  // (closed/minimized/Win+D): the pump runs a periodic visibility life gate.
  assert.match(watcherRs, /picker_window_is_visible\(/);
  assert.match(watcherRs, /VISIBILITY_TIMER_ID/);
  assert.match(watcherRs, /IsWindowVisible/);
  assert.match(watcherRs, /IsIconic/);
});

test('the picker is hidden (not destroyed) on meeting exit', () => {
  // User requirement: the picker must not remain on the desktop after the
  // user exits the meeting. Hide keeps it as a cheap-to-re-show singleton.
  assert.match(pickerRs, /fn hide_picker_on_meeting_exit/);
  assert.match(pickerRs, /picker\.hide\(\)/);
  assert.match(pickerRs, /HIDDEN singleton/);
  // Re-show must not show a stale pre-exit grid: the watcher fires one
  // immediate refresh on start (the hidden webview's onMount never re-runs).
  // Assert the CALL (fire) near the log line, not just the log string.
  assert.match(watcherRs, /initial refresh on watcher start[\s\S]{0,120}fire\(app\)/);
  // Every leave path hides it: the real session (macOS) cleanup and the
  // Windows session's leave_room_inner (covers explicit leave, forced
  // disconnect, join disconnect, and room switch).
  assert.match(sessionRs, /hide_picker_on_meeting_exit\(app\)/);
  assert.match(sessionStubRs, /hide_picker_on_meeting_exit\(app\)/);
});

// ---- picker listener -------------------------------------------------------

test('the open picker soft-refreshes on the desktop-windows-changed event', () => {
  // Listener must be registered through the race-safe helper, gated on the
  // Tauri bridge (browser fallback has no Rust events), and unlistened on
  // teardown.
  assert.match(pickerSource, /listenUntilDestroy\(\s*EVENTS\.desktopWindowsChanged/);
  assert.match(pickerSource, /hasTauriBridge\(\)/);
  assert.match(pickerSource, /refresh\(\{ showLoading: false, force: false \}\)/);
  assert.match(pickerSource, /unlistenDesktop\?\.\(\)/);
});
