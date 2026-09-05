// #510 guarded Chromium receiver workaround. It observes encoded remote audio
// frames at the browser boundary, records bounded/redacted metadata only, and
// forwards every frame unchanged. It never opens or publishes an audio input.

const MAX_RECORDED_FRAMES = 20;

export interface EncodedAudioFrameMetadata {
  /** RTP timestamp exposed by the encoded-frame API; never decoded audio. */
  rtpTimestamp: number | null;
  payloadType: number | null;
  sequenceNumber: number | null;
  byteLength: number | null;
}

export interface EncodedAudioReceiverStats {
  packetsReceived: number | null;
  packetsDiscarded: number | null;
  bytesReceived: number | null;
  totalSamplesReceived: number | null;
  totalSamplesDuration: number | null;
  totalAudioEnergy: number | null;
  jitterBufferEmittedCount: number | null;
  codecMimeType: string | null;
  codecPayloadType: number | null;
}

export interface EncodedAudioProbeState {
  enabled: boolean;
  supported: boolean | null;
  peerConnectionCount: number;
  audioReceiverCount: number;
  frameCount: number;
  frames: EncodedAudioFrameMetadata[];
  /** At most three receiver/codec snapshots; contains no IDs or payload bytes. */
  receiverStats: EncodedAudioReceiverStats[];
  errorCode: 'create-encoded-streams-unavailable' | 'create-encoded-streams-failed' | 'stream-failed' | null;
}

type EncodedAudioFrameLike = {
  timestamp?: number;
  data?: ArrayBuffer;
  getMetadata?: () => {
    payloadType?: number;
    sequenceNumber?: number;
  };
};

type EncodedStreamsLike = {
  readable: ReadableStream<EncodedAudioFrameLike>;
  writable: WritableStream<EncodedAudioFrameLike>;
};

type ReceiverWithEncodedStreams = RTCRtpReceiver & {
  createEncodedStreams?: () => EncodedStreamsLike;
};

type StatsLike = Record<string, unknown>;
const MAX_RECEIVER_STAT_SAMPLES = 3;

type ProbeWindow = Window & typeof globalThis;

function finiteNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function stringValue(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

/** Records bounded/redacted metadata only; encoded bytes never leave the frame. */
export function recordEncodedAudioFrame(
  state: EncodedAudioProbeState,
  frame: EncodedAudioFrameLike,
): void {
  state.frameCount += 1;
  if (state.frames.length >= MAX_RECORDED_FRAMES) return;

  const metadata = frame.getMetadata?.();
  state.frames.push({
    rtpTimestamp: finiteNumber(frame.timestamp),
    payloadType: finiteNumber(metadata?.payloadType),
    sequenceNumber: finiteNumber(metadata?.sequenceNumber),
    byteLength: frame.data?.byteLength ?? null,
  });
}

function latestInboundAudioStats(report: RTCStatsReport): StatsLike | null {
  let latest: StatsLike | null = null;
  report.forEach((raw) => {
    const stat = raw as StatsLike;
    if (stat.type !== 'inbound-rtp' || (stat.kind !== 'audio' && stat.mediaType !== 'audio')) return;
    if ((finiteNumber(stat.packetsReceived) ?? -1) >= (finiteNumber(latest?.packetsReceived) ?? -1)) latest = stat;
  });
  return latest;
}

/** Extracts only the counters needed to distinguish RTP delivery from decode. */
export function encodedAudioReceiverStatsFromReport(report: RTCStatsReport): EncodedAudioReceiverStats | null {
  const inbound = latestInboundAudioStats(report);
  if (!inbound) return null;

  const codecId = stringValue(inbound.codecId);
  let codec: StatsLike | null = null;
  if (codecId) {
    report.forEach((raw) => {
      const stat = raw as StatsLike;
      if (stat.type === 'codec' && stat.id === codecId) codec = stat;
    });
  }
  const codecStats = codec as StatsLike | null;
  return {
    packetsReceived: finiteNumber(inbound.packetsReceived),
    packetsDiscarded: finiteNumber(inbound.packetsDiscarded),
    bytesReceived: finiteNumber(inbound.bytesReceived),
    totalSamplesReceived: finiteNumber(inbound.totalSamplesReceived),
    totalSamplesDuration: finiteNumber(inbound.totalSamplesDuration),
    totalAudioEnergy: finiteNumber(inbound.totalAudioEnergy),
    jitterBufferEmittedCount: finiteNumber(inbound.jitterBufferEmittedCount),
    codecMimeType: stringValue(codecStats?.mimeType),
    codecPayloadType: finiteNumber(codecStats?.payloadType),
  };
}

function recordReceiverStats(state: EncodedAudioProbeState, receiver: RTCRtpReceiver): void {
  if (state.receiverStats.length >= MAX_RECEIVER_STAT_SAMPLES || typeof receiver.getStats !== 'function') return;
  void receiver.getStats()
    .then((report) => {
      if (state.receiverStats.length >= MAX_RECEIVER_STAT_SAMPLES) return;
      const stats = encodedAudioReceiverStatsFromReport(report);
      if (stats) state.receiverStats.push(stats);
    })
    .catch(() => {
      // Stats are supplemental; an unavailable report must not disturb media.
    });
}

/** Keep this allowlist deliberately narrow until Chromium ships a native fix. */
export const CHROMIUM_WORKAROUND_MIN_VERSION = 120;
const MAX_WORKAROUND_FAILURES = 8;

export type EncodedAudioWorkaroundFailureCode =
  | 'unsupported-browser'
  | 'kill-switch'
  | 'e2ee-or-receiver-transform-configured'
  | 'receiver-transform-configured'
  | 'create-encoded-streams-unavailable'
  | 'peer-connection-constructor-fallback'
  | 'create-encoded-streams-failed'
  | 'async-pipe-rejected'
  | 'peer-connection-close-failed';

export interface EncodedAudioWorkaroundFailure {
  code: EncodedAudioWorkaroundFailureCode;
  phase: 'install' | 'constructor' | 'receiver' | 'pipe' | 'cleanup';
}

export interface EncodedAudioWorkaroundState extends EncodedAudioProbeState {
  mode: 'workaround';
  browserVersion: number | null;
  disabledReason: EncodedAudioWorkaroundFailureCode | null;
  constructorFallbackCount: number;
  claimedReceiverCount: number;
  cleanedReceiverCount: number;
  reconnectRequestCount: number;
  failures: EncodedAudioWorkaroundFailure[];
}

type WorkaroundTarget = ProbeWindow & {
  __petalEncodedAudioWorkaround?: EncodedAudioWorkaroundState;
  __petalE2eeEnabled?: boolean;
  __petalReceiverTransformConfigured?: boolean;
};

type WorkaroundPeerConnection = RTCPeerConnection;

type WorkaroundReceiver = ReceiverWithEncodedStreams & {
  transform?: unknown;
};

export function chromiumWorkaroundVersion(userAgent: string): number | null {
  if (/\b(?:Edg|OPR|SamsungBrowser|Firefox|CriOS)\//i.test(userAgent)) return null;
  const match = userAgent.match(/\b(?:Chrome|Chromium)\/(\d+)/i);
  return match ? Number(match[1]) : null;
}

export function chromiumWorkaroundAllowed(userAgent: string): boolean {
  const version = chromiumWorkaroundVersion(userAgent);
  return version !== null && version >= CHROMIUM_WORKAROUND_MIN_VERSION;
}

export function encodedAudioWorkaroundDisabled(search: string): boolean {
  return new URLSearchParams(search).get('disableEncodedAudioWorkaround') === '1';
}

function workaroundFailure(
  state: EncodedAudioWorkaroundState,
  code: EncodedAudioWorkaroundFailureCode,
  phase: EncodedAudioWorkaroundFailure['phase'],
): void {
  if (state.failures.length < MAX_WORKAROUND_FAILURES) state.failures.push({ code, phase });
  state.disabledReason ??= code === 'unsupported-browser' || code === 'kill-switch' ? code : null;
}

function receiverTransformConfigured(target: WorkaroundTarget, configuration: RTCConfiguration | undefined): boolean {
  const config = configuration as (RTCConfiguration & {
    receiverTransformConfigured?: boolean;
  }) | undefined;
  return target.__petalE2eeEnabled === true || target.__petalReceiverTransformConfigured === true ||
    config?.receiverTransformConfigured === true;
}

function workaroundState(target: WorkaroundTarget): EncodedAudioWorkaroundState {
  const userAgent = target.navigator?.userAgent ?? '';
  return {
    enabled: false,
    mode: 'workaround',
    supported: false,
    browserVersion: chromiumWorkaroundVersion(userAgent),
    disabledReason: null,
    peerConnectionCount: 0,
    audioReceiverCount: 0,
    frameCount: 0,
    frames: [],
    receiverStats: [],
    constructorFallbackCount: 0,
    claimedReceiverCount: 0,
    cleanedReceiverCount: 0,
    reconnectRequestCount: 0,
    failures: [],
    errorCode: null,
  };
}

/**
 * Enables the transparent encoded-audio pipe only on allowlisted Chromium.
 * Unsupported/failed setup falls back to the browser's normal receiver. Once
 * a stream has been claimed, an async failure closes that PC so LiveKit can
 * perform its normal reconnect path rather than silently continuing degraded.
 */
export function installEncodedAudioWorkaroundFromUrl(target: WorkaroundTarget = window): EncodedAudioWorkaroundState {
  const existing = target.__petalEncodedAudioWorkaround;
  if (existing) return existing;

  const state = workaroundState(target);
  target.__petalEncodedAudioWorkaround = state;
  const NativePeerConnection = target.RTCPeerConnection;
  const receiverPrototype = target.RTCRtpReceiver?.prototype as ReceiverWithEncodedStreams | undefined;
  const userAgent = target.navigator?.userAgent ?? '';

  if (encodedAudioWorkaroundDisabled(target.location.search)) {
    state.disabledReason = 'kill-switch';
    workaroundFailure(state, 'kill-switch', 'install');
    return state;
  }
  if (!chromiumWorkaroundAllowed(userAgent) || typeof NativePeerConnection !== 'function' ||
      typeof receiverPrototype?.createEncodedStreams !== 'function') {
    state.disabledReason = 'unsupported-browser';
    workaroundFailure(state, 'unsupported-browser', 'install');
    return state;
  }
  if (receiverTransformConfigured(target, undefined)) {
    state.disabledReason = 'e2ee-or-receiver-transform-configured';
    workaroundFailure(state, 'e2ee-or-receiver-transform-configured', 'install');
    return state;
  }

  state.enabled = true;
  state.supported = true;
  const installedReceivers = new WeakSet<RTCRtpReceiver>();
  const installReceiver = (
    connection: WorkaroundPeerConnection,
    event: RTCTrackEvent,
    receiverCleanups: Set<() => void>,
  ): void => {
    if (event.track.kind !== 'audio') return;
    const receiver = event.receiver;

    let cleaned = false;
    const abort = new AbortController();
    const cleanup = () => {
      if (cleaned) return;
      cleaned = true;
      abort.abort();
      receiverCleanups.delete(cleanup);
      state.cleanedReceiverCount += 1;
    };
    receiverCleanups.add(cleanup);
    event.track.addEventListener('ended', cleanup, { once: true });

    const encodedReceiver = receiver as WorkaroundReceiver;
    if (encodedReceiver.transform != null) {
      state.enabled = false;
      workaroundFailure(state, 'receiver-transform-configured', 'receiver');
      cleanup();
      return;
    }
    if (typeof encodedReceiver.createEncodedStreams !== 'function') {
      state.enabled = false;
      workaroundFailure(state, 'create-encoded-streams-unavailable', 'receiver');
      cleanup();
      return;
    }

    let streams: EncodedStreamsLike;
    try {
      streams = encodedReceiver.createEncodedStreams();
      state.claimedReceiverCount += 1;
      recordReceiverStats(state, receiver);
    } catch {
      state.enabled = false;
      workaroundFailure(state, 'create-encoded-streams-failed', 'receiver');
      cleanup();
      return;
    }

    try {
      const transformed = streams.readable.pipeThrough(new TransformStream<EncodedAudioFrameLike, EncodedAudioFrameLike>({
        transform(frame, controller) {
          recordEncodedAudioFrame(state, frame);
          if (state.frameCount === 1 || state.frameCount === 10 || state.frameCount === MAX_RECORDED_FRAMES) {
            recordReceiverStats(state, receiver);
          }
          controller.enqueue(frame);
        },
      }));
      void transformed.pipeTo(streams.writable, { signal: abort.signal }).catch(() => {
        if (cleaned) return;
        state.enabled = false;
        workaroundFailure(state, 'async-pipe-rejected', 'pipe');
        state.reconnectRequestCount += 1;
        try {
          connection.close();
        } catch {
          workaroundFailure(state, 'peer-connection-close-failed', 'cleanup');
        }
        cleanup();
      });
    } catch {
      // The stream was already claimed, so this is not a transparent fallback.
      state.enabled = false;
      workaroundFailure(state, 'async-pipe-rejected', 'pipe');
      state.reconnectRequestCount += 1;
      try {
        connection.close();
      } catch {
        workaroundFailure(state, 'peer-connection-close-failed', 'cleanup');
      }
      cleanup();
    }
  };

  const ProbePeerConnection = function (this: unknown, configuration?: RTCConfiguration) {
    if (!state.enabled) return Reflect.construct(NativePeerConnection, [configuration]);
    if (receiverTransformConfigured(target, configuration)) {
      state.enabled = false;
      state.disabledReason ??= 'e2ee-or-receiver-transform-configured';
      workaroundFailure(state, 'e2ee-or-receiver-transform-configured', 'constructor');
      return Reflect.construct(NativePeerConnection, [configuration]);
    }
    const probeConfiguration = { ...configuration, encodedInsertableStreams: true };
    let connection: WorkaroundPeerConnection;
    try {
      connection = Reflect.construct(NativePeerConnection, [probeConfiguration]) as WorkaroundPeerConnection;
    } catch {
      state.enabled = false;
      state.supported = false;
      state.constructorFallbackCount += 1;
      workaroundFailure(state, 'peer-connection-constructor-fallback', 'constructor');
      return Reflect.construct(NativePeerConnection, [configuration]);
    }
    state.peerConnectionCount += 1;
    const receiverCleanups = new Set<() => void>();
    const cleanup = () => {
      for (const receiverCleanup of Array.from(receiverCleanups)) receiverCleanup();
      receiverCleanups.clear();
    };
    connection.addEventListener('track', (event) => {
      if (!event || event.track.kind !== 'audio' || installedReceivers.has(event.receiver)) return;
      installedReceivers.add(event.receiver);
      state.audioReceiverCount += 1;
      // Give LiveKit's own E2EE/receiver-transform setup a chance to attach
      // synchronously before we claim the encoded stream.
      queueMicrotask(() => {
        if (event.track.readyState === 'ended') return;
        installReceiver(connection, event, receiverCleanups);
      });
    });
    connection.addEventListener('close', cleanup, { once: true });
    connection.addEventListener('connectionstatechange', () => {
      if (connection.connectionState === 'closed') cleanup();
    });
    return connection;
  } as unknown as typeof RTCPeerConnection;

  ProbePeerConnection.prototype = NativePeerConnection.prototype;
  Object.setPrototypeOf(ProbePeerConnection, NativePeerConnection);
  target.RTCPeerConnection = ProbePeerConnection;
  return state;
}
