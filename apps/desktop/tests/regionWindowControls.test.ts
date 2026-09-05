import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { buildShareOptionsMenuEntries } from '../src/lib/data/shareOptionsMenu.ts';

const route = readFileSync(new URL('../src/routes/region-window/+page.svelte', import.meta.url), 'utf8');
const popup = readFileSync(new URL('../src/lib/shareOptionsPopup.ts', import.meta.url), 'utf8');
const native = readFileSync(new URL('../src-tauri/src/region_window.rs', import.meta.url), 'utf8');
const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
const session = readFileSync(new URL('../src-tauri/src/session_stub.rs', import.meta.url), 'utf8');
const remoteControl = readFileSync(new URL('../src-tauri/src/remote_control.rs', import.meta.url), 'utf8');
const consent = readFileSync(new URL('../src/routes/control-consent/+page.svelte', import.meta.url), 'utf8');

test('Petal View title bar owns Options, Share/Stop, and Close', () => {
  assert.match(route, /data-region-options-control/);
  assert.match(route, /data-region-share-control/);
  assert.match(route, /CloseButton ariaLabel="Close region selector"/);
  assert.match(route, /aria-haspopup=\{drawActive \? undefined : 'menu'\}/);
  assert.match(route, /aria-label=\{drawActive \? 'Stop drawing on Petal View' : 'Petal View options'\}/);
  assert.match(route, /M19\.4 15a1\.7 1\.7/, 'Options must use the same gear glyph as the hover tab');
  assert.doesNotMatch(route, /M12 3v2M12 19v2/, 'brightness glyph must not represent Options');
  assert.match(route, /\.title-actions[\s\S]*?flex: 0 0 auto/);
  assert.match(route, /\.title-bar[\s\S]*?flex-wrap: wrap/);
  assert.match(route, /\.title-label[\s\S]*?flex: 1 1 140px/);
});

test('Petal View options omit redundant control mode and gate share-only actions', () => {
  const idle = buildShareOptionsMenuEntries(
    'automatic',
    false,
    false,
    'fullControl',
    false,
    true,
    false,
    true
  );
  assert.equal(idle.some((entry) => entry.kind === 'control-mode'), false);
  assert.equal(idle.some((entry) => entry.kind === 'ai-chat'), false);
  assert.equal(idle.find((entry) => entry.kind === 'annotation')?.enabled, false);

  const shared = buildShareOptionsMenuEntries(
    'automatic',
    true,
    false,
    'fullControl',
    false,
    true,
    false,
    true
  );
  assert.equal(shared.some((entry) => entry.kind === 'control-mode'), false);
  assert.equal(shared.find((entry) => entry.kind === 'annotation')?.enabled, true);
  assert.equal(shared.some((entry) => entry.kind === 'ai-chat'), true);
});

test('Petal View uses label-addressed state and the shared native popup lifecycle', () => {
  assert.match(route, /COMMANDS\.closeRegionWindow/);
  assert.match(route, /COMMANDS\.syncRegionWindowFrame/);
  assert.match(route, /COMMANDS\.regionViewOptionsState/);
  assert.match(route, /COMMANDS\.setRegionSharePriority/);
  assert.match(route, /COMMANDS\.setRegionDrawActive/);
  assert.match(route, /COMMANDS\.regionAiChatStart/);
  assert.match(route, /COMMANDS\.regionAiChatStop/);
  assert.match(route, /popupShareOptionsMenu\(entries/);
  assert.match(popup, /await menu\.close\(\);/);
  assert.match(native, /pub async fn sync_region_window_frame/);
  assert.match(native, /pub async fn region_view_options_state/);
  assert.match(native, /pub fn set_region_draw_active/);
  assert.match(native, /ensure_region_token\(&app, &window_label\)/);
});

test('Draw changes the persistent Options control into a direct stop action', () => {
  const button = route.slice(
    route.indexOf('data-region-options-control'),
    route.indexOf('</button>', route.indexOf('data-region-options-control'))
  );
  assert.match(button, /if \(drawActive\)/);
  assert.match(button, /setRegionDrawActive\(false\)/);
  assert.match(button, /path d="m4 4 16 16"/);
});

test('region actions are guarded against placement, stop, and stale-token races', () => {
  assert.match(route, /disabled=\{optionsPending \|\| placementActive \|\| placementSettlementPending\}/);
  assert.match(route, /seedVersion === optionsStateVersion/);
  assert.match(native, /if !state\.is_share_active\(token\)/);
  assert.match(native, /ensure_region_token\(&app, &window_label\)/);
  assert.match(native, /emit_region_view_options_changed_from_app\(&app, token\)/);
  assert.match(session, /stop_share_token\(&app, &state, window_id\)/);
  assert.match(session, /emit_share_state_changed\(&app, window_id, false\)/);
});

test('controller and consent lifecycle state cannot strand Petal View UI', () => {
  assert.match(remoteControl, /emit_region_control_state_for_status\(app, &status\)/);
  assert.match(native, /matches!\(status\.status, "active" \| "stopped" \| "disabled"\)/);
  assert.match(native, /active_controller_display_name/);
  assert.match(route, /EVENTS\.regionControlStateChanged/);
  assert.match(route, /controllerName = event\.payload\.active \? event\.payload\.controllerName : null/);
  assert.match(consent, /queue = \[\.\.\.queue, \{ \.\.\.payload/);
  assert.match(consent, /fullControlEscalation/);
  assert.match(consent, /timeoutMs/);
  assert.match(consent, /expiresAt/);
});

test('macOS region commands cannot block the main thread or trust a label suffix', () => {
  assert.match(native, /pub async fn sync_region_window_frame/);

  const ensureStart = native.indexOf('fn ensure_region_token');
  const syncStart = native.indexOf('/// Refresh the registry frame', ensureStart);
  assert.ok(ensureStart >= 0 && syncStart > ensureStart, 'ensure_region_token must remain inspectable');
  const ensureSource = native.slice(ensureStart, syncStart);
  const macStart = ensureSource.indexOf('#[cfg(target_os = "macos")]');
  const macEnd = ensureSource.indexOf('#[cfg(not(any', macStart);
  assert.ok(macStart >= 0 && macEnd > macStart, 'macOS token branch must remain explicit');
  const macBranch = ensureSource.slice(macStart, macEnd);

  assert.match(macBranch, /registered_token_for_label\(window_label\)/);
  assert.doesNotMatch(macBranch, /\btoken_for_label\(window_label\)/);
});

test('Petal View and hover wire contracts are registered on both native handlers', () => {
  const handlers = [...lib.matchAll(/\.invoke_handler\(tauri::generate_handler!\[\s*([\s\S]*?)\n\s*\]\)/g)].map(
    (match) => match[1]
  );
  assert.equal(handlers.length, 2, 'macOS and Windows must each expose an invoke handler');
  const macos = handlers[0];
  const windows = handlers[1];
  assert.ok(macos && windows, 'both invoke handler bodies must be present');

  const regionCommands = [
    ['openRegionWindow', 'open_region_window'],
    ['closeRegionWindow', 'close_region_window'],
    ['regionPlacementActive', 'region_placement_active'],
    ['regionShareState', 'region_share_state'],
    ['syncRegionWindowFrame', 'sync_region_window_frame'],
    ['regionViewOptionsState', 'region_view_options_state'],
    ['setRegionSharePriority', 'set_region_share_priority'],
    ['setRegionDrawActive', 'set_region_draw_active'],
    ['regionAiChatStart', 'region_ai_chat_start'],
    ['regionAiChatStop', 'region_ai_chat_stop'],
    ['toggleRegionShare', 'toggle_region_share']
  ] as const;
  const regionCommandsWithArgs = new Set([
    'regionPlacementActive',
    'regionShareState',
    'syncRegionWindowFrame',
    'regionViewOptionsState',
    'setRegionSharePriority',
    'setRegionDrawActive',
    'regionAiChatStart',
    'regionAiChatStop',
    'toggleRegionShare'
  ]);
  for (const [constant, wireName] of regionCommands) {
    assert.ok(ipc.includes(`${constant}: '${wireName}'`), `${constant} must have a wire name`);
    if (regionCommandsWithArgs.has(constant)) {
      assert.ok(ipc.includes(`[COMMANDS.${constant}]:`), `${constant} must have an argument contract`);
    }
    assert.ok(macos.includes(`region_window::${wireName}`), `${wireName} missing from macOS handler`);
    assert.ok(windows.includes(`region_window::${wireName}`), `${wireName} missing from Windows handler`);
  }

  const hoverCommands = [
    ['toggleWindowShare', 'toggle_window_share', 'toggle_window_share', 'windows_hover::toggle_window_share'],
    ['shareWindow', 'share_window', 'hover_tab::share_window', 'session::share_window'],
    ['sharedWindowIds', 'shared_window_ids', 'hover_tab::shared_window_ids', 'session::shared_window_ids'],
    ['hoverTabPageMounted', 'hover_tab_page_mounted', 'hover_tab::hover_tab_page_mounted', 'windows_hover::hover_tab_page_mounted'],
    ['setHoverTabMenuOpen', 'set_hover_tab_menu_open', 'hover_tab::set_hover_tab_menu_open', 'windows_hover::set_hover_tab_menu_open']
  ] as const;
  const hoverCommandsWithArgs = new Set([
    'toggleWindowShare',
    'shareWindow'
  ]);
  for (const [constant, wireName, macosHandler, windowsHandler] of hoverCommands) {
    assert.ok(ipc.includes(`${constant}: '${wireName}'`), `${constant} must have a wire name`);
    if (hoverCommandsWithArgs.has(constant)) {
      assert.ok(ipc.includes(`[COMMANDS.${constant}]:`), `${constant} must have an argument contract`);
    }
    assert.ok(macos.includes(macosHandler), `${wireName} missing from macOS handler`);
    assert.ok(windows.includes(windowsHandler), `${wireName} missing from Windows handler`);
  }
});

test('Petal View keeps native Draw adapters, label state, and popup cleanup', () => {
  assert.match(native, /fn region_draw_active\(token: u32\) -> bool \{\s*crate::share_overlay::share_overlay_draw_active\(token\)/);
  assert.match(native, /fn region_draw_active\(token: u32\) -> bool \{\s*crate::windows_share_overlay::share_overlay_draw_active\(token\)/);
  assert.match(native, /pub async fn region_view_options_state/);
  assert.match(native, /window_label: String/);
  assert.match(route, /COMMANDS\.regionViewOptionsState/);
  assert.match(popup, /await menu\.close\(\);/);
});
