import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { accessSync, constants, readFileSync, readdirSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

import { platformKey } from '../src/lib/platform.ts';

const tokensSource = readFileSync(new URL('../../../shared/ui/tokens.css', import.meta.url), 'utf8');
const appCssSource = readFileSync(new URL('../src/styles/app.css', import.meta.url), 'utf8');
const appHtmlSource = readFileSync(new URL('../src/app.html', import.meta.url), 'utf8');
const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const settingsSource = readFileSync(
  new URL('../src/lib/components/Settings.svelte', import.meta.url),
  'utf8'
);
const pillWindowSource = readFileSync(
  new URL('../src/lib/meeting/pillWindow.svelte.ts', import.meta.url),
  'utf8'
);
const onboardingSource = readFileSync(
  new URL('../src/lib/components/Onboarding.svelte', import.meta.url),
  'utf8'
);
const libRsSource = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const updaterRsSource = readFileSync(new URL('../src-tauri/src/updater.rs', import.meta.url), 'utf8');
const networkCockpitRsSource = readFileSync(
  new URL('../src-tauri/src/network_cockpit.rs', import.meta.url),
  'utf8'
);
const windowPickerRsSource = readFileSync(
  new URL('../src-tauri/src/window_picker.rs', import.meta.url),
  'utf8'
);
const windowsCompositorRsSource = readFileSync(
  new URL('../src-tauri/src/windows_compositor.rs', import.meta.url),
  'utf8'
);
const windowsHoverRsSource = readFileSync(
  new URL('../src-tauri/src/windows_hover.rs', import.meta.url),
  'utf8'
);
const windowsShareOverlayRsSource = readFileSync(
  new URL('../src-tauri/src/windows_share_overlay.rs', import.meta.url),
  'utf8'
);
const diagnosticsRsSource = readFileSync(
  new URL('../src-tauri/src/diagnostics.rs', import.meta.url),
  'utf8'
);
const pipelineStatsRsSource = readFileSync(
  new URL('../src-tauri/src/pipeline_stats.rs', import.meta.url),
  'utf8'
);
const sessionStubRsSource = readFileSync(
  new URL('../src-tauri/src/session_stub.rs', import.meta.url),
  'utf8'
);

function cssBlock(source: string, selector: string): string {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS block for ${selector}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('}', bodyStart);
  assert.notEqual(end, -1, `unterminated CSS block for ${selector}`);
  return source.slice(bodyStart, end);
}

const WINDOWS_WEBVIEW2_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ' +
  '(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0';
const MACOS_WKWEVIEW_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 ' +
  '(KHTML, like Gecko) Version/17.4 Safari/605.1.15';
const IPAD_UA =
  'Mozilla/5.0 (iPad; CPU OS 17_4 like Mac OS X) AppleWebKit/605.1.15 ' +
  '(KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1';

test('tokens.css pins the UA color scheme dark on every platform', () => {
  const rootBlock = cssBlock(tokensSource, ':root');
  assert.match(rootBlock, /color-scheme:\s*dark;/);
});

test('app.html declares the color scheme before CSS loads', () => {
  assert.match(appHtmlSource, /<meta name="color-scheme" content="dark" \/>/);
});

test('app.css styles selection and Windows scrollbars without touching macOS', () => {
  assert.match(
    appCssSource,
    /::selection\s*\{[^}]*background:\s*color-mix\(in srgb, var\(--focus-ring\) 32%, transparent\)/
  );
  // The Windows scrollbar block is the last content in app.css, so slice
  // from the first windows selector to end of file — the corner rule is the
  // final block and must be inside the slice.
  const windowsBlock = appCssSource.slice(
    appCssSource.indexOf("html[data-platform='windows']")
  );
  assert.match(windowsBlock, /scrollbar-width:\s*thin;/);
  assert.match(windowsBlock, /scrollbar-color:\s*rgba\(255,\s*255,\s*255,\s*0\.18\)\s+transparent;/);
  assert.match(windowsBlock, /::-webkit-scrollbar\s*\{[^}]*width:\s*10px;/);
  assert.match(windowsBlock, /::-webkit-scrollbar-thumb\s*\{[^}]*border-radius:\s*999px;/);
  assert.match(windowsBlock, /::-webkit-scrollbar-thumb:hover\s*\{[^}]*rgba\(255,\s*255,\s*255,\s*0\.32\)/);
  assert.match(windowsBlock, /::-webkit-scrollbar-corner\s*\{[^}]*transparent/);
});

test('root layout stamps the platform onto the document element', () => {
  assert.match(layoutSource, /import\s*\{\s*platformKey\s*\}\s*from\s*'\$lib\/platform'/);
  assert.match(layoutSource, /document\.documentElement\.dataset\.platform = platformKey\(\)/);
});

test('Settings gates macOS-only rows and copy behind isMac()', () => {
  assert.match(settingsSource, /import\s*\{\s*isMac\s*\}\s*from\s*'\$lib\/platform'/);
  assert.ok(
    (settingsSource.match(/\{#if isMac\(\)\}/g) ?? []).length >= 3,
    'expected at least 3 isMac() gates (sentry toggle, permissions section, reset TCC instructions)'
  );
  // The whole Permissions section (Screen Recording/Microphone/Camera/
  // Accessibility rows) is macOS-gated: Windows has no TCC permission model.
  assert.match(
    settingsSource,
    /\{#if isMac\(\)\}\s*<section class="section">\s*<h2 class="section-title">Permissions<\/h2>/
  );
  // The tccutil copy-before-quit gate inside handleFactoryReset is macOS-only
  // too — Windows must not get tccutil text on its clipboard.
  assert.match(settingsSource, /if \(!skipCopy && isMac\(\)\)/);
  assert.match(settingsSource, /Clears Petal's identity/);
  assert.doesNotMatch(settingsSource, /this Mac's/);
});

test('reset confirm lives in an anchored popover that never shifts the layout', () => {
  assert.match(settingsSource, /\.reset-row\s*\{[^}]*align-items:\s*center;/);
  // The Reset button never moves or splits: the confirm is an absolutely
  // positioned popover that overlays the page (no row growth, no sideways
  // expansion), with Escape + focus-out + Cancel to close it.
  assert.match(settingsSource, /\.reset-actions\s*\{[^}]*position:\s*relative;/);
  assert.match(settingsSource, /\.reset-popover\s*\{[^}]*position:\s*absolute;/);
  // Petal popover surface (--popover-bg carries the shared gradient + same
  // z-index layer as DeviceSelect / RosterPopover).
  assert.match(settingsSource, /\.reset-popover\s*\{[^}]*background:\s*var\(--popover-bg\)/);
  assert.match(settingsSource, /\.reset-popover\s*\{[^}]*z-index:\s*41;/);
  assert.match(settingsSource, /role="dialog"[\s\S]*?aria-label="Confirm reset"/);
  assert.match(settingsSource, /aria-haspopup="dialog"/);
  assert.match(settingsSource, /handleResetKeydown/);
  assert.match(settingsSource, /handleResetFocusOut/);
});

test('reset popover animates with the Petal motion tokens', () => {
  // Always mounted but visibility-hidden so the open direction can be
  // measured before reveal; entrance/exit use semantic duration and distance
  // tokens (zeroed under prefers-reduced-motion by tokens.css).
  assert.match(settingsSource, /class:open=\{resetConfirmOpen\}/);
  assert.match(settingsSource, /aria-hidden=\{!resetConfirmOpen\}/);
  assert.match(settingsSource, /\.reset-popover\s*\{[^}]*visibility:\s*hidden;/);
  assert.match(settingsSource, /\.reset-popover\s*\{[^}]*transform:\s*translateY\(var\(--motion-distance\)\);/);
  assert.match(settingsSource, /\.reset-popover\.open,\s*\.reset-popover\.open\.open-above\s*\{[^}]*visibility:\s*visible;/);
  assert.match(settingsSource, /opacity var\(--motion-enter\) var\(--ease-standard\)/);
  // The destructive button gets the app-wide press-scale feedback.
  assert.match(settingsSource, /\.reset-button:active:not\(:disabled\)\s*\{[^}]*scale\(var\(--press-scale,\s*0\.96\)\)/);
});


test('Onboarding gates its permission checklist behind isMac()', () => {
  assert.match(onboardingSource, /import\s*\{\s*isMac\s*\}\s*from\s*'\$lib\/platform'/);
  assert.match(
    onboardingSource,
    /\{#if isMac\(\)\}\s*<PermissionRow[\s\S]*<PermissionRow[\s\S]*\{\/if\}/
  );
});

test('cross-platform backend commands are registered in the Windows handler', () => {
  // The Windows invoke handler is the LAST generate_handler block in lib.rs
  // (macOS's comes first); slice from it so macOS-only registrations cannot
  // satisfy these assertions.
  const windowsHandler = libRsSource.slice(
    libRsSource.lastIndexOf('invoke_handler(tauri::generate_handler![')
  );
  // Export logs + the updater work on Windows (the reveal/arch-guard are
  // platform-adapted); their absence here made the Windows UI error (the
  // updater check used to fail at every launch on Windows).
  assert.match(windowsHandler, /logging::export_logs/);
  assert.match(windowsHandler, /logging::log_updater_event/);
  assert.match(windowsHandler, /updater::check_compatible_update_available/);
  assert.match(windowsHandler, /updater::download_and_install_compatible_update/);
  // The Network Cockpit must be openable on Windows too — its absence from
  // this handler made the Connection stats button a silent no-op (the invoke
  // rejected and the window.open fallback is permission-blocked).
  assert.match(windowsHandler, /open_network_cockpit_window/);
  // Live Network Cockpit diagnostics (issue #19): the stats poller + event
  // journal are portable (LiveKit stats), so the snapshot/journal/gate
  // commands must be registered on Windows too — their absence left the
  // cockpit stuck on the seed "no live data" state.
  assert.match(windowsHandler, /diagnostics::get_network_snapshot/);
  assert.match(windowsHandler, /diagnostics::get_event_journal/);
  assert.match(windowsHandler, /diagnostics::set_cockpit_open/);
  assert.match(windowsHandler, /diagnostics::record_video_stream_state/);
  // Sentry stays macOS-only (its SDK + state are macOS-wired); the toggle is
  // hidden on Windows.
  assert.doesNotMatch(windowsHandler, /logging::set_sentry_enabled/);
  // The native corner-radius toggle must exist on Windows: pill mode flips
  // the main window between opaque-native-rounded (gallery) and the
  // transparent blur-behind window (pill).
  assert.match(windowsHandler, /windows_corner::set_main_pill_mode/);
});

test('the Network Cockpit command is async so it cannot deadlock window creation on Windows', () => {
  // wry#583 / WebView2: WebviewWindowBuilder::build() blocks the main thread
  // and deadlocks when called from a SYNC Tauri command on Windows — the new
  // window stayed blank white and the whole app froze. Tauri v2 runs sync
  // commands on the main thread, so every window-creating command must be
  // async (the window picker's command is async for the same reason).
  assert.match(
    networkCockpitRsSource,
    /#\[tauri::command\]\npub async fn open_network_cockpit_window/
  );
});

test('the Network Cockpit window builds hidden and reveals only after the page loads', () => {
  // On Windows the visible HWND paints blank white before WebView2 attaches —
  // a split-second flash on every open. Build hidden and reveal from
  // on_page_load(Finished) instead; the window picker and the main window
  // use the same hidden-build pattern.
  assert.match(networkCockpitRsSource, /\.visible\(false\)/);
  assert.match(networkCockpitRsSource, /\.on_page_load\(/);
  assert.match(networkCockpitRsSource, /PageLoadEvent::Finished/);
});

test('the Windows build runs the portable diagnostics poller and state', () => {
  // The diagnostics module was macOS-gated; the stats poller + event
  // journal read cross-platform LiveKit stats, so they are ungated while
  // the macOS-native display-stage feeds stay gated (honest nulls on
  // Windows).
  assert.match(libRsSource, /pub mod diagnostics;/);
  assert.doesNotMatch(
    libRsSource,
    /#\[cfg\(target_os = "macos"\)\]\s*\npub mod diagnostics;/
  );
  assert.match(diagnosticsRsSource, /pub fn start_for_room\(/);
  assert.doesNotMatch(
    diagnosticsRsSource,
    /#\[cfg\(target_os = "macos"\)\]\s*\npub fn start_for_room\(/
  );
  assert.match(diagnosticsRsSource, /pub async fn collect_tick\(/);
  // The poller's per-tick cross-peer publish path is portable too.
  assert.doesNotMatch(pipelineStatsRsSource, /#!\[cfg\(target_os = "macos"\)\]/);
  // DiagnosticsState must be managed in the Windows run() as well — the
  // slice from the LAST run() is the Windows one.
  const windowsRun = libRsSource.slice(libRsSource.lastIndexOf('pub fn run()'));
  assert.match(windowsRun, /\.manage\(diagnostics::DiagnosticsState::default\(\)\)/);
  // The Windows session starts the poller on join, mirroring macOS
  // session/room.rs (self-terminating via the generation counter).
  assert.match(sessionStubRsSource, /crate::diagnostics::start_for_room\(/);
  assert.match(sessionStubRsSource, /connected\.url\.clone\(\)/);
});

test('updater treats a missing Windows manifest entry as up-to-date, not an error', () => {
  // The published manifest currently carries only darwin-aarch64; without
  // this, the Windows check failed every launch with TargetsNotFound.
  assert.match(updaterRsSource, /tauri_plugin_updater::Error::TargetsNotFound/);
  assert.match(updaterRsSource, /treating as up-to-date/);
});

test('Windows changes only the outer shell radius and leaves in-app shapes shared', () => {
  // The old frosted-glass compensation rule is dead: the main window is an
  // opaque DWM window with native rounded corners in gallery mode, and the
  // pill carve-out is handled natively (set_main_pill_mode), not by a
  // body-background rule.
  assert.doesNotMatch(
    appCssSource,
    /html\[data-platform='windows'\] body:not\(\.pill-mode\)/
  );
  assert.doesNotMatch(appCssSource, /html\[data-platform='windows'\] body\s*\{/);

  const windowsShape = cssBlock(appCssSource, "html[data-platform='windows']");
  assert.match(windowsShape, /--radius-shell:\s*0px;/);
  assert.doesNotMatch(windowsShape, /--radius-(card|tile|menu|popover|control|input|chip|badge|check|pill)\s*:/);
  // All rectangular in-app surfaces consume the shared/default values from
  // tokens.css on both platforms; only the native outer shell is different.
  for (const [name, value] of [
    ['--radius-card', '16px'],
    ['--radius-tile', '16px'],
    ['--radius-menu', '20px'],
    ['--radius-popover', '14px'],
    ['--radius-control', '12px'],
    ['--radius-input', '10px'],
    ['--radius-chip', '8px'],
    ['--radius-badge', '5px'],
    ['--radius-check', '4px'],
    ['--radius-pill', '999px']
  ]) {
    assert.match(tokensSource, new RegExp(`${name}:\\s*${value.replace('.', '\\.')};`));
  }
});

test('pill mode toggles the native Windows window transparency', () => {
  // Gallery mode is an opaque DWM window (native corners); collapsing to the
  // pill flips the native window to the transparent blur-behind state so the
  // desktop shows around the capsule. macOS never invokes (isWindows gate).
  assert.match(pillWindowSource, /import\s*\{\s*isWindows\s*\}\s*from\s*'\$lib\/platform'/);
  assert.match(pillWindowSource, /COMMANDS\.setMainPillMode/);
  assert.match(pillWindowSource, /invoke\(COMMANDS\.setMainPillMode, \{\s*active: pill\s*\}\)/);
});

test('every rectangular Windows window gets the native DWM corner treatment', () => {
  // main (setup) + cockpit + picker + compositor surfaces flip to opaque with
  // DWM-native corners through one windows_corner seam; hover-tab capsule
  // pill is the deliberate exception (per-pixel alpha, DWM cannot round it).
  assert.match(libRsSource, /windows_corner::make_native_rounded\(&main_window\)/);
  assert.match(networkCockpitRsSource, /make_native_rounded\(&window\)/);
  assert.match(windowPickerRsSource, /make_native_rounded\(&win\)/);
  assert.match(windowsCompositorRsSource, /make_native_rounded\(&window\)/);
  assert.doesNotMatch(windowsHoverRsSource, /make_native_rounded\s*\(|set_window_native_mode\s*\(/);
});

test('the Windows hover pill only appears while in a meeting', () => {
  // Mirrors the macOS hover_tab::run gate (SPEC.md §4.2: sharing is an
  // in-meeting action) — outside a room the tracker hides the pill, logs the
  // suppression once, and polls at reduced cadence instead of inviting a
  // share that would fail with NotInRoom.
  assert.match(windowsHoverRsSource, /s\.is_in_room\(\)/);
  assert.match(windowsHoverRsSource, /suppressed -- not in a room \(pill only appears while in a meeting\)/);
  assert.match(windowsHoverRsSource, /in a room -- pill tracking active/);
});

test('Windows hover-pill presentation uses the shared event-driven native follower without activation', () => {
  // `.focused(false)` protects initial construction, while all repeated
  // movement/show operations belong to the shared WinEvent tracker and use
  // one direct native SetWindowPos call.
  assert.match(windowsHoverRsSource, /\.focused\(false\)/);
  assert.doesNotMatch(windowsHoverRsSource, /\.focusable\(false\)/);
  assert.match(windowsHoverRsSource, /pub\(crate\) fn reconcile_native_hover_tab\(/);
  assert.match(windowsHoverRsSource, /SetWindowPos\([\s\S]{0,700}SWP_SHOWWINDOW/);
  assert.match(windowsHoverRsSource, /SWP_NOACTIVATE/);
  assert.match(windowsHoverRsSource, /SWP_NOOWNERZORDER/);
  assert.match(windowsHoverRsSource, /SWP_NOZORDER/);
  assert.doesNotMatch(windowsHoverRsSource, /fn position_pill\(|fn apply_native_presentation\(|window\.set_size\(/);
  assert.match(windowsHoverRsSource, /crate::windows_share_overlay::start_tracker\(app\)/);
  assert.match(windowsShareOverlayRsSource, /SetWinEventHook/);
  assert.match(windowsShareOverlayRsSource, /native_event_targets_follower/);
  assert.match(windowsShareOverlayRsSource, /250/);

  // macOS already carries the equivalent nonactivating NSPanel contract; keep
  // the platform distinction explicit while this Windows path evolves.
  assert.match(libRsSource, /can_become_key_window:\s*false/);
  assert.match(libRsSource, /\.no_activate\(true\)/);
  assert.match(libRsSource, /nonactivating_panel\(\)/);
});

test('platformKey classifies real desktop and tablet UAs', () => {
  assert.equal(platformKey(WINDOWS_WEBVIEW2_UA), 'windows');
  assert.equal(platformKey(MACOS_WKWEVIEW_UA), 'macos');
  assert.equal(platformKey(IPAD_UA), 'other');
  assert.equal(platformKey(''), 'other');
  assert.equal(platformKey(), 'other'); // no navigator in this module context
});

// ---- Rendered-platform harness (structure follows transientTextTruncation.test.ts) ----

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

/** CDP response envelope — arrives untyped over the debug pipe, shaped minimally. */
interface CdpResponse {
  exceptionDetails?: {
    exception?: { description?: string };
    text?: string;
  };
  result?: { value?: unknown };
}

interface RenderedTestBrowser {
  call: (method: string, params?: Record<string, unknown>, sessionId?: string) => Promise<unknown>;
  evaluate: (sessionId: string, expression: string) => Promise<unknown>;
  stderr: () => string;
  close: () => Promise<void>;
}

function cachedChromiumCandidates(): string[] {
  const cacheRoots = [
    join(homedir(), 'Library', 'Caches', 'ms-playwright'),
    join(homedir(), '.cache', 'ms-playwright'),
    join(homedir(), 'AppData', 'Local', 'ms-playwright')
  ];
  const platformDirs =
    process.platform === 'darwin'
      ? [process.arch === 'arm64' ? 'chrome-headless-shell-mac-arm64' : 'chrome-headless-shell-mac-x64']
      : process.platform === 'linux' && process.arch === 'x64'
        ? ['chrome-headless-shell-linux64']
        : process.platform === 'win32' && process.arch === 'x64'
          ? ['chrome-headless-shell-win64']
          : [];
  const executableName = process.platform === 'win32' ? 'chrome-headless-shell.exe' : 'chrome-headless-shell';
  const candidates: string[] = [];
  for (const root of cacheRoots) {
    let entries: string[] = [];
    try {
      entries = readdirSync(root).filter((entry) => entry.startsWith('chromium_headless_shell-'));
    } catch {
      continue;
    }
    for (const entry of entries.sort().reverse()) {
      for (const platformDir of platformDirs) {
        candidates.push(join(root, entry, platformDir, executableName));
      }
    }
  }
  return candidates;
}

function renderedTestBrowser(): string {
  const candidates = [
    process.env.PETAL_CHROME_BIN,
    ...cachedChromiumCandidates(),
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/google-chrome',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser'
  ].filter((candidate): candidate is string => Boolean(candidate));
  const browser = candidates.find((candidate) => {
    try {
      accessSync(candidate, constants.X_OK);
      return true;
    } catch {
      return false;
    }
  });
  assert.ok(
    browser,
    `rendered settings-platform test requires Chromium; checked: ${candidates.join(', ')}`
  );
  return browser;
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
  const { promise: settled, resolve, reject } = Promise.withResolvers<T>();
  const timer = setTimeout(() => reject(new Error(`${label} timed out after ${timeoutMs}ms`)), timeoutMs);
  promise.then(
    (value) => {
      clearTimeout(timer);
      resolve(value);
    },
    (error) => {
      clearTimeout(timer);
      reject(error);
    }
  );
  return settled;
}

async function removeTempPath(path: string): Promise<void> {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try {
      await rm(path, { recursive: true, force: true });
      return;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EBUSY' || attempt === 11) throw error;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
}

async function launchRenderedTestBrowser(profileDir: string): Promise<RenderedTestBrowser> {
  const browserPath = renderedTestBrowser();
  const browserArgs = [
    '--headless',
    // The Settings fixture (heaviest rendered page) fatally crashes
    // single-process headless on Windows (in-process GPU shared-context
    // virtualization failure); multi-process headless loads it fine.
    '--no-zygote',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-software-rasterizer',
    '--disable-background-networking',
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--allow-file-access-from-files',
    '--force-device-scale-factor=1',
    '--no-first-run',
    '--no-default-browser-check',
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-pipe',
    '--no-startup-window'
  ];
  const command = process.platform === 'darwin' && process.arch === 'arm64' && basename(browserPath) === 'Google Chrome'
    ? '/usr/bin/arch'
    : browserPath;
  const args = command === '/usr/bin/arch' ? ['-arm64', browserPath, ...browserArgs] : browserArgs;
  const child = spawn(command, args, { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'] });
  // Register this before any shutdown signal so even an immediate process
  // exit cannot race past close()'s waiter.
  const { promise: browserExited, resolve: resolveBrowserExit } = Promise.withResolvers<void>();
  child.once('exit', () => resolveBrowserExit());
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr = `${stderr}${chunk}`.slice(-8000);
  });

  let nextId = 1;
  let buffer = Buffer.alloc(0);
  const pending = new Map<number, {
    resolve: (value: unknown) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }>();

  function rejectPending(error: Error) {
    for (const waiter of pending.values()) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
    pending.clear();
  }

  child.once('error', (error) => rejectPending(error));
  child.once('exit', (code, signal) => {
    if (pending.size > 0) {
      rejectPending(
        new Error(
          `rendered-test browser exited before replying (code=${code}, signal=${signal})\n${stderr}`
        )
      );
    }
  });

  const protocolInput = child.stdio[3];
  const protocolOutput = child.stdio[4];
  assert.ok(protocolInput && protocolOutput, 'Chromium did not expose its remote-debugging pipes');
  protocolOutput.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const delimiter = buffer.indexOf(0);
      if (delimiter < 0) break;
      const rawMessage = buffer.subarray(0, delimiter).toString();
      buffer = buffer.subarray(delimiter + 1);
      if (!rawMessage) continue;
      const message = JSON.parse(rawMessage) as {
        id?: number;
        error?: { message?: string };
        result?: unknown;
      };
      if (!message.id) continue;
      const waiter = pending.get(message.id);
      if (!waiter) continue;
      pending.delete(message.id);
      clearTimeout(waiter.timer);
      if (message.error) waiter.reject(new Error(message.error.message));
      else waiter.resolve(message.result);
    }
  });

  function call(method: string, params: Record<string, unknown> = {}, sessionId?: string): Promise<unknown> {
    const id = nextId++;
    const message: Record<string, unknown> = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    const { promise, resolve, reject } = Promise.withResolvers<unknown>();
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} timed out\n${stderr}`));
    }, 10_000);
    pending.set(id, { resolve, reject, timer });
    protocolInput.write(`${JSON.stringify(message)}\0`);
    return promise;
  }

  async function evaluate(sessionId: string, expression: string): Promise<unknown> {
    const result = (await call(
      'Runtime.evaluate',
      { expression, awaitPromise: true, returnByValue: true },
      sessionId
    )) as CdpResponse;
    if (result.exceptionDetails) {
      throw new Error(
        result.exceptionDetails.exception?.description ??
          result.exceptionDetails.text ??
          'browser evaluation failed'
      );
    }
    return result.result?.value;
  }

  return {
    call,
    evaluate,
    stderr: () => stderr,
    async close() {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill('SIGTERM');
      try {
        await withTimeout(browserExited, 3000, 'Chromium shutdown');
      } catch {
        child.kill('SIGKILL');
        await withTimeout(browserExited, 3000, 'forced Chromium shutdown');
      }
    }
  };
}

// Both platform cases force a realistic UA via Emulation.setUserAgentOverride:
// the default headless UA follows the HOST OS, so it cannot stand in for
// either platform deterministically.
const VIEWPORT = { width: 400, height: 800 };

test('Windows Settings renders without macOS copy; both platforms compute dark color-scheme', async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-settings-platform-build-'));
  const profileDir = await mkdtemp(join(tmpdir(), 'petal-settings-platform-chrome-'));
  let browser: RenderedTestBrowser | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      // `npm test` must work immediately after `npm ci`, before
      // `svelte-kit sync` has generated .svelte-kit/tsconfig.json.
      esbuild: {
        tsconfigRaw: JSON.stringify({
          compilerOptions: { target: 'ES2022', useDefineForClassFields: true }
        })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          // Standalone Vite build skips SvelteKit config, so provide the
          // browser-only virtual module used by session.svelte.
          '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          // ...and the shared-package alias (SvelteKit's kit.alias injects it
          // in the real build; this bare Vite instance needs it manually).
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: {
          input: fileURLToPath(new URL('./settings-platform.html', fixtureRoot))
        }
      }
    });

    browser = await launchRenderedTestBrowser(profileDir);
    const fixtureUrl = pathToFileURL(join(buildDir, 'settings-platform.html')).href;

    const scenarios = [
      {
        name: 'windows',
        userAgent: WINDOWS_WEBVIEW2_UA,
        // Export logs is cross-platform (the backend command is registered on
        // Windows and reveals in Explorer); only Sentry stays macOS-only.
        absent: [
          'Finder',
          'Terminal',
          'this Mac',
          'Sentry',
          'Permissions',
          'Screen Recording',
          'Accessibility'
        ],
        present: ['Reset Petal', 'Reset and quit', 'Diagnostics', 'Export logs']
      },
      {
        name: 'macos',
        userAgent: MACOS_WKWEVIEW_UA,
        absent: [],
        present: [
          'Diagnostics',
          'Export logs',
          'Terminal',
          'Sentry',
          'Permissions',
          'Screen Recording',
          'Accessibility'
        ]
      }
    ];

    for (const scenario of scenarios) {
      const { targetId } = (await browser.call('Target.createTarget', {
        url: 'about:blank',
        width: VIEWPORT.width,
        height: VIEWPORT.height
      })) as { targetId: string };
      const { sessionId } = (await browser.call('Target.attachToTarget', {
        targetId,
        flatten: true
      })) as { sessionId: string };
      await browser.call(
        'Emulation.setDeviceMetricsOverride',
        {
          width: VIEWPORT.width,
          height: VIEWPORT.height,
          deviceScaleFactor: 1,
          mobile: false,
          screenWidth: VIEWPORT.width,
          screenHeight: VIEWPORT.height,
          dontSetVisibleSize: false
        },
        sessionId
      );
      if (scenario.userAgent) {
        await browser.call(
          'Emulation.setUserAgentOverride',
          { userAgent: scenario.userAgent },
          sessionId
        );
      }
      await browser.call('Page.navigate', { url: fixtureUrl }, sessionId);

      // Real browser render — the page paints asynchronously and only signals
      // readiness through the DOM, so a deterministic fake clock cannot drive
      // this wait; poll with a short deadline instead.
      const renderDeadline = Date.now() + 10_000;
      let rendered = false;
      while (Date.now() < renderDeadline) {
        const state = (await browser.evaluate(
          sessionId,
          `({
            rendered: document.body?.dataset.settingsRendered ?? null,
            error: document.body?.dataset.settingsRenderedError ?? null
          })`
        )) as { rendered?: string | null; error?: string | null } | null;
        if (state?.error) {
          throw new Error(
            `rendered settings-platform fixture failed: ${decodeURIComponent(state.error)}`
          );
        }
        if (state?.rendered) {
          rendered = true;
          break;
        }
        const remainingMs = renderDeadline - Date.now();
        if (remainingMs > 0) {
          const { promise: frame, resolve: resolveFrame } = Promise.withResolvers<void>();
          setTimeout(resolveFrame, Math.min(50, remainingMs));
          await frame;
        }
      }
      if (!rendered) {
        throw new Error(
          `${scenario.name} settings-platform render timed out after 10000ms\n${browser.stderr()}`
        );
      }

      // innerText reflects CSS text-transform (section titles render
      // uppercase), so compare case-insensitively.
      const innerText = ((await browser.evaluate(sessionId, 'document.body.innerText')) as string).toLowerCase();
      for (const word of scenario.absent) {
        assert.ok(
          !innerText.includes(word.toLowerCase()),
          `${scenario.name} Settings must not contain "${word}"`
        );
      }
      for (const word of scenario.present) {
        assert.ok(
          innerText.includes(word.toLowerCase()),
          `${scenario.name} Settings must contain "${word}"`
        );
      }

      const colorScheme = (await browser.evaluate(
        sessionId,
        'getComputedStyle(document.documentElement).colorScheme'
      )) as string;
      assert.equal(
        colorScheme,
        'dark',
        `${scenario.name} page must compute color-scheme dark`
      );

      await browser.call('Target.closeTarget', { targetId });
    }
  } finally {
    try {
      await browser?.close();
    } finally {
      await Promise.all([
        removeTempPath(buildDir),
        removeTempPath(profileDir)
      ]);
    }
  }
});
