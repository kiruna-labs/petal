#!/usr/bin/env node
// rc-latency gate: the run-level remote-control latency verdict must go RED on
// a regression and on missing data, and GREEN on a healthy run (CLAUDE.md rule
// 8: test a gate in both directions). Fixtures are shaped like the real VM
// artifacts (petal-dev.log `remote-control-latency:` lines, the numbered
// suite's results JSON, the press-to-photon report).
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  LATENCY_CHECKS,
  collectLatencyMetrics,
  evaluateLatencyGate,
  pairControllerPublishes,
  parseHostLatencyLog,
  resolveLatencyBudgets,
} from './remote-control-latency-gate.mjs';
import { armObservationWatch, parseWatchLine } from './remote-control-observation-watch.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '../../..');
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
    pressToEstimatedPhotonMs: 110 + (i % 10) * 6 + slowMs,
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
    'press-to-photon-p95',
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

test('an incomplete press-to-photon run is insufficient even if its p95 looks fine', () => {
  const run = syntheticRun();
  run.photon.summary.pressToPhoton.sampleFailures = [{ sample: 3, inputKind: 'click', error: 'timeout' }];
  const verdict = evaluateLatencyGate(collectLatencyMetrics(run), budgets);
  assert.equal(verdict.checks.find((c) => c.id === 'press-to-photon-p95').status, 'insufficient');
  assert.equal(verdict.pass, false);
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
  assert.match(workflow, /remote-control-latency-gate\.mjs/);
  assert.match(workflow, /--press-to-photon/);
});

test('watcher protocol lines parse, and garbage is ignored', () => {
  assert.deepEqual(parseWatchLine('READY 1792198601528.4 present=false'), { type: 'ready', atMs: 1792198601528.4, present: false });
  assert.deepEqual(parseWatchLine('SEEN 1792198601612.0'), { type: 'seen', atMs: 1792198601612 });
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
    spawnImpl: fakeSpawn(['READY 1000.0 present=false', 'SEEN 1084.5']),
  });
  assert.equal(await watch.waitSeen(), 1084.5);
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
