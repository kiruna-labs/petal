import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const windowSource = readFileSync(
  new URL('../src-tauri/src/window_source.rs', import.meta.url),
  'utf8'
);
const sessionStub = readFileSync(
  new URL('../src-tauri/src/session_stub.rs', import.meta.url),
  'utf8'
);
const browserUrl = readFileSync(
  new URL('../src-tauri/src/browser_url.rs', import.meta.url),
  'utf8'
);
const captureModule = readFileSync(
  new URL('../src-tauri/src/windows_screen_capture.rs', import.meta.url),
  'utf8'
);
const compositorModule = readFileSync(
  new URL('../src-tauri/src/windows_compositor.rs', import.meta.url),
  'utf8'
);
const surfaceRoute = readFileSync(
  new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
  'utf8'
);
const registry = readFileSync(
  new URL('../src-tauri/src/windows_capture_target.rs', import.meta.url),
  'utf8'
);
const subscriber = readFileSync(
  new URL('../src-tauri/src/transport/subscriber.rs', import.meta.url),
  'utf8'
);
const ipcTs = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');

const windowsInvokeHandler = lib.slice(lib.indexOf('pub fn run() {', lib.indexOf('#[cfg(not(target_os = "macos"))]')));

test('Windows invoke handler registers real share commands, not unsupported stubs', () => {
  assert.match(
    windowsInvokeHandler,
    /session::share_window/,
    'the real share_window toggle must be registered on Windows'
  );
  assert.match(
    windowsInvokeHandler,
    /session::shared_window_ids/,
    'shared_window_ids must be registered on Windows'
  );
  assert.doesNotMatch(
    windowsInvokeHandler,
    /unsupported_media::share_window|unsupported_media::shared_window_ids/,
    'the unsupported_media stubs must be gone from the Windows handler'
  );
});

test('Windows window enumeration implements displays + windows with kind-aware tokens', () => {
  assert.doesNotMatch(
    windowSource,
    /Window enumeration is not implemented for Windows yet/,
    'the old unsupported-message must be removed'
  );
  assert.match(
    windowSource,
    /register_display\(hmonitor\.0 as usize\)/,
    'display enumeration must register each monitor'
  );
  assert.match(
    windowSource,
    /windows_capture_target::register\(hwnd\.0 as usize, pid\)/,
    'window enumeration must register each HWND'
  );
  // ShareableSourceKind, not TargetKind (#660): TargetKind is a SEPARATE type
  // owned by windows_capture_target.rs for the internal capture-target
  // registry (resolving a picker-chosen id back to an HWND/HMONITOR); it is
  // never referenced from window_source.rs. ShareableSourceKind is the type
  // actually serialized to the frontend on the ShareableWindow.kind field
  // (apps/desktop/src/lib/ipc.ts: `kind?: 'window' | 'display'`), on both
  // macOS and Windows -- this was a stale test expectation, not a code gap.
  assert.match(
    windowSource,
    /kind: Some\(ShareableSourceKind::Display\)/,
    'display entries must carry the Display kind'
  );
  assert.match(
    windowSource,
    /kind: Some\(ShareableSourceKind::Window\)/,
    'window entries must carry the Window kind'
  );
});

test('Windows thumbnails use the WGC one-shot capture path', () => {
  assert.match(
    windowSource,
    /capture_one_shot\(\s*window_id,\s*std::time::Duration::from_secs\(3\),?\s*\)/,
    'the non-macOS thumbnail arm must call windows_screen_capture::capture_one_shot'
  );
});

test('Windows share session wires publish_window_at + push_frame into ActiveShare', () => {
  assert.match(sessionStub, /struct ActiveShare \{/);
  assert.match(
    sessionStub,
    /\.publish_window_at\(\s*width,\s*height,\s*crate::transport::publisher::ShareQuality::Full,\s*Some\(token\),?\s*\)/,
    'shares must publish at ShareQuality::Full under petal-window-<token>'
  );
  assert.match(
    sessionStub,
    /published\s*\.push_frame\(&captured, frame\.capture_wall_time_us\)/,
    'the frame pump must push CapturedFrame::Bgra payloads'
  );
  assert.match(sessionStub, /MAX_CONCURRENT_SHARES: usize = 4/);
});

test('Windows URL extraction selects the browser address field, not page URLs', () => {
  assert.match(browserUrl, /UIA_EditControlTypeId/);
  assert.match(browserUrl, /FindAll\(TreeScope_Descendants/);
  assert.match(browserUrl, /address_bar_candidate_score/);
  assert.match(
    browserUrl,
    /CurrentBoundingRectangle[\s\S]{0,500}window\.top \+ 180/,
    'unnamed Chrome omnibox candidates must be constrained to browser chrome'
  );
  assert.match(
    browserUrl,
    /let score = metadata_score\.max\(geometry_score\);[\s\S]{0,500}privacy_minimized_openable_url\(&name\)/,
    'CurrentName URL fallback must remain behind the address-bar gate'
  );
});

test('Windows browser shares extract URLs asynchronously after border startup', () => {
  const shareStart = sessionStub.slice(
    sessionStub.indexOf('pub(crate) async fn start_share_token'),
    sessionStub.indexOf('fn start_share_url_refresh')
  );
  assert.match(
    browserUrl,
    /try_to_get_url_from_underlying_window/,
    'the Windows extractor must use the target-aware accessibility core'
  );
  assert.match(
    shareStart,
    /let source_url: Option<String> = None;/,
    'share startup must begin without waiting for optional URL metadata'
  );
  assert.doesNotMatch(
    shareStart,
    /url_for_windows_target\(target\)\.await/,
    'URL extraction must not block overlay or WGC startup'
  );
  assert.ok(
    shareStart.indexOf('create_share_overlay(') < shareStart.indexOf('TargetCaptureSession::start('),
    'the visible border must be prepared before capture starts'
  );
  assert.match(
    shareStart,
    /start_share_url_refresh\([\s\S]{0,300}source_url\.clone\(\)/,
    'the existing refresh task must remain responsible for late URL metadata'
  );
  assert.match(
    sessionStub,
    /\.set_shared_window_info\([\s\S]{0,180}source_url\.clone\(\),/,
    'the optional URL must continue through the existing metadata publication call'
  );
});

test('Windows URL extraction gates accessibility work by browser executable', () => {
  assert.match(
    browserUrl,
    /owner_process_id\(\)/,
    'URL extraction must identify the selected target process'
  );
  assert.match(
    browserUrl,
    /is_supported_windows_browser_executable/,
    'URL extraction must reject non-browser processes before accessibility traversal'
  );
  assert.match(
    browserUrl,
    /process_exe_path\(pid\)\?[\s\S]{0,180}is_supported_windows_browser_executable[\s\S]{0,180}initialize_com\(\)/,
    'process identity must gate COM and accessibility work'
  );
  const urlRefresh = sessionStub.slice(
    sessionStub.indexOf('fn start_share_url_refresh'),
    sessionStub.indexOf('/// Build the wire')
  );
  assert.match(
    urlRefresh,
    /windows_target_supports_url_extraction\(target\)\.await[\s\S]{0,100}return;/,
    'non-browser windows must leave the refresh task instead of polling forever'
  );
});

test('Windows receiver forwards URL metadata on subscribe and refresh', () => {
  assert.match(
    subscriber,
    /shared_window_url_from_metadata\([\s\S]{0,120}window_id/,
    'TrackSubscribed must decode the existing window URL metadata'
  );
  assert.match(
    subscriber,
    /ParticipantMetadataChanged[\s\S]{0,3000}shared_window_url_from_metadata/,
    'metadata refresh must decode URL updates and removals'
  );
  assert.match(
    subscriber,
    /windows_compositor::create_window\([\s\S]{0,260}source_url/,
    'initial URL metadata must reach Windows surface creation'
  );
  assert.match(
    subscriber,
    /windows_compositor::update_window_metadata\([\s\S]{0,320}source_url/,
    'late URL metadata must reach Windows surface refresh'
  );
  assert.match(
    compositorModule,
    /source_url: Option<&str>/,
    'Windows surface routing must accept an optional URL'
  );
  assert.match(
    compositorModule,
    /privacy_minimized_openable_url[\s\S]{0,300}route\.push_str\("&url="\)/,
    'Windows must add only sanitized URLs to the surface route'
  );
  assert.match(
    compositorModule,
    /window\.location\.replace\(window\.location\.pathname \+ nextSearch\)/,
    'metadata refresh must replace the route and remove a stale URL'
  );
  assert.match(
    compositorModule,
    /__petalRemoteControlMode/,
    'mode-only metadata refreshes must update the live surface without navigation'
  );
});

test('Windows surface preserves the active controller while control mode metadata changes', () => {
  assert.match(surfaceRoute, /let controlMode = \$state/);
  assert.match(surfaceRoute, /controlMode = value === 'fullControl'/);
  assert.match(surfaceRoute, /delete surfaceWindow\.__petalRemoteControlMode/);
});

test('Windows browser share refreshes changed URLs and stops the poller', () => {
  assert.match(sessionStub, /SHARE_URL_REFRESH_INTERVAL/);
  assert.match(sessionStub, /start_share_url_refresh\(/);
  assert.match(
    sessionStub,
    /if next_url == current_url\s*\{\s*continue;/,
    'unchanged URLs must not republish metadata every tick'
  );
  assert.match(
    sessionStub,
    /set_shared_window_info\([\s\S]{0,360}next_url\.clone\(\)/,
    'a changed URL must replace the existing window metadata'
  );
  assert.match(
    sessionStub,
    /if let Some\(url_refresh\) = (?:share\.)?url_refresh\s*\{\s*url_refresh\.abort\(\);/,
    'stopping a share must stop URL polling'
  );
});

test('Windows receiver compositor exists with the frontend command surface', () => {
  assert.match(compositorModule, /#!\[cfg\(target_os = "windows"\)\]/);
  assert.match(compositorModule, /compositor_list_windows/);
  assert.match(compositorModule, /compositor_hide_window/);
  assert.match(compositorModule, /compositor_activate_window/);
  assert.match(compositorModule, /struct RemoteWindowSummary \{/);
  assert.match(compositorModule, /enum Command \{/);
  assert.match(compositorModule, /Command::RemoveAll/);
});

test('Windows compositor feed consumes the connect-time events and keeps windows on republish', () => {
  assert.match(
    subscriber,
    /on_forced_disconnect: tokio::sync::mpsc::UnboundedSender<\(\)>,?\n\)/,
    'the Windows feed must take the forced-disconnect fan-out'
  );
  assert.match(
    subscriber,
    /window_id_from_track_name\(&track_name\)/,
    'the feed must select only exact petal-window-<id> tracks'
  );
  assert.match(
    subscriber,
    /window kept with frozen frame/,
    'TrackUnsubscribed must keep the compositor window (republish survival)'
  );
  // #700 correction: this assertion previously checked for the literal
  // string "removing compositor window", which `14fe9131` (2026-08-06,
  // "fix(windows): receiver holds the last frame across republish and crops
  // letterbox bars") deliberately removed -- that commit is exactly what
  // gave this TEST's own title ("...keeps windows on republish") a real
  // implementation: TrackUnpublished no longer unconditionally tears the
  // window down, it now asks `resolve_teardown` whether the SFU still holds
  // a replacement publication (macOS #627/#631 parity) and only removes the
  // window on `TeardownDecision::RemoveWindow`. The old assertion was
  // testing pre-parity behavior this test's own title never actually meant.
  //
  // Two things pinned now instead of one stale string: the terminal
  // RemoveWindow branch (unconditional removal is gone), AND -- previously
  // uncovered by this test despite its title -- that a republish is
  // actually held rather than torn down, which is the whole point #627/#631
  // exist for.
  assert.match(
    subscriber,
    /TeardownDecision::RemoveWindow => \{[\s\S]{0,300}crate::windows_compositor::remove_window\(/,
    'TrackUnpublished must resolve via resolve_teardown and only remove the window on the ' +
      'terminal RemoveWindow decision, not unconditionally'
  );
  assert.match(
    subscriber,
    /TeardownDecision::HoldForReplacement\s*\n\s*\| TeardownDecision::HoldForTransientUnsubscribe => \{/,
    'a republish (SFU still holds a replacement publication) must be held, not torn down and ' +
      'recreated -- this is the actual "keeps windows on republish" this test is named for'
  );
});

test('Windows unified capture-target registry carries a kind in the token space', () => {
  assert.match(registry, /pub\(crate\) enum TargetKind \{/);
  assert.match(registry, /kind: TargetKind/);
  assert.match(registry, /fn register_display/);
  assert.match(registry, /display_ordinal/);
});

test('Frontend ShareableWindow interface carries the kind field', () => {
  assert.match(
    ipcTs,
    /kind\?: 'window' \| 'display'/,
    'the picker needs the kind to render display cards'
  );
});

test('Windows DPI awareness is declared for physical-pixel capture', () => {
  assert.match(
    lib,
    /SetProcessDpiAwarenessContext\(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2\)/,
    'GetWindowRect/EnumDisplayMonitors/WGC must see physical pixels'
  );
});
