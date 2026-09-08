import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { meetingTeardownPlan } from '../src/lib/ipc.ts';

const rustSource = readFileSync(
  new URL('../src-tauri/src/main_window.rs', import.meta.url),
  'utf8'
);
const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const meetingSource = readFileSync(
  new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
  'utf8'
);
const meetingSessionSource = readFileSync(
  new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url),
  'utf8'
);
const settingsSource = readFileSync(
  new URL('../src/routes/settings/+page.svelte', import.meta.url),
  'utf8'
);
const popoverSource = readFileSync(
  new URL('../src/routes/menubar-popover/+page.svelte', import.meta.url),
  'utf8'
);
const mainPageSource = readFileSync(
  new URL('../src/routes/main/+page.svelte', import.meta.url),
  'utf8'
);
const galleryPageSource = readFileSync(
  new URL('../src/lib/components/Gallery.svelte', import.meta.url),
  'utf8'
);
const sessionStoreSource = readFileSync(
  new URL('../src/lib/stores/session.svelte.ts', import.meta.url),
  'utf8'
);
const settingsComponentSource = readFileSync(
  new URL('../src/lib/components/Settings.svelte', import.meta.url),
  'utf8'
);
const presenceSource = readFileSync(
  new URL('../src-tauri/src/presence.rs', import.meta.url),
  'utf8'
);

function shippedNavigationSnippet(route: string): string {
  const template = rustSource.match(
    /const NAVIGATE_JS_TEMPLATE: &str = r#"([\s\S]*?)"#;/
  )?.[1];
  assert.ok(template, 'could not locate the shipped Rust navigation JS template');
  return template.replace('__PETAL_ROUTE__', JSON.stringify(route));
}

test('native route requests prefer the shipped SvelteKit hook', () => {
  const navigated: string[] = [];
  const assigned: string[] = [];
  const fakeWindow = {
    __petalNavigate: (route: string) => navigated.push(route),
    location: { assign: (route: string) => assigned.push(route) }
  };

  new Function('window', shippedNavigationSnippet('/main'))(fakeWindow);

  assert.deepEqual(navigated, ['/main']);
  assert.deepEqual(assigned, []);
});

test('native route requests retain a cold-start location fallback', () => {
  const assigned: string[] = [];
  const fakeWindow = {
    location: { assign: (route: string) => assigned.push(route) }
  };

  new Function('window', shippedNavigationSnippet('/meeting/recent-room'))(fakeWindow);

  assert.deepEqual(assigned, ['/meeting/recent-room']);
});

test('root layout installs and removes the SvelteKit navigation hook', () => {
  assert.match(
    layoutSource,
    /import\s*\{[^}]*\bgoto\b[^}]*\}\s*from '\$app\/navigation'/,
    'goto must be imported from $app/navigation'
  );
  const onMountBody = layoutSource.slice(
    layoutSource.indexOf('onMount(() => {'),
    layoutSource.indexOf('// Route transitions')
  );
  assert.match(onMountBody, /petalWindow\.__petalNavigate\s*=\s*navigate/);
  assert.match(onMountBody, /void goto\(route\)/);
  assert.match(onMountBody, /delete petalWindow\.__petalNavigate/);
});

test('meeting teardown plan preserves a live native publish', () => {
  assert.deepEqual(meetingTeardownPlan({ stillJoined: true }), {
    releaseSelfViewPreview: true,
    stopCameraPublish: false
  });
  assert.deepEqual(meetingTeardownPlan({ stillJoined: false }), {
    releaseSelfViewPreview: true,
    stopCameraPublish: true
  });
});

test('meeting onDestroy consults the plan before stopping the camera', () => {
  const start = meetingSource.indexOf('onDestroy(() => {');
  const end = meetingSource.indexOf('\n  });', start);
  assert.ok(start >= 0 && end > start, 'could not locate meeting onDestroy body');
  const onDestroyBody = meetingSource.slice(start, end);
  const planIndex = onDestroyBody.indexOf('meetingTeardownPlan(');
  const stopIndex = onDestroyBody.indexOf('stopLocalCamera()');

  assert.ok(planIndex >= 0, 'onDestroy must call meetingTeardownPlan');
  assert.ok(stopIndex >= 0, 'onDestroy must retain the explicit-leave camera stop path');
  assert.ok(planIndex < stopIndex, 'the teardown plan must be consulted before stopLocalCamera');
  assert.doesNotMatch(
    onDestroyBody,
    /^\s*stopLocalCamera\(\);/m,
    'onDestroy must not stop the native camera publish unconditionally'
  );
  assert.match(onDestroyBody, /else releaseSelfViewPreview\(\)/);
});

test('meeting session derives stillJoined from leave intent and phase', () => {
  const accessor = meetingSessionSource.match(
    /get stillJoined\(\)\s*\{([\s\S]*?)\n\s*\},/
  )?.[1];
  assert.ok(accessor, 'MeetingSession must expose a stillJoined accessor');
  assert.match(accessor, /selfLeaveRequested/);
  assert.match(accessor, /meetingPhase/);
});

test('Settings is its own window and never routes the main webview', () => {
  // Settings used to be an in-window route reached by navigating the main
  // webview, which tore down a live /meeting/<room> route. Now every entry
  // point opens the dedicated `settings` window, and the route closes only
  // itself.
  assert.doesNotMatch(
    rustSource,
    /route == "\/settings"/,
    'main_window.rs must not allow routing the main webview to /settings'
  );
  assert.match(settingsSource, /getCurrentWindow\(\)\.close\(\)/);
  assert.doesNotMatch(
    settingsSource,
    /goto\(`\/meeting\//,
    'the Settings route must not navigate to a meeting'
  );
  assert.doesNotMatch(settingsSource, /COMMANDS\.openMainRoute/);

  const onOpenSettings = popoverSource.slice(
    popoverSource.indexOf('async function onOpenSettings()'),
    popoverSource.indexOf('async function onActivateRemoteWindow')
  );
  assert.match(onOpenSettings, /invoke\(COMMANDS\.openSettingsWindow\)/);
  assert.doesNotMatch(onOpenSettings, /openMainRoute\('\/settings'\)/);

  const handleOpenSettings = mainPageSource.match(
    /async function handleOpenSettings\(\)\s*\{([\s\S]*?)\n  \}\n/
  )?.[1];
  assert.ok(handleOpenSettings, 'could not locate main handleOpenSettings');
  assert.match(handleOpenSettings, /invoke\(COMMANDS\.openSettingsWindow\)/);

  // In-meeting entry: the Gallery "More" menu, gated on the prop so the
  // /dev harnesses that omit it render no dead row.
  assert.match(galleryPageSource, /\{#if onOpenSettings\}/);
  assert.match(meetingSource, /onOpenSettings=\{openSettingsWindow\}/);
  assert.match(meetingSource, /invoke\(COMMANDS\.openSettingsWindow\)/);
});

test('session store edits reach the other webviews', () => {
  // Each Tauri window holds its own in-memory copy of the store. Every
  // updater must commit (persist + broadcast), the listener must ignore its
  // own echo, and the snapshot must cross the event boundary as a plain
  // object -- a $state proxy cannot be structured-cloned.
  assert.match(sessionStoreSource, /function commit\(state: StoredSession\)/);
  assert.match(sessionStoreSource, /emit\(EVENTS\.sessionChanged/);
  assert.match(sessionStoreSource, /session: \$state\.snapshot\(state\)/);
  assert.match(sessionStoreSource, /event\.payload\.origin === WEBVIEW_ID\) return/);
  const updaters = sessionStoreSource.split('export function completeOnboarding')[1];
  assert.doesNotMatch(
    updaters,
    /^\s*persist\(session\);/m,
    'every store updater must go through commit(), not a bare persist()'
  );
  // The meeting roster shows the LiveKit participant name fixed at join, so
  // a rename must also reach the native side (once, from the renaming window).
  const updateIdentity = sessionStoreSource.match(
    /export function updateIdentity\([\s\S]*?\n\}/
  )?.[0];
  assert.ok(updateIdentity, 'could not locate updateIdentity');
  assert.match(updateIdentity, /invoke\(COMMANDS\.setDisplayName, \{ name \}\)/);
  assert.match(presenceSource, /RoomEvent::ParticipantNameChanged/);
});

test('Settings never previews the camera while the meeting camera is on', () => {
  // Settings is its own window now, so its getUserMedia preview competes
  // with the meeting's publish for the device. acquirePreview must consult
  // the native publish snapshot first and the publish-state event must
  // release a live preview.
  const acquire = settingsComponentSource.match(
    /async function acquirePreview\(deviceId: string\) \{([\s\S]*?)\n  \}\n/
  )?.[1];
  assert.ok(acquire, 'could not locate acquirePreview');
  const gateIndex = acquire.indexOf('await meetingCameraActive()');
  const mediaIndex = acquire.indexOf('getUserMedia(constraints)');
  assert.ok(gateIndex >= 0, 'acquirePreview must check the meeting camera state');
  assert.ok(mediaIndex > gateIndex, 'the meeting-camera gate must run before getUserMedia');
  assert.match(settingsComponentSource, /invoke<CameraPublishStateSnapshot>\(COMMANDS\.cameraPublishState\)/);
  assert.match(settingsComponentSource, /listen<CameraPublishState>\(EVENTS\.cameraPublishState/);
});

test('layout remounts the page when only a route param changes', () => {
  // #782 regression guard: SvelteKit REUSES +page.svelte across a param-only
  // change on the same route id, so /meeting/A -> /meeting/B would swap
  // page.params.room without re-running the meeting route's onMount and would
  // never join B. The full reload this change replaced hid that.
  assert.match(
    layoutSource,
    /const routeRemountKey = \$derived\(page\.url\.pathname\)/,
    'the layout must derive a remount key from the full pathname'
  );
  const renders = layoutSource.match(/\{@render children\(\)\}/g) ?? [];
  assert.ok(renders.length > 0, 'layout must render its children');
  const keyed = layoutSource.match(/\{#key routeRemountKey\}\s*\{@render children\(\)\}\s*\{\/key\}/g) ?? [];
  assert.equal(
    keyed.length,
    renders.length,
    `every {@render children()} must sit inside {#key routeRemountKey} (${keyed.length}/${renders.length} keyed)`
  );
});

test('an in-flight join cannot resurrect the gallery bridge after teardown', () => {
  // A client-side navigation unmounts the route without killing the JS
  // context, so join()'s continuation outlives dispose(). Unguarded it
  // connects a second hidden bridge participant that nothing disconnects.
  assert.match(
    meetingSessionSource,
    /function dispose\(\)\s*\{\s*disposed = true;/,
    'dispose() must record that the session is gone'
  );
  const join = meetingSessionSource.match(
    /async function join\(\): Promise<boolean> \{([\s\S]*?)\n  \}/
  )?.[1];
  assert.ok(join, 'could not locate join()');
  const guardIndex = join.indexOf('if (disposed) return false;');
  const bridgeIndex = join.indexOf('startGalleryBridge()');
  assert.ok(guardIndex >= 0, 'join() must bail out when the session was disposed mid-flight');
  assert.ok(bridgeIndex >= 0, 'join() must still start the gallery bridge on the live path');
  assert.ok(
    guardIndex < bridgeIndex,
    'the disposed guard must run BEFORE startGalleryBridge, or the bridge still leaks'
  );
});

test('Open Petal remains show-only', () => {
  const start = popoverSource.indexOf('async function onOpenMainWindow()');
  const end = popoverSource.indexOf('function onOpenSettings()', start);
  assert.ok(start >= 0 && end > start, 'could not locate onOpenMainWindow');
  const onOpenMainWindow = popoverSource.slice(start, end);
  assert.match(onOpenMainWindow, /invoke\(COMMANDS\.showMainWindow\)/);
  assert.doesNotMatch(onOpenMainWindow, /openMainRoute\('\/main'\)/);
});
