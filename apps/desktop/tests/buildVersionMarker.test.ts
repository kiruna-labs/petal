import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { displayBuildVersion } from '../src/lib/buildInfo.ts';

const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const settingsSource = readFileSync(
  new URL('../src/lib/components/Settings.svelte', import.meta.url),
  'utf8'
);
const rustSource = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');

test('non-release builds get a display-only dev suffix', () => {
  assert.equal(displayBuildVersion({ version: '0.6.4', isReleaseBuild: false }), '0.6.4-dev');
  assert.equal(displayBuildVersion({ version: '0.6.4', isReleaseBuild: true }), '0.6.4');
});

test('every desktop version surface uses the shared release-aware display version', () => {
  // `/` (+page.svelte) is the launch router, not a version surface: it never
  // rendered a version anywhere itself (that text lived in LaunchScreen's
  // footer), and LaunchScreen was deleted as unreachable dead code (#639 --
  // the reveal gate from #636 covers the same first-paint UX). The two real
  // version surfaces, the root layout and Settings, remain covered here.
  assert.match(layoutSource, /displayBuildVersion\(buildInfo\)/);
  assert.match(settingsSource, /displayBuildVersion\(buildInfo\)/);
});

test('release classification evaluates Developer ID evidence instead of parsing Authority output', () => {
  assert.match(rustSource, /is_release_build: current_executable_is_release_build\(\)/);
  assert.match(rustSource, /fn release_signing_requirement\(\) -> String/);
  assert.match(rustSource, /--verify/);
  assert.match(rustSource, /--strict/);
  // Bundle id and team are build-time configurable (PETAL_RELEASE_BUNDLE_ID /
  // PETAL_RELEASE_TEAM_ID) so a fork's own Developer-ID build is classified as
  // a release of itself. Assert the requirement is still evaluated against a
  // subject.OU constraint, and that Petal's values remain the defaults.
  assert.match(rustSource, /certificate leaf\[subject\.OU\] = \{RELEASE_SIGNING_TEAM_ID\}/);
  assert.match(rustSource, /option_env!\("PETAL_RELEASE_TEAM_ID"\)/);
  assert.match(rustSource, /None => "X83RP84J8Z"/);
  assert.match(rustSource, /None => "com\.petal\.app"/);
  assert.doesNotMatch(rustSource, /codesign_summary_is_release_build/);
  assert.match(rustSource, /fn current_executable_is_release_build\(\) -> bool \{\s*false/s);
});
