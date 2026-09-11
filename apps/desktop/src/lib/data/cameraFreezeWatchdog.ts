// #247: local freeze-watchdog decision logic for remote camera tiles,
// extracted from galleryBridge.ts so it's unit-testable without a Tauri/
// livekit-client runtime (this file has zero framework imports).
//
// `weakConnectionIdentities` (galleryBridge.ts) only reflects the SFU's own
// paused/resumed signaling (TrackStreamStateChanged) -- if the far side dies
// mid-stream without a clean disconnect, the SFU can take tens of seconds
// (or longer) to notice, during which a <video> bound to the still-"active"
// MediaStreamTrack just holds its last decoded frame forever with no local
// indication. This mirrors the native compositor's existing local watchdog
// (`NO_FRAME_RETIRE_AFTER` / `no_frame_decision` in transport/subscriber.rs):
// poll each subscribed camera's own decode progress (`framesDecoded` from
// getRTCStatsReport(), independent of any server-driven signal) and flag it
// stale if that counter stops advancing.

export const FREEZE_WATCHDOG_TIMEOUT_MS = 30_000;
export const FREEZE_WATCHDOG_POLL_MS = 2_000;
export const CAMERA_DECODE_HEALTH_LOG_MS = 15_000;

export interface CameraFreezeState {
  lastFramesDecoded: number;
  lastProgressAt: number;
}

export interface CameraDecodeHealthState {
  lastLoggedAt: number;
  lastLoggedFramesDecoded: number;
  intervalSequence: number;
  trackSid?: string;
}

export interface CameraDecodeHealth {
  identity: string;
  framesDecoded: number | null;
  decodedFps: number;
  gapSinceLastFrameMs: number;
  /** Actual elapsed time since the previous emitted interval sample. */
  intervalMs: number;
  /** Monotonic per-publication sample counter; resets on SID replacement. */
  intervalSequence: number;
  /** Optional receive-side fields from the browser's inbound-rtp report. */
  framesPerSecond?: number | null;
  framesDropped?: number | null;
  freezeCount?: number | null;
  totalFreezesDurationMs?: number | null;
  packetsLost?: number | null;
  nackCount?: number | null;
  bytesReceived?: number | null;
  packetsReceived?: number | null;
  packetsDiscarded?: number | null;
  retransmittedPacketsReceived?: number | null;
  keyFramesDecoded?: number | null;
  pliCount?: number | null;
  firCount?: number | null;
  jitterBufferDelayMs?: number | null;
  jitterBufferEmittedCount?: number | null;
  totalDecodeTimeMs?: number | null;
  decoderImplementation?: string | null;
}

/** Raw `inbound-rtp` video stat fields this module reads. Kept permissive
 * because the browser's stats dictionary is platform-dependent. */
type InboundVideoStat = {
  frameWidth?: unknown;
  frameHeight?: unknown;
  framesDecoded?: unknown;
  framesPerSecond?: unknown;
  framesDropped?: unknown;
  freezeCount?: unknown;
  totalFreezesDuration?: unknown;
  packetsLost?: unknown;
  nackCount?: unknown;
  bytesReceived?: unknown;
  packetsReceived?: unknown;
  packetsDiscarded?: unknown;
  retransmittedPacketsReceived?: unknown;
  keyFramesDecoded?: unknown;
  framesReceived?: unknown;
  framesRendered?: unknown;
  pliCount?: unknown;
  firCount?: unknown;
  jitter?: unknown;
  jitterBufferDelay?: unknown;
  jitterBufferEmittedCount?: unknown;
  totalDecodeTime?: unknown;
  decoderImplementation?: unknown;
};

/** Select the ONE primary inbound video report every receiver metric is read
 * from. A camera track can transiently expose more than one inbound-rtp video
 * report (simulcast re-selection, unsubscribe/subscribe handoff), and reading
 * different metrics off different entries mixes two layers into one sample.
 * Prefer the largest reported decoded area; equal areas keep the first entry
 * so the selection is stable across polls. */
export function cameraInboundVideoStatFromReport(
  report: RTCStatsReport | undefined
): InboundVideoStat | null {
  if (!report) return null;
  let selected: InboundVideoStat | null = null;
  let selectedArea = -1;
  report.forEach((stat) => {
    const s = stat as InboundVideoStat & { type?: string; kind?: string };
    if (s.type !== 'inbound-rtp' || s.kind !== 'video') return;
    const width = typeof s.frameWidth === 'number' && Number.isFinite(s.frameWidth) ? s.frameWidth : 0;
    const height =
      typeof s.frameHeight === 'number' && Number.isFinite(s.frameHeight) ? s.frameHeight : 0;
    const area = width * height;
    if (selected === null || area > selectedArea) {
      selected = s;
      selectedArea = area;
    }
  });
  return selected;
}

/** Every receive-side counter and decode dimension for one camera track, all
 * read from the same primary inbound video report. These values are kept
 * outside the Sentry schema and are only written to the local Petal log for
 * post-hoc sender/receiver correlation. */
export interface CameraReceiveStats {
  framesDecoded: number | null;
  framesPerSecond: number | null;
  framesDropped: number | null;
  freezeCount: number | null;
  totalFreezesDurationMs: number | null;
  packetsLost: number | null;
  nackCount: number | null;
  bytesReceived: number | null;
  packetsReceived: number | null;
  packetsDiscarded: number | null;
  retransmittedPacketsReceived: number | null;
  keyFramesDecoded: number | null;
  pliCount: number | null;
  firCount: number | null;
  jitterBufferDelayMs: number | null;
  jitterBufferEmittedCount: number | null;
  totalDecodeTimeMs: number | null;
  decoderImplementation: string | null;
  framesReceived: number | null;
  framesRendered: number | null;
  decodedWidth: number | null;
  decodedHeight: number | null;
  jitterMs: number | null;
  lossPct: number | null;
}

/** Extract the browser's receive-side video counters for one camera track from
 * the single primary inbound report (see
 * `cameraInboundVideoStatFromReport`). The result is intentionally aggregate
 * and privacy-safe: no identity, URL, or track id crosses this helper. */
export function cameraReceiveStatsFromStatsReport(
  report: RTCStatsReport | undefined
): CameraReceiveStats | null {
  const selected = cameraInboundVideoStatFromReport(report);
  if (!selected) return null;
  const numberOrNull = (value: unknown): number | null =>
    typeof value === 'number' && Number.isFinite(value) ? value : null;
  const secondsToMs = (value: unknown): number | null => {
    const seconds = numberOrNull(value);
    return seconds === null ? null : seconds * 1_000;
  };
  const stringOrNull = (value: unknown): string | null =>
    typeof value === 'string' && value.length > 0 ? value : null;
  const jitter = numberOrNull(selected.jitter);
  const packetsLost = numberOrNull(selected.packetsLost);
  const packetsReceived = numberOrNull(selected.packetsReceived);
  const lost = packetsLost === null ? null : Math.max(0, packetsLost);
  const received = packetsReceived === null ? null : Math.max(0, packetsReceived);
  const total = (lost ?? 0) + (received ?? 0);
  return {
    framesDecoded: numberOrNull(selected.framesDecoded),
    framesPerSecond: numberOrNull(selected.framesPerSecond),
    framesDropped: numberOrNull(selected.framesDropped),
    freezeCount: numberOrNull(selected.freezeCount),
    totalFreezesDurationMs: secondsToMs(selected.totalFreezesDuration),
    packetsLost,
    nackCount: numberOrNull(selected.nackCount),
    bytesReceived: numberOrNull(selected.bytesReceived),
    packetsReceived,
    packetsDiscarded: numberOrNull(selected.packetsDiscarded),
    retransmittedPacketsReceived: numberOrNull(selected.retransmittedPacketsReceived),
    keyFramesDecoded: numberOrNull(selected.keyFramesDecoded),
    pliCount: numberOrNull(selected.pliCount),
    firCount: numberOrNull(selected.firCount),
    jitterBufferDelayMs: secondsToMs(selected.jitterBufferDelay),
    jitterBufferEmittedCount: numberOrNull(selected.jitterBufferEmittedCount),
    totalDecodeTimeMs: secondsToMs(selected.totalDecodeTime),
    decoderImplementation: stringOrNull(selected.decoderImplementation),
    framesReceived: numberOrNull(selected.framesReceived),
    framesRendered: numberOrNull(selected.framesRendered),
    decodedWidth: numberOrNull(selected.frameWidth),
    decodedHeight: numberOrNull(selected.frameHeight),
    jitterMs: jitter === null ? null : jitter * 1000,
    lossPct: lost === null || total <= 0 ? null : (lost * 100) / total
  };
}

/** Extract the decoder's cumulative `framesDecoded` counter for the same
 * primary inbound report every other receiver metric uses. */
export function framesDecodedFromStatsReport(report: RTCStatsReport | undefined): number | null {
  return cameraReceiveStatsFromStatsReport(report)?.framesDecoded ?? null;
}

/** Selected-path context for one receiver, kept categorical and privacy-safe:
 * candidate types and protocols only, never addresses, ports, or candidate
 * strings. */
export interface CameraPathStats {
  protocol: string | null;
  localCandidateType: string | null;
  remoteCandidateType: string | null;
  relayProtocol: string | null;
  selectedPairChanges: number | null;
  roundTripTimeMs: number | null;
  availableIncomingKbps: number | null;
}

type TransportStat = {
  type?: string;
  selectedCandidatePairId?: unknown;
  selectedCandidatePairChanges?: unknown;
};

type CandidatePairStat = {
  type?: string;
  id?: unknown;
  localCandidateId?: unknown;
  remoteCandidateId?: unknown;
  nominated?: unknown;
  currentRoundTripTime?: unknown;
  availableIncomingBitrate?: unknown;
};

type CandidateStat = {
  type?: string;
  id?: unknown;
  candidateType?: unknown;
  protocol?: unknown;
  relayProtocol?: unknown;
};

/** Follow the transport's selected candidate-pair id into the candidate-pair
 * and candidate entries. Unknown fields stay null, never zero. */
export function cameraPathStatsFromStatsReport(
  report: RTCStatsReport | undefined
): CameraPathStats | null {
  if (!report) return null;
  const numberOrNull = (value: unknown): number | null =>
    typeof value === 'number' && Number.isFinite(value) ? value : null;
  const stringOrNull = (value: unknown): string | null =>
    typeof value === 'string' && value.length > 0 ? value : null;

  let selectedPairId: string | null = null;
  let selectedPairChanges: number | null = null;
  const candidates = new Map<
    string,
    { candidateType: string | null; protocol: string | null; relayProtocol: string | null }
  >();
  report.forEach((stat) => {
    const s = stat as TransportStat & CandidateStat;
    if (s.type === 'transport') {
      if (selectedPairId === null) selectedPairId = stringOrNull(s.selectedCandidatePairId);
      if (selectedPairChanges === null) {
        selectedPairChanges = numberOrNull(s.selectedCandidatePairChanges);
      }
      return;
    }
    if (s.type !== 'local-candidate' && s.type !== 'remote-candidate') return;
    const id = stringOrNull(s.id);
    if (id === null) return;
    candidates.set(id, {
      candidateType: stringOrNull(s.candidateType),
      protocol: stringOrNull(s.protocol),
      relayProtocol: stringOrNull(s.relayProtocol)
    });
  });

  const pairs: CandidatePairStat[] = [];
  report.forEach((stat) => {
    const s = stat as CandidatePairStat;
    if (s.type !== 'candidate-pair') return;
    pairs.push(s);
  });
  const pair =
    pairs.find((candidate) => selectedPairId !== null && candidate.id === selectedPairId) ??
    pairs.find((candidate) => candidate.nominated === true) ??
    pairs[0];
  if (!pair) return null;

  const local = candidates.get(stringOrNull(pair.localCandidateId) ?? '');
  const remote = candidates.get(stringOrNull(pair.remoteCandidateId) ?? '');
  const roundTripTime = numberOrNull(pair.currentRoundTripTime);
  const availableIncoming = numberOrNull(pair.availableIncomingBitrate);
  return {
    protocol: local?.protocol ?? remote?.protocol ?? null,
    localCandidateType: local?.candidateType ?? null,
    remoteCandidateType: remote?.candidateType ?? null,
    relayProtocol: local?.relayProtocol ?? remote?.relayProtocol ?? null,
    selectedPairChanges,
    roundTripTimeMs: roundTripTime === null ? null : roundTripTime * 1_000,
    availableIncomingKbps: availableIncoming === null ? null : availableIncoming / 1_000
  };
}

/** Closed, privacy-safe buckets accepted by the native Sentry bridge. */
export type CameraReceiveCadence = 'reduced' | 'severe' | 'stalled';
export type CameraReceiveDecoderRender = 'decoder_degraded' | 'not_applicable';

/**
 * #126: `cadence: 'stalled'` is reachable from three unrelated conditions --
 * an SFU-paused subscription, a confirmed 30s decode stall, and a zero decode
 * rate that is not yet stale. Without this discriminator every receive-side
 * `camera-health` event is byte-identical and none of them can be acted on.
 * Closed enum only: no identity, no counts, no timestamps, no free text.
 */
export type CameraReceiveStallCause =
  | 'stream_paused'
  | 'decode_stale'
  | 'decode_zero'
  | 'not_applicable';

export interface CameraReceiveHealthSignal {
  cadence: CameraReceiveCadence;
  decoderRender: CameraReceiveDecoderRender;
  stallCause: CameraReceiveStallCause;
}

/**
 * Classify only a confirmed unhealthy receive interval. A missing RTC stats
 * report is not evidence of a media fault, so it deliberately emits nothing.
 * Identity, track information, counts, timestamps, and text stay outside this
 * value and cannot cross the Sentry IPC boundary.
 */
export function classifyCameraReceiveHealth(
  decodedFps: number | null,
  streamPaused: boolean,
  stale: boolean
): CameraReceiveHealthSignal | null {
  // A missing/invalid report is never enough to diagnose a media fault. In
  // particular, do not let a stale UI state turn a stats-read failure into a
  // false `stalled` quality signal.
  if (decodedFps === null || !Number.isFinite(decodedFps) || decodedFps < 0) return null;
  // #126: an SFU pause is checked FIRST and outranks staleness -- a paused
  // subscription is why decode progress stopped, so it is the cause to report.
  // It is a network degradation, not a decoder fault: never `decoder_degraded`.
  if (streamPaused) {
    return { cadence: 'stalled', decoderRender: 'not_applicable', stallCause: 'stream_paused' };
  }
  if (stale) {
    return { cadence: 'stalled', decoderRender: 'decoder_degraded', stallCause: 'decode_stale' };
  }
  if (decodedFps >= 24) return null;
  if (decodedFps >= 10) {
    return { cadence: 'reduced', decoderRender: 'decoder_degraded', stallCause: 'not_applicable' };
  }
  if (decodedFps > 0) {
    return { cadence: 'severe', decoderRender: 'decoder_degraded', stallCause: 'not_applicable' };
  }
  return { cadence: 'stalled', decoderRender: 'decoder_degraded', stallCause: 'decode_zero' };
}

/** Durable liveness state reported alongside cadence. `paused` is the SFU's
 * own pause (a network condition); `stalled` is a genuine no-progress
 * condition. Reduced/severe cadence is still progressing media, so it stays
 * `active` and is reported only as degraded quality. */
export type CameraReceiveStreamState = 'active' | 'paused' | 'stalled';

export interface CameraReceiveObservation {
  cadence: CameraReceiveCadence | null;
  streamState: CameraReceiveStreamState;
  degraded: boolean;
  stallCause: CameraReceiveStallCause | null;
}

/** Compose the diagnostic cadence with the durable stream state. Pause is
 * checked first and preserved as its own state so it is never flattened into
 * a decoder-fault `stalled`; only real no-progress or a stalled classifier
 * result becomes `stalled`. */
export function composeCameraReceiveObservation(
  signal: CameraReceiveHealthSignal | null,
  streamPaused: boolean,
  stale: boolean
): CameraReceiveObservation {
  const cadence = signal?.cadence ?? null;
  if (streamPaused) {
    return { cadence, streamState: 'paused', degraded: false, stallCause: 'stream_paused' };
  }
  if (stale || cadence === 'stalled') {
    return {
      cadence,
      streamState: 'stalled',
      degraded: false,
      stallCause: signal?.stallCause ?? (stale ? 'decode_stale' : 'decode_zero')
    };
  }
  return {
    cadence,
    streamState: 'active',
    degraded: cadence === 'reduced' || cadence === 'severe',
    stallCause: signal?.stallCause ?? null
  };
}

export function nextCameraDecodeHealthState(
  previous: CameraDecodeHealthState | undefined,
  framesDecoded: number | null,
  now: number,
  logIntervalMs: number = CAMERA_DECODE_HEALTH_LOG_MS,
  trackSid?: string
): { state: CameraDecodeHealthState; health: Omit<CameraDecodeHealth, 'identity'> | null } {
  // A replacement publication reuses the participant identity with a new
  // track SID and counters starting over. Carrying the old baseline across
  // would read the reset as negative progress and fabricate a stall.
  if (previous?.trackSid !== undefined && trackSid !== undefined && previous.trackSid !== trackSid) {
    previous = undefined;
  }
  const currentFrames = framesDecoded ?? previous?.lastLoggedFramesDecoded ?? 0;
  if (!previous) {
    return {
      state: { lastLoggedAt: now, lastLoggedFramesDecoded: currentFrames, intervalSequence: 0, trackSid },
      health: null
    };
  }
  // A counter that went BACKWARDS is not the stream we were watching: the
  // receiver restarted or a replacement reused the identity. Keeping the old
  // baseline would read every later frame as "no progress" and manufacture a
  // stall, so re-seed from the new reading instead.
  if (previous && framesDecoded !== null && framesDecoded < previous.lastLoggedFramesDecoded) {
    return {
      state: {
        lastLoggedAt: now,
        lastLoggedFramesDecoded: framesDecoded,
        intervalSequence: 0,
        trackSid: previous.trackSid ?? trackSid
      },
      health: null
    };
  }
  const elapsedMs = now - previous.lastLoggedAt;
  if (elapsedMs < logIntervalMs) {
    return { state: previous, health: null };
  }
  const intervalSequence = previous.intervalSequence + 1;
  return {
    state: {
      lastLoggedAt: now,
      lastLoggedFramesDecoded: currentFrames,
      intervalSequence,
      trackSid: previous.trackSid ?? trackSid
    },
    health: {
      framesDecoded,
      decodedFps:
        framesDecoded === null || elapsedMs <= 0
          ? 0
          : (Math.max(0, framesDecoded - previous.lastLoggedFramesDecoded) * 1000) / elapsedMs,
      gapSinceLastFrameMs: 0,
      intervalMs: elapsedMs,
      intervalSequence
    }
  };
}

export function formatCameraDecodeHealth(health: CameraDecodeHealth): string {
  const value = (number: number | null | undefined, digits = 0) =>
    number === null || number === undefined
      ? 'unknown'
      : digits === 0
        ? String(number)
        : number.toFixed(digits);
  return `gallery bridge: camera decode health for '${health.identity}' -- frames_decoded=${value(health.framesDecoded)} decoded_fps=${value(health.decodedFps, 1)} browser_fps=${value(health.framesPerSecond, 1)} frames_dropped=${value(health.framesDropped)} freeze_count=${value(health.freezeCount)} freeze_ms=${value(health.totalFreezesDurationMs, 1)} packets_received=${value(health.packetsReceived)} packets_lost=${value(health.packetsLost)} packets_discarded=${value(health.packetsDiscarded)} retransmitted_packets=${value(health.retransmittedPacketsReceived)} bytes_received=${value(health.bytesReceived)} nack=${value(health.nackCount)} pli=${value(health.pliCount)} fir=${value(health.firCount)} key_frames_decoded=${value(health.keyFramesDecoded)} jitter_buffer_ms=${value(health.jitterBufferDelayMs, 1)} jitter_buffer_emitted=${value(health.jitterBufferEmittedCount)} decode_ms=${value(health.totalDecodeTimeMs, 1)} decoder='${health.decoderImplementation ?? 'unknown'}' gap_since_last_frame_ms=${health.gapSinceLastFrameMs}`;
}

/** Pure state transition: advances `lastProgressAt` only when
 * `framesDecoded` has genuinely increased since the last observation. A
 * `null` reading (stats temporarily unavailable) preserves the existing
 * state rather than being treated as "no progress" -- a transient stats
 * read failure must never itself trigger a false-positive stale flag. */
export function nextCameraFreezeState(
  previous: CameraFreezeState | undefined,
  framesDecoded: number | null,
  now: number
): CameraFreezeState {
  if (framesDecoded === null) {
    return previous ?? { lastFramesDecoded: -1, lastProgressAt: now };
  }
  // Backwards progress is the same replacement signal as a SID change. Without
  // this, a counter that reset mid-stream would look permanently frozen and the
  // watchdog would raise a stall for a stream that is decoding normally.
  if (previous && framesDecoded < previous.lastFramesDecoded) {
    return { lastFramesDecoded: framesDecoded, lastProgressAt: now };
  }
  if (!previous || framesDecoded > previous.lastFramesDecoded) {
    return { lastFramesDecoded: framesDecoded, lastProgressAt: now };
  }
  return previous;
}

/** Pure decision: has decode progress been stalled for at least
 * `timeoutMs`? Mirrors native's `no_frame_decision`. */
export function isCameraFrameStale(
  state: CameraFreezeState,
  now: number,
  timeoutMs: number = FREEZE_WATCHDOG_TIMEOUT_MS
): boolean {
  return now - state.lastProgressAt >= timeoutMs;
}
