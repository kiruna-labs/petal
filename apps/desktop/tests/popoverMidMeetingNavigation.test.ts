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

  new Function('window', shippedNavigationSnippet('/settings'))(fakeWindow);

  assert.deepEqual(navigated, ['/settings']);
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

test('Settings Back returns to a currently joined meeting', () => {
  const handleBack = settingsSource.match(
    /async function handleBack\(\)\s*\{([\s\S]*?)\n\s*\}/
  )?.[1];
  assert.ok(handleBack, 'could not locate Settings handleBack');
  // Read the room at CLICK time. A mount-time snapshot sends Back into a room
  // the user left while Settings was open, and join_room's publish carryover
  // then silently re-enables their camera.
  assert.match(
    handleBack,
    /await currentJoinedRoom\(\)/,
    'handleBack must re-read the current room, not use a mount-time snapshot'
  );
  assert.match(handleBack, /`\/meeting\/\$\{encodeURIComponent\(room\)\}`/);
  assert.match(settingsSource, /invoke<string \| null>\(COMMANDS\.currentRoom\)/);
  // No module-level snapshot may survive: it is what goes stale.
  assert.doesNotMatch(
    settingsSource,
    /let joinedRoom = \$state/,
    'Settings must not cache the joined room in component state'
  );
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
