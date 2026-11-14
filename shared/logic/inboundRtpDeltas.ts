// Window-track receive-path interval deltas for the remote-control latency
// summary (#375 web, #374 native). Field names are the W3C webrtc-stats
// names; both implementations must pass contracts/petal-contracts.json
// `inboundRtpDeltaVectors`. A counter that went backwards (resubscribe, new
// stream) or a missing / non-finite value yields null, never a guess or 0.

/** The cumulative inbound-rtp counters one interval delta is built from. */
export type InboundRtpCounters = {
  jitterBufferDelay?: number | null;
  jitterBufferTargetDelay?: number | null;
  jitterBufferMinimumDelay?: number | null;
  jitterBufferEmittedCount?: number | null;
  totalDecodeTime?: number | null;
  totalProcessingDelay?: number | null;
  framesDecoded?: number | null;
  framesDropped?: number | null;
  freezeCount?: number | null;
  nackCount?: number | null;
  pliCount?: number | null;
  packetsLost?: number | null;
  packetsReceived?: number | null;
};

export const INBOUND_RTP_COUNTER_KEYS = [
  'jitterBufferDelay',
  'jitterBufferTargetDelay',
  'jitterBufferMinimumDelay',
  'jitterBufferEmittedCount',
  'totalDecodeTime',
  'totalProcessingDelay',
  'framesDecoded',
  'framesDropped',
  'freezeCount',
  'nackCount',
  'pliCount',
  'packetsLost',
  'packetsReceived',
] as const satisfies readonly (keyof InboundRtpCounters)[];

export type InboundRtpDeltas = {
  jb_delay_ms: number | null;
  jb_target_ms: number | null;
  jb_min_ms: number | null;
  decode_ms: number | null;
  processing_ms: number | null;
  frames_dropped: number | null;
  freeze_count: number | null;
  nack_count: number | null;
  pli_count: number | null;
  packets_lost_pct_x100: number | null;
};

/** Closed buckets for the `jb_min_ms` dimension: performance data still never ships raw ms. */
export const JB_MIN_MS_BUCKETS = ['0', '1_19', '20_49', '50_99', '100_199', '200_plus'] as const;
export type JbMinMsBucket = (typeof JB_MIN_MS_BUCKETS)[number];

export function jbMinMsBucket(ms: number): JbMinMsBucket {
  if (!(ms >= 0.5)) return '0';
  if (ms < 20) return '1_19';
  if (ms < 50) return '20_49';
  if (ms < 100) return '50_99';
  if (ms < 200) return '100_199';
  return '200_plus';
}

type StatsLike = Record<string, unknown>;
export type StatsReportLike = { forEach(callback: (stats: unknown) => void): void };

function finite(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** `current - previous`, or null when either side is missing or the counter went backwards. */
function delta(previous: InboundRtpCounters, current: InboundRtpCounters, key: keyof InboundRtpCounters): number | null {
  const before = finite(previous[key]);
  const after = finite(current[key]);
  if (before === null || after === null || after < before) return null;
  return after - before;
}

/** Per-unit average in ms: Δseconds / Δcount × 1000, null without a positive count. */
function averageMs(
  previous: InboundRtpCounters,
  current: InboundRtpCounters,
  seconds: keyof InboundRtpCounters,
  count: keyof InboundRtpCounters
): number | null {
  const total = delta(previous, current, seconds);
  const units = delta(previous, current, count);
  if (total === null || units === null || units <= 0) return null;
  return (total / units) * 1000;
}

export function inboundRtpDeltas(previous: InboundRtpCounters, current: InboundRtpCounters): InboundRtpDeltas {
  const received = delta(previous, current, 'packetsReceived');
  const lostBefore = finite(previous.packetsLost);
  const lostAfter = finite(current.packetsLost);
  // packetsLost may fall when a late packet arrives; that interval lost nothing.
  const lost = lostBefore === null || lostAfter === null ? null : Math.max(0, lostAfter - lostBefore);
  const expected = received === null || lost === null ? null : received + lost;
  return {
    jb_delay_ms: averageMs(previous, current, 'jitterBufferDelay', 'jitterBufferEmittedCount'),
    jb_target_ms: averageMs(previous, current, 'jitterBufferTargetDelay', 'jitterBufferEmittedCount'),
    jb_min_ms: averageMs(previous, current, 'jitterBufferMinimumDelay', 'jitterBufferEmittedCount'),
    decode_ms: averageMs(previous, current, 'totalDecodeTime', 'framesDecoded'),
    processing_ms: averageMs(previous, current, 'totalProcessingDelay', 'framesDecoded'),
    frames_dropped: delta(previous, current, 'framesDropped'),
    freeze_count: delta(previous, current, 'freezeCount'),
    nack_count: delta(previous, current, 'nackCount'),
    pli_count: delta(previous, current, 'pliCount'),
    packets_lost_pct_x100:
      expected === null || lost === null || expected <= 0 ? null : Math.round((lost / expected) * 10_000),
  };
}

function isVideoInbound(stats: StatsLike): boolean {
  if (stats.type !== 'inbound-rtp') return false;
  const kind = typeof stats.kind === 'string' ? stats.kind : stats.mediaType;
  return kind === 'video';
}

/** The video inbound-rtp counters in a receiver's stats report (most bytes wins), or null. */
export function inboundRtpCounters(report: StatsReportLike): InboundRtpCounters | null {
  let best: StatsLike | null = null;
  report.forEach((value) => {
    const stats = value as StatsLike;
    if (!stats || typeof stats !== 'object' || !isVideoInbound(stats)) return;
    if (!best || (finite(stats.bytesReceived) ?? -1) > (finite(best.bytesReceived) ?? -1)) best = stats;
  });
  if (!best) return null;
  const source: StatsLike = best;
  const counters: InboundRtpCounters = {};
  for (const key of INBOUND_RTP_COUNTER_KEYS) {
    const value = finite(source[key]);
    if (value !== null) counters[key] = value;
  }
  return counters;
}

/**
 * The selected ICE candidate pair's `currentRoundTripTime` in ms: via the
 * transport's `selectedCandidatePairId`, else the nominated succeeded pair.
 */
export function selectedCandidatePairRttMs(report: StatsReportLike): number | null {
  const pairs = new Map<string, StatsLike>();
  let selectedId: string | null = null;
  let nominated: StatsLike | null = null;
  report.forEach((value) => {
    const stats = value as StatsLike;
    if (!stats || typeof stats !== 'object') return;
    if (stats.type === 'transport' && typeof stats.selectedCandidatePairId === 'string') {
      selectedId = stats.selectedCandidatePairId;
    } else if (stats.type === 'candidate-pair') {
      if (typeof stats.id === 'string') pairs.set(stats.id, stats);
      if (stats.nominated === true && stats.state === 'succeeded') nominated = stats;
    }
  });
  const pair: StatsLike | null = (selectedId !== null ? pairs.get(selectedId) : undefined) ?? nominated;
  const seconds = finite(pair?.currentRoundTripTime);
  return seconds === null || seconds < 0 ? null : seconds * 1000;
}
