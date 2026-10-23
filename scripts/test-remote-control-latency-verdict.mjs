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
  LATENCY_CHECKS,
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
function syntheticRun({ slowMs = 0, events = 60, observations = 21, photon = 40 } = {}) {
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
  const photonReport = {
    summary: { pressToPhoton: { expectedSamples: photon, sampleFailures: [], timingSamples, byInput: {} } },
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
  // Not gated: a run with no summary lines still produces the same checks.
  const metrics = collectLatencyMetrics({ hostLog: log });
  assert.equal(metrics.latencySummary.replayComplete.p95MaxMs, 131);
  assert.ok(!LATENCY_CHECKS.some((check) => check.metric === 'latencySummary'));
  const verdict = latencyVerdict({ hostLog: log });
  assert.ok(verdict.notes.some((note) => note.includes('latency summary (#370, not gated): 2 host interval(s), 1 final')));
});
