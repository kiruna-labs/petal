#!/usr/bin/env node
// rc-latency gate: the run-level remote-control latency verdict must go RED on
// a regression and on missing data, and GREEN on a healthy run (CLAUDE.md rule
// 8: test a gate in both directions). Fixtures are shaped like the real VM
// artifacts (petal-dev.log `remote-control-latency:` lines, the numbered
// suite's results JSON, the press-to-photon report).
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { mkdtempSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  ATTRIBUTION_ROWS,
  LATENCY_CHECKS,
  LOSS_ATTRIBUTION_ROWS,
  REPLAY_ATTRIBUTION_ROWS,
  SUMMARY_COUNTERS,
  attributionVerdict,
  replayAttributionVerdict,
  collectLatencyMetrics,
  evaluateLatencyGate,
  latencyVerdict,
  pairControllerPublishes,
  parseHostLatencyLog,
  parseLatencySummaryLog,
  summarizeLatencySummaries,
  resolveLatencyBudgets,
} from './ci/remote-control-latency-verdict.mjs';
import { WATCH_SCRIPT, armObservationWatch, parseWatchLine } from '../apps/desktop/scripts/remote-control-observation-watch.mjs';
import { freezeProbeExpression, summarizePresentedGaps } from '../apps/desktop/scripts/remote-control-freeze-probe.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '..');
const rcScripts = path.join(repoRoot, 'apps/desktop/scripts');
const CONTROLLER = 'web-ad016bed';

function hostLines({ seq, window = 165, receive, inject, complete, kind = 'Text', action = 'None', controller = CONTROLLER }) {
  const summary = `kind=${kind} action=${action} window=${window} seq=${seq} controller='${controller}' target='native-autotest' status='-'`;
  const prefix = '2026-10-17 00:56:41.528 [INFO] [desktop_lib::remote_control] remote-control-latency:';
  return [
    `${prefix} host receive_ts_ms=${receive} ${summary}`,
    `${prefix} host enqueue_ts_ms=${receive} ${summary} target_pid=9796`,
    `${prefix} host inject_ts_ms=${inject} ${summary} target_pid=Some(9796)`,
    `2026-10-17 00:56:41.540 [INFO] [desktop_lib::remote_control::input] remote-control-latency: ax probes ${summary} ax_ipc=0 cache_hit=0 cache_miss=0`,
    `${prefix} host replay complete_ts_ms=${complete} ${summary} elapsed_ms=${complete - inject} target_pid=Some(9796)`,
  ];
}

// A run shaped like the real thing; `slow` adds a host-side stall before
// injection (what PETAL_RC_TEST_REPLAY_DELAY_MS does on the VM) and moves the
// controller-visible numbers by the same amount.
function syntheticRun({ slowMs = 0, events = 60, observations = 21, photon = 40, freezeGapMs = 70 } = {}) {
  const lines = [];
  const publishes = [];
  let t = 1_792_198_600_000;
  for (let seq = 1; seq <= events; seq += 1) {
    const at = t;
    const receive = at + 4 + (seq % 3);
    const inject = receive + 1 + (seq % 5) + slowMs;
    const complete = inject + 10 + (seq % 7) * 5;
    lines.push(...hostLines({ seq, receive, inject, complete }));
    publishes.push({ caseId: 9, kind: 'text', action: null, seq, windowId: 165, at });
    t += 400;
  }
  const results = {
    summary: { targetObservationLatency: { methods: { watch: observations } } },
    controllerPublishes: publishes,
    results: [
      {
        caseId: 9,
        targetObservationSamplesMs: Array.from({ length: Math.min(observations, 7) }, (_, i) => 80 + i * 10 + slowMs),
        targetObservationDistributionSamplesMs: Array.from({ length: Math.max(0, observations - 7) }, (_, i) => 90 + i * 5 + slowMs),
      },
    ],
  };
  const timingSamples = Array.from({ length: photon }, (_, i) => ({
    sample: i + 1,
    inputKind: i % 2 ? 'click' : 'text',
    // Shaped like the VM (run 39052137479: p50 188.5, p95 253.9).
    pressToEstimatedPhotonMs: 170 + (i % 10) * 9 + slowMs,
  }));
  // A 20s probe at ~30fps whose worst presented-frame gap is freezeGapMs.
  const frameTimes = Array.from({ length: 600 }, (_, i) => 1000 + i * 33.3);
  frameTimes.splice(300, Math.max(0, Math.round(freezeGapMs / 33.3) - 1));
  const freezeProbe = summarizePresentedGaps(frameTimes, { startMs: 1000, endMs: 21_000 });
  const photonReport = {
    summary: { freezeProbe, pressToPhoton: { expectedSamples: photon, sampleFailures: [], timingSamples, byInput: {} } },
  };
  return { results, photon: photonReport, hostLog: lines.join('\n') };
}

const budgets = resolveLatencyBudgets({});

test('a healthy run passes every latency check', () => {
  const verdict = evaluateLatencyGate(collectLatencyMetrics(syntheticRun()), budgets);
  for (const check of verdict.checks) assert.equal(check.status, 'pass', `${check.id}: ${check.detail}`);
  assert.equal(verdict.pass, true);
});

test('a host replay stall fails the host pipeline, send->inject, observation and photon budgets', () => {
  const metrics = collectLatencyMetrics(syntheticRun({ slowMs: 150 }));
  const verdict = evaluateLatencyGate(metrics, budgets);
  assert.equal(verdict.pass, false);
  const failed = new Set(verdict.checks.filter((check) => check.status === 'fail').map((check) => check.id));
  for (const id of [
    'host-receive-to-inject-p95',
    'send-to-inject-p95',
    'observation-p50',
    'press-to-photon-p50',
  ]) {
    assert.ok(failed.has(id), `${id} should fail under a 150ms host stall: ${JSON.stringify(verdict.checks)}`);
  }
});

test('each budget individually goes red just above it and green at it', () => {
  for (const check of LATENCY_CHECKS) {
    const metrics = collectLatencyMetrics(syntheticRun());
    const at = { ...metrics[check.metric], [check.stat]: budgets[check.id] };
    const over = { ...metrics[check.metric], [check.stat]: budgets[check.id] + 0.1 };
    const green = evaluateLatencyGate({ ...metrics, [check.metric]: at }, budgets).checks.find((c) => c.id === check.id);
    const red = evaluateLatencyGate({ ...metrics, [check.metric]: over }, budgets).checks.find((c) => c.id === check.id);
    assert.equal(green.status, 'pass', `${check.id} at budget`);
    assert.equal(red.status, 'fail', `${check.id} over budget`);
  }
});

test('missing data is INSUFFICIENT and fails the gate -- never a pass by absence', () => {
  const empty = evaluateLatencyGate(collectLatencyMetrics({ results: null, photon: null, hostLog: '' }), budgets);
  assert.equal(empty.pass, false);
  for (const check of empty.checks) assert.equal(check.status, 'insufficient', check.id);

  // One sample per case (no distribution repeats) is too few for a p95.
  const thin = evaluateLatencyGate(collectLatencyMetrics(syntheticRun({ observations: 7 })), budgets);
  assert.equal(thin.checks.find((c) => c.id === 'observation-p95').status, 'insufficient');
});

// The scenario lists a missed sample in sampleFailures and NOT in
// timingSamples, so a run with N misses carries 40-N timings.
function withPhotonMisses(run, misses, { warmup = false } = {}) {
  const stats = run.photon.summary.pressToPhoton;
  if (!warmup) stats.timingSamples = stats.timingSamples.slice(misses);
  stats.sampleTimeoutMs = 2000;
  stats.sampleFailures = Array.from({ length: misses }, (_, i) => ({
    sample: i + 1, inputKind: 'click', error: 'sentinel generation 1 was not visible within 2000ms', ...(warmup ? { warmup: true } : {}),
  }));
  return run;
}

test('a missed photon sample scores as the 2000ms timeout and one or two are absorbed by the p95', () => {
  for (const misses of [1, 2]) {
    const metrics = collectLatencyMetrics(withPhotonMisses(syntheticRun(), misses));
    assert.equal(metrics.pressToPhoton.samples, 40);
    assert.equal(metrics.pressToPhoton.maxMs, 2000);
    assert.equal(metrics.pressToPhoton.misses, misses);
    const verdict = evaluateLatencyGate(metrics, budgets);
    assert.equal(verdict.checks.find((c) => c.id === 'press-to-photon-p95').status, 'pass', `${misses} miss(es)`);
    assert.equal(verdict.checks.find((c) => c.id === 'press-to-photon-p50').status, 'pass', `${misses} miss(es)`);
  }
});

test('more than two photon misses, or too few samples, is INSUFFICIENT and fails', () => {
  const three = evaluateLatencyGate(collectLatencyMetrics(withPhotonMisses(syntheticRun(), 3)), budgets);
  assert.equal(three.checks.find((c) => c.id === 'press-to-photon-p95').status, 'insufficient');
  assert.match(three.checks.find((c) => c.id === 'press-to-photon-p95').detail, /3 missed sample\(s\) > 2 allowed/);
  assert.equal(three.pass, false);

  const short = syntheticRun();
  short.photon.summary.pressToPhoton.timingSamples = short.photon.summary.pressToPhoton.timingSamples.slice(10);
  const thin = evaluateLatencyGate(collectLatencyMetrics(short), budgets);
  assert.equal(thin.checks.find((c) => c.id === 'press-to-photon-p50').status, 'insufficient');
  assert.equal(thin.pass, false);
});

test('two misses plus the recurring ~788ms text outlier still breach the photon p95', () => {
  const run = withPhotonMisses(syntheticRun(), 2);
  run.photon.summary.pressToPhoton.timingSamples[26].pressToEstimatedPhotonMs = 788;
  const verdict = evaluateLatencyGate(collectLatencyMetrics(run), budgets);
  assert.equal(verdict.checks.find((c) => c.id === 'press-to-photon-p95').status, 'fail');
});

test('warm-up misses never count against the photon run', () => {
  const metrics = collectLatencyMetrics(withPhotonMisses(syntheticRun(), 4, { warmup: true }));
  assert.equal(metrics.pressToPhoton.misses, 0);
  assert.equal(metrics.pressToPhoton.samples, 40);
  assert.equal(evaluateLatencyGate(metrics, budgets).checks.find((c) => c.id === 'press-to-photon-p95').status, 'pass');
});

test('one outlier in a 40-sample photon run does not fail the p95', () => {
  const run = syntheticRun();
  run.photon.summary.pressToPhoton.timingSamples[5].pressToEstimatedPhotonMs = 900;
  const verdict = evaluateLatencyGate(collectLatencyMetrics(run), budgets);
  assert.equal(verdict.checks.find((c) => c.id === 'press-to-photon-p95').status, 'pass');
});

test('host log parsing joins receive/inject/complete per event, real line shape', () => {
  const events = parseHostLatencyLog([
    ...hostLines({ seq: 7, receive: 100, inject: 101, complete: 112, kind: 'Pointer', action: 'Some(Click)' }),
    // A web controller reload restarts its seq counter: same seq, new event.
    ...hostLines({ seq: 7, receive: 900, inject: 905, complete: 960, controller: CONTROLLER }),
    '2026-10-17 [INFO] remote-control-latency: controller send_ts_ms=5 kind=Key action=None window=1 seq=1',
  ].join('\n'));
  assert.equal(events.length, 2);
  assert.deepEqual(
    events.map(({ seq, receiveTsMs, injectTsMs, completeTsMs, elapsedMs, kind }) => ({ seq, receiveTsMs, injectTsMs, completeTsMs, elapsedMs, kind })),
    [
      { seq: 7, receiveTsMs: 100, injectTsMs: 101, completeTsMs: 112, elapsedMs: 11, kind: 'Pointer' },
      { seq: 7, receiveTsMs: 900, injectTsMs: 905, completeTsMs: 960, elapsedMs: 55, kind: 'Text' },
    ]
  );
});

test('controller publishes pair by window+seq to the nearest later host receive', () => {
  const host = parseHostLatencyLog([
    ...hostLines({ seq: 7, receive: 1010, inject: 1012, complete: 1020 }),
    ...hostLines({ seq: 7, receive: 5010, inject: 5011, complete: 5030 }),
  ].join('\n'));
  const pairs = pairControllerPublishes(
    [
      { kind: 'text', seq: 7, windowId: 165, at: 5000 },
      { kind: 'text', seq: 7, windowId: 165, at: 1000 },
      { kind: 'request', seq: 8, windowId: 165, at: 1000 },
      { kind: 'text', seq: 7, windowId: 999, at: 1000 },
    ],
    host
  );
  assert.deepEqual(pairs.map((pair) => pair.sendToInjectMs), [11, 12]);
});

test('budgets come from the workflow env and reject garbage', () => {
  assert.equal(resolveLatencyBudgets({ PETAL_RC_SEND_TO_INJECT_P95_BUDGET_MS: '75' })['send-to-inject-p95'], 75);
  assert.throws(() => resolveLatencyBudgets({ PETAL_RC_OBSERVATION_P50_BUDGET_MS: 'fast' }), /positive number/);
  // The gate defaults ARE the VM calibration: the workflow must pin the same
  // numbers, so deleting an env line can never silently loosen a budget.
  const workflow = readFileSync(path.join(repoRoot, '.github/workflows/nightly-loopback.yml'), 'utf8');
  for (const check of LATENCY_CHECKS) {
    const match = workflow.match(new RegExp(`${check.env}:\\s*"(\\d+)"`));
    assert.ok(match, `nightly-loopback.yml must set ${check.env}`);
    assert.equal(Number(match[1]), check.defaultBudgetMs, `${check.env} in the workflow must equal the gate default`);
  }
  assert.match(workflow, /node "\$VERDICT" latency /);
  assert.match(workflow, /--press-to-photon/);
});

test('watcher protocol lines parse, and garbage is ignored', () => {
  assert.deepEqual(parseWatchLine('READY 1792198601528.4 present=false'), { type: 'ready', atMs: 1792198601528.4, present: false });
  assert.deepEqual(parseWatchLine('SEEN 1792198601612.0'), { type: 'seen', atMs: 1792198601612, reads: null, maxGapMs: null });
  assert.deepEqual(parseWatchLine('SEEN 1792198601612.0 reads=9 maxGapMs=19.2'), { type: 'seen', atMs: 1792198601612, reads: 9, maxGapMs: 19.2 });
  assert.deepEqual(parseWatchLine('TIMEOUT ""'), { type: 'timeout', lastError: '""' });
  assert.equal(parseWatchLine('osascript: some warning'), null);
});

function fakeSpawn(lines, { delayMs = 1 } = {}) {
  return () => {
    const child = new EventEmitter();
    child.stderr = new EventEmitter();
    child.stderr.setEncoding = () => {};
    child.kill = () => { setTimeout(() => child.emit('close', null, 'SIGKILL'), 0); };
    let index = 0;
    const next = () => {
      if (index >= lines.length) return;
      child.stderr.emit('data', `${lines[index++]}\n`);
      setTimeout(next, delayMs);
    };
    setTimeout(next, delayMs);
    return child;
  };
}

test('an armed watcher hands back the SEEN wall-clock instant', async () => {
  const watch = await armObservationWatch({
    mode: 'textedit', needle: ' Typed-1 ', timeoutMs: 1000,
    spawnImpl: fakeSpawn(['READY 1000.0 present=false', 'SEEN 1084.5 reads=6 maxGapMs=17.5']),
  });
  assert.equal(await watch.waitSeen(), 1084.5);
  assert.deepEqual(watch.resolution(), { reads: 6, maxGapMs: 17.5 });
});

test('a watcher that times out yields null so the caller falls back to its oracle', async () => {
  const watch = await armObservationWatch({
    mode: 'clipboard', needle: 'copy-1', timeoutMs: 10,
    spawnImpl: fakeSpawn(['READY 1000.0 present=false', 'TIMEOUT ""']),
  });
  assert.equal(await watch.waitSeen(), null);
});

test('a needle already present before the action is a re-arm bug, not a ~0ms sample', async () => {
  await assert.rejects(
    armObservationWatch({
      mode: 'textedit', needle: 'A', timeoutMs: 1000,
      spawnImpl: fakeSpawn(['READY 1000.0 present=true']),
    }),
    /did not re-arm/
  );
});

test('a watcher that never reports READY degrades to null (poll fallback)', async () => {
  const watch = await armObservationWatch({
    mode: 'textedit', needle: 'x', timeoutMs: 1000, readyTimeoutMs: 20,
    spawnImpl: fakeSpawn([]),
  });
  assert.equal(watch, null);
});

// The press-to-photon gate decodes the sentinel's Gray-code strip from the
// received video. #811's scroll strip was laid over it, so every photon
// sample timed out on the VM while the input itself landed. Nothing the
// sentinel adds may cover the strip again.
test('no sentinel subview covers the Gray-code strip the photon gate decodes', async () => {
  const swift = readFileSync(path.join(rcScripts, 'remote-control-photon-sentinel.swift'), 'utf8');
  const { REMOTE_CONTROL_SCROLL_DELTAS } = await import('../apps/desktop/scripts/remote-control-gestures.mjs');
  const literal = (text) => {
    const match = /NSRect\(x: (\d+), y: (\d+), width: (\d+), height: (\d+)\)/.exec(text);
    return match ? (([x, y, w, h]) => ({ x, y, w, h }))(match.slice(1).map(Number)) : null;
  };
  const named = (name) => {
    const match = swift.match(new RegExp(`let ${name} = (NSRect\\([^)]*\\))`));
    assert.ok(match, `${name} must be a literal NSRect in the sentinel`);
    return literal(match[1]);
  };
  // Every view added to the pattern view, resolved to its frame: either a
  // literal NSRect or a named `let ...Rect` constant. A new subview whose
  // frame cannot be resolved fails here rather than being silently skipped.
  const subviews = [...swift.matchAll(/patternView\.addSubview\((\w+)\)/g)].map((m) => m[1]);
  assert.ok(subviews.length >= 5, `expected the sentinel's subviews, found ${subviews.join(',')}`);
  const frames = {};
  for (const name of subviews) {
    const assignments = [...swift.matchAll(new RegExp(`\\b${name}(?:\\.frame)? = [^\\n]*`, 'g'))].map((m) => m[0]);
    assert.ok(assignments.length, `cannot find the frame assignment for subview ${name}`);
    for (const assignment of assignments) {
      const constant = /frame: (\w+Rect)\)/.exec(assignment);
      frames[name] = (constant ? named(constant[1]) : literal(assignment)) ?? frames[name];
    }
    assert.ok(frames[name], `cannot resolve the frame of subview ${name}: ${assignments.join(' | ')}`);
  }
  const gray = { x: 160, y: 88, w: 640, h: 30 };
  assert.match(swift, /NSRect\(x: 160 \+ CGFloat\(index \* 40\), y: 88, width: 40, height: 30\)/, 'Gray strip geometry moved; update this test');
  const overlaps = (a, b) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
  for (const [name, frame] of Object.entries(frames)) {
    assert.equal(overlaps(frame, gray), false, `subview ${name} ${JSON.stringify(frame)} covers the Gray-code strip`);
  }
  const strip = frames.scrollStrip;
  const aim = REMOTE_CONTROL_SCROLL_DELTAS.horizontalSentinel;
  const px = { x: aim.x * 960, y: aim.y * 600 };
  assert.ok(px.x > strip.x && px.x < strip.x + strip.w && px.y > strip.y && px.y < strip.y + strip.h,
    'case 21 must aim inside the scroll strip');
});

test('latencyVerdict speaks the verdict-script shape, both directions', () => {
  const green = latencyVerdict(syntheticRun());
  assert.equal(green.ok, true);
  assert.deepEqual(green.errors, []);
  const red = latencyVerdict(syntheticRun({ slowMs: 150 }));
  assert.equal(red.ok, false);
  assert.ok(red.errors.some((line) => line.startsWith('latency host-receive-to-inject-p95: FAIL')));
  const garbage = latencyVerdict({ ...syntheticRun(), env: { PETAL_RC_SEND_TO_INJECT_P95_BUDGET_MS: 'soon' } });
  assert.equal(garbage.ok, false);
});

// #378: the presented-freeze check is REPORT-ONLY (until #421 lands) unless
// E2E_IMPAIRED_BLOCKING=true -- so the clean pass keeps its pre-#378 checks.
test('presented-freeze check blocks only when E2E_IMPAIRED_BLOCKING=true', () => {
  const frozen = syntheticRun({ freezeGapMs: 650 });
  const blocking = latencyVerdict({ ...frozen, env: { E2E_IMPAIRED_BLOCKING: 'true' } });
  assert.equal(blocking.ok, false);
  assert.ok(blocking.errors.some((line) => line.startsWith('latency presented-freeze-max-gap: FAIL')));
  for (const env of [{}, { E2E_IMPAIRED_BLOCKING: 'false' }]) {
    const reportOnly = latencyVerdict({ ...frozen, env });
    assert.equal(reportOnly.ok, true, reportOnly.errors.join('\n'));
    assert.deepEqual(reportOnly.errors, []);
    assert.ok(reportOnly.warnings.some((line) => line.startsWith('REPORT-ONLY: would fail -- latency presented-freeze-max-gap: FAIL')));
    assert.equal(reportOnly.checks.find((c) => c.id === 'presented-freeze-max-gap').reportOnly, true);
  }
  // A missing probe is also non-blocking by default, never a silent pass.
  const missing = syntheticRun();
  delete missing.photon.summary.freezeProbe;
  const noProbe = latencyVerdict(missing);
  assert.equal(noProbe.ok, true);
  assert.ok(noProbe.warnings.some((line) => /REPORT-ONLY: would fail -- latency presented-freeze-max-gap: INSUFFICIENT/.test(line)));
  // Every other check still blocks with the switch off.
  assert.equal(latencyVerdict(syntheticRun({ slowMs: 150 })).ok, false);
});

// The real CLI path the workflow runs: e2e-gate-verdict.mjs latency.
test('e2e-gate-verdict.mjs latency exits 0 on a healthy run and 1 on a stalled one', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'rc-latency-verdict-'));
  const run = (label, fixture) => {
    const results = path.join(dir, `${label}-results.json`);
    const photon = path.join(dir, `${label}-photon.json`);
    const log = path.join(dir, `${label}-petal.log`);
    writeFileSync(results, JSON.stringify(fixture.results));
    writeFileSync(photon, JSON.stringify(fixture.photon));
    writeFileSync(log, fixture.hostLog);
    const env = { ...process.env };
    for (const check of LATENCY_CHECKS) delete env[check.env];
    return spawnSync(process.execPath, [path.join(scriptDir, 'ci/e2e-gate-verdict.mjs'), 'latency',
      '--results', results, '--photon', photon, '--host-log', log, '--json', path.join(dir, `${label}.json`)],
    { encoding: 'utf8', env });
  };
  const green = run('green', syntheticRun());
  assert.equal(green.status, 0, green.stdout);
  assert.match(green.stdout, /e2e-gate-verdict latency: PASS/);
  const red = run('red', syntheticRun({ slowMs: 150 }));
  assert.equal(red.status, 1, red.stdout);
  assert.match(red.stdout, /::error::latency send-to-inject-p95: FAIL/);
  const absent = spawnSync(process.execPath, [path.join(scriptDir, 'ci/e2e-gate-verdict.mjs'), 'latency',
    '--results', path.join(dir, 'missing.json'), '--photon', path.join(dir, 'missing.json'), '--host-log', path.join(dir, 'missing.log')],
  { encoding: 'utf8' });
  assert.equal(absent.status, 1, 'missing evidence must fail, never pass by absence');
});

// The watcher sleeps between reads (no busy-wait) and reports its own
// resolution; a spin loop here pinned a core for every armed observation.
test('the JXA watcher sleeps between reads instead of spinning, and reports read gaps', () => {
  assert.match(WATCH_SCRIPT, /\$\.usleep\(\d+\)/);
  assert.doesNotMatch(WATCH_SCRIPT, /while \(nowMs\(\) < next\) \{\}/);
  assert.match(WATCH_SCRIPT, /'SEEN ' \+ readAt\.toFixed\(1\) \+ ' reads=' \+ reads \+ ' maxGapMs='/);
});

// #370: the aggregated summary mirror is parsed and reported, never gated.
test('latency-summary mirror lines are parsed and reported without gating', () => {
  const contract = JSON.parse(readFileSync(path.join(repoRoot, 'contracts/petal-contracts.json'), 'utf8'));
  const prefix = '2026-10-19 10:00:00.000 [INFO] [desktop_lib::rc_latency_telemetry] ';
  const finalLine = contract.remoteControlLatencySummary.example.logLine
    .replace('final=false', 'final=true')
    .replace('replay_complete_ms_p95=108', 'replay_complete_ms_p95=131');
  const log = [prefix + contract.remoteControlLatencySummary.example.logLine, prefix + finalLine, 'noise line'].join('\n');
  const intervals = parseLatencySummaryLog(log);
  assert.equal(intervals.length, 2);
  assert.equal(intervals[0].rc_session_id, '0123456789abcdef0123456789abcdef');
  assert.equal(intervals[0].replay_complete_ms_p95, 108);
  assert.equal(intervals[1].final, true);
  const summary = summarizeLatencySummaries(intervals);
  assert.equal(summary.intervals, 2);
  assert.equal(summary.finals, 1);
  assert.equal(summary.replayComplete.p95MaxMs, 131);
  assert.equal(summary.replayComplete.samples, 796);
  assert.equal(summary.replayInject.p95MaxMs, 9);
  assert.equal(summarizeLatencySummaries(intervals, 'controller').intervals, 0);
  assert.equal(summarizeLatencySummaries([]).replayComplete.p95MaxMs, null);
  // #372: edge dimensions are reported from the latest interval carrying them.
  assert.equal(summary.edge, null);
  const edgeLine = `${prefix}${contract.remoteControlLatencySummary.example.logLine} lk_region=unknown same_edge=true self_hosted_sfu=true ice_candidate_type=host ice_protocol=udp`;
  assert.deepEqual(summarizeLatencySummaries(parseLatencySummaryLog(`${log}\n${edgeLine}`)).edge, {
    lk_region: 'unknown',
    same_edge: 'true',
    self_hosted_sfu: 'true',
    ice_candidate_type: 'host',
    ice_protocol: 'udp',
  });
  // Not gated: a run with no summary lines still produces the same checks.
  const metrics = collectLatencyMetrics({ hostLog: log });
  assert.equal(metrics.latencySummary.replayComplete.p95MaxMs, 131);
  assert.ok(!LATENCY_CHECKS.some((check) => check.metric === 'latencySummary'));
  const verdict = latencyVerdict({ hostLog: log });
  assert.ok(verdict.notes.some((note) => note.includes('latency summary (#370, not gated): 2 host interval(s), 1 final')));
});

// ---------------------------------------------------------------------------
// #378: presented-video freeze probe + impaired-pass stage attribution.
// ---------------------------------------------------------------------------

test('freeze probe summary counts edge gaps, so a stream that never starts is one long freeze', () => {
  const healthy = summarizePresentedGaps([1033, 1066, 1100, 1999], { startMs: 1000, endMs: 2000 });
  assert.equal(healthy.frames, 4);
  assert.equal(healthy.maxGapMs, 899);
  assert.equal(healthy.freezes, 1);
  const dead = summarizePresentedGaps([], { startMs: 0, endMs: 20_000 });
  assert.equal(dead.frames, 0);
  assert.equal(dead.maxGapMs, 20_000);
  // Callbacks outside the window are not frames of this probe.
  assert.equal(summarizePresentedGaps([-5, 10, 5000], { startMs: 0, endMs: 100 }).frames, 1);
});

test('presented-freeze check: green under 500ms, red on a 650ms freeze, insufficient without a probe', () => {
  const freezeCheck = (run) => evaluateLatencyGate(collectLatencyMetrics(run), budgets).checks.find((c) => c.id === 'presented-freeze-max-gap');
  assert.equal(freezeCheck(syntheticRun()).status, 'pass');
  const red = freezeCheck(syntheticRun({ freezeGapMs: 650 }));
  assert.equal(red.status, 'fail');
  assert.match(red.detail, /1 freeze\(s\)/);
  const missing = syntheticRun();
  delete missing.photon.summary.freezeProbe;
  assert.equal(freezeCheck(missing).status, 'insufficient');
  const errored = syntheticRun();
  errored.photon.summary.freezeProbe = { error: 'freeze probe: no <video>' };
  assert.match(freezeCheck(errored).detail, /did not run: freeze probe: no <video>/);
  // A source that presents almost nothing is insufficient, not a pass.
  const starved = syntheticRun();
  starved.photon.summary.freezeProbe = summarizePresentedGaps(Array.from({ length: 50 }, (_, i) => i * 400), { startMs: 0, endMs: 20_000 });
  assert.notEqual(freezeCheck(starved).status, 'pass');
});

test('the freeze probe expression is valid JS that targets the share tile video', () => {
  const expression = freezeProbeExpression({ windowId: 165, durationMs: 20000 });
  assert.doesNotThrow(() => new Function(`return ${expression};`));
  assert.match(expression, /\.share-tile\[data-window-id="165"\]/);
  assert.match(expression, /requestVideoFrameCallback/);
});

test('the sentinel motion strip stays clear of the Gray code and the calibration squares', () => {
  const swift = readFileSync(path.join(rcScripts, 'remote-control-photon-sentinel.swift'), 'utf8');
  const m = /motionStripRect = NSRect\(x: (\d+), y: (\d+), width: (\d+), height: (\d+)\)/.exec(swift);
  assert.ok(m, 'motionStripRect must be a literal NSRect');
  const [x, y, w, h] = m.slice(1).map(Number);
  const strip = { x, y, w, h };
  const overlaps = (a, b) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
  for (const other of [
    { x: 160, y: 88, w: 640, h: 30 }, // Gray code
    { x: 16, y: 16, w: 24, h: 24 }, { x: 920, y: 16, w: 24, h: 24 }, // top calibration squares
    { x: 80, y: 45, w: 800, h: 36 }, // title
  ]) {
    assert.equal(overlaps(strip, other), false, `motion strip ${JSON.stringify(strip)} overlaps ${JSON.stringify(other)}`);
  }
  assert.match(swift, /SIGUSR1, true\), \(SIGUSR2, false\)/, 'motion is toggled by SIGUSR1/SIGUSR2');
});

function latencyJson(p50s) {
  const metrics = {};
  for (const [metric, p50Ms] of Object.entries(p50s)) metrics[metric] = { p50Ms };
  return { metrics };
}
const CLEAN = latencyJson({ sendToReceive: 6, pressToPhoton: 180, hostReceiveToInject: 2 });

test('attribution: 20ms per traversal lands on the network stages only -> pass', () => {
  const impaired = latencyJson({ sendToReceive: 47, pressToPhoton: 268, hostReceiveToInject: 3 });
  const verdict = attributionVerdict({ clean: CLEAN, impaired, delayMs: 20 });
  assert.equal(verdict.ok, true, verdict.errors.join('\n'));
  assert.equal(verdict.rows.length, ATTRIBUTION_ROWS.length);
});

test('attribution: an impairment that did not apply fails (the impaired pass was really clean)', () => {
  const verdict = attributionVerdict({ clean: CLEAN, impaired: CLEAN, delayMs: 20 });
  assert.equal(verdict.ok, false);
  assert.ok(verdict.errors.some((e) => /sendToReceive \(must-move\): FAIL/.test(e)));
});

test('attribution: photon movement is reported, never gating (run 39837860987: +14.8ms at 20ms/traversal)', () => {
  const impaired = latencyJson({ sendToReceive: 42, pressToPhoton: 195, hostReceiveToInject: 2 });
  const verdict = attributionVerdict({ clean: CLEAN, impaired, delayMs: 20 });
  assert.equal(verdict.ok, true, verdict.errors.join('\n'));
  assert.ok(verdict.notes.some((n) => /pressToPhoton \(report\)/.test(n)));
});

test('attribution: delay showing up in the host stage is a misattribution and fails', () => {
  const impaired = latencyJson({ sendToReceive: 47, pressToPhoton: 268, hostReceiveToInject: 42 });
  const verdict = attributionVerdict({ clean: CLEAN, impaired, delayMs: 20 });
  assert.equal(verdict.ok, false);
  assert.ok(verdict.errors.some((e) => /hostReceiveToInject \(must-not-move\): FAIL/.test(e)));
});

test('attribution: missing evidence or a bad delay is never a pass', () => {
  assert.equal(attributionVerdict({ clean: null, impaired: CLEAN, delayMs: 20 }).ok, false);
  assert.equal(attributionVerdict({ clean: CLEAN, impaired: CLEAN, delayMs: '' }).ok, false);
  assert.equal(attributionVerdict({ clean: CLEAN, impaired: CLEAN, delayMs: 0 }).ok, false);
});

test('e2e-gate-verdict.mjs attribution exits 0 on a correct attribution and 1 on a clean "impaired" pass', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'rc-attribution-verdict-'));
  const clean = path.join(dir, 'clean.json');
  const impaired = path.join(dir, 'impaired.json');
  writeFileSync(clean, JSON.stringify(CLEAN));
  const cli = () => spawnSync(process.execPath, [path.join(scriptDir, 'ci/e2e-gate-verdict.mjs'), 'attribution',
    '--clean', clean, '--impaired', impaired, '--delay-ms', '20'], { encoding: 'utf8' });
  writeFileSync(impaired, JSON.stringify(latencyJson({ sendToReceive: 47, pressToPhoton: 268, hostReceiveToInject: 3 })));
  const green = cli();
  assert.equal(green.status, 0, green.stdout);
  writeFileSync(impaired, JSON.stringify(CLEAN));
  const red = cli();
  assert.equal(red.status, 1, red.stdout);
  assert.match(red.stdout, /::error::attribution sendToReceive/);
});

test('the workflow runs the impaired pass with its own budgets and always tears the impairment down', () => {
  const workflow = readFileSync(path.join(repoRoot, '.github/workflows/nightly-loopback.yml'), 'utf8');
  assert.match(workflow, /scripts\/ci\/net-impair\.sh on /);
  assert.match(workflow, /node "\$VERDICT" attribution /);
  // Teardown + the post-run check live in an if: always() step.
  assert.match(workflow, /- name: Remove network impairment[^\n]*\n\s+if: always\(\)/);
  assert.match(workflow, /net-impair\.sh check-clear/);
  // Impaired budgets are separate constants, never the clean ones loosened.
  for (const name of ['IMPAIRED_OBSERVATION_P50_MS', 'IMPAIRED_SEND_TO_INJECT_P95_MS', 'IMPAIRED_PHOTON_P50_MS', 'IMPAIRED_PHOTON_P95_MS']) {
    assert.match(workflow, new RegExp(`${name}:\\s*"\\d+"`), `${name} must be pinned in the workflow`);
  }
});

// #378 loss + replay rows. Fixtures go through the real collectLatencyMetrics
// (host log mirror lines + the web controller lines the harness collects),
// so the `--json` shape the attribution reads is the one the gate writes.
const SUMMARY_PREFIX = '2026-11-11 17:47:15.534 [INFO] [desktop_lib::rc_latency_telemetry] ';
function hostLine({ n = 40, inject = 1, complete = 2, capture = 22, excess = 1, excessP95 = 10, stalls = 0, leg = null } = {}) {
  let line = `remote-control-latency-summary: role=host rc_session_id=7a23449f7989695427755c28f0f75454 interval_s=25 final=true`
    + ` input_excess_delay_ms_p50=${excess} input_excess_delay_ms_p95=${excessP95} input_excess_delay_ms_max=${excessP95 + 2} input_excess_delay_ms_n=${n}`
    + ` replay_inject_ms_p50=${inject} replay_inject_ms_p95=${inject + 1} replay_inject_ms_max=${inject + 14} replay_inject_ms_n=${n}`
    + ` replay_complete_ms_p50=${complete} replay_complete_ms_p95=${complete + 60} replay_complete_ms_max=${complete + 70} replay_complete_ms_n=${n}`
    + ` inject_to_capture_ms_p50=${capture} inject_to_capture_ms_p95=${capture + 30} inject_to_capture_ms_max=${capture + 60} inject_to_capture_ms_n=${n}`;
  if (leg !== null) line += ` input_leg_ms_p50=${leg} input_leg_ms_p95=${leg + 3} input_leg_ms_max=${leg + 9} input_leg_ms_n=${n}`;
  if (stalls) line += ` input_stall_count=${stalls}`;
  return `${SUMMARY_PREFIX}${line} window_codec=h264 content_change=detected`;
}
function controllerLine({ subRtt = 2, pubRtt = 2, nacks = 0, present = null } = {}) {
  let line = `remote-control-latency-summary: role=controller rc_session_id=fedcba9876543210fedcba9876543210 interval_s=60 final=true`
    + ` sub_rtt_ms_p50=${subRtt} sub_rtt_ms_p95=${subRtt + 2} sub_rtt_ms_max=${subRtt + 7} sub_rtt_ms_n=60`
    + ` pub_rtt_ms_p50=${pubRtt} pub_rtt_ms_p95=${pubRtt + 2} pub_rtt_ms_max=${pubRtt + 7} pub_rtt_ms_n=60`
    + ` decode_ms_p50=3 decode_ms_p95=6 decode_ms_max=11 decode_ms_n=58`;
  if (present !== null) line += ` capture_to_present_ms_p50=${present} capture_to_present_ms_p95=${present + 20} capture_to_present_ms_max=${present + 70} capture_to_present_ms_n=1790`;
  if (nacks) line += ` nack_count=${nacks}`;
  return `${line} offset_err_ms=1 packets_lost_pct_x100=0 jb_min_ms=0 frame_metadata=unavailable`;
}
// One pass's latency `--json`: summary lines plus the per-event p50s.
function passJson({ host = [hostLine()], controller = [controllerLine()], p50s = { sendToReceive: 2, hostReceiveToInject: 0, pressToPhoton: 76 } } = {}) {
  const metrics = collectLatencyMetrics({ results: { controllerLatencySummaryLines: controller }, hostLog: host.join('\n') });
  for (const [metric, p50Ms] of Object.entries(p50s)) metrics[metric] = { ...metrics[metric], p50Ms };
  return { metrics };
}

test('summary stages get a run-level p50 (sample-weighted median) and counters sum per role', () => {
  const intervals = parseLatencySummaryLog([
    hostLine({ n: 10, complete: 8, stalls: 2 }),
    hostLine({ n: 30, complete: 1 }),
    hostLine({ n: 5, complete: 90, stalls: 1 }),
  ].join('\n'));
  const host = summarizeLatencySummaries(intervals);
  assert.equal(host.stages.replay_complete_ms.p50Ms, 1, '30 of 45 samples sit at 1ms');
  assert.equal(host.stages.replay_complete_ms.samples, 45);
  assert.equal(host.stages.replay_complete_ms.p95MaxMs, 150);
  assert.equal(host.counters.input_stall_count, 3);
  assert.equal(host.counters.nack_count, 0);
  assert.equal(host.stages.input_leg_ms, undefined, 'a stage no interval carries is absent, never 0');
  // Counter keys are the contract's, so a new counter cannot be silently dropped.
  const contract = JSON.parse(readFileSync(path.join(repoRoot, 'contracts/petal-contracts.json'), 'utf8'));
  assert.deepEqual([...SUMMARY_COUNTERS].sort(), [...contract.remoteControlLatencySummary.counters].sort());
  // Controller lines ride in the results/photon JSON, not the host log.
  const contractLine = contract.remoteControlLatencySummary.controllerExample.logLine;
  const metrics = collectLatencyMetrics({ results: { controllerLatencySummaryLines: [contractLine] }, photon: { controllerLatencySummaryLines: [controllerLine({ nacks: 4 })] } });
  assert.equal(metrics.controllerSummary.intervals, 2);
  assert.equal(metrics.controllerSummary.counters.frames_dropped, 3);
  assert.equal(metrics.controllerSummary.counters.nack_count, 4);
  assert.equal(metrics.latencySummary.intervals, 0, 'controller lines never count as host intervals');
});

const NET_IMPAIRED_P50S = { sendToReceive: 43, hostReceiveToInject: 0, pressToPhoton: 189 };
const LOSS_CLEAN = passJson();

test('attribution loss row: 2% loss raises web NACKs and leaves host replay still -> pass', () => {
  const impaired = passJson({ host: [hostLine({ complete: 3 })], controller: [controllerLine({ nacks: 37 })], p50s: NET_IMPAIRED_P50S });
  const verdict = attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20, lossPct: 2 });
  assert.equal(verdict.ok, true, verdict.errors.join('\n'));
  assert.equal(verdict.lossRows.length, LOSS_ATTRIBUTION_ROWS.length);
  assert.ok(verdict.notes.some((n) => /loss 2%: controller nack_count \(must-increase\): PASS -- count 0 -> 37/.test(n)));
  assert.ok(verdict.notes.some((n) => /host input_stall_count \(report\)/.test(n)), 'stalls are reported');
});

test('attribution loss row: loss that did not apply (no NACK rise) fails', () => {
  const impaired = passJson({ p50s: NET_IMPAIRED_P50S });
  const verdict = attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20, lossPct: 2 });
  assert.equal(verdict.ok, false);
  assert.ok(verdict.errors.some((e) => /controller nack_count \(must-increase\): FAIL/.test(e)));
});

test('attribution loss row: loss showing up in host replay is a misattribution and fails', () => {
  const impaired = passJson({ host: [hostLine({ inject: 30, complete: 45 })], controller: [controllerLine({ nacks: 37 })], p50s: NET_IMPAIRED_P50S });
  const verdict = attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20, lossPct: 2 });
  assert.equal(verdict.ok, false);
  assert.ok(verdict.errors.some((e) => /host replay_inject_ms \(must-not-move\): FAIL/.test(e)));
  assert.ok(verdict.errors.some((e) => /host replay_complete_ms \(must-not-move\): FAIL/.test(e)));
});

test('attribution loss row: missing controller lines are insufficient; below 2% it is not asserted', () => {
  const impaired = passJson({ controller: [], p50s: NET_IMPAIRED_P50S });
  const missing = attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20, lossPct: 2 });
  assert.equal(missing.ok, false);
  assert.ok(missing.errors.some((e) => /nack_count \(must-increase\): INSUFFICIENT/.test(e)));
  // The gate profile (1%) and an old caller without --loss-pct keep the delay rows only.
  const gate = attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20, lossPct: 1 });
  assert.equal(gate.ok, true, gate.errors.join('\n'));
  assert.ok(gate.notes.some((n) => /loss row: not asserted at 1%/.test(n)));
  assert.equal(attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20 }).ok, true);
  assert.equal(attributionVerdict({ clean: LOSS_CLEAN, impaired, delayMs: 20, lossPct: 'two' }).ok, false);
});

test('attribution loss row: a loss-only impairment (0ms delay) asserts the loss row alone', () => {
  const lossy = passJson({ controller: [controllerLine({ nacks: 37 })] });
  const green = attributionVerdict({ clean: LOSS_CLEAN, impaired: lossy, delayMs: 0, lossPct: 2 });
  assert.equal(green.ok, true, green.errors.join('\n'));
  assert.equal(green.rows.length, 0);
  assert.ok(green.notes.some((n) => /delay rows: not asserted/.test(n)));
  const red = attributionVerdict({ clean: LOSS_CLEAN, impaired: LOSS_CLEAN, delayMs: 0, lossPct: 2 });
  assert.equal(red.ok, false);
  assert.ok(red.errors.some((e) => /controller nack_count \(must-increase\): FAIL/.test(e)));
  // 0ms without a loss % is still a bad call, never a vacuous pass.
  assert.equal(attributionVerdict({ clean: LOSS_CLEAN, impaired: lossy, delayMs: 0 }).ok, false);
  assert.equal(attributionVerdict({ clean: LOSS_CLEAN, impaired: lossy, delayMs: 0, lossPct: 1 }).ok, false, '0ms at 1% asserts nothing');
});

const REPLAY_BASELINE = passJson({ host: [hostLine({ leg: 4 })], controller: [controllerLine({ present: 48 })] });
const REPLAY_RED_P50S = { sendToReceive: 3, hostReceiveToInject: 152, pressToPhoton: 337 };

test('attribution replay row: a 150ms host stall lands on the replay stages only -> pass', () => {
  const red = passJson({ host: [hostLine({ inject: 151, complete: 160, capture: 25, leg: 5 })], controller: [controllerLine({ subRtt: 3, present: 50 })], p50s: REPLAY_RED_P50S });
  const verdict = replayAttributionVerdict({ baseline: REPLAY_BASELINE, red, delayMs: 150 });
  assert.equal(verdict.ok, true, verdict.errors.join('\n'));
  assert.equal(verdict.rows.length, REPLAY_ATTRIBUTION_ROWS.length);
  assert.ok(verdict.notes.some((n) => /replay 150ms: host replay_inject_ms \(must-move\): PASS -- 1 -> 151ms moved 150ms/.test(n)));
  assert.ok(verdict.notes.some((n) => /pressToPhoton \(report\)/.test(n)), 'photon is reported only (photon_ms has no producer yet)');
});

test('attribution replay row: a stall that did not apply (red run is a baseline) fails', () => {
  const verdict = replayAttributionVerdict({ baseline: REPLAY_BASELINE, red: REPLAY_BASELINE, delayMs: 150 });
  assert.equal(verdict.ok, false);
  for (const name of ['host replay_inject_ms', 'host replay_complete_ms', 'hostReceiveToInject']) {
    assert.ok(verdict.errors.some((e) => e.includes(`${name} (must-move): FAIL`)), name);
  }
});

test('attribution replay row: replay delay showing up on the network, capture or input legs fails', () => {
  const host = [hostLine({ inject: 151, complete: 160, capture: 25, leg: 5 })];
  const controller = [controllerLine({ present: 50 })];
  const leaks = [
    [{ host, controller, p50s: { ...REPLAY_RED_P50S, sendToReceive: 150 } }, 'sendToReceive'],
    [{ host: [hostLine({ inject: 151, complete: 160, capture: 175, leg: 5 })], controller, p50s: REPLAY_RED_P50S }, 'host inject_to_capture_ms'],
    [{ host: [hostLine({ inject: 151, complete: 160, excess: 150, leg: 5 })], controller, p50s: REPLAY_RED_P50S }, 'host input_excess_delay_ms'],
    [{ host: [hostLine({ inject: 151, complete: 160, leg: 150 })], controller, p50s: REPLAY_RED_P50S }, 'host input_leg_ms'],
    [{ host, controller: [controllerLine({ subRtt: 150, present: 50 })], p50s: REPLAY_RED_P50S }, 'controller sub_rtt_ms'],
    [{ host, controller: [controllerLine({ pubRtt: 150, present: 50 })], p50s: REPLAY_RED_P50S }, 'controller pub_rtt_ms'],
    [{ host, controller: [controllerLine({ present: 200 })], p50s: REPLAY_RED_P50S }, 'controller capture_to_present_ms'],
  ];
  for (const [red, name] of leaks) {
    const verdict = replayAttributionVerdict({ baseline: REPLAY_BASELINE, red: passJson(red), delayMs: 150 });
    assert.equal(verdict.ok, false, name);
    assert.ok(verdict.errors.some((e) => e.includes(`${name} (must-not-move): FAIL`)), `${name}: ${verdict.errors.join('\n')}`);
  }
});

test('attribution replay row: optional stages are n/a when absent, required ones are insufficient', () => {
  const baseline = passJson();
  const red = passJson({ host: [hostLine({ inject: 151, complete: 160 })], p50s: REPLAY_RED_P50S });
  const verdict = replayAttributionVerdict({ baseline, red, delayMs: 150 });
  assert.equal(verdict.ok, true, verdict.errors.join('\n'));
  assert.ok(verdict.notes.some((n) => /host input_leg_ms \(must-not-move\): PASS -- n\/a/.test(n)));
  const noHostLog = passJson({ host: [], p50s: REPLAY_RED_P50S });
  const missing = replayAttributionVerdict({ baseline, red: noHostLog, delayMs: 150 });
  assert.equal(missing.ok, false);
  assert.ok(missing.errors.some((e) => /host replay_inject_ms \(must-move\): INSUFFICIENT/.test(e)));
  assert.equal(replayAttributionVerdict({ baseline: null, red, delayMs: 150 }).ok, false);
  assert.equal(replayAttributionVerdict({ baseline, red, delayMs: '' }).ok, false);
});

test('e2e-gate-verdict.mjs attribution-replay and --loss-pct exit 0 on a correct attribution and 1 on a wrong one', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'rc-attribution-rows-'));
  const write = (name, json) => { const file = path.join(dir, name); writeFileSync(file, JSON.stringify(json)); return file; };
  const verdictScript = path.join(scriptDir, 'ci/e2e-gate-verdict.mjs');
  const run = (args) => spawnSync(process.execPath, [verdictScript, ...args], { encoding: 'utf8' });
  const baseline = write('baseline.json', REPLAY_BASELINE);
  const goodRed = write('red.json', passJson({ host: [hostLine({ inject: 151, complete: 160, leg: 5 })], controller: [controllerLine({ present: 50 })], p50s: REPLAY_RED_P50S }));
  const green = run(['attribution-replay', '--baseline', baseline, '--red', goodRed, '--replay-delay-ms', '150']);
  assert.equal(green.status, 0, green.stdout);
  const red = run(['attribution-replay', '--baseline', baseline, '--red', baseline, '--replay-delay-ms', '150']);
  assert.equal(red.status, 1, red.stdout);
  assert.match(red.stdout, /::error::attribution replay 150ms: host replay_inject_ms/);
  const clean = write('clean.json', LOSS_CLEAN);
  const lossy = write('lossy.json', passJson({ controller: [controllerLine({ nacks: 37 })], p50s: NET_IMPAIRED_P50S }));
  const quiet = write('quiet.json', passJson({ p50s: NET_IMPAIRED_P50S }));
  const loss = (impaired) => run(['attribution', '--clean', clean, '--impaired', impaired, '--delay-ms', '20', '--loss-pct', '2']);
  assert.equal(loss(lossy).status, 0, loss(lossy).stdout);
  const noLoss = loss(quiet);
  assert.equal(noLoss.status, 1, noLoss.stdout);
  assert.match(noLoss.stdout, /::error::attribution loss 2%: controller nack_count/);
});

test('the workflow hands the impaired loss % to the attribution verdict', () => {
  const workflow = readFileSync(path.join(repoRoot, '.github/workflows/nightly-loopback.yml'), 'utf8');
  assert.match(workflow, /node "\$VERDICT" attribution [^\n]*\n[^\n]*--delay-ms "\$NET_DELAY_MS" --loss-pct "\$NET_LOSS_PCT"/);
  // Unconditional: a no-op impaired pass (0ms, 0%) must fail the verdict, not skip it.
  const step = workflow.slice(workflow.indexOf('Post-suite: IMPAIRED remote-control latency gate'), workflow.indexOf('node "$VERDICT" attribution '));
  assert.ok(!/NET_DELAY_MS" -gt 0|NET_LOSS_PCT"[^\n]*BEGIN/.test(step), 'attribution must run for every impaired pass');
  assert.equal(attributionVerdict({ clean: LOSS_CLEAN, impaired: LOSS_CLEAN, delayMs: '0', lossPct: '0' }).ok, false);
  // The harness banks web controller summaries only before a mid-suite rejoin,
  // never on the initial join (that page holds an earlier suite's lines).
  const scenario = readFileSync(path.join(rcScripts, 'remote-control-scenario.mjs'), 'utf8');
  const join = scenario.slice(scenario.indexOf('async function joinWebHarness('), scenario.indexOf('async function waitForMatchingWindow('));
  assert.ok(!join.includes('bankedControllerSummaryLines'), 'joinWebHarness must not bank');
  assert.match(scenario, /rejoinWebHarness: async \(\) => \{[^}]*bankedControllerSummaryLines\.push\(\.\.\.\(await readControllerSummaryLines\(cdp\)\)\);\s*const rejoined = await joinWebHarness/);
});
