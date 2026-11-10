type Phase = 'connecting' | 'connected' | 'disconnected';

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

  function flush(joined: boolean) {
    const ready = waiters;
    waiters = [];
    for (const resolve of ready) resolve(joined);
  }

  function whenJoined(): Promise<boolean> {
    if (disposed || phase === 'disconnected') return Promise.resolve(false);
    if (phase === 'connected') return Promise.resolve(true);
    return new Promise((resolve) => waiters.push(resolve));
  }

  return {
    update(next: Phase) {
      phase = next;
      if (next === 'connected') flush(true);
      else if (next === 'disconnected') flush(false);
    },
    whenJoined,
    dispose() {
      disposed = true;
      flush(false);
    }
  };
}
