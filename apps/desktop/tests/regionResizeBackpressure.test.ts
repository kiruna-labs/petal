import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const sourceRoot = new URL('../src-tauri/src/', import.meta.url);
const regionSource = readFileSync(new URL('region_window.rs', sourceRoot), 'utf8');
const captureSource = readFileSync(new URL('capture.rs', sourceRoot), 'utf8');
const shareSource = readFileSync(new URL('session/share.rs', sourceRoot), 'utf8');
const windowsCaptureSource = readFileSync(new URL('windows_screen_capture.rs', sourceRoot), 'utf8');

function pumpSource(source: string): string {
  const start = source.indexOf('fn run_pump_loop(');
  const end = source.indexOf('\nfn drain_and_push(', start);
  assert.ok(start >= 0 && end > start, 'Windows capture pump boundary disappeared');
  return source.slice(start, end);
}

test('region scheduler has one shared 50ms cadence and latest-wins proof decisions', () => {
  assert.match(
    regionSource,
    /REGION_GEOMETRY_INTERVAL:\s*std::time::Duration\s*=\s*\n\s*std::time::Duration::from_millis\(50\)/
  );
  assert.match(regionSource, /pub\(crate\) enum RegionUpdateDecision/);
  assert.match(regionSource, /ApplyLatest\s*\{ generation: u64 \}/);
  assert.match(regionSource, /WaitForProof\s*\{ generation: u64 \}/);
  assert.match(regionSource, /RetryLatest\s*\{ generation: u64 \}/);
  assert.match(regionSource, /pending_elapsed\.unwrap_or_default\(\)/);
  assert.match(regionSource, /elapsed\s*<\s*REGION_PROOF_TIMEOUT/);
  assert.match(regionSource, /RetryLatest\s*\{\s*generation:\s*newest_generation,/);
});

test('macOS capture proof state cannot overwrite an unproven configuration', () => {
  assert.match(captureSource, /struct PendingRegionConfigurationState/);
  assert.match(captureSource, /configuration:\s*crate::region_window::PendingRegionConfiguration/);
  assert.match(captureSource, /started_at:\s*std::time::Instant/);
  assert.match(captureSource, /region_update_decision\(/);
  assert.match(captureSource, /RegionUpdateDecision::WaitForProof/);
  assert.match(captureSource, /RegionUpdateDecision::RetryLatest/);
  assert.match(captureSource, /region_proof_warning_is_due/);
  assert.doesNotMatch(
    captureSource,
    /pending_region_generation:\s*Arc<Mutex<Option<\(u64,\s*u32,\s*u32\)>>/
  );

  const refreshStart = captureSource.indexOf('pub fn refresh_region_source(');
  const refreshEnd = captureSource.indexOf('\n    pub fn update_stream_configuration(', refreshStart);
  assert.ok(refreshStart >= 0 && refreshEnd > refreshStart, 'capture refresh boundary disappeared');
  const refresh = captureSource.slice(refreshStart, refreshEnd);
  assert.ok(
    refresh.indexOf('region_update_decision(') < refresh.indexOf('update_configuration(&config)'),
    'ScreenCaptureKit reconfiguration must be gated before the native update call'
  );
  assert.match(
    refresh,
    /\*self\.pending_region_generation\.lock_unpoisoned\(\)\s*=\s*Some\(\s*PendingRegionConfigurationState/
  );
});

test('macOS share pump performs ROI work only on the region tick', () => {
  assert.match(
    shareSource,
    /tokio::time::interval\(\s*crate::region_window::REGION_GEOMETRY_INTERVAL\s*,?\s*\)/
  );
  assert.match(
    shareSource,
    /if is_region_share && wake == SharePumpWake::RegionTick \{/s
  );
  assert.equal(
    (shareSource.match(/refresh_region_source\(\)/g) ?? []).length,
    1,
    'the share pump must have exactly one ROI refresh call site'
  );
  const tickGate = shareSource.indexOf('if is_region_share && wake == SharePumpWake::RegionTick {');
  const refresh = shareSource.indexOf('refresh_region_source()', tickGate);
  assert.ok(tickGate >= 0 && refresh > tickGate, 'ROI refresh escaped the region-tick gate');
});

test('Windows frame arrival cannot bypass the region geometry cadence', () => {
  const pump = pumpSource(windowsCaptureSource);
  assert.match(pump, /let mut last_region_geometry_check:\s*Option<Instant>\s*=\s*None/);
  const due = pump.indexOf('region_geometry_due(last_region_geometry_check, now)');
  const refresh = pump.indexOf('region_capture_spec(token, &target, Some(previous_region.monitor))');
  assert.ok(due >= 0 && refresh > due, 'Windows ROI refresh is not behind the cadence gate');
  assert.match(pump, /last_region_geometry_check\s*=\s*Some\(now\)/);
  assert.match(pump, /if setup\.region_paused \{\s*continue;\s*\}/s);
  assert.match(
    windowsCaptureSource,
    /context\.CopySubresourceRegion\(/,
    'Windows region capture must retain GPU ROI extraction'
  );
  assert.match(
    windowsCaptureSource,
    /WDA_EXCLUDEFROMCAPTURE|SetIsBorderRequired\(system_border_required\)/,
    'Windows selector exclusion/border behavior disappeared'
  );
});

test('selector capture and lifecycle invariants remain wired', () => {
  assert.match(regionSource, /\.transparent\(true\)/);
  // Idle selectors must remain recordable. Exclusion belongs to the active
  // capture lease, not to WebviewWindow creation.
  assert.doesNotMatch(regionSource, /set_capture_exclusion\(&window\)/);
  assert.match(regionSource, /acquire_selector_capture_exclusion/);
  assert.match(regionSource, /release_selector_capture_exclusion/);
  assert.match(regionSource, /close_region_window\(app, label\)/);
  assert.match(regionSource, /const PLACEMENT_POLL:.*from_millis\(16\)/s);
});
