#!/usr/bin/env node
// #379: the checked-in dashboard SQL may only read properties the
// `remoteControlLatencySummary` contract (plus the common event keys) defines,
// with values the contract allows, and the sync script only ever dry-runs.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { SQL_PATH, formatPlan, insightPayload, main, parseDashboardSql, planSync } from './posthog-sync-latency-dashboard.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '..');
const contract = JSON.parse(readFileSync(path.join(repoRoot, 'contracts/petal-contracts.json'), 'utf8')).remoteControlLatencySummary;
const sqlText = readFileSync(SQL_PATH, 'utf8');
const insights = parseDashboardSql(sqlText);

function commonKeys() {
  const web = readFileSync(path.join(repoRoot, 'web-harness/src/analytics.ts'), 'utf8').match(/COMMON_KEYS = \[([^\]]+)\]/)[1];
  const native = readFileSync(path.join(repoRoot, 'apps/desktop/src-tauri/src/analytics.rs'), 'utf8').match(/COMMON_KEYS: \[&str; \d+\] = \[([^\]]+)\]/)[1];
  const keys = (list) => [...list.matchAll(/['"]([a-z_]+)['"]/g)].map((match) => match[1]);
  assert.deepEqual(keys(web), keys(native), 'web and native COMMON_KEYS agree');
  return keys(web);
}

// Allowed property -> allowed string values (null: any value).
function allowedProperties() {
  const allowed = new Map();
  for (const stage of contract.stages) for (const suffix of contract.stageSuffixes) allowed.set(`${stage}${suffix}`, null);
  for (const [dimension, values] of Object.entries(contract.dimensions)) allowed.set(dimension, values);
  for (const key of [...contract.counters, ...contract.maxCounters, ...contract.alwaysPresent]) allowed.set(key, null);
  allowed.set('role', contract.roles);
  for (const key of commonKeys()) allowed.set(key, null);
  allowed.set('client', ['native', 'web']);
  return allowed;
}

// Returns a list of problems; empty means the query is in lockstep.
function lockstepProblems(query, allowed = allowedProperties()) {
  const problems = [];
  for (const [, name] of query.matchAll(/properties\.([A-Za-z0-9_$]+)/g)) {
    if (!allowed.has(name)) problems.push(`unknown property ${name}`);
  }
  const compared = [
    ...[...query.matchAll(/properties\.(\w+)\s*!?=\s*'([^']*)'/g)].map((match) => [match[1], [match[2]]]),
    ...[...query.matchAll(/properties\.(\w+)\s+IN\s*\(([^)]*)\)/g)].map((match) => [match[1], [...match[2].matchAll(/'([^']*)'/g)].map((value) => value[1])]),
  ];
  for (const [name, values] of compared) {
    const enumValues = allowed.get(name);
    for (const value of values) {
      if (enumValues && !enumValues.includes(value)) problems.push(`${name} has no value '${value}'`);
    }
  }
  return problems;
}

test('the SQL file parses into named insights with unique keys', () => {
  assert.deepEqual(insights.map((insight) => insight.key), [
    'input-leg-by-region',
    'input-leg-by-edge',
    'ice-path-mix',
    'stall-rate-by-region',
    'stage-medians',
    'photon-by-client',
    'jitter-buffer',
    'data-health',
    'decision-tile',
  ]);
  assert.equal(new Set(insights.map((insight) => insight.name)).size, insights.length);
  for (const insight of insights) {
    assert.ok(insight.description, `${insight.key}: description`);
    assert.doesNotMatch(insight.query, /^-- (insight|name|description):/m, `${insight.key}: header leaked into query`);
  }
});

test('every insight reads only contract properties and values', () => {
  for (const insight of insights) assert.deepEqual(lockstepProblems(insight.query), [], insight.key);
});

test('the lockstep check rejects an unknown property or value', () => {
  assert.deepEqual(lockstepProblems("SELECT properties.dropped_since_last FROM events"), ['unknown property dropped_since_last']);
  assert.deepEqual(lockstepProblems("WHERE properties.role = 'viewer'"), ["role has no value 'viewer'"]);
  assert.deepEqual(lockstepProblems("WHERE properties.jb_min_ms IN ('0', '1_20')"), ["jb_min_ms has no value '1_20'"]);
  assert.deepEqual(lockstepProblems("WHERE properties.lk_region = 'any-region' AND properties.photon_ms_n >= 5"), []);
});

// Checked against PostHog's HogQL allowlist (posthog/hogql/functions/) on
// 2026-11-13. `quantileExactLow` is commented out there: use medianExactLow.
const HOGQL_FUNCTIONS = new Set([
  'argMax', 'avg', 'count', 'countIf', 'dateDiff', 'greatest', 'if', 'ifNull', 'max', 'maxIf', 'medianExactLow',
  'medianExactLowIf', 'min', 'minIf', 'multiIf', 'now', 'toFloat', 'toInt', 'toStartOfDay', 'toString', 'uniqExactIf',
]);

test('the SQL calls only HogQL functions known to be allowlisted', () => {
  const code = sqlText.split('\n').filter((line) => !line.trimStart().startsWith('--')).join('\n');
  const called = new Set([...code.matchAll(/\b([A-Za-z_]\w*)\(/g)].map((match) => match[1]).filter((name) => !['IN', 'AS'].includes(name)));
  assert.deepEqual([...called].filter((name) => !HOGQL_FUNCTIONS.has(name)), []);
});

test('every events scan filters the event, the 14-day window and self-hosted SFUs', () => {
  for (const insight of insights) {
    const scans = insight.query.match(/FROM events\b/g).length;
    const count = (needle) => insight.query.split(needle).length - 1;
    assert.equal(count("event = 'remote_control_latency_summary'"), scans, `${insight.key}: event filter`);
    assert.equal(count('timestamp > now() - INTERVAL 14 DAY'), scans, `${insight.key}: window`);
    const exclusions = count("ifNull(toString(properties.self_hosted_sfu), '') != 'true'");
    if (insight.selfHosted === 'included') assert.equal(exclusions, 0, insight.key);
    else assert.equal(exclusions, scans, `${insight.key}: self-hosted exclusion`);
  }
});

test('the plan creates the dashboard and puts every insight on it', () => {
  const fresh = planSync(insights);
  assert.deepEqual(fresh[0], { action: 'create', kind: 'dashboard', name: 'Remote-control latency', payload: { name: 'Remote-control latency', tags: ['rc-latency'] } });
  assert.ok(fresh.slice(1).every((step) => step.action === 'create' && step.kind === 'insight'));
  assert.deepEqual(fresh[1].payload.dashboards, [null]);

  const [first, second, third, fourth] = insights;
  const existing = {
    dashboard: { id: 42, name: 'Remote-control latency' },
    insights: [
      { id: 1, ...insightPayload(first, 42) },
      { id: 2, ...insightPayload({ ...second, query: 'SELECT 1' }, 42) },
      { id: 3, name: third.name, description: 'old', query: { kind: 'HogQLQuery', query: third.query }, dashboards: [42] },
      { id: 5, ...insightPayload(fourth, 7) },
      { id: 4, name: 'Someone else', query: {} },
    ],
  };
  const plan = planSync(insights, existing);
  assert.deepEqual(plan[0], { action: 'unchanged', kind: 'dashboard', name: 'Remote-control latency', id: 42 });
  assert.deepEqual(plan.slice(1, 5).map((step) => [step.action, step.id, step.changed]), [
    ['unchanged', 1, []],
    ['update', 2, ['query']],
    ['update', 3, ['description']],
    ['update', 5, ['dashboard']],
  ]);
  assert.deepEqual(plan[2].payload.dashboards, [42]);
  assert.deepEqual(plan.at(-1), { action: 'not-in-file', kind: 'insight', name: 'Someone else', id: 4 });
  assert.equal(plan[2].payload.query.source.kind, 'HogQLQuery');
  assert.match(formatPlan(plan), /^DRY RUN: dashboard "Remote-control latency" in PostHog project 317298\. Nothing is sent\./);
  assert.match(formatPlan(plan), /Insights: 5 to create, 3 to update, 1 unchanged, 1 not in the file\./);
});

test('main dry-runs without touching the network and refuses other modes', () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = () => {
    throw new Error('network call in a dry run');
  };
  try {
    const out = [];
    const err = [];
    const io = { log: (line) => out.push(line), error: (line) => err.push(line) };
    assert.equal(main([], io), 0);
    assert.match(out.pop(), /Insights: 9 to create, 0 to update, 0 unchanged, 0 not in the file\./);

    const snapshot = JSON.stringify({ dashboard: { id: 9, name: 'Remote-control latency' }, insights: [{ id: 7, ...insightPayload(insights[0], 9) }] });
    const readFile = (file) => (file === 'snapshot.json' ? snapshot : readFileSync(file, 'utf8'));
    assert.equal(main(['--existing', 'snapshot.json', '--json'], { ...io, readFile }), 0);
    const plan = JSON.parse(out.pop());
    assert.deepEqual(plan[1], { ...plan[1], action: 'unchanged', id: 7 });

    const wrong = JSON.stringify({ dashboard: { id: 3, name: 'Unrelated dashboard' }, insights: [] });
    assert.equal(main(['--existing', 'wrong.json'], { ...io, readFile: () => wrong }), 2);
    assert.match(err.pop(), /not "Remote-control latency"/);

    assert.equal(main(['--apply'], io), 2);
    assert.match(err.pop(), /dry run only/);
    assert.equal(main(['--existing'], io), 2);
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('the sync script imports nothing that can reach a network', () => {
  const source = readFileSync(path.join(scriptDir, 'posthog-sync-latency-dashboard.mjs'), 'utf8');
  const imports = [...source.matchAll(/^import .* from '([^']+)';$/gm)].map((match) => match[1]);
  assert.deepEqual(imports.sort(), ['node:fs', 'node:path', 'node:url']);
  assert.doesNotMatch(source, /\bimport\(|\brequire\(|\bfetch\(|child_process/);
});

test('the CLI prints the dry-run plan', () => {
  const run = spawnSync(process.execPath, [path.join(scriptDir, 'posthog-sync-latency-dashboard.mjs')], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.match(run.stdout, /^DRY RUN:/);
  assert.match(run.stdout, /create\s+insight\s+RC latency: decision tile/);
});
