function roundMs(value) {
  return Math.round(value * 10) / 10;
}

export function nearestRankPercentile(values, percentile) {
  if (!Array.isArray(values) || values.length === 0) return null;
  if (!Number.isFinite(percentile) || percentile < 0 || percentile > 1) {
    throw new Error(`percentile must be between 0 and 1, got ${percentile}`);
  }
  const sorted = values
    .filter((value) => Number.isFinite(value))
    .slice()
    .sort((left, right) => left - right);
  if (sorted.length === 0) return null;
  const index = Math.max(0, Math.ceil(sorted.length * percentile) - 1);
  return sorted[index];
}

function summarizeKind(samples, p95BudgetMs) {
  const values = samples
    .map((sample) => sample.pressToEstimatedPhotonMs)
    .filter((value) => Number.isFinite(value));
  if (values.length === 0) {
    return { samples: 0, minMs: null, medianMs: null, p95Ms: null, maxMs: null, pass: false };
  }
  const sorted = values.slice().sort((left, right) => left - right);
  const p95Ms = nearestRankPercentile(sorted, 0.95);
  return {
    samples: sorted.length,
    minMs: roundMs(sorted[0]),
    medianMs: roundMs(nearestRankPercentile(sorted, 0.5)),
    p95Ms: roundMs(p95Ms),
    maxMs: roundMs(sorted.at(-1)),
    pass: p95Ms <= p95BudgetMs
  };
}

export function summarizePhotonSamples(samples, p95BudgetMs) {
  if (!Number.isFinite(p95BudgetMs) || p95BudgetMs <= 0) {
    throw new Error(`p95 budget must be a positive number, got ${p95BudgetMs}`);
  }
  const kinds = Array.from(new Set(samples.map((sample) => sample.inputKind))).sort();
  const byInput = Object.fromEntries(
    kinds.map((kind) => [kind, summarizeKind(samples.filter((sample) => sample.inputKind === kind), p95BudgetMs)])
  );
  const overall = summarizeKind(samples, p95BudgetMs);
  return {
    metric: 'web press to estimated browser display time',
    p95BudgetMs,
    samples: samples.length,
    pass: overall.pass && Object.values(byInput).every((summary) => summary.pass),
    overall,
    byInput
  };
}

const HOST_INJECT_RE = /remote-control-latency: host inject_ts_ms=(\d+) kind=(\w+)[^\n]*? seq=(\d+)/;
const HOST_PUSH_RE = /remote-control-latency: host frame_pushed_ts_ms=(\d+) window=\d+ input_seq=(\d+) since_input_ms=\d+ path=(\w+) accepted=(true|false)/;

/**
 * Splits one press-to-photon sample into host-clock stages, all relative to
 * the browser publish (`sentAtEpochMs`, same machine in the loopback):
 * inject -> sentinel saw the input -> sentinel redrew the new generation ->
 * frames the host pump handed WebRTC (and whether the source accepted them).
 * Any stage it cannot attribute is null rather than guessed.
 */
export function photonStageSplit(sample, sentinelEvents, hostLogText, windowMs = 2000) {
  const sent = sample?.sentAtEpochMs;
  if (!Number.isFinite(sent)) return null;
  const inWindow = (t) => Number.isFinite(t) && t >= sent - 5 && t <= sent + windowMs;
  const rel = (t) => (Number.isFinite(t) ? roundMs(t - sent) : null);
  const events = Array.isArray(sentinelEvents) ? sentinelEvents : [];
  const input = events.find((event) => inWindow(Number(event.tMs)) && (
    (event.kind === 'event' && event.type === 'keyDown') ||
    (event.kind === 'axAction')
  ));
  const redraw = events.find((event) =>
    event.kind === 'redraw' && event.generation === sample.observedGeneration && inWindow(Number(event.tMs)));

  let inject = null;
  const pushes = [];
  const lines = String(hostLogText ?? '').split('\n');
  for (const line of lines) {
    const match = HOST_INJECT_RE.exec(line);
    if (match && !inject && inWindow(Number(match[1]))) {
      inject = { tsMs: Number(match[1]), kind: match[2], seq: Number(match[3]) };
    }
  }
  if (inject) {
    for (const line of lines) {
      const match = HOST_PUSH_RE.exec(line);
      if (match && Number(match[2]) === inject.seq) {
        pushes.push({ atMs: rel(Number(match[1])), path: match[3], accepted: match[4] === 'true' });
      }
    }
  }
  return {
    hostSeq: inject?.seq ?? null,
    injectMs: rel(inject?.tsMs),
    sentinelInputMs: rel(Number(input?.tMs)),
    sentinelRedrawMs: rel(Number(redraw?.tMs)),
    pushes,
    photonMs: Number.isFinite(sample.pressToEstimatedPhotonMs) ? sample.pressToEstimatedPhotonMs : null,
  };
}
