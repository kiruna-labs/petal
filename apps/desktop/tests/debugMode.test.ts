import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { debugHeaderControlVisible } from '../src/lib/data/debugMode.ts';
import { COMMANDS, EVENTS } from '../src/lib/ipc.ts';

function input(overrides: Partial<Parameters<typeof debugHeaderControlVisible>[0]> = {}) {
  return {
    debugModeEnabled: true,
    aiChatLive: false,
    viewportWidth: 1000,
    ...overrides
  };
}

// ---- pure predicate: the setting itself ------------------------------------

test('#669: debug header control is hidden when the setting is off, shown when on', () => {
  assert.equal(debugHeaderControlVisible(input({ debugModeEnabled: true })), true);
  assert.equal(debugHeaderControlVisible(input({ debugModeEnabled: false })), false);
});

// ---- pure predicate: composition with the two existing layout suppressors --

test('#669: an enabled setting still yields to the AI-chat-live suppressor', () => {
  assert.equal(
    debugHeaderControlVisible(input({ debugModeEnabled: true, aiChatLive: true })),
    false
  );
  assert.equal(
    debugHeaderControlVisible(input({ debugModeEnabled: true, aiChatLive: false })),
    true
  );
});

test('#669: an enabled setting still yields to the <640px viewport suppressor', () => {
  assert.equal(
    debugHeaderControlVisible(input({ debugModeEnabled: true, viewportWidth: 640 })),
    false
  );
  assert.equal(
    debugHeaderControlVisible(input({ debugModeEnabled: true, viewportWidth: 300 })),
    false
  );
  assert.equal(
    debugHeaderControlVisible(input({ debugModeEnabled: true, viewportWidth: 641 })),
    true
  );
});

test('#669: a disabled setting stays hidden regardless of the layout suppressors', () => {
  assert.equal(
    debugHeaderControlVisible(
      input({ debugModeEnabled: false, aiChatLive: false, viewportWidth: 2000 })
    ),
    false
  );
});

test('#669: all three gates must hold together for the control to show', () => {
  assert.equal(
    debugHeaderControlVisible({ debugModeEnabled: true, aiChatLive: false, viewportWidth: 1000 }),
    true
  );
  assert.equal(
    debugHeaderControlVisible({ debugModeEnabled: true, aiChatLive: true, viewportWidth: 300 }),
    false
  );
});

// ---- IPC registry ------------------------------------------------------------

test('#669: debug mode commands and event are registered in the IPC registry', () => {
  assert.equal(COMMANDS.debugModeSettings, 'debug_mode_settings');
  assert.equal(COMMANDS.setDebugMode, 'set_debug_mode');
  assert.equal(EVENTS.debugModeChanged, 'debug-mode-changed');
});

// ---- Rust: store + commands registered on BOTH platform command lists ------

test('#669: debug_settings module exists with the same shape as ai_chat/settings.rs', () => {
  const source = readFileSync(
    new URL('../src-tauri/src/debug_settings.rs', import.meta.url),
    'utf8'
  );
  assert.match(source, /pub struct DebugSettings/);
  assert.match(source, /pub enabled: bool/);
  assert.match(source, /static STORE: OnceLock<Mutex<Option<Store>>>/);
  assert.match(source, /pub fn initialize\(app_data_dir: &Path\)/);
  assert.match(source, /pub fn current\(\) -> DebugSettings/);
  assert.match(source, /pub fn is_enabled\(\) -> bool/);
  assert.match(source, /pub fn update\(/);
  assert.match(source, /pub fn debug_mode_settings\(\) -> DebugSettings/);
  assert.match(
    source,
    /pub fn set_debug_mode\(app: tauri::AppHandle, enabled: bool\) -> Result<DebugSettings, String>/
  );
  // Off by default -- same fail-closed contract as AI chat's settings.
  assert.match(source, /debug mode must be OFF by default/);
  // The setter emits so an already-open header updates live -- the AI chat
  // gap this setting deliberately does not repeat (see docs above).
  assert.match(source, /app\.emit\(DEBUG_MODE_CHANGED_EVENT/);
  assert.match(source, /DEBUG_MODE_CHANGED_EVENT: &str = "debug-mode-changed"/);
});

test('#669: debug_settings is a declared module and initialized on both platform setup paths', () => {
  const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  assert.match(lib, /mod debug_settings;/);

  // Two separate `run()` entry points exist (macOS-gated and
  // not-macOS-gated) with their own `.setup(...)` closures -- initialize
  // must appear in both, not just one.
  const macRun = lib.split('#[cfg(target_os = "macos")]\n#[cfg_attr(mobile, tauri::mobile_entry_point)]\npub fn run()')[1];
  const otherRun = lib.split('#[cfg(not(target_os = "macos"))]\n#[cfg_attr(mobile, tauri::mobile_entry_point)]\npub fn run()')[1];
  assert.ok(macRun, 'macOS run() not found');
  assert.ok(otherRun, 'non-macOS run() not found');
  assert.match(macRun, /debug_settings::initialize\(&app_data_dir\)/);
  assert.match(otherRun, /debug_settings::initialize\(&app_data_dir\)/);
});

test('#669: debug_mode_settings and set_debug_mode are registered on BOTH the macOS and Windows invoke_handler lists', () => {
  const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  const [macSection, otherSection] = lib.split('.invoke_handler(tauri::generate_handler![').slice(1);
  assert.ok(macSection, 'macOS invoke_handler list not found');
  assert.ok(otherSection, 'non-macOS (Windows) invoke_handler list not found');
  assert.match(macSection, /debug_settings::debug_mode_settings,/);
  assert.match(macSection, /debug_settings::set_debug_mode,/);
  assert.match(otherSection, /debug_settings::debug_mode_settings,/);
  assert.match(otherSection, /debug_settings::set_debug_mode,/);
});

// ---- Settings.svelte ----------------------------------------------------------

test('#669: Settings panel exposes a Debug mode toggle mirroring the AI chat toggle pattern', () => {
  const settings = readFileSync(
    new URL('../src/lib/components/Settings.svelte', import.meta.url),
    'utf8'
  );
  assert.match(settings, /import \{ DEBUG_MODE_SETTING_DESCRIPTION, DEBUG_MODE_SETTING_TITLE \} from '\$lib\/data\/debugMode';/);
  assert.match(settings, /let debugSettings = \$state<DebugModeSettings>\(\{ enabled: false \}\);/);
  assert.match(
    settings,
    /async function handleDebugModeEnabledChange\(enabled: boolean\) \{[\s\S]*COMMANDS\.setDebugMode/
  );
  assert.match(settings, /checked=\{debugSettings\.enabled\}/);
  assert.match(
    settings,
    /onchange=\{\(e\) => void handleDebugModeEnabledChange\(e\.currentTarget\.checked\)\}/
  );
  assert.match(settings, /\{DEBUG_MODE_SETTING_TITLE\}/);
  assert.match(settings, /\{DEBUG_MODE_SETTING_DESCRIPTION\}/);
  assert.match(settings, /invoke<DebugModeSettings>\(COMMANDS\.debugModeSettings\)/);
});

// ---- surface/+page.svelte: ask AND listen, debugShown wiring ----------------

test('#669: the remote-window surface asks AND listens for debug mode, fixing the AI-chat live-propagation gap', () => {
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  assert.match(surfaceRoute, /import \{ debugHeaderControlVisible \} from '\$lib\/data\/debugMode';/);
  assert.match(surfaceRoute, /let debugModeEnabled = \$state\(false\);/);
  assert.match(
    surfaceRoute,
    /const debugShown = \$derived\(\s*debugHeaderControlVisible\(\{[\s\S]*debugModeEnabled,[\s\S]*aiChatLive: aiChatActive,/
  );
  // Ask.
  assert.match(
    surfaceRoute,
    /async function refreshDebugModeEnabled\(\) \{[\s\S]*invoke<DebugModeSettings>\(COMMANDS\.debugModeSettings\)/
  );
  assert.match(surfaceRoute, /void refreshDebugModeEnabled\(\);/);
  // Listen -- the belt-and-braces half AI chat's own setter never grew.
  assert.match(
    surfaceRoute,
    /listen<DebugModeSettings>\(EVENTS\.debugModeChanged, \(event\) => \{\s*debugModeEnabled = event\.payload\.enabled;/
  );
  assert.match(surfaceRoute, /unlistenDebugModeChanged\?\.\(\);/);
  // Prop wiring into the header.
  assert.match(surfaceRoute, /\{debugShown\}/);
});

// ---- RemoteWindowHeader.svelte: gated + a11y bonus fix -----------------------

test('#669: RemoteWindowHeader wraps the Debug button in {#if debugShown}, mirroring the AI chat button', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );
  assert.match(headerComponent, /debugShown\?: boolean;/);
  assert.match(headerComponent, /debugShown = false,/);
  assert.match(headerComponent, /\{#if debugShown\}\s*\n\s*<button\s*\n\s*type="button"\s*\n\s*class="header-btn debug-btn"/);
  // #669 bonus a11y fix: pressed state + Show/Hide label, matching
  // web-harness's existing Debug button behavior.
  assert.match(headerComponent, /let debugActive = \$state\(false\);/);
  assert.match(headerComponent, /class:active=\{debugActive\}/);
  assert.match(headerComponent, /aria-pressed=\{debugActive\}/);
});

// ---- web-harness parity -------------------------------------------------------

test('#669: web-harness mirrors the debug-mode gate through the SAME shared predicate as desktop', () => {
  const webHeader = readFileSync(
    new URL('../../../web-harness/src/remoteWindowHeader.ts', import.meta.url),
    'utf8'
  );
  const desktopHeaderData = readFileSync(
    new URL('../src/lib/data/debugMode.ts', import.meta.url),
    'utf8'
  );
  const sharedPredicate = readFileSync(
    new URL('../../../shared/logic/debugHeaderVisibility.ts', import.meta.url),
    'utf8'
  );

  // Both clients import the SAME module -- not independently duplicated
  // logic -- so they can never drift on when the button disappears.
  assert.match(
    webHeader,
    /import \{ debugHeaderControlVisible \} from '@petal\/shared\/logic\/debugHeaderVisibility';/
  );
  assert.match(desktopHeaderData, /@petal\/shared\/logic\/debugHeaderVisibility/);
  assert.match(sharedPredicate, /export function debugHeaderControlVisible/);

  assert.match(
    webHeader,
    /const debugVisible = debugHeaderControlVisible\(\{[\s\S]*debugModeEnabled: current\.ctx\.state\.debugModeEnabled,/
  );
  assert.match(webHeader, /debugButton\.classList\.toggle\('is-hidden', !debugVisible\);/);
});

test('#669: web-harness exposes its own Debug mode toggle, default off, persisted to localStorage', () => {
  const context = readFileSync(new URL('../../../web-harness/src/context.ts', import.meta.url), 'utf8');
  const constants = readFileSync(
    new URL('../../../web-harness/src/constants.ts', import.meta.url),
    'utf8'
  );
  const controls = readFileSync(new URL('../../../web-harness/src/controls.ts', import.meta.url), 'utf8');
  const main = readFileSync(new URL('../../../web-harness/src/main.ts', import.meta.url), 'utf8');
  const indexHtml = readFileSync(new URL('../../../web-harness/index.html', import.meta.url), 'utf8');

  assert.match(context, /debugModeEnabled: boolean;/);
  assert.match(context, /debugModeCheckbox: HTMLInputElement;/);
  assert.match(context, /syncRemoteWindowHeaders: \(\) => void;/);
  assert.match(constants, /HARNESS_DEBUG_MODE_STORAGE_KEY = 'petal-harness-debug-mode-enabled'/);
  assert.match(indexHtml, /id="debug-mode-checkbox"/);
  assert.match(
    main,
    /debugModeEnabled: localStorage\.getItem\(HARNESS_DEBUG_MODE_STORAGE_KEY\) === '1',/
  );
  assert.match(
    controls,
    /debugModeCheckbox\.addEventListener\('change', \(\) => \{[\s\S]*state\.debugModeEnabled = debugModeCheckbox\.checked;/
  );
  // Toggling it reaches already-open remote windows immediately (this JS
  // realm has no cross-webview propagation problem to solve).
  assert.match(controls, /cb\.syncRemoteWindowHeaders\(\);/);
});
