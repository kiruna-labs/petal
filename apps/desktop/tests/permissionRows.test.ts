import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const permissionRow = readFileSync(
  new URL('../src/lib/components/PermissionRow.svelte', import.meta.url),
  'utf8'
);
const onboarding = readFileSync(
  new URL('../src/lib/components/Onboarding.svelte', import.meta.url),
  'utf8'
);
const onboardingRoute = readFileSync(
  new URL('../src/routes/onboarding/+page.svelte', import.meta.url),
  'utf8'
);
const launchRoute = readFileSync(new URL('../src/routes/+page.svelte', import.meta.url), 'utf8');
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
const permissionsClient = readFileSync(
  new URL('../src/lib/data/permissions.ts', import.meta.url),
  'utf8'
);

test('denied permission rows stay calm and compact', () => {
  assert.doesNotMatch(permissionRow, /Turn this on in System Settings/);
  assert.doesNotMatch(permissionRow, /class:danger|\\.danger/);
  assert.match(permissionRow, /class:attention/);
  assert.match(permissionRow, /Open System Settings/);
});

test('onboarding permission flow keeps camera optional', () => {
  assert.equal((onboarding.match(/<PermissionRow/g) ?? []).length, 4);
  assert.doesNotMatch(onboarding, /Relaunch now/);
  assert.match(onboarding, /requiredReadyCount === 3/);
  assert.match(onboarding, /3 required ready/);
  assert.doesNotMatch(onboarding, /readyCount === 4/);

  const cameraRow = onboarding.slice(
    onboarding.indexOf('title="Camera"'),
    onboarding.indexOf('title="Accessibility"')
  );
  assert.doesNotMatch(cameraRow, /\srequired(?:\s|$)/);
  assert.match(cameraRow, /you can always join with it off/);

  const accessibilityGate = onboarding.slice(
    onboarding.indexOf('const accessibilityRowStatus'),
    onboarding.indexOf('const shellClass')
  );
  assert.doesNotMatch(accessibilityGate, /cameraStatus/);
});

test('required permission gates exclude camera', () => {
  assert.match(onboardingRoute, /const requiredPermissionsReady/);
  assert.doesNotMatch(
    onboardingRoute.slice(
      onboardingRoute.indexOf('const requiredPermissionsReady'),
      onboardingRoute.indexOf('/** Map the Rust mic/camera')
    ),
    /cameraStatus/
  );
  assert.doesNotMatch(launchRoute, /checkCamera/);
  assert.doesNotMatch(mainRoute, /checkCamera/);
});

test('privacy settings opener uses native command before plugin fallback', () => {
  assert.match(permissionsClient, /COMMANDS\.openPrivacySettings/);
  assert.match(permissionsClient, /openUrl\(SETTINGS_URLS\[which\]\)/);
  assert.match(permissionsClient, /native opener returned false, falling back/);
});

test('stale Accessibility grants get a safe guided repair and relaunch', () => {
  assert.match(permissionRow, /Remove the stale Petal row/);
  assert.match(permissionRow, /Add <code>\/Applications\/Petal\.app<\/code>, then enable it/);
  assert.match(permissionRow, /Petal could not restart\. Quit Petal/);
  assert.match(permissionRow, /\{:else if status === 'repair'\}/);

  const accessibilityHandler = onboardingRoute.slice(
    onboardingRoute.indexOf('async function handleAccessibilityAction()'),
    onboardingRoute.indexOf('async function handleRecheckPermissions()')
  );
  assert.match(accessibilityHandler, /accessibilityStatus === 'repair'/);
  assert.match(accessibilityHandler, /openPrivacySettings\('accessibility'\)/);
  assert.match(onboardingRoute, /async function handleAccessibilityRepairRestart\(\)/);
  assert.match(onboardingRoute, /restartApp\('accessibility-stale-grant-repair'\)/);
  assert.match(accessibilityHandler, /applyAccessibilityRepair\(\{ type: 'settings-opened' \}\)/);
  assert.match(onboardingRoute, /async function handleAccessibilityRecheck\(\)/);
  assert.match(onboardingRoute, /type: 'explicit-recheck', trusted: await checkAccessibility\(\)/);
  assert.doesNotMatch(accessibilityHandler, /tccutil|reset Accessibility/);
  assert.match(onboarding, /Open Accessibility Settings/);
  assert.match(permissionRow, /Recheck Accessibility/);
});
