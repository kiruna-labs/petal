// Fixed log-spaced latency histogram (#370/#375). Rust mirror:
// apps/desktop/src-tauri/src/rc_latency_telemetry.rs. Both sides must pass
// contracts/petal-contracts.json `latencyHistogramVectors`; change the bucket
// layout or rounding on both sides together.
//
// Buckets: [0, 1), then edges 1, 1.05, 1.05^2, ... up to 5000 ms. Edges are
// built by repeated `*= 1.05` (correctly rounded IEEE in both languages), so
// the layout is bit-identical to Rust. Values above 5000 clamp; negative and
// non-finite values are rejected and counted. Percentiles are nearest-rank
// `ceil(n*p) - 1`, reported as the bucket's geometric midpoint clamped to the
// observed min/max and rounded.

export const LATENCY_HISTOGRAM_MAX_MS = 5000;
const GROWTH = 1.05;

export type LatencyHistogramStats = {
  p50: number;
  p95: number;
  max: number;
  n: number;
};

let cachedEdges: readonly number[] | null = null;

/** Upper edges of every bucket; bucket `j` is `[edges[j-1], edges[j])` with `edges[-1] = 0`. */
export function latencyHistogramEdges(): readonly number[] {
  if (cachedEdges) return cachedEdges;
  const edges: number[] = [];
  let edge = 1;
  while (edge < LATENCY_HISTOGRAM_MAX_MS) {
    edges.push(edge);
    edge *= GROWTH;
  }
  edges.push(LATENCY_HISTOGRAM_MAX_MS);
  cachedEdges = edges;
  return edges;
}

/** Index of the first edge strictly greater than `ms` (Rust `partition_point`). */
function bucketIndex(ms: number): number {
  const edges = latencyHistogramEdges();
  let lo = 0;
  let hi = edges.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (edges[mid] <= ms) lo = mid + 1;
    else hi = mid;
  }
  return Math.min(lo, edges.length - 1);
}

function bucketRepresentative(index: number): number {
  const edges = latencyHistogramEdges();
  return index === 0 ? 0.5 : Math.sqrt(edges[index - 1] * edges[index]);
}

export class LatencyHistogram {
  private readonly counts = new Uint32Array(latencyHistogramEdges().length);
  private total = 0;
  private rejectedCount = 0;
  private minMs = Infinity;
  private maxMs = -Infinity;

  /** False (counted as rejected) for a negative or non-finite value. */
  record(ms: number): boolean {
    if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) {
      this.rejectedCount += 1;
      return false;
    }
    const clamped = Math.min(ms, LATENCY_HISTOGRAM_MAX_MS);
    this.counts[bucketIndex(clamped)] += 1;
    this.total += 1;
    this.minMs = Math.min(this.minMs, clamped);
    this.maxMs = Math.max(this.maxMs, clamped);
    return true;
  }

  get n(): number {
    return this.total;
  }

  get rejected(): number {
    return this.rejectedCount;
  }

  percentile(fraction: number): number | null {
    if (this.total === 0) return null;
    const rank = Math.max(0, Math.ceil(this.total * fraction) - 1);
    let cumulative = 0;
    for (let index = 0; index < this.counts.length; index += 1) {
      cumulative += this.counts[index];
      if (cumulative > rank) {
        return Math.round(Math.min(Math.max(bucketRepresentative(index), this.minMs), this.maxMs));
      }
    }
    return Math.round(this.maxMs);
  }

  max(): number | null {
    return this.total === 0 ? null : Math.round(this.maxMs);
  }

  stats(): LatencyHistogramStats | null {
    const p50 = this.percentile(0.5);
    const p95 = this.percentile(0.95);
    const max = this.max();
    if (p50 === null || p95 === null || max === null) return null;
    return { p50, p95, max, n: this.total };
  }

  reset(): void {
    this.counts.fill(0);
    this.total = 0;
    this.rejectedCount = 0;
    this.minMs = Infinity;
    this.maxMs = -Infinity;
  }
}
