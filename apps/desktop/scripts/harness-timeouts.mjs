#!/usr/bin/env node
// The live harness's timeout policy, in one place (#102).
//
// Before this, nothing bounded a live loopback run at any level: no per-command
// timeout on the autotest socket, no overall budget in the wrapper, and no
// `timeout-minutes` on the workflow job. GitHub's 6-hour default was the only
// limit, and on 2026-09-08 one wedged run held the single self-hosted macOS VM
// for 81 minutes with a release e2e gate queued behind it.
//
// Three nested bounds now, innermost first, each sized well above anything a
// healthy run does so none can ever fail a good run:
//
//   1. per autotest-socket command   60s   (healthy: milliseconds)
//   2. whole `--live` scenario       15m   (healthy: ~2 minutes)
//   3. the workflow job            45m   (healthy: 6-12 minutes)
//
// The innermost bound that fires is the one that produces the most specific
// diagnosis, which is why there are three rather than only the job cap.
import process from 'node:process';

export const COMMAND_TIMEOUT_ENV = 'PETAL_AUTOTEST_COMMAND_TIMEOUT_MS';
export const RUN_TIMEOUT_ENV = 'PETAL_HARNESS_RUN_TIMEOUT_MS';

/// Per autotest-socket command. The slowest legitimate command observed in a
/// passing run is the #298 SDK reconnect simulation at ~100ms.
export const DEFAULT_COMMAND_TIMEOUT_MS = 60_000;
/// Whole `--live` run. A healthy loopback tier takes about two minutes; the
/// wedged one ran 81 before a human cancelled it.
export const DEFAULT_RUN_TIMEOUT_MS = 900_000;

function positiveMs(name, raw, fallback) {
  if (raw === undefined || raw === '') return fallback;
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive number of milliseconds, got '${raw}'`);
  }
  return parsed;
}

export function commandTimeoutMs(env = process.env) {
  return positiveMs(COMMAND_TIMEOUT_ENV, env[COMMAND_TIMEOUT_ENV], DEFAULT_COMMAND_TIMEOUT_MS);
}

export function runTimeoutMs(env = process.env) {
  return positiveMs(RUN_TIMEOUT_ENV, env[RUN_TIMEOUT_ENV], DEFAULT_RUN_TIMEOUT_MS);
}
