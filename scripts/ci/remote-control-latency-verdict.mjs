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
// #378: the impaired pass and the presented-freeze check are REPORT-ONLY
// unless this env is exactly "true" (nightly-loopback.yml sets it).
export const IMPAIRED_BLOCKING_ENV = 'E2E_IMPAIRED_BLOCKING';

export function envFlagTrue(env, name) {
  return String(env?.[name] ?? '').trim().toLowerCase() === 'true';
}

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
  // REPORT-ONLY until #399's fix (#421) lands: it blocks only when
  // E2E_IMPAIRED_BLOCKING=true, so the clean pass keeps its pre-#378 checks.
  { id: 'presented-freeze-max-gap', metric: 'presentedFreeze', stat: 'maxGapMs', env: 'PETAL_RC_PRESENTED_FREEZE_BUDGET_MS', defaultBudgetMs: 500, minSamples: 100, blockingEnv: IMPAIRED_BLOCKING_ENV },
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

// #370: the aggregated `remote-control-latency-summary:` mirror lines (one per
// 60 s of control, plus a final partial). Reported next to the per-event log
// numbers, never gated: the summary covers every input while the per-event
// lines sample moves/wheels, so the two agree only to within a bucket or so.
export function parseLatencySummaryLog(text) {
  const intervals = [];
  for (const line of String(text ?? '').split('\n')) {
    const at = line.indexOf('remote-control-latency-summary: ');
    if (at < 0) continue;
    const fields = {};
    for (const pair of line.slice(at + 'remote-control-latency-summary: '.length).trim().split(/\s+/)) {
      const eq = pair.indexOf('=');
      if (eq <= 0) continue;
      const key = pair.slice(0, eq);
      const raw = pair.slice(eq + 1);
      fields[key] = /^-?\d+$/.test(raw) ? Number(raw) : raw === 'true' ? true : raw === 'false' ? false : raw;
    }
    if (typeof fields.role === 'string') intervals.push(fields);
  }
  return intervals;
}

export function summarizeLatencySummaries(intervals, role = 'host') {
  const mine = (intervals ?? []).filter((fields) => fields.role === role);
  const stage = (name) => {
    const withData = mine.filter((fields) => Number.isFinite(fields[`${name}_n`]) && fields[`${name}_n`] > 0);
    return {
      intervals: withData.length,
      samples: withData.reduce((sum, fields) => sum + fields[`${name}_n`], 0),
      // Worst interval: the run-level analogue of the gate's p95 checks.
      p95MaxMs: withData.length ? Math.max(...withData.map((fields) => fields[`${name}_p95`])) : null,
      p50MaxMs: withData.length ? Math.max(...withData.map((fields) => fields[`${name}_p50`])) : null,
    };
  };
  // #372: the edge dimensions of the latest interval that has them,
  // reported only (local livekit-server: self_hosted_sfu=true, lk_region=
  // unknown, same_edge=true, ice host/udp).
  const edgeKeys = ['lk_region', 'same_edge', 'self_hosted_sfu', 'ice_candidate_type', 'ice_protocol'];
  const withEdge = mine.filter((fields) => edgeKeys.some((key) => fields[key] !== undefined)).at(-1);
  const edge = withEdge
    ? Object.fromEntries(edgeKeys.map((key) => [key, withEdge[key] === undefined ? null : String(withEdge[key])]))
    : null;
  // #378: run-level view of every stage and sum counter, for attribution.
  // p50Ms is the sample-weighted median of the interval p50s.
  const stages = {};
  const names = new Set(mine.flatMap((fields) => Object.keys(fields).filter((key) => key.endsWith('_n')).map((key) => key.slice(0, -2))));
  for (const name of names) {
    const withData = mine.filter((fields) => Number.isFinite(fields[`${name}_n`]) && fields[`${name}_n`] > 0 && Number.isFinite(fields[`${name}_p50`]));
    if (!withData.length) continue;
    const samples = withData.reduce((sum, fields) => sum + fields[`${name}_n`], 0);
    const byP50 = [...withData].sort((a, b) => a[`${name}_p50`] - b[`${name}_p50`]);
    let seen = 0;
    const median = byP50.find((fields) => (seen += fields[`${name}_n`]) >= samples / 2);
    stages[name] = {
      intervals: withData.length,
      samples,
      p50Ms: median[`${name}_p50`],
      p95MaxMs: Math.max(...withData.map((fields) => fields[`${name}_p95`] ?? -Infinity)),
    };
  }
  const counters = Object.fromEntries(SUMMARY_COUNTERS.map((key) => [
    key,
    mine.reduce((sum, fields) => sum + (Number.isFinite(fields[key]) ? fields[key] : 0), 0),
  ]));
  return {
    role,
    intervals: mine.length,
    finals: mine.filter((fields) => fields.final === true).length,
    replayInject: stage('replay_inject_ms'),
    replayComplete: stage('replay_complete_ms'),
    edge,
    stages,
    counters,
  };
}

// Sum-per-interval counters (rc_latency_telemetry.rs `Counter`); an
// interval omits a zero counter, so a role with intervals sums to 0.
export const SUMMARY_COUNTERS = ['input_stall_count', 'frames_dropped', 'freeze_count', 'nack_count', 'pli_count', 'freeze_duration_ms'];

// #378: the web controller's own summary lines (#375 receive stages), which
// the harness reads from the web session log into the results/photon JSON.
export function controllerSummaryLines(...reports) {
  return reports.flatMap((report) => (Array.isArray(report?.controllerLatencySummaryLines) ? report.controllerLatencySummaryLines : []));
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
  const latencySummary = summarizeLatencySummaries(parseLatencySummaryLog(hostLog));
  const controllerSummary = summarizeLatencySummaries(parseLatencySummaryLog(controllerSummaryLines(results, photon).join('\n')), 'controller');
  return {
    latencySummary,
    controllerSummary,
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
  const summary = metrics.latencySummary;
  notes.push(
    `  latency summary (#370, not gated): ${summary.intervals} host interval(s), ${summary.finals} final; ` +
      `replay_inject_ms worst p95 ${summary.replayInject.p95MaxMs ?? 'n/a'} over ${summary.replayInject.samples} samples ` +
      `(log receive->inject p95 ${metrics.hostReceiveToInject.p95Ms ?? 'n/a'}); ` +
      `replay_complete_ms worst p95 ${summary.replayComplete.p95MaxMs ?? 'n/a'} over ${summary.replayComplete.samples} samples ` +
      `(log receive->complete p95 ${metrics.hostReceiveToComplete.p95Ms ?? 'n/a'})`
  );
  const controller = metrics.controllerSummary;
  notes.push(
    `  web controller summary (#375, not gated): ${controller.intervals} interval(s); ` +
      `nack_count ${controller.counters.nack_count}, sub_rtt_ms p50 ${controller.stages.sub_rtt_ms?.p50Ms ?? 'n/a'}, ` +
      `capture_to_present_ms p50 ${controller.stages.capture_to_present_ms?.p50Ms ?? 'n/a'}`
  );
  notes.push(`  host replay timeouts: ${metrics.hostReplayTimeouts}; send->inject paired ${metrics.sendToInject.paired}/${metrics.sendToInject.publishes} controller publishes; observation methods ${JSON.stringify(metrics.targetObservation.methods)}, watcher max read gap ${metrics.targetObservation.maxReadGapMs ?? 'n/a'}ms`);
  const freeze = metrics.presentedFreeze;
  notes.push(freeze.error
    ? `  presented-video freeze probe: n/a (${freeze.error})`
    : `  presented-video freeze probe: ${freeze.samples} frames in ${freeze.durationMs}ms (${freeze.fps} fps), max gap ${freeze.maxGapMs}ms, ${freeze.freezes} gap(s) > 500ms totalling ${freeze.frozenMs}ms; decode-side ${JSON.stringify(freeze.decodeStall)}, playback ${JSON.stringify(freeze.playback)}`);
  for (const check of verdict.checks) {
    const line = `latency ${check.id}: ${check.status.toUpperCase()} -- ${check.detail} (${check.env})`;
    const blockingEnv = LATENCY_CHECKS.find((c) => c.id === check.id)?.blockingEnv;
    check.reportOnly = Boolean(blockingEnv) && !envFlagTrue(env, blockingEnv);
    if (check.status === 'pass') notes.push(line);
    else if (check.reportOnly) warnings.push(`REPORT-ONLY: would fail -- ${line} [blocks only when ${blockingEnv}=true]`);
    else errors.push(line);
  }
  const ok = verdict.checks.every((check) => check.status === 'pass' || check.reportOnly);
  return { ok, errors, warnings, notes, budgets, metrics, checks: verdict.checks };
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

// #378 2%-loss row, asserted only when the impaired pass ran at least this
// much loss each way (a `net_loss_pct` red-proof); no VM run has measured
// the gate profile's 1% here yet. Input counters are reported, not asserted:
// since #421 every ordered input rides redundant lossy bundles and its
// receive stamp is the first copy's arrival, so 2% loss is meant NOT to stall
// input (0 stalls at 1% in run 43067248846). Video NACKs are the loss signal.
export const LOSS_ROW_MIN_PCT = 2;
export const LOSS_ATTRIBUTION_ROWS = [
  { role: 'controller', counter: 'nack_count', rule: 'must-increase' },
  { role: 'host', stage: 'replay_inject_ms', rule: 'must-not-move' },
  { role: 'host', stage: 'replay_complete_ms', rule: 'must-not-move' },
  { role: 'host', counter: 'input_stall_count', rule: 'report' },
  { role: 'host', stage: 'input_excess_delay_ms', stat: 'p95MaxMs', rule: 'report' },
];

// #378 replay-delay row: a baseline run vs a `rc_test_replay_delay_ms` red
// run. The knob stalls the host before every inject and is read once at
// launch, so both passes of one run carry it: this row compares two RUNS.
// Optional rows are n/a unless both runs produced the stage. photon_ms (P6)
// has no producer yet; pressToPhoton is reported only.
export const REPLAY_ATTRIBUTION_ROWS = [
  { role: 'host', stage: 'replay_inject_ms', rule: 'must-move' },
  { role: 'host', stage: 'replay_complete_ms', rule: 'must-move' },
  { metric: 'hostReceiveToInject', rule: 'must-move' },
  { metric: 'sendToReceive', rule: 'must-not-move' },
  { role: 'host', stage: 'inject_to_capture_ms', rule: 'must-not-move' },
  { role: 'host', stage: 'input_excess_delay_ms', rule: 'must-not-move' },
  { role: 'host', stage: 'input_leg_ms', rule: 'must-not-move', optional: true },
  { role: 'controller', stage: 'pub_rtt_ms', rule: 'must-not-move', optional: true },
  { role: 'controller', stage: 'sub_rtt_ms', rule: 'must-not-move', optional: true },
  { role: 'controller', stage: 'capture_to_present_ms', rule: 'must-not-move', optional: true },
  { metric: 'pressToPhoton', rule: 'report' },
];

function rowName(row) {
  if (row.metric) return row.metric;
  return `${row.role} ${row.stage ?? row.counter}${row.stat && row.stat !== 'p50Ms' ? ` ${row.stat.replace('Ms', '')}` : ''}`;
}

// A row's value from one latency `--json`: a per-event metric's p50, a
// summary stage's run-level p50 (or `stat`), or a summary counter's sum.
// null when the evidence is absent (a counter needs >= 1 interval).
function rowValue(json, row) {
  const metrics = json?.metrics;
  let value;
  if (row.metric) {
    value = metrics?.[row.metric]?.p50Ms;
  } else {
    const summary = row.role === 'controller' ? metrics?.controllerSummary : metrics?.latencySummary;
    if (row.counter) value = summary?.intervals > 0 ? summary.counters?.[row.counter] : null;
    else value = summary?.stages?.[row.stage]?.[row.stat ?? 'p50Ms'];
  }
  return Number.isFinite(value) ? value : null;
}

// `expectedMs` is the delay the injection should add to a must-move row.
function evaluateAttributionRow(row, before, after, expectedMs, [was, now] = ['clean', 'impaired']) {
  const name = rowName(row);
  const base = { ...row, name, before, after };
  if (before === null || after === null) {
    if (row.rule === 'report' || row.optional) return { ...base, movedMs: null, status: 'pass', detail: `n/a (${was} ${before}, ${now} ${after}; ${row.optional ? 'optional, not asserted' : 'reported only'})` };
    return { ...base, movedMs: null, status: 'insufficient', detail: `evidence missing (${was} ${before}, ${now} ${after})` };
  }
  const movedMs = Math.round((after - before) * 10) / 10;
  if (row.rule === 'must-increase') {
    const ok = after > before && after >= 1;
    return { ...base, movedMs, status: ok ? 'pass' : 'fail', detail: `count ${before} -> ${after}; must rise above ${was} and be >= 1` };
  }
  if (row.rule === 'must-move') {
    const needMs = ATTRIBUTION_MOVE_FRACTION * expectedMs;
    const ok = movedMs >= needMs;
    return { ...base, movedMs, status: ok ? 'pass' : 'fail', detail: `${before} -> ${after}ms moved ${movedMs}ms; expected ~${expectedMs}ms, need >= ${needMs}ms` };
  }
  if (row.rule === 'report') {
    return { ...base, movedMs, status: 'pass', detail: `${before} -> ${after} moved ${movedMs} (reported only)` };
  }
  const ok = Math.abs(movedMs) <= ATTRIBUTION_STILL_TOLERANCE_MS;
  return { ...base, movedMs, status: ok ? 'pass' : 'fail', detail: `${before} -> ${after}ms moved ${movedMs}ms; must stay within ${ATTRIBUTION_STILL_TOLERANCE_MS}ms` };
}

function pushRows(rows, label, notes, errors) {
  for (const row of rows) {
    const line = `attribution ${label}${row.name} (${row.rule}): ${row.status.toUpperCase()} -- ${row.detail}`;
    if (row.status === 'pass') notes.push(line);
    else errors.push(line);
  }
}

export function attributionVerdict({ clean, impaired, delayMs, lossPct }) {
  const errors = [];
  const notes = [];
  const delay = Number(delayMs);
  if (!Number.isFinite(delay) || delay <= 0) {
    errors.push(`attribution: impairment delay must be a positive number of ms, got ${JSON.stringify(delayMs)}`);
    return { ok: false, errors, warnings: [], notes, rows: [] };
  }
  const rows = ATTRIBUTION_ROWS.map((row) => evaluateAttributionRow(row, rowValue(clean, row), rowValue(impaired, row), row.traversals * delay));
  pushRows(rows, '', notes, errors);
  let lossRows = [];
  if (lossPct !== undefined && lossPct !== null && `${lossPct}`.trim() !== '') {
    const loss = Number(lossPct);
    if (!Number.isFinite(loss) || loss < 0) {
      errors.push(`attribution: impairment loss must be a non-negative number of %, got ${JSON.stringify(lossPct)}`);
    } else if (loss >= LOSS_ROW_MIN_PCT) {
      lossRows = LOSS_ATTRIBUTION_ROWS.map((row) => evaluateAttributionRow(row, rowValue(clean, row), rowValue(impaired, row), 0));
      pushRows(lossRows, `loss ${loss}%: `, notes, errors);
    } else {
      notes.push(`attribution loss row: not asserted at ${loss}% (asserted from ${LOSS_ROW_MIN_PCT}%, a net_loss_pct red-proof)`);
    }
  }
  return { ok: errors.length === 0, errors, warnings: [], notes, rows, lossRows };
}

export function replayAttributionVerdict({ baseline, red, delayMs }) {
  const errors = [];
  const notes = [];
  const delay = Number(delayMs);
  if (!Number.isFinite(delay) || delay <= 0) {
    errors.push(`attribution: replay delay must be a positive number of ms, got ${JSON.stringify(delayMs)}`);
    return { ok: false, errors, warnings: [], notes, rows: [] };
  }
  const rows = REPLAY_ATTRIBUTION_ROWS.map((row) => evaluateAttributionRow(row, rowValue(baseline, row), rowValue(red, row), delay, ['baseline', 'red']));
  pushRows(rows, `replay ${delay}ms: `, notes, errors);
  return { ok: errors.length === 0, errors, warnings: [], notes, rows };
}
