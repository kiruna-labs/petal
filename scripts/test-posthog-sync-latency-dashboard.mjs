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

test('the plan creates, updates and leaves alone, matched by name', () => {
  assert.ok(planSync(insights).every((step) => step.action === 'create'));
  const [first, second, third] = insights;
  const existing = [
    { id: 1, ...insightPayload(first) },
    { id: 2, ...insightPayload({ ...second, query: 'SELECT 1' }) },
    { id: 3, name: third.name, description: 'old', query: { kind: 'HogQLQuery', query: third.query } },
    { id: 4, name: 'Someone else', query: {} },
  ];
  const plan = planSync(insights, existing);
  assert.deepEqual(plan.slice(0, 3).map((step) => [step.action, step.id, step.changed]), [
    ['unchanged', 1, []],
    ['update', 2, ['query']],
    ['update', 3, ['description']],
  ]);
  assert.deepEqual(plan.at(-1), { action: 'not-in-file', name: 'Someone else', id: 4 });
  assert.equal(plan[1].payload.query.source.kind, 'HogQLQuery');
  assert.match(formatPlan(plan), /^DRY RUN: dashboard "Remote-control latency" in PostHog project 317298\. Nothing is sent\./);
  assert.match(formatPlan(plan), /6 to create, 2 to update, 1 unchanged, 1 not in the file\./);
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
    assert.match(out.pop(), /9 to create, 0 to update, 0 unchanged, 0 not in the file\./);

    const snapshot = JSON.stringify({ results: [{ id: 7, ...insightPayload(insights[0]) }] });
    const readFile = (file) => (file === 'snapshot.json' ? snapshot : readFileSync(file, 'utf8'));
    assert.equal(main(['--existing', 'snapshot.json', '--json'], { ...io, readFile }), 0);
    const plan = JSON.parse(out.pop());
    assert.deepEqual(plan[0], { ...plan[0], action: 'unchanged', id: 7 });

    assert.equal(main(['--apply'], io), 2);
    assert.match(err.pop(), /dry run only/);
    assert.equal(main(['--existing'], io), 2);
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('the CLI prints the dry-run plan', () => {
  const run = spawnSync(process.execPath, [path.join(scriptDir, 'posthog-sync-latency-dashboard.mjs')], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.match(run.stdout, /^DRY RUN:/);
  assert.match(run.stdout, /create\s+RC latency: decision tile/);
});
