#!/usr/bin/env node
// Unit tests for scripts/ci/e2e-gate-verdict.mjs -- the release e2e gate's
// verdict. Every loophole is tested in BOTH directions (CLAUDE.md rule 8): the
// healthy shape a real VM run produces must pass, and each degraded shape that
// used to read as green must now fail. The last block drives the CLI exactly as
// nightly-loopback.yml does, against files on disk.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import {
  EXPECTED_LOOPBACK_SKIPS,
  cockpitVerdict,
  loopbackVerdict,
  parseIdList,
} from './ci/e2e-gate-verdict.mjs';

const CLI = fileURLToPath(new URL('./ci/e2e-gate-verdict.mjs', import.meta.url));

// The two skips every healthy VM run reports, verbatim (release 0.9.32's gate).
const SKIP_23 = 'displayplacer reports fewer than two displays';
const SKIP_30 = 'host advertises legacy control (macOS by contract; docs/CONTRACTS.md) -- v2 cached-terminal replay unreachable';

function healthyReport() {
  const results = [];
  for (let id = 1; id <= 36; id += 1) {
    if (id === 23) results.push({ caseId: 23, name: 'Retina/secondary-display mapping', status: 'skip', detail: SKIP_23 });
    else if (id === 30) results.push({ caseId: 30, name: 'completed click duplicate', status: 'skip', detail: SKIP_30 });
    else results.push({ caseId: id, name: `case ${id}`, status: 'pass', detail: 'ok' });
  }
  return {
    summary: { total: 36, pass: 34, fail: 0, skip: 2, recoveries: 0, tokenlessDrops: 0, mode: 'numbered', shareReadiness: 'live-tile' },
    results,
  };
}

function withCase(report, caseId, patch) {
  const r = structuredClone(report);
  const c = r.results.find((x) => x.caseId === caseId);
  Object.assign(c, patch);
  r.summary.pass = r.results.filter((x) => x.status === 'pass').length;
  r.summary.fail = r.results.filter((x) => x.status === 'fail').length;
  r.summary.skip = r.results.filter((x) => x.status === 'skip').length;
  return r;
}

const errorsMatch = (v, re) => v.errors.some((e) => re.test(e));

// ---------------------------------------------------------------- loopback

test('loopback: a healthy VM run (34 pass, skips 23+30, exit 0) passes', () => {
  const v = loopbackVerdict({ report: healthyReport(), harnessExit: 0 });
  assert.deepEqual(v.errors, []);
  assert.equal(v.ok, true);
});

test('loopback: missing results JSON fails', () => {
  assert.equal(loopbackVerdict({ report: null, harnessExit: 0 }).ok, false);
});

test('loopback: zero cases fails', () => {
  const r = healthyReport(); r.results = []; r.summary.total = 0;
  assert.equal(loopbackVerdict({ report: r, harnessExit: 0 }).ok, false);
});

test('loopback: zero passes fails even with no failures (all skipped)', () => {
  const r = healthyReport();
  for (const c of r.results) { c.status = 'skip'; c.detail = c.detail === 'ok' ? 'AXSelectedText unavailable' : c.detail; }
  const v = loopbackVerdict({ report: r, harnessExit: 0 });
  assert.equal(v.ok, false);
  assert.ok(errorsMatch(v, /zero of 36 cases passed/));
});

test('loopback: an unquarantined fail fails; a quarantined one warns and passes', () => {
  const r = withCase(healthyReport(), 9, { status: 'fail', detail: 'typing never arrived' });
  assert.equal(loopbackVerdict({ report: r, harnessExit: 1 }).ok, false);
  const q = loopbackVerdict({ report: r, harnessExit: 1, quarantine: parseIdList('9') });
  assert.equal(q.ok, true, q.errors.join('\n'));
  assert.ok(q.warnings.some((w) => /case 9 .*QUARANTINED/.test(w)));
});

test('loopback: an AX-unavailable skip is NOT silently allowed', () => {
  const r = withCase(healthyReport(), 6, { status: 'skip', detail: 'AXSelectedText unavailable; deterministic drag assertion needs sentinel app' });
  const v = loopbackVerdict({ report: r, harnessExit: 0 });
  assert.equal(v.ok, false);
  assert.ok(errorsMatch(v, /case 6 .*unexpected skip/));
});

test('loopback: a #134 stress-case skip is not allowed (issue closed; quarantine it explicitly if needed)', () => {
  const r = withCase(healthyReport(), 33, { status: 'skip', detail: 'QUARANTINED (#134) -- the stress case could not complete: boom' });
  assert.equal(loopbackVerdict({ report: r, harnessExit: 0 }).ok, false);
  assert.equal(loopbackVerdict({ report: r, harnessExit: 0, quarantine: parseIdList('33') }).ok, true);
});

test('loopback: an allow-listed case skipping for a DIFFERENT reason fails', () => {
  const r = withCase(healthyReport(), 23, { status: 'skip', detail: 'AX visible character range unavailable' });
  assert.equal(loopbackVerdict({ report: r, harnessExit: 0 }).ok, false);
  const r30 = withCase(healthyReport(), 30, { status: 'skip', detail: 'photon sentinel exited 1' });
  assert.equal(loopbackVerdict({ report: r30, harnessExit: 0 }).ok, false);
});

test('loopback: the allow-list is exactly cases 23 and 30', () => {
  assert.deepEqual(EXPECTED_LOOPBACK_SKIPS.map((s) => s.caseId), ['23', '30']);
});

test('loopback: tokenlessDrops > 0 fails; a missing tokenlessDrops fails', () => {
  const r = healthyReport(); r.summary.tokenlessDrops = 3;
  const v = loopbackVerdict({ report: r, harnessExit: 1 });
  assert.equal(v.ok, false);
  assert.ok(errorsMatch(v, /tokenless input packet/));
  const m = healthyReport(); delete m.summary.tokenlessDrops;
  assert.equal(loopbackVerdict({ report: m, harnessExit: 0 }).ok, false);
});

test('loopback: non-zero harness exit with no quarantined failure fails', () => {
  for (const exit of [1, 2, 124, 137]) {
    const v = loopbackVerdict({ report: healthyReport(), harnessExit: exit });
    assert.equal(v.ok, false, `exit ${exit} must fail`);
    assert.ok(errorsMatch(v, new RegExp(`harness exited ${exit}`)));
  }
});

test('loopback: exit 2 (NO RESULT) fails even when a quarantined fail exists', () => {
  const r = withCase(healthyReport(), 9, { status: 'fail', detail: 'x' });
  assert.equal(loopbackVerdict({ report: r, harnessExit: 2, quarantine: parseIdList('9') }).ok, false);
});

test('loopback: a quarantined SKIP does not excuse a non-zero exit', () => {
  const r = withCase(healthyReport(), 6, { status: 'skip', detail: 'AXSelectedText unavailable' });
  assert.equal(loopbackVerdict({ report: r, harnessExit: 1, quarantine: parseIdList('6') }).ok, false);
});

test('loopback: unknown/empty harness exit fails', () => {
  assert.equal(loopbackVerdict({ report: healthyReport(), harnessExit: '' }).ok, false);
  assert.equal(loopbackVerdict({ report: healthyReport(), harnessExit: undefined }).ok, false);
});

test('loopback: inconsistent scorecard, duplicates, input-only mode and unknown status fail', () => {
  const t = healthyReport(); t.summary.total = 40;
  assert.equal(loopbackVerdict({ report: t, harnessExit: 0 }).ok, false);
  const d = healthyReport(); d.results[1].caseId = 1;
  assert.equal(loopbackVerdict({ report: d, harnessExit: 0 }).ok, false);
  const io = healthyReport(); io.summary.mode = 'input-only'; io.summary.shareReadiness = 'target-present';
  assert.equal(loopbackVerdict({ report: io, harnessExit: 0 }).ok, false);
  const u = withCase(healthyReport(), 5, { status: 'flaky' });
  assert.equal(loopbackVerdict({ report: u, harnessExit: 0 }).ok, false);
  const ns = healthyReport(); delete ns.summary;
  assert.equal(loopbackVerdict({ report: ns, harnessExit: 0 }).ok, false);
});

// ---------------------------------------------------------------- cockpit

const QUICK = ['PLUGIN-BOOT', 'SHARE-N2W-Q', 'SHARE-W2N-Q', 'SHARE-W2N-STALL', 'DRAW-N', 'CAM', 'AUD', 'TELE', 'SHARE-DESKTOP'];
const STARTED_AT = 1792198250000;
const RUN_ID = '1792198256594';

// Mirrors conclusions.rs verdict_label: proxy-evidence passes carry a suffix.
const LABEL = { pass: 'PASS', 'test-fail': 'FAIL', 'infra-fail': 'FAIL', skipped: 'SKIPPED', cancelled: 'CANCELLED' };
const conclusionEntry = (id, verdict) => ({
  scenarioId: id,
  verdict: verdict === 'pass' && id === 'DRAW-N' ? 'PASS (proxy — not content-verified)' : (LABEL[verdict] ?? verdict),
  evidenceBasis: 'x',
  detail: `${id} msg`,
});

function healthyCockpit({ verdicts = {} } = {}) {
  const lines = [{ kind: 'meta', payload: { runId: RUN_ID, selector: 'quick' } }];
  for (const id of QUICK) {
    lines.push({ kind: 'scenario-start', scenarioId: id, payload: id });
    lines.push({ kind: 'scenario-verdict', scenarioId: id, payload: { scenarioId: id, verdict: verdicts[id] ?? 'pass', message: `${id} msg` } });
  }
  lines.push({ kind: 'cleanup', payload: { passed: true } });
  lines.push({ kind: 'conclusion', payload: { status: 'complete', scenarios: QUICK.map((id) => conclusionEntry(id, verdicts[id] ?? 'pass')), notChecked: [] } });
  return lines;
}

// Exactly what the engine writes when cancel_requested is seen before
// scenario index `at` (mod.rs run loop): the earlier scenarios start and get a
// verdict, the cancelled one is pushed to outcomes with NO start/verdict line,
// the loop breaks, and cleanup + a 'complete' conclusion are still written.
function cancelledCockpit(at) {
  const lines = [{ kind: 'meta', payload: { runId: RUN_ID, selector: 'quick' } }];
  const ran = QUICK.slice(0, at);
  for (const id of ran) {
    lines.push({ kind: 'scenario-start', scenarioId: id, payload: id });
    lines.push({ kind: 'scenario-verdict', scenarioId: id, payload: { scenarioId: id, verdict: 'pass', message: `${id} msg` } });
  }
  lines.push({ kind: 'cleanup', payload: { passed: true } });
  const scenarios = [...ran.map((id) => conclusionEntry(id, 'pass')), conclusionEntry(QUICK[at], 'cancelled')];
  lines.push({ kind: 'conclusion', payload: { status: 'complete', scenarios, notChecked: [{ scenarioId: QUICK[at], reason: 'Test cockpit run cancelled' }] } });
  return lines;
}
const cockpit = (lines, extra = {}) => cockpitVerdict({ lines, runDirName: RUN_ID, startedAtMs: STARTED_AT, selector: 'quick', engineExit: 0, quarantine: parseIdList('SHARE-DESKTOP'), ...extra });

test('cockpit: a healthy quick run passes', () => {
  const v = cockpit(healthyCockpit());
  assert.deepEqual(v.errors, []);
});

test('cockpit: missing run.jsonl / meta / zero verdicts fail', () => {
  assert.equal(cockpit(null).ok, false);
  assert.equal(cockpit(healthyCockpit().filter((o) => o.kind !== 'meta')).ok, false);
  assert.equal(cockpit(healthyCockpit().filter((o) => o.kind !== 'scenario-verdict')).ok, false);
});

test('cockpit: a STALE run dir (runId before this step started) fails', () => {
  const v = cockpit(healthyCockpit(), { startedAtMs: Number(RUN_ID) + 1 });
  assert.equal(v.ok, false);
  assert.ok(errorsMatch(v, /STALE/));
  assert.equal(cockpit(healthyCockpit(), { startedAtMs: '' }).ok, false);
});

test('cockpit: run dir / runId mismatch and selector mismatch fail', () => {
  assert.equal(cockpit(healthyCockpit(), { runDirName: '1700000000000' }).ok, false);
  assert.equal(cockpit(healthyCockpit(), { selector: 'full' }).ok, false);
  assert.equal(cockpit(healthyCockpit(), { selector: 'QUICK' }).ok, true);
});

test('cockpit: a scenario that started but never got a verdict fails', () => {
  const lines = healthyCockpit().filter((o) => !(o.kind === 'scenario-verdict' && o.scenarioId === 'TELE'));
  const v = cockpit(lines);
  assert.equal(v.ok, false);
  assert.ok(errorsMatch(v, /never reached a verdict: TELE/));
});

test('cockpit: non-pass fails unless quarantined (skipped / infra-fail count as failures)', () => {
  for (const verdict of ['test-fail', 'infra-fail', 'skipped', 'cancelled']) {
    assert.equal(cockpit(healthyCockpit({ verdicts: { CAM: verdict } })).ok, false, verdict);
  }
  const q = cockpit(healthyCockpit({ verdicts: { 'SHARE-DESKTOP': 'test-fail' } }), { engineExit: 1 });
  assert.equal(q.ok, true, q.errors.join('\n'));
  assert.ok(q.warnings.some((w) => /SHARE-DESKTOP.*QUARANTINED/.test(w)));
});

test('cockpit: a failed or missing cleanup, or a missing conclusion, fails', () => {
  const failedCleanup = healthyCockpit().map((o) => (o.kind === 'cleanup' ? { kind: 'cleanup', payload: { passed: false } } : o));
  assert.equal(cockpit(failedCleanup).ok, false);
  assert.equal(cockpit(healthyCockpit().filter((o) => o.kind !== 'cleanup')).ok, false);
  assert.equal(cockpit(healthyCockpit().filter((o) => o.kind !== 'conclusion')).ok, false);
});

test('cockpit: a run cancelled mid-way fails even though every verdict written is a pass', () => {
  for (const at of [1, 3, QUICK.length - 1]) {
    for (const engineExit of [0, 1]) {
      const v = cockpit(cancelledCockpit(at), { engineExit });
      assert.equal(v.ok, false, `cancel at ${at}, exit ${engineExit} must fail`);
      assert.ok(errorsMatch(v, /CANCELLED/), v.errors.join('\n'));
      assert.ok(errorsMatch(v, new RegExp(`no verdict line: ${QUICK[at]}`)), v.errors.join('\n'));
    }
  }
  // Even with the cancelled entry's label scrubbed, the count/verdict-per-
  // listed-scenario check alone still catches it.
  const scrubbed = cancelledCockpit(3).map((o) => (o.kind === 'conclusion'
    ? { ...o, payload: { ...o.payload, scenarios: o.payload.scenarios.map((s) => ({ ...s, verdict: 'PASS' })) } } : o));
  assert.equal(cockpit(scrubbed).ok, false);
});

test('cockpit: a full run with a real-shaped conclusion still passes (no false red)', () => {
  const v = cockpit(healthyCockpit());
  assert.deepEqual(v.errors, []);
  assert.deepEqual(v.warnings, []);
});

test('cockpit: an aborted/non-complete conclusion, or one listing no scenarios, fails', () => {
  const withConclusion = (patch) => healthyCockpit().map((o) => (o.kind === 'conclusion' ? { ...o, payload: { ...o.payload, ...patch } } : o));
  for (const status of ['aborted', 'cancelled', undefined]) {
    const v = cockpit(withConclusion({ status }));
    assert.equal(v.ok, false, String(status));
    assert.ok(errorsMatch(v, /conclusion status/));
  }
  assert.equal(cockpit(withConclusion({ scenarios: [] })).ok, false);
  assert.equal(cockpit(withConclusion({ scenarios: undefined })).ok, false);
  // A verdict line for a scenario the conclusion never scored is inconsistent.
  assert.equal(cockpit(withConclusion({ scenarios: QUICK.slice(1).map((id) => conclusionEntry(id, 'pass')) })).ok, false);
});

test('cockpit: engine exit -- 0 passes; non-zero fails unless a quarantined failure explains exit 1', () => {
  assert.equal(cockpit(healthyCockpit(), { engineExit: 0 }).ok, true);
  assert.equal(cockpit(healthyCockpit(), { engineExit: '0' }).ok, true);
  for (const engineExit of [1, 2, 101, 134, 137]) {
    const v = cockpit(healthyCockpit(), { engineExit });
    assert.equal(v.ok, false, `exit ${engineExit} must fail`);
    assert.ok(errorsMatch(v, new RegExp(`engine exited ${engineExit} with no quarantined failure`)));
  }
  for (const engineExit of ['', undefined, 'x']) {
    assert.equal(cockpit(healthyCockpit(), { engineExit }).ok, false, JSON.stringify(engineExit));
  }
  const q = healthyCockpit({ verdicts: { 'SHARE-DESKTOP': 'infra-fail' } });
  const ok1 = cockpit(q, { engineExit: 1 });
  assert.equal(ok1.ok, true, ok1.errors.join('\n'));
  assert.ok(ok1.warnings.some((w) => /engine exited 1, explained by .*SHARE-DESKTOP/.test(w)));
  assert.equal(cockpit(q, { engineExit: 134 }).ok, false, 'a crash is not explained by a quarantine');
  // A quarantined SKIP does not make the engine exit 1, so it cannot excuse it.
  assert.equal(cockpit(healthyCockpit({ verdicts: { 'SHARE-DESKTOP': 'skipped' } }), { engineExit: 1 }).ok, false);
});

// Required cases (display share 37-41, #349/#353): must PASS.
function withDisplayCases(report) {
  const r = structuredClone(report);
  for (let id = 37; id <= 41; id += 1) r.results.push({ caseId: id, name: `display ${id}`, status: 'pass', detail: 'ok' });
  r.summary.total = r.results.length;
  r.summary.pass = r.results.filter((x) => x.status === 'pass').length;
  return r;
}
const REQUIRED = parseIdList('37,38,39,40,41');

test('loopback required: all required cases passing passes (no false red)', () => {
  const v = loopbackVerdict({ report: withDisplayCases(healthyReport()), harnessExit: 0, required: REQUIRED });
  assert.deepEqual(v.errors, []);
});

test('loopback required: a missing required case fails', () => {
  const r = withDisplayCases(healthyReport());
  r.results = r.results.filter((c) => c.caseId !== 39);
  r.summary.total = r.results.length;
  const v = loopbackVerdict({ report: r, harnessExit: 0, required: REQUIRED });
  assert.equal(v.ok, false);
  assert.ok(errorsMatch(v, /required loopback case 39 produced no result/));
});

test('loopback required: a required skip fails even when quarantined as a skip', () => {
  const r = withCase(withDisplayCases(healthyReport()), 41, { status: 'skip', detail: 'no geometry' });
  const v = loopbackVerdict({ report: r, harnessExit: 0, required: REQUIRED });
  assert.ok(errorsMatch(v, /required loopback case 41 .*did not pass: skip/));
  const q = loopbackVerdict({ report: r, harnessExit: 0, required: REQUIRED, quarantine: parseIdList('41') });
  assert.equal(q.ok, false);
  assert.ok(errorsMatch(q, /required loopback case 41 .*quarantine does not excuse/));
});

test('loopback required: a required fail fails, and quarantine does not excuse it', () => {
  const r = withCase(withDisplayCases(healthyReport()), 38, { status: 'fail', detail: 'marker never landed' });
  const q = loopbackVerdict({ report: r, harnessExit: 1, required: REQUIRED, quarantine: parseIdList('38') });
  assert.equal(q.ok, false);
  assert.ok(errorsMatch(q, /required loopback case 38 .*did not pass: fail .*quarantine does not excuse/));
  // Without --required the same quarantined fail still warns-and-passes.
  assert.equal(loopbackVerdict({ report: r, harnessExit: 1, quarantine: parseIdList('38') }).ok, true);
});

test('loopback required: an empty list requires nothing', () => {
  assert.equal(loopbackVerdict({ report: healthyReport(), harnessExit: 0, required: parseIdList('') }).ok, true);
});

// ---------------------------------------------------------------- CLI

function runCli(args) {
  const r = spawnSync(process.execPath, [CLI, ...args], { encoding: 'utf8' });
  return { status: r.status, out: r.stdout + r.stderr };
}

test('CLI loopback: real files, both directions, and a missing file', () => {
  const dir = mkdtempSync(join(tmpdir(), 'e2e-gate-verdict-'));
  const good = join(dir, 'good.json');
  writeFileSync(good, JSON.stringify(healthyReport()));
  const ok = runCli(['loopback', '--results', good, '--harness-exit', '0', '--quarantine', '']);
  assert.equal(ok.status, 0, ok.out);
  assert.match(ok.out, /PASS/);
  const bad = runCli(['loopback', '--results', good, '--harness-exit', '1', '--quarantine', '']);
  assert.equal(bad.status, 1, bad.out);
  assert.match(bad.out, /::error::harness exited 1/);
  const missing = runCli(['loopback', '--results', join(dir, 'nope.json'), '--harness-exit', '0']);
  assert.equal(missing.status, 1);
  const requiredMissing = runCli(['loopback', '--results', good, '--harness-exit', '0', '--required', '37,38']);
  assert.equal(requiredMissing.status, 1, requiredMissing.out);
  assert.match(requiredMissing.out, /::error::required loopback case 37 produced no result/);
  const display = join(dir, 'display.json');
  writeFileSync(display, JSON.stringify(withDisplayCases(healthyReport())));
  const requiredOk = runCli(['loopback', '--results', display, '--harness-exit', '0', '--required', '37,38,39,40,41']);
  assert.equal(requiredOk.status, 0, requiredOk.out);
  const garbage = join(dir, 'garbage.json');
  writeFileSync(garbage, '{not json');
  assert.equal(runCli(['loopback', '--results', garbage, '--harness-exit', '0']).status, 1);
});

test('CLI cockpit: real run dir, both directions, and an unknown tier', () => {
  const root = mkdtempSync(join(tmpdir(), 'e2e-gate-verdict-'));
  const runDir = join(root, RUN_ID);
  mkdirSync(runDir);
  writeFileSync(join(runDir, 'run.jsonl'), `${healthyCockpit().map((o) => JSON.stringify(o)).join('\n')}\n`);
  const args = ['cockpit', '--run-dir', `${runDir}/`, '--selector', 'quick', '--quarantine', 'SHARE-DESKTOP', '--engine-exit', '0'];
  const ok = runCli([...args, '--started-at-ms', String(STARTED_AT)]);
  assert.equal(ok.status, 0, ok.out);
  const exited = runCli([...args, '--started-at-ms', String(STARTED_AT), '--engine-exit', '1']);
  assert.equal(exited.status, 1, exited.out);
  assert.match(exited.out, /::error::Test Cockpit engine exited 1/);
  const missingExit = runCli(['cockpit', '--run-dir', `${runDir}/`, '--selector', 'quick', '--started-at-ms', String(STARTED_AT)]);
  assert.equal(missingExit.status, 1, missingExit.out);
  writeFileSync(join(runDir, 'run.jsonl'), `${cancelledCockpit(4).map((o) => JSON.stringify(o)).join('\n')}\n`);
  const cancelled = runCli([...args, '--started-at-ms', String(STARTED_AT)]);
  assert.equal(cancelled.status, 1, cancelled.out);
  assert.match(cancelled.out, /::error::run was CANCELLED/);
  const stale = runCli([...args, '--started-at-ms', String(Number(RUN_ID) + 5000)]);
  assert.equal(stale.status, 1, stale.out);
  assert.match(stale.out, /STALE/);
  assert.equal(runCli(['cockpit', '--run-dir', '', '--started-at-ms', '0', '--selector', 'quick']).status, 1);
  assert.equal(runCli(['bogus']).status, 1);
});

// ---------------------------------------------------------------- wiring
// The verdict is only as good as its call sites: pin that nightly-loopback.yml
// and release.yml actually use it, and prove each pin fails when its line is
// removed (a check never seen to fail is worth nothing).

const WORKFLOW = fileURLToPath(new URL('../.github/workflows/nightly-loopback.yml', import.meta.url));
const RELEASE = fileURLToPath(new URL('../.github/workflows/release.yml', import.meta.url));

const WIRING = [
  ['workflow', 'loopback verdict is called with the required cases', /--required "\$E2E_REQUIRED_RC_CASES"/],
  ['workflow', 'the display-share cases are required', /E2E_REQUIRED_RC_CASES: "37,38,39,40,41"/],
  ['workflow', 'loopback verdict is called with the harness exit', /node "\$VERDICT" loopback --results "\$RESULTS_JSON" --harness-exit "\$status"/],
  ['workflow', 'cockpit verdict is called with the step start time', /node "\$VERDICT" cockpit --run-dir "\$\{RUN_DIR:-\}" --started-at-ms "\$COCKPIT_STARTED_MS"/],
  ['workflow', 'cockpit verdict is called with the engine exit', /--engine-exit "\$engine_exit"/],
  ['workflow', 'the engine exit is captured right after the engine runs', /--test-case="\$COCKPIT_SELECTOR"\n\s+engine_exit=\$\?/],
  ['workflow', 'the step start time is recorded', /COCKPIT_STARTED_MS=\$\(\( \$\(date \+%s\) \* 1000 \)\)/],
  ['workflow', 'a missing setup marker fails as the release gate', /if \[ "\$E2E_GATE" = "1" \]; then\n\s+echo "::error::Test Cockpit Quick tier CANNOT RUN[^\n]*\n\s+exit 1/],
  ['workflow', 'E2E_GATE comes from the gate input', /E2E_GATE: \$\{\{ inputs\.gate && '1' \|\| '0' \}\}/],
  ['workflow', 'workflow_call defaults gate to true', /workflow_call:[\s\S]*?\n      gate:\n(?:\s+[a-z].*\n)*?\s+default: true\n\s+type: boolean/],
  ['release', 'release.yml passes gate: true to the e2e gate', /uses: \.\/\.github\/workflows\/nightly-loopback\.yml\n[\s\S]*?\n\s+gate: true\n/],
];

function checkWiring(texts) {
  return WIRING.filter(([file, , re]) => !re.test(texts[file])).map(([, what]) => what);
}

test('wiring: the real workflows call the verdict and pass gate strictness', () => {
  const texts = { workflow: readFileSync(WORKFLOW, 'utf8'), release: readFileSync(RELEASE, 'utf8') };
  assert.deepEqual(checkWiring(texts), []);
  // The old lax paths must be gone.
  assert.doesNotMatch(texts.workflow, /every failing case is quarantined; the gate passes on the scorecard/);
  assert.doesNotMatch(texts.workflow, /failed = \[c for c in results if c\.get\("status"\) == "fail"\]/);
});

test('wiring: each pin fails when its line is deleted', () => {
  const texts = { workflow: readFileSync(WORKFLOW, 'utf8'), release: readFileSync(RELEASE, 'utf8') };
  const mutations = [
    ['workflow', /^.*node "\$VERDICT" loopback.*\n/m],
    ['workflow', /^.*node "\$VERDICT" cockpit.*\n/m],
    ['workflow', /^.*COCKPIT_STARTED_MS=\$\(\(.*\n/m],
    ['workflow', / --engine-exit "\$engine_exit"/],
    ['workflow', /^\s+engine_exit=\$\?\n/m],
    ['workflow', /^.*if \[ "\$E2E_GATE" = "1" \]; then\n/m],
    ['workflow', /^.*E2E_GATE: \$\{\{.*\n/m],
    ['workflow', /(workflow_call:[\s\S]*?\n      gate:\n(?:\s+[a-z].*\n)*?\s+default: )true/],
    ['release', /^\s+gate: true\n/m],
  ];
  for (const [file, re] of mutations) {
    const mutant = { ...texts, [file]: texts[file].replace(re, (m, g1) => (g1 ? `${g1}false` : '')) };
    assert.notEqual(mutant[file], texts[file], `mutation ${re} changed nothing`);
    assert.ok(checkWiring(mutant).length > 0, `mutation ${re} was not caught`);
  }
});
