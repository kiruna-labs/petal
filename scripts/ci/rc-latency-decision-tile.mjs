#!/usr/bin/env node
// #379: the remote-control latency decision tile, evaluated in JS. Mirrors the
// `decision-tile` insight in docs/telemetry/remote-control-latency-dashboard.sql
// (tests keep the rule ids and thresholds in lockstep with that SQL).
//
// Input: PostHog events `{ event, timestamp, properties }`; only
// `remote_control_latency_summary` counts. One row per rule: FIRES, CLEAR or
// INSUFFICIENT. A stage with no data, or too few sessions or days behind it,
// is INSUFFICIENT, never a guessed 0. Host and controller mint their own
// `rc_session_id`, so the two sides cannot be joined; multi-stage rules
// compare fleet medians.
//
// CLI: node scripts/ci/rc-latency-decision-tile.mjs <events.json> [--now <iso>]
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

import { nearestRank } from './remote-control-latency-verdict.mjs';

export const EVENT_NAME = 'remote_control_latency_summary';
export const WINDOW_DAYS = 14;
export const MIN_SESSIONS = 50;
export const MIN_SPAN_DAYS = 14;
// An interval's percentile counts only when its stage has this many samples.
export const MIN_INTERVAL_SAMPLES = 5;

// Option (b) stages a photon is made of, upstream of the receiver.
export const SEND_NETWORK_STAGES = [
  'input_leg_ms',
  'replay_inject_ms',
  'inject_to_capture_ms',
  'capture_to_encoded_ms',
  'encoded_to_packetized_ms',
  'video_net_ms',
];
export const PHOTON_SHARE_STAGES = [...SEND_NETWORK_STAGES, 'jb_delay_ms', 'decode_ms', 'receive_to_present_ms'];

// Upper bound of each bounded, non-zero `jb_min_ms` bucket. `0` and
// `200_plus` give no usable bound, so those intervals are left out.
export const JB_MIN_UPPER_MS = { '1_19': 19, '20_49': 49, '50_99': 99, '100_199': 199 };

const DAY_MS = 86_400_000;

function num(value) {
  if (value === null || value === undefined || value === '') return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function timeMs(timestamp) {
  return typeof timestamp === 'number' ? timestamp : Date.parse(timestamp);
}

function median(values) {
  return nearestRank(values, 0.5);
}

// Distinct sessions and the inclusive calendar-day span (UTC) of a set of
// intervals: the minimum-data guard every rule applies to its own data.
export function coverage(intervals) {
  if (intervals.length === 0) return { sessions: 0, spanDays: 0 };
  const days = intervals.map((event) => Math.floor(event.ts / DAY_MS));
  return {
    sessions: new Set(intervals.map((event) => event.p.rc_session_id)).size,
    spanDays: Math.max(...days) - Math.min(...days) + 1,
  };
}

function enough({ sessions, spanDays }) {
  return sessions >= MIN_SESSIONS && spanDays >= MIN_SPAN_DAYS;
}

// Intervals with enough samples of `stage` AND a value for the field read.
function withStage(events, stage, { field = 'p50', minSamples = MIN_INTERVAL_SAMPLES } = {}) {
  return events.filter((event) => (num(event.p[`${stage}_n`]) ?? 0) >= minSamples && num(event.p[`${stage}_${field}`]) !== null);
}

// Fleet median of one stage's per-interval p50, or null when the stage lacks
// the data to decide on.
function fleetStage(events, stage) {
  const intervals = withStage(events, stage);
  const cov = coverage(intervals);
  return { stage, ...cov, ok: enough(cov), median: median(intervals.map((event) => num(event.p[`${stage}_p50`]))) };
}

function verdict(ok, fires) {
  if (!ok) return 'INSUFFICIENT';
  return fires ? 'FIRES' : 'CLEAR';
}

function singleStageRule(stage, field, limit) {
  return (events) => {
    const intervals = withStage(events, stage, { field });
    const cov = coverage(intervals);
    const value = median(intervals.map((event) => num(event.p[`${stage}_${field}`])));
    return { value, ...cov, fires: verdict(enough(cov), value > limit) };
  };
}

// `sql` lists the comparisons the SQL row must contain, so the two stay in
// lockstep (scripts/test-rc-latency-decision-tile.mjs).
export const RULES = [
  {
    rule: 'a_input_leg_p50_share',
    option: 'a',
    threshold: '>= 25% of sessions with median input_leg_ms_p50 > 20 ms',
    sql: ['session_p50 > 20', 'value >= 0.25'],
    evaluate(events) {
      const intervals = withStage(events, 'input_leg_ms');
      const bySession = new Map();
      for (const event of intervals) {
        const list = bySession.get(event.p.rc_session_id) ?? [];
        list.push(num(event.p.input_leg_ms_p50));
        bySession.set(event.p.rc_session_id, list);
      }
      const medians = [...bySession.values()].map(median);
      const value = medians.length ? medians.filter((ms) => ms > 20).length / medians.length : null;
      const cov = coverage(intervals);
      return { value, ...cov, fires: verdict(enough(cov), value >= 0.25) };
    },
  },
  {
    rule: 'a_input_leg_p95_fleet',
    option: 'a',
    threshold: 'fleet median of input_leg_ms_p95 > 60 ms',
    sql: ['value > 60'],
    evaluate: singleStageRule('input_leg_ms', 'p95', 60),
  },
  {
    rule: 'a_stall_minutes',
    option: 'a',
    threshold: '> 10% of control-minutes with input_stall_count > 0',
    sql: ['value > 0.1'],
    evaluate(events) {
      // The detector ran in a minute that recorded input_excess_delay_ms;
      // input_stall_count is omitted when 0, so only those minutes count.
      const minutes = withStage(events, 'input_excess_delay_ms', { minSamples: 1 });
      const value = minutes.length ? minutes.filter((event) => (num(event.p.input_stall_count) ?? 0) > 0).length / minutes.length : null;
      const cov = coverage(minutes);
      return { value, ...cov, fires: verdict(enough(cov), value > 0.1) };
    },
  },
  {
    rule: 'a_cross_edge_penalty',
    option: 'a',
    threshold: 'cross-edge minus same-edge session median input_leg_ms_p50 > 15 ms with > 30% cross-edge sessions',
    sql: ['value > 15', 'cross_share > 0.3'],
    evaluate(events) {
      // A session's edge is its latest interval's; one that ends `unknown`
      // (or unset) is left out, never classified from an older value.
      const all = new Map();
      for (const event of [...withStage(events, 'input_leg_ms')].sort((a, b) => a.ts - b.ts)) {
        const entry = all.get(event.p.rc_session_id) ?? { values: [], events: [], edge: null };
        entry.values.push(num(event.p.input_leg_ms_p50));
        entry.events.push(event);
        entry.edge = String(event.p.same_edge ?? 'unknown');
        all.set(event.p.rc_session_id, entry);
      }
      const sessions = new Map([...all].filter(([, entry]) => entry.edge === 'true' || entry.edge === 'false'));
      const intervals = [...sessions.values()].flatMap((entry) => entry.events);
      const group = (edge) => [...sessions.values()].filter((entry) => entry.edge === edge).map((entry) => median(entry.values));
      const cross = group('false');
      const same = group('true');
      const cov = coverage(intervals);
      const ok = enough(cov) && cross.length > 0 && same.length > 0;
      const value = ok ? median(cross) - median(same) : null;
      const crossShare = sessions.size ? cross.length / sessions.size : null;
      return {
        value,
        ...cov,
        detail: crossShare === null ? '' : `cross-edge share ${crossShare.toFixed(3)}`,
        fires: verdict(ok, value > 15 && crossShare > 0.3),
      };
    },
  },
  {
    rule: 'b_jb_delay',
    option: 'b',
    threshold: 'fleet median of jb_delay_ms_p50 > 30 ms',
    sql: ['value > 30'],
    evaluate: singleStageRule('jb_delay_ms', 'p50', 30),
  },
  {
    rule: 'b_jb_target_vs_min',
    option: 'b',
    threshold: 'fleet median of jb_target_ms_p50 / jb_min_ms bucket upper bound > 2',
    sql: ['value > 2'],
    evaluate(events) {
      const intervals = withStage(events, 'jb_target_ms').filter((event) => JB_MIN_UPPER_MS[event.p.jb_min_ms] !== undefined);
      const value = median(intervals.map((event) => num(event.p.jb_target_ms_p50) / JB_MIN_UPPER_MS[event.p.jb_min_ms]));
      const cov = coverage(intervals);
      return { value, ...cov, fires: verdict(enough(cov), value > 2) };
    },
  },
  {
    rule: 'b_receive_to_present',
    option: 'b',
    threshold: 'fleet median of receive_to_present_ms_p50 > 25 ms',
    sql: ['value > 25'],
    evaluate: singleStageRule('receive_to_present_ms', 'p50', 25),
  },
  {
    rule: 'b_photon_residual',
    option: 'b',
    threshold: 'fleet median photon_ms_p50 minus send and network stage medians > 50 ms',
    sql: ['value > 50'],
    evaluate(events) {
      const photon = fleetStage(events, 'photon_ms');
      const stages = SEND_NETWORK_STAGES.map((stage) => fleetStage(events, stage));
      const missing = [photon, ...stages].filter((entry) => !entry.ok).map((entry) => entry.stage);
      const ok = missing.length === 0;
      const value = ok ? photon.median - stages.reduce((sum, entry) => sum + entry.median, 0) : null;
      return {
        value,
        sessions: photon.sessions,
        spanDays: photon.spanDays,
        detail: ok ? '' : `insufficient: ${missing.join(', ')}`,
        fires: verdict(ok, value > 50),
      };
    },
  },
  {
    rule: 'capture_cadence',
    option: 'b',
    threshold: 'fleet median of inject_to_capture_ms_p50 > 33 ms',
    sql: ['value > 33'],
    evaluate: singleStageRule('inject_to_capture_ms', 'p50', 33),
  },
  {
    rule: 'stage_share_of_photon',
    option: 'either',
    threshold: 'any stage median > 40% of fleet median photon_ms_p50',
    sql: ['value > 0.4'],
    evaluate(events) {
      const photon = fleetStage(events, 'photon_ms');
      const base = { sessions: photon.sessions, spanDays: photon.spanDays };
      if (!photon.ok || !(photon.median > 0)) {
        return { value: null, ...base, detail: 'insufficient: photon_ms', fires: 'INSUFFICIENT' };
      }
      const stages = PHOTON_SHARE_STAGES.map((stage) => fleetStage(events, stage));
      const present = stages.filter((entry) => entry.ok);
      const missing = stages.filter((entry) => !entry.ok).map((entry) => entry.stage);
      const worst = present.reduce((top, entry) => (top && top.median >= entry.median ? top : entry), null);
      const value = worst ? worst.median / photon.median : null;
      if (value > 0.4) return { value, ...base, detail: worst.stage, fires: 'FIRES' };
      // A missing stage could be the one over 40%: no guess.
      if (missing.length) return { value, ...base, detail: `insufficient: ${missing.join(', ')}`, fires: 'INSUFFICIENT' };
      return { value, ...base, detail: worst.stage, fires: 'CLEAR' };
    },
  },
];

// Normalise and filter: the summary event only, inside the window, and never
// a self-hosted SFU (its edge and network numbers are not the fleet's).
export function decisionEvents(events, { now = Date.now(), windowDays = WINDOW_DAYS } = {}) {
  const since = windowDays === null ? -Infinity : timeMs(now) - windowDays * DAY_MS;
  return (events ?? [])
    .filter((event) => event?.event === EVENT_NAME)
    .map((event) => ({ ts: timeMs(event.timestamp), p: event.properties ?? {} }))
    .filter((event) => Number.isFinite(event.ts) && event.ts > since)
    .filter((event) => String(event.p.self_hosted_sfu) !== 'true');
}

export function evaluateDecisionTile(events, options = {}) {
  const filtered = decisionEvents(events, options);
  return RULES.map(({ rule, option, threshold, evaluate }) => {
    const result = evaluate(filtered);
    const value = result.value === null || result.value === undefined || !Number.isFinite(result.value) ? null : Math.round(result.value * 1000) / 1000;
    return { rule, option, value, threshold, fires: result.fires, sessions: result.sessions, spanDays: result.spanDays, detail: result.detail ?? '' };
  });
}

export function formatDecisionTable(rows) {
  const header = ['rule', 'value', 'fires', 'sessions', 'days', 'threshold', 'detail'];
  const body = rows.map((row) => [row.rule, row.value === null ? '-' : String(row.value), row.fires, String(row.sessions), String(row.spanDays), row.threshold, row.detail]);
  const widths = header.map((_, column) => Math.max(header[column].length, ...body.map((cells) => cells[column].length)));
  return [header, ...body].map((cells) => cells.map((cell, column) => cell.padEnd(widths[column])).join('  ').trimEnd()).join('\n');
}

function main(argv) {
  const nowAt = argv.indexOf('--now');
  const file = argv.find((arg, index) => !arg.startsWith('--') && (nowAt < 0 || index !== nowAt + 1));
  if (!file) {
    console.error('usage: rc-latency-decision-tile.mjs <events.json> [--now <iso>]');
    return 2;
  }
  const parsed = JSON.parse(readFileSync(file, 'utf8'));
  const events = Array.isArray(parsed) ? parsed : parsed.results ?? [];
  const now = nowAt >= 0 ? Date.parse(argv[nowAt + 1]) : Date.now();
  if (!Number.isFinite(now)) {
    console.error(`invalid --now: ${argv[nowAt + 1]}`);
    return 2;
  }
  console.log(formatDecisionTable(evaluateDecisionTile(events, { now })));
  return 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  process.exitCode = main(process.argv.slice(2));
}
