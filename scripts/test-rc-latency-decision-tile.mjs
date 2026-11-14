#!/usr/bin/env node
// #379: the decision-tile evaluator must fire and clear every rule once, and
// report a stage it has no data for as INSUFFICIENT, never a guessed 0. The
// rule ids and thresholds must match the SQL decision tile.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { RULES, evaluateDecisionTile, formatDecisionTable } from './ci/rc-latency-decision-tile.mjs';
import { SQL_PATH, parseDashboardSql } from './posthog-sync-latency-dashboard.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const NOW = Date.parse('2026-11-13T12:00:00Z');
const DAY = 86_400_000;

const HEALTHY_HOST = {
  input_leg_ms: [10, 30],
  replay_inject_ms: [3, 9],
  inject_to_capture_ms: [16, 30],
  capture_to_encoded_ms: [8, 12],
  encoded_to_packetized_ms: [2, 4],
  input_excess_delay_ms: [1, 5],
};
const HEALTHY_CONTROLLER = {
  video_net_ms: [15, 25],
  jb_delay_ms: [15, 30],
  jb_target_ms: [20, 40],
  decode_ms: [3, 6],
  receive_to_present_ms: [10, 20],
  photon_ms: [90, 140],
};

function stageProps(stages) {
  const props = {};
  for (const [stage, value] of Object.entries(stages)) {
    if (value === null) continue;
    const [p50, p95] = value;
    Object.assign(props, { [`${stage}_p50`]: p50, [`${stage}_p95`]: p95, [`${stage}_max`]: p95, [`${stage}_n`]: 10 });
  }
  return props;
}

// `sessions` host+controller session pairs, spread over `spanDays` calendar
// days, two control-minutes per side. `host`/`controller` may be functions of
// the session index to vary stages per session.
function fleet({ sessions = 60, spanDays = 14, host = {}, controller = {}, hostDims = () => ({}), controllerDims = {}, extra = {} } = {}) {
  const events = [];
  for (let s = 0; s < sessions; s += 1) {
    const base = NOW - (s % spanDays) * DAY - 3_600_000;
    const hostStages = { ...HEALTHY_HOST, ...(typeof host === 'function' ? host(s) : host) };
    const controllerStages = { ...HEALTHY_CONTROLLER, ...(typeof controller === 'function' ? controller(s) : controller) };
    for (let minute = 0; minute < 2; minute += 1) {
      const timestamp = new Date(base + minute * 60_000).toISOString();
      events.push({
        event: 'remote_control_latency_summary',
        timestamp,
        properties: { role: 'host', rc_session_id: `h${s}`, interval_s: 60, final: false, client: 'native', same_edge: s % 2 ? 'false' : 'true', self_hosted_sfu: 'false', ...stageProps(hostStages), ...hostDims(s, minute), ...extra },
      });
      events.push({
        event: 'remote_control_latency_summary',
        timestamp,
        properties: { role: 'controller', rc_session_id: `c${s}`, interval_s: 60, final: false, client: 'web', jb_min_ms: '20_49', self_hosted_sfu: 'false', ...stageProps(controllerStages), ...controllerDims, ...extra },
      });
    }
  }
  return events;
}

function verdicts(events) {
  return Object.fromEntries(evaluateDecisionTile(events, { now: NOW }).map((row) => [row.rule, row.fires]));
}

test('a healthy fleet clears every rule', () => {
  const rows = evaluateDecisionTile(fleet(), { now: NOW });
  assert.deepEqual(rows.map((row) => row.rule), RULES.map((rule) => rule.rule));
  for (const row of rows) assert.equal(row.fires, 'CLEAR', `${row.rule}: ${JSON.stringify(row)}`);
  const residual = rows.find((row) => row.rule === 'b_photon_residual');
  assert.equal(residual.value, 90 - (10 + 3 + 16 + 8 + 2 + 15));
});

const FIRING = {
  a_input_leg_p50_share: { host: (s) => (s % 10 < 3 ? { input_leg_ms: [25, 30] } : {}) },
  a_input_leg_p95_fleet: { host: { input_leg_ms: [10, 70] } },
  a_stall_minutes: { hostDims: (s, minute) => (s % 4 === 0 && minute === 0 ? { input_stall_count: 1 } : {}) },
  a_cross_edge_penalty: { host: (s) => ({ input_leg_ms: s % 2 ? [19, 30] : [3, 10] }) },
  b_jb_delay: { controller: { jb_delay_ms: [35, 50] } },
  b_jb_target_vs_min: { controller: { jb_target_ms: [120, 160] } },
  b_receive_to_present: { controller: { receive_to_present_ms: [30, 45] } },
  b_photon_residual: { controller: { photon_ms: [150, 200] } },
  capture_cadence: { host: { inject_to_capture_ms: [40, 60] } },
  stage_share_of_photon: { controller: { decode_ms: [40, 60] } },
};

test('every rule fires on its own fixture', () => {
  assert.deepEqual(Object.keys(FIRING).sort(), RULES.map((rule) => rule.rule).sort());
  for (const [rule, options] of Object.entries(FIRING)) {
    assert.equal(verdicts(fleet(options))[rule], 'FIRES', rule);
  }
});

test('thresholds are strict where the rule says so', () => {
  // 20 ms is not > 20 ms, and a penalty with too few cross-edge sessions clears.
  assert.equal(verdicts(fleet({ host: { input_leg_ms: [20, 30] } })).a_input_leg_p50_share, 'CLEAR');
  assert.equal(verdicts(fleet({ host: { input_leg_ms: [10, 60] } })).a_input_leg_p95_fleet, 'CLEAR');
  const fewCross = fleet({
    host: (s) => ({ input_leg_ms: s % 5 === 0 ? [30, 40] : [3, 10] }),
    hostDims: (s) => ({ same_edge: s % 5 === 0 ? 'false' : 'true' }),
  });
  const row = evaluateDecisionTile(fewCross, { now: NOW }).find((entry) => entry.rule === 'a_cross_edge_penalty');
  assert.equal(row.value, 27);
  assert.equal(row.fires, 'CLEAR');
  assert.match(row.detail, /cross-edge share 0\.200/);
});

test('an absent stage is INSUFFICIENT, never a guessed 0', () => {
  const noPhoton = verdicts(fleet({ controller: { photon_ms: null } }));
  assert.equal(noPhoton.b_photon_residual, 'INSUFFICIENT');
  assert.equal(noPhoton.stage_share_of_photon, 'INSUFFICIENT');
  assert.equal(noPhoton.b_jb_delay, 'CLEAR');

  // Without the stall detector's stage there is no denominator, even though
  // no event carries a stall.
  assert.equal(verdicts(fleet({ host: { input_excess_delay_ms: null } })).a_stall_minutes, 'INSUFFICIENT');
  assert.equal(verdicts(fleet({ host: { input_leg_ms: null } })).a_input_leg_p95_fleet, 'INSUFFICIENT');
  assert.equal(verdicts(fleet({ controllerDims: { jb_min_ms: '0' } })).b_jb_target_vs_min, 'INSUFFICIENT');
  assert.equal(verdicts(fleet({ hostDims: () => ({ same_edge: 'true' }) })).a_cross_edge_penalty, 'INSUFFICIENT');

  // A missing stage could be the one over 40% of photon...
  const noNet = evaluateDecisionTile(fleet({ controller: { video_net_ms: null } }), { now: NOW });
  const share = noNet.find((row) => row.rule === 'stage_share_of_photon');
  assert.equal(share.fires, 'INSUFFICIENT');
  assert.match(share.detail, /video_net_ms/);
  assert.equal(noNet.find((row) => row.rule === 'b_photon_residual').fires, 'INSUFFICIENT');
  // ...but a present stage over it still fires.
  assert.equal(verdicts(fleet({ controller: { video_net_ms: null, decode_ms: [40, 60] } })).stage_share_of_photon, 'FIRES');

  // A stage with samples but no value for the percentile a rule reads.
  const noP95 = fleet({ host: { input_leg_ms: [30, 70] } }).map((event) => {
    const { input_leg_ms_p95: _drop, ...properties } = event.properties;
    return { ...event, properties };
  });
  assert.equal(verdicts(noP95).a_input_leg_p95_fleet, 'INSUFFICIENT');

  // A session whose latest edge is `unknown` is not classified from an older
  // value: here every cross-edge session ends unknown.
  const endsUnknown = fleet({
    host: (s) => ({ input_leg_ms: s % 2 ? [19, 30] : [3, 10] }),
    hostDims: (s, minute) => (s % 2 && minute === 1 ? { same_edge: 'unknown' } : {}),
  });
  assert.equal(verdicts(endsUnknown).a_cross_edge_penalty, 'INSUFFICIENT');

  // Fewer than 5 samples in an interval do not count.
  const thin = fleet({ controller: { jb_delay_ms: [35, 50] } }).map((event) =>
    event.properties.jb_delay_ms_n ? { ...event, properties: { ...event.properties, jb_delay_ms_n: 4 } } : event,
  );
  assert.equal(verdicts(thin).b_jb_delay, 'INSUFFICIENT');
});

test('the minimum-data guard needs 50 sessions over 14 calendar days', () => {
  const firing = { host: { input_leg_ms: [30, 70] }, controller: { jb_delay_ms: [35, 50] } };
  assert.equal(verdicts(fleet({ ...firing, sessions: 50 })).a_input_leg_p95_fleet, 'FIRES');
  for (const verdict of Object.values(verdicts(fleet({ ...firing, sessions: 49 })))) assert.equal(verdict, 'INSUFFICIENT');
  for (const verdict of Object.values(verdicts(fleet({ ...firing, spanDays: 13 })))) assert.equal(verdict, 'INSUFFICIENT');
  for (const verdict of Object.values(verdicts([]))) assert.equal(verdict, 'INSUFFICIENT');
});

test('self-hosted SFUs, other events and old events are left out', () => {
  const selfHosted = fleet({ host: { input_leg_ms: [30, 70] }, extra: { self_hosted_sfu: 'true' } });
  assert.equal(verdicts([...fleet(), ...selfHosted]).a_input_leg_p95_fleet, 'CLEAR');
  assert.equal(verdicts(selfHosted).a_input_leg_p95_fleet, 'INSUFFICIENT');
  const unnamed = fleet({ host: { input_leg_ms: [30, 70] } }).map(({ event: _name, ...rest }) => rest);
  assert.equal(verdicts(unnamed).a_input_leg_p95_fleet, 'INSUFFICIENT');
  // 49 sessions plus intervals with no session id are still 49 sessions.
  const noId = fleet({ sessions: 1, host: { input_leg_ms: [30, 70] } }).map((event) => {
    const { rc_session_id: _drop, ...properties } = event.properties;
    return { ...event, properties };
  });
  assert.equal(verdicts([...fleet({ sessions: 49, host: { input_leg_ms: [30, 70] } }), ...noId]).a_input_leg_p95_fleet, 'INSUFFICIENT');
  const other = fleet({ host: { input_leg_ms: [30, 70] } }).map((event) => ({ ...event, event: 'meeting_joined' }));
  assert.equal(verdicts(other).a_input_leg_p95_fleet, 'INSUFFICIENT');
  const old = fleet({ host: { input_leg_ms: [30, 70] } }).map((event) => ({ ...event, timestamp: Date.parse(event.timestamp) - 30 * DAY }));
  assert.equal(verdicts([...fleet(), ...old]).a_input_leg_p95_fleet, 'CLEAR');
});

test('rule ids and thresholds match the SQL decision tile', () => {
  const tile = parseDashboardSql(readFileSync(SQL_PATH, 'utf8')).find((insight) => insight.key === 'decision-tile');
  assert.ok(tile, 'decision-tile insight');
  const segments = tile.query.split(/UNION ALL/);
  const sqlRules = segments.map((segment) => segment.match(/'([a-z0-9_]+)' AS rule/)?.[1]).filter(Boolean);
  assert.deepEqual(sqlRules, RULES.map((rule) => rule.rule));
  for (const rule of RULES) {
    const segment = segments.find((entry) => entry.includes(`'${rule.rule}' AS rule`));
    assert.ok(segment.includes(`'${rule.threshold}' AS threshold`), `${rule.rule}: threshold text`);
    for (const comparison of rule.sql) assert.ok(segment.includes(comparison), `${rule.rule}: SQL lacks "${comparison}"`);
    assert.ok(segment.includes("'INSUFFICIENT'"), `${rule.rule}: no INSUFFICIENT branch`);
  }
  for (const constant of ['>= 50', '>= 14', '>= 5']) assert.ok(tile.query.includes(constant), constant);
});

test('the CLI prints the rule table for an events file', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'rc-decision-'));
  const file = path.join(dir, 'events.json');
  writeFileSync(file, JSON.stringify({ results: fleet(FIRING.b_jb_delay) }));
  const run = spawnSync(process.execPath, [path.join(scriptDir, 'ci/rc-latency-decision-tile.mjs'), '--now', new Date(NOW).toISOString(), file], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.match(run.stdout, /^b_jb_delay\s+35\s+FIRES\s+60\s+14\s/m);
  assert.equal(run.stdout.trim(), formatDecisionTable(evaluateDecisionTile(fleet(FIRING.b_jb_delay), { now: NOW })));
  assert.equal(spawnSync(process.execPath, [path.join(scriptDir, 'ci/rc-latency-decision-tile.mjs')], { encoding: 'utf8' }).status, 2);
});
