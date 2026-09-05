import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const __dirname = dirname(fileURLToPath(import.meta.url));
const hoverTabSource = readFileSync(resolve(__dirname, '../src-tauri/src/hover_tab.rs'), 'utf8');
const coreSource = readFileSync(resolve(__dirname, '../src-tauri/src/hover_core.rs'), 'utf8');
const appkitSource = readFileSync(resolve(__dirname, '../src-tauri/src/platform/appkit.rs'), 'utf8');
const roomSource = readFileSync(resolve(__dirname, '../src-tauri/src/session/room.rs'), 'utf8');
const libSource = readFileSync(resolve(__dirname, '../src-tauri/src/lib.rs'), 'utf8');
const hoverTabPageSource = readFileSync(resolve(__dirname, '../src/routes/hover-tab/+page.svelte'), 'utf8');

test('macOS hover-tab parity keeps drag placement on the native panel path', () => {
  assert.match(hoverTabSource, /pub fn hover_tab_drag\(/);
  assert.match(hoverTabSource, /platform::set_drag_active\(true\)/);
  assert.match(hoverTabSource, /platform::set_drag_active\(false\)/);
  assert.match(hoverTabSource, /platform::tab_position_with_offset/);
  assert.match(hoverTabSource, /work_area\(\)/);
  assert.match(hoverTabSource, /crate::platform::cg::frame_for_window_id/);
  assert.match(hoverTabSource, /crate::platform::on_main/);
  assert.match(hoverTabSource, /reset_drag_state\(true\)/);
  assert.match(coreSource, /DEFAULT_HOVER_TAB_VERTICAL_OFFSET/);
  assert.match(coreSource, /HoverTabDragPhase/);
});

test('macOS hover-tab teardown cancels placement and preserves nonactivating behavior', () => {
  assert.match(roomSource, /crate::hover_tab::cancel_drag_for_lifecycle\(\)/);
  assert.match(libSource, /can_become_key_window: false/);
  assert.match(libSource, /nonactivating_panel/);
  assert.match(libSource, /hides_on_deactivate\(false\)/);
  assert.match(libSource, /allow_tooltips_when_application_inactive\(&window\)/);
  assert.match(libSource, /set_hover_tab_tooltip/);
  assert.match(hoverTabPageSource, /COMMANDS\.setHoverTabTooltip/);
  assert.match(hoverTabPageSource, /if \(!isMac\(\)\) return/);
  assert.match(hoverTabSource, /with_webview\(move \|webview\|/);
  assert.match(appkitSource, /setAllowsToolTipsWhenApplicationIsInactive/);
  assert.match(appkitSource, /setToolTip/);
  assert.match(
    appkitSource,
    /allow_tooltips_when_application_inactive[\s\S]*?MainThreadMarker::new/
  );
});
