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
