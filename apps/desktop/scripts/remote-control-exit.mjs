// Single source of truth for how a remote-control suite run becomes an exit
// code, and for the SUMMARY key set the cross-machine reducer allowlists.
//
// Exit 2 means "no result": the run proved nothing and must never read as a
// pass. That convention already existed in remote-control-scenario.mjs (the
// --acceptance-446 branch used it for a failed positive control); this module
// makes it the rule instead of one branch's local decision. On 2026-08-10 an
// unreachable Chrome CDP endpoint made the scenario print one `# SKIP` line,
// execute ZERO cases, and exit 0 -- a false green no gate could catch.

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
