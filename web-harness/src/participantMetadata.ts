// The single owner of THIS peer's LiveKit participant metadata (#73).
//
// Participant metadata is ONE JSON blob, written whole. `livekit-client` only
// updates `localParticipant.metadata` when the SERVER echo of a write arrives,
// so every call site that read that field, merged its own key and wrote the
// blob back raced every other writer: two merges from the same stale base, and
// the later write silently dropped the earlier one's key. That is how the
// plugin advert kept vanishing behind the palette-index write at connect.
//
// So no call site reads `localParticipant.metadata` to write it any more.
// Writers hand a merge function to `update()`, which:
//   * merges against `desired` -- the last locally-known blob, updated
//     SYNCHRONOUSLY at write time, so two racing writers never share a base;
//   * remembers every top-level key it touched, and
//   * on a server echo that came back without one of those keys (another
//     whole-blob writer won), re-applies them -- bounded, so a key the server
//     will never accept cannot spin forever.
//
// The desktop client is not exposed to this: publisher.rs re-encodes from its
// authoritative in-process `share_metadata`, never from the server echo.

/** Thrown when a caller's merge function rejects the write (e.g. a size cap). */
export class MetadataMergeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'MetadataMergeError';
  }
}

/** The slice of `LocalParticipant` this owner needs (kept structural so tests can fake it). */
export interface MetadataWritable {
  metadata?: string;
  setMetadata?: (metadata: string) => Promise<void>;
}

/** Merge one key into the blob and return the whole blob. Must not mutate its argument. */
export type MetadataMerge = (current: string) => string;

export type MetadataLog = (line: string, kind?: 'info' | 'ok' | 'warn' | 'error') => void;

export interface MetadataOwnerOptions {
  /**
   * Upper bound on waiting for one write's server echo before the NEXT queued
   * write is published. Releasing early is safe for correctness -- the next
   * merge reads `desired`, not the echo -- it only bounds how long a slow or
   * superseded echo holds the queue. (livekit resolves `setMetadata` when the
   * echo matches what it sent, so a write superseded by a later local write
   * never resolves at all and would otherwise hold the queue for livekit's
   * full ~5 s timeout.)
   */
  echoWaitMs?: number;
  /** Max re-applies after echoes that dropped a key we own. */
  maxReapplyAttempts?: number;
  log?: MetadataLog;
}

export interface MetadataOwner {
  /** Take ownership of a local participant's metadata, seeding from what it already has. */
  attach(participant: MetadataWritable | null): void;
  detach(): void;
  setLogger(log: MetadataLog | undefined): void;
  /** The last locally-known blob: what every merge is applied to. */
  current(): string;
  /** Merge and publish. Resolves when the write went out (or its echo wait elapsed). */
  update(merge: MetadataMerge): Promise<void>;
  /**
   * Feed a server echo (`ParticipantMetadataChanged` for the local
   * participant, or the join response). Adopts keys we do not own and
   * re-applies the ones we do when the echo came back without them.
   */
  onEcho(metadata: string | null | undefined): void;
}

const DEFAULT_ECHO_WAIT_MS = 1000;
const DEFAULT_MAX_REAPPLY_ATTEMPTS = 5;

function parseObject(metadata: string | null | undefined): Record<string, unknown> {
  if (!metadata) return {};
  try {
    const parsed: unknown = JSON.parse(metadata);
    return typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

/** Key-order-independent JSON identity, so re-serialising a blob is not seen as a change. */
function canonical(value: unknown): string {
  if (value === undefined) return 'undefined';
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, v]) => v !== undefined)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(',')}}`;
  }
  return JSON.stringify(value) ?? 'null';
}

function sameJson(a: unknown, b: unknown): boolean {
  return canonical(a) === canonical(b);
}

export function createMetadataOwner(options: MetadataOwnerOptions = {}): MetadataOwner {
  const echoWaitMs = options.echoWaitMs ?? DEFAULT_ECHO_WAIT_MS;
  const maxReapplyAttempts = options.maxReapplyAttempts ?? DEFAULT_MAX_REAPPLY_ATTEMPTS;
  let log: MetadataLog | undefined = options.log;
  let participant: MetadataWritable | null = null;
  let desired = '{}';
  /** Top-level keys THIS peer has written, and therefore re-applies after a losing echo. */
  const owned = new Set<string>();
  let queue: Promise<void> = Promise.resolve();
  let reapplyAttempts = 0;
  let capReported = false;

  function reset(): void {
    desired = '{}';
    owned.clear();
    queue = Promise.resolve();
    reapplyAttempts = 0;
    capReported = false;
  }

  async function publish(): Promise<void> {
    const target = participant;
    const setter = target?.setMetadata;
    if (!target || typeof setter !== 'function') {
      log?.('participant metadata is not writable here; keeping the local state only', 'warn');
      return;
    }
    const blob = desired;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const bound = new Promise<void>((resolve) => {
      timer = setTimeout(resolve, echoWaitMs);
    });
    const write = setter.call(target, blob);
    // A rejection the bound already won must not surface as an unhandled one.
    write.catch(() => {});
    try {
      await Promise.race([write, bound]);
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  function enqueue(task: () => Promise<void>): Promise<void> {
    const next = queue.then(task, task);
    queue = next.catch(() => {});
    return next;
  }

  return {
    attach(next: MetadataWritable | null) {
      participant = next;
      reset();
      desired = JSON.stringify(parseObject(next?.metadata));
    },
    detach() {
      participant = null;
      reset();
    },
    setLogger(next: MetadataLog | undefined) {
      log = next;
    },
    current() {
      return desired;
    },
    update(merge: MetadataMerge): Promise<void> {
      let next: string;
      try {
        next = merge(desired);
      } catch (err) {
        return Promise.reject(new MetadataMergeError((err as Error)?.message ?? String(err)));
      }
      const before = parseObject(desired);
      const after = parseObject(next);
      for (const key of new Set([...Object.keys(before), ...Object.keys(after)])) {
        if (!sameJson(before[key], after[key])) owned.add(key);
      }
      // Synchronously visible to the next writer: this is what makes the
      // stale-base race impossible rather than merely convergent.
      desired = JSON.stringify(after);
      reapplyAttempts = 0;
      capReported = false;
      return enqueue(publish);
    },
    onEcho(metadata: string | null | undefined) {
      const echo = parseObject(metadata);
      const want = parseObject(desired);
      // The echo is authoritative for every key we never wrote (token
      // metadata, anything set server-side); our own keys stay ours.
      const reconciled: Record<string, unknown> = { ...echo };
      for (const key of owned) {
        if (key in want) reconciled[key] = want[key];
        else delete reconciled[key];
      }
      desired = JSON.stringify(reconciled);
      if (sameJson(reconciled, echo)) {
        reapplyAttempts = 0;
        capReported = false;
        return;
      }
      if (reapplyAttempts >= maxReapplyAttempts) {
        if (!capReported) {
          capReported = true;
          log?.(
            `participant metadata did not take after ${maxReapplyAttempts} re-applies; giving up until the next write`,
            'warn',
          );
        }
        return;
      }
      reapplyAttempts += 1;
      void enqueue(publish);
    },
  };
}

/** The process-wide owner every web writer goes through. */
export const localParticipantMetadata = createMetadataOwner();
