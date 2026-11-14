// #375: receive-path stage timing for the window a web controller controls.
// Fed only while control is active, and only for the controlled window:
// - a 1 s poll of that track's inbound-rtp / candidate-pair stats (jitter
//   buffer, decode, processing, loss counters, sub_rtt_ms) plus the local
//   publisher's selected pair (pub_rtt_ms, the input data-channel path);
// - one requestVideoFrameCallback loop on its <video>, joined to the host's
//   capture stamp through `lookupFrameMetadata` (capture_to_present_ms).
// Per-frame values go into the histograms only; nothing here logs per frame.

import {
  inboundRtpCounters,
  inboundRtpDeltas,
  jbMinMsBucket,
  selectedCandidatePairRttMs,
  type InboundRtpCounters,
  type StatsReportLike,
} from '@petal/shared/logic/inboundRtpDeltas';
import type { RcLatencyAggregator, RcLatencyCounter } from '@petal/shared/logic/rcLatencyAggregator';
import type { ClockEstimate } from './latencyProbeClock.ts';

export type FrameMetadataLike = { userTimestamp: bigint | number; frameId?: number };

export interface RcTimingTrack {
  getRTCStatsReport?: () => Promise<StatsReportLike | undefined | null>;
  lookupFrameMetadata?: (query: { rtpTimestamp: number }) => FrameMetadataLike | undefined;
}

export type RcFrameMetadata = {
  expectedDisplayTime: number;
  receiveTime?: number;
  rtpTimestamp?: number;
};

export interface RcTimingVideo {
  requestVideoFrameCallback?: (callback: (now: number, metadata: RcFrameMetadata) => void) => number;
  cancelVideoFrameCallback?: (handle: number) => void;
}

export interface RcReceiverTarget {
  /** LiveKit-verified identity of the host whose clock stamped the frames. */
  hostIdentity: string;
  track: RcTimingTrack;
  video: RcTimingVideo;
}

export interface RcReceiverTimingDeps {
  aggregator: Pick<RcLatencyAggregator, 'recordStage' | 'recordStageWithMax' | 'noteMax' | 'addCounter' | 'setDimension'>;
  /** Local-minus-host clock offset from the latency probe, or null when stale. */
  clock: (hostIdentity: string, nowMs: number) => ClockEstimate | null;
  /** The local publisher transport's stats (the input data channels ride it). */
  publisherStats: () => Promise<StatsReportLike | undefined | null>;
  /** True when this browser can run the frame-metadata transform at all. */
  frameMetadataSupported: () => boolean;
  nowMs?: () => number;
  /** Wall-clock ms at performance.now() = 0, read now (not timeOrigin) so it tracks clock steps. */
  wallClockAnchor?: () => number;
  /** #376: presented frames (trailer id, expectedDisplayTime) for press-to-photon, from this same rVFC loop. */
  photon?: { presented(frameId: number, presentMs: number): void; reset(): void };
}

const DELTA_COUNTERS: ReadonlyArray<[keyof ReturnType<typeof inboundRtpDeltas>, RcLatencyCounter]> = [
  ['frames_dropped', 'frames_dropped'],
  ['freeze_count', 'freeze_count'],
  ['freeze_duration_ms', 'freeze_duration_ms'],
  ['nack_count', 'nack_count'],
  ['pli_count', 'pli_count'],
];

/**
 * `localMs - (hostMs + offset)`, the input-leg rule from rc_input_timing.rs:
 * a negative value inside the offset error is 0, beyond it (a clock step) null.
 */
export function offsetCorrectedLatencyMs(localMs: number, hostMs: number, clock: ClockEstimate): number | null {
  const latency = localMs - (hostMs + clock.offsetMs);
  if (!Number.isFinite(latency) || latency < -clock.errMs) return null;
  return Math.max(0, latency);
}

export function createRcReceiverTiming(deps: RcReceiverTimingDeps) {
  const { aggregator } = deps;
  const nowMs = deps.nowMs ?? (() => Date.now());
  const wallClockAnchor = deps.wallClockAnchor ?? (() => Date.now() - performance.now());

  let current: RcReceiverTarget | null = null;
  let generation = 0;
  let frameHandle: number | null = null;
  let previousCounters: InboundRtpCounters | null = null;
  let pollGenerationInFlight: number | null = null;
  let metadataHit = false;
  let metadataDimension: 'available' | 'unavailable' | null = null;

  function setFrameMetadata(value: 'available' | 'unavailable') {
    if (metadataDimension === value) return;
    if (aggregator.setDimension('frame_metadata', value)) metadataDimension = value;
  }

  function stopFrames() {
    if (current && frameHandle !== null) current.video.cancelVideoFrameCallback?.(frameHandle);
    frameHandle = null;
  }

  function startFrames(target: RcReceiverTarget, frameGeneration: number) {
    const request = target.video.requestVideoFrameCallback;
    const lookup =
      typeof request === 'function' &&
      deps.frameMetadataSupported() &&
      typeof target.track.lookupFrameMetadata === 'function';
    if (!lookup) setFrameMetadata('unavailable');
    if (typeof request !== 'function') return;
    const onFrame = (_now: number, metadata: RcFrameMetadata) => {
      if (frameGeneration !== generation) return;
      frameHandle = request.call(target.video, onFrame);
      recordFrame(target, metadata, lookup);
    };
    frameHandle = request.call(target.video, onFrame);
  }

  function recordFrame(target: RcReceiverTarget, metadata: RcFrameMetadata, lookup: boolean) {
    const receiveTime = typeof metadata.receiveTime === 'number' ? metadata.receiveTime : null;
    if (receiveTime !== null) aggregator.recordStage('receive_to_present_ms', metadata.expectedDisplayTime - receiveTime);
    if (!lookup) return;
    const frame =
      typeof metadata.rtpTimestamp === 'number'
        ? target.track.lookupFrameMetadata?.({ rtpTimestamp: metadata.rtpTimestamp })
        : undefined;
    if (!frame) {
      if (!metadataHit) setFrameMetadata('unavailable');
      return;
    }
    metadataHit = true;
    setFrameMetadata('available');
    // #376: same monotonic timebase as the controller's send stamp; no clock offset.
    if (typeof frame.frameId === 'number') deps.photon?.presented(frame.frameId, metadata.expectedDisplayTime);
    const clock = deps.clock(target.hostIdentity, nowMs());
    if (!clock) return;
    const captureHostMs = Number(frame.userTimestamp) / 1000;
    if (!Number.isFinite(captureHostMs) || captureHostMs <= 0) return;
    const errMs = Math.ceil(clock.errMs);
    const origin = wallClockAnchor();
    const present = offsetCorrectedLatencyMs(origin + metadata.expectedDisplayTime, captureHostMs, clock);
    if (present !== null) aggregator.recordStageWithMax('capture_to_present_ms', present, 'offset_err_ms', errMs);
    if (receiveTime === null) return;
    const receive = offsetCorrectedLatencyMs(origin + receiveTime, captureHostMs, clock);
    if (receive !== null) aggregator.recordStageWithMax('capture_to_receive_ms', receive, 'offset_err_ms', errMs);
  }

  function recordStats(report: StatsReportLike | null | undefined) {
    if (!report) return;
    const counters = inboundRtpCounters(report);
    if (counters && previousCounters) {
      const deltas = inboundRtpDeltas(previousCounters, counters);
      if (deltas.jb_delay_ms !== null) aggregator.recordStage('jb_delay_ms', deltas.jb_delay_ms);
      if (deltas.jb_target_ms !== null) aggregator.recordStage('jb_target_ms', deltas.jb_target_ms);
      if (deltas.decode_ms !== null) aggregator.recordStage('decode_ms', deltas.decode_ms);
      if (deltas.processing_ms !== null) aggregator.recordStage('processing_ms', deltas.processing_ms);
      if (deltas.jb_min_ms !== null) aggregator.setDimension('jb_min_ms', jbMinMsBucket(deltas.jb_min_ms));
      for (const [key, counter] of DELTA_COUNTERS) {
        const value = deltas[key];
        if (value !== null) aggregator.addCounter(counter, value);
      }
      if (deltas.packets_lost_pct_x100 !== null) aggregator.noteMax('packets_lost_pct_x100', deltas.packets_lost_pct_x100);
    }
    previousCounters = counters;
    const subRtt = selectedCandidatePairRttMs(report);
    if (subRtt !== null) aggregator.recordStage('sub_rtt_ms', subRtt);
  }

  async function poll(target: RcReceiverTarget, pollGeneration: number) {
    pollGenerationInFlight = pollGeneration;
    try {
      const [report, publisher] = await Promise.all([
        target.track.getRTCStatsReport?.().catch(() => null) ?? null,
        deps.publisherStats().catch(() => null),
      ]);
      // A release or retarget during the await must not land in the next session.
      if (pollGeneration !== generation) return;
      recordStats(report);
      const pubRtt = publisher ? selectedCandidatePairRttMs(publisher) : null;
      if (pubRtt !== null) aggregator.recordStage('pub_rtt_ms', pubRtt);
    } finally {
      // Only this poll's own generation may clear the flag (a stale one must not unlock the next session's).
      if (pollGenerationInFlight === pollGeneration) pollGenerationInFlight = null;
    }
  }

  /**
   * Called once per second by the control lifecycle with the controlled
   * window, or null when control is not active (which stops everything).
   */
  function sync(target: RcReceiverTarget | null): Promise<void> {
    if (!target) {
      stop();
      return Promise.resolve();
    }
    if (!current || current.track !== target.track || current.video !== target.video || current.hostIdentity !== target.hostIdentity) {
      stop();
      current = target;
      startFrames(target, generation);
    }
    if (pollGenerationInFlight === generation) return Promise.resolve();
    return poll(target, generation);
  }

  function stop() {
    stopFrames();
    // A new track restarts frame ids; release ends the session.
    deps.photon?.reset();
    current = null;
    generation += 1;
    previousCounters = null;
    metadataHit = false;
    metadataDimension = null;
  }

  return { sync, stop };
}

/** Frame metadata needs the worker plus an encoded-transform API; detect both, never assume (#375). */
export function browserSupportsFrameMetadata(worker: unknown): boolean {
  if (!worker) return false;
  const scope = globalThis as { RTCRtpScriptTransform?: unknown; RTCRtpReceiver?: { prototype?: object } };
  return (
    typeof scope.RTCRtpScriptTransform === 'function' ||
    Boolean(scope.RTCRtpReceiver?.prototype && 'createEncodedStreams' in scope.RTCRtpReceiver.prototype)
  );
}
