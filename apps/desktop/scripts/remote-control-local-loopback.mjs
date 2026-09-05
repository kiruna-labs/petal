#!/usr/bin/env node
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';
import { NO_RESULT_EXIT_CODE, noResultSummary } from './remote-control-exit.mjs';
import { INPUT_ONLY_SCOPE_LINES } from './remote-control-share-readiness.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const preflightPath = path.join(scriptDir, 'remote-control-harness-preflight.mjs');
const scenarioPath = path.join(scriptDir, 'remote-control-scenario.mjs');

const rawArgs = process.argv.slice(2);
const allowedFlags = new Set([
  '--live',
  '--check-only',
  '--skip-preflight',
  '--press-to-photon',
  '--acceptance-446',
  '--rapid-click-burst',
  '--input-only',
  '--skip-swift-typecheck',
  '--help',
  '-h',
]);
let live = false;
let checkOnly = false;
let skipPreflight = false;
let pressToPhoton = false;
let acceptance446 = false;
let rapidClickBurst = false;
let inputOnly = false;
let skipSwiftTypecheck = false;
let jsonOutputPath = null;
const seenArgs = new Set();
for (let i = 0; i < rawArgs.length; i += 1) {
  const arg = rawArgs[i];
  if (arg === '--json') {
    if (seenArgs.has(arg)) {
      throw new Error('duplicate --json argument');
    }
    seenArgs.add(arg);
    jsonOutputPath = rawArgs[++i] ?? null;
    if (!jsonOutputPath) {
      console.error('--json requires a file path');
      process.exit(2);
    }
    continue;
  }
  if (!allowedFlags.has(arg)) {
    throw new Error(`unknown argument: ${arg}; pass --help for usage`);
  }
  if (seenArgs.has(arg)) {
    throw new Error(`duplicate argument: ${arg}; pass --help for usage`);
  }
  seenArgs.add(arg);
  live ||= arg === '--live';
  checkOnly ||= arg === '--check-only';
  skipPreflight ||= arg === '--skip-preflight';
  pressToPhoton ||= arg === '--press-to-photon';
  acceptance446 ||= arg === '--acceptance-446';
  rapidClickBurst ||= arg === '--rapid-click-burst';
  inputOnly ||= arg === '--input-only';
  skipSwiftTypecheck ||= arg === '--skip-swift-typecheck';
}

function usage() {
  console.log(`Remote-control local-loopback harness

CI-safe path:
  node apps/desktop/scripts/remote-control-local-loopback.mjs --check-only
  node apps/desktop/scripts/remote-control-local-loopback.mjs --check-only --skip-swift-typecheck

Live path:
  1. Start local LiveKit/backend/web-harness and join the same room from Chrome.
  2. Launch Chrome with --remote-debugging-port=9222 and open the web harness tab.
  3. Launch Petal dev with PETAL_AUTOTEST_SOCK plus PETAL_AUTOTEST_ROOM/IDENTITY.
  4. Grant Accessibility to the Petal dev binary.
  5. Run:
     PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock node apps/desktop/scripts/remote-control-local-loopback.mjs --live
     PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock node apps/desktop/scripts/remote-control-local-loopback.mjs --live --json /tmp/rc-results.json

Video-independent path (same 30 cases, relaxed share-readiness bar):
     PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock node apps/desktop/scripts/remote-control-local-loopback.mjs --live --input-only

  Accepts a share as ready as soon as the publication is present as a
  controllable target, instead of waiting for a decoded, sized video frame.
  start_share still blocks on a first captured frame, so this is NOT "runs with
  capture dead" -- it is "runs when capture starts but the stream, encode/
  publish or browser decode degrades afterwards". It proves nothing about
  pixels reaching a viewer. Results go to /tmp/rc-results-input-only.json.

Press-to-photon path (runs instead of the TextEdit matrix):
     PETAL_AUTOTEST_SOCK=/tmp/petal-rc.sock node apps/desktop/scripts/remote-control-local-loopback.mjs --live --press-to-photon --json /tmp/rc-photon.json

  This compiles and launches the local AppKit sentinel, shares it through the
  native app, injects text/click input from the web client, and waits for the
  input-caused Gray-code generation in requestVideoFrameCallback.

What live mode validates:
  29 fixed numbered cases over the real web controller -> LiveKit data channel
  -> native host -> CGEventPostToPid path. Each case prints one RESULT line,
  then the suite prints one SUMMARY line. Cases that need AX/sentinel
  observability skip instead of false-greening.

Threshold env:
  PETAL_REMOTE_CONTROL_ACQUIRE_TIMEOUT_MS (default 7000)
  PETAL_REMOTE_CONTROL_STATUS_TIMEOUT_MS (default acquire timeout)
  PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS (default 500; enforced only for named target observations)
  PETAL_REMOTE_CONTROL_PHOTON_SAMPLES (default 20 per input kind)
  PETAL_REMOTE_CONTROL_PHOTON_WARMUP_SAMPLES (default 2 per input kind)
  PETAL_REMOTE_CONTROL_PHOTON_TIMEOUT_MS (default 2000 per sample)
  PETAL_REMOTE_CONTROL_PHOTON_P95_BUDGET_MS (default 250)
`);
}

if (seenArgs.has('--help') || seenArgs.has('-h')) {
  if (rawArgs.length !== 1) {
    throw new Error('--help cannot be combined with other arguments');
  }
  usage();
  process.exit(0);
}

if (skipSwiftTypecheck && !checkOnly) {
  throw new Error('--skip-swift-typecheck is only valid with --check-only');
}
if (skipSwiftTypecheck && (live || skipPreflight || pressToPhoton || jsonOutputPath)) {
  throw new Error('--skip-swift-typecheck cannot be combined with live, --skip-preflight, --press-to-photon, or --json');
}
if (checkOnly && (live || skipPreflight || pressToPhoton || acceptance446 || rapidClickBurst || inputOnly || jsonOutputPath)) {
  throw new Error('--check-only cannot be combined with live, --skip-preflight, --press-to-photon, or --json');
}
if (pressToPhoton && !live) {
  throw new Error('--press-to-photon requires --live');
}
if (acceptance446 && !live) {
  throw new Error('--acceptance-446 requires --live');
}
if (acceptance446 && pressToPhoton) {
  throw new Error('--acceptance-446 cannot be combined with --press-to-photon');
}
// #618 queueing test: a burst of clicks fired without awaiting each one.
if (rapidClickBurst && !live) {
  throw new Error('--rapid-click-burst requires --live');
}
if (rapidClickBurst && (pressToPhoton || acceptance446)) {
  throw new Error('--rapid-click-burst cannot be combined with --press-to-photon or --acceptance-446');
}
// 6c: the video-independent gate. Not a new runner and not a new PETAL_* env
// var -- a flag, exactly like --press-to-photon and --acceptance-446.
if (inputOnly && !live) {
  throw new Error('--input-only requires --live');
}
if (inputOnly && (pressToPhoton || rapidClickBurst)) {
  throw new Error('--input-only cannot be combined with --press-to-photon or --rapid-click-burst (both read video frames)');
}
// Anti-confusion mechanism 3 of 3: a relaxed run must never land in the same
// artifact as the full gate. Default it, and refuse an explicit path that does
// not name itself.
const INPUT_ONLY_RESULTS_PATH = '/tmp/rc-results-input-only.json';
if (inputOnly) {
  if (!jsonOutputPath) {
    jsonOutputPath = INPUT_ONLY_RESULTS_PATH;
  } else if (!path.basename(jsonOutputPath).includes('input-only')) {
    throw new Error(
      `--input-only refuses to write to '${jsonOutputPath}': the filename must contain 'input-only' so a relaxed run cannot be cited as the full gate`
    );
  }
  fs.rmSync(jsonOutputPath, { force: true });
}

const socketPath = process.env.PETAL_AUTOTEST_SOCK;

console.log('==> Remote-control local-loopback harness');
console.log('CI-safe checks fail this command; live CGEvent/TextEdit checks skip unless explicitly runnable.');

if (!skipPreflight) {
  console.log('\n==> CI-safe preflight');
  const preflightArgs = [preflightPath, '--check-only'];
  if (skipSwiftTypecheck) preflightArgs.push('--skip-swift-typecheck');
  execFileSync(process.execPath, preflightArgs, {
    cwd: path.resolve(scriptDir, '../../..'),
    stdio: 'inherit',
  });
}

if (checkOnly) {
  console.log('\n==> Live loopback');
  console.log('skipped by --check-only');
  process.exit(0);
}

if (!live && !socketPath) {
  // NOT a pass. This is exactly how a misconfigured live run lands -- neither
  // --live nor PETAL_AUTOTEST_SOCK -- and exiting 0 here made "the suite never
  // ran" indistinguishable from "the suite ran clean". --check-only keeps its
  // exit 0 below: that branch genuinely ran the preflight.
  const reason =
    'set PETAL_AUTOTEST_SOCK and pass --live to exercise CGEventPostToPid against TextEdit';
  console.log('\n==> Live loopback');
  console.log(`# SKIP remote-control local loopback: ${reason}`);
  console.log(`SUMMARY ${JSON.stringify(noResultSummary(reason))}`);
  usage();
  process.exit(NO_RESULT_EXIT_CODE);
}

console.log('\n==> Live loopback');
if (inputOnly) {
  // Anti-confusion mechanism 2 of 3.
  for (const line of INPUT_ONLY_SCOPE_LINES) console.log(`# ${line}`);
  console.log(`# input-only results artifact: ${jsonOutputPath}`);
}
const scenarioArgs = [scenarioPath];
if (pressToPhoton) scenarioArgs.push('--press-to-photon');
if (acceptance446) scenarioArgs.push('--acceptance-446');
if (rapidClickBurst) scenarioArgs.push('--rapid-click-burst');
if (inputOnly) scenarioArgs.push('--input-only');
if (jsonOutputPath) scenarioArgs.push('--json', jsonOutputPath);
// Deliberately `spawn`, not `spawnSync`. spawnSync blocks the event loop, so a
// SIGTERM to this wrapper -- exactly how an external `timeout` ends a run --
// could never run a handler here: Node died and left the scenario, and the
// AppKit sentinel it owns, orphaned (plan Item 7).
const child = spawn(process.execPath, scenarioArgs, {
  cwd: path.resolve(scriptDir, '..'),
  env: process.env,
  stdio: ['inherit', 'pipe', 'pipe'],
});
let childStdout = '';
let childStderr = '';
child.stdout.setEncoding('utf8');
child.stderr.setEncoding('utf8');
child.stdout.on('data', (chunk) => {
  childStdout += chunk;
});
child.stderr.on('data', (chunk) => {
  childStderr += chunk;
});

let forwardedSignal = null;
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(signal, () => {
    forwardedSignal = signal;
    console.log(`# ${signal} received; forwarding to the remote-control scenario and waiting for its teardown`);
    try {
      child.kill(signal);
    } catch {
      // Already gone; the close handler below still resolves.
    }
  });
}

const result = await new Promise((resolve) => {
  child.once('error', (error) => resolve({ error, status: null, signalCode: null }));
  child.once('close', (status, signalCode) => resolve({ error: null, status, signalCode }));
});

if (result.error) {
  throw result.error;
}
let parsedSummary = null;
let parsedResults = 0;
for (const stream of [childStdout, childStderr]) {
  if (!stream) continue;
  for (const line of stream.split(/\r?\n/)) {
    if (!line) continue;
    console.log(line);
    if (line.startsWith('RESULT ')) {
      JSON.parse(line.slice('RESULT '.length));
      parsedResults += 1;
    } else if (line.startsWith('SUMMARY ')) {
      parsedSummary = JSON.parse(line.slice('SUMMARY '.length));
    }
  }
}
if (result.status === 0 && parsedSummary) {
  console.log(
    `# parsed remote-control suite: total=${parsedSummary.total} pass=${parsedSummary.pass} fail=${parsedSummary.fail} skip=${parsedSummary.skip}`
  );
}
if (result.status !== 0 && parsedResults > 0 && parsedSummary) {
  console.log(
    `# parsed remote-control suite before failure: total=${parsedSummary.total} pass=${parsedSummary.pass} fail=${parsedSummary.fail} skip=${parsedSummary.skip}`
  );
}
if (forwardedSignal || result.signalCode) {
  console.log(
    `# NO RESULT: the remote-control scenario was killed by ${result.signalCode ?? forwardedSignal}`
  );
  process.exit(NO_RESULT_EXIT_CODE);
}
// No SUMMARY line means the scenario produced no accounting at all -- it died,
// was killed, or skipped without reporting. That is "no result", never a pass,
// and never a plain failure either.
if (!parsedSummary) {
  console.log(
    '# NO RESULT: the remote-control scenario emitted no SUMMARY line; nothing was proved by this run'
  );
  process.exit(NO_RESULT_EXIT_CODE);
}
process.exit(result.status ?? 1);
