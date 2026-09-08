#!/usr/bin/env node
// #45: one over-budget target observation must not fail an otherwise-correct
// case, and a genuine latency regression must still fail.
//
// The defect these tests pin: the release e2e gate failed twice in one evening
// (runs 34078048040 and 34081151407) on a single sample -- 605.3ms and 699.2ms
// against a 500ms budget -- while the rest of the same run sat at p95 460ms.
// The correctness assertion had passed in both. The old code measured once,
// compared once, and threw, so a Tart-guest scheduling hiccup read as a
// product regression and cost a ~10 minute manual re-run per release.
//
// These drive the real retry loop (measureObservationWithRetry) with an
// injected clock, not a re-implementation of it.
import assert from 'node:assert/strict';
import path from 'node:path';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  CORRECTNESS_TIMEOUT_BUDGET_MULTIPLE,
  DEFAULT_OBSERVATION_BUDGET_MS,
  MAX_OBSERVATION_ATTEMPTS,
  correctnessTimeoutMs,
  evaluateObservationSamples,
  measureObservationWithRetry,
  resolveObservationBudgetMs,
  summarizeObservationLatency,
} from './remote-control-observation.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '../../..');

// A clock that advances by a scripted amount per attempt, so a test can say
// "attempt 1 took 605.3ms, attempt 2 took 431.0ms" without sleeping.
function scriptedClock(durationsMs) {
  // now() is called exactly twice per attempt: once before the action, once
  // after the observation. The second call is the one that advances.
  let elapsed = 0;
  let calls = 0;
  return () => {
    if (calls % 2 === 1) elapsed += durationsMs[(calls - 1) / 2];
    calls += 1;
    return elapsed;
  };
}

function harness(durationsMs, { budgetMs = 500, onObserve = () => {} } = {}) {
  const prepared = [];
  const retries = [];
  return {
    prepared,
    retries,
    run: () => measureObservationWithRetry({
      label: 'clipboard equals selected TextEdit document',
      budgetMs,
      now: scriptedClock(durationsMs),
      onRetry: (info) => retries.push(info),
      prepare: (attempt) => {
        prepared.push(attempt);
        return {
          action: async () => {},
          observe: async () => onObserve(attempt),
        };
      },
    }),
  };
}

test('a single over-budget observation is retried once and passes on the second sample', async () => {
  // Exactly run 34078048040's shape: 605.3ms outlier, healthy retry.
  const rig = harness([605.3, 431.0]);
  const measurement = await rig.run();

  assert.deepEqual(rig.prepared, [1, 2], 'the retry must re-arm the target, not re-read attempt 1');
  assert.equal(measurement.targetObservationAttempts, 2);
  assert.deepEqual(measurement.targetObservationSamplesMs, [605.3, 431.0], 'BOTH samples are recorded');
  assert.equal(measurement.targetObservationLatencyMs, 431.0, 'the accepted sample is the in-budget one');
  assert.equal(measurement.targetObservationBudgetMs, 500);
  assert.equal(rig.retries.length, 1);
  assert.equal(rig.retries[0].sampleMs, 605.3);
});

test('an observation inside the budget never retries', async () => {
  const rig = harness([460.3]);
  const measurement = await rig.run();
  assert.deepEqual(rig.prepared, [1]);
  assert.deepEqual(measurement.targetObservationSamplesMs, [460.3]);
  assert.equal(measurement.targetObservationAttempts, 1);
  assert.equal(rig.retries.length, 0);
});

test('a real latency regression still fails, with both samples on the error', async () => {
  const rig = harness([812.0, 947.5]);
  const thrown = await rig.run().catch((error) => error);
  assert.ok(thrown instanceof Error);
  assert.match(thrown.message, /exceeded the 500ms input budget on all 2 attempt\(s\)/);
  assert.match(thrown.message, /812ms, 947\.5ms/);
  assert.match(thrown.message, /correctness passed every attempt/);
  assert.deepEqual(thrown.observation.targetObservationSamplesMs, [812.0, 947.5]);
  assert.equal(thrown.observation.targetObservationAttempts, 2);
  assert.deepEqual(rig.prepared, [1, 2]);
});

test('the retry is bounded to MAX_OBSERVATION_ATTEMPTS', async () => {
  assert.equal(MAX_OBSERVATION_ATTEMPTS, 2);
  const rig = harness([900, 900, 900]);
  await rig.run().catch(() => {});
  assert.deepEqual(rig.prepared, [1, 2], 'a third attempt would start hiding a systemic slowdown');
});

test('a correctness failure is never retried away as a latency outlier', async () => {
  const rig = harness([120, 120], {
    onObserve: () => {
      throw new Error('timed out waiting for TextEdit document contains paste-1');
    },
  });
  const thrown = await rig.run().catch((error) => error);
  assert.match(thrown.message, /timed out waiting for TextEdit document/);
  assert.equal(thrown.observation, undefined, 'a correctness failure carries no budget verdict');
  assert.deepEqual(rig.prepared, [1], 'correctness failures propagate on the first attempt');
});

test('a slow-but-correct observation is a budget verdict, not a correctness timeout', () => {
  // The two are separate checks now: correctness gets 3x the budget, so the
  // 699.2ms sample from run 34081151407 lands well inside its own timeout and
  // is judged on latency alone.
  assert.equal(CORRECTNESS_TIMEOUT_BUDGET_MULTIPLE, 3);
  assert.ok(correctnessTimeoutMs(500) > 699.2);
  assert.equal(correctnessTimeoutMs(500), 1500);
  assert.equal(correctnessTimeoutMs(620), 1860);
  assert.ok(correctnessTimeoutMs(200) >= 1500, 'a tiny budget must not shrink the correctness window');
});

test('the budget is runner-aware and defaults to 500ms on bare metal', () => {
  assert.equal(DEFAULT_OBSERVATION_BUDGET_MS, 500);
  assert.deepEqual(resolveObservationBudgetMs({}), { budgetMs: 500, source: 'default' });
  assert.deepEqual(resolveObservationBudgetMs({ PETAL_RC_OBSERVATION_BUDGET_MS: '' }), {
    budgetMs: 500,
    source: 'default',
  });
  assert.deepEqual(resolveObservationBudgetMs({ PETAL_RC_OBSERVATION_BUDGET_MS: '620' }), {
    budgetMs: 620,
    source: 'PETAL_RC_OBSERVATION_BUDGET_MS',
  });
  // Legacy name still honored for existing rigs.
  assert.deepEqual(resolveObservationBudgetMs({ PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS: '750' }), {
    budgetMs: 750,
    source: 'PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS',
  });
  // New name wins when both are set.
  assert.equal(
    resolveObservationBudgetMs({
      PETAL_RC_OBSERVATION_BUDGET_MS: '620',
      PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS: '750',
    }).budgetMs,
    620,
  );
  assert.throws(() => resolveObservationBudgetMs({ PETAL_RC_OBSERVATION_BUDGET_MS: 'soon' }), /positive number/);
  assert.throws(() => resolveObservationBudgetMs({ PETAL_RC_OBSERVATION_BUDGET_MS: '0' }), /positive number/);
  assert.throws(() => resolveObservationBudgetMs({ PETAL_RC_OBSERVATION_BUDGET_MS: '-1' }), /positive number/);
});

test('evaluateObservationSamples accepts a run where any attempt landed in budget', () => {
  assert.equal(evaluateObservationSamples([605.3, 431.0], 500).withinBudget, true);
  assert.equal(evaluateObservationSamples([605.3, 431.0], 500).acceptedMs, 431.0);
  assert.equal(evaluateObservationSamples([605.3, 431.0], 500).overBudgetSamples, 1);
  assert.equal(evaluateObservationSamples([812, 947.5], 500).withinBudget, false);
  // The reported latency of a failure is the last sample, not null: the RESULT
  // line must still carry a number.
  assert.equal(evaluateObservationSamples([812, 947.5], 500).acceptedMs, 947.5);
});

test('the scorecard keeps a rescued outlier visible in the distribution', () => {
  // Plan item 3: no case failed here, but maxMs/overBudgetSamples still say a
  // sample blew the budget, so a creeping regression cannot hide behind the
  // retry.
  const scorecard = summarizeObservationLatency({
    samplesMs: [431.0, 605.3, 402.1, 460.3, 388.7, 455.9],
    budgetMs: 500,
    budgetSource: 'PETAL_RC_OBSERVATION_BUDGET_MS',
    retriedObservations: 1,
  });
  assert.equal(scorecard.samples, 6);
  assert.equal(scorecard.minMs, 388.7);
  assert.equal(scorecard.p50Ms, 431.0);
  assert.equal(scorecard.p95Ms, 605.3);
  assert.equal(scorecard.maxMs, 605.3);
  assert.equal(scorecard.overBudgetSamples, 1);
  assert.equal(scorecard.retriedObservations, 1);
  assert.equal(scorecard.budgetSource, 'PETAL_RC_OBSERVATION_BUDGET_MS');
  assert.equal(scorecard.budgetMs, 500);
});

test('the scorecard survives a run with no measured observations', () => {
  const scorecard = summarizeObservationLatency({ samplesMs: [], budgetMs: 500 });
  assert.deepEqual(scorecard, {
    budgetMs: 500,
    budgetSource: 'default',
    samples: 0,
    minMs: null,
    p50Ms: null,
    p95Ms: null,
    maxMs: null,
    overBudgetSamples: 0,
    retriedObservations: 0,
  });
});

test('every scenario call site re-arms per attempt', () => {
  // A prepare() that closes over a marker built ONCE outside it would make the
  // retry a no-op measurement (~0ms) -- a false pass that looks like a fix.
  // Every measure* call site must therefore take an `(attempt) =>` factory.
  const scenario = readFileSync(path.join(scriptDir, 'remote-control-scenario.mjs'), 'utf8');
  const callSites = scenario.match(/await measure(?:TargetObservation|DocumentInput)\(/g) ?? [];
  assert.ok(callSites.length >= 7, `expected the known measurement call sites, found ${callSites.length}`);
  for (const match of scenario.matchAll(/await measure(TargetObservation|DocumentInput)\(([\s\S]{0,900}?)\n      \);/g)) {
    assert.match(match[2], /\(attempt\) =>/, `measure${match[1]} call site does not re-arm per attempt:\n${match[2]}`);
  }
  // And the correctness waits must use the correctness timeout, never the
  // budget -- conflating them is what made "slow but correct" indistinguishable
  // from "never happened".
  assert.doesNotMatch(scenario, /, inputBudgetMs\)/);
  assert.ok(scenario.includes('observationCorrectnessTimeoutMs'));
});

test('the VM budget in nightly-loopback.yml is derived from the observed VM p95', () => {
  const workflow = readFileSync(path.join(repoRoot, '.github/workflows/nightly-loopback.yml'), 'utf8');
  const match = workflow.match(/PETAL_RC_OBSERVATION_BUDGET_MS:\s*"(\d+)"/);
  assert.ok(match, 'the self-hosted job must set the VM observation budget explicitly');
  const vmBudgetMs = Number(match[1]);
  // Observed healthy VM samples: p95 460.3ms (run 34078048040) and 459.3ms
  // (run 34081151407). The allowance is headroom over that measurement, not a
  // number chosen to swallow the 605.3/699.2ms outliers -- those are the
  // retry's job.
  assert.ok(vmBudgetMs > 460.3, 'the VM budget must clear the observed healthy p95');
  assert.ok(vmBudgetMs < 699.2, 'the VM budget must NOT be wide enough to swallow the observed outliers');
  assert.ok(vmBudgetMs > DEFAULT_OBSERVATION_BUDGET_MS, 'and it is an allowance, not the bare-metal default');
  assert.match(workflow, /#45/, 'the allowance needs its justification in the workflow');
});
