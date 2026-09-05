export interface RemoteWindowStatsSnapshot {
  fps: number | null;
  width: number | null;
  height: number | null;
  bitrateBps: number | null;
  framesReceived: number | null;
  framesDecoded: number | null;
  framesPresented: number | null;
  presentedFps: number | null;
  secondsSinceLastDecodedFrame: number | null;
  secondsSinceLastPresentedFrame: number | null;
  packetsLost: number | null;
  jitterMs: number | null;
  freezeCount: number | null;
  framesDropped: number | null;
  qualityLimitationReason: string | null;
}

export interface RemoteWindowStatsState {
  sampledAtMs: number;
  bytesReceived: number | null;
  framesDecoded: number | null;
  framesPresented: number | null;
  lastDecodedFrameAtMs: number | null;
  lastPresentedFrameAtMs: number | null;
}

export interface RemoteWindowStatsDerivation {
  snapshot: RemoteWindowStatsSnapshot;
  state: RemoteWindowStatsState;
}

export interface RemoteWindowVideoSize {
  width: number;
  height: number;
}

export interface RemoteWindowPlaybackQuality {
  totalVideoFrames?: number | null;
}

export interface RemoteWindowDebugLine {
  label: string;
  value: string;
  prominent?: boolean;
}

export const REMOTE_WINDOW_FRESHNESS_STALE_MS = 3000;

/** Compact, hover-only copy for the shared-window header. */
export function formatRemoteWindowFreshness(
  lastFrameReceivedMs: number | null | undefined,
  nowMs = Date.now()
): string {
  if (!lastFrameReceivedMs || !Number.isFinite(lastFrameReceivedMs)) {
    return 'waiting · no frame received yet';
  }

  const ageMs = Math.max(0, nowMs - lastFrameReceivedMs);
  const seconds = Math.floor(ageMs / 1000);
  const age = seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m`;
  return `${ageMs >= REMOTE_WINDOW_FRESHNESS_STALE_MS ? 'stale' : 'live'} · updated ${age} ago`;
}

type StatsLike = Record<string, unknown>;
type StatsReportLike = Pick<RTCStatsReport, 'forEach'>;

function finiteNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function nonnegativeInteger(value: unknown): number | null {
  const number = finiteNumber(value);
  if (number === null || number < 0) return null;
  return Math.round(number);
}

function positiveInteger(value: unknown): number | null {
  const number = nonnegativeInteger(value);
  return number !== null && number > 0 ? number : null;
}

function statsKind(stats: StatsLike): string {
  const kind = typeof stats.kind === 'string' ? stats.kind : null;
  const mediaType = typeof stats.mediaType === 'string' ? stats.mediaType : null;
  return (kind ?? mediaType ?? '').toLowerCase();
}

function hasVideoCounters(stats: StatsLike): boolean {
  return (
    finiteNumber(stats.framesDecoded) !== null ||
    finiteNumber(stats.framesReceived) !== null ||
    finiteNumber(stats.frameWidth) !== null ||
    finiteNumber(stats.frameHeight) !== null ||
    finiteNumber(stats.framesPerSecond) !== null
  );
}

function isVideoStats(stats: StatsLike): boolean {
  const kind = statsKind(stats);
  return kind === 'video' || (!kind && hasVideoCounters(stats));
}

function betterInboundStats(current: StatsLike | null, candidate: StatsLike): StatsLike {
  if (!current) return candidate;
  const currentBytes = finiteNumber(current.bytesReceived) ?? -1;
  const candidateBytes = finiteNumber(candidate.bytesReceived) ?? -1;
  return candidateBytes > currentBytes ? candidate : current;
}

function firstNumber(...values: Array<unknown>): number | null {
  for (const value of values) {
    const number = finiteNumber(value);
    if (number !== null) return number;
  }
  return null;
}

function firstPositiveInteger(...values: Array<unknown>): number | null {
  for (const value of values) {
    const number = positiveInteger(value);
    if (number !== null) return number;
  }
  return null;
}

function firstNonnegativeInteger(...values: Array<unknown>): number | null {
  for (const value of values) {
    const number = nonnegativeInteger(value);
    if (number !== null) return number;
  }
  return null;
}

function collectVideoStats(report: StatsReportLike): {
  inbound: StatsLike | null;
  track: StatsLike | null;
  mediaSource: StatsLike | null;
} {
  let inbound: StatsLike | null = null;
  let track: StatsLike | null = null;
  let mediaSource: StatsLike | null = null;

  report.forEach((raw) => {
    const stats = raw as StatsLike;
    if (stats.type === 'inbound-rtp' && isVideoStats(stats)) {
      inbound = betterInboundStats(inbound, stats);
    } else if (stats.type === 'track' && isVideoStats(stats) && !track) {
      track = stats;
    } else if (stats.type === 'media-source' && isVideoStats(stats) && !mediaSource) {
      mediaSource = stats;
    }
  });

  return { inbound, track, mediaSource };
}

function deriveBitrateBps(
  bytesReceived: number | null,
  previous: RemoteWindowStatsState | null,
  sampledAtMs: number
): number | null {
  if (
    bytesReceived === null ||
    previous?.bytesReceived === null ||
    previous?.bytesReceived === undefined ||
    sampledAtMs <= previous.sampledAtMs
  ) {
    return null;
  }
  const byteDelta = bytesReceived - previous.bytesReceived;
  if (byteDelta < 0) return null;
  const seconds = (sampledAtMs - previous.sampledAtMs) / 1000;
  return seconds > 0 ? (byteDelta * 8) / seconds : null;
}

function deriveFps(
  reportedFps: number | null,
  framesDecoded: number | null,
  previous: RemoteWindowStatsState | null,
  sampledAtMs: number
): number | null {
  if (reportedFps !== null) return reportedFps;
  if (
    framesDecoded === null ||
    previous?.framesDecoded === null ||
    previous?.framesDecoded === undefined ||
    sampledAtMs <= previous.sampledAtMs
  ) {
    return null;
  }
  const frameDelta = framesDecoded - previous.framesDecoded;
  if (frameDelta < 0) return null;
  const seconds = (sampledAtMs - previous.sampledAtMs) / 1000;
  return seconds > 0 ? frameDelta / seconds : null;
}

function deriveLastDecodedFrameAtMs(
  framesDecoded: number | null,
  previous: RemoteWindowStatsState | null,
  sampledAtMs: number
): number | null {
  if (framesDecoded === null) return previous?.lastDecodedFrameAtMs ?? null;
  if (previous?.framesDecoded === null || previous?.framesDecoded === undefined) {
    return framesDecoded > 0 ? sampledAtMs : null;
  }
  if (framesDecoded > previous.framesDecoded) return sampledAtMs;
  return previous.lastDecodedFrameAtMs;
}

function derivePresentedFps(
  framesPresented: number | null,
  previous: RemoteWindowStatsState | null,
  sampledAtMs: number
): number | null {
  if (
    framesPresented === null ||
    previous?.framesPresented === null ||
    previous?.framesPresented === undefined ||
    sampledAtMs <= previous.sampledAtMs
  ) {
    return null;
  }
  const frameDelta = framesPresented - previous.framesPresented;
  if (frameDelta < 0) return null;
  const seconds = (sampledAtMs - previous.sampledAtMs) / 1000;
  return seconds > 0 ? frameDelta / seconds : null;
}

function deriveLastPresentedFrameAtMs(
  framesPresented: number | null,
  previous: RemoteWindowStatsState | null,
  sampledAtMs: number
): number | null {
  if (framesPresented === null) return previous?.lastPresentedFrameAtMs ?? null;
  if (previous?.framesPresented === null || previous?.framesPresented === undefined) {
    return framesPresented > 0 ? sampledAtMs : null;
  }
  if (framesPresented > previous.framesPresented) return sampledAtMs;
  return previous.lastPresentedFrameAtMs;
}

export function deriveRemoteWindowStats(
  report: StatsReportLike,
  previous: RemoteWindowStatsState | null = null,
  sampledAtMs = Date.now(),
  fallbackSize?: RemoteWindowVideoSize,
  playbackQuality?: RemoteWindowPlaybackQuality | null
): RemoteWindowStatsDerivation {
  const { inbound, track, mediaSource } = collectVideoStats(report);
  const bytesReceived = firstNumber(inbound?.bytesReceived);
  const framesDecoded = firstNonnegativeInteger(inbound?.framesDecoded, track?.framesDecoded);
  const framesPresented = firstNonnegativeInteger(playbackQuality?.totalVideoFrames);
  const lastDecodedFrameAtMs = deriveLastDecodedFrameAtMs(framesDecoded, previous, sampledAtMs);
  const lastPresentedFrameAtMs = deriveLastPresentedFrameAtMs(framesPresented, previous, sampledAtMs);
  const secondsSinceLastDecodedFrame =
    lastDecodedFrameAtMs === null ? null : Math.max(0, (sampledAtMs - lastDecodedFrameAtMs) / 1000);
  const secondsSinceLastPresentedFrame =
    lastPresentedFrameAtMs === null ? null : Math.max(0, (sampledAtMs - lastPresentedFrameAtMs) / 1000);

  const snapshot: RemoteWindowStatsSnapshot = {
    fps: deriveFps(firstNumber(inbound?.framesPerSecond, track?.framesPerSecond), framesDecoded, previous, sampledAtMs),
    width: firstPositiveInteger(inbound?.frameWidth, track?.frameWidth, mediaSource?.width, fallbackSize?.width),
    height: firstPositiveInteger(inbound?.frameHeight, track?.frameHeight, mediaSource?.height, fallbackSize?.height),
    bitrateBps: deriveBitrateBps(bytesReceived, previous, sampledAtMs),
    framesReceived: firstNonnegativeInteger(inbound?.framesReceived, track?.framesReceived),
    framesDecoded,
    framesPresented,
    presentedFps: derivePresentedFps(framesPresented, previous, sampledAtMs),
    secondsSinceLastDecodedFrame,
    secondsSinceLastPresentedFrame,
    packetsLost: firstNumber(inbound?.packetsLost),
    jitterMs: firstNumber(inbound?.jitter) === null ? null : firstNumber(inbound?.jitter)! * 1000,
    freezeCount: firstNonnegativeInteger(inbound?.freezeCount),
    framesDropped: firstNonnegativeInteger(inbound?.framesDropped),
    // qualityLimitationReason is defined by the WebRTC spec only on
    // outbound-rtp stats, never on inbound-rtp -- so on this receiver-side
    // path it is structurally always undefined/null, never a measured value.
    // Downstream consumers must render that as "unmeasured", not coerce it
    // to a real WebRTC value like "none" (see issue #180 review).
    qualityLimitationReason:
      typeof inbound?.qualityLimitationReason === 'string'
        ? inbound.qualityLimitationReason
        : null,
  };

  return {
    snapshot,
    state: {
      sampledAtMs,
      bytesReceived,
      framesDecoded,
      framesPresented,
      lastDecodedFrameAtMs,
      lastPresentedFrameAtMs,
    },
  };
}

function formatNumber(value: number | null, digits = 1): string {
  if (value === null) return 'n/a';
  if (Math.abs(value) >= 100 || Number.isInteger(value)) return value.toFixed(0);
  return value.toFixed(digits);
}

export function formatBitrate(bps: number | null): string {
  if (bps === null) return 'warming up';
  if (bps < 1000) return `${bps.toFixed(0)} bps`;
  if (bps < 1_000_000) return `${(bps / 1000).toFixed(bps < 100_000 ? 1 : 0)} kbps`;
  return `${(bps / 1_000_000).toFixed(bps < 10_000_000 ? 1 : 0)} Mbps`;
}

/**
 * A small SVG line-path for a rolling history of samples, so the debug
 * overlay can show a trend instead of only the latest instantaneous number
 * (mirrors the plain per-metric sparklines in the native NetworkCockpit,
 * apps/desktop/src/lib/data/networkCockpit.ts `sparkPath`). Pure/testable:
 * takes the values array directly rather than reading DOM/live state.
 */
export function sparkPath(values: (number | null)[], w = 120, h = 28): string {
  const nums = values.filter((v): v is number => v !== null && !Number.isNaN(v));
  if (nums.length < 2) return '';
  const min = Math.min(...nums);
  const max = Math.max(...nums);
  const span = max - min || 1;
  const n = values.length;
  let d = '';
  let pen = false;
  values.forEach((v, i) => {
    if (v === null || Number.isNaN(v)) {
      pen = false;
      return;
    }
    const x = n > 1 ? (i / (n - 1)) * w : 0;
    const y = h - 2 - ((v - min) / span) * (h - 4);
    d += `${pen ? 'L' : 'M'}${x.toFixed(1)} ${y.toFixed(1)}`;
    pen = true;
  });
  return d;
}

export function formatRemoteWindowDebugStats(snapshot: RemoteWindowStatsSnapshot): RemoteWindowDebugLine[] {
  const resolution =
    snapshot.width !== null && snapshot.height !== null ? `${snapshot.width}x${snapshot.height}` : 'unknown';
  const frames =
    snapshot.framesReceived !== null || snapshot.framesDecoded !== null
      ? `${snapshot.framesReceived ?? 'n/a'} / ${snapshot.framesDecoded ?? 'n/a'}`
      : 'n/a';
  const presented =
    snapshot.framesPresented !== null
      ? `${formatNumber(snapshot.framesPresented, 0)} / ${formatNumber(snapshot.presentedFps, 1)} fps`
      : 'n/a';
  const lines: RemoteWindowDebugLine[] = [
    {
      label: 'Last frame',
      value:
        snapshot.secondsSinceLastDecodedFrame === null
          ? 'waiting'
          : `${formatNumber(snapshot.secondsSinceLastDecodedFrame, 1)}s`,
      prominent: true,
    },
    { label: 'FPS', value: formatNumber(snapshot.fps, 1) },
    { label: 'Size', value: resolution },
    { label: 'Bitrate', value: formatBitrate(snapshot.bitrateBps) },
    { label: 'Frames', value: frames },
    { label: 'Presented', value: presented },
  ];

  if (snapshot.packetsLost !== null) {
    lines.push({ label: 'Lost', value: formatNumber(snapshot.packetsLost, 0) });
  }
  if (snapshot.jitterMs !== null) {
    lines.push({ label: 'Jitter', value: `${formatNumber(snapshot.jitterMs, 1)}ms` });
  }

  return lines;
}
