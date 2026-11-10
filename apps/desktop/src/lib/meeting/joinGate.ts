type Phase = 'connecting' | 'connected' | 'disconnected';

/** Where the meeting stands after a control was refused as "not in room". */
export type RefusalPhase =
  /** It (re)connected after the refusal: worth trying again. */
  | 'joined'
  /** It stayed connected for the whole settle window: the refusal is real. */
  | 'held'
  /** It ended up disconnected (or the route went away). */
  | 'disconnected';

/**
 * Lets a control used while the meeting is still connecting (first join or a
 * reconnect) wait for the outcome instead of failing. Driven by the live
 * meeting phase, so it stays correct across retry-join and connection
 * recovery: connected → true, disconnected (or disposed) → false.
 */
export function createJoinGate(initial: Phase = 'connecting') {
  let phase: Phase = initial;
  let disposed = false;
  let waiters: Array<(joined: boolean) => void> = [];
  // `afterRefusal` callers waiting for a "connected" phase to move.
  let movers: Array<() => void> = [];

  function flush(joined: boolean) {
    const ready = waiters;
    waiters = [];
    for (const resolve of ready) resolve(joined);
  }

  function wakeMovers() {
    const ready = movers;
    movers = [];
    for (const wake of ready) wake();
  }

  function whenJoined(): Promise<boolean> {
    if (disposed || phase === 'disconnected') return Promise.resolve(false);
    if (phase === 'connected') return Promise.resolve(true);
    return new Promise((resolve) => waiters.push(resolve));
  }

  /**
   * The caller was just refused as if not in a room. Native drops the room
   * before room-left reaches this route, so a "connected" phase may be stale:
   * wait up to `settleMs` for it to move and then for the outcome, rather
   * than trusting it straight away (PR #503).
   */
  function afterRefusal(settleMs: number): Promise<RefusalPhase> {
    const outcome = (joined: boolean): RefusalPhase => (joined ? 'joined' : 'disconnected');
    if (phase !== 'connected' || disposed) return whenJoined().then(outcome);
    return new Promise((resolve) => {
      const wake = () => {
        clearTimeout(timer);
        resolve(whenJoined().then(outcome));
      };
      const timer = setTimeout(() => {
        movers = movers.filter((w) => w !== wake);
        resolve(phase === 'connected' && !disposed ? 'held' : whenJoined().then(outcome));
      }, settleMs);
      movers.push(wake);
    });
  }

  return {
    update(next: Phase) {
      const moved = next !== phase;
      phase = next;
      if (next === 'connected') flush(true);
      else if (next === 'disconnected') flush(false);
      if (moved) wakeMovers();
    },
    whenJoined,
    afterRefusal,
    dispose() {
      disposed = true;
      flush(false);
      wakeMovers();
    }
  };
}
