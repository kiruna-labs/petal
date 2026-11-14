import assert from 'node:assert/strict';
import test, { mock } from 'node:test';

import { RcLatencyAggregator, type RcLatencySummary } from '@petal/shared/logic/rcLatencyAggregator';
import { installTestSink, uninstallTestSink } from '../src/analytics.ts';
import type { HarnessContext } from '../src/context.ts';
import { ClockWindows, estimatePeerToLocalOffsetMs, type ClockEstimate } from '../src/latencyProbeClock.ts';
import {
  createRcReceiverTiming,
  offsetCorrectedLatencyMs,
  type RcFrameMetadata,
  type RcReceiverTarget,
} from '../src/rcReceiverTiming.ts';
import { RC_LATENCY_SYNC_MS, setupRemoteControlUi } from '../src/remoteControlUi.ts';

type FrameCallback = (now: number, metadata: RcFrameMetadata) => void;

class FakeVideo {
  private nextHandle = 1;
  readonly callbacks = new Map<number, FrameCallback>();
  cancelled = 0;

  requestVideoFrameCallback(callback: FrameCallback): number {
    const handle = this.nextHandle++;
    this.callbacks.set(handle, callback);
    return handle;
  }

  cancelVideoFrameCallback(handle: number) {
    if (this.callbacks.delete(handle)) this.cancelled += 1;
  }

  present(metadata: RcFrameMetadata) {
    const pending = [...this.callbacks.values()];
    this.callbacks.clear();
    for (const callback of pending) callback(0, metadata);
  }
}

type Stats = Array<Record<string, unknown>>;

class FakeTrack {
  readonly kind = 'video';
  polls = 0;
  lookups = 0;
  reports: Stats[] = [];
  readonly frames = new Map<number, { userTimestamp: bigint }>();

  getRTCStatsReport() {
    this.polls += 1;
    const entries = this.reports.shift() ?? [];
    return Promise.resolve({ forEach: (callback: (stats: unknown) => void) => entries.forEach(callback) });
  }

  lookupFrameMetadata({ rtpTimestamp }: { rtpTimestamp: number }) {
    this.lookups += 1;
    return this.frames.get(rtpTimestamp);
  }
}

function inbound(values: Record<string, number>): Stats {
  return [
    { type: 'inbound-rtp', kind: 'video', bytesReceived: 1, ...values },
    { type: 'transport', selectedCandidatePairId: 'p' },
    { type: 'candidate-pair', id: 'p', currentRoundTripTime: 0.008 },
  ];
}

const flush = async () => {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
};

function harness(options: { clock?: ClockEstimate | null; supported?: boolean } = {}) {
  const emitted: RcLatencySummary[] = [];
  const aggregator = new RcLatencyAggregator('controller', { emit: (s) => emitted.push(s) });
  aggregator.sync(true, 0);
  const video = new FakeVideo();
  const track = new FakeTrack();
  const timing = createRcReceiverTiming({
    aggregator,
    clock: () => (options.clock === undefined ? { offsetMs: -250, errMs: 2 } : options.clock),
    publisherStats: () =>
      Promise.resolve({
        forEach: (callback: (stats: unknown) => void) =>
          [{ type: 'candidate-pair', id: 'q', nominated: true, state: 'succeeded', currentRoundTripTime: 0.003 }].forEach(callback),
      }),
    frameMetadataSupported: () => options.supported ?? true,
    nowMs: () => 0,
    wallClockAnchor: () => 1_000_000,
  });
  const target: RcReceiverTarget = { hostIdentity: 'native-host', track, video };
  let now = 0;
  const summary = () => {
    now += 60_000;
    aggregator.sync(true, now);
    return emitted.at(-1);
  };
  return { aggregator, emitted, video, track, timing, target, summary };
}

test('probe offsets use the native NTP sign convention and the lowest fresh RTT', () => {
  // Host clock 250 ms ahead of ours: local = host - 250.
  assert.equal(estimatePeerToLocalOffsetMs(1_000, 1_255, 1_255, 1_010), -250);
  assert.equal(estimatePeerToLocalOffsetMs(1_000, 1_255, 1_250, 1_010), null, 'impossible ordering');
  const windows = new ClockWindows();
  windows.record('host', -250, 40, 0);
  windows.record('host', -240, 8, 1_000);
  windows.record('other', 5, 1, 1_000);
  assert.deepEqual(windows.estimate('host', 2_000), { offsetMs: -240, errMs: 4 });
  assert.equal(windows.estimate('host', 11_001), null, 'every answer is stale');
  assert.equal(offsetCorrectedLatencyMs(1_000, 1_249, { offsetMs: -250, errMs: 2 }), 1);
  assert.equal(offsetCorrectedLatencyMs(1_000, 1_251, { offsetMs: -250, errMs: 2 }), 0, 'inside the error bound');
  assert.equal(offsetCorrectedLatencyMs(1_000, 1_260, { offsetMs: -250, errMs: 2 }), null, 'a clock step');
});

test('capture-to-present applies the host offset to the frame-metadata capture stamp', async () => {
  const { video, track, timing, target, summary } = harness();
  // Host captured at 1_000_700 ms on its clock = 1_000_450 on ours.
  track.frames.set(9000, { userTimestamp: 1_000_700_000n });
  await timing.sync(target);
  for (let i = 0; i < 5; i += 1) video.present({ expectedDisplayTime: 500, receiveTime: 480, rtpTimestamp: 9000 });
  const emitted = summary()!;
  assert.deepEqual(emitted.stages.capture_to_present_ms, { p50: 50, p95: 50, max: 50, n: 5 });
  assert.deepEqual(emitted.stages.capture_to_receive_ms, { p50: 30, p95: 30, max: 30, n: 5 });
  assert.deepEqual(emitted.stages.receive_to_present_ms, { p50: 20, p95: 20, max: 20, n: 5 });
  assert.equal(emitted.maxCounters.offset_err_ms, 2);
  assert.equal(emitted.dimensions.frame_metadata, 'available');
});

test('a lookupFrameMetadata miss records no capture sample and sets frame_metadata=unavailable', async () => {
  const { video, track, timing, target, summary } = harness();
  await timing.sync(target);
  video.present({ expectedDisplayTime: 500, receiveTime: 480, rtpTimestamp: 1 });
  const missed = summary()!;
  assert.equal(track.lookups, 1);
  assert.equal(missed.stages.capture_to_present_ms, undefined);
  assert.equal(missed.stages.receive_to_present_ms?.n, 1, 'receive-to-present needs no metadata');
  assert.equal(missed.dimensions.frame_metadata, 'unavailable');

  track.frames.set(2, { userTimestamp: 1_000_700_000n });
  video.present({ expectedDisplayTime: 500, rtpTimestamp: 2 });
  assert.equal(summary()!.dimensions.frame_metadata, 'available', 'the first join flips it');
});

test('without a fresh offset, or without frame-metadata support, there is no capture sample', async () => {
  const stale = harness({ clock: null });
  stale.track.frames.set(5, { userTimestamp: 1_000_700_000n });
  await stale.timing.sync(stale.target);
  stale.video.present({ expectedDisplayTime: 500, rtpTimestamp: 5 });
  stale.video.present({ expectedDisplayTime: 500, rtpTimestamp: 5 });
  stale.aggregator.recordStage('decode_ms', 1);
  const staleSummary = stale.summary()!;
  assert.equal(staleSummary.stages.capture_to_present_ms, undefined);
  assert.equal(staleSummary.dimensions.frame_metadata, 'available');

  const unsupported = harness({ supported: false });
  unsupported.track.frames.set(5, { userTimestamp: 1_000_700_000n });
  await unsupported.timing.sync(unsupported.target);
  unsupported.video.present({ expectedDisplayTime: 500, receiveTime: 490, rtpTimestamp: 5 });
  const unsupportedSummary = unsupported.summary()!;
  assert.equal(unsupported.track.lookups, 0, 'detected, not assumed');
  assert.equal(unsupportedSummary.dimensions.frame_metadata, 'unavailable');
  assert.equal(unsupportedSummary.stages.capture_to_present_ms, undefined);
});

test('frame_metadata is unavailable when the browser gives no rtpTimestamp or no rVFC', async () => {
  const noRtp = harness();
  await noRtp.timing.sync(noRtp.target);
  noRtp.video.present({ expectedDisplayTime: 500, receiveTime: 480 });
  assert.equal(noRtp.summary()!.dimensions.frame_metadata, 'unavailable');

  const noRvfc = harness();
  noRvfc.aggregator.recordStage('decode_ms', 1);
  await noRvfc.timing.sync({ ...noRvfc.target, video: {} });
  assert.equal(noRvfc.summary()!.dimensions.frame_metadata, 'unavailable');
});

test('a stale poll finishing late cannot unlock overlapping polls in the next session', async () => {
  const { track, timing, target } = harness();
  const pending: Array<() => void> = [];
  track.getRTCStatsReport = () => {
    track.polls += 1;
    return new Promise((resolve) => pending.push(() => resolve({ forEach: () => {} })));
  };
  void timing.sync(target);
  timing.stop();
  void timing.sync(target);
  assert.equal(track.polls, 2);
  pending[0]();
  await flush();
  void timing.sync(target);
  assert.equal(track.polls, 2, 'the new session still has its own poll in flight');
  pending[1]();
  await flush();
  void timing.sync(target);
  assert.equal(track.polls, 3);
});

test('stats polls record jitter-buffer deltas, counters, loss and both RTTs', async () => {
  const { track, timing, target, summary } = harness();
  track.reports.push(
    inbound({ jitterBufferDelay: 1, jitterBufferEmittedCount: 100, jitterBufferMinimumDelay: 0, totalDecodeTime: 0.2, framesDecoded: 100, freezeCount: 0, packetsLost: 10, packetsReceived: 1000 }),
    inbound({ jitterBufferDelay: 1.6, jitterBufferEmittedCount: 130, jitterBufferMinimumDelay: 0.3, totalDecodeTime: 0.29, framesDecoded: 130, freezeCount: 2, packetsLost: 12, packetsReceived: 1198 })
  );
  await timing.sync(target);
  await timing.sync(target);
  const emitted = summary()!;
  assert.equal(emitted.stages.jb_delay_ms?.p50, 20);
  assert.equal(emitted.stages.decode_ms?.p50, 3);
  assert.equal(emitted.stages.sub_rtt_ms?.n, 2);
  assert.equal(emitted.stages.sub_rtt_ms?.p50, 8);
  assert.equal(emitted.stages.pub_rtt_ms?.p50, 3);
  assert.deepEqual(emitted.counters, { freeze_count: 2 });
  assert.equal(emitted.maxCounters.packets_lost_pct_x100, 100);
  assert.equal(emitted.dimensions.jb_min_ms, '1_19', 'bucketed, never raw ms');
});

test('release stops the frame loop and drops a poll still in flight', async () => {
  const { aggregator, video, track, timing, target, summary } = harness();
  let resolveReport!: (report: { forEach: (callback: (stats: unknown) => void) => void }) => void;
  track.getRTCStatsReport = () => {
    track.polls += 1;
    return new Promise((resolve) => (resolveReport = resolve));
  };
  const inFlight = timing.sync(target);
  assert.equal(video.callbacks.size, 1);
  await timing.sync(null);
  assert.equal(video.callbacks.size, 0);
  assert.equal(video.cancelled, 1);
  resolveReport({ forEach: (callback) => inbound({}).forEach(callback) });
  await inFlight;
  aggregator.recordStage('decode_ms', 1);
  assert.equal(summary()!.stages.sub_rtt_ms, undefined, 'a poll answered after release is dropped');
  assert.equal(track.polls, 1);
});

// The real control lifecycle: setupRemoteControlUi's 1 s reconcile drives the
// aggregator and polls only the controlled window, only while granted.
test('the control lifecycle polls only the controlled window and stops on release', async () => {
  const controlled = { video: new FakeVideo(), track: new FakeTrack() };
  const other = { video: new FakeVideo(), track: new FakeTrack() };
  const tiles = new Map(
    [
      ['tile-1', 'TR_controlled', controlled],
      ['tile-2', 'TR_other', other],
    ].map(([id, sid, parts]) => {
      const { video } = parts as typeof controlled;
      return [
        id as string,
        { id, dataset: { owner: 'native-host', trackSid: sid }, classList: { toggle: () => {} },
          querySelector: (selector: string) => (selector === 'video' ? video : { classList: { toggle: () => {} } }) },
      ];
    })
  );
  const priorDocument = (globalThis as { document?: unknown }).document;
  (globalThis as { document?: unknown }).document = {
    getElementById: (id: string) => tiles.get(id) ?? null,
    querySelectorAll: () => [],
  };
  const logs: string[] = [];
  const ctx = {
    state: {
      room: {
        localParticipant: { identity: 'web', publishData: () => Promise.resolve() },
        remoteParticipants: new Map([
          [
            'native-host',
            {
              identity: 'native-host',
              trackPublications: new Map([
                ['TR_controlled', { track: controlled.track }],
                ['TR_other', { track: other.track }],
              ]),
            },
          ],
        ]),
      },
      remoteControlSeq: 0,
      activeRemoteControl: null as unknown,
      frameMetadataWorker: null,
    },
    ui: { logEvent: (message: string) => logs.push(message), showToast: () => {} },
    hook: {},
    cb: { shareTileForWindowId: () => null },
  } as unknown as HarnessContext;
  const sink = installTestSink();
  mock.timers.enable({ apis: ['setInterval', 'Date'], now: 0 });
  try {
    const setupResult = setupRemoteControlUi(ctx);
    mock.timers.tick(RC_LATENCY_SYNC_MS);
    await flush();
    assert.equal(controlled.track.polls + other.track.polls, 0, 'nothing is polled without a grant');

    ctx.state.activeRemoteControl = { tileId: 'tile-1', targetUserId: 'native-host', windowId: 7, pointerId: null, grantToken: null };
    mock.timers.tick(RC_LATENCY_SYNC_MS);
    await flush();
    assert.equal(controlled.track.polls, 0, 'a pending request is not control');

    controlled.track.reports.push(
      inbound({ jitterBufferDelay: 1, jitterBufferEmittedCount: 100 }),
      inbound({ jitterBufferDelay: 1.6, jitterBufferEmittedCount: 130 }),
      inbound({ jitterBufferDelay: 2.2, jitterBufferEmittedCount: 160 })
    );
    ctx.state.activeRemoteControl!.grantToken = 'grant';
    for (let i = 0; i < 3; i += 1) {
      mock.timers.tick(RC_LATENCY_SYNC_MS);
      await flush();
    }
    assert.equal(controlled.track.polls, 3);
    assert.equal(controlled.video.callbacks.size, 1, 'one frame loop on the controlled video');
    assert.equal(other.track.polls, 0, 'only the controlled window is polled');
    assert.equal(other.video.callbacks.size, 0);
    for (let i = 0; i < 5; i += 1) controlled.video.present({ expectedDisplayTime: 40, receiveTime: 25 });

    // A local release stops the frame loop at once, not on the next tick.
    (setupResult as { stopRemoteControl: () => void }).stopRemoteControl();
    assert.equal(controlled.video.callbacks.size, 0, 'no frame lands after release');
    assert.equal(ctx.state.activeRemoteControl, null);
    for (let i = 0; i < 3; i += 1) {
      mock.timers.tick(RC_LATENCY_SYNC_MS);
      await flush();
    }
    assert.equal(controlled.track.polls, 3, 'polling stops on release');
    assert.equal(controlled.video.callbacks.size, 0);
    const events = sink.filter((event) => event.name === 'remote_control_latency_summary');
    assert.equal(events.length, 1, 'the final partial');
    assert.equal(events[0].properties.role, 'controller');
    assert.equal(events[0].properties.client, 'web');
    assert.equal(events[0].properties.final, true);
    assert.equal(events[0].properties.receive_to_present_ms_n, 5);
    assert.equal(events[0].properties.sub_rtt_ms_n, 3);
    assert.equal(events[0].properties.jb_delay_ms_p50, 20);
    assert.equal(events[0].properties.frame_metadata, 'unavailable', 'no worker in node');
    assert.ok(logs.some((line) => line.startsWith('remote-control-latency-summary: role=controller ')));
  } finally {
    mock.timers.reset();
    uninstallTestSink();
    (globalThis as { document?: unknown }).document = priorDocument;
  }
});
