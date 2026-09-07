// Per-plugin quotas. The same numbers are enforced OUTBOUND here (in the
// broker, before anything reaches LiveKit) and INBOUND in both data
// dispatchers (Rust plugins::bus and web dataTopics.ts) so a misbehaving
// peer cannot flood a plugin either. Pinned in contracts as `pluginLimits`
// (M2). Design: plugins/README.md §2.6 and §5.

export const PLUGIN_LIMITS = {
  /** LiveKit drops lossy packets above ~15 KB; stay under with headroom. */
  maxPayloadBytes: 16384,
  lossyPerSecond: 30,
  reliablePerSecond: 10,
  inboundPerSenderPerSecond: 60,
  statePerSecond: 2,
  stateMaxBytes: 2048,
  stateTotalMaxBytes: 8192,
  storageWritesPerSecond: 20,
  storageValueMaxBytes: 65536,
  toastPerSecond: 0.5,
  toastMaxChars: 80,
  netFetchPerSecond: 5,
  netResponseMaxBytes: 1024 * 1024,
  logPerSecond: 20,
  uiPerSecond: 30,
} as const;

export interface RateLimiter {
  /** Take one token for `key`; false when the bucket is empty. */
  tryTake(key: string, now?: number): boolean;
  reset(key?: string): void;
}

export interface RateLimiterOptions {
  perSecond: number;
  /** Bucket capacity; defaults to one second of tokens, minimum 1. */
  burst?: number;
  now?: () => number;
}

/** Token bucket keyed by an arbitrary string, e.g. `${pluginId}:${method}`. */
export function createRateLimiter({ perSecond, burst, now = () => Date.now() }: RateLimiterOptions): RateLimiter {
  if (!(perSecond > 0)) throw new Error('perSecond must be > 0');
  const capacity = Math.max(1, burst ?? Math.ceil(perSecond));
  const buckets = new Map<string, { tokens: number; at: number }>();
  return {
    tryTake(key, at = now()) {
      let bucket = buckets.get(key);
      if (!bucket) {
        bucket = { tokens: capacity, at };
        buckets.set(key, bucket);
      } else if (at > bucket.at) {
        bucket.tokens = Math.min(capacity, bucket.tokens + ((at - bucket.at) / 1000) * perSecond);
        bucket.at = at;
      }
      if (bucket.tokens < 1) return false;
      bucket.tokens -= 1;
      return true;
    },
    reset(key) {
      if (key === undefined) buckets.clear();
      else buckets.delete(key);
    },
  };
}

/** UTF-8 byte length of a JSON-serialisable value; Infinity if unserialisable. */
export function jsonByteLength(value: unknown): number {
  try {
    const text = JSON.stringify(value);
    if (text === undefined) return 0;
    return new TextEncoder().encode(text).length;
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}
