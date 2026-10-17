// Bounded automatic rejoin after the native room lost its connection (#354).
//
// LiveKit's SDK already retries a dropped connection ~10 times (about a
// minute). When it gives up, the native side tears the room down and emits
// `room-left { connectionLost: true }`. Before #354 the meeting route showed
// "Meeting ended" and dropped the user on the room list (PETAL-DESKTOP-2R).
// This controller runs a fresh join (new token + connect) with backoff.
// It is pure, and its seams are injected so tests drive the real policy.

/** Delay before each rejoin attempt. Each attempt is itself bounded by the
 * native join budget (45s), so the worst case is a few minutes, during which
 * the user can Leave at any time. */
export const REJOIN_BACKOFF_MS: readonly number[] = [1000, 3000, 8000, 15000];

export type RejoinOutcome =
  | { kind: 'rejoined'; attempts: number }
  | { kind: 'cancelled' }
  | { kind: 'failed'; attempts: number; lastError: unknown };

export interface RejoinSeams {
  /** One complete native join of the same room. Rejects on failure. */
  join: () => Promise<void>;
  /** Undo a join that completed after the user cancelled (Leave/unmount). */
  leave: () => Promise<void>;
  sleep: (ms: number) => Promise<void>;
  /** True once the user left or the route unmounted. */
  cancelled: () => boolean;
  onAttempt?: (attempt: number, total: number) => void;
  onAttemptFailed?: (attempt: number, total: number, error: unknown) => void;
}

export async function runBoundedRejoin(
  seams: RejoinSeams,
  delays: readonly number[] = REJOIN_BACKOFF_MS
): Promise<RejoinOutcome> {
  const total = delays.length;
  let lastError: unknown = null;
  for (let i = 0; i < total; i += 1) {
    if (seams.cancelled()) return { kind: 'cancelled' };
    await seams.sleep(delays[i]);
    if (seams.cancelled()) return { kind: 'cancelled' };
    const attempt = i + 1;
    seams.onAttempt?.(attempt, total);
    try {
      await seams.join();
    } catch (error) {
      lastError = error;
      seams.onAttemptFailed?.(attempt, total, error);
      continue;
    }
    if (seams.cancelled()) {
      // The user left while this join was in flight: don't strand them in a
      // room they asked to leave.
      await seams.leave().catch(() => {});
      return { kind: 'cancelled' };
    }
    return { kind: 'rejoined', attempts: attempt };
  }
  return { kind: 'failed', attempts: total, lastError };
}

/** Toast copy once the rejoin succeeded. Window shares are not restarted by a
 * rejoin (#170), so say so instead of letting them vanish silently. */
export function rejoinedMessage(stoppedShareCount: number): string {
  if (stoppedShareCount <= 0) return 'Reconnected';
  return stoppedShareCount === 1
    ? 'Reconnected — share your window again'
    : 'Reconnected — share your windows again';
}

/** Gallery-bridge restarts each mint a token against a per-IP bucket shared
 * with native rejoins, so they back off exponentially (2s -> 60s) and stop
 * after a cap until a connect succeeds again. */
export const GALLERY_BRIDGE_MAX_RESTARTS = 8;
export function galleryBridgeRestartDelayMs(restartsSoFar: number): number {
  return Math.min(2000 * 2 ** Math.max(0, restartsSoFar), 60000);
}
