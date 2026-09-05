import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const compositor = readFileSync(
  new URL('../src-tauri/src/windows_compositor.rs', import.meta.url),
  'utf8'
);
const sharerOverlay = readFileSync(
  new URL('../src-tauri/src/windows_share_overlay.rs', import.meta.url),
  'utf8'
);
const windowsHover = readFileSync(
  new URL('../src-tauri/src/windows_hover.rs', import.meta.url),
  'utf8'
);
const hoverTab = readFileSync(new URL('../src/routes/hover-tab/+page.svelte', import.meta.url), 'utf8');
const pointer = readFileSync(new URL('../src/routes/compositor/pointer/+page.svelte', import.meta.url), 'utf8');
const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');


test('Windows Draw activation is registered and no longer a logged no-op', () => {
  assert.match(lib, /windows_compositor::compositor_set_draw_active/);
  assert.match(lib, /draw::draw_send/);
  assert.match(lib, /windows_share_overlay::share_overlay_set_draw_active/);
  assert.doesNotMatch(compositor, /draw mode not implemented on Windows/);
  assert.match(compositor, /window\.\__petalDrawSetActive\?\.\(\{active_json\}\)/);
});

test('Windows sharer Draw refuses higher-integrity targets before enabling the overlay', () => {
  assert.match(sharerOverlay, /window_integrity_exceeds_petal/);
  assert.match(sharerOverlay, /Draw is unavailable for windows running with higher privileges than Petal/);
});

test('Windows sharer Draw toggles click-through and forwards state to the route', () => {
  assert.match(sharerOverlay, /state\.is_share_active\(window_id\)/);
  assert.match(sharerOverlay, /set_ignore_cursor_events\(!active\)/);
  assert.match(sharerOverlay, /window\.\__petalDrawSetActive\?\.\(\{active_json\}\)/);
  assert.match(sharerOverlay, /OVERLAY_DRAW_ACTIVE/);
  assert.match(sharerOverlay, /OVERLAY_DRAW_ACTIVE\.lock_unpoisoned\(\)\.remove\(&window_id\)/);
  assert.match(sharerOverlay, /pub\(crate\) fn is_draw_active\(window_id: u32\)/);
});

test('Windows remote overlays stay hidden until post-frame geometry is ready', () => {
  assert.match(compositor, /overlay_geometry_ready: bool/);
  assert.match(compositor, /let show_overlays = window\.overlay_geometry_ready && !window\.hidden/);
  assert.match(compositor, /if !window\.overlay_geometry_ready[\s\S]*?window\.latest_frame\.is_some\(\)[\s\S]*?window\.overlay_geometry_ready = true;/);
  assert.match(compositor, /if hidden \|\| !ready \{[\s\S]*?overlay\.hide\(\)/);
});

test('Windows sharer Draw focuses the interactive overlay when enabled', () => {
  assert.match(
    sharerOverlay,
    /if active \{[\s\S]*?window\.show\(\)[\s\S]*?window\.set_focus\(\)/
  );
});

test('Windows sharer Draw activation cannot trigger transient indicator fallback', () => {
  assert.match(sharerOverlay, /OVERLAY_DRAW_TRANSITIONING/);
  assert.match(sharerOverlay, /source_owned_overlay_fallback_allowed/);
  assert.match(
    sharerOverlay,
    /set_focus\(\)[\s\S]*?re-show sharer draw overlay failed/
  );
});

test('Windows keeps the hover pill alive while its native options menu is open', () => {
  assert.match(windowsHover, /static MENU_OPEN: AtomicBool/);
  assert.match(windowsHover, /MENU_OPEN\.load\(Ordering::Acquire\)/);
  assert.match(windowsHover, /pub fn set_hover_tab_menu_open\(open: bool\)/);
  assert.match(lib, /windows_hover::set_hover_tab_menu_open/);
});

test('hiding the hover pill does not cancel active sharer Draw', () => {
  const hideHandler = hoverTab.match(
    /const unHide = listen\(EVENTS\.hoverTabHide,[\s\S]*?\n  \}\);/
  );
  assert.ok(hideHandler, 'hover-tab hide handler should exist');
  assert.doesNotMatch(hideHandler[0], /stopDrawForWindow/);
  assert.match(hideHandler[0], /if \(!drawActive\) \{[\s\S]*?visible = false;/);
  assert.match(windowsHover, /windows_share_overlay::hwnd_for_local_share\(window_id\)/);
  assert.match(windowsHover, /last_hover_update\(\)\.is_none\(\)/);
});

test('sharer Draw uses the pen cursor used by controller Draw', () => {
  assert.match(pointer, /function penCursor\(color: string\)/);
  assert.match(pointer, /style:--draw-cursor=\{drawCursor\}/);
  assert.match(pointer, /cursor: var\(--draw-cursor, crosshair\)/);
  assert.doesNotMatch(pointer, /\.overlay\.sharer-input-active \{[\s\S]*?cursor: crosshair;/);
});
