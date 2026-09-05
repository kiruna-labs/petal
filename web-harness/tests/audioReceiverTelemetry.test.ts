import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  audioReceiverStatsSummary,
  audioReceiverTelemetryFromStatsReport,
  formatAudioReceiverTelemetry,
  startAudioReceiverTelemetry,
} from '../src/audioReceiverTelemetry.ts';

function statsReport(entries: Array<[string, Record<string, unknown>]>): RTCStatsReport {
  return new Map(entries) as unknown as RTCStatsReport;
}

function nextTurn(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

test('extracts privacy-safe inbound audio and linked codec telemetry', () => {
  const telemetry = audioReceiverTelemetryFromStatsReport(
    statsReport([
      ['video', { type: 'inbound-rtp', kind: 'video', packetsReceived: 999 }],
      ['audio', {
        type: 'inbound-rtp', kind: 'audio', packetsReceived: 42, payloadType: 111, codecId: 'codec-opus',
        totalSamplesReceived: 96_000, totalSamplesDuration: 2, totalAudioEnergy: 0.25,
        jitterBufferEmittedCount: 95_000, jitterBufferDelay: 0.03,
      }],
      ['codec-opus', { id: 'codec-opus', type: 'codec', mimeType: 'audio/opus', payloadType: 111, clockRate: 48_000, channels: 2 }],
    ])
  );

  assert.deepEqual(telemetry, {
    packetsReceived: 42,
    payloadType: 111,
    codecId: 'codec-opus',
    codecMimeType: 'audio/opus',
    codecPayloadType: 111,
    codecClockRate: 48_000,
    codecChannels: 2,
    totalSamplesReceived: 96_000,
    totalSamplesDuration: 2,
    totalAudioEnergy: 0.25,
    jitterBufferEmittedCount: 95_000,
    jitterBufferDelay: 0.03,
  });
});

test('formats unavailable fields explicitly without identity or track metadata', () => {
  const message = formatAudioReceiverTelemetry(
    2,
    audioReceiverTelemetryFromStatsReport(statsReport([['audio', { type: 'inbound-rtp', mediaType: 'audio' }]])),
    audioReceiverStatsSummary({ bytesReceived: 700, totalAudioEnergy: 0.5, jitter: 0.03, totalSamplesDuration: 2 })
  );

  assert.match(message, /audio receiver stats 2\/3/);
  assert.match(message, /payloadType=unavailable/);
  assert.match(message, /receiver\{bytesReceived=700 jitter=0.03 totalSamplesDuration=2 totalAudioEnergy=0.5 concealedSamples=unavailable concealmentEvents=unavailable\}/);
  assert.equal(message.includes('identity'), false);
  assert.equal(message.includes('trackName'), false);

  const bounded = formatAudioReceiverTelemetry(2, null, null, 5);
  assert.match(bounded, /audio receiver stats 2\/5/);
});

test('polls public receiver APIs and self-cleans after three samples', async () => {
  const callbacks: Array<() => void> = [];
  let clearCount = 0;
  const logs: string[] = [];
  const track = {
    getRTCStatsReport: async () =>
      statsReport([
        ['audio', {
          type: 'inbound-rtp', kind: 'audio', packetsReceived: 7, payloadType: 111, codecId: 'opus',
          totalSamplesReceived: 48_000, totalAudioEnergy: 0.5, jitterBufferEmittedCount: 47_900,
        }],
        ['opus', { type: 'codec', id: 'opus', mimeType: 'audio/opus', payloadType: 111, clockRate: 48_000, channels: 2 }],
      ]),
    getReceiverStats: async () => ({ bytesReceived: 700, totalAudioEnergy: 0.5, jitter: 0.01 }),
  };
  const stop = startAudioReceiverTelemetry(track, (message) => logs.push(message), {
    scheduler: {
      setInterval: (callback) => {
        callbacks.push(callback);
        return callbacks.length as unknown as ReturnType<typeof setInterval>;
      },
      clearInterval: () => {
        clearCount += 1;
      },
    },
  });

  await nextTurn();
  assert.equal(logs.length, 1);
  callbacks[0]!();
  await nextTurn();
  callbacks[0]!();
  await nextTurn();
  assert.equal(logs.length, 3);
  assert.equal(clearCount, 1);
  assert.match(logs[2]!, /payloadType=111/);
  assert.match(logs[2]!, /codecMime=audio\/opus/);
  assert.match(logs[2]!, /totalSamplesReceived=48000/);
  stop();
  assert.equal(clearCount, 1);
});

test('polling remains safe when public stats are absent', async () => {
  const logs: string[] = [];
  const stop = startAudioReceiverTelemetry({}, (message) => logs.push(message), { intervalMs: 10 });
  await Promise.resolve();
  stop();
  assert.equal(logs.length, 1);
  assert.match(logs[0]!, /inbound audio stats unavailable/);
});

test('stopping an in-flight sample prevents a deferred result from logging', async () => {
  let resolveReport!: (report: RTCStatsReport) => void;
  const reportPromise = new Promise<RTCStatsReport>((resolve) => {
    resolveReport = resolve;
  });
  const logs: string[] = [];
  const stop = startAudioReceiverTelemetry(
    { getRTCStatsReport: async () => reportPromise },
    (message) => logs.push(message),
    { intervalMs: 10 }
  );

  stop();
  resolveReport(statsReport([['audio', { type: 'inbound-rtp', kind: 'audio', packetsReceived: 1 }]]));
  await nextTurn();
  assert.equal(logs.length, 0);
});
