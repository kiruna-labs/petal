import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(resolve(__dirname, '../src/routes/hover-tab/+page.svelte'), 'utf8');
const rustSource =
  readFileSync(resolve(__dirname, '../src-tauri/src/hover_tab.rs'), 'utf8') +
  '\n' +
  readFileSync(resolve(__dirname, '../src-tauri/src/hover_core.rs'), 'utf8');
const windowsHoverSource = readFileSync(resolve(__dirname, '../src-tauri/src/windows_hover.rs'), 'utf8');
const shareSessionSource = readFileSync(resolve(__dirname, '../src-tauri/src/session/share.rs'), 'utf8');
const roomSource = readFileSync(resolve(__dirname, '../src-tauri/src/session/room.rs'), 'utf8');
const publisherSource = readFileSync(resolve(__dirname, '../src-tauri/src/transport/publisher.rs'), 'utf8');
const popupSource = readFileSync(resolve(__dirname, '../src/lib/shareOptionsPopup.ts'), 'utf8');
const ipcSource = readFileSync(resolve(__dirname, '../src/lib/ipc.ts'), 'utf8');

test('hover tab disables share toggle while native state is pending', () => {
  assert.match(source, /if \(pending \|\| currentWindowId === null \|\| currentFrame === null\) return;/);
  assert.match(source, /disabled=\{pending\}/);
  assert.match(source, /aria-busy=\{pending\}/);
});

test('hover tab keeps start optimism but waits for the native stop boundary', () => {
  const handler = source.slice(source.indexOf('async function onToggleShare()'));
  const invokeIndex = handler.indexOf('await invoke<boolean>(COMMANDS.toggleWindowShare');
  const beforeInvoke = handler.slice(0, invokeIndex);

  assert.ok(invokeIndex > 0, 'toggleWindowShare invoke is present');
  assert.match(beforeInvoke, /const wasShared = sharedWindows\.has\(windowId\);/);
  assert.match(beforeInvoke, /const targetShared = !wasShared;/);
  assert.match(beforeInvoke, /if \(targetShared\) sharedWindows = windowShareSet\(sharedWindows, windowId, true\);/);
  assert.doesNotMatch(beforeInvoke, /sharedWindows = windowShareSet\(sharedWindows, windowId, targetShared\);/);
  assert.match(source, /EVENTS\.shareStateChanged/);
});

test('hover tab only rolls back optimistic start when native toggle fails', () => {
  const handler = source.slice(source.indexOf('async function onToggleShare()'));
  const catchIndex = handler.indexOf('} catch {');
  const finallyIndex = handler.indexOf('} finally {', catchIndex);
  const catchBlock = handler.slice(catchIndex, finallyIndex);

  assert.match(catchBlock, /if \(targetShared\) sharedWindows = windowShareSet\(sharedWindows, windowId, wasShared\);/);
  assert.match(catchBlock, /A failed stop is already/);
  assert.match(catchBlock, /must stay unshared/);
});

test('native stop emits the pill transition between capture stop and unpublish', () => {
  const captureEnd = shareSessionSource.indexOf('capture.stop() end');
  const indicatorBoundary = shareSessionSource.indexOf('clear_share_state_for_window(app, window_id);', captureEnd);
  const unpublishBegin = shareSessionSource.indexOf('unpublish begin', indicatorBoundary);

  assert.ok(captureEnd >= 0, 'stop capture boundary is present');
  assert.ok(indicatorBoundary > captureEnd, 'local indicators clear after capture.stop returns');
  assert.ok(unpublishBegin > indicatorBoundary, 'network unpublish remains after local indicator clear');
  assert.match(rustSource, /emit_share_state_changed\(app, window_id, false\);/);
});

test('Windows stop handoff updates the token without a frontend visibility workaround', () => {
  assert.match(source, /const previousWindowId = currentWindowId;/);
  assert.match(source, /currentWindowId = windowId;/);
  assert.match(source, /if \(!pending\) \{[\s\S]{0,180}sharedWindows = windowShareSet/);
  assert.match(
    windowsHoverSource,
    /adopt_hover_target_replacement\(last\)[\s\S]{0,900}update\.window_id = replacement_token;[\s\S]{0,180}update\.shared = false;/
  );
});

test('real toggle path can inject a slow or failing unpublish tail', () => {
  assert.match(publisherSource, /PETAL_TEST_UNPUBLISH_DELAY_MS/);
  assert.match(publisherSource, /PETAL_TEST_UNPUBLISH_FAIL/);
  assert.match(publisherSource, /injected unpublish failure/);
  assert.match(source, /COMMANDS\.toggleWindowShare/);
});

test('the hover tab is exactly one fixed 40 by 40 native surface', () => {
  assert.match(rustSource, /pub const HOVER_TAB_COMPACT_WIDTH: f64 = 40\.0;/);
  assert.match(rustSource, /pub const HOVER_TAB_COMPACT_HEIGHT: f64 = 40\.0;/);
  assert.match(source, /class="hover-tab-host"/);
  assert.match(source, /class="hover-tab-action hover-tab-trigger"/);
  assert.match(source, /width: 40px;/);
  assert.match(source, /height: 40px;/);
  assert.doesNotMatch(source, /hover-tab-tray|hover-tab-options|promptExpanded|isExpanded/);
  assert.match(source, /class="hover-tab-action hover-tab-trigger"/);
  assert.doesNotMatch(rustSource, /SHARE_TAB_WIDTH|HOVER_TAB_PROMPT_HEIGHT|expanded: bool|prompt_expanded/);
  assert.doesNotMatch(windowsHoverSource, /set_hover_tab_presentation|HOVER_TAB_ESCALATION_HEIGHT/);
  assert.doesNotMatch(ipcSource, /setHoverTabPresentation|set_hover_tab_presentation|promptExpanded/);
});

test('primary activation is direct Share/Stop and menu activation cannot fall through', () => {
  assert.match(source, /onclick=\{onToggleShare\}/);
  assert.match(source, /aria-label=\{shareActionAriaLabel\}/);
  assert.match(source, /title=\{isWindows\(\) \? shareActionTooltip : undefined\}/);
  assert.match(source, /data-allow-native-tooltip/);
  assert.match(source, /oncontextmenu=\{onActionContextMenu\}/);
  assert.match(source, /event\.preventDefault\(\);/);
  assert.match(source, /event\.stopPropagation\(\);/);
  assert.match(source, /event\.key === 'F10' && event\.shiftKey/);
  assert.match(source, /event\.key === 'ContextMenu'|event\.key === 'Menu'/);
  assert.match(source, /new LogicalPosition\(rect\.left, rect\.bottom\)/);
});

test('native options menu remains shared, held, and deterministically disposed', () => {
  assert.match(source, /buildShareOptionsMenuEntries\(/);
  assert.match(source, /await popupShareOptionsMenu\(entries,/);
  assert.match(source, /COMMANDS\.setHoverTabMenuOpen, \{ open: true \}/);
  assert.match(source, /COMMANDS\.setHoverTabMenuOpen, \{ open: false \}/);
  assert.match(popupSource, /options\.position/);
  assert.match(popupSource, /options\.window/);
  assert.match(popupSource, /await menu\.close\(\);/);
  assert.match(popupSource, /Promise\.all\(items\.map\(\(item\) => item\.close\(\)\)\)/);
});

test('hover tab native tracker includes already-shared windows and preserves compact ordering', () => {
  assert.match(rustSource, /let mut blocked_by_surface = false;[\s\S]*shareable_window_in\(cursor, stack\)/);
  assert.doesNotMatch(rustSource, /filter\(\|\(_, window_id\)\| !super::is_shared\(app, \*window_id\)\)/);
  assert.match(rustSource, /shared: super::is_shared\(app, window_id\),/);
  assert.match(windowsHoverSource, /crate::share_target::classify\(&inspection\.facts\)/);
  assert.match(windowsHoverSource, /native_hover_tab_attachment/);
  assert.match(windowsHoverSource, /attach_hover_tab_follower/);
  assert.match(windowsHoverSource, /replace_hover_tab_follower_token/);
  assert.match(windowsHoverSource, /SWP_SHOWWINDOW/);
  assert.doesNotMatch(windowsHoverSource, /fn position_pill\(|window\.set_size\(/);
  assert.match(rustSource, /begin_hover_tab_presentation\(\)/);
});

test('Draw remains reachable from the native menu without changing primary Stop semantics', () => {
  assert.match(source, /drawActive/);
  assert.match(source, /COMMANDS\.shareOverlaySetDrawActive/);
  assert.match(source, /onDraw: \(active\) => void selectDraw\(active\)/);
  assert.match(source, /onclick=\{onToggleShare\}/);
  assert.match(source, /Drawing is active on this shared window/);
  assert.match(source, /if \(!drawActive\) \{/);
});

test('shared state retains identity color, live marker, controlled context, and AI error context', () => {
  assert.match(source, /class:is-shared=\{isShared\}/);
  assert.match(source, /hover-tab-live-dot/);
  assert.match(source, /controlledWindows/);
  assert.match(source, /shareControlMode === 'fullControl'/);
  assert.match(source, /EVENTS\.shareControlModeChanged/);
  assert.match(source, /currentAiChatError/);
  assert.match(source, /aiChatHoverTabOptionsTitle\(currentAiChatError\)/);
});

test('unshared hover tabs use the bright live border while shared tabs keep their fill', () => {
  assert.match(source, /\.hover-tab-action:not\(\.is-shared\)\s*\{[\s\S]*border-color:\s*var\(--live-bright, #7ff0a3\);/);
  assert.match(source, /border:\s*1px solid transparent;/);
});

test('phase drag command freezes followers, validates target state, and owns commit/cancel', () => {
  assert.match(ipcSource, /hoverTabDrag: 'hover_tab_drag'/);
  assert.match(ipcSource, /export type HoverTabDragPhase = 'begin' \| 'update' \| 'commit' \| 'cancel'/);
  assert.match(windowsHoverSource, /pub fn hover_tab_drag\(/);
  assert.match(windowsHoverSource, /DRAG_ACTIVE\.load/);
  assert.match(windowsHoverSource, /project_hover_tab_native_frame_with_offset/);
  assert.match(windowsHoverSource, /commit_hover_tab_vertical_offset/);
  assert.match(windowsHoverSource, /finish_hover_tab_drag/);
  assert.match(rustSource, /pub fn hover_tab_drag\(/);
  assert.match(rustSource, /platform::reset_drag_state\(true\)/);
  assert.match(roomSource, /cancel_drag_for_lifecycle/);
});

test('macOS hover input path retains drag handling but no tray dismissal interception', () => {
  const gesture = readFileSync(resolve(__dirname, '../src-tauri/src/platform/gesture_tap.rs'), 'utf8');
  assert.match(gesture, /K_CG_EVENT_LEFT_MOUSE_DOWN/);
  assert.match(gesture, /gesture_track_for|target\(\)/);
  assert.doesNotMatch(gesture, /dismiss_hover_tab_for_click_away|dismiss_hover_tab_for_escape|K_CG_KEYCODE_ESCAPE|K_CG_EVENT_KEY_DOWN/);
});
