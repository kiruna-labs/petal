#!/usr/bin/env node
// Share-readiness diagnostics (plan 6b).
//
// Five consecutive share-ready failures on 2026-08-10 reported `last=null` and
// nothing else, because waitForLiveTile discarded its own probe state on the
// failing path. These tests pin the three distinct diagnoses apart and pin the
// wiring that carries them out of the timeout.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  INPUT_ONLY_PASS_BAR_CASE_IDS,
  INPUT_ONLY_PASS_BAR_EXCLUDED,
  INPUT_ONLY_SCOPE_LINES,
  describeTileState,
  inputOnlyPassBarVerdict,
  liveTileProbeExpression,
  shareReadinessMode,
  shareReadyPredicate,
  tileFailureDetail,
  tileIsInputReady,
  tileIsLive,
} from './remote-control-share-readiness.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '../../..');
const scenarioPath = path.join(scriptDir, 'remote-control-scenario.mjs');
const loopbackPath = path.join(scriptDir, 'remote-control-local-loopback.mjs');

const LIVE = { found: true, tileId: 'tile-7', readyState: 4, videoWidth: 1288, live: true };
const NO_TARGET = { found: false, tileId: null, readyState: -1, videoWidth: 0, live: false };
const NO_VIDEO_ELEMENT = { found: true, tileId: 'tile-7', readyState: -1, videoWidth: 0, live: false };
const NO_MEDIA_DATA = { found: true, tileId: 'tile-7', readyState: 0, videoWidth: 0, live: false };
const NO_SIZED_FRAME = { found: true, tileId: 'tile-7', readyState: 4, videoWidth: 0, live: false };

test('tileIsLive accepts only a decoded, sized frame', () => {
  assert.equal(tileIsLive(LIVE), true);
  for (const state of [NO_TARGET, NO_VIDEO_ELEMENT, NO_MEDIA_DATA, NO_SIZED_FRAME, null, undefined]) {
    assert.equal(tileIsLive(state), false, JSON.stringify(state));
  }
});

test('the three share-ready failure shapes get three different diagnoses', () => {
  // These are three different bugs with three different next steps, and they
  // were all reported identically as `last=null`.
  const diagnoses = [NO_TARGET, NO_MEDIA_DATA, NO_SIZED_FRAME].map(describeTileState);
  assert.equal(new Set(diagnoses).size, 3, diagnoses.join(' | '));
  assert.match(diagnoses[0], /never reached the browser/);
  assert.match(diagnoses[1], /readyState=0/);
  assert.match(diagnoses[2], /videoWidth=0/);
  assert.match(describeTileState(NO_VIDEO_ELEMENT), /no <video> element/);
  assert.match(describeTileState(null), /never returned/);
  assert.equal(describeTileState(LIVE), 'live');
});

test('tileFailureDetail carries the raw state AND the diagnosis', () => {
  const detail = tileFailureDetail(NO_SIZED_FRAME);
  assert.match(detail, /"readyState":4/);
  assert.match(detail, /"videoWidth":0/);
  assert.match(detail, /diagnosis=video ready but videoWidth=0/);
  // The historical failure: a null state must still say something useful.
  assert.match(tileFailureDetail(null), /lastTileState=null diagnosis=.+/);
  assert.ok(!tileFailureDetail(null).endsWith('diagnosis='));
});

test('the probe expression reads every field the diagnoses depend on', () => {
  const expression = liveTileProbeExpression(4242);
  assert.match(expression, /candidate\.windowId === 4242/);
  for (const field of ['found', 'tileId', 'readyState', 'videoWidth', 'live']) {
    assert.match(expression, new RegExp(`${field}:`));
  }
});

test('the scenario reports the last probe state and runs forensics on a share-ready timeout', () => {
  const source = readFileSync(scenarioPath, 'utf8');
  // One probe expression, shared by every readiness mode: a relaxed mode must
  // never be able to measure something different from the full gate.
  assert.match(source, /liveTileProbeExpression\(windowId\)/);
  assert.ok(
    !source.includes('const api = window.__petalHarness?.remoteControl;\n          const target'),
    'the probe expression must not be inlined in the scenario as well'
  );
  // The state must leave waitForLiveTile, not be swallowed on the failing path.
  assert.match(source, /\$\{error\.message\} \$\{tileFailureDetail\(lastState\)\}/);
  // captureCaseFailureForensics already screenshots and dumps metrics; it just
  // never ran for the one failure that mattered.
  assert.match(source, /await captureCaseFailureForensics\(client, 'share-ready'\);/);
});

// ---------------------------------------------------------------------------
// --input-only (plan 6c)

test('the input-only bar accepts a present target and nothing weaker', () => {
  // Same probe, relaxed predicate: no video element, readyState or frame size.
  assert.equal(tileIsInputReady(NO_MEDIA_DATA), true);
  assert.equal(tileIsInputReady(NO_SIZED_FRAME), true);
  assert.equal(tileIsInputReady(NO_VIDEO_ELEMENT), true);
  assert.equal(tileIsInputReady(LIVE), true);
  // Still fails closed on the one thing it does check.
  assert.equal(tileIsInputReady(NO_TARGET), false);
  assert.equal(tileIsInputReady({ found: true, tileId: null }), false);
  assert.equal(tileIsInputReady(null), false);
  // And it is genuinely weaker than the full gate, not a rename of it.
  assert.equal(tileIsLive(NO_MEDIA_DATA), false);
});

test('shareReadyPredicate/shareReadinessMode pick the mode, and label it', () => {
  assert.equal(shareReadyPredicate(true), tileIsInputReady);
  assert.equal(shareReadyPredicate(false), tileIsLive);
  assert.equal(shareReadinessMode(true), 'target-present');
  assert.equal(shareReadinessMode(false), 'live-tile');
});

test('the scope note names BOTH observed failure shapes, and rescues only one', () => {
  const note = INPUT_ONLY_SCOPE_LINES.join('\n');
  assert.match(note, /video path NOT verified/);
  // This is NOT "runs with capture dead".
  assert.match(note, /still blocks on a first captured frame/);
  assert.match(note, /proves nothing about pixels reaching a viewer/);

  // The correction that matters. The plan asserted --input-only covers "the
  // observed failure"; the 6e log analysis found TWO shapes, and this flag
  // rescues only one. petal-e2e-final.log and petal-dev-781.log both show
  // session::share emitting exactly four lines, stopping at "starting SCStream
  // capture" -- start_share never returned, so a relaxed readiness predicate
  // downstream of it cannot help. Claiming otherwise is the named risk here.
  assert.match(note, /RESCUES this/);
  assert.match(note, /does NOT rescue this/);
  assert.match(note, /start_share itself never returns/);
  assert.match(note, /status=Idle, dirty_rects=0/);
  // Cited evidence, so the claim is checkable rather than asserted.
  assert.match(note, /petal-dev-rc3\.log is shape \(a\)/);
  assert.match(note, /petal-e2e-final\.log is shape \(b\)/);
});

test('the input-only pass bar is exactly the sentinel-oracle cases', () => {
  // Fixed so a later reader cannot quietly lower it. These are the cases whose
  // oracle is the sentinel's own foreign-process NSEvent ledger.
  assert.deepEqual([...INPUT_ONLY_PASS_BAR_CASE_IDS], [5, 8, 15, 16, 21, 25, 26, 28, 29, 30]);
  // Case 23 is a sentinel case but needs a second display, so it is excluded by
  // name rather than by silently dropping it from the list.
  assert.deepEqual([...INPUT_ONLY_PASS_BAR_EXCLUDED], [{ caseId: 23, reason: 'needs a second display' }]);
});

test('a skipped bar case does NOT count as meeting the bar', () => {
  // The whole reason the bar has teeth: a skip produces no failure count, so a
  // relaxed run could otherwise exit 0 having proved none of the #779 claim.
  const allPass = INPUT_ONLY_PASS_BAR_CASE_IDS.map((caseId) => ({ caseId, status: 'pass' }));
  assert.equal(inputOnlyPassBarVerdict(allPass).met, true);

  const oneSkipped = allPass.map((result) => (result.caseId === 21 ? { caseId: 21, status: 'skip' } : result));
  const skippedVerdict = inputOnlyPassBarVerdict(oneSkipped);
  assert.equal(skippedVerdict.met, false);
  assert.deepEqual(skippedVerdict.missing, [21]);
  assert.equal(skippedVerdict.passed, 9);

  // A case absent from the report entirely is missing, not passing.
  assert.deepEqual(inputOnlyPassBarVerdict([]).missing, [...INPUT_ONLY_PASS_BAR_CASE_IDS]);
  assert.equal(inputOnlyPassBarVerdict(undefined).met, false);
});

test('the excluded second-display case may skip but may never fail', () => {
  const allPass = INPUT_ONLY_PASS_BAR_CASE_IDS.map((caseId) => ({ caseId, status: 'pass' }));
  assert.equal(inputOnlyPassBarVerdict([...allPass, { caseId: 23, status: 'skip' }]).met, true);
  const failed = inputOnlyPassBarVerdict([...allPass, { caseId: 23, status: 'fail' }]);
  assert.equal(failed.met, false);
  assert.deepEqual(failed.excludedFailures, [23]);
});

test('the scenario wires the mode through predicate, SUMMARY and exit code', () => {
  const source = readFileSync(scenarioPath, 'utf8');
  // One probe, one predicate seam -- the acceptance predicate is the ONLY thing
  // --input-only changes about readiness.
  assert.match(source, /const tileAccepted = shareReadyPredicate\(inputOnlyMode\);/);
  assert.match(source, /return tileAccepted\(lastState\) \? lastState : null;/);
  // assertShareBorderStacked is a WindowServer readback and stays fatal in BOTH
  // modes -- it is not gated on inputOnlyMode anywhere.
  assert.match(source, /await assertShareBorderStacked\(client, shared\.windowId\);/);
  assert.ok(
    !/inputOnlyMode[^\n]*assertShareBorderStacked|assertShareBorderStacked[^\n]*inputOnlyMode/.test(source),
    'assertShareBorderStacked must not be conditional on the readiness mode'
  );
  // Anti-confusion mechanism 1 of 3: the SUMMARY says which gate ran.
  assert.match(source, /mode: inputOnlyMode \? 'input-only' : 'numbered',/);
  assert.match(source, /shareReadiness: shareReadinessMode\(inputOnlyMode\),/);
  // Excluded from the two video-reading modes.
  assert.match(source, /--input-only cannot be combined with --press-to-photon/);
  assert.match(source, /--input-only cannot be combined with --rapid-click-burst/);
  // The bar is enforced, not merely printed.
  assert.match(source, /passBarShortfall = verdict\.met \? 0 : 1;/);
});

test('the wrapper isolates the input-only artifact and labels the run', () => {
  const source = readFileSync(loopbackPath, 'utf8');
  assert.match(source, /const INPUT_ONLY_RESULTS_PATH = '\/tmp\/rc-results-input-only\.json';/);
  assert.match(source, /fs\.rmSync\(jsonOutputPath, \{ force: true \}\);/);
  // A relaxed run must not be writable into the full gate's artifact.
  assert.match(source, /must contain 'input-only'/);
  assert.match(source, /--input-only requires --live/);
  assert.match(source, /for \(const line of INPUT_ONLY_SCOPE_LINES\) console\.log\(`# \$\{line\}`\);/);
  assert.match(source, /if \(inputOnly\) scenarioArgs\.push\('--input-only'\);/);
});

test('the wrapper actually rejects every unsafe --input-only combination', () => {
  // Executed, not grepped: these throws all happen during argument validation,
  // before any socket or preflight is needed.
  const cases = [
    [['--input-only'], /--input-only requires --live/],
    [['--live', '--input-only', '--press-to-photon'], /cannot be combined with --press-to-photon/],
    [['--live', '--input-only', '--rapid-click-burst'], /cannot be combined with .*--rapid-click-burst/],
    [['--check-only', '--input-only'], /--check-only cannot be combined/],
    [
      ['--live', '--input-only', '--json', '/tmp/rc-results.json'],
      /must contain 'input-only'/,
    ],
  ];
  for (const [args, expected] of cases) {
    const result = spawnSync(process.execPath, [loopbackPath, ...args], {
      cwd: repoRoot,
      encoding: 'utf8',
      env: { ...process.env, PETAL_AUTOTEST_SOCK: '' },
    });
    assert.notEqual(result.status, 0, `${args.join(' ')} must be refused`);
    assert.match(`${result.stdout}${result.stderr}`, expected, args.join(' '));
  }
});
