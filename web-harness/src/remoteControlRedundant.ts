import type { RemoteControlMessage } from './trackNames.ts';

/**
 * #399: redundant lossy transport for remote-control input, controller side.
 * Mirrors apps/desktop/src-tauri/src/remote_control_redundant.rs; the wire
 * shape and constants are pinned by contracts/petal-contracts.json
 * (`remoteControlInputBundle`). Used only once the host advertised
 * `supportsRedundantInput` on its `active` status; otherwise input keeps the
 * reliable ordered path.
 */
export const INPUT_BUNDLE_KIND = 'inputBundle';
export const INPUT_BUNDLE_VERSION = 1;
export const MAX_REDUNDANT_COPIES = 3;
export const MAX_BUNDLE_INPUTS = 48;
export const MAX_BUNDLE_PAYLOAD_BYTES = 1200;
export const MAX_REDUNDANT_AGE_MS = 500;
/** One bundle per interval at most: a same-millisecond burst of one bundle per
 * input overflowed the lossy send buffer and LiveKit dropped the tail. */
export const MIN_BUNDLE_INTERVAL_MS = 8;
export const IMMEDIATE_COPIES = 2;
export const TRAILING_COPY_DELAYS_MS = [30, 100] as const;

export interface RemoteControlInputBundle {
  v: typeof INPUT_BUNDLE_VERSION;
  kind: typeof INPUT_BUNDLE_KIND;
  windowId: number;
  stream: string;
  first: number;
  inputs: RemoteControlMessage[];
}

/** Every input except a hover move (those stay lossy and latest-wins). */
export function ridesRedundantStream(message: RemoteControlMessage): boolean {
  if (message.kind === 'pointer') return !(message.action === 'move' && (message.buttons ?? 0) === 0);
  return message.kind === 'wheel' || message.kind === 'key' || message.kind === 'text';
}

interface QueuedInput {
  rseq: number;
  queuedAt: number;
  encodedLength: number;
  message: RemoteControlMessage;
}

export type PushOutcome =
  | { kind: 'publish'; bundle: RemoteControlInputBundle; flush: number }
  | { kind: 'flushAfter'; waitMs: number }
  | { kind: 'queued' };

const encoder = new TextEncoder();

/** One per (window, host) grant. rseq starts at 1 for each new stream. */
export class RedundantInputSender {
  readonly windowId: number;
  readonly stream: string;
  private nextRseq = 1;
  /** Published inputs kept for redundancy, then `unsent` new ones. */
  private recent: QueuedInput[] = [];
  private unsent = 0;
  private lastFlushAt: number | null = null;
  private flushScheduled = false;
  private lastFlushLength = 0;
  flushes = 0;

  constructor(windowId: number, stream: string) {
    this.windowId = windowId;
    this.stream = stream;
  }

  push(message: RemoteControlMessage, now: number): PushOutcome {
    const rseq = this.nextRseq++;
    this.recent.push({ rseq, queuedAt: now, encodedLength: encoder.encode(JSON.stringify(message)).length, message });
    this.unsent += 1;
    const waitMs = this.lastFlushAt === null ? 0 : Math.max(0, MIN_BUNDLE_INTERVAL_MS - (now - this.lastFlushAt));
    if (waitMs === 0 || this.unsent >= MAX_BUNDLE_INPUTS) {
      const bundle = this.flush(now)!;
      return { kind: 'publish', bundle, flush: this.flushes };
    }
    if (this.flushScheduled) return { kind: 'queued' };
    this.flushScheduled = true;
    return { kind: 'flushAfter', waitMs };
  }

  /** Bundle for every queued input; null when a forced flush already sent them. */
  flush(now: number): RemoteControlInputBundle | null {
    this.flushScheduled = false;
    if (this.unsent === 0) return null;
    const bundle = this.build(now, this.unsent);
    this.lastFlushLength = this.unsent;
    this.unsent = 0;
    this.flushes += 1;
    this.lastFlushAt = now;
    const keep = this.lastFlushLength + MAX_REDUNDANT_COPIES;
    if (this.recent.length > keep) this.recent.splice(0, this.recent.length - keep);
    return bundle;
  }

  /** Trailing copy of flush `flush`, unless a newer input superseded it. */
  trailing(flush: number, now: number): RemoteControlInputBundle | null {
    if (this.flushes !== flush || this.unsent !== 0 || this.recent.length === 0) return null;
    return this.build(now, this.lastFlushLength);
  }

  /** The `fresh` newest inputs plus as many contiguous older copies as fit. */
  private build(now: number, freshCount: number): RemoteControlInputBundle {
    const fresh = Math.min(Math.max(freshCount, 1), this.recent.length);
    let taken = fresh;
    let bytes = this.recent.slice(this.recent.length - fresh).reduce((sum, input) => sum + input.encodedLength, 0);
    for (let i = this.recent.length - fresh - 1; i >= 0; i -= 1) {
      const older = this.recent[i];
      if (
        taken - fresh >= MAX_REDUNDANT_COPIES ||
        now - older.queuedAt > MAX_REDUNDANT_AGE_MS ||
        bytes + older.encodedLength > MAX_BUNDLE_PAYLOAD_BYTES
      ) {
        break;
      }
      taken += 1;
      bytes += older.encodedLength;
    }
    const inputs = this.recent.slice(this.recent.length - taken);
    return {
      v: INPUT_BUNDLE_VERSION,
      kind: INPUT_BUNDLE_KIND,
      windowId: this.windowId,
      stream: this.stream,
      first: inputs[0].rseq,
      inputs: inputs.map((input) => input.message)
    };
  }
}

export function newRedundantStreamId(): string {
  const bytes = new Uint8Array(8);
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') crypto.getRandomValues(bytes);
  else for (let i = 0; i < bytes.length; i += 1) bytes[i] = Math.floor(Math.random() * 256);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

type Timer = ReturnType<typeof setTimeout>;
export interface RedundantClock {
  now: () => number;
  setTimeout: (fn: () => void, ms: number) => Timer;
}
const realClock: RedundantClock = { now: () => Date.now(), setTimeout: (fn, ms) => setTimeout(fn, ms) };

function publishFlushed(
  sender: RedundantInputSender,
  bundle: RemoteControlInputBundle,
  flush: number,
  publish: (bundle: RemoteControlInputBundle) => Promise<void>,
  clock: RedundantClock
): Promise<void> {
  const first = publish(bundle);
  for (let copy = 1; copy < IMMEDIATE_COPIES; copy += 1) void publish(bundle).catch(() => {});
  for (const delay of TRAILING_COPY_DELAYS_MS) {
    clock.setTimeout(() => {
      const trailing = sender.trailing(flush, clock.now());
      if (trailing) void publish(trailing).catch(() => {});
    }, delay);
  }
  return first;
}

/**
 * Queues `message` on `sender` and publishes per the pacing decision: now
 * (sent IMMEDIATE_COPIES times, then trailing copies), after a short paced
 * flush, or as part of a flush already scheduled.
 */
export function publishRedundantInput(
  sender: RedundantInputSender,
  message: RemoteControlMessage,
  publish: (bundle: RemoteControlInputBundle) => Promise<void>,
  clock: RedundantClock = realClock
): Promise<void> {
  const outcome = sender.push(message, clock.now());
  if (outcome.kind === 'publish') return publishFlushed(sender, outcome.bundle, outcome.flush, publish, clock);
  if (outcome.kind === 'flushAfter') {
    clock.setTimeout(() => {
      const bundle = sender.flush(clock.now());
      if (bundle) void publishFlushed(sender, bundle, sender.flushes, publish, clock).catch(() => {});
    }, outcome.waitMs);
  }
  return Promise.resolve();
}
