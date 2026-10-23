// #377: single source for the "usage statistics" (PostHog) opt-out, shared by
// the desktop app (Settings copy) and the web client (copy + stored choice).
// Default ON with opt-out, matching the Sentry crash-report toggle. Desktop
// keeps its value in the session store and mirrors it to Rust
// (`set_usage_stats_enabled`); web persists it under USAGE_STATS_STORAGE_KEY.

export const USAGE_STATS_LABEL = 'Share anonymous usage and performance statistics';

export const USAGE_STATS_DESCRIPTION =
  'Counts and timings such as meetings joined and connection speed. Never names, rooms, window titles, or what you type.';

export const USAGE_STATS_STORAGE_KEY = 'petal-usage-stats';

export type UsageStatsStorage = Pick<Storage, 'getItem' | 'setItem'>;

// Only set when storage refused the write (private mode, blocked storage),
// so an opt-out still holds for the rest of this page.
let unpersistedChoice: boolean | null = null;

function defaultStorage(): UsageStatsStorage | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage;
  } catch {
    return null;
  }
}

/**
 * The one consent check for web usage telemetry (#377). Only an explicit
 * opt-out disables it; with nothing stored the default is ON. Producers (e.g. the #370 latency aggregator) call this before
 * doing any work, and `analytics.ts` `send()` calls it before every request.
 */
export function usageStatsEnabled(storage: UsageStatsStorage | null = defaultStorage()): boolean {
  let stored: string | null = null;
  try {
    stored = storage?.getItem(USAGE_STATS_STORAGE_KEY) ?? null;
  } catch {
    stored = null;
  }
  if (stored === null) return unpersistedChoice ?? true;
  return stored !== '0';
}

export function setUsageStatsEnabled(
  enabled: boolean,
  storage: UsageStatsStorage | null = defaultStorage()
): void {
  try {
    if (!storage) throw new Error('no storage');
    storage.setItem(USAGE_STATS_STORAGE_KEY, enabled ? '1' : '0');
    unpersistedChoice = null;
  } catch {
    unpersistedChoice = enabled;
  }
}
