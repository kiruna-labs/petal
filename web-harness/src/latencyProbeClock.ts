// Per-peer clock offset from `petal.latency-probe` pongs (#375). Mirrors the
// native estimator (latency_probe.rs `estimate_peer_to_local_clock_offset_us`
// and rc_input_timing.rs `ClockWindows`): NTP-style offset per answer, the
// lowest-RTT answer among the last 16 that are at most 10 s old, and an error
// bound of half that answer's round trip.

export const CLOCK_WINDOW_SAMPLES = 16;
const CLOCK_WINDOW_RETAIN_MS = 60_000;
export const CLOCK_OFFSET_STALE_MS = 10_000;

/** `offsetMs` = local clock minus peer clock: `local = peer + offsetMs`. */
export type ClockEstimate = { offsetMs: number; errMs: number };

type ClockSample = { offsetMs: number; rttMs: number; atMs: number };

/** Local-minus-peer offset from one ping/pong exchange, or null for an impossible ordering. */
export function estimatePeerToLocalOffsetMs(
  localSendMs: number,
  peerReceiveMs: number,
  peerSendMs: number,
  localReceiveMs: number
): number | null {
  if (localReceiveMs < localSendMs || peerSendMs < peerReceiveMs) return null;
  const peerMinusLocal = (peerReceiveMs - localSendMs + (peerSendMs - localReceiveMs)) / 2;
  return -peerMinusLocal;
}

export class ClockWindows {
  private readonly peers = new Map<string, ClockSample[]>();

  record(peer: string, offsetMs: number, rttMs: number, nowMs: number): void {
    if (!Number.isFinite(offsetMs) || !Number.isFinite(rttMs) || rttMs < 0) return;
    for (const [key, window] of this.peers) {
      const newest = window[window.length - 1];
      if (!newest || nowMs - newest.atMs > CLOCK_WINDOW_RETAIN_MS) this.peers.delete(key);
    }
    let window = this.peers.get(peer);
    if (!window) {
      window = [];
      this.peers.set(peer, window);
    }
    if (window.length >= CLOCK_WINDOW_SAMPLES) window.shift();
    window.push({ offsetMs, rttMs, atMs: nowMs });
  }

  /** Lowest-RTT fresh answer; null when every answer is stale (offsets drift). */
  estimate(peer: string, nowMs: number): ClockEstimate | null {
    let best: ClockSample | null = null;
    for (const sample of this.peers.get(peer) ?? []) {
      if (nowMs - sample.atMs > CLOCK_OFFSET_STALE_MS) continue;
      if (!best || sample.rttMs < best.rttMs) best = sample;
    }
    return best ? { offsetMs: best.offsetMs, errMs: best.rttMs / 2 } : null;
  }

  clear(): void {
    this.peers.clear();
  }
}
