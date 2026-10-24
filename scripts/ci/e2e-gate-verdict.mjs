#!/usr/bin/env node
// The release e2e gate's verdict for BOTH tiers of nightly-loopback.yml.
//
// Every check here closes a path by which a broken run used to read as green:
// the loopback scorecard failed only on `status == "fail"` (zero passes, an
// unexplained skip, a #580 tokenless drop, or a non-zero harness exit all
// passed), and the Cockpit tier trusted whatever test-runs directory was
// newest -- including one left by an EARLIER run. Absence of evidence is
// never a pass here. Unit tests (both directions): scripts/test-e2e-gate-verdict.mjs.
//
//   node scripts/ci/e2e-gate-verdict.mjs loopback --results <json> --harness-exit <n> [--quarantine <ids>] [--required <ids>]
//   node scripts/ci/e2e-gate-verdict.mjs latency  --results <json> --photon <json> --host-log <petal log> [--json <out>]
//   node scripts/ci/e2e-gate-verdict.mjs attribution --clean <latency json> --impaired <latency json> --delay-ms <n>
//   Any tier takes --pass clean|impaired (default clean). An impaired-pass
//   verdict is REPORT-ONLY (failures print as warnings, exit 0) unless
//   E2E_IMPAIRED_BLOCKING=true (#378; nightly-loopback.yml sets it).
//   node scripts/ci/e2e-gate-verdict.mjs cockpit  --run-dir <dir> --started-at-ms <ms> --selector <sel> --engine-exit <n> [--quarantine <ids>]
//   node scripts/ci/e2e-gate-verdict.mjs freeze   --raw <share-freeze-raw.json> [--sender-log <sharer petal log>] [--json <report out>]
//   The freeze tier is REPORT-ONLY unless E2E_FREEZE_SOAK_BLOCKING=true (#391;
//   nightly-loopback.yml sets it).
//
// Prints GitHub ::error/::warning annotations; exits 0 (pass) or 1 (fail).

import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { attributionVerdict, envFlagTrue, IMPAIRED_BLOCKING_ENV, latencyVerdict } from './remote-control-latency-verdict.mjs';
import { freezeVerdict } from './share-freeze-verdict.mjs';

// Skips that are structurally unavoidable on the release runner and carry no
// signal there. Keyed by case id AND reason: the same case skipping for any
// OTHER reason (AX unavailable, a dead fixture) fails the gate. Observed on
// every healthy VM run checked (release gates 38987683137, 38926248564;
// dispatches 35953250430, 35266967135): exactly these two, nothing else.
// AX-unavailable skips (cases 6/12/16/17) are deliberately NOT here: on this
// runner they mean the Accessibility grant broke, which is a real outage.
export const EXPECTED_LOOPBACK_SKIPS = Object.freeze([
  Object.freeze({
    caseId: '23',
    reason: /^displayplacer reports fewer than two displays$/,
    why: 'the Tart guest has one 1920x1080 display (scripts/runner/tart/make-golden.sh)',
  }),
  Object.freeze({
    caseId: '29',
    reason: /^one-shot reconnect already used by an earlier pass in this Petal process \(PETAL_RC_RECONNECT_ALREADY_USED\)$/,
    why: 'the #378 impaired pass reuses the clean pass\'s Petal, whose reconnect simulation is one-shot (autotest.rs); only set for that pass',
  }),
  Object.freeze({
    caseId: '30',
    reason: /^host advertises legacy control \(macOS by contract; docs\/CONTRACTS\.md\)/,
    why: 'a macOS host speaks legacy control by contract, so v2 cached-terminal replay is unreachable',
  }),
]);

export function parseIdList(value) {
  return new Set(String(value ?? '').split(',').map((s) => s.trim()).filter(Boolean));
}

function clip(value, n = 300) {
  return String(value ?? '').replace(/\s+/g, ' ').slice(0, n);
}

/// report: the parsed --json results file (or null when it is missing).
/// harnessExit: remote-control-local-loopback.mjs's exit status.
/// required: case ids that must PASS -- missing, skipped or failed (even
/// quarantined) fails. The display-share cases 37-41 (#349/#353) use it.
export function loopbackVerdict({ report, harnessExit, quarantine = new Set(), required = new Set() }) {
  const errors = [];
  const warnings = [];
  const notes = [];
  const done = () => ({ ok: errors.length === 0, errors, warnings, notes });

  if (!report || typeof report !== 'object') {
    errors.push('loopback produced no results JSON (or it did not parse) -- nothing was proved');
    return done();
  }
  const results = Array.isArray(report.results) ? report.results : [];
  const summary = report.summary && typeof report.summary === 'object' ? report.summary : null;
  if (results.length === 0) {
    errors.push('loopback results JSON has zero cases (INSUFFICIENT DATA)');
    return done();
  }
  if (!summary) {
    errors.push('loopback results JSON has no summary -- the suite did not finish its accounting');
    return done();
  }
  if (summary.total !== results.length) {
    errors.push(`summary.total=${summary.total} but the results list holds ${results.length} cases -- the scorecard is inconsistent`);
  }
  const ids = results.map((c) => String(c.caseId));
  const dupes = [...new Set(ids.filter((id, i) => ids.indexOf(id) !== i))];
  if (dupes.length) errors.push(`case id(s) reported more than once: ${dupes.join(',')}`);
  // The release gate runs the full numbered suite with the live-tile share
  // gate; an --input-only run proves less and must not stand in for it.
  if (summary.mode !== 'numbered' || summary.shareReadiness !== 'live-tile') {
    errors.push(`suite ran mode=${summary.mode} shareReadiness=${summary.shareReadiness}; the gate requires mode=numbered shareReadiness=live-tile`);
  }

  const passed = results.filter((c) => c.status === 'pass');
  if (passed.length === 0) errors.push(`zero of ${results.length} cases passed -- a run that proves nothing is not a pass`);

  const quarantinedFails = [];
  for (const c of results) {
    const id = String(c.caseId);
    const label = `loopback case ${id} '${c.name ?? ''}'`;
    if (c.status === 'pass') continue;
    if (c.status === 'fail') {
      if (quarantine.has(id)) {
        quarantinedFails.push(id);
        warnings.push(`${label}: fail -- QUARANTINED (known failure, tracked in an issue; NOT a pass) ${clip(c.detail)}`);
      } else {
        errors.push(`${label}: fail -- ${clip(c.detail)}`);
      }
      continue;
    }
    if (c.status === 'skip') {
      const expected = EXPECTED_LOOPBACK_SKIPS.find((s) => s.caseId === id && s.reason.test(String(c.detail ?? '')));
      if (expected) {
        notes.push(`${label}: expected skip on this runner (${expected.why})`);
      } else if (quarantine.has(id)) {
        warnings.push(`${label}: skip -- QUARANTINED (known, tracked in an issue; NOT a pass) ${clip(c.detail)}`);
      } else {
        errors.push(`${label}: unexpected skip -- ${clip(c.detail)} (a skip proves nothing; add it to EXPECTED_LOOPBACK_SKIPS with evidence, or quarantine it with an issue)`);
      }
      continue;
    }
    errors.push(`${label}: unknown status '${c.status}'`);
  }

  for (const id of required) {
    const c = results.find((r) => String(r.caseId) === id);
    if (!c) {
      errors.push(`required loopback case ${id} produced no result`);
    } else if (c.status !== 'pass') {
      const quarantined = quarantine.has(id) ? ' (quarantine does not excuse a required case; remove it from E2E_REQUIRED_RC_CASES first)' : '';
      errors.push(`required loopback case ${id} '${c.name ?? ''}' did not pass: ${c.status}${quarantined} -- ${clip(c.detail)}`);
    }
  }

  // #580: a tokenless drop means the host never injected what the suite
  // claims it did. Missing is not zero.
  if (!Number.isInteger(summary.tokenlessDrops)) {
    errors.push(`summary.tokenlessDrops is missing or not an integer (${JSON.stringify(summary.tokenlessDrops)}) -- cannot prove no input was dropped`);
  } else if (summary.tokenlessDrops > 0) {
    errors.push(`host dropped ${summary.tokenlessDrops} tokenless input packet(s) (#580) -- this run did not inject what it claims`);
  }

  // The harness exit carries failures the scorecard cannot (a crash after the
  // JSON was written, a wedge kill, no-result exit 2). Exit 1 is excused ONLY
  // by a quarantined failing case, which is exactly what makes the suite exit 1.
  const exit = /^\d+$/.test(String(harnessExit ?? '').trim()) ? Number(harnessExit) : NaN;
  if (!Number.isInteger(exit)) {
    errors.push(`harness exit status unknown (${JSON.stringify(harnessExit)})`);
  } else if (exit !== 0) {
    if (exit === 1 && quarantinedFails.length > 0) {
      warnings.push(`harness exited 1, explained by quarantined failing case(s) ${quarantinedFails.join(',')}`);
    } else {
      errors.push(`harness exited ${exit} with no quarantined failure to explain it${exit === 2 ? ' (2 = NO RESULT)' : ''}`);
    }
  }

  const skips = results.filter((c) => c.status === 'skip').length;
  notes.push(`loopback: total=${results.length} pass=${passed.length} fail=${results.filter((c) => c.status === 'fail').length} skip=${skips} quarantinedFail=${quarantinedFails.length} tokenlessDrops=${summary.tokenlessDrops} harnessExit=${harnessExit}`);
  return done();
}

/// lines: run.jsonl parsed into objects (unparseable lines dropped).
/// runDirName: basename of the results directory (the engine names it runId).
/// engineExit: the `desktop --test-case=<sel>` process's exit status. The
/// engine exits 0 only when its own run status is Passed (1 otherwise:
/// test-fail/infra-fail, cleanup failure, or a cancelled run).
export function cockpitVerdict({ lines, runDirName, startedAtMs, selector, engineExit, quarantine = new Set() }) {
  const errors = [];
  const warnings = [];
  const notes = [];
  const done = () => ({ ok: errors.length === 0, errors, warnings, notes });

  if (!Array.isArray(lines)) {
    errors.push('Test Cockpit produced no run.jsonl');
    return done();
  }
  const meta = lines.find((o) => o?.kind === 'meta')?.payload;
  if (!meta) {
    errors.push('run.jsonl has no meta line -- not a Test Cockpit run this gate can attribute');
    return done();
  }
  // Freshness: the step picks the NEWEST test-runs dir, which is a previous
  // run's when this one wrote nothing. runId is the engine's start time (ms).
  const runId = Number(meta.runId);
  const since = /^\d+$/.test(String(startedAtMs ?? '').trim()) ? Number(startedAtMs) : NaN;
  if (!Number.isFinite(runId) || !Number.isFinite(since) || runId < since) {
    errors.push(`run.jsonl is STALE: runId=${meta.runId} predates this step (started ${startedAtMs}) -- these verdicts belong to an earlier run`);
  }
  if (runDirName != null && String(runDirName) !== String(meta.runId)) {
    errors.push(`results dir '${runDirName}' does not match the run's runId ${meta.runId}`);
  }
  if (selector != null && String(meta.selector).toLowerCase() !== String(selector).trim().toLowerCase()) {
    errors.push(`run.jsonl ran selector '${meta.selector}', the gate asked for '${selector}'`);
  }

  const started = lines.filter((o) => o?.kind === 'scenario-start').map((o) => String(o.scenarioId ?? o.payload));
  const verdicts = lines.filter((o) => o?.kind === 'scenario-verdict').map((o) => {
    const p = o.payload ?? {};
    return { id: String(p.scenarioId ?? o.scenarioId), verdict: String(p.verdict ?? '').toLowerCase(), why: clip(p.message ?? p.reason ?? p.detail, 160) };
  });
  if (verdicts.length === 0) errors.push('Test Cockpit emitted zero verdicts (INSUFFICIENT DATA)');
  const verdictIds = new Set(verdicts.map((v) => v.id));
  const unfinished = started.filter((id) => !verdictIds.has(id));
  if (unfinished.length) errors.push(`scenario(s) started but never reached a verdict: ${unfinished.join(',')}`);

  for (const v of verdicts) {
    notes.push(`  ${v.id}: ${v.verdict} ${v.why}`);
    if (v.verdict === 'pass') continue;
    if (quarantine.has(v.id)) {
      warnings.push(`Test Cockpit ${v.id}: ${v.verdict} -- QUARANTINED (known failure, tracked in an issue; NOT a pass) ${v.why}`);
    } else {
      errors.push(`Test Cockpit ${v.id}: ${v.verdict} -- ${v.why}`);
    }
  }

  // The engine scores a failed cleanup (orphaned web peer / native peer) as a
  // run failure; the gate must agree. Both lines are written only at the end.
  const cleanup = lines.find((o) => o?.kind === 'cleanup')?.payload;
  if (!cleanup) errors.push('run.jsonl has no cleanup line -- the engine never finished the run');
  else if (cleanup.passed !== true) errors.push('Test Cockpit cleanup verifier failed (orphaned peer process or stale participant)');
  // The conclusion lists every scenario the engine scored -- including a
  // Cancelled one, which gets NO scenario-start/verdict line. A run cancelled
  // mid-way otherwise read as N<expected passes and went green.
  const conclusion = lines.find((o) => o?.kind === 'conclusion')?.payload;
  if (!conclusion) {
    errors.push('run.jsonl has no conclusion line -- the engine never finished the run');
  } else {
    const status = String(conclusion.status ?? '').toLowerCase();
    if (status !== 'complete') errors.push(`run conclusion status is '${conclusion.status}' (not 'complete') -- the run was aborted or cancelled`);
    const listed = Array.isArray(conclusion.scenarios) ? conclusion.scenarios : null;
    if (!listed || listed.length === 0) {
      errors.push('run conclusion lists no scenarios -- cannot prove every scenario reached a verdict');
    } else {
      const listedIds = listed.map((s) => String(s?.scenarioId));
      const cancelled = listed.filter((s) => /^(cancel|abort)/i.test(String(s?.verdict ?? ''))).map((s) => String(s?.scenarioId));
      if (cancelled.length) errors.push(`run was CANCELLED: conclusion scores ${cancelled.join(',')} as cancelled -- the remaining scenarios never ran`);
      const noVerdict = listedIds.filter((id) => !verdictIds.has(id));
      if (noVerdict.length) errors.push(`scenario(s) in the run conclusion with no verdict line: ${noVerdict.join(',')}`);
      const unlisted = [...verdictIds].filter((id) => !listedIds.includes(id));
      if (unlisted.length) errors.push(`verdict line(s) for scenario(s) missing from the run conclusion: ${unlisted.join(',')}`);
      if (listed.length !== verdicts.length) errors.push(`run conclusion lists ${listed.length} scenario(s) but run.jsonl holds ${verdicts.length} verdict line(s)`);
    }
  }

  // The engine exit carries failures run.jsonl cannot (a crash or kill after
  // the verdicts were written, a launch error). Exit 1 is excused ONLY by a
  // quarantined test-fail/infra-fail, which is exactly what makes the engine
  // exit 1 on an otherwise healthy run (a skipped verdict does not).
  const engineQuarantined = verdicts.filter((v) => (v.verdict === 'test-fail' || v.verdict === 'infra-fail') && quarantine.has(v.id)).map((v) => v.id);
  const exit = /^\d+$/.test(String(engineExit ?? '').trim()) ? Number(engineExit) : NaN;
  if (!Number.isInteger(exit)) {
    errors.push(`Test Cockpit engine exit status unknown (${JSON.stringify(engineExit)})`);
  } else if (exit !== 0) {
    if (exit === 1 && engineQuarantined.length > 0) {
      warnings.push(`Test Cockpit engine exited 1, explained by quarantined failing scenario(s) ${engineQuarantined.join(',')}`);
    } else {
      errors.push(`Test Cockpit engine exited ${exit} with no quarantined failure to explain it`);
    }
  }

  const bad = verdicts.filter((v) => v.verdict !== 'pass' && !quarantine.has(v.id)).length;
  const quarantined = verdicts.filter((v) => v.verdict !== 'pass' && quarantine.has(v.id)).length;
  notes.push(`Test Cockpit ${meta.selector}: ${verdicts.length - bad - quarantined}/${verdicts.length} PASS, ${quarantined} quarantined, ${bad} not PASS`);
  return done();
}

export function readJsonl(file) {
  if (!file || !fs.existsSync(file)) return null;
  const out = [];
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    if (!line.trim()) continue;
    try { out.push(JSON.parse(line)); } catch { /* a torn line is not evidence */ }
  }
  return out;
}

function readJson(file) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return null; }
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith('--')) throw new Error(`unexpected argument '${key}'`);
    args[key.slice(2)] = argv[i + 1] ?? '';
    i += 1;
  }
  return args;
}

export function emit(verdict, log = console.log) {
  for (const n of verdict.notes) log(n);
  for (const w of verdict.warnings) log(`::warning::${w}`);
  for (const e of verdict.errors) log(`::error::${e}`);
}

// #391: the share-freeze soak's switch, same semantics as E2E_IMPAIRED_BLOCKING.
export const FREEZE_SOAK_BLOCKING_ENV = 'E2E_FREEZE_SOAK_BLOCKING';

// #378: a non-blocking verdict keeps every finding visible (as warnings
// prefixed REPORT-ONLY) but cannot fail the gate.
export function applyReportOnly(verdict, { blocking }) {
  if (blocking || verdict.ok) return { ...verdict, reportOnlyWouldFail: false };
  return {
    ...verdict,
    ok: true,
    errors: [],
    warnings: [...verdict.warnings, ...verdict.errors.map((e) => `REPORT-ONLY: would fail -- ${e}`)],
    reportOnlyWouldFail: true,
    wouldFailCount: verdict.errors.length,
  };
}

function main(argv) {
  const [tier, ...rest] = argv;
  const args = parseArgs(rest);
  const pass = args.pass || 'clean';
  if (pass !== 'clean' && pass !== 'impaired') {
    console.log(`::error::e2e-gate-verdict: unknown --pass '${pass}' (expected clean|impaired)`);
    return 1;
  }
  let verdict;
  if (tier === 'loopback') {
    const report = args.results && fs.existsSync(args.results) ? readJson(args.results) : null;
    verdict = loopbackVerdict({ report, harnessExit: args['harness-exit'], quarantine: parseIdList(args.quarantine), required: parseIdList(args.required) });
  } else if (tier === 'cockpit') {
    const dir = args['run-dir'] || '';
    verdict = cockpitVerdict({
      lines: dir ? readJsonl(path.join(dir, 'run.jsonl')) : null,
      runDirName: dir ? path.basename(path.resolve(dir)) : null,
      startedAtMs: args['started-at-ms'],
      selector: args.selector,
      engineExit: args['engine-exit'],
      quarantine: parseIdList(args.quarantine),
    });
  } else if (tier === 'latency') {
    // Run-level remote-control latency budgets: scripts/ci/remote-control-latency-verdict.mjs.
    const hostLog = args['host-log'] && fs.existsSync(args['host-log']) ? fs.readFileSync(args['host-log'], 'utf8') : '';
    verdict = latencyVerdict({
      results: args.results && fs.existsSync(args.results) ? readJson(args.results) : null,
      photon: args.photon && fs.existsSync(args.photon) ? readJson(args.photon) : null,
      hostLog,
      env: process.env,
    });
    if (args.json) {
      fs.writeFileSync(args.json, `${JSON.stringify({ pass: verdict.ok, budgets: verdict.budgets, metrics: verdict.metrics, checks: verdict.checks }, null, 2)}\n`);
    }
  } else if (tier === 'attribution') {
    // #378: the impaired pass's delay must land on the network stages only.
    verdict = attributionVerdict({
      clean: args.clean && fs.existsSync(args.clean) ? readJson(args.clean) : null,
      impaired: args.impaired && fs.existsSync(args.impaired) ? readJson(args.impaired) : null,
      delayMs: args['delay-ms'],
    });
  } else if (tier === 'freeze') {
    // Share-freeze soak (#391): scripts/ci/share-freeze-verdict.mjs. The
    // report is written even when the run was INFRA-INVALID.
    const senderLog = args['sender-log'] && fs.existsSync(args['sender-log']) ? fs.readFileSync(args['sender-log'], 'utf8') : null;
    verdict = freezeVerdict({ raw: args.raw && fs.existsSync(args.raw) ? readJson(args.raw) : null, senderLogText: senderLog });
    if (args.json) fs.writeFileSync(args.json, `${JSON.stringify(verdict.report, null, 2)}\n`);
    console.log(`share-freeze outcome: ${verdict.outcome.toUpperCase()}`);
    verdict = applyReportOnly(verdict, { blocking: envFlagTrue(process.env, FREEZE_SOAK_BLOCKING_ENV) });
  } else {
    console.log(`::error::e2e-gate-verdict: unknown tier '${tier}' (expected loopback|latency|attribution|cockpit|freeze)`);
    return 1;
  }
  if (pass === 'impaired') {
    verdict = applyReportOnly(verdict, { blocking: envFlagTrue(process.env, IMPAIRED_BLOCKING_ENV) });
  }
  emit(verdict);
  const label = pass === 'clean' ? tier : `${tier} (${pass} pass)`;
  if (verdict.reportOnlyWouldFail) {
    const switchEnv = tier === 'freeze' ? FREEZE_SOAK_BLOCKING_ENV : IMPAIRED_BLOCKING_ENV;
    console.log(`e2e-gate-verdict ${label}: REPORT-ONLY: would fail (${verdict.wouldFailCount} problem(s)); not blocking because ${switchEnv} is not true`);
  } else {
    console.log(verdict.ok ? `e2e-gate-verdict ${label}: PASS` : `e2e-gate-verdict ${label}: FAIL (${verdict.errors.length} problem(s))`);
  }
  return verdict.ok ? 0 : 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  try {
    process.exitCode = main(process.argv.slice(2));
  } catch (error) {
    console.log(`::error::e2e-gate-verdict crashed: ${error?.stack ?? error}`);
    process.exitCode = 1;
  }
}
