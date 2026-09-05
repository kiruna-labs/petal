// Rate-limit bucket storage.
//
// SEAM FOR A SHARED STORE. Vercel runs this backend as N independent warm
// instances, and `MemoryRateLimitStore` below is per-instance: every bucket
// cap in lib/handlers.ts is therefore a per-instance cap, not a global one.
// A caller spread across K instances gets roughly K× the stated capacity.
// That is acceptable as burst protection but it is NOT a hard quota. To make
// the limits global, implement `RateLimitStore` over a shared store (Vercel
// KV / Upstash Redis — any store with get/set/TTL) and hand it to
// `configureRateLimitStores` at module init. The interface is async-capable
// for exactly that reason; the in-memory implementation just happens to be
// synchronous.
//
// Why the memory store must bound itself: the previous implementation was a
// bare `Map<string, RateBucket>` with NO eviction, so every distinct
// x-forwarded-for ever seen by an instance stayed resident until the instance
// was recycled — an unauthenticated caller rotating source addresses could
// grow it without limit.

export interface RateBucket {
  tokens: number;
  updatedAt: number;
}

type MaybePromise<T> = T | Promise<T>;

export interface RateLimitStore {
  get(key: string): MaybePromise<RateBucket | undefined>;
  // `nowMs` is the logical clock the bucket was computed against; a TTL-based
  // backend uses it to expire the entry once it would be fully refilled.
  set(key: string, bucket: RateBucket, nowMs: number): MaybePromise<void>;
  clear(): MaybePromise<void>;
}

export interface MemoryRateLimitStoreOptions {
  // Entries older than this are dropped: a bucket untouched for a full refill
  // period is indistinguishable from an absent one, so the TTL is the refill
  // window of the limit it backs.
  ttlMs: number;
  // Hard ceiling on resident keys. Oldest-touched keys are evicted first.
  maxKeys?: number;
  // How many `set`s between full expiry sweeps (a cap breach sweeps
  // immediately regardless).
  sweepEvery?: number;
}

export const DEFAULT_RATE_LIMIT_MAX_KEYS = 10_000;
const DEFAULT_SWEEP_EVERY = 256;

export class MemoryRateLimitStore implements RateLimitStore {
  private readonly buckets = new Map<string, RateBucket>();
  private readonly ttlMs: number;
  private readonly maxKeys: number;
  private readonly sweepEvery: number;
  private setsSinceSweep = 0;

  constructor(options: MemoryRateLimitStoreOptions) {
    if (!(options.ttlMs > 0)) throw new Error('ttlMs must be positive');
    this.ttlMs = options.ttlMs;
    this.maxKeys = options.maxKeys ?? DEFAULT_RATE_LIMIT_MAX_KEYS;
    this.sweepEvery = options.sweepEvery ?? DEFAULT_SWEEP_EVERY;
    if (!(this.maxKeys > 0)) throw new Error('maxKeys must be positive');
  }

  get(key: string): RateBucket | undefined {
    return this.buckets.get(key);
  }

  set(key: string, bucket: RateBucket, nowMs: number): void {
    // Delete-then-set moves the key to the end of the Map's insertion order,
    // so the front of the Map is always the least-recently-touched key and
    // cap eviction below is LRU without a second index.
    this.buckets.delete(key);
    this.buckets.set(key, bucket);
    this.setsSinceSweep++;
    if (this.setsSinceSweep >= this.sweepEvery || this.buckets.size > this.maxKeys) {
      this.sweep(nowMs);
    }
  }

  clear(): void {
    this.buckets.clear();
    this.setsSinceSweep = 0;
  }

  get size(): number {
    return this.buckets.size;
  }

  // Drop every expired entry, then enforce the key cap oldest-first.
  sweep(nowMs: number): void {
    this.setsSinceSweep = 0;
    for (const [key, bucket] of this.buckets) {
      if (nowMs - bucket.updatedAt >= this.ttlMs) this.buckets.delete(key);
    }
    if (this.buckets.size <= this.maxKeys) return;
    const excess = this.buckets.size - this.maxKeys;
    let dropped = 0;
    for (const key of this.buckets.keys()) {
      if (dropped >= excess) break;
      this.buckets.delete(key);
      dropped++;
    }
  }
}
