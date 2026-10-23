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
export const MAX_BUNDLE_INPUTS = 4;
export const MAX_BUNDLE_PAYLOAD_BYTES = 1200;
export const MAX_REDUNDANT_AGE_MS = 500;
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

interface SentInput {
  rseq: number;
  sentAt: number;
  encodedLength: number;
  message: RemoteControlMessage;
}

const encoder = new TextEncoder();

/** One per (window, host) grant. rseq starts at 1 for each new stream. */
export class RedundantInputSender {
  private nextRseq = 1;
  private recent: SentInput[] = [];

  readonly windowId: number;
  readonly stream: string;

  constructor(windowId: number, stream: string) {
    this.windowId = windowId;
    this.stream = stream;
  }

  get latestRseq(): number | undefined {
    return this.recent.at(-1)?.rseq;
  }

  push(message: RemoteControlMessage, now: number): RemoteControlInputBundle {
    const rseq = this.nextRseq++;
    this.recent.push({ rseq, sentAt: now, encodedLength: encoder.encode(JSON.stringify(message)).length, message });
    if (this.recent.length > MAX_BUNDLE_INPUTS) this.recent.splice(0, this.recent.length - MAX_BUNDLE_INPUTS);
    return this.bundle(now)!;
  }

  /** The newest input plus as many contiguous older ones as fit the bounds. */
  bundle(now: number): RemoteControlInputBundle | null {
    const newest = this.recent.at(-1);
    if (!newest) return null;
    let taken = 1;
    let bytes = newest.encodedLength;
    for (let i = this.recent.length - 2; i >= 0; i -= 1) {
      const older = this.recent[i];
      if (
        taken >= MAX_BUNDLE_INPUTS ||
        now - older.sentAt > MAX_REDUNDANT_AGE_MS ||
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

/**
 * Publishes `message` through `sender` and schedules the trailing copies that
 * recover a lost final input. A newer push supersedes pending trailing copies
 * (its own bundle already carries the older inputs).
 */
export function publishRedundantInput(
  sender: RedundantInputSender,
  message: RemoteControlMessage,
  publish: (bundle: RemoteControlInputBundle) => Promise<void>,
  clock: {
    now: () => number;
    setTimeout: (fn: () => void, ms: number) => Timer;
  } = { now: () => Date.now(), setTimeout: (fn, ms) => setTimeout(fn, ms) }
): Promise<void> {
  const bundle = sender.push(message, clock.now());
  const rseq = sender.latestRseq;
  for (const delay of TRAILING_COPY_DELAYS_MS) {
    clock.setTimeout(() => {
      if (sender.latestRseq !== rseq) return;
      const trailing = sender.bundle(clock.now());
      if (trailing) void publish(trailing).catch(() => {});
    }, delay);
  }
  return publish(bundle);
}
