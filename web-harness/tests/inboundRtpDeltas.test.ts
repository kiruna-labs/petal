import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  jbMinMsBucket,
  inboundRtpCounters,
  inboundRtpDeltas,
  selectedCandidatePairRttMs,
  type InboundRtpCounters,
  type InboundRtpDeltas,
} from '@petal/shared/logic/inboundRtpDeltas';

const contract = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
) as {
  inboundRtpDeltaVectors: Array<{
    name: string;
    previous: InboundRtpCounters;
    current: InboundRtpCounters;
    expected: InboundRtpDeltas;
  }>;
  jbMinMsBucketVectors: Array<{ ms: number; bucket: string }>;
};

function assertDeltas(actual: InboundRtpDeltas, expected: InboundRtpDeltas, name: string) {
  assert.deepEqual(Object.keys(actual).sort(), Object.keys(expected).sort(), `${name} keys`);
  for (const key of Object.keys(expected) as Array<keyof InboundRtpDeltas>) {
    const want = expected[key];
    const got = actual[key];
    if (want === null) assert.equal(got, null, `${name} ${key}`);
    else assert.ok(got !== null && Math.abs(got - want) < 1e-6, `${name} ${key}: ${got} != ${want}`);
  }
}

function report(entries: Array<Record<string, unknown>>) {
  return { forEach: (callback: (stats: unknown) => void) => entries.forEach(callback) };
}

test('interval deltas pass the shared contract vectors (counter reset, zero emitted, missing)', () => {
  const names = contract.inboundRtpDeltaVectors.map((vector) => vector.name);
  for (const required of ['counter-reset', 'zero-emitted', 'missing-and-non-finite']) {
    assert.ok(names.includes(required), `contract is missing ${required}`);
  }
  for (const vector of contract.inboundRtpDeltaVectors) {
    assertDeltas(inboundRtpDeltas(vector.previous, vector.current), vector.expected, vector.name);
  }
});

test('jb_min_ms buckets pass the shared contract vectors', () => {
  for (const { ms, bucket } of contract.jbMinMsBucketVectors) assert.equal(jbMinMsBucket(ms), bucket, String(ms));
  assert.equal(jbMinMsBucket(Number.NaN), '0');
});

test('a NaN or Infinity counter is missing data, the same as JSON null', () => {
  const vector = contract.inboundRtpDeltaVectors.find((v) => v.name === 'missing-and-non-finite')!;
  const current = { ...vector.current, jitterBufferMinimumDelay: Number.NaN, totalProcessingDelay: Number.POSITIVE_INFINITY };
  assertDeltas(inboundRtpDeltas(vector.previous, current), vector.expected, 'NaN');
});

test('inbound counters come from the busiest video inbound-rtp entry only', () => {
  const counters = inboundRtpCounters(
    report([
      { type: 'inbound-rtp', kind: 'audio', bytesReceived: 9_000, jitterBufferDelay: 99 },
      { type: 'inbound-rtp', kind: 'video', bytesReceived: 10, jitterBufferDelay: 1 },
      { type: 'inbound-rtp', kind: 'video', bytesReceived: 500, jitterBufferDelay: 2, framesDecoded: 30, freezeCount: Number.NaN },
      { type: 'outbound-rtp', kind: 'video', bytesSent: 1e6 },
    ])
  );
  assert.deepEqual(counters, { jitterBufferDelay: 2, framesDecoded: 30 });
  assert.equal(inboundRtpCounters(report([{ type: 'candidate-pair' }])), null);
});

test('the selected candidate pair RTT follows the transport, else the nominated pair', () => {
  assert.equal(
    selectedCandidatePairRttMs(
      report([
        { type: 'candidate-pair', id: 'a', nominated: true, state: 'succeeded', currentRoundTripTime: 0.5 },
        { type: 'candidate-pair', id: 'b', currentRoundTripTime: 0.012 },
        { type: 'transport', selectedCandidatePairId: 'b' },
      ])
    ),
    12
  );
  assert.equal(
    selectedCandidatePairRttMs(
      report([
        { type: 'candidate-pair', id: 'a', nominated: false, state: 'succeeded', currentRoundTripTime: 0.5 },
        { type: 'candidate-pair', id: 'c', nominated: true, state: 'succeeded', currentRoundTripTime: 0.004 },
      ])
    ),
    4
  );
  assert.equal(selectedCandidatePairRttMs(report([{ type: 'candidate-pair', id: 'x', currentRoundTripTime: 0.1 }])), null);
});
