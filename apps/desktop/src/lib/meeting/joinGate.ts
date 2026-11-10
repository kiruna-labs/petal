type Phase = 'connecting' | 'connected' | 'disconnected';

/**
 * Lets a control used while the meeting is still connecting (first join or a
 * reconnect) wait for the outcome instead of failing. Driven by the live
 * meeting phase, so it stays correct across retry-join and connection
 * recovery: connected → true, disconnected (or disposed) → false.
 *
 * `settleMs`: the caller was just refused as if not connected. Native can drop
 * the room a moment before room-left reaches this route, so "connected" may
 * be stale: give the phase that long to move before trusting it (PR #503).
 */
export function createJoinGate(initial: Phase = 'connecting') {
  let phase: Phase = initial;
  let disposed = false;
  let waiters: Array<(joined: boolean) => void> = [];
  // Waiting out `settleMs` on a "connected" that may be stale.
  let settling: Array<() => void> = [];

  function flush(joined: boolean) {
    const ready = waiters;
    waiters = [];
    for (const resolve of ready) resolve(joined);
  }

  function settle() {
    const ready = settling;
    settling = [];
    for (const wake of ready) wake();
  }

  function whenJoined(opts: { settleMs?: number } = {}): Promise<boolean> {
    if (disposed || phase === 'disconnected') return Promise.resolve(false);
    if (phase === 'connected') {
      const settleMs = opts.settleMs ?? 0;
      if (settleMs <= 0) return Promise.resolve(true);
      return new Promise((resolve) => {
        const wake = () => {
          clearTimeout(timer);
          resolve(whenJoined());
        };
        const timer = setTimeout(() => {
          settling = settling.filter((w) => w !== wake);
          resolve(whenJoined());
        }, settleMs);
        settling.push(wake);
      });
    }
    return new Promise((resolve) => waiters.push(resolve));
  }

  return {
    update(next: Phase) {
      const moved = next !== phase;
      phase = next;
      if (next === 'connected') flush(true);
      else if (next === 'disconnected') flush(false);
      if (moved) settle();
    },
    whenJoined,
    dispose() {
      disposed = true;
      flush(false);
      settle();
    }
  };
}
