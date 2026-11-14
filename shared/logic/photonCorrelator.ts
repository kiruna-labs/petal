// #376: production press-to-photon, measured on the CONTROLLER's own clock.
// The controller remembers when it sent each discrete input (by `seq`), the
// host answers `inputFrame { seq, frameId }` naming the first published frame
// that changed after the inject, and the controller watches presented frames
// (web: rVFC + lookupFrameMetadata; native: the compositor hand-off).
// `photon_ms = present - sent`, both monotonic on this machine: no offset.
//
// Pure and shared by the web controller (web-harness/src/remoteControlUi.ts)
// and, through its Rust mirror `rc_photon.rs`, the native controller. Both
// are pinned by contracts/petal-contracts.json `photonCorrelatorVectors`.

/** Sent inputs remembered at once; the oldest is dropped beyond this. */
export const PHOTON_SENT_CAPACITY = 64;
/** An input the host never announced is forgotten after this. */
export const PHOTON_SENT_TTL_MS = 2000;
/** Presented frames kept for retroactive resolution (video can beat data). */
export const PHOTON_RING_MS = 2000;
export const PHOTON_RING_CAPACITY = 256;
/** An announced input not presented within this is `photon_timeout`, never a sample. */
export const PHOTON_TIMEOUT_MS = 1000;
/** A wheel after this much wheel silence starts a new burst (its first wheel counts). */
export const PHOTON_WHEEL_BURST_IDLE_MS = 500;
/** Host emission cap per controller (4/s). */
export const PHOTON_HOST_MIN_INTERVAL_MS = 250;

export type PhotonOutcome = { kind: 'photon'; seq: number; ms: number } | { kind: 'timeout'; seq: number };

export type PhotonInput = {
  kind: string;
  action?: string;
  repeat?: boolean;
};

/**
 * Discrete ops only: a press (pointer down/click), a non-repeat key down, a
 * text input, and the first wheel of a burst. Never a pointer move or up.
 * Stateful only for the wheel burst. Mirrors `rc_photon::PhotonInputFilter`.
 */
export class PhotonInputFilter {
  private lastWheelMs: number | null = null;

  admit(input: PhotonInput, nowMs: number): boolean {
    switch (input.kind) {
      case 'pointer':
        return input.action === 'down' || input.action === 'click';
      case 'key':
        return input.action === 'down' && !input.repeat;
      case 'text':
        return true;
      case 'wheel': {
        const first = this.lastWheelMs === null || nowMs - this.lastWheelMs >= PHOTON_WHEEL_BURST_IDLE_MS;
        this.lastWheelMs = nowMs;
        return first;
      }
      default:
        return false;
    }
  }
}

type SentEntry = { sentMs: number; frameId: number | null };

export class PhotonCorrelator {
  // Insertion order is send order (a Map keeps it).
  private sent = new Map<number, SentEntry>();
  private ring: Array<{ frameId: number; presentMs: number }> = [];
  private lastFrameId: number | null = null;

  /** A discrete input left this controller at `sentMs` (monotonic). */
  noteSent(seq: number, sentMs: number): PhotonOutcome[] {
    const out = this.expire(sentMs);
    if (this.sent.has(seq)) return out;
    if (this.sent.size >= PHOTON_SENT_CAPACITY) {
      const oldest = this.sent.keys().next().value as number;
      this.sent.delete(oldest);
    }
    this.sent.set(seq, { sentMs, frameId: null });
    return out;
  }

  /** A frame of the controlled window was presented at `presentMs`. */
  notePresented(frameId: number, presentMs: number): PhotonOutcome[] {
    const out = this.expire(presentMs);
    // Frame ids only grow within one publication; a drop means a new track.
    if (this.lastFrameId !== null && frameId < this.lastFrameId) this.reset();
    this.lastFrameId = frameId;
    this.ring.push({ frameId, presentMs });
    if (this.ring.length > PHOTON_RING_CAPACITY) this.ring.shift();
    for (const [seq, entry] of this.sent) {
      if (entry.frameId === null || frameId < entry.frameId) continue;
      this.sent.delete(seq);
      const outcome = resolve(seq, entry.sentMs, presentMs);
      if (outcome) out.push(outcome);
    }
    return out;
  }

  /** The host said input `seq` first shows in frame `frameId`. */
  noteInputFrame(seq: number, frameId: number, nowMs: number): PhotonOutcome[] {
    const out = this.expire(nowMs);
    const entry = this.sent.get(seq);
    // Unknown (never sent, expired, other track) or a duplicate announcement.
    if (!entry || entry.frameId !== null) return out;
    entry.frameId = frameId;
    const hit = this.ring.find((frame) => frame.frameId >= frameId && frame.presentMs >= entry.sentMs);
    if (hit) {
      this.sent.delete(seq);
      const outcome = resolve(seq, entry.sentMs, hit.presentMs);
      if (outcome) out.push(outcome);
    }
    return out;
  }

  /** Periodic tick: announced inputs past the timeout become `timeout`. */
  expire(nowMs: number): PhotonOutcome[] {
    const out: PhotonOutcome[] = [];
    for (const [seq, entry] of this.sent) {
      const age = nowMs - entry.sentMs;
      if (entry.frameId !== null && age > PHOTON_TIMEOUT_MS) {
        this.sent.delete(seq);
        out.push({ kind: 'timeout', seq });
      } else if (entry.frameId === null && age > PHOTON_SENT_TTL_MS) {
        this.sent.delete(seq);
      }
    }
    while (this.ring.length > 0 && this.ring[0].presentMs < nowMs - PHOTON_RING_MS) this.ring.shift();
    return out;
  }

  /** Track republished, share epoch changed, or control ended. */
  reset(): void {
    this.sent.clear();
    this.ring = [];
    this.lastFrameId = null;
  }

  pendingCount(): number {
    return this.sent.size;
  }
}

function resolve(seq: number, sentMs: number, presentMs: number): PhotonOutcome | null {
  const ms = presentMs - sentMs;
  if (!Number.isFinite(ms) || ms < 0) return null;
  return ms > PHOTON_TIMEOUT_MS ? { kind: 'timeout', seq } : { kind: 'photon', seq, ms };
}
