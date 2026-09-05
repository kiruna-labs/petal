import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// #842: a live meeting was disrupted when the network-cockpit window (which
// mounts this same root layout) re-ran app-global device seeding on mount,
// stop/restarting the live camera and hot-swapping audio devices. Asserts
// against the REAL shipped root layout source (not an extracted helper) that
// device seeding is gated to the main window, exactly like the existing
// launch-update-check guard seven lines below it.
const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const cameraSessionSource = readFileSync(
  new URL('../src-tauri/src/camera_session.rs', import.meta.url),
  'utf8'
);

test('root layout gates device seeding to the main window', () => {
  const onMountBody = layoutSource.slice(
    layoutSource.indexOf('onMount(() => {'),
    layoutSource.indexOf('// Route transitions')
  );
  assert.ok(onMountBody.length > 0, 'could not locate the onMount() body in +layout.svelte');

  // The three device-seeding calls must appear together, guarded by a
  // condition that checks the window label -- not route name alone (a
  // regex-only guard is exactly what let network-cockpit slip through:
  // it was never in the overlay-route list).
  const seedingBlockMatch = onMountBody.match(
    /if\s*\(([\s\S]*?)\)\s*\{\s*void seedAudioDevicePreferences\(\);\s*void seedCameraDevicePreference\(\);\s*void seedCameraModePreference\(\);\s*\}/
  );
  assert.ok(
    seedingBlockMatch,
    'expected a single guarded block calling all three device-seeding functions together'
  );
  const guardCondition = seedingBlockMatch![1];
  assert.match(
    guardCondition,
    /getCurrentWindow\(\)\.label === 'main'/,
    `device seeding must be gated on the main window label, guard was: ${guardCondition}`
  );
});

test('camera_session.rs no-ops set_camera_device/set_camera_prefs when the request is unchanged', () => {
  // Both commands must consult the same extracted decision function (not a
  // re-implementation) before falling through to stop_camera_publish.
  const setCameraPrefs = cameraSessionSource.slice(
    cameraSessionSource.indexOf('pub async fn set_camera_prefs'),
    cameraSessionSource.indexOf('pub async fn set_camera_device')
  );
  const setCameraDevice = cameraSessionSource.slice(
    cameraSessionSource.indexOf('pub async fn set_camera_device')
  );
  for (const [name, body] of [
    ['set_camera_prefs', setCameraPrefs],
    ['set_camera_device', setCameraDevice]
  ] as const) {
    assert.match(
      body,
      /if camera_request_is_unchanged\(/,
      `${name} must early-return via camera_request_is_unchanged before stop_camera_publish`
    );
    const noopIndex = body.indexOf('camera_request_is_unchanged(');
    const stopIndex = body.indexOf('stop_camera_publish(&state).await;');
    assert.ok(
      noopIndex >= 0 && stopIndex >= 0 && noopIndex < stopIndex,
      `${name}: the no-op check must run BEFORE stop_camera_publish, not after`
    );
  }
});

test('dead window.open network-cockpit fallbacks are removed from production routes', () => {
  const meetingPageSource = readFileSync(
    new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
    'utf8'
  );
  const hoverTabSource = readFileSync(new URL('../src/routes/hover-tab/+page.svelte', import.meta.url), 'utf8');
  for (const [name, source] of [
    ['meeting/[room]/+page.svelte', meetingPageSource],
    ['hover-tab/+page.svelte', hoverTabSource]
  ] as const) {
    assert.doesNotMatch(
      source,
      /window\.open\('\/network-cockpit'/,
      `${name} must not fall back to window.open for network-cockpit (silent no-op inside wry)`
    );
  }
});
