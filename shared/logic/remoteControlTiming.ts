// #371: controller-side remote-control input timing, shared by the desktop
// control overlay (apps/desktop/src/routes/compositor/control/+page.svelte)
// and the web client (web-harness/src/remoteControlUi.ts + harnessApi.ts).
//
// `sentAtMs` is the controller's wall clock when an input was PRODUCED. Stamp
// it before any send queue (#425's ordered sender) so queue wait stays in the
// measurement; a message that already carries a stamp keeps it.

export function stampSentAtMs<T extends object>(
  message: T & { sentAtMs?: number },
  nowMs: number = Date.now()
): T & { sentAtMs: number } {
  if (Number.isSafeInteger(message.sentAtMs) && (message.sentAtMs as number) >= 0) {
    return message as T & { sentAtMs: number };
  }
  return { ...message, sentAtMs: Math.floor(nowMs) };
}

/** A wire value usable as integer ms, else undefined (newer/odd peers). */
export function wireMs(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : undefined;
}

/**
 * `input_rtt_ms = (resultRecv - sentAt) - hostHoldMs` for a paired v2
 * discrete result. Undefined without `hostHoldMs` (an old host: its round
 * trip would include host replay time) or when the result is negative.
 * Mirrors `rc_input_timing::input_rtt_ms` in the native app.
 */
export function remoteControlInputRttMs(
  sentAtMs: number | undefined,
  resultRecvMs: number,
  hostHoldMs: number | undefined
): number | undefined {
  const sent = wireMs(sentAtMs);
  const hold = wireMs(hostHoldMs);
  if (sent === undefined || hold === undefined) return undefined;
  const rtt = Math.floor(resultRecvMs) - sent - hold;
  return rtt >= 0 ? rtt : undefined;
}

/**
 * #376: the native host replays at most this many Unicode scalar values per
 * `text` packet (Rust `MAX_REPLAY_TEXT_CHARS`); `remote_control_send` splits a
 * longer one into chunks numbered `seq, seq+1, ...`.
 */
export const REMOTE_TEXT_CHUNK_CHARS = 1000;

/** How many seqs `remote_control_send` uses for `text` (an empty text is one packet). */
export function remoteTextChunkCount(text: string): number {
  return Math.max(1, Math.ceil(Array.from(text).length / REMOTE_TEXT_CHUNK_CHARS));
}

/**
 * Reserve `count` consecutive seqs after `last` so no later input reuses a
 * chunk's seq (a reused seq would let the host's `inputFrame` for a chunk
 * resolve that input). Wraps to 1 like the per-input counter.
 */
export function reserveSeqs(last: number, count: number): { first: number; last: number } {
  const n = Math.max(1, Math.floor(count));
  const first = last + n > Number.MAX_SAFE_INTEGER ? 1 : last + 1;
  return { first, last: first + n - 1 };
}
