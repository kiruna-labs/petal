#!/usr/bin/env node
// Score a camera-freeze soak (#456): apps/desktop/scripts/camera-freeze-soak.mjs
// writes camera-freeze-raw.json; this turns it into per-viewer freeze numbers,
// the sender's encoder trajectory, and counts of the sender-log lines that
// mark encoder reconfiguration.
//
//   node scripts/ci/camera-freeze-summary.mjs --raw <camera-freeze-raw.json>
//     [--sender-log <petal log>] [--json <out.json>] [--markdown <out.md>]
//
// REPORT-ONLY: prints a verdict line ("would fail" when any viewer shows a
// presented-frame gap over GAP_FAIL_MS, a 16 s window decoding under
// LOW_DECODED_FPS, or a concurrent share stalling >= 5 s), but
// always exits 0 unless the raw file is unusable (exit 2).
import fs from 'node:fs';
import process from 'node:process';

export const GAP_FAIL_MS = 1000;
export const WINDOW_MS = 16_000;
export const LOW_FPS = 24;
// Decoder-side floor for a 16 s window. The rVFC rate of a headless viewer on
// the 8-CPU VM sits at 23-25 fps even for a clean 30 fps stream, so the verdict
// uses the receiver's own framesDecoded rate (the field camera-health signal).
export const LOW_DECODED_FPS = 20;

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) args[argv[i].replace(/^--/, '')] = argv[i + 1];
  return args;
}

const DELTA_FIELDS = ['pliCount', 'firCount', 'keyFramesDecoded', 'framesDropped', 'freezeCount', 'totalFreezesDuration',
  'packetsLost', 'nackCount', 'framesDecoded', 'framesReceived'];

export function summarizeViewer(record, startMs, endMs) {
  const frames = record.frames.filter(([t]) => t >= startMs && t <= endMs).map((f) => f[0]).sort((a, b) => a - b);
  const gaps = [];
  for (let i = 1; i < frames.length; i += 1) gaps.push(frames[i] - frames[i - 1]);
  const spanMs = endMs - startMs;
  const windows = [];
  for (let w = startMs; w + WINDOW_MS <= endMs; w += WINDOW_MS) {
    const n = frames.filter((t) => t >= w && t < w + WINDOW_MS).length;
    windows.push(Math.round((n / (WINDOW_MS / 1000)) * 10) / 10);
  }
  const stats = record.stats.filter((s) => s.t >= startMs && s.t <= endMs && s.framesDecoded !== undefined);
  const deltas = {};
  if (stats.length >= 2) {
    const a = stats[0];
    const b = stats[stats.length - 1];
    for (const f of DELTA_FIELDS) if (typeof a[f] === 'number' && typeof b[f] === 'number') deltas[f] = Math.round((b[f] - a[f]) * 1000) / 1000;
  }
  const dims = [];
  for (const s of stats) {
    const d = `${s.frameWidth ?? '?'}x${s.frameHeight ?? '?'}`;
    if (dims[dims.length - 1] !== d) dims.push(d);
  }
  const frameDims = [];
  for (const f of record.frames) {
    if (f[0] < startMs || f[0] > endMs) continue;
    const d = `${f[2]}x${f[3]}`;
    if (frameDims[frameDims.length - 1] !== d) frameDims.push(d);
  }
  const decodedWindows = [];
  for (let w = startMs; w + WINDOW_MS <= endMs; w += WINDOW_MS) {
    const inWindow = stats.filter((s) => s.t >= w && s.t <= w + WINDOW_MS);
    if (inWindow.length < 2) continue;
    const a = inWindow[0];
    const b = inWindow[inWindow.length - 1];
    decodedWindows.push(Math.round(((b.framesDecoded - a.framesDecoded) / ((b.t - a.t) / 1000)) * 10) / 10);
  }
  const avail = stats.map((s) => s.availInKbps).filter((v) => typeof v === 'number');
  return {
    label: record.label,
    frames: frames.length,
    fps: Math.round((frames.length / (spanMs / 1000)) * 10) / 10,
    gapsOver200: gaps.filter((g) => g > 200).length,
    gapsOver500: gaps.filter((g) => g > 500).length,
    gapsOver1000: gaps.filter((g) => g > 1000).length,
    maxGapMs: Math.round(Math.max(0, ...gaps)),
    frozenMs: Math.round(gaps.filter((g) => g > 200).reduce((sum, g) => sum + g - 33, 0)),
    windowsFps: windows,
    windowsUnder24: windows.filter((f) => f < LOW_FPS).length,
    decodedFps: deltas.framesDecoded !== undefined ? Math.round((deltas.framesDecoded / (spanMs / 1000)) * 10) / 10 : null,
    decodedWindowsFps: decodedWindows,
    decodedWindowsLow: decodedWindows.filter((f) => f < LOW_DECODED_FPS).length,
    statDeltas: deltas,
    statDims: dims,
    presentedDims: frameDims,
    resolutionChanges: Math.max(0, frameDims.length - 1),
    availInKbpsMedian: avail.length ? avail.sort((x, y) => x - y)[Math.floor(avail.length / 2)] : null,
    lagEvents: record.lag.length,
  };
}

// The concurrent window share (--with-share). With a static share the
// source repaints only every shareStaticMs, so a "stall" is a gap well past
// that; >= 5 s matches the native stats stall the field counts.
export function summarizeShare(record, startMs, endMs, shareStaticMs) {
  const frames = (record.shareFrames ?? []).filter(([t]) => t >= startMs && t <= endMs).map((f) => f[0]).sort((a, b) => a - b);
  if (!frames.length && !(record.shareStats ?? []).length) return null;
  const gaps = [];
  for (let i = 1; i < frames.length; i += 1) gaps.push(frames[i] - frames[i - 1]);
  const stallMs = Math.max(2000, (shareStaticMs || 33) + 1500);
  const stats = (record.shareStats ?? []).filter((s) => s.t >= startMs && s.t <= endMs && s.framesDecoded !== undefined);
  const deltas = {};
  if (stats.length >= 2) {
    for (const f of DELTA_FIELDS) {
      if (typeof stats[0][f] === 'number' && typeof stats[stats.length - 1][f] === 'number') deltas[f] = Math.round((stats[stats.length - 1][f] - stats[0][f]) * 1000) / 1000;
    }
  }
  return {
    label: record.label,
    frames: frames.length,
    stallThresholdMs: stallMs,
    stalls: gaps.filter((g) => g > stallMs).length,
    stallsOver5s: gaps.filter((g) => g >= 5000).length,
    maxGapMs: Math.round(Math.max(0, ...gaps)),
    statDeltas: deltas,
  };
}

export function summarizeSender(rows, startMs, endMs) {
  const camera = rows.filter((r) => String(r.name || '').includes('camera') && r.t >= startMs && r.t <= endMs);
  const ql = {};
  const dims = [];
  for (const r of camera) {
    ql[r.quality_limitation || '?'] = (ql[r.quality_limitation || '?'] || 0) + 1;
    const d = `${r.width}x${r.height}`;
    if (dims[dims.length - 1] !== d) dims.push(d);
  }
  const fps = camera.map((r) => r.fps).filter((v) => typeof v === 'number');
  const first = camera[0];
  const last = camera[camera.length - 1];
  const delta = (k) => (first && last && typeof first[k] === 'number' ? last[k] - first[k] : null);
  return {
    samples: camera.length,
    trackNames: [...new Set(camera.map((r) => r.name))],
    dims,
    resolutionChanges: Math.max(0, dims.length - 1),
    qualityLimitation: ql,
    fpsMean: fps.length ? Math.round((fps.reduce((a, b) => a + b, 0) / fps.length) * 10) / 10 : null,
    fpsMin: fps.length ? Math.min(...fps) : null,
    targetKbpsLast: last?.target_kbps ?? null,
    keyFramesEncoded: delta('key_frames_encoded'),
    framesEncoded: delta('frames_encoded'),
    pliReceived: delta('pli_count'),
    firReceived: delta('fir_count'),
    codecImpl: last?.codec_impl ?? null,
    otherSendTracks: [...new Set(rows.filter((r) => !String(r.name || '').includes('camera')).map((r) => r.name))],
  };
}

export const LOG_PATTERNS = {
  maximalAllowedFrameRate: /maximal allowed frame rate (\d+)/,
  vtSessionCreated: /PrioritizeEncodingSpeedOverQuality/,
  h264EncodeFailed: /H264 encode failed/,
  cameraPublished: /Published camera track/,
  cameraRecreated: /re-creat|recreat/i,
  encoderReinit: /InitEncode|uninitialized fallback wrapper/,
  frameSizeMismatch: /camera frame size .* != published/,
};

export function scanSenderLog(text) {
  const out = {};
  for (const [key, re] of Object.entries(LOG_PATTERNS)) out[key] = 0;
  const allowed = {};
  const published = [];
  for (const line of text.split('\n')) {
    for (const [key, re] of Object.entries(LOG_PATTERNS)) {
      const m = line.match(re);
      if (!m) continue;
      // The log dedupes repeats: "[repeated 6x in the last 32s ...]" stands
      // for six more occurrences.
      const repeated = line.match(/\[repeated (\d+)x/);
      out[key] += 1 + (repeated ? Number(repeated[1]) : 0);
      if (key === 'maximalAllowedFrameRate') allowed[m[1]] = (allowed[m[1]] || 0) + 1;
      if (key === 'cameraPublished' && published.length < 5) published.push(line.slice(0, 400));
    }
  }
  return { counts: out, maximalAllowedFrameRateValues: allowed, cameraPublishLines: published };
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  let raw;
  try {
    raw = JSON.parse(fs.readFileSync(args.raw, 'utf8'));
  } catch (error) {
    console.log(`::error::camera-freeze summary: cannot read ${args.raw}: ${error.message}`);
    process.exit(2);
  }
  const startMs = raw.measureStartMs;
  const endMs = raw.measureEndMs;
  const report = {
    infra: raw.infra,
    fatal: raw.fatal,
    withShare: raw.withShare,
    host: raw.host,
    measuredMs: startMs && endMs ? endMs - startMs : 0,
    viewers: [],
    sender: null,
    senderLog: null,
  };
  if (startMs && endMs) {
    report.viewers = raw.viewers.map((v) => summarizeViewer(v, startMs, endMs));
    report.sender = summarizeSender(raw.sender, startMs, endMs);
    report.share = raw.withShare ? raw.viewers.map((v) => summarizeShare(v, startMs, endMs, raw.shareStaticMs)) : [];
  }
  if (args['sender-log'] && fs.existsSync(args['sender-log'])) report.senderLog = scanSenderLog(fs.readFileSync(args['sender-log'], 'utf8'));
  const failing = report.viewers.filter((v) => v.gapsOver1000 > 0 || v.decodedWindowsLow > 0);
  const shareStalled = (report.share ?? []).filter((v) => v && v.stallsOver5s > 0);
  report.verdict = report.infra.length || !report.viewers.length ? 'INFRA-INVALID' : failing.length || shareStalled.length ? 'WOULD-FAIL' : 'PASS';

  const lines = [];
  lines.push(`### Camera-freeze soak: ${report.verdict} (report-only)`);
  lines.push('');
  lines.push(`measured ${Math.round(report.measuredMs / 1000)} s, with share: ${report.withShare}, host macOS ${raw.host?.macos}`);
  lines.push('');
  lines.push('| viewer | presented fps | decoded fps | gaps >500ms | gaps >1s | max gap ms | 16s windows decoded <20fps | PLI | FIR | keyframes | dims (stats) |');
  lines.push('|---|---|---|---|---|---|---|---|---|---|---|');
  for (const v of report.viewers) {
    lines.push(`| ${v.label} | ${v.fps} | ${v.decodedFps} | ${v.gapsOver500} | ${v.gapsOver1000} | ${v.maxGapMs} | ${v.decodedWindowsLow}/${v.decodedWindowsFps.length} | ${v.statDeltas.pliCount ?? '?'} | ${v.statDeltas.firCount ?? '?'} | ${v.statDeltas.keyFramesDecoded ?? '?'} | ${v.statDims.join(' > ')} |`);
  }
  if (report.share?.some(Boolean)) {
    lines.push('');
    lines.push(`| share viewer (static ${raw.shareStaticMs || 0} ms) | frames | stalls > threshold | stalls >= 5s | max gap ms | PLI | keyframes |`);
    lines.push('|---|---|---|---|---|---|---|');
    for (const v of report.share.filter(Boolean)) {
      lines.push(`| ${v.label} | ${v.frames} | ${v.stalls} (>${v.stallThresholdMs}ms) | ${v.stallsOver5s} | ${v.maxGapMs} | ${v.statDeltas.pliCount ?? '?'} | ${v.statDeltas.keyFramesDecoded ?? '?'} |`);
    }
  }
  if (report.sender) {
    const s = report.sender;
    lines.push('');
    lines.push(`sender camera: fps mean ${s.fpsMean} min ${s.fpsMin}, dims ${s.dims.join(' > ')}, quality limitation ${JSON.stringify(s.qualityLimitation)}, keyframes encoded ${s.keyFramesEncoded}, PLI received ${s.pliReceived}, target ${s.targetKbpsLast} kbps, encoder ${s.codecImpl}`);
  }
  if (report.senderLog) {
    lines.push('');
    lines.push(`sender log: ${JSON.stringify(report.senderLog.counts)} frame-rate clamps ${JSON.stringify(report.senderLog.maximalAllowedFrameRateValues)}`);
  }
  for (const problem of report.infra) lines.push(`- infra: ${problem}`);
  const md = lines.join('\n');
  console.log(md);
  if (args.json) fs.writeFileSync(args.json, `${JSON.stringify(report, null, 2)}\n`);
  if (args.markdown) fs.writeFileSync(args.markdown, `${md}\n`);
  if (report.verdict === 'INFRA-INVALID') process.exitCode = 2;
}

if (import.meta.url === `file://${process.argv[1]}`) main();
