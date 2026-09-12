import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  LAUNCH_LOCATION_CLASSES,
  isLaunchLocationClass,
  launchRoute,
  needsRelocation,
  relocateCopy
} from '../src/lib/data/launchLocation.ts';
import { COMMANDS } from '../src/lib/ipc.ts';

// ---- the router decision (#172) ------------------------------------------------

test('#172: a returning, fully-permissioned user is still diverted for a disk-image or translocated run', () => {
  for (const location of ['disk_image', 'translocated', 'other_read_only'] as const) {
    assert.equal(
      launchRoute({ onboardingComplete: true, hasBridge: true, location, permissionsOk: true }),
      '/relocate',
      location
    );
    // And it wins over onboarding too: moving the app comes first.
    assert.equal(
      launchRoute({ onboardingComplete: false, hasBridge: true, location, permissionsOk: false }),
      '/relocate',
      location
    );
  }
});

test('#172: every other location keeps the pre-existing decision order', () => {
  for (const location of ['applications', 'user_applications', 'other', 'unbundled'] as const) {
    assert.equal(
      launchRoute({ onboardingComplete: true, hasBridge: true, location, permissionsOk: true }),
      '/main'
    );
    assert.equal(
      launchRoute({ onboardingComplete: true, hasBridge: true, location, permissionsOk: false }),
      '/onboarding'
    );
    assert.equal(
      launchRoute({ onboardingComplete: false, hasBridge: true, location, permissionsOk: true }),
      '/onboarding'
    );
  }
  // No bridge: localStorage-only fallback, never /relocate.
  assert.equal(
    launchRoute({ onboardingComplete: true, hasBridge: false, location: null, permissionsOk: false }),
    '/main'
  );
  assert.equal(
    launchRoute({ onboardingComplete: false, hasBridge: false, location: null, permissionsOk: true }),
    '/onboarding'
  );
});

test('#172: the class set is closed and the relocation predicate names exactly the read-only/ephemeral ones', () => {
  assert.deepEqual(
    LAUNCH_LOCATION_CLASSES.filter(needsRelocation),
    ['disk_image', 'translocated', 'other_read_only']
  );
  assert.equal(isLaunchLocationClass('disk_image'), true);
  assert.equal(isLaunchLocationClass('/Volumes/Petal/Petal.app'), false, 'a path is never a class');
  assert.equal(isLaunchLocationClass(undefined), false);
});

test('#172: the Rust wire values match the TypeScript class set exactly', () => {
  const rust = readFileSync(new URL('../src-tauri/src/launch_location.rs', import.meta.url), 'utf8');
  const asStr = rust.split('pub fn as_str(self)')[1].split('}')[0];
  const wire = [...asStr.matchAll(/=> "([a-z_]+)"/g)].map((m) => m[1]).sort();
  assert.deepEqual(wire, [...LAUNCH_LOCATION_CLASSES].sort());
});

test('#172: the three commands are registered on BOTH invoke_handler lists and in COMMANDS', () => {
  assert.equal(COMMANDS.launchLocationClass, 'launch_location_class');
  assert.equal(COMMANDS.openApplicationsFolder, 'open_applications_folder');
  assert.equal(COMMANDS.revealRunningBundle, 'reveal_running_bundle');
  const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
  const [macSection, otherSection] = lib.split('.invoke_handler(tauri::generate_handler![').slice(1);
  for (const section of [macSection, otherSection]) {
    assert.match(section, /launch_location::launch_location_class,/);
    assert.match(section, /launch_location::open_applications_folder,/);
    assert.match(section, /launch_location::reveal_running_bundle,/);
  }
  // The probe runs BEFORE LaunchServices registration and gates it.
  const macRun = lib.split('#[cfg(target_os = "macos")]\n#[cfg_attr(mobile, tauri::mobile_entry_point)]\npub fn run()')[1];
  assert.ok(macRun);
  const probeAt = macRun.indexOf('launch_location::probe_at_startup()');
  const repairAt = macRun.indexOf('platform::launch_services::repair_registration_if_missing()');
  const builderAt = macRun.indexOf('tauri::Builder::default()');
  assert.ok(probeAt > -1 && repairAt > -1 && builderAt > -1);
  assert.ok(probeAt < repairAt && repairAt < builderAt, 'probe, then (gated) registration, then the Builder');
  assert.match(macRun, /if launch_location\.skip_launch_services_registration\(\)/);
});

test('#172: the root route consults the location before the onboarding/main decision', () => {
  const page = readFileSync(new URL('../src/routes/+page.svelte', import.meta.url), 'utf8');
  assert.match(page, /fetchLaunchLocationClass/);
  assert.match(page, /launchRoute\(/);
  const script = page.split('<script lang="ts">')[1];
  assert.ok(
    script.indexOf('await fetchLaunchLocationClass()') < script.indexOf('await permissionsOk()'),
    'location is read before the permission recheck'
  );
});

// ---- the notice copy -------------------------------------------------------------

test('#172: relocate copy offers Finder reveal only where the bundle is visible', () => {
  assert.equal(relocateCopy('disk_image').showReveal, true);
  assert.equal(relocateCopy('translocated').showReveal, false, 'a translocated bundle lives in a hidden mount');
  assert.equal(relocateCopy('other_read_only').showReveal, false);
  for (const cls of ['disk_image', 'translocated', 'other_read_only'] as const) {
    const copy = relocateCopy(cls);
    assert.match(copy.detail, /Applications/);
    assert.ok(copy.title.length <= 40, 'the title is one line at 14px in a 400px window');
  }
});
