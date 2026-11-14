// Per-control-session remote-control latency aggregator (#370/#375). Rust
// mirror: apps/desktop/src-tauri/src/rc_latency_telemetry.rs. Keys, cadence,
// rendering and the example are pinned by contracts/petal-contracts.json
// `remoteControlLatencySummary`; add a key there and on both sides together.
//
// Producers call `recordStage`, `recordStageWithMax`, `noteMax`, `addCounter`
// and `setDimension`. The owner calls `sync(active, nowMs)` on a timer with
// the current "is control active" truth; each 60 s interval with samples (or
// a non-zero counter) is handed to `emit` once. Integer-ms aggregates only.

import { LatencyHistogram, type LatencyHistogramStats } from './latencyHistogram';

export type RcRole = 'host' | 'controller';

export const RC_LATENCY_STAGES = [
  'input_leg_ms',
  'input_leg_lossy_ms',
  'input_rtt_ms',
  'input_excess_delay_ms',
  'replay_inject_ms',
  'replay_complete_ms',
  'inject_to_capture_ms',
  'capture_interval_ms',
  'capture_to_encoded_ms',
  'encoded_to_packetized_ms',
  'pub_rtt_ms',
  'sub_rtt_ms',
  'video_net_ms',
  'capture_to_receive_ms',
  'jb_delay_ms',
  'jb_target_ms',
  'decode_ms',
  'processing_ms',
  'receive_to_present_ms',
  'capture_to_present_ms',
  'photon_ms',
] as const;
export type RcLatencyStage = (typeof RC_LATENCY_STAGES)[number];

/** Summed per interval; a zero counter is never emitted. */
export const RC_LATENCY_COUNTERS = [
  'input_stall_count',
  'frames_dropped',
  'freeze_count',
  'nack_count',
  'pli_count',
] as const;
export type RcLatencyCounter = (typeof RC_LATENCY_COUNTERS)[number];

/** Max per interval; emitted whenever set (0 included), never emit an interval alone. */
export const RC_LATENCY_MAX_COUNTERS = ['offset_err_ms', 'packets_lost_pct_x100'] as const;
export type RcLatencyMaxCounter = (typeof RC_LATENCY_MAX_COUNTERS)[number];

/** `null` = free-form token (lowercase `[a-z0-9_-]`, at most 32 chars). */
export const RC_LATENCY_DIMENSIONS = {
  lk_region: null,
  same_edge: ['true', 'false', 'unknown'],
  self_hosted_sfu: ['true', 'false', 'unknown'],
  ice_candidate_type: ['host', 'srflx', 'prflx', 'relay', 'unknown'],
  ice_protocol: ['udp', 'tcp', 'unknown'],
  window_codec: ['h264', 'h265', 'vp8', 'vp9', 'av1', 'unknown'],
  content_change: ['detected', 'unknown'],
  jb_min_ms: ['0', '1_19', '20_49', '50_99', '100_199', '200_plus'],
  frame_metadata: ['available', 'unavailable'],
} as const satisfies Record<string, readonly string[] | null>;
export type RcLatencyDimension = keyof typeof RC_LATENCY_DIMENSIONS;
export const RC_LATENCY_DIMENSION_KEYS = Object.keys(RC_LATENCY_DIMENSIONS) as RcLatencyDimension[];

export const RC_LATENCY_ROLES: readonly RcRole[] = ['host', 'controller'];
export const RC_LATENCY_INTERVAL_MS = 60_000;
export const RC_LATENCY_FINAL_MIN_SAMPLES = 5;
export const RC_LATENCY_MAX_EVENTS_PER_SESSION = 120;
export const RC_LATENCY_LOG_PREFIX = 'remote-control-latency-summary:';

export type RcLatencySummary = {
  role: RcRole;
  rcSessionId: string;
  intervalS: number;
  final: boolean;
  stages: Partial<Record<RcLatencyStage, LatencyHistogramStats>>;
  counters: Partial<Record<RcLatencyCounter, number>>;
  maxCounters: Partial<Record<RcLatencyMaxCounter, number>>;
  dimensions: Partial<Record<RcLatencyDimension, string>>;
};

const TOKEN = /^[a-z0-9_-]{1,32}$/;

export function rcLatencyDimensionValueAllowed(key: RcLatencyDimension, value: string): boolean {
  const allowed = RC_LATENCY_DIMENSIONS[key] as readonly string[] | null;
  return allowed === null ? TOKEN.test(value) : allowed.includes(value);
}

/** PostHog properties, in log-line order. Common properties are added by the analytics pipe. */
export function rcLatencySummaryProperties(summary: RcLatencySummary): Record<string, string | number | boolean> {
  const properties: Record<string, string | number | boolean> = {
    role: summary.role,
    rc_session_id: summary.rcSessionId,
    interval_s: summary.intervalS,
    final: summary.final,
  };
  for (const stage of RC_LATENCY_STAGES) {
    const stats = summary.stages[stage];
    if (!stats || stats.n === 0) continue;
    properties[`${stage}_p50`] = stats.p50;
    properties[`${stage}_p95`] = stats.p95;
    properties[`${stage}_max`] = stats.max;
    properties[`${stage}_n`] = stats.n;
  }
  for (const counter of RC_LATENCY_COUNTERS) {
    const value = summary.counters[counter];
    if (value !== undefined && value > 0) properties[counter] = value;
  }
  for (const counter of RC_LATENCY_MAX_COUNTERS) {
    const value = summary.maxCounters[counter];
    if (value !== undefined) properties[counter] = value;
  }
  for (const key of RC_LATENCY_DIMENSION_KEYS) {
    const value = summary.dimensions[key];
    if (value !== undefined) properties[key] = value;
  }
  return properties;
}

/** The `remote-control-latency-summary:` log line; same keys and order as the properties. */
export function rcLatencySummaryLogLine(summary: RcLatencySummary): string {
  const parts = Object.entries(rcLatencySummaryProperties(summary)).map(([key, value]) => ` ${key}=${value}`);
  return `${RC_LATENCY_LOG_PREFIX}${parts.join('')}`;
}

function newRcSessionId(): string {
  const bytes = new Uint8Array(16);
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    crypto.getRandomValues(bytes);
  } else {
    for (let i = 0; i < 16; i += 1) bytes[i] = Math.floor(Math.random() * 256);
  }
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function nonNegativeInteger(value: number): number | null {
  return Number.isFinite(value) && value >= 0 ? Math.round(value) : null;
}

type Session = {
  rcSessionId: string;
  intervalStartedAt: number;
  eventsEmitted: number;
  stages: Map<RcLatencyStage, LatencyHistogram>;
  counters: Map<RcLatencyCounter, number>;
  maxCounters: Map<RcLatencyMaxCounter, number>;
  dimensions: Map<RcLatencyDimension, string>;
};

export type RcLatencyAggregatorOptions = {
  emit: (summary: RcLatencySummary) => void;
  newSessionId?: () => string;
};

export class RcLatencyAggregator {
  readonly role: RcRole;
  private session: Session | null = null;
  private readonly emit: (summary: RcLatencySummary) => void;
  private readonly newSessionId: () => string;

  constructor(role: RcRole, options: RcLatencyAggregatorOptions) {
    this.role = role;
    this.emit = options.emit;
    this.newSessionId = options.newSessionId ?? newRcSessionId;
  }

  get active(): boolean {
    return this.session !== null;
  }

  /** Reconcile with the current control state: start, emit a due interval, or end with a final partial. */
  sync(active: boolean, nowMs: number): void {
    const session = this.session;
    if (active && !session) {
      this.session = {
        rcSessionId: this.newSessionId(),
        intervalStartedAt: nowMs,
        eventsEmitted: 0,
        stages: new Map(),
        counters: new Map(),
        maxCounters: new Map(),
        dimensions: new Map(),
      };
      return;
    }
    if (!session) return;
    if (!active) {
      this.session = null;
      // A counter-only final partial still emits: n >= 5 is about percentile quality (#371).
      const enough =
        [...session.counters.values()].some((value) => value > 0) ||
        [...session.stages.values()].some((histogram) => histogram.n >= RC_LATENCY_FINAL_MIN_SAMPLES);
      if (enough) this.takeInterval(session, nowMs, true);
      return;
    }
    if (nowMs - session.intervalStartedAt >= RC_LATENCY_INTERVAL_MS) this.takeInterval(session, nowMs, false);
  }

  /** False when no session is active or the value is rejected (negative / non-finite). */
  recordStage(stage: RcLatencyStage, ms: number): boolean {
    const session = this.session;
    if (!session) return false;
    let histogram = session.stages.get(stage);
    if (!histogram) {
      histogram = new LatencyHistogram();
      session.stages.set(stage, histogram);
    }
    return histogram.record(ms);
  }

  /** A stage sample plus the max counter that qualifies it; the max lands only with an accepted sample. */
  recordStageWithMax(stage: RcLatencyStage, ms: number, counter: RcLatencyMaxCounter, value: number): void {
    if (this.recordStage(stage, ms)) this.noteMax(counter, value);
  }

  /** Raise a max counter for this interval. Never makes an interval emit on its own. */
  noteMax(counter: RcLatencyMaxCounter, value: number): void {
    const session = this.session;
    const rounded = nonNegativeInteger(value);
    if (!session || rounded === null) return;
    session.maxCounters.set(counter, Math.max(session.maxCounters.get(counter) ?? 0, rounded));
  }

  addCounter(counter: RcLatencyCounter, delta: number): void {
    const session = this.session;
    const rounded = nonNegativeInteger(delta);
    if (!session || rounded === null) return;
    session.counters.set(counter, (session.counters.get(counter) ?? 0) + rounded);
  }

  /** False (old value kept) for a value outside the dimension's closed set, or with no session. */
  setDimension(key: RcLatencyDimension, value: string): boolean {
    const session = this.session;
    if (!session || !rcLatencyDimensionValueAllowed(key, value)) return false;
    session.dimensions.set(key, value);
    return true;
  }

  /** #372: remove a dimension that no longer has data (missing is omitted). */
  clearDimension(key: RcLatencyDimension): void {
    this.session?.dimensions.delete(key);
  }

  /** Drain the interval. Empty intervals (no samples, no non-zero counter) and capped sessions emit nothing. */
  private takeInterval(session: Session, nowMs: number, final: boolean): void {
    const stages: RcLatencySummary['stages'] = {};
    for (const stage of RC_LATENCY_STAGES) {
      const stats = session.stages.get(stage)?.stats();
      if (stats) stages[stage] = stats;
    }
    const counters: RcLatencySummary['counters'] = {};
    for (const counter of RC_LATENCY_COUNTERS) {
      const value = session.counters.get(counter) ?? 0;
      if (value > 0) counters[counter] = value;
    }
    const maxCounters: RcLatencySummary['maxCounters'] = {};
    for (const counter of RC_LATENCY_MAX_COUNTERS) {
      const value = session.maxCounters.get(counter);
      if (value !== undefined) maxCounters[counter] = value;
    }
    const elapsedMs = Math.max(0, Math.floor(nowMs - session.intervalStartedAt));
    session.intervalStartedAt = nowMs;
    session.stages.forEach((histogram) => histogram.reset());
    session.counters.clear();
    session.maxCounters.clear();
    const empty = Object.keys(stages).length === 0 && Object.keys(counters).length === 0;
    if (empty || session.eventsEmitted >= RC_LATENCY_MAX_EVENTS_PER_SESSION) return;
    session.eventsEmitted += 1;
    const dimensions: RcLatencySummary['dimensions'] = {};
    for (const key of RC_LATENCY_DIMENSION_KEYS) {
      const value = session.dimensions.get(key);
      if (value !== undefined) dimensions[key] = value;
    }
    this.emit({
      role: this.role,
      rcSessionId: session.rcSessionId,
      intervalS: Math.floor((elapsedMs + 500) / 1000),
      final,
      stages,
      counters,
      maxCounters,
      dimensions,
    });
  }
}
