import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  COMMANDS,
  formatRemoteWindowHeaderTitle,
  remoteWindowSourceLabel
} from '../src/lib/ipc.ts';

test('remote window header formats human labels with the owner', () => {
  assert.equal(formatRemoteWindowHeaderTitle('Sublime', 'Bob'), 'Sublime by Bob');
  assert.equal(
    formatRemoteWindowHeaderTitle('  Example & Specs — Chrome  ', '  Ada  '),
    'Chrome by Ada'
  );
});

test('remote window header hides raw Petal track names from the title', () => {
  assert.equal(remoteWindowSourceLabel('petal-window'), 'Shared window');
  assert.equal(remoteWindowSourceLabel('petal-window-57'), 'Shared window');
  assert.equal(remoteWindowSourceLabel('petal-window-capture'), 'Shared window');
  assert.equal(remoteWindowSourceLabel('PETAL-WINDOW-57'), 'Shared window');
  assert.equal(remoteWindowSourceLabel('petal-window-57 — petal-window'), 'Shared window');
  assert.equal(formatRemoteWindowHeaderTitle('petal-window-57', 'Bob'), 'Shared window by Bob');
  assert.equal(formatRemoteWindowHeaderTitle('petal-camera-bob', ''), 'Camera by Someone');
});

test('remote window header prefers parsed app names over raw track fallbacks', () => {
  assert.equal(remoteWindowSourceLabel('petal-window-57 — Sublime'), 'Sublime');
  assert.equal(remoteWindowSourceLabel('Untitled — '), 'Untitled');
  assert.equal(
    formatRemoteWindowHeaderTitle('petal-window-57 — Sublime', 'Bob'),
    'Sublime by Bob'
  );
});

test('remote window hide command is part of the IPC registry', () => {
  assert.equal(COMMANDS.compositorHideWindow, 'compositor_hide_window');
  assert.equal(COMMANDS.compositorFitToSource, 'compositor_fit_to_source');
  assert.equal(COMMANDS.compositorListWindows, 'compositor_list_windows');
  assert.equal(COMMANDS.compositorActivateWindow, 'compositor_activate_window');
  assert.equal(COMMANDS.compositorToggleDebugPanel, 'compositor_toggle_debug_panel');
  assert.equal(COMMANDS.compositorWindowDebugStats, 'compositor_window_debug_stats');
});

test('remote compositor windows keep resize zones and cursors without corner indicators', () => {
  assert.equal(COMMANDS.compositorBeginResize, 'compositor_begin_resize');
  assert.equal(COMMANDS.compositorResizeWindow, 'compositor_resize_window');
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );

  assert.match(surfaceRoute, /beginCompositorResizeDrag\(event, windowId, ownerIdentity, direction\)/);
  assert.match(surfaceRoute, /class="resize-zone resize-n"/);
  assert.match(surfaceRoute, /class="resize-zone resize-nw"/);
  assert.match(controlRoute, /class="resize-zone resize-s"/);
  assert.match(controlRoute, /class="resize-zone resize-se"/);
  assert.match(controlRoute, /class="resize-zone resize-sw"/);
  assert.match(surfaceRoute, /\.resize-nw\s*\{[\s\S]*cursor:\s*nwse-resize;/);
  assert.match(surfaceRoute, /\.resize-ne[\s\S]*cursor:\s*nesw-resize;/);
  assert.match(controlRoute, /\.resize-se,[\s\S]*cursor:\s*nwse-resize;/);
  assert.match(controlRoute, /\.resize-sw\s*\{[\s\S]*cursor:\s*nesw-resize;/);
  assert.doesNotMatch(surfaceRoute, /\.resize-n[ew]::after/);
  assert.doesNotMatch(controlRoute, /\.resize-s[ew]::after/);
  assert.doesNotMatch(controlRoute, /\.resize-grip::before/);
  assert.match(controlRoute, /beginCompositorResizeDrag\(event, windowId, ownerIdentity, direction\)/);
  assert.match(compositor, /pub async fn compositor_begin_resize\(/);
  assert.match(compositor, /pub fn compositor_resize_window\(/);
  assert.match(compositor, /fn resized_frame_from_drag\(/);
});

test('remote window chrome accepts first mouse for one-gesture header drag', () => {
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );
  assert.match(compositor, /\.accept_first_mouse\(true\)/);
  assert.match(
    compositor,
    /\.with_window\(\|w\| w\.decorations\(false\)\.resizable\(true\)\.accept_first_mouse\(true\)\)/
  );
});

test('remote window surface route wires traffic dots to hide and fit-to-source', () => {
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );
  assert.match(surfaceRoute, /function onHideWindow\(\)/);
  assert.match(surfaceRoute, /invoke\(COMMANDS\.compositorHideWindow, \{ windowId, ownerIdentity \}\)/);
  assert.match(surfaceRoute, /\{onHideWindow\}/);
  assert.match(surfaceRoute, /function onFitToSource\(\)/);
  assert.match(surfaceRoute, /invoke\(COMMANDS\.compositorFitToSource, \{ windowId, ownerIdentity \}\)/);
  assert.match(surfaceRoute, /\{onFitToSource\}/);
  assert.match(
    compositor,
    /pub fn compositor_hide_window\(app: AppHandle, window_id: u32, owner_identity: Option<String>\)/
  );
  assert.match(
    compositor,
    /remove_window\(\s*&app,\s*&owner_identity,\s*window_id,\s*RemoveWindowReason::ManualHide,\s*\)/
  );
});

test('#675: the Collapse feature is fully removed from remote windows', () => {
  // #675 (user decision, 2026-08-06): Collapse was removed entirely -- the
  // yellow-dot hide button is the only "get it out of the way" affordance
  // now. This is a permanent regression guard, not a stand-in for the #497
  // contract test it replaces: it must never come back on either platform or
  // in the frontend.
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );
  const windowsCompositor = readFileSync(
    new URL('../src-tauri/src/windows_compositor.rs', import.meta.url),
    'utf8'
  );
  const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');

  assert.doesNotMatch(surfaceRoute, /[Cc]ollapse/);
  assert.doesNotMatch(headerComponent, /collapse-btn|onToggleCollapse|ToggleCollapse/);
  assert.doesNotMatch(compositor, /[Cc]ollapse/);
  assert.doesNotMatch(windowsCompositor, /[Cc]ollapse/);
  assert.doesNotMatch(ipc, /[Cc]ollapse/);
});

test('remote window surface header stays transparent and height-locked to native constant', () => {
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );

  assert.match(compositor, /const HEADER_HEIGHT: f64 = 44\.0;/);
  assert.match(surfaceRoute, /:global\(html\),\s*:global\(body\)\s*{[\s\S]*background:\s*transparent !important;/);
  assert.match(surfaceRoute, /<RemoteWindowHeader/);
  assert.match(
    compositor,
    /fn reveal_remote_window_after_first_frame_on_main[\s\S]*crate::webview_transparency::apply_or_retry\(app, &win\);[\s\S]*let _ = win\.show\(\);/
  );
  assert.match(headerComponent, /\.header\s*{[\s\S]*height:\s*44px;[\s\S]*min-height:\s*44px;[\s\S]*max-height:\s*44px;/);
  assert.match(headerComponent, /padding:\s*0 14px 0 16px;/);
  assert.match(headerComponent, /identityHeaderCss\(identity\)/);
  assert.match(headerComponent, /background:\s*var\(--identity-header-bg/);
  assert.match(surfaceRoute, /colorForIdentity\(ownerIdentity \|\| ownerName\)/);
  assert.doesNotMatch(headerComponent, /accent-stripe/);
  assert.match(headerComponent, /\.header\.idle\s*{[\s\S]*height:\s*4px;/);
});

test('remote window header renders the View Control Draw switcher and debug toggle', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');

  assert.match(headerComponent, /type HeaderMode = 'view' \| 'control' \| 'draw';/);
  assert.match(ipc, /drawSend: 'draw_send'/);
  assert.match(ipc, /compositorSetDrawActive: 'compositor_set_draw_active'/);
  assert.match(ipc, /\[COMMANDS\.drawSend\]: \{ draft: DrawDraft \}/);
  assert.match(ipc, /\[COMMANDS\.compositorSetDrawActive\]: \{ windowId: number; ownerIdentity\?: string; active: boolean \}/);
  assert.match(headerComponent, /class="mode-switcher"/);
  assert.match(headerComponent, /aria-label="Remote window mode"/);
  assert.match(headerComponent, /class="active-indicator"/);
  assert.match(headerComponent, /const activeModeIndex = \$derived/);
  assert.match(headerComponent, /width:\s*86px;/);
  assert.match(headerComponent, /selectMode\('view'\)/);
  assert.match(headerComponent, /selectMode\('control'\)/);
  assert.match(headerComponent, /selectMode\('draw'\)/);
  assert.match(headerComponent, /drawActive \? 'draw'/);
  assert.match(headerComponent, /<span>View<\/span>/);
  assert.match(headerComponent, /<span>Control<\/span>/);
  assert.match(headerComponent, /<span>Draw<\/span>/);
  assert.doesNotMatch(headerComponent, /remoteControlLabel/);
  assert.doesNotMatch(headerComponent, /Requesting\.\.\./);
  assert.match(headerComponent, /class="mode-segment draw"[\s\S]*aria-label=\{drawActive \? 'Drawing on shared window' : 'Draw on shared window'\}/);
  assert.match(headerComponent, /if \(drawActive\) \{[\s\S]*onToggleDraw\?\.\(\);/);
  assert.match(headerComponent, /function onDebugClick\(\) \{[\s\S]*debugActive = !debugActive;[\s\S]*onToggleDebug\?\.\(\);/);
  assert.match(headerComponent, /class="header-btn debug-btn"/);
  // #669: bring native up to web-harness's Debug button a11y behavior --
  // aria-pressed + a Show/Hide label toggle instead of a static one.
  assert.match(headerComponent, /class="header-btn debug-btn"[\s\S]*aria-label=\{debugActionLabel\}/);
  assert.match(headerComponent, /aria-pressed=\{debugActive\}/);
  assert.match(
    headerComponent,
    /const debugActionLabel = \$derived\(debugActive \? 'Hide debug stats' : 'Show debug stats'\);/
  );
  assert.match(headerComponent, /if \(remoteControlActive && !remoteControlRequesting\) \{[\s\S]*onToggleRemoteControl\?\.\(\);/);
  assert.match(surfaceRoute, /const remoteControlAvailable = \$derived\(page\.url\.searchParams\.get\('remoteControl'\) === '1'\);/);
  assert.match(surfaceRoute, /\{remoteControlAvailable\}[\s\S]*\{onToggleRemoteControl\}/);
  // A sharer's DENIAL must hide the Control segment, not render it disabled
  // behind "Preparing...". The two states have different meanings: preparing
  // resolves on its own, denial never does. A web sharer is the common case --
  // a browser cannot inject OS input, so it publishes an explicit denial.
  assert.match(
    surfaceRoute,
    /const remoteControlDisallowed = \$derived\([\s\S]*searchParams\.get\('remoteControlDisallowed'\) === '1'/
  );
  assert.match(surfaceRoute, /\{remoteControlDisallowed\}/);
  assert.match(headerComponent, /const controlSegmentShown = \$derived\(!remoteControlDisallowed\);/);
  assert.match(headerComponent, /\{#if controlSegmentShown\}[\s\S]*class="mode-segment control"/);
  // Draw must shift into Control's slot when Control is hidden, or the sliding
  // active-indicator lands on empty space.
  assert.match(
    headerComponent,
    /activeMode === 'draw' \? \(controlSegmentShown \? 2 : 1\)/
  );
  // "Preparing" must not claim a denied window is about to become available.
  assert.match(headerComponent, /!remoteControlAvailable &&\s*\n\s*!remoteControlDisallowed &&/);
  assert.match(surfaceRoute, /function onToggleDebug\(\) \{[\s\S]*COMMANDS\.compositorToggleDebugPanel/);
  assert.match(surfaceRoute, /function onToggleDraw\(\) \{[\s\S]*COMMANDS\.remoteControlSetActive[\s\S]*setDrawActive\(next\)/);
  assert.match(surfaceRoute, /function setDrawActive\(value: boolean\) \{[\s\S]*COMMANDS\.compositorSetDrawActive/);
  assert.match(surfaceRoute, /\{onToggleDebug\}/);
  assert.match(surfaceRoute, /compositorWindowDebugStats/);
  assert.match(surfaceRoute, /setInterval\(\(\) => void refreshFreshness\(\), 1000\)/);
  assert.doesNotMatch(surfaceRoute, /\{freshnessTooltip\}/, 'freshness prop was removed with the native tooltip title');
  assert.match(surfaceRoute, /\{drawActive\}[\s\S]*\{onToggleDraw\}/);
  assert.match(controlRoute, /__petalDebugToggle/);
  assert.match(controlRoute, /__petalDrawSetActive/);
  assert.match(controlRoute, /COMMANDS\.drawSend/);
  assert.match(controlRoute, /class:draw-active=\{drawActive\}/);
  assert.match(controlRoute, /class="debug-panel"/);
});

test('remote window header relocates remote-control feedback into the status chip', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );

  assert.match(headerComponent, /const statusText = \$derived\(/);
  assert.match(headerComponent, /remoteControlRequesting[\s\S]*\? 'Requesting control'/);
  assert.match(headerComponent, /remoteControlFeedback[\s\S]*\? remoteControlFeedback/);
  assert.match(headerComponent, /\{#if statusText\}/);
  assert.match(headerComponent, /class:warning=\{remoteControlFeedbackWarning\}/);
  assert.match(headerComponent, /title=\{statusTitle\}/);
  assert.doesNotMatch(headerComponent, /remoteControlFeedback \?\? 'Control'/);
  assert.doesNotMatch(headerComponent, /sharing to 1 viewer/);
});

test('remote window header uses platform window controls and generic app avatar', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );

  // The close affordance is REMOVED entirely: a remote window is recreated
  // after being closed, so a close button would just destroy and rebuild it
  // (user decision; macOS already hid it). Windows instead gets native-style
  // minimize/maximize controls wired to the window API.
  assert.doesNotMatch(headerComponent, /traffic-close/);
  assert.doesNotMatch(headerComponent, /Close unavailable/);
  assert.match(headerComponent, /class="win-ctl win-min"/);
  assert.match(headerComponent, /class="win-ctl win-max"/);
  assert.match(headerComponent, /function onWinMinimize\(\) \{\s*\n\s*getCurrentWindow\(\)\s*\.minimize\(\)/);
  assert.match(headerComponent, /function onWinMaximize\(\) \{\s*\n\s*getCurrentWindow\(\)\s*\.toggleMaximize\(\)/);
  // The macOS traffic dots (hide + fit) remain for non-Windows hosts.
  assert.match(headerComponent, /function onTrafficHide\(\) \{\s*\n\s*onHideWindow\?\.\(\);/);
  assert.match(headerComponent, /function onTrafficFit\(\) \{\s*\n\s*onFitToSource\?\.\(\);/);
  assert.match(headerComponent, /class="traffic-dot traffic-hide"[\s\S]*onclick=\{onTrafficHide\}/);
  assert.match(headerComponent, /class="traffic-dot traffic-fit"[\s\S]*onclick=\{onTrafficFit\}/);
  assert.match(headerComponent, /\.traffic-hide\s*\{[\s\S]*background:\s*#febc2e;/);
  assert.match(headerComponent, /\.traffic-fit\s*\{[\s\S]*background:\s*#28c840;/);
  // The source-app avatar/logo was removed per user request.
  assert.doesNotMatch(headerComponent, /class="app-avatar"/);
  assert.doesNotMatch(headerComponent, /linear-gradient\(160deg, #9e86ff, #6b4fd6\)/);
  assert.doesNotMatch(headerComponent, /<Avatar/);
  // The " by " separator is an expression so Svelte preserves the leading
  // space ("Finder by Bob", not "Finderby Bob"), and .title is not flex.
  assert.match(headerComponent, /\{' by '\}\{ownerLabel\}/);
  assert.match(headerComponent, /\.title\s*\{[\s\S]*display:\s*block;/);
});

test('remote window debug panel uses exact owner and window track matching', () => {
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const debugHelper = readFileSync(
    new URL('../src/lib/data/remoteWindowDebug.ts', import.meta.url),
    'utf8'
  );
  const diagnostics = readFileSync(
    new URL('../src-tauri/src/diagnostics.rs', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );

  assert.match(controlRoute, /findRemoteWindowDebugTrack\(debugSnapshot, ownerIdentity, windowId\)/);
  assert.match(debugHelper, /track\.ownerIdentity === ownerIdentity/);
  assert.match(debugHelper, /track\.windowId === windowId/);
  assert.doesNotMatch(debugHelper, /track\.name\.includes/);
  assert.match(diagnostics, /pub owner_identity: Option<String>/);
  assert.match(diagnostics, /pub window_id: Option<u32>/);
  assert.match(diagnostics, /raw_track_name: Some\(publication\.name\(\)\)/);
  assert.match(compositor, /pub struct RemoteWindowDebugStats/);
  assert.match(compositor, /last_frame_received_ms: AtomicU64/);
  assert.match(compositor, /frames_received: AtomicU64/);
});

test('#497: small windows replace the segmented switcher with a native overflow menu', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );

  // The header's collapse ladder is container-query based since #918 and the
  // switcher may yield earlier than 470px; the #497 invariant is that BY 470px
  // the segmented switcher is gone and the overflow button has replaced it,
  // in the same breakpoint block.
  const narrowBlock = [...headerComponent.matchAll(/@container \(max-width: (\d+)px\) \{(?<body>[\s\S]*?)\n  \}/g)].find((m) =>
    /\.mode-switcher\s*\{\s*display:\s*none;/.test(m.groups?.body ?? '')
  );
  assert.ok(narrowBlock, 'no container breakpoint hides .mode-switcher');
  assert.ok(Number(narrowBlock![1]) >= 470, `switcher still shown below ${narrowBlock![1]}px; #497 needs it gone by 470px`);
  assert.match(narrowBlock!.groups!.body, /\.overflow-btn\s*\{\s*display:\s*inline-flex;/);
  assert.match(headerComponent, /aria-label="More remote window modes"/);
  assert.match(surfaceRoute, /CheckMenuItem\.new\([\s\S]*View shared window/);
  assert.match(surfaceRoute, /text: 'Request remote control'/);
  assert.match(surfaceRoute, /text: 'Draw on shared window'/);
  assert.match(surfaceRoute, /modeMenu\.popup\(new LogicalPosition/);
});

test('#497: receiver windows enforce the final compact-header breakpoint natively', () => {
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );
  assert.match(compositor, /const MIN_RESIZE_CONTENT_WIDTH: f64 = 300\.0;/);
  assert.match(compositor, /window\.set_min_size\(Some\(tauri::Size::Logical/);
});

test('#376 item 2: unavailable control reads as transient "preparing", not a flat dead end', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );

  assert.match(headerComponent, /const remoteControlPreparing = \$derived\(/);
  assert.match(headerComponent, /Preparing remote control/);
  assert.doesNotMatch(headerComponent, /Remote control unavailable for this window/);
  assert.match(headerComponent, /class:preparing=\{remoteControlPreparing\}/);
  assert.match(headerComponent, /\.mode-segment\.preparing\s*\{[\s\S]*animation:\s*preparing-pulse/);
});

test('#376 item 3: focus-loss during an active control session shows a resume cue', () => {
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );

  assert.match(controlRoute, /let hasFocus = \$state\(true\);/);
  assert.match(controlRoute, /const showFocusLostCue = \$derived\(active && !hasFocus\);/);
  // #450: focus/blur moved from DOM-element attributes to window-level
  // listeners (native-window focus loss, not DOM focus loss).
  assert.match(controlRoute, /const onWindowFocus = \(\) => \(hasFocus = true\);/);
  assert.match(controlRoute, /const onWindowBlur = \(\) => \(hasFocus = false\);/);
  assert.match(controlRoute, /window\.addEventListener\('focus', onWindowFocus\);/);
  assert.match(controlRoute, /window\.addEventListener\('blur', onWindowBlur\);/);
  assert.match(controlRoute, /\{#if showFocusLostCue\}[\s\S]*Click to resume control/);
  // Passive cue: must not block the click that's supposed to refocus.
  assert.match(controlRoute, /\.focus-lost-hint\s*\{[\s\S]*pointer-events:\s*none;/);
  // Wraps instead of truncating -- never ellipsized text (hard rule).
  assert.doesNotMatch(controlRoute.match(/\.focus-lost-hint\s*\{[^}]*\}/)?.[0] ?? '', /text-overflow/);
});

test('#376 item 4: latency chip only renders while controlling, and always labels estimates with "~"', () => {
  const headerComponent = readFileSync(
    new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
    'utf8'
  );
  const surfaceRoute = readFileSync(
    new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
    'utf8'
  );
  const debugHelper = readFileSync(
    new URL('../src/lib/data/remoteWindowDebug.ts', import.meta.url),
    'utf8'
  );

  assert.match(headerComponent, /\{#if remoteControlActive && remoteControlLatency\}/);
  assert.match(headerComponent, /class="latency-chip"/);
  assert.match(debugHelper, /export function formatGlassToGlassLatencyChip/);
  assert.match(debugHelper, /text: `~\$\{Math\.round\(track\.glassToGlassEstimateMs\)\} ms`/);
  assert.match(surfaceRoute, /formatGlassToGlassLatencyChip/);
  assert.match(surfaceRoute, /remoteControlActive \? formatGlassToGlassLatencyChip\(latencyTrack\) : null/);
  // Only polled while actively controlling -- not a standing debug feature.
  assert.match(surfaceRoute, /if \(remoteControlActive\) startLatencyPolling\(\);\s*else stopLatencyPolling\(\);/);
});

test('menubar switcher lists hidden windows and activates them', () => {
  const menubarRoute = readFileSync(
    new URL('../src/routes/menubar-popover/+page.svelte', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );
  assert.match(menubarRoute, /COMMANDS\.compositorListWindows/);
  assert.match(menubarRoute, /COMMANDS\.compositorActivateWindow/);
  assert.match(menubarRoute, /remoteWindow\.hidden \? 'Hidden' : 'Open'/);
  assert.match(compositor, /entries\.extend\(s\.windows\.iter\(\)\.map\(\|\(key, window\)\| RemoteWindowSummary/s);
  assert.match(compositor, /entries\.extend\(s\.retired\.iter\(\)\.map\(\|\(key, window\)\| RemoteWindowSummary/s);
  assert.match(compositor, /s\.retired_order\.retain\(\|stored\| stored != &key_for_main\);\s+s\.retired\.remove\(&key_for_main\)/s);
  // #678: the menubar switcher used to omit ownerIdentity, which makes
  // resolve_window_key silently no-op on an ambiguous windowId (two
  // participants sharing the same CGWindowID) -- the click would then do
  // nothing, with no error. Assert it's passed at both the handler and the
  // call site.
  assert.match(
    menubarRoute,
    /async function onActivateRemoteWindow\(windowId: number, ownerIdentity: string\)/
  );
  assert.match(
    menubarRoute,
    /invoke\(COMMANDS\.compositorActivateWindow, \{ windowId, ownerIdentity \}\)/
  );
  assert.match(
    menubarRoute,
    /onActivateRemoteWindow\(remoteWindow\.windowId, remoteWindow\.ownerIdentity\)/
  );
});

test('#678: clicking anywhere in a remote window raises it before any View/remote-control/draw mode handling', () => {
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const compositor = readFileSync(
    new URL('../src-tauri/src/compositor.rs', import.meta.url),
    'utf8'
  );
  const appkit = readFileSync(
    new URL('../src-tauri/src/platform/appkit.rs', import.meta.url),
    'utf8'
  );
  const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');

  // Ordering is the whole bug: a test that only checks the call exists would
  // pass on a broken implementation that calls it after the early returns.
  // Find onPointerDown's body and assert the raise invoke's index precedes
  // both the drawActive branch and the `if (!active) return;` branch.
  const bodyStart = controlRoute.indexOf('function onPointerDown(event: PointerEvent) {');
  assert.ok(bodyStart >= 0, 'onPointerDown not found');
  const bodyEnd = controlRoute.indexOf('function onPointerMove', bodyStart);
  assert.ok(bodyEnd > bodyStart, 'onPointerMove not found after onPointerDown');
  const body = controlRoute.slice(bodyStart, bodyEnd);

  // Tighter needle than a bare "COMMANDS.compositorRaiseWindowForClick" --
  // that string could also appear inside an explanatory comment placed
  // before the branches while the real call moves after, which would still
  // match `indexOf` and silently pass. Requiring "invoke(COMMANDS...." only
  // matches the real call expression.
  const raiseCallIndex = body.indexOf('invoke(COMMANDS.compositorRaiseWindowForClick');
  const drawActiveIndex = body.indexOf('if (drawActive)');
  const activeGuardIndex = body.indexOf('if (!active) return;');

  assert.ok(raiseCallIndex >= 0, 'raise-on-click invoke not found in onPointerDown');
  assert.ok(drawActiveIndex >= 0, 'drawActive branch not found');
  assert.ok(activeGuardIndex >= 0, '!active early return not found');
  assert.ok(
    raiseCallIndex < drawActiveIndex,
    'raise-on-click must be invoked before the drawActive branch, not after'
  );
  assert.ok(
    raiseCallIndex < activeGuardIndex,
    'raise-on-click must be invoked before the !active early return, not after'
  );

  // Left-button pointerdown only (#450: never on hover).
  assert.match(body, /if \(event\.button === 0\) \{/);

  // ownerIdentity AND keyControlChild passed at the frontend call site.
  // keyControlChild must be exactly `active` -- NOT `active || drawActive`
  // and NOT unconditionally true. #678 review finding: keying the control
  // overlay (WebviewWindow::set_focus) activates the whole app internally
  // (tao calls activateIgnoringOtherApps:YES before keying), so passing
  // true for a plain View-mode click would activate Petal on every click --
  // exactly the #356 regression this command exists to avoid. Draw mode
  // never keyed the overlay even before #678 (the old #450
  // compositorFocusControl call only fired in the `active` branch).
  assert.match(
    controlRoute,
    /invoke\(COMMANDS\.compositorRaiseWindowForClick, \{\s*windowId,\s*ownerIdentity,\s*keyControlChild: active\s*\}\)/
  );
  assert.doesNotMatch(controlRoute, /keyControlChild: (true|active \|\| drawActive)/);

  // Registered in the IPC command table with the right arg shape.
  assert.match(ipc, /compositorRaiseWindowForClick: 'compositor_raise_window_for_click'/);
  assert.match(
    ipc,
    /\[COMMANDS\.compositorRaiseWindowForClick\]: \{\s*windowId: number;\s*ownerIdentity\?: string;\s*keyControlChild: boolean;\s*\};/
  );

  // Rust side: the raise-on-click path resolves via resolve_open_window_key
  // (never resolve_window_key, which also matches retired windows) so a
  // click can never resurrect a retired/phantom window, and it never calls
  // makeKeyWindow directly (goes through raise_panel_only).
  const raiseFnMatch = compositor.match(
    /fn raise_window_for_click\(\s*app: &AppHandle,\s*window_id: u32,\s*owner_identity: Option<&str>,\s*key_control_child: bool,\s*\) \{([\s\S]*?)\n\}/
  );
  assert.ok(raiseFnMatch, 'raise_window_for_click function not found');
  const raiseFnBody = raiseFnMatch[1];
  assert.match(raiseFnBody, /resolve_open_window_key\(window_id, owner_identity\)/);
  assert.doesNotMatch(raiseFnBody, /resolve_window_key\(window_id/);
  assert.match(raiseFnBody, /raise_panel_only\(&panel\)/);
  assert.match(raiseFnBody, /order_chrome_above_panel\(&app_for_thread, &key_for_main\)/);

  // #678 review finding: `control.set_focus()` is NOT activation-free (tao
  // internally activates the app before keying) -- pin that it is only ever
  // reachable inside an `if key_control_child` gate, not unconditionally.
  const setFocusIndex = raiseFnBody.indexOf('control.set_focus()');
  assert.ok(setFocusIndex >= 0, 'control.set_focus() call not found in raise_window_for_click');
  const gateIndex = raiseFnBody.indexOf('if key_control_child {');
  assert.ok(gateIndex >= 0, 'key_control_child gate not found');
  assert.ok(
    gateIndex < setFocusIndex,
    'control.set_focus() must be gated behind `if key_control_child`, not called unconditionally \
     -- calling it unconditionally activates the app on every click (tao\'s set_focus calls \
     activateIgnoringOtherApps:YES internally before keying)'
  );

  // No CALL to activateIgnoringOtherApps was written on this path (msg_send!
  // is how this codebase's raw-FFI convention invokes an Objective-C
  // selector -- see raise_panel_and_make_key/raise_panel_only above, which
  // both use it). Both compositor.rs's and control/+page.svelte's own doc
  // comments legitimately MENTION the string in prose (explaining why
  // set_focus is gated), so assert no msg_send! invocation, not a bare
  // string search -- the latter would fail on that documentation, which is
  // correct to keep. (Svelte/TS code cannot call an AppKit selector at all,
  // so there is nothing further to check on the frontend side.)
  assert.doesNotMatch(compositor, /msg_send!\[[^\]]*activateIgnoringOtherApps/);
  assert.doesNotMatch(appkit, /raise_panel_only[\s\S]{0,400}msg_send!\[[^\]]*activateIgnoringOtherApps/);

  // raise_panel_only omits makeKeyWindow -- that's the whole point (avoids
  // stealing key status from the control child mid-click).
  const raisePanelOnlyMatch = appkit.match(
    /pub fn raise_panel_only\(window: &tauri::WebviewWindow\) -> Result<\(\), String> \{([\s\S]*?)\n\}/
  );
  assert.ok(raisePanelOnlyMatch, 'raise_panel_only function not found');
  assert.doesNotMatch(raisePanelOnlyMatch[1], /makeKeyWindow/);
});

test('remote control overlay suppresses per-key relay during IME composition and sends the composed text once (#373)', () => {
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const remoteControlRs = readFileSync(
    new URL('../src-tauri/src/remote_control.rs', import.meta.url),
    'utf8'
  );

  // onKey bails out early for both signals -- the browser's own
  // KeyboardEvent.isComposing and the compositionstart/compositionend-
  // tracked local flag (defense in depth against engines that don't set
  // isComposing reliably on every keydown/keyup of a sequence).
  assert.match(controlRoute, /if \(event\.isComposing \|\| composing\) return;/);
  // #450: composition listeners moved from DOM-element attributes to
  // window-level listeners (DOM focus no longer gates keyboard delivery).
  assert.match(controlRoute, /const onWindowCompositionStart = \(\) => onCompositionStart\(\);/);
  assert.match(controlRoute, /const onWindowCompositionEnd = \(event: CompositionEvent\) => onCompositionEnd\(event\);/);
  assert.match(controlRoute, /window\.addEventListener\('compositionstart', onWindowCompositionStart\);/);
  assert.match(controlRoute, /window\.addEventListener\('compositionend', onWindowCompositionEnd\);/);
  // The composed string is sent as the existing `text` wire kind, not a
  // stream of per-key events.
  assert.match(controlRoute, /function sendComposedText\(text: string\)/);
  assert.match(controlRoute, /kind: 'text',/);
  assert.match(controlRoute, /sendComposedText\(event\.data \?\? ''\)/);
  // The host already knows how to inject a `text` message via replay_text.
  assert.match(remoteControlRs, /fn replay_text\(message: &RemoteControlMessage, sink: &dyn InputSink\)/);
});

test('remote control pointer draft carries an optional clickCount for real double-click (#373)', () => {
  const controlRoute = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
  // #789: click_count lives in remote_control_core.rs, not remote_control.rs --
  // it moved there in the platform-neutral-core extraction for Windows support.
  // If this ever moves again, prefer asserting through a wire-contract fixture
  // (contracts/petal-contracts.json) or a serialization round-trip instead of a
  // source-file path, so the assertion survives future refactors.
  const remoteControlCoreRs = readFileSync(
    new URL('../src-tauri/src/remote_control_core.rs', import.meta.url),
    'utf8'
  );

  assert.match(controlRoute, /clickCount\?: number;/);
  assert.match(
    controlRoute,
    /clickCount: action === 'move' \? undefined : Math\.max\(1, event\.detail \|\| 1\),/
  );
  assert.match(ipc, /clickCount\?: number;/);
  assert.match(remoteControlCoreRs, /pub click_count: Option<u32>,/);
});
