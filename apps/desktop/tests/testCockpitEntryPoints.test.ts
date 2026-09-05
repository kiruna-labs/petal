import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { COMMANDS, EVENTS } from '../src/lib/ipc.ts';
import {
  COCKPIT_SCENARIOS,
  COCKPIT_TIER_OPTIONS,
  availableCockpitTierOptions,
  cockpitSelectorFromInput,
  cockpitSummaryLine,
  latestCockpitMessage
} from '../src/lib/data/testCockpit.ts';

function rustCockpitScenarios(): { id: string; tier: string }[] {
  const rust = readFileSync(
    new URL('../src-tauri/src/test_cockpit/mod.rs', import.meta.url),
    'utf8'
  );
  const tableMatch = rust.match(/const SCENARIO_TABLE: &\[ScenarioSpec\] = &\[(?<table>[\s\S]*?)\];/);
  assert.ok(tableMatch?.groups?.table, 'Rust SCENARIO_TABLE should be parseable');
  return [...tableMatch.groups.table.matchAll(/ScenarioSpec\s*\{(?<body>[\s\S]*?)\},/g)].map(
    (match) => {
      const body = match.groups?.body ?? '';
      const id = body.match(/\bid:\s*"([^"]+)"/)?.[1];
      const tier = body.match(/\btier:\s*"([^"]+)"/)?.[1];
      assert.ok(id, `Rust scenario is missing id: ${body}`);
      assert.ok(tier, `Rust scenario ${id} is missing tier`);
      return { id, tier };
    }
  );
}

test('test cockpit IPC entry points are registered in the frontend contract', () => {
  assert.equal(COMMANDS.startTestCockpit, 'start_test_cockpit');
  assert.equal(COMMANDS.cockpitStatus, 'cockpit_status');
  assert.equal(COMMANDS.cancelTestCockpit, 'cancel_test_cockpit');
  assert.equal(COMMANDS.openTestCockpitResultsFolder, 'open_test_cockpit_results_folder');
  assert.equal(COMMANDS.listTestCockpitRuns, 'list_test_cockpit_runs');
  assert.equal(COMMANDS.getTestCockpitRun, 'get_test_cockpit_run');
  assert.equal(COMMANDS.getTestCockpitArtifactDataUrl, 'get_test_cockpit_artifact_data_url');
  assert.equal(EVENTS.testProgress, 'test-progress');
});

test('test-pattern heartbeat stays cockpit-gated and privacy-safe', () => {
  const page = readFileSync(
    new URL('../src/routes/dev/test-pattern/+page.svelte', import.meta.url),
    'utf8'
  );
  const rust = readFileSync(
    new URL('../src-tauri/src/dev_test_pattern.rs', import.meta.url),
    'utf8'
  );
  const cockpit = readFileSync(
    new URL('../src-tauri/src/test_cockpit/mod.rs', import.meta.url),
    'utf8'
  );
  const app = readFileSync(new URL('../src-tauri/src/platform/appkit.rs', import.meta.url), 'utf8');

  assert.match(page, /LIVENESS_REPORT_EVERY_FRAMES/);
  assert.match(page, /invoke\('report_test_pattern_frame', \{ counter: livenessCounter \}\)/);
  assert.match(rust, /#\[cfg\(feature = "cockpit-privileged"\)\]\s*#\[tauri::command\]\s*pub fn report_test_pattern_frame/);
  assert.match(rust, /window\.label\(\).*TEST_PATTERN_DEV_LABEL/s);
  assert.match(cockpit, /INFRA-FAIL cockpit-source-not-active-or-drawing/);
  assert.match(
    cockpit,
    /for attempt in 1\.\.=8 \{[\s\S]*?toggle_after_native_test_pattern_readiness\([\s\S]*?ensure_native_test_pattern_readiness\(app, writer, scenario, window_id\)\.await[\s\S]*?toggle_share_for_window/
  );
  assert.match(app, /pub fn window_readiness/);
  assert.match(app, /pub fn activate_cockpit_source_window/);
  assert.match(app, /NSApplicationActivationPolicy::Regular/);
  assert.match(app, /ActivateAllWindows[\s\S]*ActivateIgnoringOtherApps/);
  assert.doesNotMatch(app.match(/pub fn activate_cockpit_source_window[\s\S]*/)?.[0] ?? '', /makeMainWindow/);
  assert.match(rust, /\.resizable\(true\)[\s\S]*\.min_inner_size\(TEST_PATTERN_SOURCE_WIDTH, TEST_PATTERN_SOURCE_HEIGHT\)[\s\S]*\.max_inner_size\(TEST_PATTERN_SOURCE_WIDTH, TEST_PATTERN_SOURCE_HEIGHT\)/);
  assert.match(rust, /first_counter: Option<u64>/);
  assert.match(rust, /last_counter: Option<u64>/);
  assert.match(rust, /report_sequence: u64/);
  assert.match(cockpit, /activate_then_sample_liveness_sequence/);
  assert.match(cockpit, /report_sequence > report_sequence_after_activation/);
  assert.match(cockpit, /post_activation_report/);
  assert.match(cockpit, /cockpit-source-not-keyable/);
  assert.match(cockpit, /cockpit-source-geometry-drift/);
  assert.match(cockpit, /cockpit_activation_reassert_due/);
  assert.match(cockpit, /tokio::time::timeout/);
  assert.match(cockpit, /ACTIVATION_QUEUED[\s\S]*ACTIVATION_STARTED[\s\S]*ACTIVATION_CANCELLED/);
  assert.match(cockpit, /compare_exchange/);
  assert.match(cockpit, /cockpit-main-thread-dispatch-timeout/);
  assert.match(cockpit, /cockpit-main-thread-source-missing/);
  assert.match(cockpit, /cockpit-main-thread-dispatch-superseded/);
  assert.match(cockpit, /cockpit-main-thread-dispatch-receiver-closed/);
  assert.match(cockpit, /cockpit-main-thread-dispatch-execution-failed/);
  assert.match(cockpit, /activationQueueLatencyMs/);
  assert.match(app, /ns_app_activate_requested/);
  assert.match(app, /legacy_activate_requested/);
  assert.match(app, /cockpit_activation_plan/);
  assert.match(app, /app_active_after_primary_request/);
  assert.match(app, /legacy_activate_requested && responds[\s\S]*activateIgnoringOtherApps\(true\)[\s\S]*orderFrontRegardless[\s\S]*makeKeyAndOrderFront/);
  assert.match(app, /cockpit_source_geometry_matches/);
  assert.match(cockpit, /activation_accepted/);
});

test('test cockpit selector prefers explicit scenario IDs over tier', () => {
  assert.equal(cockpitSelectorFromInput('quick', ''), 'quick');
  assert.equal(cockpitSelectorFromInput('full', 'SHARE-W2N-Q, PERM-SETUP'), 'SHARE-W2N-Q,PERM-SETUP');
  assert.equal(cockpitSelectorFromInput('soak', '  SHARE-W2N-Q  '), 'SHARE-W2N-Q');
});

test('settings cockpit tier picker only exposes tiers with available scenarios', () => {
  assert.deepEqual(COCKPIT_TIER_OPTIONS, [
    { id: 'quick', label: 'Quick' },
    { id: 'full', label: 'Full' },
    { id: 'soak', label: 'Soak' }
  ]);
  assert.deepEqual(
    availableCockpitTierOptions([
      { id: 'ONE', tier: 'quick' },
      { id: 'TWO', tier: 'quick' },
      { id: 'THREE', tier: 'stress-test' }
    ]),
    [
      { id: 'quick', label: 'Quick' },
      { id: 'stress-test', label: 'Stress Test' }
    ]
  );
});

test('settings cockpit scenario metadata stays in lockstep with Rust SCENARIO_TABLE', () => {
  assert.deepEqual(COCKPIT_SCENARIOS, rustCockpitScenarios());
});

test('test cockpit summary always includes skipped coverage', () => {
  assert.equal(
    cockpitSummaryLine({
      status: 'passed',
      passed: 3,
      failed: 0,
      skipped: [{ id: 'SOAK-W2N-STALL', reason: 'tier gate' }],
      message: 'done'
    }),
    'passed: 3 passed, 0 failed, 1 skipped'
  );
});

test('settings cockpit UI is gated by native QA build info', () => {
  const settings = readFileSync(
    new URL('../src/lib/components/Settings.svelte', import.meta.url),
    'utf8'
  );
  assert.match(settings, /buildInfo\?\.cockpitPrivileged/);
  assert.match(settings, /COMMANDS\.startTestCockpit/);
  assert.match(settings, /EVENTS\.testProgress/);
  assert.doesNotMatch(settings, /petal:\/\/test/);
});

test('test cockpit results viewer renders saved media artifacts safely', () => {
  const viewer = readFileSync(
    new URL('../src/lib/components/TestCockpitResults.svelte', import.meta.url),
    'utf8'
  );
  assert.match(viewer, /COMMANDS\.getTestCockpitArtifactDataUrl/);
  assert.match(viewer, /activeRun\?\.runId !== runId/);
  assert.match(viewer, /activeRun\?\.resultsDir !== resultsDir/);
  assert.match(viewer, /<img src=\{artifactSources\[key\]\}/);
  assert.match(viewer, /<video src=\{artifactSources\[key\]\}/);
  assert.match(viewer, /<audio src=\{artifactSources\[key\]\}/);
  assert.match(viewer, /grid-template-columns:\s*1fr/);
});

test('latest cockpit message follows progress before final status', () => {
  assert.equal(
    latestCockpitMessage(
      {
        running: false,
        summary: {
          status: 'failed',
          passed: 0,
          failed: 1,
          skipped: [],
          message: 'final'
        }
      },
      []
    ),
    'final'
  );
  assert.equal(
    latestCockpitMessage(null, [
      {
        runId: '1',
        selector: 'quick',
        phase: 'running',
        message: 'step',
        completed: 0,
        total: 1,
        skipped: []
      }
    ]),
    'step'
  );
});
