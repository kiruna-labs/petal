type StatsLike = Record<string, unknown>;

export interface AudioReceiverTelemetry {
  packetsReceived: number | null;
  payloadType: number | null;
  codecId: string | null;
  codecMimeType: string | null;
  codecPayloadType: number | null;
  codecClockRate: number | null;
  codecChannels: number | null;
  totalSamplesReceived: number | null;
  totalSamplesDuration: number | null;
  totalAudioEnergy: number | null;
  jitterBufferEmittedCount: number | null;
  jitterBufferDelay: number | null;
}

export interface AudioReceiverStatsSummary {
  bytesReceived: number | null;
  jitter: number | null;
  totalSamplesDuration: number | null;
  totalAudioEnergy: number | null;
  concealedSamples: number | null;
  concealmentEvents: number | null;
}

export interface PublicAudioReceiverStats {
  bytesReceived?: unknown;
  jitter?: unknown;
  totalSamplesDuration?: unknown;
  totalAudioEnergy?: unknown;
  concealedSamples?: unknown;
  concealmentEvents?: unknown;
}

export interface PublicRemoteAudioTrack {
  getRTCStatsReport?: () => Promise<RTCStatsReport | undefined>;
  getReceiverStats?: () => Promise<PublicAudioReceiverStats | undefined>;
}

export interface AudioReceiverTelemetryOptions {
  intervalMs?: number;
  maxSamples?: number;
  scheduler?: AudioReceiverTelemetryScheduler;
}

type TimerHandle = ReturnType<typeof globalThis.setInterval>;

export interface AudioReceiverTelemetryScheduler {
  setInterval(callback: () => void, delayMs: number): TimerHandle;
  clearInterval(handle: TimerHandle): void;
}

function finiteNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function stringValue(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

function audioInboundStats(report: RTCStatsReport): StatsLike | null {
  let best: StatsLike | null = null;
  report.forEach((raw) => {
    const stat = raw as StatsLike;
    if (stat.type !== 'inbound-rtp' || (stat.kind !== 'audio' && stat.mediaType !== 'audio')) return;
    if ((finiteNumber(stat.packetsReceived) ?? -1) >= (finiteNumber(best?.packetsReceived) ?? -1)) best = stat;
  });
  return best;
}

/** Extract only browser decoder/codec counters. No participant, room, SSRC, or track names. */
export function audioReceiverTelemetryFromStatsReport(report: RTCStatsReport): AudioReceiverTelemetry | null {
  const inbound = audioInboundStats(report);
  if (!inbound) return null;

  const codecId = stringValue(inbound.codecId);
  let codec: StatsLike | null = null;
  if (codecId) {
    report.forEach((raw) => {
      const stat = raw as StatsLike;
      if (stat.type === 'codec' && stat.id === codecId) codec = stat;
    });
  }
  // The assignment happens inside RTCStatsReport.forEach, so make the
  // callback-produced value explicit for TypeScript's control-flow analysis.
  const codecStats = codec as StatsLike | null;

  return {
    packetsReceived: finiteNumber(inbound.packetsReceived),
    payloadType: finiteNumber(inbound.payloadType),
    codecId,
    codecMimeType: stringValue(codecStats?.mimeType),
    codecPayloadType: finiteNumber(codecStats?.payloadType),
    codecClockRate: finiteNumber(codecStats?.clockRate),
    codecChannels: finiteNumber(codecStats?.channels),
    totalSamplesReceived: finiteNumber(inbound.totalSamplesReceived),
    totalSamplesDuration: finiteNumber(inbound.totalSamplesDuration),
    totalAudioEnergy: finiteNumber(inbound.totalAudioEnergy),
    jitterBufferEmittedCount: finiteNumber(inbound.jitterBufferEmittedCount),
    jitterBufferDelay: finiteNumber(inbound.jitterBufferDelay),
  };
}

export function audioReceiverStatsSummary(stats: PublicAudioReceiverStats | undefined): AudioReceiverStatsSummary | null {
  if (!stats) return null;
  return {
    bytesReceived: finiteNumber(stats.bytesReceived),
    jitter: finiteNumber(stats.jitter),
    totalSamplesDuration: finiteNumber(stats.totalSamplesDuration),
    totalAudioEnergy: finiteNumber(stats.totalAudioEnergy),
    concealedSamples: finiteNumber(stats.concealedSamples),
    concealmentEvents: finiteNumber(stats.concealmentEvents),
  };
}

function printable(value: string | number | null): string {
  return value === null ? 'unavailable' : String(value);
}

/** A one-line, privacy-safe session-log record for issue #510 diagnosis. */
export function formatAudioReceiverTelemetry(
  sample: number,
  telemetry: AudioReceiverTelemetry | null,
  receiverStats: AudioReceiverStatsSummary | null,
  maxSamples = 3
): string {
  const sampleLimit = Math.max(1, Math.floor(maxSamples));
  if (!telemetry) return `audio receiver stats ${sample}/${sampleLimit}: inbound audio stats unavailable`;

  const receiver = receiverStats
    ? ` receiver{bytesReceived=${printable(receiverStats.bytesReceived)} jitter=${printable(receiverStats.jitter)} totalSamplesDuration=${printable(receiverStats.totalSamplesDuration)} totalAudioEnergy=${printable(receiverStats.totalAudioEnergy)} concealedSamples=${printable(receiverStats.concealedSamples)} concealmentEvents=${printable(receiverStats.concealmentEvents)}}`
    : ' receiver{unavailable}';
  return `audio receiver stats ${sample}/${sampleLimit}: inbound{packetsReceived=${printable(telemetry.packetsReceived)} payloadType=${printable(telemetry.payloadType)} codecId=${printable(telemetry.codecId)} codecMime=${printable(telemetry.codecMimeType)} totalSamplesReceived=${printable(telemetry.totalSamplesReceived)} totalSamplesDuration=${printable(telemetry.totalSamplesDuration)} totalAudioEnergy=${printable(telemetry.totalAudioEnergy)} jitterBufferEmittedCount=${printable(telemetry.jitterBufferEmittedCount)} jitterBufferDelay=${printable(telemetry.jitterBufferDelay)}} codec{payloadType=${printable(telemetry.codecPayloadType)} clockRate=${printable(telemetry.codecClockRate)} channels=${printable(telemetry.codecChannels)}}${receiver}`;
}

/**
 * Polls only public LiveKit receiver APIs for three bounded samples. The
 * returned cleanup is idempotent so TrackUnsubscribed/session teardown can
 * stop the timer before the natural three-sample limit.
 */
export function startAudioReceiverTelemetry(
  track: PublicRemoteAudioTrack,
  logEvent: (message: string, level?: 'info' | 'ok' | 'warn' | 'error') => void,
  options: AudioReceiverTelemetryOptions = {}
): () => void {
  const maxSamples = Math.max(1, Math.floor(options.maxSamples ?? 3));
  const intervalMs = Math.max(1, Math.floor(options.intervalMs ?? 4000));
  const scheduler = options.scheduler ?? {
    setInterval: (callback: () => void, delayMs: number) => globalThis.setInterval(callback, delayMs),
    clearInterval: (handle: TimerHandle) => globalThis.clearInterval(handle),
  };
  let sampleNumber = 0;
  let stopped = false;
  let generation = 0;
  let running = false;
  let timer: TimerHandle | null = null;

  const stop = () => {
    if (stopped) return;
    stopped = true;
    generation += 1;
    if (timer !== null) scheduler.clearInterval(timer);
    timer = null;
  };

  const sample = async () => {
    if (stopped || running) return;
    const sampleGeneration = generation;
    running = true;
    try {
      let telemetry: AudioReceiverTelemetry | null = null;
      let receiverStats: AudioReceiverStatsSummary | null = null;
      if (typeof track.getRTCStatsReport === 'function') {
        try {
          const report = await track.getRTCStatsReport();
          if (stopped || sampleGeneration !== generation) return;
          telemetry = report ? audioReceiverTelemetryFromStatsReport(report) : null;
        } catch {
          telemetry = null;
        }
      }
      if (typeof track.getReceiverStats === 'function') {
        try {
          const stats = await track.getReceiverStats();
          if (stopped || sampleGeneration !== generation) return;
          receiverStats = audioReceiverStatsSummary(stats);
        } catch {
          receiverStats = null;
        }
      }
      if (stopped || sampleGeneration !== generation) return;
      sampleNumber += 1;
      logEvent(
        formatAudioReceiverTelemetry(sampleNumber, telemetry, receiverStats, maxSamples),
        telemetry ? 'info' : 'warn'
      );
      if (sampleNumber >= maxSamples) stop();
    } finally {
      running = false;
    }
  };

  timer = scheduler.setInterval(() => void sample(), intervalMs);
  void sample();
  return stop;
}
