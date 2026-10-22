// Run-level remote-control latency gate for the release e2e loopback: the
// `latency` tier of scripts/ci/e2e-gate-verdict.mjs (tests, both directions:
// scripts/test-remote-control-latency-verdict.mjs).
//
// Before this, the only latency check in the gate was a per-observation 620ms
// budget, retried once, measured with a 250ms poll; p50/p95 were printed and
// never enforced. Remote control of a shared window is the pair-programming
// core of the product, so a release now fails when the DISTRIBUTION regresses:
//
//   target observation  web send -> TextEdit/clipboard shows it (the existing
//                       correctness oracle, timed by the in-process watcher)
//   host pipeline       Petal's own receive -> inject and receive -> replay
//                       complete, from the host's `remote-control-latency:`
//                       log lines -- isolates Petal from TextEdit/AppleScript
//   send -> inject      the web controller's publish time paired by seq with
//                       the host's inject line (one clock: same machine)
//   press-to-photon     web input -> sentinel repaint -> decoded frame back in
//                       the controller's browser: what a controller feels
//
// Every check fails on a breach AND on too few samples -- absence is never a
// pass. Budgets are calibrated on the self-hosted Tart VM; see
// .github/workflows/nightly-loopback.yml for the runs and the headroom.

export function nearestRank(values, fraction) {
  const sorted = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)];
}

function round(value) {
  return value === null || value === undefined ? null : Math.round(value * 10) / 10;
}

export function distribution(values) {
  const finite = values.filter((value) => Number.isFinite(value));
  return {
    samples: finite.length,
    minMs: round(nearestRank(finite, 0)),
    p50Ms: round(nearestRank(finite, 0.5)),
    p95Ms: round(nearestRank(finite, 0.95)),
    maxMs: round(finite.length ? Math.max(...finite) : null),
  };
}

// Defaults are the VM calibration (the workflow sets the same values
// explicitly); a check whose env var is set uses that instead.
export const LATENCY_CHECKS = [
  { id: 'observation-p50', metric: 'targetObservation', stat: 'p50Ms', env: 'PETAL_RC_OBSERVATION_P50_BUDGET_MS', defaultBudgetMs: 200, minSamples: 14 },
  { id: 'observation-p95', metric: 'targetObservation', stat: 'p95Ms', env: 'PETAL_RC_OBSERVATION_P95_BUDGET_MS', defaultBudgetMs: 400, minSamples: 14 },
  { id: 'host-receive-to-inject-p95', metric: 'hostReceiveToInject', stat: 'p95Ms', env: 'PETAL_RC_HOST_RECEIVE_TO_INJECT_P95_BUDGET_MS', defaultBudgetMs: 40, minSamples: 40 },
  { id: 'host-receive-to-complete-p95', metric: 'hostReceiveToComplete', stat: 'p95Ms', env: 'PETAL_RC_HOST_RECEIVE_TO_COMPLETE_P95_BUDGET_MS', defaultBudgetMs: 200, minSamples: 40 },
  { id: 'send-to-inject-p95', metric: 'sendToInject', stat: 'p95Ms', env: 'PETAL_RC_SEND_TO_INJECT_P95_BUDGET_MS', defaultBudgetMs: 50, minSamples: 30 },
  { id: 'press-to-photon-p50', metric: 'pressToPhoton', stat: 'p50Ms', env: 'PETAL_RC_PHOTON_P50_BUDGET_MS', defaultBudgetMs: 250, minSamples: 30 },
  { id: 'press-to-photon-p95', metric: 'pressToPhoton', stat: 'p95Ms', env: 'PETAL_REMOTE_CONTROL_PHOTON_P95_BUDGET_MS', defaultBudgetMs: 400, minSamples: 30 },
  // #378: longest gap between presented frames at the web viewer while the
  // sentinel animates continuously (samples = presented frames; 100 over the
  // 20s probe is a 5fps floor). Frozen video is what users report first.
  { id: 'presented-freeze-max-gap', metric: 'presentedFreeze', stat: 'maxGapMs', env: 'PETAL_RC_PRESENTED_FREEZE_BUDGET_MS', defaultBudgetMs: 500, minSamples: 100 },
];

export function resolveLatencyBudgets(env = {}) {
  const budgets = {};
  for (const check of LATENCY_CHECKS) {
    const raw = env[check.env];
    if (raw === undefined || raw === null || `${raw}`.trim() === '') {
      budgets[check.id] = check.defaultBudgetMs;
      continue;
    }
    const value = Number(raw);
    if (!Number.isFinite(value) || value <= 0) {
      throw new Error(`${check.env} must be a positive number, got ${JSON.stringify(raw)}`);
    }
    budgets[check.id] = value;
  }
  return budgets;
}

// One event per (controller, window, seq) occurrence. A seq can repeat when
// the web controller reloads and its counter restarts, so a second `receive`
// for a key that already has one starts a new event.
export function parseHostLatencyLog(text) {
  const events = [];
  const open = new Map();
  const fields = [
    ['receiveTsMs', /host receive_ts_ms=(\d+)/],
    ['enqueueTsMs', /host enqueue_ts_ms=(\d+)/],
    ['injectTsMs', /host inject_ts_ms=(\d+)/],
    ['completeTsMs', /host replay complete_ts_ms=(\d+)/],
    ['timeoutTsMs', /host replay timeout_ts_ms=(\d+)/],
    ['frameShownTsMs', /host frame_shown_ts_ms=(\d+)/],
  ];
  for (const line of String(text ?? '').split('\n')) {
    if (!line.includes('remote-control-latency: host ')) continue;
    const seq = /\bseq=(\d+)/.exec(line);
    const window = /\bwindow=(\d+)/.exec(line);
    if (!seq || !window) continue;
    const controller = /controller='([^']*)'/.exec(line)?.[1] ?? '';
    const key = `${controller}\u0000${window[1]}\u0000${seq[1]}`;
    let event = open.get(key);
    const isReceive = fields[0][1].test(line);
    if (!event || (isReceive && event.receiveTsMs !== undefined)) {
      const kind = /\bkind=(\w+) action=(\S+)/.exec(line);
      event = {
        controller,
        windowId: Number(window[1]),
        seq: Number(seq[1]),
        kind: kind?.[1] ?? null,
        action: kind?.[2] ?? null,
      };
      open.set(key, event);
      events.push(event);
    }
    for (const [name, pattern] of fields) {
      const match = pattern.exec(line);
      if (match && event[name] === undefined) event[name] = Number(match[1]);
    }
    const elapsed = /\belapsed_ms=(\d+)/.exec(line);
    if (elapsed && event.elapsedMs === undefined) event.elapsedMs = Number(elapsed[1]);
    const injectToFrame = /\binject_to_frame_ms=(\d+)/.exec(line);
    if (injectToFrame && event.injectToFrameMs === undefined) event.injectToFrameMs = Number(injectToFrame[1]);
  }
  return events;
}

const INPUT_KINDS = new Set(['pointer', 'key', 'text', 'wheel']);
// The host cannot receive before the controller sent (one clock); 5ms covers
// the ms rounding on both sides. 5s is far past any healthy delivery.
const PAIR_EARLY_SLACK_MS = 5;
const PAIR_WINDOW_MS = 5000;

// Pair each controller publish with the host event of the same window+seq
// whose receive time is nearest after the send.
export function pairControllerPublishes(publishes, hostEvents) {
  const byKey = new Map();
  for (const event of hostEvents) {
    if (!Number.isFinite(event.receiveTsMs)) continue;
    const key = `${event.windowId}:${event.seq}`;
    if (!byKey.has(key)) byKey.set(key, []);
    byKey.get(key).push(event);
  }
  const used = new Set();
  const pairs = [];
  for (const publish of publishes ?? []) {
    if (!INPUT_KINDS.has(publish.kind) || !Number.isFinite(publish.at)) continue;
    let best = null;
    for (const event of byKey.get(`${publish.windowId}:${publish.seq}`) ?? []) {
      if (used.has(event)) continue;
      const delta = event.receiveTsMs - publish.at;
      if (delta < -PAIR_EARLY_SLACK_MS || delta > PAIR_WINDOW_MS) continue;
      if (!best || Math.abs(delta) < Math.abs(best.receiveTsMs - publish.at)) best = event;
    }
    if (!best) continue;
    used.add(best);
    pairs.push({
      caseId: publish.caseId ?? null,
      kind: publish.kind,
      seq: publish.seq,
      sendToReceiveMs: Math.max(0, best.receiveTsMs - publish.at),
      sendToInjectMs: Number.isFinite(best.injectTsMs) ? Math.max(0, best.injectTsMs - publish.at) : null,
    });
  }
  return pairs;
}

export function observationSamples(results) {
  return (results ?? []).flatMap((result) => [
    ...(Array.isArray(result.targetObservationSamplesMs) ? result.targetObservationSamplesMs : []),
    ...(Array.isArray(result.targetObservationDistributionSamplesMs) ? result.targetObservationDistributionSamplesMs : []),
  ]).filter((value) => Number.isFinite(value));
}

// A measured photon sample that missed (generation never decoded within the
// per-sample timeout) is scored AS the timeout, so one miss lands in the tail
// the p95 already tolerates instead of voiding the run. More than
// MAX_PHOTON_MISSES, or fewer scored samples than scheduled, is insufficient.
export const MAX_PHOTON_MISSES = 2;
export const DEFAULT_PHOTON_MISS_SCORE_MS = 2000;

export function photonValues(photonReport) {
  const stats = photonReport?.summary?.pressToPhoton;
  if (!stats) return { values: [], complete: false, misses: 0, reason: 'no press-to-photon summary' };
  const measured = (stats.timingSamples ?? []).map((sample) => sample.pressToEstimatedPhotonMs);
  const missScoreMs = Number.isFinite(stats.sampleTimeoutMs) ? stats.sampleTimeoutMs : DEFAULT_PHOTON_MISS_SCORE_MS;
  // Warm-up misses never count (warm-up absorbs the encoder/focus ramp).
  const misses = (stats.sampleFailures ?? []).filter((failure) => !failure?.warmup).length;
  const values = [...measured, ...Array.from({ length: misses }, () => missScoreMs)];
  const expected = stats.expectedSamples ?? Infinity;
  const complete = misses <= MAX_PHOTON_MISSES && values.length >= expected;
  let reason = null;
  if (misses > MAX_PHOTON_MISSES) reason = `${misses} missed sample(s) > ${MAX_PHOTON_MISSES} allowed`;
  else if (values.length < expected) reason = `scored ${values.length}/${expected} samples`;
  return { values, complete, misses, reason };
}

export function presentedFreezeMetric(photonReport) {
  const probe = photonReport?.summary?.freezeProbe;
  if (!probe) return { samples: 0, maxGapMs: null, error: 'no freeze probe in the press-to-photon report (PETAL_RC_FREEZE_PROBE_MS unset?)' };
  if (probe.error) return { samples: 0, maxGapMs: null, error: probe.error };
  return {
    samples: probe.frames ?? 0,
    maxGapMs: probe.maxGapMs ?? null,
    fps: probe.fps ?? null,
    freezes: probe.freezes ?? null,
    frozenMs: probe.frozenMs ?? null,
    durationMs: probe.durationMs ?? null,
    worstGaps: probe.worstGaps ?? [],
    decodeStall: probe.decodeStall ?? null,
    playback: probe.playback ?? null,
    error: null,
  };
}


export function collectLatencyMetrics({ results = null, photon = null, hostLog = '' }) {
  const hostEvents = parseHostLatencyLog(hostLog);
  const publishes = [...(results?.controllerPublishes ?? []), ...(photon?.controllerPublishes ?? [])];
  const pairs = pairControllerPublishes(publishes, hostEvents);
  const photonData = photonValues(photon);
  const receiveToInject = hostEvents
    .filter((event) => Number.isFinite(event.receiveTsMs) && Number.isFinite(event.injectTsMs))
    .map((event) => event.injectTsMs - event.receiveTsMs);
  const receiveToComplete = hostEvents
    .filter((event) => Number.isFinite(event.receiveTsMs) && Number.isFinite(event.completeTsMs))
    .map((event) => event.completeTsMs - event.receiveTsMs);
  return {
    targetObservation: {
      ...distribution(observationSamples(results?.results)),
      methods: results?.summary?.targetObservationLatency?.methods ?? null,
      maxReadGapMs: results?.summary?.targetObservationLatency?.maxReadGapMs ?? null,
    },
    hostReceiveToInject: distribution(receiveToInject),
    hostReceiveToComplete: distribution(receiveToComplete),
    sendToReceive: distribution(pairs.map((pair) => pair.sendToReceiveMs)),
    sendToInject: { ...distribution(pairs.map((pair) => pair.sendToInjectMs)), publishes: publishes.length, paired: pairs.length },
    hostInjectToFrame: distribution(hostEvents.map((event) => event.injectToFrameMs)),
    hostReplayTimeouts: hostEvents.filter((event) => Number.isFinite(event.timeoutTsMs)).length,
    pressToPhoton: {
      ...distribution(photonData.values),
      complete: photonData.complete,
      misses: photonData.misses,
      incompleteReason: photonData.reason,
      byInput: photon?.summary?.pressToPhoton?.byInput ?? null,
    },
    presentedFreeze: presentedFreezeMetric(photon),
  };
}

// Pure verdict: pass only when every check has enough samples AND is inside
// its budget. `insufficient` fails the gate exactly like a breach does.
export function evaluateLatencyGate(metrics, budgets) {
  const checks = LATENCY_CHECKS.map((check) => {
    const metric = metrics?.[check.metric] ?? {};
    const value = metric[check.stat] ?? null;
    const budgetMs = budgets[check.id];
    let status;
    let detail;
    if (check.metric === 'pressToPhoton' && metric.complete === false) {
      status = 'insufficient';
      detail = `press-to-photon run incomplete: ${metric.incompleteReason}`;
    } else if (check.metric === 'presentedFreeze' && metric.error) {
      status = 'insufficient';
      detail = `freeze probe did not run: ${metric.error}`;
    } else if ((metric.samples ?? 0) < check.minSamples || value === null) {
      status = 'insufficient';
      detail = `${metric.samples ?? 0} sample(s) < ${check.minSamples} required`;
    } else if (value > budgetMs) {
      status = 'fail';
      detail = `${check.stat.replace('Ms', '')} ${value}ms > ${budgetMs}ms budget over ${metric.samples} samples`;
      if (check.metric === 'presentedFreeze') {
        detail += ` (${metric.freezes} freeze(s), ${metric.frozenMs}ms frozen in ${metric.durationMs}ms; worst ${JSON.stringify(metric.worstGaps.slice(0, 3))})`;
      }
    } else {
      status = 'pass';
      detail = `${check.stat.replace('Ms', '')} ${value}ms <= ${budgetMs}ms over ${metric.samples} samples`;
    }
    return { id: check.id, env: check.env, valueMs: value, budgetMs, samples: metric.samples ?? 0, minSamples: check.minSamples, status, detail };
  });
  return { pass: checks.every((check) => check.status === 'pass'), checks };
}

// The `latency` tier of scripts/ci/e2e-gate-verdict.mjs: same
// { ok, errors, warnings, notes } shape as the loopback/cockpit verdicts.
export function latencyVerdict({ results = null, photon = null, hostLog = '', env = {} }) {
  const errors = [];
  const warnings = [];
  const notes = [];
  let budgets;
  try {
    budgets = resolveLatencyBudgets(env);
  } catch (error) {
    errors.push(error.message);
    return { ok: false, errors, warnings, notes, budgets: null, metrics: null, checks: [] };
  }
  const metrics = collectLatencyMetrics({ results, photon, hostLog });
  const verdict = evaluateLatencyGate(metrics, budgets);
  notes.push('remote-control latency (ms)        samples    p50     p95     max');
  for (const [name, metric] of Object.entries(metrics)) {
    if (!metric || typeof metric !== 'object' || !('samples' in metric) || name === 'presentedFreeze') continue;
    notes.push(`  ${name.padEnd(30)} ${String(metric.samples).padStart(7)} ${String(metric.p50Ms ?? 'n/a').padStart(6)} ${String(metric.p95Ms ?? 'n/a').padStart(7)} ${String(metric.maxMs ?? 'n/a').padStart(7)}`);
  }
  notes.push(`  host replay timeouts: ${metrics.hostReplayTimeouts}; send->inject paired ${metrics.sendToInject.paired}/${metrics.sendToInject.publishes} controller publishes; observation methods ${JSON.stringify(metrics.targetObservation.methods)}, watcher max read gap ${metrics.targetObservation.maxReadGapMs ?? 'n/a'}ms`);
  const freeze = metrics.presentedFreeze;
  notes.push(freeze.error
    ? `  presented-video freeze probe: n/a (${freeze.error})`
    : `  presented-video freeze probe: ${freeze.samples} frames in ${freeze.durationMs}ms (${freeze.fps} fps), max gap ${freeze.maxGapMs}ms, ${freeze.freezes} gap(s) > 500ms totalling ${freeze.frozenMs}ms; decode-side ${JSON.stringify(freeze.decodeStall)}, playback ${JSON.stringify(freeze.playback)}`);
  for (const check of verdict.checks) {
    const line = `latency ${check.id}: ${check.status.toUpperCase()} -- ${check.detail} (${check.env})`;
    if (check.status === 'pass') notes.push(line);
    else errors.push(line);
  }
  return { ok: verdict.pass, errors, warnings, notes, budgets, metrics, checks: verdict.checks };
}

// #378 stage attribution for the impaired pass. `clean`/`impaired` are the
// `--json` outputs of the two `latency` verdicts of one run; `delayMs` is the
// one-way delay per SFU traversal scripts/ci/net-impair.sh applied. Proves the
// impairment landed on the NETWORK stages and nowhere else: a web input
// crosses two traversals (controller->SFU->host). 0.75 absorbs dummynet's 10ms tick; the host stage must
// not move by more than 10ms. Without this, an impairment that silently did
// not apply would make the "impaired" pass a second clean one.
export const ATTRIBUTION_MOVE_FRACTION = 0.75;
export const ATTRIBUTION_STILL_TOLERANCE_MS = 10;
// pressToPhoton is REPORTED, not asserted: its p50 is quantized to the 60Hz
// display and swings run to run (+14.8ms at 20ms/traversal in run
// 39837860987, +248ms at 60ms in 39845464328), and Chrome leaves rVFC
// captureTime/receiveTime unset here, so the video leg cannot be isolated.
// The impaired photon BUDGETS still gate it.
export const ATTRIBUTION_ROWS = [
  { metric: 'sendToReceive', traversals: 2, rule: 'must-move' },
  { metric: 'hostReceiveToInject', traversals: 0, rule: 'must-not-move' },
  { metric: 'pressToPhoton', traversals: 4, rule: 'report' },
];

export function attributionVerdict({ clean, impaired, delayMs }) {
  const errors = [];
  const notes = [];
  const delay = Number(delayMs);
  if (!Number.isFinite(delay) || delay <= 0) {
    errors.push(`attribution: impairment delay must be a positive number of ms, got ${JSON.stringify(delayMs)}`);
    return { ok: false, errors, warnings: [], notes, rows: [] };
  }
  const rows = ATTRIBUTION_ROWS.map((row) => {
    const before = clean?.metrics?.[row.metric]?.p50Ms ?? null;
    const after = impaired?.metrics?.[row.metric]?.p50Ms ?? null;
    if (!Number.isFinite(before) || !Number.isFinite(after)) {
      if (row.rule === 'report') return { ...row, before, after, movedMs: null, status: 'pass', detail: 'n/a (reported only)' };
      return { ...row, before, after, movedMs: null, status: 'insufficient', detail: `p50 missing (clean ${before}, impaired ${after})` };
    }
    const movedMs = Math.round((after - before) * 10) / 10;
    if (row.rule === 'must-move') {
      const needMs = ATTRIBUTION_MOVE_FRACTION * row.traversals * delay;
      const ok = movedMs >= needMs;
      return { ...row, before, after, movedMs, status: ok ? 'pass' : 'fail', detail: `p50 ${before} -> ${after}ms moved ${movedMs}ms; expected ~${row.traversals * delay}ms (${row.traversals} x ${delay}ms), need >= ${needMs}ms` };
    }
    if (row.rule === 'report') {
      return { ...row, before, after, movedMs, status: 'pass', detail: `p50 ${before} -> ${after}ms moved ${movedMs}ms (reported only; modelled ~${row.traversals * delay}ms)` };
    }
    const ok = Math.abs(movedMs) <= ATTRIBUTION_STILL_TOLERANCE_MS;
    return { ...row, before, after, movedMs, status: ok ? 'pass' : 'fail', detail: `p50 ${before} -> ${after}ms moved ${movedMs}ms; host stage must stay within ${ATTRIBUTION_STILL_TOLERANCE_MS}ms` };
  });
  for (const row of rows) {
    const line = `attribution ${row.metric} (${row.rule}): ${row.status.toUpperCase()} -- ${row.detail}`;
    if (row.status === 'pass') notes.push(line);
    else errors.push(line);
  }
  return { ok: errors.length === 0, errors, warnings: [], notes, rows };
}
