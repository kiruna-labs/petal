#!/usr/bin/env node
// Fail-closed exit accounting for the remote-control harness (plan §6a).
//
// The defect these tests pin: on 2026-08-10 the scenario printed one `# SKIP`
// line, executed ZERO cases and exited 0 because Chrome's CDP endpoint was
// unreachable. Nothing downstream could tell that from a clean run. Exit 2 --
// already this file's convention for "no result" via --acceptance-446's failed
// positive control -- now covers every way a run can prove nothing.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  HOST_RC_LINE_NEEDLE,
  NO_RESULT_EXIT_CODE,
  SUITE_SUMMARY_KEYS,
  TOKENLESS_DROP_NEEDLE,
  assessHostLogEvidence,
  noResultSummary,
  numberedSuiteExitCode,
  resolvePetalLogPath,
  suiteExitCode,
} from './remote-control-exit.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '../../..');
const scenarioPath = path.join(scriptDir, 'remote-control-scenario.mjs');
const loopbackPath = path.join(scriptDir, 'remote-control-local-loopback.mjs');

function runWithoutSocket(script, args) {
  const env = { ...process.env };
  delete env.PETAL_AUTOTEST_SOCK;
  return spawnSync(process.execPath, [script, ...args], {
    cwd: repoRoot,
    encoding: 'utf8',
    env,
  });
}

function summaryLines(stdout) {
  return (stdout ?? '')
    .split(/\r?\n/)
    .filter((line) => line.startsWith('SUMMARY '))
    .map((line) => JSON.parse(line.slice('SUMMARY '.length)));
}

test('suiteExitCode calls a run that proved nothing "no result", not a pass', () => {
  // Zero cases executed -- the exact 2026-08-10 false green.
  assert.equal(suiteExitCode({ total: 0, pass: 0, fail: 0, skip: 0 }), NO_RESULT_EXIT_CODE);
  // Cases executed, none passing: also "ran, proved nothing".
  assert.equal(suiteExitCode({ total: 30, pass: 0, fail: 0, skip: 30 }), NO_RESULT_EXIT_CODE);
  // A malformed or absent summary can never read as a pass either.
  assert.equal(suiteExitCode(undefined), NO_RESULT_EXIT_CODE);
  assert.equal(suiteExitCode({}), NO_RESULT_EXIT_CODE);
  assert.equal(suiteExitCode({ total: 1, pass: '1', fail: 0 }), NO_RESULT_EXIT_CODE);
  // Real verdicts still work.
  assert.equal(suiteExitCode({ total: 30, pass: 29, fail: 1, skip: 0 }), 1);
  assert.equal(suiteExitCode({ total: 30, pass: 30, fail: 0, skip: 0 }), 0);
  assert.equal(suiteExitCode({ total: 30, pass: 28, fail: 0, skip: 2 }), 0);
});

test('noResultSummary is a real SUMMARY that reports zero cases plus a reason', () => {
  const summary = noResultSummary('CDP endpoint unreachable');
  assert.deepEqual(summary, {
    total: 0,
    pass: 0,
    fail: 0,
    skip: 0,
    noResult: { reason: 'CDP endpoint unreachable' },
  });
  assert.equal(suiteExitCode(summary), NO_RESULT_EXIT_CODE);
});

test('the scenario exits 2 with a no-result SUMMARY when it cannot start', () => {
  const result = runWithoutSocket(scenarioPath, []);
  assert.equal(result.status, NO_RESULT_EXIT_CODE, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
  const summaries = summaryLines(result.stdout);
  assert.equal(summaries.length, 1, `expected exactly one SUMMARY line, got ${summaries.length}`);
  assert.equal(summaries[0].total, 0);
  assert.equal(summaries[0].pass, 0);
  assert.match(summaries[0].noResult?.reason ?? '', /PETAL_AUTOTEST_SOCK/);
});

test('the loopback wrapper exits 2 when neither --live nor a socket is given', () => {
  // This is exactly how a misconfigured live run lands.
  const result = runWithoutSocket(loopbackPath, ['--skip-preflight']);
  assert.equal(result.status, NO_RESULT_EXIT_CODE, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
  const summaries = summaryLines(result.stdout);
  assert.equal(summaries.length, 1);
  assert.equal(summaries[0].total, 0);
});

test('--check-only keeps its exit 0 -- it genuinely ran the preflight', () => {
  const result = runWithoutSocket(loopbackPath, ['--check-only', '--skip-swift-typecheck']);
  assert.equal(result.status, 0, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
});

test("the numbered suite's SUMMARY literal matches SUITE_SUMMARY_KEYS", () => {
  // Keeps the producer, apps/desktop/scripts/remote-control-exit.mjs's key
  // list, and scripts/cross-machine-rc-suite.sh's reducer allowlist (pinned to
  // the same list by scripts/test-cross-machine-rc-suite.sh) in lockstep.
  const source = readFileSync(scenarioPath, 'utf8');
  const marker = 'SUMMARY-KEYS-PINNED';
  const markerIndex = source.indexOf(marker);
  assert.notEqual(markerIndex, -1, `${marker} marker missing from remote-control-scenario.mjs`);
  const literalStart = source.indexOf('const summary = {', markerIndex);
  assert.notEqual(literalStart, -1, 'no `const summary = {` after the marker');
  const literalEnd = source.indexOf('\n    };\n', literalStart);
  assert.notEqual(literalEnd, -1, 'no terminating `};` for the pinned summary literal');
  const body = source.slice(literalStart, literalEnd);
  // Accepts both `key: value` and shorthand `key,` properties.
  const keys = [...body.matchAll(/^ {6}([A-Za-z_$][\w$]*)\s*[:,]/gm)].map((entry) => entry[1]);
  assert.deepEqual(keys, [...SUITE_SUMMARY_KEYS]);
});

test('every exit site in the scenario is accounted for', () => {
  // A snapshot pin, deliberately. §6a exists because an exit site quietly read
  // `? 1 : 0` where it should have failed closed, and no test could see it: the
  // numbered/press-to-photon/acceptance branches all need a live socket to
  // reach. Any change to any exit site must fail here and be argued for.
  const source = readFileSync(scenarioPath, 'utf8');
  const sites = [...source.matchAll(/^\s*process\.exitCode = (.+)$/gm)].map((entry) => entry[1].trim());
  assert.deepEqual(sites, [
    // --acceptance-446: 2 for a failed positive control, and suiteExitCode
    // applies the same "proved nothing" rule when the control passed.
    'report.controlPassed ? suiteExitCode(report.summary) : 2;',
    // --rapid-click-burst: already fails closed on an empty burst ledger.
    'report.bursts.length === 0 ? 1 : 0;',
    // --cockpit-drive: already fails closed on an empty drive ledger.
    'report.driven.length ? 0 : 1;',
    // press-to-photon infrastructure branch: reports status 'skip', so its
    // pass count is 0 and suiteExitCode returns 2. It used to hardcode 0.
    'suiteExitCode(report.summary);',
    // press-to-photon suite proper.
    'suiteExitCode(report.summary);',
    // The numbered suite. `passBarShortfall` is 6c's --input-only pass bar:
    // a bar case that SKIPS adds no failure count, so without it a relaxed run
    // could exit 0 having proved none of the sentinel-oracle claims. A blind
    // host log (#580) makes it "no result"; see numberedSuiteExitCode tests.
    'numberedSuiteExitCode(summary, passBarShortfall);',
  ]);
});

// ---- #580: the tokenless-drop count must come from the log the host writes --
//
// On the CI VM the host writes PETAL_LOG_PATH (petal-dev.log); the scenario
// read ~/Library/Logs/Petal/petal.log and counted 0 drops from a log that
// never saw the run. Both directions are pinned: a log with host
// remote-control lines is trusted, a log without them is infra-invalid.

const hostLine = (text) => `2026-10-17 18:10:43.408 [INFO] [desktop_lib::remote_control] ${text}`;
const VM_LOG = [
  hostLine(`${HOST_RC_LINE_NEEDLE}Request action=None window=165 seq=1 controller='web-1'`),
  hostLine(`${HOST_RC_LINE_NEEDLE}Pointer action=Some(Click) window=165 seq=7 controller='web-1'`),
].join('\n');

test('resolvePetalLogPath finds the dated user log #905 writes when petal.log is gone', () => {
  const home = '/Users/runner';
  const dir = '/Users/runner/Library/Logs/Petal';
  const listing = ['petal.log.2026-10-20.gz', 'petal.log.2026-10-22', 'petal.log.2026-10-23', 'petal.log.release-smoke-baseline', 'test-runs'];
  assert.equal(resolvePetalLogPath({}, home, () => listing), `${dir}/petal.log.2026-10-23`);
  assert.equal(resolvePetalLogPath({}, home, () => [...listing, 'petal.log']), `${dir}/petal.log`);
  assert.equal(resolvePetalLogPath({}, home, () => ['petal.log.2026-10-20.gz']), `${dir}/petal.log`);
  assert.equal(resolvePetalLogPath({ PETAL_LOG_PATH: '/tmp/x.log' }, home, () => listing), '/tmp/x.log');
});

test('resolvePetalLogPath prefers PETAL_LOG_PATH, then PETAL_ACCEPTANCE_LOG, then the user log', () => {
  const home = '/Users/runner';
  assert.equal(
    resolvePetalLogPath({ PETAL_LOG_PATH: '/tmp/petal-dev.log', PETAL_ACCEPTANCE_LOG: '/tmp/acc.log' }, home),
    '/tmp/petal-dev.log'
  );
  assert.equal(resolvePetalLogPath({ PETAL_ACCEPTANCE_LOG: '/tmp/acc.log' }, home), '/tmp/acc.log');
  assert.equal(resolvePetalLogPath({}, home), '/Users/runner/Library/Logs/Petal/petal.log');
  assert.equal(resolvePetalLogPath({ PETAL_LOG_PATH: '' }, home), '/Users/runner/Library/Logs/Petal/petal.log');
});

test('a host log with remote-control lines is trusted and its drops are counted', () => {
  const clean = assessHostLogEvidence({ logText: VM_LOG, logPath: '/tmp/petal-dev.log', rcCasesRan: 30 });
  assert.equal(clean.tokenlessDrops, 0);
  assert.deepEqual(clean.hostLogEvidence, { path: '/tmp/petal-dev.log', remoteControlLines: 2, verdict: 'ok' });

  const dropped = assessHostLogEvidence({
    logText: `${VM_LOG}\n${hostLine(`remote-control: ${TOKENLESS_DROP_NEEDLE} from 'web-1' for window 165`)}`,
    logPath: '/tmp/petal-dev.log',
    rcCasesRan: 30,
  });
  assert.equal(dropped.tokenlessDrops, 1);
  assert.equal(dropped.hostLogEvidence.verdict, 'ok');
});

test('a host log with no remote-control line while cases ran is infra-invalid, never 0 drops', () => {
  for (const logText of ['', 'unrelated line\nanother one', null]) {
    const blind = assessHostLogEvidence({ logText, logPath: '/Users/runner/Library/Logs/Petal/petal.log', rcCasesRan: 30 });
    assert.equal(blind.tokenlessDrops, null, `logText=${JSON.stringify(logText)}`);
    assert.equal(blind.hostLogEvidence.verdict, 'infra-invalid');
    assert.equal(blind.hostLogEvidence.remoteControlLines, 0);
  }
  // A drop line alone does not prove the reader sees this run's host traffic.
  const dropOnly = assessHostLogEvidence({ logText: TOKENLESS_DROP_NEEDLE, logPath: 'x', rcCasesRan: 1 });
  assert.equal(dropOnly.hostLogEvidence.verdict, 'infra-invalid');
});

test('no remote-control case ran: an empty host log is not evidence of a wrong path', () => {
  const idle = assessHostLogEvidence({ logText: '', logPath: 'x', rcCasesRan: 0 });
  assert.equal(idle.tokenlessDrops, 0);
  assert.equal(idle.hostLogEvidence.verdict, 'ok');
});

test('numberedSuiteExitCode: a blind host log is "no result", a trusted one keeps the old accounting', () => {
  const ok = { remoteControlLines: 12, verdict: 'ok', path: 'x' };
  const blind = { remoteControlLines: 0, verdict: 'infra-invalid', path: 'x' };
  const green = { total: 3, pass: 3, fail: 0, skip: 0 };
  assert.equal(numberedSuiteExitCode({ ...green, tokenlessDrops: 0, hostLogEvidence: ok }), 0);
  assert.equal(numberedSuiteExitCode({ ...green, tokenlessDrops: 2, hostLogEvidence: ok }), 1);
  assert.equal(numberedSuiteExitCode({ ...green, tokenlessDrops: 0, hostLogEvidence: ok }, 1), 1);
  assert.equal(numberedSuiteExitCode({ ...green, fail: 1, pass: 2, tokenlessDrops: 0, hostLogEvidence: ok }), 1);
  assert.equal(numberedSuiteExitCode({ total: 0, pass: 0, fail: 0, skip: 0, tokenlessDrops: 0, hostLogEvidence: ok }), NO_RESULT_EXIT_CODE);
  // The defect: every case green, 0 drops "counted" from the wrong file.
  assert.equal(numberedSuiteExitCode({ ...green, tokenlessDrops: null, hostLogEvidence: blind }), NO_RESULT_EXIT_CODE);
  assert.equal(numberedSuiteExitCode({ ...green, fail: 1, pass: 2, tokenlessDrops: null, hostLogEvidence: blind }), NO_RESULT_EXIT_CODE);
  // Missing evidence is not "ok".
  assert.equal(numberedSuiteExitCode({ ...green, tokenlessDrops: 0 }), NO_RESULT_EXIT_CODE);
});

test('the scenario counts tokenless drops through the host-log evidence check, from this run only', () => {
  const source = readFileSync(scenarioPath, 'utf8');
  assert.match(source, /const hostLogStartOffset = petalLogSize\(\);/);
  assert.match(source, /assessHostLogEvidence\(\{\s*logText: readPetalLogSince\(hostLogStartOffset\)/);
  assert.match(source, /const petalLogPath = resolvePetalLogPath\(\);/);
  assert.doesNotMatch(source, /process\.env\.PETAL_ACCEPTANCE_LOG/, 'log path resolution lives in resolvePetalLogPath only');
});
