import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { LatencyHistogram, latencyHistogramEdges } from '@petal/shared/logic/latencyHistogram';
import {
  RC_LATENCY_COUNTERS,
  RC_LATENCY_DIMENSIONS,
  RC_LATENCY_DIMENSION_KEYS,
  RC_LATENCY_FINAL_MIN_SAMPLES,
  RC_LATENCY_INTERVAL_MS,
  RC_LATENCY_LOG_PREFIX,
  RC_LATENCY_MAX_COUNTERS,
  RC_LATENCY_MAX_EVENTS_PER_SESSION,
  RC_LATENCY_ROLES,
  RC_LATENCY_STAGES,
  RcLatencyAggregator,
  rcLatencySummaryLogLine,
  rcLatencySummaryProperties,
  type RcLatencySummary,
} from '@petal/shared/logic/rcLatencyAggregator';

const rawContract = readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8');
const contract = JSON.parse(rawContract) as {
  latencyHistogramVectors: Array<{
    name: string;
    samples: Array<number | null>;
    expected: { n: number; rejected: number; p50: number | null; p95: number | null; max: number | null };
  }>;
  remoteControlLatencySummary: {
    stages: string[];
    dimensions: Record<string, string[] | null>;
    counters: string[];
    maxCounters: string[];
    roles: string[];
    intervalMs: number;
    finalMinSamples: number;
    maxEventsPerSession: number;
    logPrefix: string;
    example: ContractExample;
    controllerExample: ContractExample;
  };
};

type ContractExample = {
  summary: RcLatencySummary;
  properties: Record<string, string | number | boolean>;
  logLine: string;
};

function collect() {
  const emitted: RcLatencySummary[] = [];
  const aggregator = new RcLatencyAggregator('controller', {
    emit: (summary) => emitted.push(summary),
    newSessionId: () => 'f'.repeat(32),
  });
  return { aggregator, emitted };
}

test('the TS histogram passes the shared Rust contract vectors', () => {
  assert.equal(latencyHistogramEdges().length, 176);
  assert.ok(contract.latencyHistogramVectors.length >= 5);
  for (const vector of contract.latencyHistogramVectors) {
    const histogram = new LatencyHistogram();
    for (const sample of vector.samples) histogram.record(sample ?? Number.NaN);
    assert.equal(histogram.n, vector.expected.n, `${vector.name} n`);
    assert.equal(histogram.rejected, vector.expected.rejected, `${vector.name} rejected`);
    assert.equal(histogram.percentile(0.5), vector.expected.p50, `${vector.name} p50`);
    assert.equal(histogram.percentile(0.95), vector.expected.p95, `${vector.name} p95`);
    assert.equal(histogram.max(), vector.expected.max, `${vector.name} max`);
  }
});

test('closed keys, order and cadence match the summary contract', () => {
  const schema = contract.remoteControlLatencySummary;
  assert.deepEqual([...RC_LATENCY_STAGES], schema.stages);
  assert.deepEqual([...RC_LATENCY_COUNTERS], schema.counters);
  assert.deepEqual([...RC_LATENCY_MAX_COUNTERS], schema.maxCounters);
  assert.deepEqual([...RC_LATENCY_ROLES], schema.roles);
  // JSON.parse keeps declared key order, which the log line follows.
  assert.deepEqual(RC_LATENCY_DIMENSION_KEYS, Object.keys(schema.dimensions));
  for (const key of RC_LATENCY_DIMENSION_KEYS) {
    const values = RC_LATENCY_DIMENSIONS[key] as readonly string[] | null;
    assert.deepEqual(values === null ? null : [...values], schema.dimensions[key], key);
  }
  assert.equal(RC_LATENCY_INTERVAL_MS, schema.intervalMs);
  assert.equal(RC_LATENCY_FINAL_MIN_SAMPLES, schema.finalMinSamples);
  assert.equal(RC_LATENCY_MAX_EVENTS_PER_SESSION, schema.maxEventsPerSession);
  assert.equal(RC_LATENCY_LOG_PREFIX, schema.logPrefix);
});

test('both contract examples render the exact properties (typed) and log line', () => {
  for (const example of [contract.remoteControlLatencySummary.example, contract.remoteControlLatencySummary.controllerExample]) {
    const summary: RcLatencySummary = {
      ...example.summary,
      counters: example.summary.counters ?? {},
      maxCounters: example.summary.maxCounters ?? {},
    };
    const properties = rcLatencySummaryProperties(summary);
    assert.deepEqual(properties, example.properties);
    assert.deepEqual(Object.keys(properties), Object.keys(example.properties), 'key order');
    assert.equal(rcLatencySummaryLogLine(summary), example.logLine);
  }
});

test('lifecycle: grant starts a session, 60 s emits, release emits a final partial at n >= 5', () => {
  const { aggregator, emitted } = collect();
  assert.equal(aggregator.recordStage('jb_delay_ms', 4), false, 'no session before grant');
  aggregator.sync(true, 0);
  for (const ms of [2, 3, 4, 5, 6, 7]) aggregator.recordStage('jb_delay_ms', ms);
  aggregator.sync(true, 59_000);
  assert.equal(emitted.length, 0);
  aggregator.sync(true, 60_000);
  assert.equal(emitted.length, 1);
  assert.equal(emitted[0].final, false);
  assert.equal(emitted[0].intervalS, 60);
  assert.equal(emitted[0].role, 'controller');
  assert.equal(emitted[0].stages.jb_delay_ms?.n, 6);
  for (const ms of [10, 11, 12, 13, 14]) aggregator.recordStage('capture_to_present_ms', ms);
  aggregator.sync(false, 75_000);
  assert.equal(emitted.length, 2);
  assert.equal(emitted[1].final, true);
  assert.equal(emitted[1].intervalS, 15);
  assert.deepEqual(Object.keys(emitted[1].stages), ['capture_to_present_ms'], 'n = 0 stages are omitted');
  assert.equal(aggregator.active, false);
});

test('a final partial needs five samples, unless it carries a non-zero counter', () => {
  const { aggregator, emitted } = collect();
  aggregator.sync(true, 0);
  for (const ms of [1, 2, 3, 4]) aggregator.recordStage('decode_ms', ms);
  aggregator.sync(false, 5_000);
  assert.equal(emitted.length, 0, 'n < 5');
  aggregator.sync(true, 6_000);
  aggregator.recordStage('decode_ms', 3);
  aggregator.addCounter('freeze_count', 1);
  aggregator.sync(false, 9_000);
  assert.equal(emitted.length, 1, 'a counter-only final partial emits');
  assert.deepEqual(emitted[0].counters, { freeze_count: 1 });
});

test('empty intervals and zero counters never emit or spend the cap', () => {
  const { aggregator, emitted } = collect();
  let now = 0;
  aggregator.sync(true, now);
  assert.ok(aggregator.setDimension('frame_metadata', 'available'));
  aggregator.addCounter('nack_count', 0);
  aggregator.noteMax('packets_lost_pct_x100', 40);
  for (let i = 0; i < 200; i += 1) {
    now += RC_LATENCY_INTERVAL_MS;
    aggregator.sync(true, now);
  }
  assert.equal(emitted.length, 0, 'dimensions, zero counters and maxes alone are not data');
  aggregator.recordStage('sub_rtt_ms', 3);
  now += RC_LATENCY_INTERVAL_MS;
  aggregator.sync(true, now);
  assert.equal(emitted.length, 1);
  assert.deepEqual(emitted[0].dimensions, { frame_metadata: 'available' });
  assert.deepEqual(emitted[0].maxCounters, {}, 'maxes reset with their interval');
});

test('max counters keep the interval max and ride only with accepted samples', () => {
  const { aggregator, emitted } = collect();
  aggregator.sync(true, 0);
  aggregator.recordStageWithMax('capture_to_present_ms', -1, 'offset_err_ms', 50);
  for (const [ms, err] of [[40, 3], [41, 9], [42, 4]]) {
    aggregator.recordStageWithMax('capture_to_present_ms', ms, 'offset_err_ms', err);
  }
  aggregator.noteMax('packets_lost_pct_x100', 25);
  aggregator.noteMax('packets_lost_pct_x100', 10);
  aggregator.sync(true, RC_LATENCY_INTERVAL_MS);
  assert.deepEqual(emitted[0].maxCounters, { offset_err_ms: 9, packets_lost_pct_x100: 25 });
  assert.match(rcLatencySummaryLogLine(emitted[0]), / capture_to_present_ms_n=3 offset_err_ms=9 packets_lost_pct_x100=25$/);
});

test('dimensions are closed and a session caps at 120 events including the final', () => {
  const { aggregator, emitted } = collect();
  assert.equal(aggregator.setDimension('frame_metadata', 'available'), false, 'no session yet');
  let now = 0;
  aggregator.sync(true, now);
  assert.equal(aggregator.setDimension('frame_metadata', 'maybe'), false);
  assert.equal(aggregator.setDimension('jb_min_ms', '12'), false, 'raw ms never ships');
  assert.equal(aggregator.setDimension('jb_min_ms', '1_19'), true);
  for (let i = 0; i < 130; i += 1) {
    aggregator.recordStage('jb_delay_ms', 3);
    now += RC_LATENCY_INTERVAL_MS;
    aggregator.sync(true, now);
  }
  assert.equal(emitted.length, RC_LATENCY_MAX_EVENTS_PER_SESSION);
  for (let i = 0; i < 5; i += 1) aggregator.recordStage('jb_delay_ms', 3);
  aggregator.sync(false, now);
  assert.equal(emitted.length, RC_LATENCY_MAX_EVENTS_PER_SESSION, 'the final partial is capped too');
});
