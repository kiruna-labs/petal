// Background update-availability scheduling (#90).
//
// Before this, the only passive update check ran once per process at launch
// (`run_launch_update_check`) plus one throttled check whenever the main menu
// route mounted. A Petal left running for days therefore never learned about a
// release -- the reporter's exact symptom: "doesn't seem to check for updates
// unless you restart it".
//
// This module owns WHEN a background check happens. It deliberately knows
// nothing about Tauri, Svelte or the updater itself: the caller supplies
// `runCheck` (which performs the real availability check) and `isBusy` (which
// reports whether update UI would land on top of something the user cares
// about, i.e. a live meeting), so the scheduling policy is exercisable by a
// plain unit test with fake timers instead of only through a live app.
//
// Policy, and why:
//   - A 6-hour interval. Releases are cut a few times a week at most, so an
//     hours-scale interval is the right order of magnitude; 6h means a machine
//     left running through a workday learns about a release the same day while
//     costing at most 4 requests/device/day against `/api/updater` (a Vercel
//     function behind a CDN). Minutes-scale polling would be pure endpoint
//     load for no user-visible benefit.
//   - Events beat the timer. A laptop that slept for two days should check on
//     wake, not wait out the remainder of an interval, and a machine that just
//     regained connectivity should check then rather than 6 hours later.
//     Sleep/suspend is detected as WALL-CLOCK DRIFT on the heartbeat tick (the
//     webview's timers do not run while the machine is asleep), which needs no
//     platform API and works on both macOS and Windows.
//   - Never during a meeting. `isBusy()` defers instead of dropping: the
//     deferred check is retried on each heartbeat and fires once the meeting
//     ends.
//   - A minimum spacing floor applies to the event-driven triggers so a
//     flapping network (or an aggressively throttled background webview
//     producing false drift) can never hammer the endpoint.

/** Why a background check fired. Manual/launch/main-menu checks do not come
 *  through this scheduler and are unaffected by it. */
export type BackgroundCheckReason = 'periodic' | 'wake' | 'network';

/** Every reason `checkForUpdate()` accepts. */
export type UpdateCheckReason = 'launch' | 'main-menu' | 'manual' | BackgroundCheckReason;

/**
 * What the caller's `runCheck` did. `deferred` means the check did NOT happen
 * (the caller found a live meeting after all) and must be retried; the
 * scheduler keeps it pending rather than waiting out another full interval.
 */
export type BackgroundCheckOutcome = 'checked' | 'deferred';

/** 6 hours -- see the policy note above. */
export const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

/** Heartbeat: how often the scheduler re-evaluates. Cheap (no I/O unless a
 *  check is actually due) and doubles as the sleep/suspend detector. */
export const UPDATE_SCHEDULER_TICK_MS = 60 * 1000;

/** Wall-clock drift on one heartbeat above which the machine is assumed to
 *  have been asleep/suspended rather than merely busy. */
export const UPDATE_WAKE_GAP_MS = 10 * 60 * 1000;

/** Floor between event-driven checks, mirroring the root layout's existing
 *  30-minute main-menu throttle. Endpoint protection, not policy. */
export const UPDATE_CHECK_MIN_SPACING_MS = 30 * 60 * 1000;

const QUIET_REASONS: ReadonlySet<string> = new Set<BackgroundCheckReason>([
  'periodic',
  'wake',
  'network'
]);

/**
 * True for checks the user did not ask for. Those must not paint the
 * "Updating Petal…" toast and must not nag with a failure toast when the
 * machine is simply offline; an explicit Settings/main-menu check still
 * reports both. Lives here (not in `updater.ts`) so it is importable by tests
 * without pulling in the Tauri bridge.
 */
export function isQuietUpdateCheckReason(reason: UpdateCheckReason | undefined): boolean {
  return reason !== undefined && QUIET_REASONS.has(reason);
}

export interface UpdateSchedulerOptions {
  /** Perform the real availability check. May be sync (tests) or async. */
  runCheck: (reason: BackgroundCheckReason) => BackgroundCheckOutcome | Promise<BackgroundCheckOutcome>;
  /** True while update UI must not be surfaced (a live meeting). */
  isBusy: () => boolean;
  intervalMs?: number;
  tickMs?: number;
  wakeGapMs?: number;
  minSpacingMs?: number;
  /** Test seam only: the scheduler assumes a check just ran at start (the
   *  launch check), so the first periodic check is one interval out. */
  lastCheckAt?: number;
}

export interface UpdateScheduler {
  /** Stop the heartbeat and drop the network listener. Idempotent. */
  stop(): void;
  /** External "the machine woke" signal. */
  notifyWake(): void;
  /** External "the network changed / reconnected" signal. */
  notifyNetworkChange(): void;
  /** Re-evaluate now (e.g. a meeting just ended). */
  pump(): void;
}

type ListenerHost = {
  addEventListener(type: string, handler: () => void): void;
  removeEventListener(type: string, handler: () => void): void;
};

function listenerHost(): ListenerHost | null {
  const host = (globalThis as { window?: unknown }).window as ListenerHost | undefined;
  if (!host || typeof host.addEventListener !== 'function') return null;
  return host;
}

export function startUpdateScheduler(opts: UpdateSchedulerOptions): UpdateScheduler {
  const intervalMs = opts.intervalMs ?? UPDATE_CHECK_INTERVAL_MS;
  const tickMs = opts.tickMs ?? UPDATE_SCHEDULER_TICK_MS;
  const wakeGapMs = opts.wakeGapMs ?? UPDATE_WAKE_GAP_MS;
  const minSpacingMs = opts.minSpacingMs ?? UPDATE_CHECK_MIN_SPACING_MS;

  let lastCheckAt = opts.lastCheckAt ?? Date.now();
  let expectedTickAt = Date.now() + tickMs;
  let deferredReason: BackgroundCheckReason | null = null;
  let inFlight = false;
  let stopped = false;

  function settle(reason: BackgroundCheckReason, outcome: BackgroundCheckOutcome) {
    inFlight = false;
    if (outcome === 'deferred') {
      // Keep it pending: the next heartbeat retries, so a check missed
      // because of a meeting fires when the meeting ends, not 6h later.
      deferredReason = deferredReason ?? reason;
      return;
    }
    deferredReason = null;
    lastCheckAt = Date.now();
  }

  function request(reason: BackgroundCheckReason) {
    if (stopped || inFlight) return;
    // Endpoint floor for the event-driven triggers only; the periodic trigger
    // is already spaced by its own interval.
    if (reason !== 'periodic' && Date.now() - lastCheckAt < minSpacingMs) return;
    if (opts.isBusy()) {
      deferredReason = deferredReason ?? reason;
      return;
    }
    inFlight = true;
    let outcome: BackgroundCheckOutcome | Promise<BackgroundCheckOutcome>;
    try {
      outcome = opts.runCheck(reason);
    } catch {
      // A throwing check still counts as an attempt -- otherwise a
      // consistently failing check would retry every heartbeat.
      inFlight = false;
      lastCheckAt = Date.now();
      return;
    }
    if (outcome && typeof (outcome as Promise<BackgroundCheckOutcome>).then === 'function') {
      void (outcome as Promise<BackgroundCheckOutcome>).then(
        (settled) => settle(reason, settled),
        () => {
          inFlight = false;
          lastCheckAt = Date.now();
        }
      );
    } else {
      settle(reason, outcome as BackgroundCheckOutcome);
    }
  }

  function pump() {
    if (stopped) return;
    if (deferredReason) request(deferredReason);
  }

  function tick() {
    if (stopped) return;
    const now = Date.now();
    const drift = now - expectedTickAt;
    expectedTickAt = now + tickMs;
    // Timers do not run while the machine sleeps, so a heartbeat that arrives
    // far later than scheduled is the wake signal -- no platform API needed.
    if (drift >= wakeGapMs) {
      request('wake');
      return;
    }
    if (deferredReason) {
      request(deferredReason);
      return;
    }
    if (now - lastCheckAt >= intervalMs) request('periodic');
  }

  const timer = setInterval(tick, tickMs);
  // Node/Tauri parity: never let the heartbeat hold a process alive.
  (timer as unknown as { unref?: () => void }).unref?.();

  const host = listenerHost();
  const onOnline = () => request('network');
  host?.addEventListener('online', onOnline);

  return {
    stop() {
      if (stopped) return;
      stopped = true;
      clearInterval(timer);
      host?.removeEventListener('online', onOnline);
    },
    notifyWake() {
      request('wake');
    },
    notifyNetworkChange() {
      request('network');
    },
    pump
  };
}
