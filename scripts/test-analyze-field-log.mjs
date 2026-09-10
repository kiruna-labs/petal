#!/usr/bin/env node
// Unit tests for scripts/analyze-field-log.mjs.
//
// The fixtures in scripts/fixtures/field-logs/ are read from disk on purpose:
// the real path (open, decode, stream, parse) is what a user runs, and a test
// that hand-feeds strings would not prove the file reading works at all.
//
// NOT covered here: the gzip path is exercised (fixture .gz built in-process),
// but a multi-gigabyte real log's throughput is not -- that was verified by
// hand against a 734k-line field log, see the PR.

import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

import {
  CAMERA_INTENT_FIRST_VERSION,
  PICKER_ICON_FIX_VERSION,
  PICKER_MARKS_FIRST_VERSION,
  analyzeFile,
  analyzeLines,
  buildVersion,
  cameraVerdict,
  compareVersions,
  concurrencyWarnings,
  crossFileWarnings,
  formatReport,
  parseBuildIdentity,
  parseMessage,
  parsePickerMark,
  parseTimestampMs,
  pickerVerdict,
  toJson,
} from './analyze-field-log.mjs';

const FIXTURES = join(fileURLToPath(new URL('.', import.meta.url)), 'fixtures', 'field-logs');
const fixture = (name) => join(FIXTURES, name);

let failures = 0;
function section(name) {
  console.log(`PASS: ${name}`);
}
function fail(message, error) {
  failures += 1;
  console.error(`FAIL: ${message}`);
  if (error) console.error(error);
}

async function run(name, body) {
  try {
    await body();
    section(name);
  } catch (error) {
    fail(name, error);
  }
}

// ---------------------------------------------------------------------------
// Line parsing
// ---------------------------------------------------------------------------

await run('parseTimestampMs / parseMessage read the real fern line shape', () => {
  const line =
    '2026-09-10 12:00:12.482 [INFO] [desktop_lib::camera_session] session: start_camera_publish begin';
  assert.equal(parseTimestampMs(line), Date.UTC(2026, 8, 10, 12, 0, 12) + 482);
  assert.equal(parseMessage(line), 'session: start_camera_publish begin');
  // A continuation line of a multi-line message has no timestamp and must not
  // be mistaken for one.
  assert.equal(parseTimestampMs('    at some::frame (lib.rs:12)'), null);
  assert.equal(parseTimestampMs(''), null);
  assert.equal(parseTimestampMs(null), null);
  // A target containing "] " must not truncate the message early.
  assert.equal(
    parseMessage('2026-09-10 12:00:12.482 [WARN] [libwebrtc] (Enc.mm:614): frame rate 29'),
    '(Enc.mm:614): frame rate 29'
  );
});

await run('compareVersions is numeric, not lexicographic', () => {
  assert.ok(compareVersions('0.9.4', '0.9.11') < 0);
  assert.equal(compareVersions('0.9.21', '0.9.21'), 0);
  assert.ok(compareVersions('0.9.21', '0.9.20') > 0);
  assert.equal(compareVersions('not-a-version', '0.9.0'), null);
});

await run('parseBuildIdentity and parsePickerMark read the emitted fields', () => {
  assert.deepEqual(
    parseBuildIdentity(
      'petal: startup build identity -- version=0.9.23 commit=605bd3cd build_date=2026-09-10 bundle_id=com.petal.app'
    ),
    { version: '0.9.23', commit: '605bd3cd' }
  );
  assert.equal(parseBuildIdentity('petal: app startup begin'), null);

  const mark = parsePickerMark(
    'window_source: picker memory mark -- stage=prewarm_done phys_footprint_mb=1492 ' +
      'live_pixel_buffers=0 sources=3d/18w thumbnail_captures=13 prewarm=completed ' +
      'vm_walk=complete vm_regions=512 vm_resident_mb=1380 vm_mapped_dirty_mb=1372 ' +
      'vm_top=iosurface:1300/1300,malloc:80/72 vm_other_tag=n/a'
  );
  assert.deepEqual(mark, {
    stage: 'prewarm_done',
    footprintMb: 1492,
    thumbnailCaptures: 13,
    displays: 3,
    windows: 18,
    prewarm: 'completed',
  });
  // `list_begin` deliberately carries no counts; it must read as null, never 0.
  const begin = parsePickerMark(
    'window_source: picker memory mark -- stage=list_begin phys_footprint_mb=127 ' +
      'live_pixel_buffers=0 sources=n/a thumbnail_captures=4 prewarm=pending vm_walk=unavailable'
  );
  assert.equal(begin.displays, null);
  assert.equal(begin.windows, null);
  assert.equal(begin.footprintMb, 127);
  assert.equal(parsePickerMark('window_source: list() refused'), null);
});

// ---------------------------------------------------------------------------
// Measurement 1 -- #76
// ---------------------------------------------------------------------------

await run('#76 fixture: first attempt wins, with ON / OFF / device-switch split out', async () => {
  const analysis = await analyzeFile(fixture('camera-first-attempt-wins.log'));
  const summary = cameraVerdict(analysis);
  assert.equal(summary.verdict, 'first-attempt-wins');
  assert.equal(summary.retries.length, 0);
  assert.equal(summary.failures.length, 0);

  const kinds = analysis.cameraEpisodes.map((episode) => episode.kind);
  assert.deepEqual(kinds, ['on', 'off', 'on', 'device-switch'], 'all three cases are separated');

  const [first] = analysis.cameraEpisodes;
  assert.equal(first.marginMs, 412, 'margin is intent -> outcome');
  assert.equal(first.releaseWindowMs, 2, 'release window is intent -> publish begin');
  assert.equal(first.attemptCount, 1);
  assert.equal(first.winningAttempt, 1);
  assert.equal(first.verdict, 'first-attempt-wins');

  const off = analysis.cameraEpisodes[1];
  assert.equal(off.stopReleaseMs, 38, 'the OFF case reports how long the release took');
  assert.equal(off.attemptCount, 0);

  const swap = analysis.cameraEpisodes[3];
  assert.equal(swap.kind, 'device-switch', 'a stop pair immediately before intent=true is a switch');
  assert.equal(swap.stopReleaseMs, 41);
  assert.equal(swap.marginMs, 498);
  assert.equal(swap.verdict, 'first-attempt-wins');

  const report = formatReport(analysis);
  assert.match(report, /VERDICT: first-attempt-wins/);
  assert.match(report, /device switch/);
  // A win is only evidence about the CONTENDED case if the log came from #76's
  // runbook; the preview holds the device from the webview and logs nothing.
  assert.match(report, /CAVEAT: nothing in a log says whether the Settings camera preview/);
});

await run('#76 fixture: a self-heal retry flips the verdict to retry-needed', async () => {
  const analysis = await analyzeFile(fixture('camera-retry-needed.log'));
  const summary = cameraVerdict(analysis);
  assert.equal(summary.verdict, 'retry-needed');
  assert.equal(summary.retries.length, 1);

  const [episode] = analysis.cameraEpisodes;
  assert.equal(episode.kind, 'on');
  assert.equal(episode.attemptCount, 3, 'the self-heal retries belong to the same episode');
  assert.equal(episode.winningAttempt, 3);
  assert.equal(episode.healHandoff, true);
  assert.equal(episode.healAttemptsFailed, 1);
  assert.equal(episode.marginMs, 11_880, 'margin runs to the attempt that actually won');
  assert.equal(episode.firstAttemptMs, 5007, 'the losing first attempt is still reported');
  assert.equal(episode.verdict, 'retry-needed');

  const report = formatReport(analysis);
  assert.match(report, /VERDICT: retry-needed/);
  assert.match(report, /bounded acknowledgement/, 'points at #76 step 3');
});

await run('#76: a publish with no intent line cannot set the verdict', async () => {
  // The real 0.9.4 field log this guard came from had three publishes DAYS
  // apart and no intent line at all. Folding them into one episode reported
  // "first-attempt-wins" for a build that cannot answer the question.
  const analysis = analyzeLines([
    '2026-09-02 15:41:00.000 [INFO] [desktop_lib] petal: startup build identity -- version=0.9.4 commit=66634904 build_date=2026-08-26 bundle_id=com.petal.app',
    "2026-09-02 15:43:20.877 [INFO] [desktop_lib::camera_session] session: start_camera_publish begin (identity '<redacted:i1>')",
    '2026-09-02 15:43:21.946 [INFO] [desktop_lib::camera_session] session: start_camera_publish succeeded (1920x1080)',
    "2026-09-02 17:31:32.490 [INFO] [desktop_lib::camera_session] session: start_camera_publish begin (identity '<redacted:i1>')",
    '2026-09-02 17:31:33.475 [INFO] [desktop_lib::camera_session] session: start_camera_publish succeeded (1920x1080)',
  ]);
  assert.equal(analysis.cameraEpisodes.length, 2, 'two separate publishes, not one 2-attempt one');
  assert.ok(analysis.cameraEpisodes.every((episode) => episode.kind === 'unattributed'));
  const summary = cameraVerdict(analysis);
  assert.equal(summary.verdict, 'inconclusive');
  assert.equal(summary.measurable.length, 0);
  assert.equal(summary.unattributed.length, 2);

  const report = formatReport(analysis);
  assert.match(report, /NOT COUNTED/);
  assert.match(report, new RegExp(`predates #78[\\s\\S]*${CAMERA_INTENT_FIRST_VERSION}`));
  assert.doesNotMatch(report, /VERDICT: first-attempt-wins/);
});

await run('#76: an episode never spans an app restart', () => {
  const analysis = analyzeLines([
    '2026-09-10 12:00:00.000 [INFO] [desktop_lib] petal: startup build identity -- version=0.9.23 commit=aaaaaaa build_date=2026-09-10 bundle_id=com.petal.app',
    '2026-09-10 12:00:10.000 [INFO] [desktop_lib::camera_session] session: camera-intent intended=true',
    "2026-09-10 12:00:10.004 [INFO] [desktop_lib::camera_session] session: start_camera_publish begin (identity '<redacted:i1>')",
    '2026-09-10 12:05:00.000 [INFO] [desktop_lib] petal: startup build identity -- version=0.9.23 commit=aaaaaaa build_date=2026-09-10 bundle_id=com.petal.app',
    '2026-09-10 12:05:30.000 [INFO] [desktop_lib::camera_session] session: start_camera_publish succeeded (1280x720)',
  ]);
  assert.equal(analysis.cameraEpisodes.length, 1, 'the restart closes it; nothing after joins it');
  const [crossed] = analysis.cameraEpisodes;
  assert.equal(crossed.crossedRestart, true);
  assert.equal(crossed.verdict, 'incomplete');
  assert.equal(crossed.marginMs, null, 'a 5-minute "margin" across a restart is never reported');
  assert.equal(cameraVerdict(analysis).verdict, 'inconclusive');
});

await run('#76: a user who turns the camera off mid-attempt is not a retry', () => {
  const analysis = analyzeLines([
    '2026-09-10 12:00:00.000 [INFO] [desktop_lib] petal: startup build identity -- version=0.9.23 commit=aaaaaaa build_date=2026-09-10 bundle_id=com.petal.app',
    '2026-09-10 12:00:10.000 [INFO] [desktop_lib::camera_session] session: camera-intent intended=true',
    "2026-09-10 12:00:10.004 [INFO] [desktop_lib::camera_session] session: start_camera_publish begin (identity '<redacted:i1>')",
    '2026-09-10 12:00:15.010 [WARN] [desktop_lib::camera_session] session: start_camera_publish failed: camera capture: first frame timed out',
    '2026-09-10 12:00:15.011 [WARN] [desktop_lib::camera_session] session: start_camera_publish_command immediate attempt failed, starting self-heal: camera capture: first frame timed out',
    '2026-09-10 12:00:16.000 [INFO] [desktop_lib::camera_session] session: camera-intent intended=false',
  ]);
  const [episode] = analysis.cameraEpisodes;
  assert.equal(episode.verdict, 'cancelled');
  assert.equal(episode.cancelledByUser, true);
  assert.equal(episode.marginMs, null, 'a cancelled episode has no margin');
  const summary = cameraVerdict(analysis);
  assert.equal(summary.verdict, 'inconclusive', 'a cancel is neither a win nor a retry');
  assert.equal(summary.retries.length, 0);
});

// ---------------------------------------------------------------------------
// Measurement 2 -- #159
// ---------------------------------------------------------------------------

await run('#159 fixture: pre-fix magnitude, split into enumeration and thumbnails', async () => {
  const analysis = await analyzeFile(fixture('picker-pre-fix.log'));
  assert.equal(buildVersion(analysis), PICKER_MARKS_FIRST_VERSION);
  const summary = pickerVerdict(analysis);
  assert.equal(summary.verdict, 'hundreds-of-mb');

  const [episode] = analysis.pickerEpisodes;
  assert.equal(episode.complete, true);
  assert.equal(episode.displays, 3);
  assert.equal(episode.windows, 18);
  assert.equal(episode.enumerationMb, 1370, 'the half #148 fixed');
  assert.equal(episode.thumbnailMb, 18, 'the half #159 says has never been priced');
  assert.equal(episode.totalMb, 1388);
  assert.equal(episode.captures, 13);
  assert.equal(episode.enumerationDurationMs, 812);
  assert.equal(episode.thumbnailDurationMs, 1901);
  assert.ok(Math.abs(episode.mbPerWindow - 77.1) < 0.1, 'matches #106 at ~70 MB per window');

  const report = formatReport(analysis);
  assert.match(report, /PREDATES/, 'a pre-0.9.21 build must be labelled as pre-fix');
  assert.match(report, /reopen #106/);
});

await run('#159 fixture: post-fix magnitude, and the thumbnail half now dominates', async () => {
  const analysis = await analyzeFile(fixture('picker-post-fix.log'));
  assert.equal(buildVersion(analysis), PICKER_ICON_FIX_VERSION);
  const summary = pickerVerdict(analysis);
  assert.equal(summary.verdict, 'tens-of-mb');

  const [episode] = analysis.pickerEpisodes;
  assert.equal(episode.enumerationMb, 5, '#148 took the per-window icon cost out');
  assert.equal(episode.thumbnailMb, 18);
  assert.equal(episode.totalMb, 23);
  assert.ok(episode.thumbnailMb > episode.enumerationMb, 'the unpriced half is now the larger one');

  const report = formatReport(analysis);
  assert.doesNotMatch(report, /PREDATES/);
  assert.match(report, /never been priced -- is 78% of it/);
});

await run('#159: an episode that captured nothing never sets the verdict', () => {
  const mark = (stage, mb, extra) =>
    `2026-09-10 10:00:0${stage === 'list_begin' ? '0' : stage === 'list_done' ? '1' : '2'}.000 ` +
    '[INFO] [desktop_lib::window_source] window_source: picker memory mark -- ' +
    `stage=${stage} phys_footprint_mb=${mb} live_pixel_buffers=0 ${extra} vm_walk=complete`;
  const analysis = analyzeLines([
    '2026-09-10 10:00:00.000 [INFO] [desktop_lib] petal: startup build identity -- version=0.9.23 commit=aaaaaaa build_date=2026-09-10 bundle_id=com.petal.app',
    mark('list_begin', 200, 'sources=n/a thumbnail_captures=9 prewarm=pending'),
    mark('list_done', 900, 'sources=3d/18w thumbnail_captures=9 prewarm=pending'),
    mark('prewarm_done', 905, 'sources=3d/18w thumbnail_captures=9 prewarm=skipped_in_flight'),
  ]);
  assert.equal(analysis.pickerEpisodes.length, 1);
  assert.equal(analysis.pickerEpisodes[0].prewarm, 'skipped_in_flight');
  const summary = pickerVerdict(analysis);
  assert.equal(summary.verdict, 'inconclusive', 'a 705 MB delta that priced no burst is not a reading');
  assert.match(formatReport(analysis), /skipped_in_flight/);
});

await run('#159: a lone list_begin reports as incomplete rather than as a burst', () => {
  const analysis = analyzeLines([
    '2026-09-10 10:00:00.000 [INFO] [desktop_lib::window_source] window_source: picker memory mark -- stage=list_begin phys_footprint_mb=200 live_pixel_buffers=0 sources=n/a thumbnail_captures=0 prewarm=pending vm_walk=complete',
    '2026-09-10 10:00:00.100 [WARN] [desktop_lib::window_source] window_source: list() refused',
  ]);
  assert.equal(analysis.pickerEpisodes.length, 1);
  assert.equal(analysis.pickerEpisodes[0].complete, false);
  assert.equal(pickerVerdict(analysis).verdict, 'inconclusive');
});

// ---------------------------------------------------------------------------
// The lines-absent path -- this has to be helpful, not a stack trace
// ---------------------------------------------------------------------------

await run('a log with neither instrumentation set reports why, and does not throw', async () => {
  const analysis = await analyzeFile(fixture('no-instrumentation.log'));
  assert.equal(analysis.cameraEpisodes.length, 0);
  assert.equal(analysis.pickerEpisodes.length, 0);
  assert.equal(cameraVerdict(analysis).verdict, 'no-data');
  assert.equal(pickerVerdict(analysis).verdict, 'no-data');

  const report = formatReport(analysis);
  assert.match(report, /NO DATA/);
  // It must say the build CANNOT have the lines -- "absent" is not "broken".
  assert.match(report, /build 0\.9\.4; the intent line first shipped in 0\.9\.10/);
  assert.match(report, /build 0\.9\.4; the picker marks first shipped in 0\.9\.20/);
  // And it must say what to do next, for a user who is not a developer.
  assert.match(report, /Open Settings so the camera preview is live/);
  assert.match(report, /Open the source picker and leave it open a few seconds/);
});

await run('a log with no timestamped line at all still produces a report', () => {
  const analysis = analyzeLines(['not a petal log', '']);
  const report = formatReport(analysis);
  assert.match(report, /NO DATA/);
  assert.match(report, /build: not stated in this log/);
  assert.doesNotMatch(report, /NaN/);
  assert.doesNotMatch(report, /undefined/);
});

// ---------------------------------------------------------------------------
// Concurrent instances -- the hazard that makes a hand-read wrong
// ---------------------------------------------------------------------------

await run('two builds in one file are flagged, and their episodes stay separate', async () => {
  const analysis = await analyzeFile(fixture('concurrent-instances.log'));
  const warnings = concurrencyWarnings(analysis);
  assert.equal(warnings.length, 2, 'both the mixed builds and the backwards clock are reported');
  assert.match(warnings[0], /2 DIFFERENT builds/);
  assert.match(warnings[0], /0\.9\.4/);
  assert.match(warnings[0], /0\.9\.9/);
  assert.match(warnings[1], /jump backwards/);
  assert.match(formatReport(analysis), /DIFFERENT builds/);
});

await run('cross-file: overlap is flagged, a rotation handover is not', () => {
  const at = (stamp) => Date.parse(`${stamp}Z`);
  const rotatedA = { file: 'a.log', firstMs: at('2026-09-09T00:00:00'), lastMs: at('2026-09-09T15:02:41'), builds: [] };
  const rotatedB = { file: 'b.log', firstMs: at('2026-09-09T15:02:41'), lastMs: at('2026-09-09T15:12:50'), builds: [] };
  assert.deepEqual(crossFileWarnings([rotatedA, rotatedB]), [], 'a handover shares one instant');

  const concurrent = { file: 'c.log', firstMs: at('2026-09-09T14:00:00'), lastMs: at('2026-09-09T16:00:00'), builds: [] };
  const overlapping = crossFileWarnings([rotatedA, concurrent]);
  assert.equal(overlapping.length, 1);
  assert.match(overlapping[0], /two app instances were running at once/);

  const mixedBuilds = crossFileWarnings([
    { file: 'a.log', firstMs: 0, lastMs: 1, builds: [{ version: '0.9.4', commit: 'a' }] },
    { file: 'b.log', firstMs: 10, lastMs: 11, builds: [{ version: '0.9.11', commit: 'b' }] },
  ]);
  assert.equal(mixedBuilds.length, 1);
  assert.match(mixedBuilds[0], /0\.9\.4, 0\.9\.11/, 'versions sort numerically, not lexically');
});

// ---------------------------------------------------------------------------
// Privacy -- these logs come from users
// ---------------------------------------------------------------------------

await run('no user content reaches the output, even from an UNREDACTED log', () => {
  // A raw field log (not exported through `redact_for_export`) carries the
  // LiveKit identity on the publish line and the home path on the sink line.
  const analysis = analyzeLines([
    '2026-09-10 12:00:00.000 [INFO] [desktop_lib::logging] logging: file sink initialized at /Users/eric.sauser/Library/Logs/Petal/petal.log',
    '2026-09-10 12:00:00.145 [INFO] [desktop_lib] petal: startup build identity -- version=0.9.23 commit=605bd3cd build_date=2026-09-10 bundle_id=com.petal.app',
    "2026-09-10 12:00:04.010 [INFO] [desktop_lib::session::room] session: joined room 'eng-standup'",
    "2026-09-10 12:00:05.000 [INFO] [desktop_lib::window_diag] window-stack: z=3 id=4675 owner='Google Chrome' name='Inbox - alice@example.com - Mail' layer=0",
    '2026-09-10 12:00:12.480 [INFO] [desktop_lib::camera_session] session: camera-intent intended=true',
    "2026-09-10 12:00:12.482 [INFO] [desktop_lib::camera_session] session: start_camera_publish begin (identity '094fbbc8-cbfb-4b6b-af29-06ab09d4c3fb')",
    '2026-09-10 12:00:12.892 [INFO] [desktop_lib::camera_session] session: start_camera_publish succeeded (1280x720)',
  ]);
  const surfaces = [formatReport(analysis), JSON.stringify(toJson(analysis))];
  const secrets = [
    'eric.sauser',
    '/Users/',
    'eng-standup',
    'Google Chrome',
    'alice@example.com',
    'Inbox',
    '094fbbc8',
  ];
  for (const surface of surfaces) {
    for (const secret of secrets) {
      assert.ok(!surface.includes(secret), `output must not contain ${secret}`);
    }
    // Absolute wall clock is withheld too: when a person used their computer is
    // an activity record. That covers the JSON's numbers, not just the text --
    // an epoch stamp is every bit as absolute as a formatted one.
    assert.ok(!surface.includes('2026-09-10 12:00'), 'absolute timestamps are withheld');
    assert.ok(
      !surface.includes(String(Date.UTC(2026, 8, 10, 12, 0, 12) + 482)),
      'no absolute epoch stamp survives into the output either'
    );
  }
  const episode = toJson(analysis).camera.episodes[0];
  assert.equal(episode.atMs, undefined, 'episodes carry an offset, not a wall-clock stamp');
  assert.equal(episode.offsetMs, 12_480);
  // The measurement still lands.
  assert.equal(cameraVerdict(analysis).verdict, 'first-attempt-wins');
  assert.equal(analysis.cameraEpisodes[0].marginMs, 412);
});

// ---------------------------------------------------------------------------
// Reading the file at all
// ---------------------------------------------------------------------------

await run('a gzipped log reads the same as the plain one', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'petal-field-log-test-'));
  const source = fixture('picker-post-fix.log');
  const gzPath = join(dir, 'petal.log.gz');
  writeFileSync(gzPath, gzipSync(readFileSync(source)));
  const [plain, gzipped] = await Promise.all([analyzeFile(source), analyzeFile(gzPath)]);
  assert.deepEqual(
    gzipped.pickerEpisodes,
    plain.pickerEpisodes,
    'gzip is a transport detail, not a different reading'
  );
});

await run('toJson carries the verdicts and the warnings', async () => {
  const analysis = await analyzeFile(fixture('camera-retry-needed.log'));
  const json = toJson(analysis);
  assert.equal(json.camera.verdict, 'retry-needed');
  assert.equal(json.picker.verdict, 'no-data');
  assert.equal(json.build, '0.9.23');
  assert.equal(json.file, 'camera-retry-needed.log');
  assert.ok(Array.isArray(json.warnings));
});

if (failures > 0) {
  console.error(`\n${failures} test(s) failed.`);
  process.exitCode = 1;
} else {
  console.log('\nanalyze-field-log: all tests passed.');
}
