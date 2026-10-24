// Single source of truth for how a remote-control suite run becomes an exit
// code, and for the SUMMARY key set the cross-machine reducer allowlists.
//
// Exit 2 means "no result": the run proved nothing and must never read as a
// pass. That convention already existed in remote-control-scenario.mjs (the
// --acceptance-446 branch used it for a failed positive control); this module
// makes it the rule instead of one branch's local decision. On 2026-08-10 an
// unreachable Chrome CDP endpoint made the scenario print one `# SKIP` line,
// execute ZERO cases, and exit 0 -- a false green no gate could catch.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const NO_RESULT_EXIT_CODE = 2;

// Canonical key set of the numbered suite's SUMMARY line and of the `summary`
// object in its --json report. scripts/cross-machine-rc-suite.sh's
// reduce_suite_results allowlists exactly these keys, and
// scripts/test-cross-machine-rc-suite.sh pins the two together, so adding a
// key here without widening that allowlist fails the gate instead of silently
// reducing every real run to `malformed-results` -- which is what #580's
// `tokenlessDrops` did for months.
export const SUITE_SUMMARY_KEYS = Object.freeze([
  'total',
  'pass',
  'fail',
  'skip',
  'recoveries',
  'tokenlessDrops',
  // Which host log `tokenlessDrops` was counted from, and whether it can be
  // trusted: see assessHostLogEvidence below.
  'hostLogEvidence',
  // 6c: which gate this run actually ran. `shareReadiness` is 'live-tile' for
  // the full gate and 'target-present' for --input-only, so no consumer can
  // mistake a relaxed run for the full one.
  'mode',
  'shareReadiness',
  'targetObservationLatency',
]);

// The SUMMARY emitted when a run cannot start. Printing a real SUMMARY is the
// load-bearing half of the fix: it removes remote-control-local-loopback.mjs's
// silent `parsedSummary == null` path, so "nothing ran" is reported rather
// than inferred from an absent line.
export function noResultSummary(reason) {
  return { total: 0, pass: 0, fail: 0, skip: 0, noResult: { reason } };
}

export function suiteExitCode(summary) {
  const total = summary?.total;
  const pass = summary?.pass;
  const fail = summary?.fail;
  if (!Number.isInteger(total) || !Number.isInteger(pass) || !Number.isInteger(fail)) {
    return NO_RESULT_EXIT_CODE;
  }
  // "Ran, proved nothing" is the shape of every historical false green here:
  // zero cases executed, or cases executed with not one of them passing.
  if (total === 0 || pass === 0) return NO_RESULT_EXIT_CODE;
  return fail > 0 ? 1 : 0;
}

// ---- #580 tokenless-drop evidence: read the log the host actually writes ----
//
// `tokenlessDrops` is counted from the host's own log. On the CI VM the host
// writes to PETAL_LOG_PATH (petal-dev.log), not ~/Library/Logs/Petal/petal.log,
// and the scenario used to read only the latter -- so the check counted 0 from
// a log that never saw the run. A zero from a blind log is not a zero.

/// Logged by the host for every remote-control packet it receives (high-rate
/// pointer moves are sampled, everything else is logged), at info level, from
/// the same module as the tokenless-drop warning.
export const HOST_RC_LINE_NEEDLE = 'remote-control: received kind=';
export const TOKENLESS_DROP_NEEDLE = 'dropping tokenless input';

/// The host log the scenario reads. PETAL_LOG_PATH wins because it is where a
/// harness-launched host writes; PETAL_ACCEPTANCE_LOG is the older override.
///
/// Without an override it is the user log. Since #905 the active file is
/// `petal.log.<YYYY-MM-DD>` and a bare `petal.log` no longer exists, so a run
/// that read `petal.log` counted 0 host lines and went infra-invalid (and the
/// display-share cases 37-41 read their routing slice from the same blind
/// file). Prefer a live `petal.log`, else the newest dated, uncompressed file.
export function resolvePetalLogPath(env = process.env, homedir = os.homedir(), listDir = defaultListDir) {
  if (env.PETAL_LOG_PATH) return env.PETAL_LOG_PATH;
  if (env.PETAL_ACCEPTANCE_LOG) return env.PETAL_ACCEPTANCE_LOG;
  const dir = path.join(homedir, 'Library', 'Logs', 'Petal');
  const names = listDir(dir);
  if (names.includes('petal.log')) return path.join(dir, 'petal.log');
  const dated = names.filter((name) => /^petal\.log\.\d{4}-\d{2}-\d{2}$/.test(name)).sort();
  return path.join(dir, dated.length > 0 ? dated[dated.length - 1] : 'petal.log');
}

function defaultListDir(dir) {
  try {
    return fs.readdirSync(dir);
  } catch {
    return [];
  }
}

/// Count tokenless drops in `logText`, the host log written during this run.
/// If remote-control cases ran but the log holds not one host remote-control
/// line, the reader is looking at the wrong file (or an empty one): the count
/// is unknown, `tokenlessDrops` is null, and the run is infra-invalid.
export function assessHostLogEvidence({ logText, logPath, rcCasesRan }) {
  const lines = String(logText ?? '').split('\n');
  const remoteControlLines = lines.filter((line) => line.includes(HOST_RC_LINE_NEEDLE)).length;
  const drops = lines.filter((line) => line.includes(TOKENLESS_DROP_NEEDLE)).length;
  const blind = rcCasesRan > 0 && remoteControlLines === 0;
  return {
    tokenlessDrops: blind ? null : drops,
    hostLogEvidence: {
      path: logPath ?? null,
      remoteControlLines,
      verdict: blind ? 'infra-invalid' : 'ok',
    },
  };
}

/// Exit code of the numbered suite. An infra-invalid host log means the #580
/// check proved nothing, so the run is "no result" (2), never a pass. A
/// tokenless drop or a missed --input-only pass bar turns a clean run into 1.
export function numberedSuiteExitCode(summary, passBarShortfall = 0) {
  const base = suiteExitCode(summary);
  if (summary?.hostLogEvidence?.verdict !== 'ok') return Math.max(base, NO_RESULT_EXIT_CODE);
  if (base === 0 && (summary.tokenlessDrops > 0 || passBarShortfall > 0)) return 1;
  return base;
}
