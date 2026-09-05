#!/usr/bin/env node
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(SCRIPT_DIR, '../../..');
const DEFAULT_DOC = path.join(REPO_ROOT, 'docs/TESTING.md');
const DEFAULT_RUNS_ROOT = path.join(os.homedir(), 'Library/Logs/Petal/test-runs');
const START = '<!-- cockpit-status:start -->';
const END = '<!-- cockpit-status:end -->';

function usage() {
  return [
    'Usage: node apps/desktop/scripts/update-testing-status.mjs [results-dir] [--doc docs/TESTING.md] [--expect-total N]',
    '',
    'If results-dir is omitted, the latest directory under',
    '~/Library/Logs/Petal/test-runs is used (by the run timestamp encoded in',
    'the directory name, not mtime).',
    '',
    '--expect-total N additionally requires exactly N scenario verdicts;',
    'fewer or more publishes as failed/incomplete, never as passed (#622).',
  ].join('\n');
}

function parseArgs(argv) {
  let resultsDir = null;
  let docPath = DEFAULT_DOC;
  let expectTotal = null;
  const takeExpectTotal = (raw) => {
    const n = Number(raw);
    if (!Number.isInteger(n) || n <= 0) throw new Error(`--expect-total requires a positive integer, got: ${raw}`);
    expectTotal = n;
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '-h' || arg === '--help') {
      console.log(usage());
      process.exit(0);
    }
    if (arg === '--doc') {
      docPath = argv[i + 1];
      if (!docPath) throw new Error('--doc requires a path');
      i += 1;
      continue;
    }
    if (arg.startsWith('--doc=')) {
      docPath = arg.slice('--doc='.length);
      continue;
    }
    if (arg === '--expect-total') {
      takeExpectTotal(argv[i + 1]);
      i += 1;
      continue;
    }
    if (arg.startsWith('--expect-total=')) {
      takeExpectTotal(arg.slice('--expect-total='.length));
      continue;
    }
    if (arg.startsWith('-')) throw new Error(`unknown argument: ${arg}\n\n${usage()}`);
    if (resultsDir) throw new Error(`unexpected extra results dir: ${arg}\n\n${usage()}`);
    resultsDir = arg;
  }
  return {
    resultsDir: resultsDir ? path.resolve(resultsDir) : null,
    docPath: path.resolve(docPath),
    expectTotal,
  };
}

// #622: pick "latest" by the run start time encoded in the directory name
// (epoch-ms run ids, or ISO-like timestamps), not by mtime -- mtime is
// touched by later reads/prunes and silently promoted stale runs.
function runStartMsFromDirName(name) {
  if (/^\d{10,}$/.test(name)) return Number(name);
  const isoLike = name.match(/^(\d{4})-(\d{2})-(\d{2})T(\d{2})-(\d{2})-(\d{2})-(\d{3})Z$/);
  if (isoLike) {
    const [, y, mo, d, h, mi, s, ms] = isoLike;
    return Date.UTC(Number(y), Number(mo) - 1, Number(d), Number(h), Number(mi), Number(s), Number(ms));
  }
  return null;
}

function latestResultsDir(root = DEFAULT_RUNS_ROOT) {
  const entries = fs.readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => {
      const fullPath = path.join(root, entry.name);
      const startMs = runStartMsFromDirName(entry.name);
      return { fullPath, sortMs: startMs ?? fs.statSync(fullPath).mtimeMs };
    })
    .sort((a, b) => b.sortMs - a.sortMs);
  if (entries.length === 0) throw new Error(`no test run directories found under ${root}`);
  return entries[0].fullPath;
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

function normalizeVerdict(value) {
  return String(value ?? '').trim().toLowerCase().replace(/_/g, '-');
}

function emptyCounts() {
  return { pass: 0, fail: 0, skip: 0, unknown: 0 };
}

// #622: an unrecognised verdict string ("error", "timeout", "crashed", a
// future misspelling...) must never be silently dropped -- it counts as
// `unknown` and forces the published status to failed.
function addVerdict(counts, verdict, unknownVerdicts) {
  switch (normalizeVerdict(verdict)) {
    case 'pass':
    case 'passed':
      counts.pass += 1;
      return;
    case 'test-fail':
    case 'infra-fail':
    case 'fail':
    case 'failed':
    case 'failure':
    case 'cancelled':
    case 'canceled':
      counts.fail += 1;
      return;
    case 'skip':
    case 'skipped':
      counts.skip += 1;
      return;
    default:
      counts.unknown += 1;
      unknownVerdicts.push(String(verdict ?? '<missing>'));
  }
}

function totalVerdicts(counts) {
  return counts.pass + counts.fail + counts.skip + counts.unknown;
}

function tierFromSelector(selector) {
  const normalized = String(selector ?? '').trim().toLowerCase();
  if (normalized === 'quick' || normalized === 'full' || normalized === 'soak') return normalized;
  if (!normalized) return 'unknown';
  return normalized.includes(',') ? 'custom' : normalized;
}

function parseRunJsonl(resultsDir) {
  const runPath = path.join(resultsDir, 'run.jsonl');
  if (!fs.existsSync(runPath)) return null;

  const counts = emptyCounts();
  const unknownVerdicts = [];
  let meta = {};
  let scenarioStarts = 0;
  let conclusion = null;
  const lines = fs.readFileSync(runPath, 'utf8').split(/\r?\n/);
  for (const line of lines) {
    if (!line.trim()) continue;
    const event = JSON.parse(line);
    if (event.kind === 'meta' && event.payload && typeof event.payload === 'object') {
      meta = event.payload;
    }
    if (event.kind === 'scenario-start') scenarioStarts += 1;
    if (event.kind === 'scenario-verdict') {
      addVerdict(counts, event.payload?.verdict, unknownVerdicts);
    }
    if (event.kind === 'conclusion' && event.payload && typeof event.payload === 'object') {
      conclusion = {
        status: String(event.payload.status ?? ''),
        scenarioCount: Array.isArray(event.payload.scenarios) ? event.payload.scenarios.length : null,
      };
    }
  }

  // #622: completeness evidence intrinsic to the artifact. The cockpit writes
  // the `conclusion` event only after the whole run finished; a crashed run
  // has verdicts but no conclusion, or fewer verdicts than scenario-starts.
  const incompleteReasons = [];
  if (!conclusion) {
    incompleteReasons.push('no `conclusion` event: the run never reached its end (crashed, killed, or written by a pre-#622 binary)');
  } else {
    if (conclusion.status !== 'complete') {
      incompleteReasons.push(`conclusion status is \`${conclusion.status}\`, not \`complete\``);
    }
    if (conclusion.scenarioCount !== null && conclusion.scenarioCount !== totalVerdicts(counts)) {
      incompleteReasons.push(`conclusion records ${conclusion.scenarioCount} scenario(s) but ${totalVerdicts(counts)} verdict(s) were logged`);
    }
  }
  if (scenarioStarts !== totalVerdicts(counts)) {
    incompleteReasons.push(`${scenarioStarts} scenario-start(s) but ${totalVerdicts(counts)} verdict(s): at least one scenario never reached a verdict`);
  }

  return {
    counts,
    unknownVerdicts,
    incompleteReasons,
    tier: tierFromSelector(meta.selector),
    runId: meta.runId ?? path.basename(resultsDir),
    source: 'run.jsonl',
    verdicts: totalVerdicts(counts),
  };
}

function parseScorecard(resultsDir) {
  const scorecardPath = path.join(resultsDir, 'scorecard.json');
  if (!fs.existsSync(scorecardPath)) return null;

  const scorecard = readJson(scorecardPath);
  const counts = emptyCounts();
  const unknownVerdicts = [];
  const summary = scorecard.summary && typeof scorecard.summary === 'object' ? scorecard.summary : scorecard;
  const passed = Number(summary.passed ?? summary.pass);
  const failed = Number(summary.failed ?? summary.fail);
  const skipped = Number(summary.skipped ?? summary.skip);
  if ([passed, failed, skipped].every(Number.isFinite)) {
    counts.pass = passed;
    counts.fail = failed;
    counts.skip = skipped;
  }

  if (totalVerdicts(counts) === 0) {
    const scenarios = Array.isArray(scorecard.scenarios) ? scorecard.scenarios : [];
    for (const scenario of scenarios) {
      const verdict = scenario.verdict ?? scenario.status ?? scenario.result ?? scenario.outcome;
      if (verdict === undefined) continue; // scorecard rows without any verdict field carry no verdict evidence
      addVerdict(counts, verdict, unknownVerdicts);
    }
  }

  const generatedAt = Number(scorecard.generatedAtUnixMs ?? scorecard.generated_at_unix_ms);
  return {
    counts,
    unknownVerdicts,
    // scorecard.json is only written after a run finishes, so its presence is
    // itself the completeness evidence; run.jsonl (preferred) carries more.
    incompleteReasons: [],
    generatedAt: Number.isFinite(generatedAt) && generatedAt > 0 ? generatedAt : null,
    tier: tierFromSelector(scorecard.selector ?? scorecard.tier ?? scorecard.coverageKind),
    runId: scorecard.runId ?? path.basename(resultsDir),
    source: 'scorecard.json',
    verdicts: totalVerdicts(counts),
  };
}

// #622: `passed` requires positive evidence -- verdicts present, a complete
// run, no failures, no unrecognised verdicts, and (when given) the expected
// total. Zero evidence renders as INSUFFICIENT DATA, never as passed.
function runStatus(parsed, expectTotal) {
  const counts = parsed.counts;
  const problems = [];
  if (parsed.verdicts === 0) {
    return { status: 'INSUFFICIENT DATA', problems: ['no scenario verdicts were found in the artifact'] };
  }
  if (counts.unknown > 0) {
    problems.push(`${counts.unknown} unrecognised verdict(s): ${parsed.unknownVerdicts.join(', ')}`);
  }
  if (expectTotal !== null && parsed.verdicts !== expectTotal) {
    problems.push(`expected ${expectTotal} verdict(s) (--expect-total), found ${parsed.verdicts}`);
  }
  for (const reason of parsed.incompleteReasons) problems.push(`incomplete run: ${reason}`);
  if (counts.fail > 0) {
    return { status: 'failed', problems };
  }
  if (problems.length > 0) {
    return { status: 'failed (incomplete evidence)', problems };
  }
  if (counts.pass > 0) return { status: 'passed', problems: [] };
  return { status: 'INSUFFICIENT DATA', problems: ['no scenario passed and none failed (all skipped)'] };
}

function displayPath(filePath) {
  const home = os.homedir();
  if (filePath === home) return '~';
  if (filePath.startsWith(`${home}${path.sep}`)) {
    return `~${path.sep}${path.relative(home, filePath)}`;
  }
  return filePath;
}

function timestampFor(resultsDir, parsed) {
  if (parsed.generatedAt) return new Date(parsed.generatedAt);
  return fs.statSync(resultsDir).mtime;
}

function renderStatus(resultsDir, parsed, expectTotal) {
  const timestamp = timestampFor(resultsDir, parsed).toISOString();
  const counts = parsed.counts;
  const { status, problems } = runStatus(parsed, expectTotal);
  const notes = problems.length > 0
    ? `\n\n${problems.map((problem) => `- ${problem}`).join('\n')}`
    : '';
  return `${START}
Last updated: ${timestamp}

| Field | Value |
|---|---|
| Results dir | \`${displayPath(resultsDir)}\` |
| Artifact | \`${parsed.source}\` |
| Run ID | \`${parsed.runId}\` |
| Tier | \`${parsed.tier}\` |
| Status | \`${status}\` |
| Passed | ${counts.pass} |
| Failed | ${counts.fail} |
| Skipped | ${counts.skip} |
| Unrecognised | ${counts.unknown} |${notes}
${END}`;
}

function replaceStatusBlock(docText, block) {
  const start = docText.indexOf(START);
  const end = docText.indexOf(END);
  if (start === -1 || end === -1 || end < start) {
    throw new Error(`docs file must contain a bounded cockpit status block (${START} ... ${END})`);
  }
  return `${docText.slice(0, start)}${block}${docText.slice(end + END.length)}`;
}

try {
  const { resultsDir: requestedResultsDir, docPath, expectTotal } = parseArgs(process.argv.slice(2));
  const resultsDir = requestedResultsDir ?? latestResultsDir();
  if (!fs.statSync(resultsDir).isDirectory()) {
    throw new Error(`results dir is not a directory: ${resultsDir}`);
  }

  const parsed = parseRunJsonl(resultsDir) ?? parseScorecard(resultsDir);
  if (!parsed) {
    throw new Error(`expected run.jsonl or scorecard.json in ${resultsDir}`);
  }

  const docText = fs.readFileSync(docPath, 'utf8');
  const nextDocText = replaceStatusBlock(docText, renderStatus(resultsDir, parsed, expectTotal));
  fs.writeFileSync(docPath, nextDocText);
  console.log(`updated ${docPath} from ${path.join(resultsDir, parsed.source)}`);
  const { status, problems } = runStatus(parsed, expectTotal);
  console.log(`published status: ${status}`);
  for (const problem of problems) console.error(`problem: ${problem}`);
  // #622: the tool succeeds by publishing a TRUTHFUL status; a non-passed
  // status still exits nonzero so pipelines cannot mistake publication for a
  // green run.
  if (status !== 'passed') process.exitCode = 2;
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
