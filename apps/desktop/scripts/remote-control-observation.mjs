// Target-observation latency policy for the numbered remote-control suite:
// how a measured observation becomes a verdict, and how the run's latency
// distribution is reported.
//
// #45: the release e2e gate failed twice in one evening on a SINGLE sample
// (605.3ms and 699.2ms against a 500ms budget) while every other sample in the
// same run sat at p95 460ms. The correctness assertion passed both times --
// only the budget failed -- so the gate was reporting a scheduling hiccup in a
// Tart guest as a product regression. Two separate mistakes were baked into
// the old one-liner:
//
//   1. Correctness and latency were the SAME check. The observe() wait was
//      given the budget as its timeout, so "slow but correct" and "never
//      happened" were indistinguishable. They are split here: correctness gets
//      its own, wider timeout (`correctnessTimeoutMs`), and the budget is a
//      verdict applied to the elapsed time AFTERWARDS.
//   2. One sample decided the verdict. A single observation is retried once --
//      against a freshly re-armed target, never a re-read of the first
//      attempt's result -- and BOTH samples are recorded, so a real regression
//      (both attempts slow, or the distribution creeping up) still fails while
//      a lone outlier does not.
//
// Do NOT "fix" a slow runner by raising DEFAULT_OBSERVATION_BUDGET_MS. The
// budget is runner-aware: bare metal keeps 500ms and the VM sets
// PETAL_RC_OBSERVATION_BUDGET_MS in .github/workflows/nightly-loopback.yml.

export const DEFAULT_OBSERVATION_BUDGET_MS = 500;

// One retry, not a loop. Two attempts is what separates "a scheduler hiccup
// hit this one observation" from "this path is slow now"; more attempts would
// start hiding the second case.
export const MAX_OBSERVATION_ATTEMPTS = 2;

// Correctness must not be gated on the latency budget (mistake 1 above), but
// it still needs SOME bound or a broken target hangs the case. 3x the budget
// is wide enough that every over-budget-but-correct sample seen on the VM
// (699.2ms against a 500ms budget) lands inside it with room to spare, and
// tight enough that a genuinely broken observation fails in seconds.
export const CORRECTNESS_TIMEOUT_BUDGET_MULTIPLE = 3;
const MIN_CORRECTNESS_TIMEOUT_MS = 1500;

export function roundMs(value) {
  return Math.round(value * 10) / 10;
}

// Legacy name first-class, new name preferred: PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS
// predates #45 and is still honored so existing rigs keep working.
export function resolveObservationBudgetMs(env = {}) {
  for (const name of ['PETAL_RC_OBSERVATION_BUDGET_MS', 'PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS']) {
    const raw = env[name];
    if (raw === undefined || raw === null || `${raw}`.trim() === '') continue;
    const budgetMs = Number(raw);
    if (!Number.isFinite(budgetMs) || budgetMs <= 0) {
      throw new Error(`${name} must be a positive number, got ${JSON.stringify(raw)}`);
    }
    return { budgetMs, source: name };
  }
  return { budgetMs: DEFAULT_OBSERVATION_BUDGET_MS, source: 'default' };
}

export function correctnessTimeoutMs(budgetMs) {
  return Math.max(MIN_CORRECTNESS_TIMEOUT_MS, Math.round(budgetMs * CORRECTNESS_TIMEOUT_BUDGET_MULTIPLE));
}

// A case passes the budget if ANY attempt landed inside it; the accepted
// sample is the one that did. The rejected sample is never discarded -- it
// stays in `samplesMs` and in the run's distribution.
export function evaluateObservationSamples(samplesMs, budgetMs) {
  const within = samplesMs.filter((sample) => sample <= budgetMs);
  return {
    withinBudget: within.length > 0,
    acceptedMs: within.length > 0 ? within.at(-1) : (samplesMs.at(-1) ?? null),
    overBudgetSamples: samplesMs.length - within.length,
    attempts: samplesMs.length,
  };
}

export function observationBudgetFailureMessage(label, samplesMs, budgetMs) {
  const samples = samplesMs.map((sample) => `${sample}ms`).join(', ');
  return `${label} target observation exceeded the ${budgetMs}ms input budget on all `
    + `${samplesMs.length} attempt(s) [${samples}]; correctness passed every attempt`;
}

function percentile(sorted, fraction) {
  if (sorted.length === 0) return null;
  const index = Math.max(0, Math.ceil(sorted.length * fraction) - 1);
  return sorted[index] ?? null;
}

// Scorecard metric (#45 plan item 3): the distribution is reported on every
// run, pass or fail, so a budget that is never breached still shows a median
// creeping toward it.
export function summarizeObservationLatency({
  samplesMs = [],
  budgetMs,
  budgetSource = 'default',
  retriedObservations = 0,
}) {
  const sorted = samplesMs.filter((sample) => Number.isFinite(sample)).sort((a, b) => a - b);
  return {
    budgetMs,
    budgetSource,
    samples: sorted.length,
    minMs: sorted[0] ?? null,
    p50Ms: percentile(sorted, 0.5),
    p95Ms: percentile(sorted, 0.95),
    maxMs: sorted.at(-1) ?? null,
    overBudgetSamples: sorted.filter((sample) => sample > budgetMs).length,
    retriedObservations,
  };
}

// The retry driver itself. `prepare(attempt)` must RE-ARM the target and hand
// back a fresh { action, observe } pair: without that a retry would re-read
// the first attempt's own result (the marker is already in the document, the
// clipboard already holds the selection) and clock a meaningless ~0ms.
// Timing covers action + observe only, never the re-arm.
export async function measureObservationWithRetry({
  label,
  prepare,
  budgetMs,
  maxAttempts = MAX_OBSERVATION_ATTEMPTS,
  now = () => performance.now(),
  onRetry = () => {},
}) {
  const samplesMs = [];
  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    const { action, observe } = await prepare(attempt);
    const started = now();
    await action();
    // Correctness. A throw here is a REAL failure and is never retried for
    // latency reasons -- it propagates with the case's own message.
    await observe();
    const sampleMs = roundMs(now() - started);
    samplesMs.push(sampleMs);
    if (sampleMs <= budgetMs) break;
    if (attempt < maxAttempts) {
      onRetry({ label, attempt, sampleMs, budgetMs });
    }
  }

  const verdict = evaluateObservationSamples(samplesMs, budgetMs);
  const measurement = {
    targetObservation: label,
    targetObservationLatencyMs: verdict.acceptedMs,
    targetObservationSamplesMs: samplesMs,
    targetObservationAttempts: verdict.attempts,
    targetObservationBudgetMs: budgetMs,
  };
  if (!verdict.withinBudget) {
    const error = new Error(observationBudgetFailureMessage(label, samplesMs, budgetMs));
    // runCase re-attaches this so a budget failure still reports BOTH samples
    // in its RESULT line instead of only the message.
    error.observation = measurement;
    throw error;
  }
  return measurement;
}
