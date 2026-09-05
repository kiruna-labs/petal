import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (path: string) => readFileSync(new URL(path, import.meta.url), 'utf8');

test('main Share control toggles the picker instead of stopping active shares', () => {
  const route = read('../src/routes/meeting/[room]/+page.svelte');
  assert.match(route, /COMMANDS\.toggleWindowPickerWindow/);
  assert.match(route, /async function handleScreenshareControl\(\) \{\s*await openSharePicker\(\);/);
  assert.doesNotMatch(route, /stopping active shares/);
  assert.doesNotMatch(route, /for \(const windowId of ids\)/);
});

test('picker visibility highlights the main Share control and picker opens refresh once', () => {
  const meeting = read('../src/routes/meeting/[room]/+page.svelte');
  const pickerRoute = read('../src/routes/window-picker/+page.svelte');
  const picker = read('../src/lib/components/WindowPicker.svelte');
  const chrome = read('../src/lib/components/MeetingChrome.svelte');

  assert.match(meeting, /sharePickerVisibilityChanged/);
  assert.match(meeting, /sharingPickerOpen=\{sharePickerOpen\}/);
  assert.match(pickerRoute, /notifyVisibility\(true\)/);
  assert.match(pickerRoute, /notifyVisibility\(false\)/);
  assert.match(picker, /EVENTS\.sharePickerOpened/);
  assert.match(chrome, /sharingPickerOpen\?: boolean/);
  assert.match(chrome, /screenshareControlActive/);
});

test('only active Windows display cards expose sharing settings', () => {
  const picker = read('../src/lib/components/WindowPicker.svelte');
  assert.match(picker, /isShared && win\.kind === 'display' && isWindows\(\)/);
  assert.match(picker, /COMMANDS\.shareOverlayDrawActive/);
  assert.match(picker, /COMMANDS\.shareOverlaySetDrawActive/);
  assert.match(picker, /Full control enabled/);
  assert.match(picker, /Open sharing settings for/);
});

test('sharer Draw keeps a reachable stop control above the interactive overlay', () => {
  const pointer = read('../src/routes/compositor/pointer/+page.svelte');
  const macOverlay = read('../src-tauri/src/share_overlay.rs');
  const macHover = read('../src-tauri/src/hover_tab.rs');
  const windowsOverlay = read('../src-tauri/src/windows_share_overlay.rs');
  const windowsSession = read('../src-tauri/src/session_stub.rs');
  assert.match(pointer, /sharer-draw-toolbar/);
  assert.match(pointer, /Stop drawing/);
  assert.match(pointer, /COMMANDS\.shareOverlaySetDrawActive/);
  assert.match(pointer, /stopPropagation\(\)/);
  assert.match(pointer, /sharerSurface && drawActive && sharerDrawToolbar/);
  assert.match(pointer, /drawToolbar/);
  assert.match(pointer, /__petalDrawSetToolbarVisible/);
  assert.match(macHover, /source_kind == crate::transport::publisher::SharedSourceKind::Display/);
  assert.match(macOverlay, /drawToolbar=\{draw_toolbar\}/);
  assert.match(windowsSession, /kind == SharedSourceKind::Display/);
  assert.match(windowsOverlay, /drawToolbar=\{draw_toolbar\}/);
});
