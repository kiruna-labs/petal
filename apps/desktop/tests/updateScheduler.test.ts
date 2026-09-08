// #90 -- "Petal doesn't seem to check for updates unless you restart it."
//
// These tests drive the REAL scheduler (`src/lib/updateScheduler.ts`, the same
// module +layout.svelte starts) on fake timers: a real `setInterval`, a real
// clock, the real drift/deferral/spacing logic. Per CLAUDE.md's rule that a
// unit test on an extracted helper proves nothing about the path that calls
// it, the source assertions at the bottom pin the wiring in the shipped root
// layout (which cannot be imported here -- it is a Svelte component using
// `$lib` aliases and the Tauri bridge).

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  isQuietUpdateCheckReason,
  startUpdateScheduler,
  UPDATE_CHECK_INTERVAL_MS,
  UPDATE_CHECK_MIN_SPACING_MS,
  UPDATE_SCHEDULER_TICK_MS,
  UPDATE_WAKE_GAP_MS,
  type BackgroundCheckOutcome,
  type BackgroundCheckReason
} from '../src/lib/updateScheduler.ts';

const layoutSource = readFileSync(new URL('../src/routes/+layout.svelte', import.meta.url), 'utf8');
const updaterSource = readFileSync(new URL('../src/lib/updater.ts', import.meta.url), 'utf8');
const settingsSource = readFileSync(
  new URL('../src/lib/components/Settings.svelte', import.meta.url),
  'utf8'
);

const START_TIME = 1_800_000_000_000;
const HOUR = 60 * 60 * 1000;

/**
 * Advance the fake clock the way real time passes: one heartbeat at a time.
 * `mock.timers.tick(N)` jumps `Date.now()` straight to the end of the window
 * and only then runs the timers it released, which the scheduler would
 * (correctly) read as a machine that had been asleep -- that is what the
 * dedicated sleep test below simulates on purpose.
 */
function advance(t: { mock: { timers: { tick(ms: number): void } } }, ms: number) {
  const steps = Math.floor(ms / UPDATE_SCHEDULER_TICK_MS);
  for (let i = 0; i < steps; i += 1) t.mock.timers.tick(UPDATE_SCHEDULER_TICK_MS);
  const remainder = ms - steps * UPDATE_SCHEDULER_TICK_MS;
  if (remainder > 0) t.mock.timers.tick(remainder);
}

/** A recorder for the checks the scheduler actually asks for. */
function recorder(outcome: BackgroundCheckOutcome = 'checked') {
  const reasons: BackgroundCheckReason[] = [];
  return {
    reasons,
    runCheck(reason: BackgroundCheckReason): BackgroundCheckOutcome {
      reasons.push(reason);
      return outcome;
    }
  };
}

// ---------------------------------------------------------------------------
// 1. The bug itself: time passing must produce a check, with no restart.
// ---------------------------------------------------------------------------

test('a Petal left running checks again on the interval, without a restart (#90)', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => false });
  t.after(() => scheduler.stop());

  // Just after launch: the launch check already ran, nothing is due.
  advance(t, 5 * HOUR);
  assert.deepEqual(rec.reasons, [], 'must not re-check before the interval elapses');

  advance(t, UPDATE_CHECK_INTERVAL_MS - 5 * HOUR + UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, ['periodic'], 'one check once the interval elapses');

  // ...and it keeps going for the life of the process, not just once.
  advance(t, UPDATE_CHECK_INTERVAL_MS + UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, ['periodic', 'periodic']);

  // Endpoint sanity: two days of uptime is a handful of requests, not a poll
  // storm (6h cadence -> 4/day).
  rec.reasons.length = 0;
  advance(t, 48 * HOUR);
  assert.equal(rec.reasons.length, 8, `expected 8 checks in 48h, got ${rec.reasons.length}`);
});

test('stop() ends the schedule', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => false });
  scheduler.stop();
  advance(t, 24 * HOUR);
  assert.deepEqual(rec.reasons, []);
});

// ---------------------------------------------------------------------------
// 2. Never during a meeting -- deferred, not dropped.
// ---------------------------------------------------------------------------

test('a due check is deferred while in a meeting and lands when it ends (#90)', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  let inMeeting = true;
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => inMeeting });
  t.after(() => scheduler.stop());

  // A long meeting spanning several intervals: no update UI, ever.
  advance(t, 2 * UPDATE_CHECK_INTERVAL_MS);
  assert.deepEqual(rec.reasons, [], 'an update prompt must never appear mid-meeting');

  // Meeting ends: the deferred check lands on the next heartbeat -- it does not
  // wait out another full interval.
  inMeeting = false;
  t.mock.timers.tick(UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, ['periodic']);
});

test('a check the caller defers (authoritative in-room state) is retried, not lost', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  // `isBusy` says idle, but the caller's own authoritative check (the layout's
  // `current_room` lookup) reports a live meeting and returns 'deferred'.
  const reasons: BackgroundCheckReason[] = [];
  let stillInRoom = true;
  const scheduler = startUpdateScheduler({
    runCheck: (reason) => {
      reasons.push(reason);
      return stillInRoom ? 'deferred' : 'checked';
    },
    isBusy: () => false
  });
  t.after(() => scheduler.stop());

  advance(t, UPDATE_CHECK_INTERVAL_MS);
  assert.equal(reasons.length, 1);
  // Deferred means "not checked": the next heartbeat retries rather than
  // waiting another 6 hours.
  advance(t, UPDATE_SCHEDULER_TICK_MS);
  assert.equal(reasons.length, 2);
  stillInRoom = false;
  advance(t, UPDATE_SCHEDULER_TICK_MS);
  assert.equal(reasons.length, 3);
  // Once it succeeds it stops retrying and the interval takes over again.
  advance(t, 10 * UPDATE_SCHEDULER_TICK_MS);
  assert.equal(reasons.length, 3);
});

test('an async runCheck is awaited before the schedule advances', async (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const reasons: BackgroundCheckReason[] = [];
  let resolve!: (outcome: BackgroundCheckOutcome) => void;
  const scheduler = startUpdateScheduler({
    runCheck: (reason) => {
      reasons.push(reason);
      return new Promise<BackgroundCheckOutcome>((r) => {
        resolve = r;
      });
    },
    isBusy: () => false
  });
  t.after(() => scheduler.stop());

  advance(t, UPDATE_CHECK_INTERVAL_MS + UPDATE_SCHEDULER_TICK_MS);
  assert.equal(reasons.length, 1);
  // While the first check is in flight, later heartbeats must not stack a
  // second request on top of it.
  advance(t, 10 * UPDATE_SCHEDULER_TICK_MS);
  assert.equal(reasons.length, 1);
  resolve('checked');
  await Promise.resolve();
  await Promise.resolve();
  advance(t, UPDATE_SCHEDULER_TICK_MS);
  assert.equal(reasons.length, 1, 'the completed check reset the interval');
});

// ---------------------------------------------------------------------------
// 3. Events that matter more than a timer: wake and network.
// ---------------------------------------------------------------------------

test('a laptop that slept for two days checks on wake, not after the leftover interval', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => false });
  t.after(() => scheduler.stop());

  advance(t, UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, []);

  // Sleep: the webview's timers do not run, and the wall clock jumps. This is
  // exactly the signature the scheduler treats as a wake.
  t.mock.timers.setTime(START_TIME + 48 * HOUR);
  t.mock.timers.tick(UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, ['wake'], 'wake must check immediately');

  // ...and exactly once: the backlog of heartbeats the OS releases on wake
  // must not turn into a burst of update requests.
  advance(t, 10 * UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, ['wake']);
});

test('a short pause is not mistaken for a wake', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => false });
  t.after(() => scheduler.stop());

  t.mock.timers.setTime(START_TIME + UPDATE_WAKE_GAP_MS / 2);
  t.mock.timers.tick(UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, [], 'a busy main thread is not a wake'); 
});

test('the network coming back triggers a check, floored by the spacing guard', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => false });
  t.after(() => scheduler.stop());

  // Right after the launch check: a network flap must not re-hit the endpoint.
  scheduler.notifyNetworkChange();
  scheduler.notifyNetworkChange();
  assert.deepEqual(rec.reasons, [], 'the spacing floor protects /api/updater');

  advance(t, UPDATE_CHECK_MIN_SPACING_MS + UPDATE_SCHEDULER_TICK_MS);
  scheduler.notifyNetworkChange();
  assert.deepEqual(rec.reasons, ['network']);
});

test("the webview's own online event is wired up by the scheduler itself", (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const handlers = new Map<string, Set<() => void>>();
  const fakeWindow = {
    addEventListener(type: string, handler: () => void) {
      const set = handlers.get(type) ?? new Set();
      set.add(handler);
      handlers.set(type, set);
    },
    removeEventListener(type: string, handler: () => void) {
      handlers.get(type)?.delete(handler);
    }
  };
  const globals = globalThis as { window?: unknown };
  const previousWindow = globals.window;
  globals.window = fakeWindow;
  t.after(() => {
    if (previousWindow === undefined) delete globals.window;
    else globals.window = previousWindow;
  });

  const rec = recorder();
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => false });
  assert.equal(handlers.get('online')?.size, 1, "expected an 'online' listener");

  advance(t, UPDATE_CHECK_MIN_SPACING_MS + UPDATE_SCHEDULER_TICK_MS);
  for (const handler of handlers.get('online') ?? []) handler();
  assert.deepEqual(rec.reasons, ['network']);

  scheduler.stop();
  assert.equal(handlers.get('online')?.size, 0, 'stop() must drop the listener');
});

test('an in-meeting wake is deferred like any other check', (t) => {
  t.mock.timers.enable({ apis: ['setInterval', 'Date'], now: START_TIME });
  const rec = recorder();
  let inMeeting = true;
  const scheduler = startUpdateScheduler({ runCheck: rec.runCheck, isBusy: () => inMeeting });
  t.after(() => scheduler.stop());

  t.mock.timers.setTime(START_TIME + 48 * HOUR);
  t.mock.timers.tick(UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, []);
  inMeeting = false;
  advance(t, UPDATE_SCHEDULER_TICK_MS);
  assert.deepEqual(rec.reasons, ['wake']);
});

// ---------------------------------------------------------------------------
// 4. The manual (Settings) path is untouched.
// ---------------------------------------------------------------------------

test('only background checks are quiet -- the manual path still reports progress and failure', () => {
  assert.equal(isQuietUpdateCheckReason('periodic'), true);
  assert.equal(isQuietUpdateCheckReason('wake'), true);
  assert.equal(isQuietUpdateCheckReason('network'), true);
  assert.equal(isQuietUpdateCheckReason('manual'), false);
  assert.equal(isQuietUpdateCheckReason('main-menu'), false);
  assert.equal(isQuietUpdateCheckReason('launch'), false);
  assert.equal(isQuietUpdateCheckReason(undefined), false);

  // Settings' manual check still calls checkForUpdate directly, unchanged, and
  // its options (skipRelaunch + reason) still exist (#113).
  assert.match(settingsSource, /checkForUpdate\(\{ skipRelaunch: true, reason: 'manual' \}\)/);
  assert.match(updaterSource, /opts: \{ skipRelaunch\?: boolean; reason\?: UpdateCheckReason \} = \{\}/);

  // The toast/failure suppression is keyed on `quiet` only -- a manual check
  // takes the same code path it always did.
  assert.match(updaterSource, /const quiet = isQuietUpdateCheckReason\(opts\.reason\)/);
  assert.match(updaterSource, /if \(!quiet\) markUpdateDownloading\(\)/);
  assert.match(updaterSource, /if \(!quiet\) markUpdateFailed\(friendlyMessage\)/);
  // Availability semantics unchanged: a check never downloads or installs.
  assert.match(updaterSource, /COMMANDS\.checkCompatibleUpdateAvailable/);
  assert.doesNotMatch(
    updaterSource.slice(0, updaterSource.indexOf('export async function installUpdateAndRelaunch')),
    /COMMANDS\.downloadAndInstallCompatibleUpdate/
  );
});

// ---------------------------------------------------------------------------
// 5. The wiring in the shipped root layout (the path that actually calls this).
// ---------------------------------------------------------------------------

test('the root layout starts the scheduler in the main window, beside the launch check', () => {
  assert.match(layoutSource, /import \{\s*startUpdateScheduler,/);
  const onMountBody = layoutSource.slice(
    layoutSource.indexOf('onMount(() => {'),
    layoutSource.indexOf('// Route transitions')
  );
  assert.ok(onMountBody.length > 0, 'could not locate the onMount() body in +layout.svelte');

  const block = onMountBody.match(
    /if \(([\s\S]*?)\) \{\s*void runUpdateCheck\('launch', \{ force: true \}\);([\s\S]*?)\n    \}/
  );
  assert.ok(block, 'expected the launch check and the scheduler to share one main-window guard');
  assert.match(
    block![1],
    /getCurrentWindow\(\)\.label === 'main'/,
    'the scheduler must be main-window-only, like the launch check'
  );
  assert.match(block![2], /startUpdateScheduler\(\{/);
  assert.match(block![2], /runCheck: runBackgroundUpdateCheck/);
  assert.match(block![2], /isBusy: \(\) => isMeetingRoute/);
  // Petal's existing OS-level network signal feeds the scheduler too.
  assert.match(block![2], /EVENTS\.resilienceEvent/);
  assert.match(block![2], /scheduler\.notifyNetworkChange\(\)/);
  // And it is torn down with the layout.
  assert.match(onMountBody, /stopUpdateScheduler\?\.\(\)/);
});

test('the layout defers a background check on authoritative in-room state', () => {
  const fn = layoutSource.slice(
    layoutSource.indexOf('async function runBackgroundUpdateCheck'),
    layoutSource.indexOf('$effect(() => {\n    if (!isMainRoute) return;')
  );
  assert.ok(fn.length > 0, 'could not locate runBackgroundUpdateCheck in +layout.svelte');
  assert.match(fn, /if \(isOverlayRoute \|\| isMeetingRoute\) return 'deferred'/);
  assert.match(fn, /COMMANDS\.currentRoom/);
  assert.match(fn, /if \(room\) return 'deferred'/);
  assert.match(fn, /await runUpdateCheck\(reason\)/);
  assert.match(fn, /return 'checked'/);
});

test('the chosen cadence is hours, not minutes, and is stated in the source', () => {
  assert.equal(UPDATE_CHECK_INTERVAL_MS, 6 * HOUR);
  assert.ok(UPDATE_CHECK_INTERVAL_MS >= HOUR, 'polling must be hours-scale, never minutes');
  assert.equal(UPDATE_SCHEDULER_TICK_MS, 60 * 1000);
  assert.ok(
    UPDATE_WAKE_GAP_MS > UPDATE_SCHEDULER_TICK_MS,
    'the wake threshold must be well above one heartbeat'
  );
  assert.ok(UPDATE_CHECK_MIN_SPACING_MS <= UPDATE_CHECK_INTERVAL_MS);
});
