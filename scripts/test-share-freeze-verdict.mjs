#!/usr/bin/env node
// Unit tests for scripts/ci/share-freeze-verdict.mjs (#391), both directions:
// a healthy soak passes, an induced sender pause fails as a CORRELATED freeze,
// a lone receiver stall fails as single-viewer, a source stall is not counted,
// and every way the run can fail to measure is INFRA-INVALID, never a pass.
// The last block drives the e2e-gate-verdict CLI the way nightly-loopback.yml does.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import { FREEZE_GAP_THRESHOLD_MS, correlateFreezes, freezeVerdict, gapHistogram, parseSenderLog, sourceActivity } from './ci/share-freeze-verdict.mjs';

// The sharer log's real line shape (logging.rs: UTC "YYYY-MM-DD HH:MM:SS.mmm [LEVEL] [target] msg").
function logLine(ms, message, level = 'WARN') {
  const iso = new Date(ms).toISOString();
  return `${iso.slice(0, 10)} ${iso.slice(11, 23)} [${level}] [desktop_lib::session::share] ${message}`;
}

const CLI = fileURLToPath(new URL('./ci/e2e-gate-verdict.mjs', import.meta.url));
const T0 = 1_800_000_000_000;
const DURATION = 120_000;

// A soak where the source repaints every 33ms and each viewer presents every
// frame ~80ms later. `holes` drop a viewer's frames over [from, to) (relative
// ms); `sourceHoles` stop the source's repaints the same way.
function soak({ viewers = 2, holes = {}, sourceHoles = [], lag = {}, senderFps = () => 30, patch = {} } = {}) {
  const start = T0 + 20_000;
  const end = start + DURATION;
  const ticks = [];
  let tick = 0;
  for (let t = T0; t <= end + 1000; t += 33) {
    const rel = t - start;
    if (sourceHoles.some(([a, b]) => rel >= a && rel < b)) continue;
    tick += 1;
    ticks.push([tick, t]);
  }
  const records = [];
  for (let v = 0; v < viewers; v += 1) {
    const label = `freeze-viewer-${v + 1}`;
    const frames = [];
    for (const [k, t] of ticks) {
      const shown = t + 80;
      const rel = shown - start;
      if ((holes[label] ?? []).some(([a, b]) => rel >= a && rel < b)) continue;
      frames.push([shown, k % 65536, (k * 2970) >>> 0, 960, 600, k]);
    }
    const stats = [];
    for (let t = start - 2000, i = 0; t <= end; t += 2000, i += 1) {
      stats.push({ t, subscribed: true, framesDecoded: i * 60, framesDropped: 0, freezeCount: 0, totalFreezesDuration: 0, packetsLost: 0, pliCount: 0, nackCount: 0, jitterBufferDelay: i * 60 * 0.03, jitterBufferEmittedCount: i * 60, rttMs: 40, frameWidth: 960, frameHeight: 600 });
    }
    records.push({ label, identity: `p-viewer-${v}`, firstFrameAtMs: frames[0]?.[0] ?? null, frames, stats, lag: lag[label] ?? [], events: [] });
  }
  const sender = [];
  for (let t = start - 2000; t <= end; t += 2000) {
    sender.push({ t, track: true, fps: senderFps(t - start), width: 960, height: 600, quality_limitation: 'none', actual_kbps: 1500, key_frames_encoded: 3, pli_count: 0, nack_count: 0 });
  }
  return {
    schema: 1,
    harnessUrl: 'https://meet.petal.live',
    requestedDurationMs: DURATION,
    viewerCount: viewers,
    measureStartMs: start,
    measureEndMs: end,
    sharer: { identity: 'p-freezesoak-1', windowId: 7 },
    viewers: records,
    sender,
    fixture: { ticks },
    infra: [],
    fatal: [],
    ...patch,
  };
}

test('the threshold is the single named constant, 500ms', () => {
  assert.equal(FREEZE_GAP_THRESHOLD_MS, 500);
});

test('a healthy soak passes and reports per-viewer histograms', () => {
  const v = freezeVerdict({ raw: soak() });
  assert.equal(v.outcome, 'pass', v.errors.join('\n'));
  assert.equal(v.report.viewers.length, 2);
  for (const viewer of v.report.viewers) {
    assert.equal(viewer.freezes.length, 0);
    assert.ok(viewer.fps > 29 && viewer.fps < 31, `fps ${viewer.fps}`);
    assert.equal(viewer.decodedTickFraction, 1);
    const counted = Object.values(viewer.gapHistogram).reduce((a, b) => a + b, 0);
    assert.equal(counted, viewer.presentedFrames + 1, 'every gap, window edges included, is bucketed');
    assert.equal(viewer.gapHistogram['<50'] + viewer.gapHistogram['50-100'], counted);
  }
  assert.equal(v.report.clusters.length, 0);
  assert.ok(Math.abs(v.report.latency.p50Ms - 80) < 1, `latency ${v.report.latency.p50Ms}`);
});

test('a sender pause every viewer sees is a CORRELATED freeze and fails', () => {
  const hole = [[60_000, 62_000]];
  const v = freezeVerdict({
    raw: soak({ holes: { 'freeze-viewer-1': hole, 'freeze-viewer-2': hole }, senderFps: (rel) => (rel >= 60_000 && rel < 62_000 ? 0 : 30) }),
  });
  assert.equal(v.outcome, 'fail');
  assert.equal(v.report.clusters.length, 1);
  const [cluster] = v.report.clusters;
  assert.equal(cluster.scope, 'all-viewers');
  assert.equal(cluster.correlated, true);
  assert.match(cluster.attribution, /^sender: the encoder stopped/);
  assert.equal(v.report.correlatedFreezes, 1);
  const freeze = v.report.viewers[0].freezes[0];
  assert.ok(freeze.durationMs >= 2000 && freeze.durationMs < 2100, `duration ${freeze.durationMs}`);
  assert.ok(freeze.sourceTickDelta >= 60, 'the source kept changing across the gap');
  assert.match(v.errors.join('\n'), /FREEZE all-viewers/);
});

test('all viewers freezing while the sender keeps encoding points at the SFU path', () => {
  const hole = [[30_000, 31_000]];
  const v = freezeVerdict({ raw: soak({ holes: { 'freeze-viewer-1': hole, 'freeze-viewer-2': [[30_100, 30_900]] } }) });
  assert.equal(v.outcome, 'fail');
  assert.equal(v.report.clusters[0].scope, 'all-viewers');
  assert.match(v.report.clusters[0].attribution, /^sender-or-sfu/);
});

test('a correlated freeze next to a silent-SCK line on the sharer is attributed to capture', () => {
  const hole = [[40_000, 42_000]];
  const at = T0 + 20_000 + 39_500;
  const log = [
    logLine(at, 'capture-pull: window 7 ENGAGED -- raw stream silent 1.5s, pulling snapshots at <= 10fps'),
    logLine(at + 100, 'capture-diag: window 7 seq=900 ~2.0fps status=Complete dirty=1rects/10px hash=00 unchanged_run=0 occlusion=- mem=- -> ok', 'INFO'),
    logLine(T0 + 20_000 + 90_000, 'capture-diag: window 7 seq=2000 ~30.0fps status=Complete -> ok', 'INFO'),
  ].join('\n');
  const v = freezeVerdict({ raw: soak({ holes: { 'freeze-viewer-1': hole, 'freeze-viewer-2': hole } }), senderLogText: log });
  assert.equal(v.outcome, 'fail');
  assert.match(v.report.clusters[0].attribution, /^sender-capture: capture-pull-engaged/);
  assert.equal(v.report.clusters[0].senderCaptureFpsMin, 2);
  assert.deepEqual(v.report.senderCapture.eventCounts, { 'capture-pull-engaged': 1 });
  assert.equal(v.report.senderCapture.captureFpsP50, 2);
});

test('the induced red-proof pause is named as such', () => {
  const hole = [[60_000, 62_000]];
  const log = logLine(T0 + 20_000 + 60_000, 'TEST share pause: dropping every window-share frame for 2000ms (PETAL_TEST_SHARE_PAUSE, red proof only)');
  // Run 39838539435: both renderers also lagged ~315ms as frames resumed.
  const lagAt = T0 + 20_000 + 60_000;
  const raw = soak({ holes: { 'freeze-viewer-1': hole, 'freeze-viewer-2': hole }, lag: { 'freeze-viewer-1': [[lagAt, 319]], 'freeze-viewer-2': [[lagAt, 313]] } });
  const v = freezeVerdict({ raw, senderLogText: log });
  assert.equal(v.outcome, 'fail');
  assert.match(v.report.clusters[0].attribution, /induced PETAL_TEST_SHARE_PAUSE/);
});

test('parseSenderLog ignores untimestamped and unrelated lines', () => {
  const parsed = parseSenderLog(['garbage ENGAGED -- raw stream silent', logLine(T0, 'session: window 7 frame pump starting')].join('\n'));
  assert.deepEqual(parsed, { events: [], captureFps: [] });
});

test('one viewer freezing alone is a single-viewer (receiver-side) freeze', () => {
  const v = freezeVerdict({ raw: soak({ holes: { 'freeze-viewer-2': [[45_000, 45_700]] } }) });
  assert.equal(v.outcome, 'fail');
  assert.equal(v.report.clusters.length, 1);
  assert.equal(v.report.clusters[0].scope, 'single-viewer');
  assert.match(v.report.clusters[0].attribution, /receiver-side: only freeze-viewer-2/);
  assert.equal(v.report.receiverOnlyFreezes, 1);
});

test('a gap just under the threshold passes, just over fails', () => {
  // Frames every 33ms: a hole of 440ms leaves a ~473ms gap, 500ms a ~533ms gap.
  assert.equal(freezeVerdict({ raw: soak({ holes: { 'freeze-viewer-1': [[10_000, 10_440]] } }) }).outcome, 'pass');
  assert.equal(freezeVerdict({ raw: soak({ holes: { 'freeze-viewer-1': [[10_000, 10_500]] } }) }).outcome, 'fail');
});

test('a viewer that stops presenting before the end fails (the end edge counts)', () => {
  const v = freezeVerdict({ raw: soak({ holes: { 'freeze-viewer-1': [[DURATION - 5_000, DURATION + 10_000]] } }) });
  assert.equal(v.outcome, 'fail');
  assert.equal(v.report.viewers[0].freezes[0].edge, 'measure-end');
});

test('a gap where the SOURCE stopped repainting is not counted as a freeze', () => {
  const hole = [[50_000, 51_500]];
  const v = freezeVerdict({ raw: soak({ sourceHoles: hole }) });
  assert.equal(v.outcome, 'pass', v.errors.join('\n'));
  assert.equal(v.report.viewers[0].sourceStalls.length, 1);
  assert.match(v.warnings.join('\n'), /NOT counted: the source itself did not repaint/);
});

test('fixture AND every viewer page stalling together is a host-VM stall: reported, not counted', () => {
  // Run 39838535897's shape: fixture repaint gap 327ms, renderers 329/639ms.
  const rel = 70_000;
  const lagAt = T0 + 20_000 + rel;
  const raw = soak({
    holes: { 'freeze-viewer-1': [[rel, rel + 950]], 'freeze-viewer-2': [[rel, rel + 1000]] },
    sourceHoles: [[rel + 300, rel + 627]],
    lag: { 'freeze-viewer-1': [[lagAt, 329]], 'freeze-viewer-2': [[lagAt, 639]] },
  });
  const v = freezeVerdict({ raw });
  assert.equal(v.outcome, 'pass', v.errors.join('\n'));
  assert.equal(v.report.hostVmStalls, 1);
  assert.match(v.report.clusters[0].attribution, /^host-vm-stall/);
  assert.match(v.warnings.join('\n'), /NOT counted: all-viewers gap/);
});

test('renderer lag alone (source kept repainting) never excuses a correlated freeze', () => {
  const rel = 70_000;
  const lagAt = T0 + 20_000 + rel;
  const v = freezeVerdict({
    raw: soak({
      holes: { 'freeze-viewer-1': [[rel, rel + 2000]], 'freeze-viewer-2': [[rel, rel + 2000]] },
      lag: { 'freeze-viewer-1': [[lagAt, 319]], 'freeze-viewer-2': [[lagAt, 313]] },
    }),
  });
  assert.equal(v.outcome, 'fail');
  assert.equal(v.report.clusters[0].counted, true);
  assert.match(v.report.clusters[0].attribution, /^sender-or-sfu/);
});

test('more than two host-VM stalls in a window is INFRA-INVALID', () => {
  const holes = { 'freeze-viewer-1': [], 'freeze-viewer-2': [] };
  const sourceHoles = [];
  const lag = { 'freeze-viewer-1': [], 'freeze-viewer-2': [] };
  for (const rel of [20_000, 50_000, 80_000]) {
    holes['freeze-viewer-1'].push([rel, rel + 900]);
    holes['freeze-viewer-2'].push([rel, rel + 900]);
    sourceHoles.push([rel + 300, rel + 600]);
    lag['freeze-viewer-1'].push([T0 + 20_000 + rel, 400]);
    lag['freeze-viewer-2'].push([T0 + 20_000 + rel, 400]);
  }
  const v = freezeVerdict({ raw: soak({ holes, sourceHoles, lag }) });
  assert.equal(v.outcome, 'infra-invalid');
  assert.match(v.errors[0], /too noisy/);
});

test('the sharer dying is a FREEZE failure, not infra', () => {
  const v = freezeVerdict({ raw: soak({ patch: { fatal: ['sharer stopped answering the autotest socket'] } }) });
  assert.equal(v.outcome, 'fail');
  assert.match(v.errors.join('\n'), /FREEZE: sharer stopped answering/);
});

test('every way of measuring nothing is INFRA-INVALID and fails', () => {
  const cases = {
    'missing raw file': null,
    'token mint failed': { ...soak(), infra: ['native join to a fresh room failed (token mint / SFU connect): 403'], measureStartMs: null, measureEndMs: null },
    'no measurement window': { ...soak(), measureStartMs: null },
    'run cut short': { ...soak(), measureEndMs: T0 + 20_000 + 30_000 },
    'one viewer only': (() => { const r = soak(); r.viewers = r.viewers.slice(0, 1); return r; })(),
    'viewer never presented': (() => { const r = soak(); r.viewers[1].frames = []; r.viewers[1].firstFrameAtMs = null; return r; })(),
    'source never animated': { ...soak(), fixture: { ticks: [] } },
    'source too slow': (() => { const r = soak(); r.fixture.ticks = r.fixture.ticks.filter((_, i) => i % 5 === 0); return r; })(),
  };
  for (const [name, raw] of Object.entries(cases)) {
    const v = freezeVerdict({ raw });
    assert.equal(v.outcome, 'infra-invalid', `${name}: ${v.outcome} ${v.errors.join(' | ')}`);
    assert.equal(v.ok, false, name);
    assert.match(v.errors[0], /^INFRA-INVALID: /, name);
  }
});

test('helpers: histogram buckets, source activity, clustering slack', () => {
  assert.deepEqual(gapHistogram([10, 60, 499, 500, 501, 7000]), {
    '<50': 1, '50-100': 1, '100-200': 0, '200-300': 0, '300-500': 1, '500-1000': 2, '1000-2000': 0, '2000-5000': 0, '>=5000': 1,
  });
  const ticks = [[1, 0], [2, 33], [3, 66], [4, 1066], [5, 1099]];
  // A source gap that only touches the interval is clipped to the 100ms slack.
  assert.ok(sourceActivity(ticks, 0, 50).maxGapMs <= 100);
  assert.ok(sourceActivity(ticks, 200, 900).maxGapMs >= 700);
  const clusters = correlateFreezes([
    { label: 'a', freezes: [{ viewer: 'a', startMs: 1000, endMs: 1600, durationMs: 600 }] },
    { label: 'b', freezes: [{ viewer: 'b', startMs: 1800, endMs: 2400, durationMs: 600 }] },
  ], []);
  assert.equal(clusters.length, 1, 'gaps within the correlation slack are one event');
  assert.equal(clusters[0].scope, 'all-viewers');
});

test('CLI: freeze tier exits 0 on a healthy soak, 1 on a freeze and on infra, and writes the report', () => {
  const dir = mkdtempSync(join(tmpdir(), 'share-freeze-verdict-'));
  const run = (raw, name) => {
    const rawPath = join(dir, `${name}-raw.json`);
    const out = join(dir, `${name}-report.json`);
    if (raw) writeFileSync(rawPath, JSON.stringify(raw));
    const result = spawnSync(process.execPath, [CLI, 'freeze', '--raw', rawPath, '--json', out], { encoding: 'utf8' });
    return { status: result.status, stdout: result.stdout, report: JSON.parse(readFileSync(out, 'utf8')) };
  };
  const green = run(soak(), 'green');
  assert.equal(green.status, 0, green.stdout);
  assert.equal(green.report.outcome, 'pass');
  assert.match(green.stdout, /share-freeze outcome: PASS/);
  const red = run(soak({ holes: { 'freeze-viewer-1': [[60_000, 62_000]], 'freeze-viewer-2': [[60_000, 62_000]] } }), 'red');
  assert.equal(red.status, 1);
  assert.equal(red.report.outcome, 'fail');
  assert.match(red.stdout, /::error::FREEZE all-viewers/);
  const infra = run(null, 'missing');
  assert.equal(infra.status, 1);
  assert.equal(infra.report.outcome, 'infra-invalid');
  assert.match(infra.stdout, /::error::INFRA-INVALID/);
});
