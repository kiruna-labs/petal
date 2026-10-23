// Share-freeze soak verdict (#391): the `freeze` tier of e2e-gate-verdict.mjs.
//
// Input: share-freeze-raw.json from apps/desktop/scripts/share-freeze-soak.mjs
// -- a native window share held open on a real LiveKit deployment, watched by
// two or more independent web viewers that record every PRESENTED frame
// (requestVideoFrameCallback) plus inbound-rtp stats, the sender's outbound
// stats, and the animated source's own repaint log.
//
// Three outcomes, never a pass by absence:
//   pass           every viewer presented frames with no gap over the
//                  threshold while the source was changing
//   fail           at least one such gap (a FREEZE), or the sharer died
//   infra-invalid  the run could not measure (no join/token, no frames, a
//                  dead viewer, a source that did not animate, too short a
//                  run). Fails the gate too, under its own name.
//
// The correlated-freeze analysis is the point of the report: a freeze every
// viewer saw at once is sender/SFU side; one viewer alone is receiver side.

/// The single knob: a presented-frame gap longer than this, while the source
/// was changing, is a freeze. Users notice ~500ms; normal 30fps is ~33ms.
export const FREEZE_GAP_THRESHOLD_MS = 500;
/// The source must repaint at least this often for a gap to mean anything.
const SOURCE_CHANGE_BUDGET_MS = 100;
/// More than this fraction of the window lost to SOURCE stalls = no valid run.
const MAX_SOURCE_STALL_FRACTION = 0.1;
/// A run shorter than this fraction of the requested duration measured too little.
const MIN_MEASURED_FRACTION = 0.9;
/// Freeze intervals on different viewers this close count as the same event.
const CORRELATION_SLACK_MS = 250;
/// A viewer page whose event loop stalled this long was itself stalled.
const VIEWER_LAG_STALL_MS = 300;
/// A source repaint gap this long inside an all-viewer freeze, with every
/// viewer page stalled too, means the whole VM hiccupped (run 39838535897:
/// fixture 327ms, Chrome renderers 329/639ms, capture 8fps for one second).
const HOST_STALL_SOURCE_GAP_MS = 250;
/// More host-VM stalls than this in one window: the VM is too noisy to judge.
const MAX_HOST_STALLS = 2;
const GAP_BUCKETS = [50, 100, 200, 300, 500, 1000, 2000, 5000];
/// Sender log events this close to a freeze are attached to it as evidence.
const SENDER_EVENT_SLACK_MS = 5000;

// Sharer capture/encode lines (session/share.rs, transport/publisher.rs) that
// place a freeze on the CAPTURE side. Field reports (PETAL-DESKTOP-3H) tie the
// real "everyone froze" meetings to a silent ScreenCaptureKit stream.
const SENDER_LOG_EVENTS = [
  ['capture-pull-engaged', /capture-pull: window \d+ ENGAGED -- raw stream silent/],
  ['capture-pull-disengaged', /capture-pull: window \d+ disengaged -- raw stream resumed/],
  ['capture-held-on-pulls', /raw ScreenCaptureKit stream keeps wedging/],
  ['capture-restart', /restarting capture in place|capture_restart\(window|post-wake restart of share/],
  ['republish-timeout', /timed out after [\d.]+s republishing window/],
  ['publisher-degraded', /publisher: window-share DEGRADED/],
  ['test-share-pause', /TEST share pause: (dropping|ended)/],
];
const LOG_TIME = /^(\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2}\.\d{3}) /;
const CAPTURE_DIAG = /capture-diag: window \d+ seq=\d+ ~([\d.]+)fps/;

/// The sharer's petal log -> timestamped capture/encode events plus the
/// capture-diag cadence samples (`~N fps`). Log times are UTC.
export function parseSenderLog(text) {
  const events = [];
  const captureFps = [];
  for (const line of String(text ?? '').split(/\r?\n/)) {
    const time = LOG_TIME.exec(line);
    if (!time) continue;
    const t = Date.parse(`${time[1]}T${time[2]}Z`);
    const diag = CAPTURE_DIAG.exec(line);
    if (diag) {
      captureFps.push([t, Number(diag[1])]);
      continue;
    }
    for (const [kind, pattern] of SENDER_LOG_EVENTS) {
      if (pattern.test(line)) {
        events.push({ t, kind, line: line.slice(0, 300) });
        break;
      }
    }
  }
  return { events, captureFps };
}

const iso = (ms) => (Number.isFinite(ms) ? new Date(ms).toISOString() : null);
const round = (n, d = 1) => (Number.isFinite(n) ? Math.round(n * 10 ** d) / 10 ** d : null);

function percentile(sorted, p) {
  if (!sorted.length) return null;
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[rank];
}

export function gapHistogram(gaps) {
  const labels = GAP_BUCKETS.map((edge, i) => (i === 0 ? `<${edge}` : `${GAP_BUCKETS[i - 1]}-${edge}`));
  labels.push(`>=${GAP_BUCKETS[GAP_BUCKETS.length - 1]}`);
  const histogram = Object.fromEntries(labels.map((label) => [label, 0]));
  for (const gap of gaps) {
    const index = GAP_BUCKETS.findIndex((edge) => gap < edge);
    histogram[labels[index === -1 ? labels.length - 1 : index]] += 1;
  }
  return histogram;
}

/// Longest repaint gap of the SOURCE that overlaps [start, end], plus how many
/// repaints fell inside it. ticks: [[tick, epochMs], ...] sorted by time.
export function sourceActivity(ticks, start, end) {
  let maxGap = 0;
  let inside = 0;
  for (let i = 0; i < ticks.length; i += 1) {
    const t = ticks[i][1];
    if (t >= start && t <= end) inside += 1;
    if (i === 0) continue;
    const prev = ticks[i - 1][1];
    if (t >= start && prev <= end) maxGap = Math.max(maxGap, Math.min(t, end + SOURCE_CHANGE_BUDGET_MS) - Math.max(prev, start - SOURCE_CHANGE_BUDGET_MS));
  }
  const first = ticks[0]?.[1];
  const last = ticks[ticks.length - 1]?.[1];
  // No repaint log covering the interval at all: the source cannot be shown
  // to have changed, so treat the whole interval as a source stall.
  if (!ticks.length || first > end || last < start) maxGap = end - start;
  return { maxGapMs: round(maxGap), repaints: inside };
}

function viewerLagDuring(lag, start, end) {
  let worst = 0;
  for (const [at, ms] of lag ?? []) {
    if (at <= end && at + ms >= start) worst = Math.max(worst, ms);
  }
  return worst;
}

function inboundSummary(stats, start, end) {
  const rows = (stats ?? []).filter((r) => r.t >= start && r.t <= end && r.framesDecoded !== undefined);
  if (!rows.length) return null;
  const first = rows[0];
  const last = rows[rows.length - 1];
  const delta = (key) => (Number.isFinite(last[key]) && Number.isFinite(first[key]) ? round(last[key] - first[key], 3) : null);
  const emitted = delta('jitterBufferEmittedCount');
  const rtts = rows.map((r) => r.rttMs).filter(Number.isFinite).sort((a, b) => a - b);
  return {
    samples: rows.length,
    freezeCount: delta('freezeCount'),
    totalFreezesDurationS: delta('totalFreezesDuration'),
    pauseCount: delta('pauseCount'),
    framesDecoded: delta('framesDecoded'),
    framesDropped: delta('framesDropped'),
    packetsLost: delta('packetsLost'),
    nackCount: delta('nackCount'),
    pliCount: delta('pliCount'),
    keyFramesDecoded: delta('keyFramesDecoded'),
    jitterBufferAvgMs: emitted ? round((delta('jitterBufferDelay') / emitted) * 1000) : null,
    rttP50Ms: round(percentile(rtts, 50)),
    lastResolution: last.frameWidth ? `${last.frameWidth}x${last.frameHeight}` : null,
    decoderImplementation: last.decoderImplementation ?? null,
    unsubscribedSamples: (stats ?? []).filter((r) => r.t >= start && r.t <= end && !r.subscribed).length,
  };
}

function senderSummary(sender, start, end) {
  const rows = (sender ?? []).filter((r) => r.t >= start && r.t <= end && r.track);
  if (!rows.length) return null;
  const fps = rows.map((r) => r.fps).filter(Number.isFinite).sort((a, b) => a - b);
  const kbps = rows.map((r) => r.actual_kbps).filter(Number.isFinite).sort((a, b) => a - b);
  const limits = {};
  for (const r of rows) limits[r.quality_limitation || 'unknown'] = (limits[r.quality_limitation || 'unknown'] ?? 0) + 1;
  const first = rows[0];
  const last = rows[rows.length - 1];
  const delta = (key) => (Number.isFinite(last[key]) && Number.isFinite(first[key]) ? last[key] - first[key] : null);
  return {
    samples: rows.length,
    fpsMin: round(fps[0]),
    fpsP50: round(percentile(fps, 50)),
    actualKbpsP50: round(percentile(kbps, 50)),
    qualityLimitation: limits,
    keyFramesEncoded: delta('key_frames_encoded'),
    pliCount: delta('pli_count'),
    nackCount: delta('nack_count'),
    resolutions: [...new Set(rows.map((r) => `${r.width}x${r.height}`))],
    codecImpl: last.codec_impl ?? null,
  };
}

function senderDuring(sender, start, end) {
  const rows = (sender ?? []).filter((r) => r.track && r.t >= start - 2000 && r.t <= end + 2000);
  const fps = rows.map((r) => r.fps).filter(Number.isFinite);
  return {
    samples: rows.length,
    minFps: fps.length ? round(Math.min(...fps)) : null,
    qualityLimitation: [...new Set(rows.map((r) => r.quality_limitation).filter(Boolean))],
  };
}

function rtpDeltaMs(before, after) {
  if (!Number.isFinite(before) || !Number.isFinite(after)) return null;
  return round(((after - before) >>> 0) / 90, 0);
}

/// Per-viewer gaps inside [start, end]. The window edges count: a viewer that
/// stops presenting before the end has a gap running to the end.
export function analyzeViewer(viewer, { start, end, ticks, thresholdMs }) {
  const frames = (viewer.frames ?? []).filter((f) => f[0] >= start && f[0] <= end).sort((a, b) => a[0] - b[0]);
  const points = [[start, null, null, null, null, null], ...frames, [end, null, null, null, null, null]];
  const gaps = [];
  const freezes = [];
  const sourceStalls = [];
  for (let i = 1; i < points.length; i += 1) {
    const before = points[i - 1];
    const after = points[i];
    const gap = after[0] - before[0];
    gaps.push(gap);
    if (gap <= thresholdMs) continue;
    const source = sourceActivity(ticks, before[0], after[0]);
    const lagMs = viewerLagDuring(viewer.lag, before[0], after[0]);
    const tickDelta = Number.isFinite(before[1]) && Number.isFinite(after[1]) ? (after[1] - before[1] + 65536) % 65536 : null;
    const entry = {
      viewer: viewer.label,
      startMs: round(before[0], 0),
      endMs: round(after[0], 0),
      start: iso(before[0]),
      end: iso(after[0]),
      durationMs: round(gap, 0),
      edge: i === 1 ? 'measure-start' : i === points.length - 1 ? 'measure-end' : null,
      sourceRepaintsDuringGap: source.repaints,
      sourceMaxRepaintGapMs: source.maxGapMs,
      sourceTickDelta: tickDelta,
      rtpDeltaMs: rtpDeltaMs(before[2], after[2]),
      presentedFramesDelta: Number.isFinite(before[5]) && Number.isFinite(after[5]) ? after[5] - before[5] : null,
      viewerEventLoopLagMs: lagMs,
      viewerRendererStalled: lagMs >= VIEWER_LAG_STALL_MS,
    };
    if (source.maxGapMs >= thresholdMs) sourceStalls.push(entry);
    else freezes.push(entry);
  }
  const sorted = [...gaps].sort((a, b) => a - b);
  const decoded = frames.filter((f) => Number.isFinite(f[1])).length;
  const seconds = Math.max((end - start) / 1000, 0.001);
  return {
    label: viewer.label,
    identity: viewer.identity ?? null,
    presentedFrames: frames.length,
    fps: round(frames.length / seconds),
    decodedTickFraction: frames.length ? round(decoded / frames.length, 3) : 0,
    gapP50Ms: round(percentile(sorted, 50)),
    gapP95Ms: round(percentile(sorted, 95)),
    gapP99Ms: round(percentile(sorted, 99)),
    gapMaxMs: round(sorted[sorted.length - 1]),
    gapHistogram: gapHistogram(gaps),
    frozenMs: round(freezes.reduce((sum, f) => sum + f.durationMs, 0), 0),
    freezes,
    sourceStalls,
    resolutions: [...new Set(frames.map((f) => `${f[3]}x${f[4]}`))],
    inbound: inboundSummary(viewer.stats, start, end),
    eventLoopStalls: (viewer.lag ?? []).filter(([at]) => at >= start && at <= end).length,
  };
}

/// Group freeze intervals from all viewers into events. An event every
/// measured viewer took part in is CORRELATED (sender/SFU side); one viewer
/// alone is receiver side.
export function correlateFreezes(viewerReports, sender, senderLog = { events: [], captureFps: [] }) {
  const all = viewerReports.flatMap((v) => v.freezes).sort((a, b) => a.startMs - b.startMs);
  const clusters = [];
  for (const freeze of all) {
    const current = clusters[clusters.length - 1];
    if (current && freeze.startMs <= current.endMs + CORRELATION_SLACK_MS) {
      current.endMs = Math.max(current.endMs, freeze.endMs);
      current.members.push(freeze);
    } else {
      clusters.push({ startMs: freeze.startMs, endMs: freeze.endMs, members: [freeze] });
    }
  }
  const total = viewerReports.length;
  return clusters.map((cluster) => {
    const viewers = [...new Set(cluster.members.map((m) => m.viewer))];
    const scope = viewers.length === total ? 'all-viewers' : viewers.length === 1 ? 'single-viewer' : 'some-viewers';
    const during = senderDuring(sender, cluster.startMs, cluster.endMs);
    const everyRendererStalled = cluster.members.every((m) => m.viewerRendererStalled);
    const near = (t) => t >= cluster.startMs - SENDER_EVENT_SLACK_MS && t <= cluster.endMs + SENDER_EVENT_SLACK_MS;
    const senderEvents = senderLog.events.filter((e) => near(e.t));
    const captureFps = senderLog.captureFps.filter(([t]) => near(t)).map(([, fps]) => fps);
    const induced = senderEvents.find((e) => e.kind === 'test-share-pause');
    const captureSide = senderEvents.find((e) => e.kind.startsWith('capture-') || e.kind === 'republish-timeout');
    // Host stall needs THREE independent processes stalled at once (the fixture
    // and every viewer's renderer) and no sharer-side evidence. Anything less
    // is counted as a freeze.
    const hostStall = scope === 'all-viewers' && !induced && !captureSide && everyRendererStalled
      && cluster.members.every((m) => m.sourceMaxRepaintGapMs >= HOST_STALL_SOURCE_GAP_MS);
    let attribution;
    if (scope === 'all-viewers') {
      if (induced) attribution = 'sender: the induced PETAL_TEST_SHARE_PAUSE (red proof)';
      else if (captureSide) attribution = `sender-capture: ${captureSide.kind} on the sharer`;
      else if (hostStall) attribution = 'host-vm-stall: the source fixture and every viewer page stalled at the same moment (VM-wide, not the stream)';
      else if (during.minFps !== null && during.minFps < 1) attribution = 'sender: the encoder stopped producing frames';
      else attribution = 'sender-or-sfu: every viewer froze together while the sender kept encoding';
    } else if (scope === 'single-viewer') {
      attribution = everyRendererStalled
        ? `receiver-side: ${viewers[0]}'s page stalled (renderer, not the stream)`
        : `receiver-side: only ${viewers[0]} froze`;
    } else {
      attribution = `partial: ${viewers.join(', ')} froze, the others did not`;
    }
    return {
      start: iso(cluster.startMs),
      end: iso(cluster.endMs),
      startMs: cluster.startMs,
      endMs: cluster.endMs,
      spanMs: cluster.endMs - cluster.startMs,
      viewers,
      scope,
      correlated: scope === 'all-viewers',
      counted: !hostStall,
      attribution,
      sender: during,
      senderLogEvents: senderEvents,
      senderCaptureFpsMin: captureFps.length ? Math.min(...captureFps) : null,
      members: cluster.members.map((m) => ({
        viewer: m.viewer, start: m.start, durationMs: m.durationMs,
        viewerEventLoopLagMs: m.viewerEventLoopLagMs, sourceMaxRepaintGapMs: m.sourceMaxRepaintGapMs, rtpDeltaMs: m.rtpDeltaMs,
      })),
    };
  });
}

function tickToPresentLatency(viewers, ticks, start, end) {
  const byTick = new Map();
  for (const [tick, ms] of ticks) byTick.set(tick % 65536, ms);
  const samples = [];
  for (const viewer of viewers) {
    for (const f of viewer.frames ?? []) {
      if (f[0] < start || f[0] > end || !Number.isFinite(f[1])) continue;
      const painted = byTick.get(f[1]);
      if (Number.isFinite(painted) && f[0] >= painted && f[0] - painted < 10_000) samples.push(f[0] - painted);
    }
  }
  samples.sort((a, b) => a - b);
  return { samples: samples.length, p50Ms: round(percentile(samples, 50)), p95Ms: round(percentile(samples, 95)), note: 'report-only: fixture repaint -> viewer presentation, same host clock' };
}

export function freezeVerdict({ raw, senderLogText = null, thresholdMs = FREEZE_GAP_THRESHOLD_MS }) {
  const errors = [];
  const warnings = [];
  const notes = [];
  const infra = [];
  const report = { outcome: null, thresholdMs, harnessUrl: raw?.harnessUrl ?? null };
  const done = () => {
    for (const problem of infra) errors.unshift(`INFRA-INVALID: ${problem}`);
    report.outcome = infra.length ? 'infra-invalid' : errors.length ? 'fail' : 'pass';
    report.infra = infra;
    return { ok: errors.length === 0, outcome: report.outcome, errors, warnings, notes, report };
  };

  if (!raw || typeof raw !== 'object') {
    infra.push('no share-freeze-raw.json (or it did not parse): the soak measured nothing');
    return done();
  }
  for (const problem of raw.infra ?? []) infra.push(problem);
  for (const problem of raw.fatal ?? []) errors.push(`FREEZE: ${problem}`);
  report.sharer = raw.sharer ?? null;
  report.host = raw.host ?? null;
  report.testSharePause = raw.testSharePause ?? null;
  const start = raw.measureStartMs;
  const end = raw.measureEndMs;
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
    if (!infra.length && !errors.length) infra.push('the soak never opened a measurement window');
    return done();
  }
  report.measureStart = iso(start);
  report.measureEnd = iso(end);
  report.measuredMs = end - start;
  const requested = Number(raw.requestedDurationMs);
  if (!raw.fatal?.length && (!Number.isFinite(requested) || end - start < requested * MIN_MEASURED_FRACTION)) {
    infra.push(`measured ${end - start}ms of the requested ${raw.requestedDurationMs}ms`);
  }

  const ticks = [...(raw.fixture?.ticks ?? [])].sort((a, b) => a[1] - b[1]);
  const tickGaps = [];
  for (let i = 1; i < ticks.length; i += 1) {
    if (ticks[i][1] >= start && ticks[i][1] <= end) tickGaps.push(ticks[i][1] - ticks[i - 1][1]);
  }
  tickGaps.sort((a, b) => a - b);
  report.source = {
    repaints: tickGaps.length,
    repaintGapP50Ms: round(percentile(tickGaps, 50)),
    repaintGapP99Ms: round(percentile(tickGaps, 99)),
    repaintGapMaxMs: round(tickGaps[tickGaps.length - 1]),
  };
  if (tickGaps.length < 10) {
    infra.push('the source fixture logged (almost) no repaints in the window, so no gap can be shown to be a freeze');
  } else if (percentile(tickGaps, 50) > SOURCE_CHANGE_BUDGET_MS) {
    infra.push(`the source repainted every ${round(percentile(tickGaps, 50))}ms (median), slower than the ${SOURCE_CHANGE_BUDGET_MS}ms the gate needs`);
  }

  const viewers = (raw.viewers ?? []).map((v) => analyzeViewer(v, { start, end, ticks, thresholdMs }));
  report.viewers = viewers;
  if (viewers.length < 2) infra.push(`only ${viewers.length} viewer(s) measured; the gate needs at least 2 to tell sender from receiver`);
  for (const v of viewers) {
    if (v.presentedFrames === 0 && !(raw.viewers ?? []).find((r) => r.label === v.label)?.firstFrameAtMs) {
      infra.push(`${v.label} never presented a frame`);
    }
    if (v.decodedTickFraction < 0.5 && v.presentedFrames > 0) {
      warnings.push(`${v.label}: decoded the source tick from only ${Math.round(v.decodedTickFraction * 100)}% of frames (fixture geometry?); gaps still count`);
    }
    const stalledMs = v.sourceStalls.reduce((sum, s) => sum + s.durationMs, 0);
    if (stalledMs > (end - start) * MAX_SOURCE_STALL_FRACTION) {
      infra.push(`${v.label}: ${stalledMs}ms of the window coincided with the SOURCE not repainting (host stall), too much to measure`);
    }
    for (const s of v.sourceStalls) {
      warnings.push(`${v.label}: ${s.durationMs}ms gap at ${s.start} NOT counted: the source itself did not repaint for ${s.sourceMaxRepaintGapMs}ms`);
    }
  }

  report.sender = senderSummary(raw.sender, start, end);
  if (!report.sender) warnings.push('no sender (outbound) samples from the native sharer in the window');
  const senderLog = parseSenderLog(senderLogText);
  if (senderLogText === null) warnings.push('no sharer log given (--sender-log): freezes cannot be placed on the capture side');
  const inWindow = (t) => t >= start && t <= end;
  const captureFps = senderLog.captureFps.filter(([t]) => inWindow(t)).map(([, fps]) => fps).sort((a, b) => a - b);
  const counts = {};
  for (const e of senderLog.events) if (inWindow(e.t)) counts[e.kind] = (counts[e.kind] ?? 0) + 1;
  report.senderCapture = {
    eventCounts: counts,
    captureDiagSamples: captureFps.length,
    captureFpsMin: round(captureFps[0]),
    captureFpsP50: round(percentile(captureFps, 50)),
    events: senderLog.events.filter((e) => inWindow(e.t)).slice(0, 200),
  };
  report.clusters = correlateFreezes(viewers, raw.sender, senderLog);
  report.correlatedFreezes = report.clusters.filter((c) => c.correlated && c.counted).length;
  report.receiverOnlyFreezes = report.clusters.filter((c) => c.scope === 'single-viewer').length;
  report.latency = tickToPresentLatency(raw.viewers ?? [], ticks, start, end);

  const hostStalls = report.clusters.filter((c) => !c.counted);
  report.hostVmStalls = hostStalls.length;
  if (hostStalls.length > MAX_HOST_STALLS) {
    infra.push(`${hostStalls.length} host-VM stalls (fixture and every viewer page frozen together) in one window: the VM is too noisy to judge freezes`);
  }
  for (const cluster of report.clusters) {
    const detail = cluster.members.map((m) => `${m.viewer} ${m.durationMs}ms`).join(', ');
    if (cluster.counted) errors.push(`FREEZE ${cluster.scope} at ${cluster.start} (${detail}) -- ${cluster.attribution}`);
    else warnings.push(`NOT counted: ${cluster.scope} gap at ${cluster.start} (${detail}) -- ${cluster.attribution}`);
  }
  for (const v of viewers) {
    const inbound = v.inbound;
    notes.push(`${v.label}: ${v.presentedFrames} frames ${v.fps}fps gap p50/p95/p99/max ${v.gapP50Ms}/${v.gapP95Ms}/${v.gapP99Ms}/${v.gapMaxMs}ms freezes>${thresholdMs}ms=${v.freezes.length} frozen=${v.frozenMs}ms res=${v.resolutions.join(',')}` +
      (inbound ? ` inbound: freezeCount+${inbound.freezeCount} freezesDur+${inbound.totalFreezesDurationS}s dropped+${inbound.framesDropped} lost+${inbound.packetsLost} pli+${inbound.pliCount} jb=${inbound.jitterBufferAvgMs}ms rtt=${inbound.rttP50Ms}ms` : ' inbound: none'));
  }
  if (report.sender) {
    const s = report.sender;
    notes.push(`sender: fps min/p50 ${s.fpsMin}/${s.fpsP50} kbps p50 ${s.actualKbpsP50} qualityLimitation ${JSON.stringify(s.qualityLimitation)} keyframes+${s.keyFramesEncoded} pli+${s.pliCount} res=${s.resolutions.join(',')}`);
  }
  notes.push(`sender capture (sharer log): ${JSON.stringify(report.senderCapture.eventCounts)} capture-diag fps min/p50 ${report.senderCapture.captureFpsMin}/${report.senderCapture.captureFpsP50}; host ${JSON.stringify(report.host)}`);
  notes.push(`source: repaint gap p50/p99/max ${report.source.repaintGapP50Ms}/${report.source.repaintGapP99Ms}/${report.source.repaintGapMaxMs}ms; tick->present latency p50/p95 ${report.latency.p50Ms}/${report.latency.p95Ms}ms (report-only)`);
  notes.push(`share-freeze: ${report.clusters.length} freeze event(s) over ${thresholdMs}ms in ${Math.round((end - start) / 1000)}s: ${report.correlatedFreezes} correlated (all viewers), ${report.receiverOnlyFreezes} single-viewer, ${report.hostVmStalls} host-VM stall(s) not counted`);
  return done();
}
