import type { AutotestJoinResult } from '$lib/ipc';

export function autotestMeetingRoute(roomName: string): string {
  return `/meeting/${encodeURIComponent(roomName)}`;
}

type Subscribe<T> = (handler: (result: T) => void) => Promise<() => void>;

/** Accept only the first terminal result: the event and one-shot pull race. */
export function onceAutotestJoinResult(
  onResult: (result: AutotestJoinResult) => void
): (result: AutotestJoinResult) => boolean {
  let handled = false;
  return (result) => {
    if (handled) return false;
    handled = true;
    onResult(result);
    return true;
  };
}

/** Only a successful replay navigates away from `/main`; failures keep setup alive. */
export function shouldExitMainInitialization(result: AutotestJoinResult | null): boolean {
  return result?.status === 'joined';
}

/**
 * Registers a terminal-result listener without leaking it if the route is
 * destroyed while the asynchronous bridge subscription is still resolving.
 */
export async function subscribeToAutotestJoinResult(
  subscribe: Subscribe<AutotestJoinResult>,
  onResult: (result: AutotestJoinResult) => void,
  isRouteActive: () => boolean,
  setUnlisten: (unlisten: (() => void) | undefined) => void
): Promise<void> {
  const unlisten = await subscribe(onResult);
  if (!isRouteActive()) {
    unlisten();
    return;
  }
  setUnlisten(unlisten);
}

/** Replays a terminal result recorded before the frontend listener mounted. */
export async function replayAutotestJoinResult(
  read: () => Promise<AutotestJoinResult | null>,
  onResult: (result: AutotestJoinResult) => void,
  isRouteActive: () => boolean
): Promise<AutotestJoinResult | null> {
  const result = await read();
  if (!isRouteActive() || !result) return null;
  onResult(result);
  return result;
}
