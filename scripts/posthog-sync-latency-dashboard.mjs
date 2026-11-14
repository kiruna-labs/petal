#!/usr/bin/env node
// #379: plan the "Remote-control latency" PostHog dashboard from
// docs/telemetry/remote-control-latency-dashboard.sql. DRY RUN ONLY: it never
// calls PostHog and has no apply mode. It prints, per insight, whether it
// would be created, updated or left unchanged against an optional snapshot of
// the existing insights (`--existing <file.json>`: an array, or `{ results }`,
// of `{ id, name, description, query }` as the insights API returns them).
// `--json` prints the insight payloads the plan would send.
//
// Usage: node scripts/posthog-sync-latency-dashboard.mjs [--existing f] [--json]
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const PROJECT_ID = 317298;
export const DASHBOARD_NAME = 'Remote-control latency';
export const SQL_PATH = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../docs/telemetry/remote-control-latency-dashboard.sql');

// Splits the file into `-- insight: <key>` blocks. Header comment lines
// (`-- name:`, `-- description:`, `-- self-hosted:`) end at the first line
// that is not a header; everything after is the HogQL query.
export function parseDashboardSql(text) {
  const insights = [];
  let current = null;
  for (const line of String(text).split('\n')) {
    const start = line.match(/^--\s*insight:\s*(\S+)\s*$/);
    if (start) {
      current = { key: start[1], name: '', description: '', selfHosted: 'excluded', lines: [], inHeader: true };
      insights.push(current);
      continue;
    }
    if (!current) continue;
    const header = current.inHeader && line.match(/^--\s*(name|description|self-hosted):\s*(.*)$/);
    if (header) {
      if (header[1] === 'self-hosted') current.selfHosted = header[2].trim();
      else current[header[1]] = header[2].trim();
      continue;
    }
    current.inHeader = false;
    current.lines.push(line);
  }
  return insights.map(({ key, name, description, selfHosted, lines }) => {
    const query = lines.join('\n').trim().replace(/;\s*$/, '');
    if (!name || !query) throw new Error(`insight ${key}: needs a -- name: header and a query`);
    return { key, name, description, selfHosted, query };
  });
}

export function insightPayload(insight) {
  return {
    name: insight.name,
    description: insight.description,
    query: { kind: 'DataVisualizationNode', source: { kind: 'HogQLQuery', query: insight.query } },
    tags: ['rc-latency', `rc-latency:${insight.key}`],
  };
}

function existingQuery(entry) {
  return entry?.query?.source?.query ?? entry?.query?.query ?? null;
}

// Matched by name. Existing insights the file does not define are reported
// and left alone; nothing is ever deleted.
export function planSync(insights, existing = []) {
  const byName = new Map(existing.map((entry) => [entry.name, entry]));
  const names = new Set(insights.map((insight) => insight.name));
  const actions = insights.map((insight) => {
    const payload = insightPayload(insight);
    const current = byName.get(insight.name);
    if (!current) return { action: 'create', key: insight.key, name: insight.name, payload };
    const changed = [];
    if (existingQuery(current) !== insight.query) changed.push('query');
    if ((current.description ?? '') !== insight.description) changed.push('description');
    return { action: changed.length ? 'update' : 'unchanged', key: insight.key, name: insight.name, id: current.id, changed, payload };
  });
  const untracked = existing.filter((entry) => !names.has(entry.name)).map((entry) => ({ action: 'not-in-file', name: entry.name, id: entry.id }));
  return [...actions, ...untracked];
}

export function formatPlan(plan) {
  const lines = [`DRY RUN: dashboard "${DASHBOARD_NAME}" in PostHog project ${PROJECT_ID}. Nothing is sent.`];
  for (const step of plan) {
    const id = step.id === undefined ? '' : ` (id ${step.id})`;
    const changed = step.changed?.length ? ` [${step.changed.join(', ')}]` : '';
    const note = step.action === 'not-in-file' ? ' -- left alone' : '';
    lines.push(`  ${step.action.padEnd(11)} ${step.name}${id}${changed}${note}`);
  }
  const count = (action) => plan.filter((step) => step.action === action).length;
  lines.push(`${count('create')} to create, ${count('update')} to update, ${count('unchanged')} unchanged, ${count('not-in-file')} not in the file.`);
  return lines.join('\n');
}

export function main(argv, { readFile = (file) => readFileSync(file, 'utf8'), log = console.log, error = console.error } = {}) {
  const known = new Set(['--existing', '--json']);
  const unknown = argv.filter((arg, index) => arg.startsWith('--') && !known.has(arg) && argv[index - 1] !== '--existing');
  if (unknown.length) {
    error(`dry run only; unknown option(s): ${unknown.join(' ')}`);
    return 2;
  }
  const at = argv.indexOf('--existing');
  let existing = [];
  if (at >= 0) {
    if (!argv[at + 1]) {
      error('--existing needs a file');
      return 2;
    }
    const parsed = JSON.parse(readFile(argv[at + 1]));
    existing = Array.isArray(parsed) ? parsed : parsed.results ?? [];
  }
  const plan = planSync(parseDashboardSql(readFile(SQL_PATH)), existing);
  log(argv.includes('--json') ? JSON.stringify(plan, null, 2) : formatPlan(plan));
  return 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  process.exitCode = main(process.argv.slice(2));
}
