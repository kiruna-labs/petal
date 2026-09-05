#!/usr/bin/env node
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';
import {
  REMOTE_CONTROL_ACCEPTANCE_446,
  REMOTE_CONTROL_BUTTONS,
  REMOTE_CONTROL_COORDINATES,
  REMOTE_CONTROL_DRAG_STEPS,
  REMOTE_CONTROL_SCROLL_DELTAS,
  REMOTE_CONTROL_SHORTCUTS,
} from './remote-control-gestures.mjs';
import { summarizePhotonSamples } from './remote-control-photon-metrics.mjs';
import { noResultSummary, suiteExitCode } from './remote-control-exit.mjs';
import { ProcessLeaseLedger, psIdentity } from './process-lease-ledger.mjs';
import {
  INPUT_ONLY_SCOPE_LINES,
  inputOnlyPassBarVerdict,
  liveTileProbeExpression,
  shareReadinessMode,
  shareReadyPredicate,
  tileFailureDetail,
} from './remote-control-share-readiness.mjs';

const positional = [];
let jsonOutputPath = null;
let pressToPhotonMode = false;
let cockpitDriveMode = false;
let acceptance446Mode = false;
let rapidClickBurstMode = false;
let inputOnlyMode = false;
let photonShuffleSeed = 288;
for (let i = 2; i < process.argv.length; i += 1) {
  const arg = process.argv[i];
  if (arg === '--json') {
    jsonOutputPath = process.argv[++i] ?? null;
    if (!jsonOutputPath) throw new Error('--json requires a file path');
  } else if (arg === '--photon-shuffle-seed') {
    photonShuffleSeed = parsePhotonShuffleSeed(process.argv[++i]);
  } else if (arg === '--press-to-photon') {
    pressToPhotonMode = true;
  } else if (arg === '--cockpit-drive') {
    cockpitDriveMode = true;
  } else if (arg === '--acceptance-446') {
    acceptance446Mode = true;
  } else if (arg === '--rapid-click-burst') {
    rapidClickBurstMode = true;
  } else if (arg === '--input-only') {
    inputOnlyMode = true;
  } else {
    positional.push(arg);
  }
}

// --press-to-photon and --rapid-click-burst are the only two modes that read
// video frames, so a relaxed share-readiness bar is meaningless for them.
if (inputOnlyMode && pressToPhotonMode) {
  throw new Error('--input-only cannot be combined with --press-to-photon (it reads video frames)');
}
if (inputOnlyMode && rapidClickBurstMode) {
  throw new Error('--input-only cannot be combined with --rapid-click-burst (it reads video frames)');
}
const tileAccepted = shareReadyPredicate(inputOnlyMode);

const socketPath = positional[0] || process.env.PETAL_AUTOTEST_SOCK;
const targetUserId = process.env.PETAL_REMOTE_CONTROL_TARGET_IDENTITY || 'native-autotest';
const cdpListUrl = process.env.PETAL_REMOTE_CONTROL_CDP_JSON || 'http://127.0.0.1:9222/json';
const harnessUrlNeedle = process.env.PETAL_WEB_HARNESS_URL_MATCH || 'localhost:5184';
const acquisitionTimeoutMs = Number(process.env.PETAL_REMOTE_CONTROL_ACQUIRE_TIMEOUT_MS || 7000);
const statusTimeoutMs = Number(process.env.PETAL_REMOTE_CONTROL_STATUS_TIMEOUT_MS || acquisitionTimeoutMs);
const shareReadyTimeoutMs = Number(process.env.PETAL_REMOTE_CONTROL_SHARE_READY_TIMEOUT_MS || 8000);
const caseSettleMs = Number(process.env.PETAL_REMOTE_CONTROL_CASE_SETTLE_MS || 500);
const inputBudgetMs = Number(process.env.PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS || 500);
const photonSamplesPerInput = Number(process.env.PETAL_REMOTE_CONTROL_PHOTON_SAMPLES || 20);
const photonWarmupSamplesPerInput = Number(process.env.PETAL_REMOTE_CONTROL_PHOTON_WARMUP_SAMPLES || 2);
const photonSampleTimeoutMs = Number(process.env.PETAL_REMOTE_CONTROL_PHOTON_TIMEOUT_MS || 2000);
const photonP95BudgetMs = Number(process.env.PETAL_REMOTE_CONTROL_PHOTON_P95_BUDGET_MS || 250);
const maxTextEditRecoveries = 2;
const photonSentinelSource = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  'remote-control-photon-sentinel.swift'
);
const photonSentinelBundle = path.join(os.tmpdir(), 'PetalRCPhotonSentinel.app');
const photonSentinelBinary = path.join(
  photonSentinelBundle,
  'Contents',
  'MacOS',
  'PetalRCPhotonSentinel'
);

for (const [label, value] of [
  ['PETAL_REMOTE_CONTROL_PHOTON_SAMPLES', photonSamplesPerInput],
  ['PETAL_REMOTE_CONTROL_PHOTON_TIMEOUT_MS', photonSampleTimeoutMs],
  ['PETAL_REMOTE_CONTROL_PHOTON_P95_BUDGET_MS', photonP95BudgetMs]
]) {
  if (!Number.isFinite(value) || value <= 0) throw new Error(`${label} must be a positive number`);
}
if (!Number.isInteger(photonSamplesPerInput)) {
  throw new Error('PETAL_REMOTE_CONTROL_PHOTON_SAMPLES must be an integer');
}
if (!Number.isInteger(photonWarmupSamplesPerInput) || photonWarmupSamplesPerInput < 0) {
  throw new Error('PETAL_REMOTE_CONTROL_PHOTON_WARMUP_SAMPLES must be a non-negative integer');
}

function parsePhotonShuffleSeed(value) {
  if (!/^(?:0|[1-9]\d*)$/.test(value ?? '')) {
    throw new Error('--photon-shuffle-seed must be an unsigned 32-bit integer');
  }
  const seed = Number(value);
  if (!Number.isSafeInteger(seed) || seed > 0xffffffff) {
    throw new Error('--photon-shuffle-seed must be an unsigned 32-bit integer');
  }
  return seed;
}

// A skip is NOT a pass. This used to print one line and exit 0 with no
// SUMMARY, so an unreachable Chrome CDP endpoint produced a green run that
// executed ZERO cases. Emit a real SUMMARY (which removes the wrapper's silent
// `parsedSummary == null` path) and exit 2 -- "no result", never a pass.
function skip(reason) {
  console.log(`# SKIP remote-control scenario: ${reason}`);
  console.log(`SUMMARY ${JSON.stringify(noResultSummary(reason))}`);
  process.exit(2);
}

if (!socketPath) {
  skip('set PETAL_AUTOTEST_SOCK or pass the socket path as argv[2]');
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function connectSocket(file) {
  const socket = net.createConnection(file);
  socket.setEncoding('utf8');
  let buffer = '';
  let pending;
  socket.on('data', (chunk) => {
    buffer += chunk;
    let idx;
    while ((idx = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, idx);
      buffer = buffer.slice(idx + 1);
      if (!line.trim()) continue;
      pending?.resolve(JSON.parse(line));
      pending = undefined;
    }
  });
  socket.on('error', (error) => {
    pending?.reject(error);
    pending = undefined;
  });
  return {
    send(command) {
      return new Promise((resolve, reject) => {
        if (pending) {
          reject(new Error('autotest socket only supports one in-flight command'));
          return;
        }
        pending = { resolve, reject };
        socket.write(`${JSON.stringify(command)}\n`);
      });
    },
    close() {
      socket.end();
    },
  };
}

async function command(client, payload) {
  const response = await client.send(payload);
  if (!response.ok) throw new Error(`${payload.cmd} failed: ${response.error}`);
  return response.result;
}

async function waitForCommand(client, payload, label, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  let lastError = '';
  while (Date.now() <= deadline) {
    const response = await client.send(payload);
    if (response.ok) return response.result;
    lastError = response.error;
    await sleep(250);
  }
  throw new Error(`${label} timed out after ${timeoutMs}ms; last=${lastError}`);
}

async function waitUntil(label, fn, timeoutMs = 2000, intervalMs = 250) {
  const deadline = Date.now() + timeoutMs;
  let lastValue;
  while (Date.now() <= deadline) {
    lastValue = await fn();
    if (lastValue) return lastValue;
    await sleep(intervalMs);
  }
  throw new Error(`${label} timed out after ${timeoutMs}ms; last=${JSON.stringify(lastValue)}`);
}

function isExecFileTimeout(error) {
  return (
    error?.code === 'ETIMEDOUT' ||
    error?.signal === 'SIGTERM' ||
    /ETIMEDOUT|timed out|timeout/i.test(error?.message ?? '')
  );
}

function isMissingTextEditDocument(error) {
  return /Invalid index|Can't get .*document|document .*not found/i.test(
    `${error?.message ?? ''}\n${error?.stderr ?? ''}`
  );
}

function shellQuote(value) {
  return `'${String(value).replace(/'/g, `'\\''`)}'`;
}

function remoteCommand(args) {
  return args.map(shellQuote).join(' ');
}

function osascriptArgs(lines) {
  return lines.flatMap((line) => ['-e', line]);
}

function osascript(...lines) {
  // `timeout` is load-bearing: TextEdit (the sacrificial target app, not
  // Petal) was confirmed live to occasionally stop servicing ANY Apple
  // Event at all -- even a trivial `count of documents` -- after sustained
  // CGEventPostToPid replay + AppleScript polling across many back-to-back
  // cases. Without a timeout, `execFileSync` blocks forever and the whole
  // 30-case suite hangs on one wedged case instead of failing it and moving
  // on.
  try {
    const args = osascriptArgs(lines);
    const remoteHost = process.env.PETAL_REMOTE_OSASCRIPT_HOST;
    if (remoteHost) {
      return execFileSync('ssh', [remoteHost, remoteCommand(['osascript', ...args])], {
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'pipe'],
        timeout: 5000,
      }).trim();
    }
    return execFileSync('osascript', args, {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 5000,
    }).trim();
  } catch (error) {
    if (isExecFileTimeout(error) || isMissingTextEditDocument(error)) textEditWedged = true;
    throw error;
  }
}

// Confirmed live, real bug (not introduced this session, present the whole
// time this suite has existed): `osascript()` above `.trim()`s its output,
// which is fine for AX attribute reads (single tokens/booleans) but silently
// eats the shared window's ACTUAL leading/trailing whitespace when reading
// its live text back. Every marker in this file is deliberately padded with
// a leading/trailing space (` typed-... `, ` L... `, ` clamp-... `) so a
// stray keystroke can't accidentally make it match; with the trim in place,
// `assertDocumentIncludes(marker)` could never succeed no matter how
// correctly remote-control replay worked -- verified live with a minimal
// CDP-driven repro: sending `api.text({ text: ' XX' })` landed exactly
// " XX" in TextEdit (confirmed via `execFileSync` with NO trim), but the
// harness's own `.trim()`-ing read reported "XX", masking a perfectly
// correct replay as "text never landed." `readTextEditDocument()` uses this
// untrimmed variant instead; every other AX/AppleScript read in this file
// intentionally returns short trim-safe tokens, so they keep using the
// trimming `osascript()` above.
function osascriptRaw(...lines) {
  try {
    const args = osascriptArgs(lines);
    const remoteHost = process.env.PETAL_REMOTE_OSASCRIPT_HOST;
    if (remoteHost) {
      return execFileSync('ssh', [remoteHost, remoteCommand(['osascript', ...args])], {
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'pipe'],
        timeout: 5000,
      });
    }
    return execFileSync('osascript', args, {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 5000,
    });
  } catch (error) {
    if (isExecFileTimeout(error) || isMissingTextEditDocument(error)) textEditWedged = true;
    throw error;
  }
}

function readFrontmostProcess() {
  return osascript('tell application "System Events"', 'return name of first process whose frontmost is true', 'end tell');
}

// Set once, right after the sacrificial TextEdit document is opened (see the
// `marker`/`targetPath` setup below). All TextEdit AppleScript helpers below
// address the document/window BY THIS MARKER, never by ordinal ("document 1"
// / "window 1"). Ordinal addressing was a real, confirmed bug: repeated live
// runs never closed their sacrificial TextEdit window (no cleanup existed),
// so windows accumulated across runs, and macOS additionally injects its own
// overlay ("WindowSharingSessionButton", surfaced to System Events as a
// window literally named "Window") once ScreenCaptureKit is actively
// capturing a window -- both push whatever the harness actually wants off of
// index 1. Confirmed live: `AXSelectedText of ... window 1` resolved to that
// phantom overlay window instead of the real document, so the query always
// returned "" rather than raising an AppleScript error -- which read as "the
// drag/select genuinely produced no selection" when the real selection was
// fine all along.
let sacrificialDocMarker = null;
let textEditWedged = false;
let photonSentinelProcess = null;
let photonSentinelLease = null;
const processLeases = new ProcessLeaseLedger();
let leaseLedgerReady = false;

// Sweep whatever a previous run left behind, then take a fresh ledger and arm
// the signal traps. An external `timeout` kill -- the normal way these runs end
// -- bypasses every `finally`, so without the traps the sentinel is orphaned.
function ensureLeaseLedgerReady() {
  if (leaseLedgerReady) return;
  leaseLedgerReady = true;
  for (const swept of processLeases.sweepStaleLeases()) {
    console.log(
      `# swept stale process lease role=${swept.role} pid=${swept.pid} outcome=${swept.outcome}`
    );
  }
  processLeases.startRun();
  for (const swept of sweepNamedStrays('PetalRCPhotonSentinel')) {
    console.log(`# swept stray sentinel pid=${swept.pid} outcome=${swept.outcome}`);
  }
  processLeases.installSignalTraps();
}

// Replaces the old blind `pkill -TERM -x PetalRCPhotonSentinel`. `pgrep -x`
// matches the process NAME exactly, never this script's own command line, and
// every kill below goes to a PID whose identity was re-checked through ps.
function sweepNamedStrays(name) {
  const found = spawnSync('/usr/bin/pgrep', ['-x', name], { encoding: 'utf8' });
  if (found.error || !found.stdout) return [];
  const reports = [];
  for (const raw of found.stdout.split('\n')) {
    const pid = Number(raw.trim());
    if (!Number.isInteger(pid) || pid <= 1 || pid === process.pid) continue;
    const identity = psIdentity(pid);
    if (!identity) continue;
    const entry = {
      role: 'stray-sentinel',
      pid,
      pgid: identity.pgid,
      command: identity.command,
      groupLeader: identity.pgid === pid,
    };
    reports.push({ pid, outcome: processLeases.terminate(entry, 'SWEPT') });
  }
  return reports;
}
const sentinelEventLogPath = path.join(os.tmpdir(), `petal-rc-sentinel-events-${process.pid}.jsonl`);

function clearSentinelEventLog() {
  fs.writeFileSync(sentinelEventLogPath, '', 'utf8');
}

function cockpitDrivenEvents() {
  if (!fs.existsSync(sentinelEventLogPath)) return [];
  return sentinelEvents().filter((event) => event.kind === 'event');
}

async function runCockpitDriveSuite(ctx) {
  // RC-P1080 is an intentionally narrow smoke check, not a replacement for
  // the 30-case suite; see issue #482 for the full rationale.
  clearSentinelEventLog();
  const driven = [];
  async function drive(kind, body, expected) {
    const startedAtMs = Date.now();
    await send(ctx, body);
    await sleep(caseSettleMs);
    const finishedAtMs = Date.now();
    driven.push({ kind, startedAtMs, finishedAtMs, expected });
  }

  await send(ctx, 'api.request(target); return true;');
  await waitForActiveStatus(ctx, Date.now());
  await drive('click', `api.click({ target, ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.cockpitCenter)}, button: ${REMOTE_CONTROL_BUTTONS.left} }); return true;`, {
    types: ['leftMouseDown', 'leftMouseUp'],
  });
  await drive('middle-click', `api.click({ target, ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.cockpitCenter)}, button: ${REMOTE_CONTROL_BUTTONS.middle} }); return true;`, {
    types: ['otherMouseDown', 'otherMouseUp'],
    button: 2,
  });
  await drive('drag', `api.drag({ target, from: ${JSON.stringify(REMOTE_CONTROL_COORDINATES.cockpitDragFrom)}, to: ${JSON.stringify(REMOTE_CONTROL_COORDINATES.cockpitDragTo)}, steps: ${REMOTE_CONTROL_DRAG_STEPS.cockpit}, button: ${REMOTE_CONTROL_BUTTONS.left} }); return true;`, {
    types: ['leftMouseDown', 'leftMouseDragged', 'leftMouseUp'],
    button: 0,
  });
  await drive('type', `api.text({ target, text: ${JSON.stringify('cockpit') } }); return true;`, {
    types: ['keyDown', 'keyUp'],
  });
  await drive('shortcut', `api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdA)} }); return true;`, {
    types: ['keyDown', 'keyUp'],
  });
  await drive('scroll', `api.wheel({ target, ...${JSON.stringify(REMOTE_CONTROL_SCROLL_DELTAS.cockpit)} }); return true;`, {
    types: ['scrollWheel'],
  });
  await send(ctx, 'api.release(target); return true;');
  const report = { mode: 'cockpit-drive', driven, observed: cockpitDrivenEvents() };
  if (jsonOutputPath) fs.writeFileSync(jsonOutputPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8');
  console.log(`COCKPIT_DRIVE ${JSON.stringify(report)}`);
  return report;
}


// ---------------------------------------------------------------------------
// #446 acceptance suite.
//
// The rule this suite exists to obey: a negative result is uninterpretable
// without a positive control that PASSES in the same run. Every case below is
// gated on `PC-DIRECT` -- a click posted by a separate process through exactly
// the route the fix uses (`CGEventPost(.cgSessionEventTap)`) at exactly the
// coordinate under test. If that control does not land in the sentinel's own
// event ledger, the harness is broken and this suite reports NO RESULT rather
// than reporting zeros as findings.
// ---------------------------------------------------------------------------

function sentinelGeometry() {
  const record = sentinelEvents().find((event) => event.kind === 'geometry');
  if (!record) throw new Error('sentinel never reported its geometry');
  return record;
}

function hostileEventsSince(sinceMs) {
  return sentinelEvents().filter((event) => event.kind === 'hostile' && event.tMs >= sinceMs);
}

function cursorSamplesSince(sinceMs) {
  return sentinelEvents().filter((event) => event.kind === 'cursor' && event.tMs >= sinceMs);
}

function cursorTravelSince(sinceMs) {
  const samples = cursorSamplesSince(sinceMs);
  let travel = 0;
  for (let i = 1; i < samples.length; i += 1) {
    travel += Math.hypot(samples[i].x - samples[i - 1].x, samples[i].y - samples[i - 1].y);
  }
  return { travel, samples };
}

function lastCursor() {
  const samples = sentinelEvents().filter((event) => event.kind === 'cursor');
  return samples.at(-1) ?? null;
}

async function waitForHostile(sinceMs, predicate, label, timeoutMs = acquisitionTimeoutMs) {
  return waitUntil(label, () => hostileEventsSince(sinceMs).find(predicate) ?? null, timeoutMs, 40);
}

function runSentinelTool(args, label) {
  const result = spawnSync(photonSentinelBinary, args, { encoding: 'utf8', timeout: 8000 });
  if (result.status !== 0) {
    throw new Error(`${label} failed (status ${result.status}): ${(result.stderr || result.stdout || '').trim()}`);
  }
  return (result.stdout || '').trim();
}

function petalLogTail() {
  const candidates = [
    process.env.PETAL_ACCEPTANCE_LOG,
    path.join(os.homedir(), 'Library', 'Logs', 'Petal', 'petal.log'),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (!fs.existsSync(candidate)) continue;
    const text = fs.readFileSync(candidate, 'utf8');
    return text.slice(-400_000);
  }
  return '';
}

function petalLogLinesSince(marker) {
  const text = petalLogTail();
  const index = marker ? text.lastIndexOf(marker) : -1;
  return (index >= 0 ? text.slice(index) : text).split('\n');
}

/// Raise the sentinel the same way Petal's own tier-3 does (AX raise +
/// AXFrontmost). Used only to establish the PRE-CONDITION for the positive
/// control -- never inside a Petal-driven measurement, where Petal must do its
/// own raising.
function raiseSentinel() {
  try {
    osascript(
      'tell application "System Events"',
      'tell process "PetalRCPhotonSentinel"',
      'set frontmost to true',
      'perform action "AXRaise" of window 1',
      'end tell',
      'end tell'
    );
  } catch {
    // Reported by the control's own pass/fail, not here.
  }
}

async function runAcceptance446Suite(ctx) {
  const geometry = sentinelGeometry();
  const results = [];
  const record = (id, status, detail, extra = {}) => {
    const entry = { id, status, detail, ...extra };
    results.push(entry);
    console.log(`ACCEPT ${JSON.stringify(entry)}`);
    return entry;
  };
  const A = REMOTE_CONTROL_ACCEPTANCE_446;
  const center = geometry.hostileCenterTopLeft;

  // ---- PC-DIRECT: the gate. -------------------------------------------------
  raiseSentinel();
  await sleep(400);
  let mark = Date.now();
  runSentinelTool(['--click', String(center.x), String(center.y)], 'positive-control click');
  let controlOk = false;
  try {
    await waitForHostile(mark, (event) => event.action === 'down', 'positive-control mouse-down', 4000);
    await waitForHostile(mark, (event) => event.action === 'up', 'positive-control mouse-up', 4000);
    controlOk = true;
    record('PC-DIRECT', 'pass', `session-tap click at (${center.x},${center.y}) actuated the AX-hostile canvas`);
  } catch (error) {
    record('PC-DIRECT', 'fail', `positive control did not land: ${error.message}`);
  }
  if (!controlOk) {
    console.log('ACCEPT_ABORT positive control failed -- reporting NO RESULT rather than zeros');
    return { mode: 'acceptance-446', controlPassed: false, results, geometry };
  }

  // ---- Take control. Both forms mint a grant token since #580; this one
  // keeps exercising the no-argument resolution path.
  const granted = await evaluate(
    ctx.cdp,
    `(() => {
      const api = window.__petalHarness?.remoteControl;
      if (!api) throw new Error('window.__petalHarness.remoteControl is unavailable');
      return api.request();
    })()`
  );
  if (!granted || typeof granted.windowId !== 'number') {
    throw new Error(`api.request() returned no usable target: ${JSON.stringify(granted)}`);
  }
  if (granted.windowId !== ctx.target.windowId) {
    throw new Error(`api.request() resolved window ${granted.windowId}, expected the sentinel ${ctx.target.windowId}`);
  }
  await waitForActiveStatus(ctx, Date.now() - 1);
  record('GRANT', 'pass', `control active on window ${granted.windowId} via api.request() (no explicit target)`);

  // ---- A1: click actuates AX-hostile content. --------------------------------
  // A1 also calibrates where `A.hostileCenter` lands in the target's own local
  // coordinates. A7 asserts its cancellation release against this MEASURED
  // anchor (#611) rather than a second hardcoded copy of the sentinel geometry.
  let hostileCenterLocal = null;
  mark = Date.now();
  await send(ctx, `api.click({ target, ...${JSON.stringify(A.hostileCenter)}, button: ${REMOTE_CONTROL_BUTTONS.left} }); return true;`);
  try {
    const down = await waitForHostile(mark, (event) => event.action === 'down', 'remote click mouse-down');
    await waitForHostile(mark, (event) => event.action === 'up', 'remote click mouse-up');
    hostileCenterLocal = { x: down.localX, y: down.localY };
    record('A1-CLICK', 'pass', 'remote click actuated the AX-hostile canvas', {
      localX: down.localX, localY: down.localY,
    });
  } catch (error) {
    record('A1-CLICK', 'fail', error.message);
  }

  // ---- A5: host cursor restored after that gesture. ---------------------------
  // Measured around its own dedicated click so the restore is not confounded
  // with A1's assertions.
  raiseSentinel();
  await sleep(300);
  const cursorBefore = lastCursor();
  mark = Date.now();
  await send(ctx, `api.click({ target, ...${JSON.stringify(A.hostileCenter)}, button: ${REMOTE_CONTROL_BUTTONS.left} }); return true;`);
  try {
    await waitForHostile(mark, (event) => event.action === 'up', 'restore-case mouse-up');
    await sleep(600);
    const after = lastCursor();
    const distance = Math.hypot(after.x - cursorBefore.x, after.y - cursorBefore.y);
    const injected = Math.hypot(center.x - cursorBefore.x, center.y - cursorBefore.y);
    if (injected < 20) {
      record('A5-RESTORE', 'skip', `host cursor started within ${injected.toFixed(1)}pt of the injection point; restore is unobservable`);
    } else if (distance <= 6.0) {
      record('A5-RESTORE', 'pass', `cursor returned to within ${distance.toFixed(2)}pt of its pre-gesture position (moved ${injected.toFixed(1)}pt during the gesture)`);
    } else {
      record('A5-RESTORE', 'fail', `cursor ended ${distance.toFixed(2)}pt from its pre-gesture position`);
    }
  } catch (error) {
    record('A5-RESTORE', 'fail', error.message);
  }

  // ---- A2: drag actuates AX-hostile content. ---------------------------------
  raiseSentinel();
  await sleep(300);
  mark = Date.now();
  await send(
    ctx,
    `return api.drag({ target, from: ${JSON.stringify(A.hostileDragFrom)}, to: ${JSON.stringify(A.hostileDragTo)}, steps: ${A.dragSteps}, button: ${REMOTE_CONTROL_BUTTONS.left} });`
  );
  try {
    await waitForHostile(mark, (event) => event.action === 'down', 'remote drag mouse-down');
    await waitForHostile(mark, (event) => event.action === 'drag', 'remote drag mouse-dragged');
    await waitForHostile(mark, (event) => event.action === 'up', 'remote drag mouse-up');
    const drags = hostileEventsSince(mark).filter((event) => event.action === 'drag');
    const spread = Math.max(...drags.map((event) => event.localX)) - Math.min(...drags.map((event) => event.localX));
    record('A2-DRAG', 'pass', `remote drag actuated the canvas: ${drags.length} dragged events spanning ${spread.toFixed(0)}pt`);
  } catch (error) {
    record('A2-DRAG', 'fail', error.message);
  }

  // ---- A3: scroll actuates AX-hostile content. -------------------------------
  raiseSentinel();
  await sleep(300);
  mark = Date.now();
  await send(
    ctx,
    `api.wheel({ target, ...${JSON.stringify(A.hostileCenter)}, ...${JSON.stringify(A.wheel)} }); return true;`
  );
  try {
    const scroll = await waitForHostile(mark, (event) => event.action === 'scroll', 'remote scroll wheel event');
    const nonZero = hostileEventsSince(mark).some(
      (event) => event.action === 'scroll' && (Math.abs(event.scrollingDeltaY) > 0 || Math.abs(event.scrollingDeltaX) > 0)
    );
    if (nonZero) {
      record('A3-SCROLL', 'pass', `remote scroll actuated the canvas (deltaY=${scroll.scrollingDeltaY})`);
    } else {
      record('A3-SCROLL', 'fail', 'scroll events arrived with zero delta on both axes');
    }
  } catch (error) {
    record('A3-SCROLL', 'fail', error.message);
  }

  // ---- A4: an AX-serviceable target still takes the AX path, cursor unmoved. --
  raiseSentinel();
  await sleep(400);
  mark = Date.now();
  const cursorBeforeAx = lastCursor();
  // `axAction` records carry no timestamp, so count them instead of matching
  // one by time -- a delta of at least one is the observation.
  const axPressesBefore = sentinelEvents().filter((event) => event.kind === 'axAction' && event.action === 'press').length;
  await send(ctx, `api.click({ target, ...${JSON.stringify(A.axButtonCenter)}, button: ${REMOTE_CONTROL_BUTTONS.left} }); return true;`);
  try {
    await waitUntil(
      'AX press on the AppKit button',
      () =>
        sentinelEvents().filter((event) => event.kind === 'axAction' && event.action === 'press').length >
        axPressesBefore,
      acquisitionTimeoutMs,
      40
    );
    const axPresses =
      sentinelEvents().filter((event) => event.kind === 'axAction' && event.action === 'press').length - axPressesBefore;
    const { travel } = cursorTravelSince(mark);
    const hostileDuring = hostileEventsSince(mark).length;
    const afterAx = lastCursor();
    const drift = Math.hypot(afterAx.x - cursorBeforeAx.x, afterAx.y - cursorBeforeAx.y);
    if (travel <= 1.0 && drift <= 1.0 && hostileDuring === 0) {
      record('A4-AX-PATH', 'pass', `AX press observed (${axPresses} total); cursor travel ${travel.toFixed(2)}pt, drift ${drift.toFixed(2)}pt, zero coordinate-route events`);
    } else {
      record('A4-AX-PATH', 'fail', `AX press observed but cursor travel=${travel.toFixed(2)}pt drift=${drift.toFixed(2)}pt hostileEvents=${hostileDuring}`);
    }
  } catch (error) {
    record('A4-AX-PATH', 'fail', error.message);
  }

  // ---- A6: restore SKIPPED when the host reclaims the mouse. -----------------
  // Driven through the WHEEL path deliberately. A pointer gesture's own Up is
  // posted AT the gesture point, which itself moves the OS cursor there and
  // re-arms `last_posted` immediately before the takeover ends -- so a mid-drag
  // reclaim is structurally unobservable by the host-presence check. Wheel has
  // no Up, so its restore is deferred by SESSION_TAP_WHEEL_SETTLE (300ms) to
  // the watchdog, which is exactly the window the policy was written for. The
  // pointer variant is measured too, and reported, but as an observation.
  raiseSentinel();
  await sleep(300);
  mark = Date.now();
  const cursorBeforeWheel = lastCursor();
  try {
    await send(
      ctx,
      `api.wheel({ target, ...${JSON.stringify(A.hostileCenter)}, ...${JSON.stringify(A.wheel)} }); return true;`
    );
    await waitForHostile(mark, (event) => event.action === 'scroll', 'A6 wheel event', 5000);
    // Host physically takes the mouse back, inside the 300ms settle window.
    const reclaimX = 40;
    const reclaimY = 40;
    runSentinelTool(['--warp', String(reclaimX), String(reclaimY)], 'host reclaim warp');
    await sleep(1500);
    const after = lastCursor();
    const distanceFromReclaim = Math.hypot(after.x - reclaimX, after.y - reclaimY);
    const distanceFromSaved = Math.hypot(after.x - cursorBeforeWheel.x, after.y - cursorBeforeWheel.y);
    const logged = petalLogLinesSince(null).some(
      (line) => line.includes('cursor restore skipped') && line.includes('host-moved-cursor')
    );
    if (distanceFromReclaim <= 8.0) {
      record('A6-RECLAIM', 'pass', `restore correctly skipped: cursor left where the host put it (${distanceFromReclaim.toFixed(2)}pt from the reclaim point, ${distanceFromSaved.toFixed(2)}pt from the pre-gesture position); host log line seen=${logged}`);
    } else {
      record('A6-RECLAIM', 'fail', `cursor was moved ${distanceFromReclaim.toFixed(2)}pt away from where the host put it (${distanceFromSaved.toFixed(2)}pt from the pre-gesture position); host log line seen=${logged}`);
    }
  } catch (error) {
    record('A6-RECLAIM', 'fail', error.message);
  }

  // ---- A6b: the same reclaim during a pointer drag, recorded as observation. --
  raiseSentinel();
  await sleep(300);
  mark = Date.now();
  const cursorBeforeDrag = lastCursor();
  try {
    await send(ctx, `api.pointer({ target, action: 'down', ...${JSON.stringify(A.hostileDragFrom)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`);
    await waitForHostile(mark, (event) => event.action === 'down', 'A6b mouse-down');
    runSentinelTool(['--warp', '40', '40'], 'A6b host reclaim warp');
    await sleep(150);
    await send(ctx, `api.pointer({ target, action: 'up', ...${JSON.stringify(A.hostileDragFrom)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 0 }); return true;`);
    await sleep(900);
    const after = lastCursor();
    const distanceFromReclaim = Math.hypot(after.x - 40, after.y - 40);
    const distanceFromSaved = Math.hypot(after.x - cursorBeforeDrag.x, after.y - cursorBeforeDrag.y);
    record('A6b-RECLAIM-DRAG', 'measured', `after a mid-drag reclaim the cursor ended ${distanceFromReclaim.toFixed(2)}pt from the reclaim point and ${distanceFromSaved.toFixed(2)}pt from its pre-gesture position`);
  } catch (error) {
    record('A6b-RECLAIM-DRAG', 'skip', error.message);
  }

  // ---- A7: revoke mid-drag posts a synthetic mouse-up (no phantom button). ----
  raiseSentinel();
  await sleep(300);
  mark = Date.now();
  await send(ctx, `api.pointer({ target, action: 'down', ...${JSON.stringify(A.hostileDragFrom)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`);
  try {
    const down = await waitForHostile(mark, (event) => event.action === 'down', 'A7 mouse-down');
    await send(
      ctx,
      `api.pointer({ target, action: 'move', ...${JSON.stringify(A.hostileCenter)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`
    );
    await sleep(200);
    await command(ctx.client, { cmd: 'remote-control-disable', window_id: ctx.target.windowId });
    const up = await waitForHostile(mark, (event) => event.action === 'up', 'synthetic mouse-up after revoke', 6000);
    const cancelled = petalLogLinesSince(null).some((line) => line.includes('session-tap gesture cancelled'));
    // #611: this case used to PRINT the up coordinate and never assert it, so
    // it stayed green while the release landed ~280pt away at the drag ORIGIN
    // -- the same print-don't-assert defect that hid Q-OCCLUDED. Assert both
    // sides: near where the drag ENDED, and provably not back at the origin.
    // A one-sided "not at the origin" check alone would be satisfied by a
    // release at any wrong coordinate.
    const releaseToleranceLocal = 24;
    const originDistance = Math.hypot(up.localX - down.localX, up.localY - down.localY);
    if (originDistance <= releaseToleranceLocal) {
      throw new Error(
        `revoke released at the drag ORIGIN (${up.localX.toFixed(0)},${up.localY.toFixed(0)}), ${originDistance.toFixed(0)}pt from the mouse-down, instead of where the drag ended (#611)`
      );
    }
    if (!hostileCenterLocal) {
      throw new Error('A1-CLICK did not measure where hostileCenter lands, so the release coordinate cannot be asserted');
    }
    const endDistance = Math.hypot(up.localX - hostileCenterLocal.x, up.localY - hostileCenterLocal.y);
    if (endDistance > releaseToleranceLocal) {
      throw new Error(
        `revoke released at (${up.localX.toFixed(0)},${up.localY.toFixed(0)}), ${endDistance.toFixed(0)}pt from where the drag ended (${hostileCenterLocal.x.toFixed(0)},${hostileCenterLocal.y.toFixed(0)}) -- tolerance ${releaseToleranceLocal}pt (#611)`
      );
    }
    record(
      'A7-REVOKE',
      'pass',
      `revoke released at (${up.localX.toFixed(0)},${up.localY.toFixed(0)}), within ${releaseToleranceLocal}pt of the drag end and ${originDistance.toFixed(0)}pt from the origin; host log line seen=${cancelled}`,
      { localX: up.localX, localY: up.localY, endDistance, originDistance }
    );
  } catch (error) {
    record('A7-REVOKE', 'fail', error.message);
  }

  // ---- Q: does tier 3 report success when its event cannot reach the target? --
  // A `.floating` occluder sits above anything AXRaise can lift a normal window
  // to, so the tier's raise and post both "succeed" while delivery provably does
  // not happen. The PC-DIRECT control above already proved the same coordinate
  // is deliverable when unobstructed, so a zero here is a real zero.
  let occluder = null;
  try {
    const box = geometry.hostileOnScreenBottomLeft;
    const topLeftY = geometry.screenFrame.h - (box.y + box.h);
    occluder = spawn(
      photonSentinelBinary,
      ['--occluder', String(box.x - 20), String(topLeftY - 20), String(box.w + 40), String(box.h + 40)],
      { stdio: ['ignore', 'pipe', 'pipe'] }
    );
    await sleep(1200);
    await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId }).catch(() => null);
    await evaluate(
      ctx.cdp,
      `(() => { const api = window.__petalHarness?.remoteControl; return api.request(); })()`
    );
    await waitForActiveStatus(ctx, Date.now() - 1).catch(() => null);
    // Control first: prove the occluder really does block this coordinate.
    raiseSentinel();
    await sleep(300);
    let occMark = Date.now();
    runSentinelTool(['--click', String(center.x), String(center.y)], 'occluded control click');
    await sleep(700);
    const controlBlocked = hostileEventsSince(occMark).length === 0;

    occMark = Date.now();
    const statusBefore = Date.now();
    await send(ctx, `api.click({ target, ...${JSON.stringify(A.hostileCenter)}, button: ${REMOTE_CONTROL_BUTTONS.left} }); return true;`);
    await sleep(1500);
    const delivered = hostileEventsSince(occMark).length;
    const outcomes = await evaluate(
      ctx.cdp,
      metricExpression(
        ctx.target,
        `return metrics.results.filter((entry) => entry.receivedAt >= ${statusBefore}).map(({ outcome, deliveryRoute, failureCode }) => ({ outcome, deliveryRoute, failureCode }));`
      )
    );
    const hostLines = petalLogLinesSince(null)
      .filter((line) => line.includes('mode=SessionTap') || line.includes('pre-post hit test'))
      .slice(-6);
    // #599: this was a bare `measured` -- it printed the numbers and left a
    // human to judge them, which is how `0 delivered / outcome=applied` sat in
    // the suite as a "result". The question is now asserted: with the coordinate
    // provably blocked (controlBlocked), reporting success is a FAIL.
    let status;
    let detail;
    if (!controlBlocked) {
      status = 'skip';
      detail = 'occluder did not actually block the coordinate; question not settled this run';
    } else if (delivered > 0) {
      status = 'skip';
      detail = `occluder blocked the control click but not Petal's (${delivered} events); occlusion unstable, question not settled`;
    } else if (outcomes.length === 0) {
      status = 'fail';
      detail = 'target received 0 events and NO controller outcome was recorded -- a nack cannot be distinguished from a lost result';
    } else if (outcomes.some((entry) => entry.outcome === 'applied')) {
      status = 'fail';
      detail = `#599: target received 0 events but the controller recorded outcome=applied -- the tier reported delivery it did not verify: ${JSON.stringify(outcomes)}`;
    } else {
      status = 'pass';
      detail = `occluded: target received 0 events and the controller was nacked instead of told applied: ${JSON.stringify(outcomes)}`;
    }
    record('Q-OCCLUDED', status, detail, { delivered, outcomes, hostLines, controlBlocked });
  } catch (error) {
    record('Q-OCCLUDED', 'skip', `occlusion probe did not run: ${error.message}`);
  } finally {
    occluder?.kill('SIGTERM');
  }

  try {
    await send(ctx, 'api.release(target); return true;');
  } catch {
    // best-effort teardown
  }

  const report = {
    mode: 'acceptance-446',
    controlPassed: true,
    geometry,
    results,
    summary: {
      total: results.length,
      pass: results.filter((entry) => entry.status === 'pass').length,
      fail: results.filter((entry) => entry.status === 'fail').length,
      skip: results.filter((entry) => entry.status === 'skip').length,
    },
  };
  console.log(`ACCEPT_SUMMARY ${JSON.stringify(report.summary)}`);
  return report;
}

function sentinelEvents() {
  if (!fs.existsSync(sentinelEventLogPath)) return [];
  return fs.readFileSync(sentinelEventLogPath, 'utf8').split('\n').filter(Boolean).flatMap((line) => {
    try { return [JSON.parse(line)]; } catch { return []; }
  });
}

async function waitForSentinelEvent(predicate, label = 'sentinel event', timeoutMs = acquisitionTimeoutMs) {
  return waitUntil(label, () => sentinelEvents().find(predicate) ?? null, timeoutMs, 50);
}

function sentinelModifier(event, mask) {
  return (Number(event.modifierFlags) & mask) !== 0;
}

function displayplacerHasSecondaryDisplay() {
  try {
    const output = execFileSync('displayplacer', ['list'], { encoding: 'utf8', timeout: 3000 });
    return (output.match(/Persistent screen id:/g) ?? []).length > 1;
  } catch {
    return false;
  }
}

function readTextEditDocument() {
  // Only strip the single trailing newline `osascript` itself appends to its
  // stdout -- NOT `.trim()` (see `osascriptRaw`'s doc comment) -- so the
  // document's own leading/trailing spaces survive for the marker check.
  if (!sacrificialDocMarker) {
    return osascriptRaw('tell application "TextEdit"', 'if (count of documents) is 0 then return ""', 'return text of document 1', 'end tell').replace(/\n$/, '');
  }
  return osascriptRaw(
    'tell application "TextEdit"',
    'try',
    `return text of (first document whose name contains "${sacrificialDocMarker}")`,
    'on error',
    'return ""',
    'end try',
    'end tell'
  ).replace(/\n$/, '');
}

function setTextEditDocument(text) {
  if (!sacrificialDocMarker) {
    osascript(
      'tell application "TextEdit"',
      'if (count of documents) is 0 then make new document',
      `set text of document 1 to ${JSON.stringify(text)}`,
      'activate',
      'end tell'
    );
    return;
  }
  osascript(
    'tell application "TextEdit"',
    `set theDoc to first document whose name contains "${sacrificialDocMarker}"`,
    `set text of theDoc to ${JSON.stringify(text)}`,
    'activate',
    'end tell'
  );
}

function maybeReadTextEditSelection() {
  if (!sacrificialDocMarker) return null;
  try {
    const selected = osascript(
      'tell application "System Events"',
      'tell process "TextEdit"',
      'set frontmost to true',
      'try',
      `set theWin to first window whose name contains "${sacrificialDocMarker}"`,
      'return value of attribute "AXSelectedText" of text area 1 of scroll area 1 of theWin',
      'on error',
      'return ""',
      'end try',
      'end tell',
      'end tell'
    );
    return selected;
  } catch {
    return null;
  }
}

async function waitForTextEditSelection(timeoutMs = acquisitionTimeoutMs, intervalMs = 250) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const selected = maybeReadTextEditSelection();
    if (selected === null || selected) return selected;
    await sleep(intervalMs);
  }
  return '';
}

function maybeReadTextEditScrollValue() {
  if (!sacrificialDocMarker) return null;
  try {
    const raw = osascript(
      'tell application "System Events"',
      'tell process "TextEdit"',
      'set frontmost to true',
      'try',
      `set theWin to first window whose name contains "${sacrificialDocMarker}"`,
      'return value of attribute "AXVisibleCharacterRange" of text area 1 of scroll area 1 of theWin',
      'on error',
      'return ""',
      'end try',
      'end tell',
      'end tell'
    );
    return raw ? raw : null;
  } catch {
    return null;
  }
}

function closeTextEditSacrificialDocument() {
  if (!sacrificialDocMarker) return;
  try {
    osascript(
      'tell application "TextEdit"',
      'try',
      `close (first document whose name contains "${sacrificialDocMarker}") saving no`,
      'end try',
      'end tell'
    );
  } catch {
    // best-effort cleanup only -- never fail the run over this.
  }
}

function captureTextEditWedgeForensics() {
  const timestamp = Date.now();
  const samplePath = path.join(os.tmpdir(), `petal-rc-wedge-sample-${timestamp}.txt`);
  const screenshotPath = path.join(os.tmpdir(), `petal-rc-wedge-screenshot-${timestamp}.png`);

  try {
    spawnSync('sample', ['TextEdit', '2', '-file', samplePath], { stdio: 'ignore' });
  } catch {
    // Best-effort wedge diagnostics only; recovery must still proceed.
  }
  try {
    spawnSync('screencapture', ['-x', screenshotPath], { stdio: 'ignore' });
  } catch {
    // Best-effort wedge diagnostics only; recovery must still proceed.
  }

  const location = process.env.PETAL_REMOTE_OSASCRIPT_HOST ? 'remote host via PATH shims' : 'local host';
  console.log(`# WEDGE-FORENSICS sample=${samplePath} screenshot=${screenshotPath} location=${location}`);
}

// Any failing case gets the same on-machine screenshot `captureTextEditWedgeForensics`
// already takes for the TextEdit-wedge case specifically (via `spawnSync('screencapture', ...)`,
// which resolves to the real remote host under cross-machine-rc-suite.sh's PATH shims --
// see that script's `screencapture` wrapper), plus a `dump_metrics` snapshot (network state +
// a short diagnostics-journal tail) so a failure has forensic evidence attached without anyone
// needing to reproduce it live. Best-effort only: a forensics-capture failure must never mask
// the real test failure it's trying to document.
async function captureCaseFailureForensics(client, caseId) {
  const timestamp = Date.now();
  const screenshotPath = path.join(os.tmpdir(), `petal-rc-case-${caseId}-failure-${timestamp}.png`);
  let screenshot = null;
  try {
    spawnSync('screencapture', ['-x', screenshotPath], { stdio: 'ignore' });
    screenshot = screenshotPath;
  } catch {
    // Best-effort only.
  }
  let metrics = null;
  try {
    metrics = await command(client, { cmd: 'dump_metrics', journal_tail: 20 });
  } catch {
    // Best-effort only -- diagnostics state may be unavailable, or the socket
    // itself may already be in a bad state if the failure was connection-related.
  }
  console.log(`# FAILURE-FORENSICS case=${caseId} screenshot=${screenshot ?? 'unavailable'} metrics=${metrics ? 'captured' : 'unavailable'}`);
  return { screenshot, metrics };
}

function readClipboard() {
  return execFileSync('pbpaste', { encoding: 'utf8' });
}

function writeClipboard(text) {
  execFileSync('pbcopy', { input: text, encoding: 'utf8' });
}

async function cdpPageWebSocket() {
  let pages;
  try {
    const response = await fetch(cdpListUrl);
    pages = await response.json();
  } catch (error) {
    skip(
      `Chrome DevTools endpoint not reachable at ${cdpListUrl}. Launch Chrome with --remote-debugging-port=9222 and open the web harness. (${error.message})`
    );
  }
  const page = pages.find((candidate) => candidate.type === 'page' && candidate.url?.includes(harnessUrlNeedle));
  if (!page?.webSocketDebuggerUrl) {
    skip(`no Chrome tab matching "${harnessUrlNeedle}" found at ${cdpListUrl}`);
  }
  return { wsUrl: page.webSocketDebuggerUrl, pageUrl: page.url };
}

function connectCdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let nextId = 1;
  const pending = new Map();
  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    if (!message.id) return;
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(message.error.message));
    else waiter.resolve(message.result);
  });
  return new Promise((resolve, reject) => {
    ws.addEventListener('open', () => {
      resolve({
        call(method, params = {}) {
          const id = nextId++;
          ws.send(JSON.stringify({ id, method, params }));
          return new Promise((callResolve, callReject) => {
            pending.set(id, { resolve: callResolve, reject: callReject });
          });
        },
        close() {
          ws.close();
        },
      });
    });
    ws.addEventListener('error', () => reject(new Error(`failed to connect CDP websocket ${wsUrl}`)));
  });
}

async function evaluate(cdp, expression) {
  const result = await cdp.call('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description || result.exceptionDetails.text || 'browser evaluation failed');
  }
  return result.result?.value;
}

function harnessExpression(target, body) {
  return `(() => {
    const api = window.__petalHarness?.remoteControl;
    if (!api) throw new Error('window.__petalHarness.remoteControl is unavailable');
    const target = ${JSON.stringify(target)};
    ${body}
  })()`;
}

function metricExpression(target, body) {
  return harnessExpression(target, `const metrics = api.metrics(); ${body}`);
}

// The cross-machine harness consumes only this bounded terminal-result shape.
// Do not return general harness metrics: they can contain identities, published
// packets, target information, text, coordinates, or diagnostic strings.
async function collectTerminalDeliveries(ctx) {
  const records = await evaluate(
    ctx.cdp,
    metricExpression(
      ctx.target,
      `return metrics.results
        .filter((entry) => entry.receivedAt >= ${ctx.caseStartedAt})
        .map(({ inputId, inputSeq, outcome, deliveryRoute, failureCode, windowId, receivedAt }) => ({ inputId, inputSeq, outcome, ...(deliveryRoute === undefined ? {} : { deliveryRoute }), ...(failureCode === undefined ? {} : { failureCode }), windowId, receivedAt }));`
    )
  );
  if (!Array.isArray(records)) throw new Error('remote-control metrics results were malformed');
  return records;
}

async function waitForTerminalDeliveryCount(ctx, count, timeoutMs = acquisitionTimeoutMs) {
  return waitUntil(
    `${count} terminal remote-control deliveries`,
    async () => {
      const records = await collectTerminalDeliveries(ctx);
      return records.length >= count ? records : null;
    },
    timeoutMs,
    50
  );
}

function sameTerminalDelivery(left, right) {
  return left.inputId === right.inputId
    && left.inputSeq === right.inputSeq
    && left.outcome === right.outcome
    && left.deliveryRoute === right.deliveryRoute
    && left.failureCode === right.failureCode
    && left.windowId === right.windowId;
}

async function joinWebHarness(cdp, pageUrl, credential, livekitRoom) {
  const url = new URL(pageUrl);
  const joinUrl = `${url.origin}/?code=${encodeURIComponent(credential)}`;
  await cdp.call('Page.enable');
  await evaluate(
    cdp,
    `(() => {
      try { localStorage.setItem('petal-harness-name', 'rc-harness'); } catch {}
      return true;
    })()`
  );
  await cdp.call('Page.navigate', { url: joinUrl });
  await waitUntil(
    // The DOM alone is not readiness: on a cold vite start the harness module
    // can still be transforming when the elements exist, and the join click
    // then no-ops into a timeout that reads as a room/transport failure. Any
    // manual debugging warms the page, so this passes while you watch and
    // fails when you do not (docs/TESTING.md).
    'web harness DOM + __petalHarness',
    () => evaluate(cdp, `!!document.querySelector('#display-name') && !!document.querySelector('#meeting-code') && !!document.querySelector('#join-btn') && !!window.__petalHarness`),
    8000
  );
  await evaluate(
    cdp,
    `(() => {
      localStorage.setItem('petal-harness-name', 'rc-harness');
      const display = document.querySelector('#display-name');
      const code = document.querySelector('#meeting-code');
      const join = document.querySelector('#join-btn');
      display.value = 'rc-harness';
      code.value = ${JSON.stringify(credential)};
      display.dispatchEvent(new Event('input', { bubbles: true }));
      code.dispatchEvent(new Event('input', { bubbles: true }));
      if (window.__petalHarness?.room?.state !== 'connected') join.click();
      return true;
    })()`
  );
  const connected = await waitUntil(
    'web harness connected to native room',
    () =>
      evaluate(
        cdp,
        `(() => {
          const room = window.__petalHarness?.room;
          if (!room) return null;
          return {
            state: room.state,
            name: room.name ?? room.roomInfo?.name ?? null,
            remoteParticipants: room.remoteParticipants?.size ?? 0
          };
        })()`
      ).then((state) => (state?.state === 'connected' && state.remoteParticipants > 0 ? state : null)),
    // Overridable: on a heavily loaded machine the browser peer's ICE/DTLS
    // setup can exceed the default comfortably, and a join timeout is a
    // harness-environment failure, not a product signal.
    Number(process.env.PETAL_REMOTE_CONTROL_WEB_JOIN_TIMEOUT_MS || 15000),
    100
  );
  if (connected.name !== livekitRoom) {
    throw new Error(`web/native LiveKit room mismatch: web=${JSON.stringify(connected.name)} native=${JSON.stringify(livekitRoom)}`);
  }
  return connected;
}

async function waitForMatchingWindow(client, selectors, timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  let lastError = '';
  while (Date.now() < deadline) {
    const response = await client.send({ cmd: 'share_matching', ...selectors });
    if (response.ok) return response.result;
    lastError = response.error;
    await sleep(350);
  }
  throw new Error(`matching shareable window did not appear: ${lastError}`);
}

function compilePhotonSentinel() {
  fs.mkdirSync(path.dirname(photonSentinelBinary), { recursive: true });
  fs.writeFileSync(
    path.join(photonSentinelBundle, 'Contents', 'Info.plist'),
    `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>PetalRCPhotonSentinel</string>
<key>CFBundleIdentifier</key><string>com.petal.testing.rc-photon-sentinel</string>
<key>CFBundleName</key><string>PetalRCPhotonSentinel</string>
<key>CFBundleDisplayName</key><string>Petal RC Photon Sentinel</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
`,
    'utf8'
  );
  const result = spawnSync(
    'xcrun',
    ['swiftc', photonSentinelSource, '-framework', 'AppKit', '-o', photonSentinelBinary],
    {
      encoding: 'utf8',
      env: {
        ...process.env,
        SWIFT_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-rc-photon-swift-cache'),
        CLANG_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-rc-photon-clang-cache')
      }
    }
  );
  if (result.error) throw new Error(`failed to launch swiftc for photon sentinel: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`failed to compile photon sentinel: ${(result.stderr || result.stdout || '').trim()}`);
  }
}

// One unverified SIGTERM used to be the whole teardown: no wait, no SIGKILL
// escalation, no verification, and the handle was nulled immediately, so a
// sentinel stuck in a modal/AX prompt survived silently.
function stopPhotonSentinel() {
  if (!photonSentinelLease) {
    photonSentinelProcess = null;
    return;
  }
  processLeases.release(photonSentinelLease);
  photonSentinelLease = null;
  photonSentinelProcess = null;
}

async function bootstrapPhotonSentinel(client, cdp) {
  if (process.env.PETAL_REMOTE_OSASCRIPT_HOST) {
    throw new Error('press-to-photon mode is local-only; PETAL_REMOTE_OSASCRIPT_HOST is not supported');
  }
  ensureLeaseLedgerReady();
  compilePhotonSentinel();

  // `detached: true` gives the sentinel its own process group, so teardown can
  // signal the group rather than one pid. stdio stays piped -- readiness is
  // detected from its stdout.
  const child = spawn(photonSentinelBinary, [], {
    detached: true,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, PETAL_RC_SENTINEL_EVENT_LOG: sentinelEventLogPath },
  });
  if (!Number.isInteger(child.pid)) {
    child.kill('SIGTERM');
    throw new Error('photon sentinel launched without a process id');
  }
  photonSentinelProcess = child;
  photonSentinelLease = processLeases.register('photon-sentinel', child, {
    command: 'PetalRCPhotonSentinel',
  });
  clearSentinelEventLog();
  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (chunk) => {
    stdout += chunk;
  });
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });

  try {
    await waitUntil(
      'native photon sentinel ready',
      () => {
        if (child.exitCode !== null) {
          throw new Error(`photon sentinel exited ${child.exitCode}: ${stderr.trim()}`);
        }
        return stdout.includes('PETAL_RC_PHOTON_SENTINEL_READY') ? true : null;
      },
      5000,
      25
    );
    const shared = await waitForMatchingWindow(client, { pid: child.pid }, 10_000);
    await ensureShareReady(client, cdp, shared);
    return { shared, target: { targetUserId, windowId: shared.windowId } };
  } catch (error) {
    stopPhotonSentinel();
    throw error;
  }
}

async function waitForLiveTile(cdp, windowId, timeoutMs) {
  // Hoisted, because the probe state used to be discarded on the failing path
  // and every timeout reported `last=null` -- the reason five consecutive
  // share-ready failures produced no information at all.
  let lastState = null;
  try {
    return await waitUntil(
      `live video tile for window ${windowId}`,
      async () => {
        lastState = await evaluate(cdp, liveTileProbeExpression(windowId));
        return tileAccepted(lastState) ? lastState : null;
      },
      timeoutMs,
      100
    );
  } catch (error) {
    const failure = new Error(`${error.message} ${tileFailureDetail(lastState)}`);
    failure.lastTileState = lastState;
    throw failure;
  }
}

async function ensureShareReady(client, cdp, shared) {
  // Confirmed live: the old retry strategy (stop_share + start_share + wait
  // shareReadyTimeoutMs again, up to 3x) actively worked against itself --
  // tile readiness is fundamentally about LiveKit/encoder/Chrome-tab startup
  // timing, not about anything a restarted share fixes (see #61's "restarting
  // doesn't make idle content less idle" finding, same idea applies here:
  // restarting a share resets whatever startup progress had already been
  // made, so 3x8s-then-restart could take LONGER to succeed than one patient
  // wait). Share once, wait up to 3x as long without ever restarting.
  await command(client, { cmd: 'share', window_id: shared.windowId });
  try {
    const live = await waitForLiveTile(cdp, shared.windowId, shareReadyTimeoutMs * 3);
    console.log(
      `# share-ready window=${shared.windowId} tile=${live.tileId} readiness=${shareReadinessMode(inputOnlyMode)}`
    );
  } catch (error) {
    // This already screenshots and dumps metrics -- it just never ran for the
    // one failure that mattered.
    await captureCaseFailureForensics(client, 'share-ready');
    throw new Error(`shared window ${shared.windowId} never produced a live web video tile: ${error.message}`);
  }
  await assertShareBorderStacked(client, shared.windowId);
}

// Guards the NSPanel lifecycle crash class this repo has hit before (a share
// border that fails to actually order in front of its source window is either
// invisible to the user or, worse, a sign the panel retire/reuse lifecycle
// left a stale/hidden panel behind). `qa_share_border_stack_report` (#300) is
// a WindowServer front-to-back readback (index 0 = frontmost) that exists
// specifically for this kind of QA assertion -- this was the one caller
// missing to make it load-bearing rather than dead code.
async function assertShareBorderStacked(client, windowId) {
  const report = await command(client, { cmd: 'share_border_stack', window_id: windowId });
  if (report.border == null || report.border.stackIndex == null) {
    throw new Error(`share border missing or not on-screen for window ${windowId}: ${JSON.stringify(report)}`);
  }
  if (report.source.stackIndex == null) {
    throw new Error(`shared source window ${windowId} not found in the on-screen stack: ${JSON.stringify(report)}`);
  }
  if (report.border.stackIndex >= report.source.stackIndex) {
    throw new Error(`share border is not stacked in front of its source window: ${JSON.stringify(report)}`);
  }
  console.log(`# share-border-stack window=${windowId} border=${report.border.stackIndex} source=${report.source.stackIndex} (border in front)`);
}

async function bootstrapTextEditTarget(client, cdp) {
  // #69: TextEdit can wedge after sustained CGEventPostToPid replay plus
  // AppleScript polling. Recovery needs a fresh process/window, not backoff.
  try {
    osascript('tell application "System Events" to tell process "TextEdit" to quit');
  } catch {
    // no running TextEdit process, or it didn't respond to a polite quit --
    // the force-kill below covers both.
  }
  spawnSync('pkill', ['-9', '-x', 'TextEdit'], { stdio: 'ignore' });
  await sleep(500);

  const marker = `petal-remote-control-${Date.now()}`;
  const targetPath = path.join(os.tmpdir(), `${marker}.txt`);
  // Confirmed live, real flakiness (not a wedge, not a code bug): opening a
  // genuinely EMPTY document meant ScreenCaptureKit had nothing to paint
  // beyond the very first frame -- it only delivers a callback on an actual
  // screen change, so a truly blank/static window can sit for 7s+ producing
  // zero new frames (matches this repo's own documented
  // callback-per-screen-change behavior). `ensureShareReady`'s
  // stop_share/start_share retry doesn't help here -- restarting the share
  // doesn't make idle content less idle, it just resets the readiness clock.
  // Seeding real multi-line text before the first share attempt gives
  // ScreenCaptureKit substantial content to paint immediately, so the first
  // frame (and the web tile's readyState=4 check) arrives promptly. Each
  // case still resets the document to its own content via
  // `setTextEditDocument`, so this seed only affects the initial share-ready
  // wait, not any assertion.
  fs.writeFileSync(targetPath, `${marker}\nremote-control live suite\n`.repeat(20), 'utf8');
  // `timeout` is load-bearing here too, same reason as `osascript()` above:
  // a wedged cfprefsd (observed live, sustained 44+min hang on a busy shared
  // Mac) or a stuck `open` otherwise blocks this synchronous call forever,
  // taking the whole suite down with it instead of failing this case.
  spawnSync('defaults', ['write', 'com.apple.TextEdit', 'NSAutomaticCapitalizationEnabled', '-bool', 'false'], { stdio: 'ignore', timeout: 5000 });
  spawnSync('defaults', ['write', 'com.apple.TextEdit', 'NSAutomaticSpellingCorrectionEnabled', '-bool', 'false'], { stdio: 'ignore', timeout: 5000 });
  const opened = spawnSync('open', ['-a', 'TextEdit', targetPath], { stdio: 'ignore', timeout: 5000 });
  if (opened.status !== 0) throw new Error('failed to open sacrificial TextEdit document');
  sacrificialDocMarker = marker;
  try {
    osascript(
      'tell application "TextEdit"',
      'try',
      `close (every document whose name does not contain "${marker}") saving no`,
      'end try',
      'end tell'
    );
  } catch {
    // best-effort cleanup only -- stale restored documents should not fail bootstrap.
  }

  const shared = await waitForMatchingWindow(client, { app_name: 'TextEdit', title_contains: marker });
  await ensureShareReady(client, cdp, shared);
  return { shared, target: { targetUserId, windowId: shared.windowId } };
}

async function waitForActiveStatus(ctx, startedAt) {
  const status = await waitUntil(
    'native host active status',
    () =>
      evaluate(
        ctx.cdp,
        metricExpression(
          ctx.target,
          `return metrics.statuses.find((m) => m.windowId === target.windowId && m.status === 'active' && m.receivedAt >= ${startedAt}) ?? null;`
        )
      ),
    statusTimeoutMs
  );
  // #580: an `active` status is NOT proof this peer can inject. The host
  // requires a grant token on every input packet
  // (TOKENLESS_GRANT_COMPATIBILITY_ENABLED = false) and silently drops
  // tokenless ones -- so a run could previously inject nothing at all while
  // its absence-asserting cases (24 "release drops later input", 27
  // "non-focus-stealing", Q-OCCLUDED) still reported pass. Fail loudly here:
  // a scenario that cannot inject must never report PASS.
  const grant = await evaluate(ctx.cdp, harnessExpression(ctx.target, 'return api.grant(target);'));
  if (!grant?.granted) {
    // #808: `granted:false` has two very different causes and the old message
    // could not tell them apart: the controller adopted a status but got no
    // token, or its `activeRemoteControl` is null / points at another window
    // so `api.grant()` never even compares tokens. Report which.
    const active = await evaluate(
      ctx.cdp,
      harnessExpression(ctx.target, 'return api.active();')
    );
    const statuses = await evaluate(
      ctx.cdp,
      metricExpression(
        ctx.target,
        // NOT `hasToken`: `statusMetrics` records no grantToken field at all
        // (harnessApi.ts), so any token check here reads false for every
        // status and cannot distinguish "arrived without a token" from
        // "arrived with one" -- the exact class of uninformative signal this
        // repo keeps getting burned by. Report what the metric really has,
        // plus the sender, which is what the session-restore path gates on.
        `return metrics.statuses.filter((m) => m.receivedAt >= ${startedAt}).slice(-6).map((m) => ({windowId: m.windowId, status: m.status, sender: m.senderIdentity, seq: m.seq, receivedAt: m.receivedAt}));`
      )
    );
    throw new Error(
      `remote control reported active but the harness holds no grant token; every input packet would be dropped (#580): ${JSON.stringify(grant)} active=${JSON.stringify(active)} statusesSinceStart=${JSON.stringify(statuses)}`
    );
  }
  return status;
}

// #580: the host-side half of the same gate. Read the host's own drop
// warnings rather than trusting the browser's view alone.
function tokenlessDropLines(marker) {
  return petalLogLinesSince(marker).filter((line) => line.includes('dropping tokenless input'));
}

async function send(ctx, body) {
  return evaluate(ctx.cdp, harnessExpression(ctx.target, body));
}

async function published(ctx, predicateSource, timeoutMs = acquisitionTimeoutMs) {
  return waitUntil(
    'remote-control publish metric',
    () =>
      evaluate(
        ctx.cdp,
        metricExpression(ctx.target, `return metrics.published.find((m) => m.windowId === target.windowId && (${predicateSource})) ?? null;`)
      ),
    timeoutMs
  );
}

function pass(detail, measurement = {}) {
  return { status: 'pass', detail, ...measurement };
}

function skipCase(detail) {
  return { status: 'skip', detail };
}

async function assertDocumentIncludes(fragment, timeoutMs = acquisitionTimeoutMs) {
  const text = await waitUntil(
    `TextEdit document contains ${fragment}`,
    () => {
      const current = readTextEditDocument();
      return current.includes(fragment) ? current : null;
    },
    timeoutMs
  );
  return text;
}

function roundMs(value) {
  return Math.round(value * 10) / 10;
}

function deterministicRandom(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 0x100000000;
  };
}

// `sample` is a per-class label, NOT a time index -- the schedule is shuffled after
// numbering, so sample 1 may run last. Do not filter on it to exclude a warmup/ramp
// window (`sample > 3` was used that way against the old sequential harness and is
// now meaningless); use `mediaTime` for anything temporal. See #288.
function shuffledPhotonInputSchedule(samplesPerInput, seed) {
  const schedule = ['text', 'click'].flatMap((inputKind) =>
    Array.from({ length: samplesPerInput }, (_, index) => ({ inputKind, sample: index + 1 }))
  );
  const random = deterministicRandom(seed);
  for (let index = schedule.length - 1; index > 0; index -= 1) {
    const swapIndex = Math.floor(random() * (index + 1));
    [schedule[index], schedule[swapIndex]] = [schedule[swapIndex], schedule[index]];
  }
  return schedule;
}

function elapsedTimeVsLatencySlope(samples) {
  const pairs = samples.filter(
    ({ mediaTime, pressToEstimatedPhotonMs }) =>
      Number.isFinite(mediaTime) && Number.isFinite(pressToEstimatedPhotonMs)
  );
  if (pairs.length < 2) return { samples: pairs.length, slopeMsPerSecond: null };

  const meanMediaTime = pairs.reduce((sum, sample) => sum + sample.mediaTime, 0) / pairs.length;
  const meanLatencyMs = pairs.reduce((sum, sample) => sum + sample.pressToEstimatedPhotonMs, 0) / pairs.length;
  let covariance = 0;
  let mediaTimeVariance = 0;
  for (const sample of pairs) {
    const mediaTimeDelta = sample.mediaTime - meanMediaTime;
    covariance += mediaTimeDelta * (sample.pressToEstimatedPhotonMs - meanLatencyMs);
    mediaTimeVariance += mediaTimeDelta * mediaTimeDelta;
  }
  return {
    samples: pairs.length,
    slopeMsPerSecond: mediaTimeVariance === 0 ? null : roundMs(covariance / mediaTimeVariance)
  };
}

function summarizeElapsedTimeVsLatency(photonSamples) {
  const inputKinds = ['text', 'click'];
  return {
    method: 'least-squares',
    unit: 'ms per second',
    overall: elapsedTimeVsLatencySlope(photonSamples),
    byInput: Object.fromEntries(
      inputKinds.map((inputKind) => [
        inputKind,
        elapsedTimeVsLatencySlope(photonSamples.filter((sample) => sample.inputKind === inputKind))
      ])
    )
  };
}

async function measureTargetObservation(label, action, observe) {
  const started = performance.now();
  await action();
  await observe();
  const targetObservationLatencyMs = roundMs(performance.now() - started);
  if (targetObservationLatencyMs > inputBudgetMs) {
    throw new Error(
      `${label} target observation took ${targetObservationLatencyMs}ms, exceeding ${inputBudgetMs}ms input budget`
    );
  }
  return { targetObservation: label, targetObservationLatencyMs };
}

function measureDocumentInput(ctx, label, body, fragment) {
  return measureTargetObservation(
    label,
    () => send(ctx, body),
    () => assertDocumentIncludes(fragment, inputBudgetMs)
  );
}

async function decodedPhotonFrame(ctx) {
  return evaluate(
    ctx.cdp,
    harnessExpression(ctx.target, 'return api.photonFrame({ target });')
  );
}

async function measurePhotonInput(ctx, inputKind) {
  const input = inputKind === 'text'
    ? `{ target, kind: 'text', text: 'x', timeoutMs: ${photonSampleTimeoutMs} }`
    : `{ target, kind: 'click', x: 0.75, y: 0.58, timeoutMs: ${photonSampleTimeoutMs} }`;
  return evaluate(
    ctx.cdp,
    harnessExpression(ctx.target, `return api.pressToPhoton(${input});`)
  );
}

async function photonBrowserPrerequisiteFailure(cdp) {
  const capabilities = await evaluate(
    cdp,
    `(() => {
      const video = document.createElement('video');
      const canvas = document.createElement('canvas');
      return {
        requestVideoFrameCallback: typeof video.requestVideoFrameCallback === 'function',
        canvas2d: Boolean(canvas.getContext('2d'))
      };
    })()`
  );
  const missing = Object.entries(capabilities ?? {})
    .filter(([, available]) => !available)
    .map(([name]) => name);
  return missing.length > 0 ? `browser prerequisites unavailable: ${missing.join(', ')}` : null;
}

function photonInfrastructureReport(reason) {
  const result = {
    caseId: 'press-to-photon',
    name: 'web control input -> native reaction -> decoded browser display',
    features: 'remote-control/press-to-photon',
    sequence: 'web publish -> native sentinel -> SCK/H264/LiveKit -> requestVideoFrameCallback',
    status: 'skip',
    detail: `INFRA/SKIP: ${reason}`,
    caseDurationMs: 0,
    targetObservation: null,
    targetObservationLatencyMs: null,
    inputOrderSeed: photonShuffleSeed
  };
  return {
    summary: {
      mode: 'press-to-photon',
      total: 1,
      pass: 0,
      fail: 0,
      skip: 1,
      inputOrderSeed: photonShuffleSeed,
      infrastructure: { status: 'unsupported', reason }
    },
    results: [result],
    photonSamples: []
  };
}

function writePhotonReport(report) {
  if (jsonOutputPath) {
    fs.writeFileSync(jsonOutputPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8');
  }
}

async function runPressToPhotonSuite(ctx) {
  const suiteStarted = performance.now();
  ctx.caseStartedAt = Date.now();
  await send(ctx, 'api.resetMetrics(); api.request(target); return true;');
  await waitForActiveStatus(ctx, ctx.caseStartedAt);
  const initialFrame = await waitUntil(
    'decodable photon sentinel frame',
    () => decodedPhotonFrame(ctx),
    shareReadyTimeoutMs,
    100
  );
  console.log(
    `# photon-sentinel-ready generation=${initialFrame.generation} decoded=${initialFrame.width}x${initialFrame.height} confidence=${initialFrame.confidence.toFixed(3)}`
  );

  const sampleFailures = [];
  const blockedInputKinds = new Set();
  const warmupSchedule = shuffledPhotonInputSchedule(photonWarmupSamplesPerInput, photonShuffleSeed);
  const measuredSchedule = shuffledPhotonInputSchedule(photonSamplesPerInput, photonShuffleSeed);
  console.log(
    `# photon-input-order seed=${photonShuffleSeed} warmup=${warmupSchedule.map(({ inputKind, sample }) => `${inputKind}:${sample}`).join(',') || 'none'} measured=${measuredSchedule.map(({ inputKind, sample }) => `${inputKind}:${sample}`).join(',')}`
  );
  for (const { inputKind, sample } of warmupSchedule) {
    if (blockedInputKinds.has(inputKind)) continue;
    try {
      await measurePhotonInput(ctx, inputKind);
    } catch (error) {
      sampleFailures.push({ sample, inputKind, warmup: true, error: error.message });
      blockedInputKinds.add(inputKind);
    }
  }

  const photonSamples = [];
  for (const { inputKind, sample: sampleNumber } of measuredSchedule) {
    if (blockedInputKinds.has(inputKind)) continue;
    try {
      const measured = await measurePhotonInput(ctx, inputKind);
      const sample = {
        sample: sampleNumber,
        inputKind,
        ...measured,
        pressToFrameCallbackMs: roundMs(measured.pressToFrameCallbackMs),
        pressToEstimatedPhotonMs: roundMs(measured.pressToEstimatedPhotonMs),
        publishCompleteMs: roundMs(measured.publishCompleteMs)
      };
      photonSamples.push(sample);
      console.log(`PHOTON_SAMPLE ${JSON.stringify(sample)}`);
    } catch (error) {
      const failure = { sample: sampleNumber, inputKind, error: error.message };
      sampleFailures.push(failure);
      console.log(`PHOTON_SAMPLE ${JSON.stringify({ ...failure, status: 'fail' })}`);
    }
  }

  const expectedSamples = photonSamplesPerInput * 2;
  const stats = summarizePhotonSamples(photonSamples, photonP95BudgetMs);
  const elapsedTimeVsLatency = summarizeElapsedTimeVsLatency(photonSamples);
  const gatePass = sampleFailures.length === 0 && photonSamples.length === expectedSamples && stats.pass;
  const gate = {
    caseId: 'press-to-photon',
    name: 'web control input -> native reaction -> decoded browser display',
    features: 'remote-control/press-to-photon',
    sequence: 'web publish -> native sentinel -> SCK/H264/LiveKit -> requestVideoFrameCallback',
    status: gatePass ? 'pass' : 'fail',
    detail: gatePass
      ? `p95 ${stats.overall.p95Ms}ms <= ${photonP95BudgetMs}ms across ${photonSamples.length} samples`
      : `p95 gate or sample completeness failed: measured=${photonSamples.length}/${expectedSamples} failures=${sampleFailures.length} p95=${stats.overall.p95Ms}ms budget=${photonP95BudgetMs}ms`,
    caseDurationMs: roundMs(performance.now() - suiteStarted),
    targetObservation: stats.metric,
    targetObservationLatencyMs: stats.overall.p95Ms,
    inputOrderSeed: photonShuffleSeed
  };
  const summary = {
    mode: 'press-to-photon',
    total: 1,
    pass: gatePass ? 1 : 0,
    fail: gatePass ? 0 : 1,
    skip: 0,
    inputOrderSeed: photonShuffleSeed,
    pressToPhoton: {
      ...stats,
      expectedSamples,
      warmupSamplesPerInput: photonWarmupSamplesPerInput,
      sampleTimeoutMs: photonSampleTimeoutMs,
      sampleFailures,
      timingSamples: photonSamples.map(({ sample, inputKind, mediaTime, pressToEstimatedPhotonMs }) => ({
        sample,
        inputKind,
        mediaTime: mediaTime ?? null,
        pressToEstimatedPhotonMs
      })),
      elapsedTimeVsLatency
    }
  };
  console.log(`RESULT ${JSON.stringify(gate)}`);
  console.log(`SUMMARY ${JSON.stringify(summary)}`);
  return { summary, results: [gate], photonSamples };
}

// ---------------------------------------------------------------------------
// #618 rapid-click queueing test.
//
// The press-to-photon suite above issues ONE input and awaits its photon before
// the next, so it structurally cannot expose a serialised replay shard backing
// up. This mode fires a burst of clicks on a fixed schedule WITHOUT awaiting
// each one, and measures how far each event falls behind its own send schedule.
//
// Primary metric is host-side landing (the sentinel button action's own tMs),
// not press-to-photon: photon is capped at the capture frame rate and cannot
// resolve individual events above ~30 clicks/second.
// ---------------------------------------------------------------------------

const BURST_GENERATION_MASK = 0xffff;

// The host already logs everything the queueing question needs, per event:
//   host enqueue_ts_ms=  -- admitted onto the shard
//   host inject_ts_ms=   -- the shard actually started running it
//   host replay complete_ts_ms= ... elapsed_ms=  -- shard occupancy
//   ax probes ... ax_ipc= cache_hit= cache_miss=
// inject - enqueue is queue wait: the direct measure of a backing-up shard,
// with no clock alignment and no frame-rate ceiling. It is the primary signal
// wherever it is available.
const petalLogPath = process.env.PETAL_LOG_PATH
  || path.join(os.homedir(), 'Library', 'Logs', 'Petal', 'petal.log');

function petalLogSize() {
  try { return fs.statSync(petalLogPath).size; } catch { return 0; }
}

function readPetalLogSince(offset) {
  try {
    const size = petalLogSize();
    if (size <= offset) return '';
    const handle = fs.openSync(petalLogPath, 'r');
    try {
      const buffer = Buffer.alloc(size - offset);
      fs.readSync(handle, buffer, 0, buffer.length, offset);
      return buffer.toString('utf8');
    } finally {
      fs.closeSync(handle);
    }
  } catch {
    return '';
  }
}

function parseHostClickLatency(text) {
  const bySeq = new Map();
  const entry = (seq) => {
    if (!bySeq.has(seq)) bySeq.set(seq, { seq });
    return bySeq.get(seq);
  };
  const isClick = (line) => line.includes('kind=Pointer') && line.includes('action=Some(Click)');
  for (const line of text.split('\n')) {
    if (!line.includes('remote-control-latency:') || !isClick(line)) continue;
    const seqMatch = /\bseq=(\d+)/.exec(line);
    if (!seqMatch) continue;
    const seq = Number(seqMatch[1]);
    let match;
    if ((match = /host enqueue_ts_ms=(\d+)/.exec(line))) entry(seq).enqueueTsMs = Number(match[1]);
    if ((match = /host inject_ts_ms=(\d+)/.exec(line))) entry(seq).injectTsMs = Number(match[1]);
    if ((match = /host replay complete_ts_ms=(\d+)/.exec(line))) {
      entry(seq).completeTsMs = Number(match[1]);
      const elapsed = /elapsed_ms=(\d+)/.exec(line);
      if (elapsed) entry(seq).elapsedMs = Number(elapsed[1]);
    }
    if ((match = /host replay timeout_ts_ms=(\d+)/.exec(line))) {
      entry(seq).timeoutTsMs = Number(match[1]);
      const elapsed = /elapsed_ms=(\d+)/.exec(line);
      if (elapsed) entry(seq).elapsedMs = Number(elapsed[1]);
    }
    if ((match = /ax_ipc=(\d+) cache_hit=(\d+) cache_miss=(\d+)/.exec(line))) {
      entry(seq).axIpc = Number(match[1]);
      entry(seq).cacheHit = Number(match[2]);
      entry(seq).cacheMiss = Number(match[3]);
    }
  }
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

async function runClickBurstInPage(ctx, { count, intervalMs, drainMs }) {
  return evaluate(
    ctx.cdp,
    `(async () => {
      const api = window.__petalHarness?.remoteControl;
      if (!api) throw new Error('window.__petalHarness.remoteControl is unavailable');
      const target = ${JSON.stringify(ctx.target)};
      const listed = api.targets().find((candidate) => candidate.windowId === target.windowId);
      const tile = listed?.tileId ? document.getElementById(listed.tileId) : null;
      const video = tile?.querySelector('video') ?? null;
      if (!video?.requestVideoFrameCallback) {
        throw new Error('burst target has no video element with requestVideoFrameCallback');
      }
      const baseline = api.photonFrame({ target });
      if (!baseline) throw new Error('burst baseline sentinel frame is not decodable');

      const frames = [];
      let stop = false;
      let callbackId = null;
      const onFrame = (now, metadata) => {
        callbackId = null;
        const frame = api.photonFrame({ target });
        if (frame) {
          frames.push({
            generation: frame.generation,
            confidence: Math.round(frame.confidence * 1000) / 1000,
            callbackNowMs: now,
            expectedDisplayTimeMs: metadata.expectedDisplayTime,
            mediaTime: metadata.mediaTime
          });
        }
        if (!stop) callbackId = video.requestVideoFrameCallback(onFrame);
      };
      callbackId = video.requestVideoFrameCallback(onFrame);

      const presses = [];
      const startedAt = performance.now();
      for (let index = 0; index < ${count}; index += 1) {
        const due = startedAt + index * ${intervalMs};
        const wait = due - performance.now();
        if (wait > 0) await new Promise((resolve) => setTimeout(resolve, wait));
        const sentAt = performance.now();
        const sentWallMs = Date.now();
        api.click({ target, x: 0.75, y: 0.58, button: 0 });
        presses.push({ index, sentAt, sentWallMs });
      }
      const lastSentWallMs = Date.now();
      await new Promise((resolve) => setTimeout(resolve, ${drainMs}));
      stop = true;
      if (callbackId !== null) video.cancelVideoFrameCallback?.(callbackId);

      const results = api.metrics().results ?? [];
      const outcomes = {};
      for (const entry of results) {
        outcomes[entry.outcome] = (outcomes[entry.outcome] ?? 0) + 1;
      }
      return {
        baselineGeneration: baseline.generation,
        firstSentWallMs: presses[0]?.sentWallMs ?? null,
        lastSentWallMs,
        presses,
        frames,
        outcomes,
        resultCount: results.length
      };
    })()`
  );
}

function leastSquaresSlope(values) {
  if (values.length < 2) return null;
  const meanIndex = (values.length - 1) / 2;
  const meanValue = values.reduce((sum, value) => sum + value, 0) / values.length;
  let covariance = 0;
  let variance = 0;
  values.forEach((value, index) => {
    const indexDelta = index - meanIndex;
    covariance += indexDelta * (value - meanValue);
    variance += indexDelta * indexDelta;
  });
  return variance === 0 ? null : roundMs(covariance / variance);
}

function unwrapGeneration(baseline, generation) {
  return baseline + (((generation - baseline) % (BURST_GENERATION_MASK + 1)) + BURST_GENERATION_MASK + 1) % (BURST_GENERATION_MASK + 1);
}

function analyzeBurst(cadenceHz, burst, landings, hostEvents) {
  const presses = burst.presses;
  const matched = Math.min(presses.length, landings.length);
  const events = [];
  const unwrappedFrames = burst.frames.map((frame) => ({
    ...frame,
    cumulativeGeneration: unwrapGeneration(burst.baselineGeneration, frame.generation)
  }));

  for (let index = 0; index < matched; index += 1) {
    const scheduleDelta = presses[index].sentWallMs - presses[0].sentWallMs;
    const landedDelta = landings[index].tMs - landings[0].tMs;
    const wantedGeneration = burst.baselineGeneration + index + 1;
    const frame = unwrappedFrames.find((candidate) => candidate.cumulativeGeneration >= wantedGeneration);
    const host = hostEvents[index] ?? {};
    events.push({
      index,
      sentWallMs: presses[index].sentWallMs,
      landedWallMs: landings[index].tMs,
      accumulatedLagMs: roundMs(landedDelta - scheduleDelta),
      hostSeq: host.seq ?? null,
      queueWaitMs: Number.isFinite(host.injectTsMs) && Number.isFinite(host.enqueueTsMs)
        ? host.injectTsMs - host.enqueueTsMs
        : null,
      shardOccupancyMs: host.elapsedMs ?? null,
      axIpc: host.axIpc ?? null,
      cacheHit: host.cacheHit ?? null,
      cacheMiss: host.cacheMiss ?? null,
      photonMs: frame ? roundMs(frame.expectedDisplayTimeMs - presses[index].sentAt) : null,
      // `resolved` is false when the capture frame rate could not separate this
      // event from its neighbours -- the photon number is then an upper bound
      // shared with the events folded into the same frame, not a per-event
      // measurement.
      photonResolved: frame ? frame.cumulativeGeneration === wantedGeneration : false
    });
  }

  const lags = events.map((event) => event.accumulatedLagMs);
  const sentDeltas = presses.slice(1).map((press, index) => press.sentWallMs - presses[index].sentWallMs);
  sentDeltas.sort((a, b) => a - b);
  const resolvedPhotons = events.filter((event) => event.photonResolved && Number.isFinite(event.photonMs));
  const photonValues = resolvedPhotons.map((event) => event.photonMs);
  const queueWaits = events.map((event) => event.queueWaitMs).filter((value) => Number.isFinite(value));
  const occupancies = events.map((event) => event.shardOccupancyMs).filter((value) => Number.isFinite(value));
  const cacheHits = events.filter((event) => event.cacheHit > 0).length;
  const cacheSamples = events.filter((event) => Number.isFinite(event.cacheHit)).length;

  return {
    cadenceHz,
    requestedIntervalMs: roundMs(1000 / cadenceHz),
    medianSendIntervalMs: sentDeltas.length ? roundMs(sentDeltas[Math.floor(sentDeltas.length / 2)]) : null,
    clicksSent: presses.length,
    clicksLanded: landings.length,
    matched,
    outcomes: burst.outcomes,
    accumulatedLagMs: {
      first: lags[0] ?? null,
      final: lags.at(-1) ?? null,
      max: lags.length ? roundMs(Math.max(...lags)) : null,
      slopeMsPerClick: leastSquaresSlope(lags)
    },
    hostQueue: {
      events: queueWaits.length,
      firstMs: queueWaits[0] ?? null,
      finalMs: queueWaits.at(-1) ?? null,
      maxMs: queueWaits.length ? Math.max(...queueWaits) : null,
      slopeMsPerClick: leastSquaresSlope(queueWaits)
    },
    shardOccupancyMs: {
      events: occupancies.length,
      medianMs: occupancies.length
        ? [...occupancies].sort((a, b) => a - b)[Math.floor(occupancies.length / 2)]
        : null,
      maxMs: occupancies.length ? Math.max(...occupancies) : null
    },
    axCache: {
      samples: cacheSamples,
      hits: cacheHits,
      hitRate: cacheSamples ? Math.round((cacheHits / cacheSamples) * 1000) / 1000 : null
    },
    photon: {
      resolvedEvents: resolvedPhotons.length,
      framesObserved: burst.frames.length,
      firstMs: photonValues[0] ?? null,
      lastMs: photonValues.at(-1) ?? null,
      slopeMsPerResolvedEvent: leastSquaresSlope(photonValues)
    },
    events
  };
}

async function runRapidClickBurstSuite(ctx) {
  const cadences = String(process.env.PETAL_RC_BURST_CADENCES || '5,10,15,20,30')
    .split(',')
    .map((value) => Number(value.trim()))
    .filter((value) => Number.isFinite(value) && value > 0);
  const burstSeconds = Number(process.env.PETAL_RC_BURST_SECONDS || 4);
  const minClicks = Number(process.env.PETAL_RC_BURST_MIN_CLICKS || 30);
  const drainMs = Number(process.env.PETAL_RC_BURST_DRAIN_MS || 3000);
  const settleMs = Number(process.env.PETAL_RC_BURST_SETTLE_MS || 3000);
  const injectedDelayMs = Number(process.env.PETAL_RC_SENTINEL_CLICK_DELAY_MS || 0);

  ctx.caseStartedAt = Date.now();
  await send(ctx, 'api.resetMetrics(); api.request(target); return true;');
  await waitForActiveStatus(ctx, ctx.caseStartedAt);
  const initialFrame = await waitUntil(
    'decodable photon sentinel frame',
    () => decodedPhotonFrame(ctx),
    shareReadyTimeoutMs,
    100
  );
  console.log(
    `# burst-sentinel-ready generation=${initialFrame.generation} decoded=${initialFrame.width}x${initialFrame.height} injectedSentinelDelayMs=${injectedDelayMs}`
  );

  const bursts = [];
  for (const cadenceHz of cadences) {
    const intervalMs = 1000 / cadenceHz;
    const count = Math.max(minClicks, Math.round(burstSeconds * cadenceHz));
    // Drain must outlast a FULLY serialised burst, or the tail of a saturating
    // shard is still arriving when the ledger is read and saturation reads as
    // dropped events instead of as lag.
    const burstDrainMs = Math.max(drainMs, count * 200);
    await sleep(settleMs);
    await send(ctx, 'api.resetMetrics(); return true;');
    const beforeLandings = sentinelEvents().filter(
      (event) => event.kind === 'axAction' && event.action === 'press'
    ).length;
    const loadBefore = readLoadAverages();
    const logOffset = petalLogSize();
    const burst = await runClickBurstInPage(ctx, { count, intervalMs, drainMs: burstDrainMs });
    const loadAfter = readLoadAverages();
    const hostEvents = parseHostClickLatency(readPetalLogSince(logOffset));
    const landings = sentinelEvents()
      .filter((event) => event.kind === 'axAction' && event.action === 'press' && Number.isFinite(event.tMs))
      .slice(beforeLandings);
    const analysis = {
      ...analyzeBurst(cadenceHz, burst, landings, hostEvents),
      hostEventsParsed: hostEvents.length,
      injectedSentinelDelayMs: injectedDelayMs,
      loadBefore,
      loadAfter
    };
    bursts.push(analysis);
    const { events, ...headline } = analysis;
    console.log(`BURST ${JSON.stringify(headline)}`);
    for (const event of events) console.log(`BURST_EVENT ${JSON.stringify({ cadenceHz, ...event })}`);
  }

  const summary = {
    mode: 'rapid-click-burst',
    injectedSentinelDelayMs: injectedDelayMs,
    cadences: bursts.map((burst) => ({
      cadenceHz: burst.cadenceHz,
      clicksSent: burst.clicksSent,
      clicksLanded: burst.clicksLanded,
      finalLagMs: burst.accumulatedLagMs.final,
      maxLagMs: burst.accumulatedLagMs.max,
      lagSlopeMsPerClick: burst.accumulatedLagMs.slopeMsPerClick,
      queueWaitFinalMs: burst.hostQueue.finalMs,
      queueWaitMaxMs: burst.hostQueue.maxMs,
      queueWaitSlopeMsPerClick: burst.hostQueue.slopeMsPerClick,
      medianShardOccupancyMs: burst.shardOccupancyMs.medianMs,
      axCacheHitRate: burst.axCache.hitRate,
      loadAfter: burst.loadAfter
    }))
  };
  console.log(`SUMMARY ${JSON.stringify(summary)}`);
  return { summary, bursts };
}

function readLoadAverages() {
  const [one, five, fifteen] = os.loadavg();
  return { one: Math.round(one * 100) / 100, five: Math.round(five * 100) / 100, fifteen: Math.round(fifteen * 100) / 100 };
}

const CASES = [
  {
    id: 1,
    name: 'request->active status',
    features: 'auth/status',
    sequence: 'request',
    run: async (ctx) => {
      const active = await waitForActiveStatus(ctx, ctx.caseStartedAt);
      return pass(`active from ${active.senderIdentity ?? 'unknown'} seq=${active.seq}`);
    },
  },
  {
    id: 2,
    name: 'pointer move',
    features: 'pointer',
    sequence: 'move',
    run: async (ctx) => {
      await send(ctx, `api.pointer({ target, action: 'move', x: 0.25, y: 0.35, button: -1, buttons: 0 }); return true;`);
      const metric = await published(ctx, `m.kind === 'pointer' && m.action === 'move' && m.reliable === false`);
      return pass(`published unreliable move seq=${metric.seq}; OS cursor readback needs sentinel`);
    },
  },
  {
    id: 3,
    name: 'left click',
    features: 'pointer,text',
    sequence: 'left click -> text',
    run: async (ctx) => {
      const marker = ` L${Date.now()} `;
      const measurement = await measureDocumentInput(
        ctx,
        'click-plus-text marker visible in TextEdit',
        `api.click({ target, x: 0.18, y: 0.28, button: 0 }); api.text({ target, text: ${JSON.stringify(marker)} }); return true;`,
        marker
      );
      return pass('text landed after the click command; click effect itself still needs a sentinel', measurement);
    },
  },
  {
    id: 4,
    name: 'right click',
    features: 'pointer/buttons',
    sequence: 'right click',
    run: async (ctx) => {
      await send(ctx, `api.click({ target, ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteClick)}, button: ${REMOTE_CONTROL_BUTTONS.right} }); return true;`);
      const metric = await published(ctx, `m.kind === 'pointer' && m.action === 'click' && m.button === 2 && m.buttons === 0`);
      await send(ctx, `api.key({ target, key: 'Escape', code: 'Escape' }); return true;`);
      return pass(`right-click packet shape action=click button=2 buttons=0 seq=${metric.seq}; host-side effect needs sentinel`);
    },
  },
  {
    id: 5,
    target: 'sentinel',
    name: 'middle click',
    features: 'pointer/buttons',
    sequence: 'middle click',
    run: async (ctx) => {
      await send(ctx, `api.click({ target, ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteClick)}, button: ${REMOTE_CONTROL_BUTTONS.middle} }); return true;`);
      // #808: `api.click()` publishes ONE semantic `action: 'click'` packet
      // with `buttons: 0` (harnessApi.ts `harnessClick`); it has not published
      // a down/up pair for some time. This predicate still described the old
      // shape, so it could never match and the case died on a 7s publish-metric
      // timeout BEFORE reaching its real oracle -- the sentinel's own
      // `otherMouseDown`, which is the assertion that actually matters here.
      const metric = await published(ctx, `m.kind === 'pointer' && m.action === 'click' && m.button === 1`);
      const event = await waitForSentinelEvent((event) => event.type === 'otherMouseDown' && event.button === 2, 'middle mouse-down');
      return pass(`middle click button=${event.button} seq=${metric.seq}`);
    },
  },
  {
    id: 6,
    name: 'left drag',
    features: 'pointer/drag',
    sequence: 'left drag',
    run: async (ctx) => {
      setTextEditDocument('remote-control drag target line\n'.repeat(12));
      await send(ctx, `return api.drag({ target, from: ${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteDragFrom)}, to: ${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteDragTo)}, steps: ${REMOTE_CONTROL_DRAG_STEPS.suite}, button: ${REMOTE_CONTROL_BUTTONS.left} });`);
      const selected = await waitForTextEditSelection();
      if (selected === null) return skipCase('AXSelectedText unavailable; deterministic drag assertion needs sentinel app');
      if (!selected) throw new Error('AXSelectedText was available but empty after left drag');
      return pass(`AXSelectedText changed to ${JSON.stringify(selected)}`);
    },
  },
  {
    id: 7,
    name: 'right drag held button',
    features: 'pointer/drag/buttons',
    sequence: 'right drag',
    run: async (ctx) => {
      await send(ctx, `return api.drag({ target, from: ${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteClick)}, to: { x: 0.40, y: 0.30 }, steps: ${REMOTE_CONTROL_DRAG_STEPS.short}, button: ${REMOTE_CONTROL_BUTTONS.right} });`);
      const metric = await published(ctx, `m.kind === 'pointer' && m.action === 'move' && m.button === -1 && m.buttons === 2`);
      await send(ctx, `api.key({ target, key: 'Escape', code: 'Escape' }); return true;`);
      // #455 review finding: `published()` only reflects the controller's own
      // wire echo (it always publishes an Up regardless of whether native
      // injection actually succeeded) -- it would never have caught the bug
      // this case exists to catch (native AX/SkyLight exhaustion on the Up,
      // leaving a phantom held button). Assert the real host-side effect via
      // remote-control-status's pressedInputs snapshot instead, same pattern
      // case 25's TTL-release assertion already uses.
      const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
      if (snapshot.pressedInputs?.length) {
        throw new Error(`right-drag held input remained after release: ${JSON.stringify(snapshot)}`);
      }
      return pass(`right-drag held buttons=2 seq=${metric.seq}; host confirms no held input after release`);
    },
  },
  {
    id: 8,
    target: 'sentinel',
    name: 'middle drag',
    features: 'pointer/drag/buttons',
    sequence: 'middle drag',
    run: async (ctx) => {
      await send(ctx, `return api.drag({ target, from: ${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteClick)}, to: { x: 0.40, y: 0.30 }, steps: ${REMOTE_CONTROL_DRAG_STEPS.short}, button: ${REMOTE_CONTROL_BUTTONS.middle} });`);
      const metric = await published(ctx, `m.kind === 'pointer' && m.action === 'move' && m.button === -1 && m.buttons === 4`);
      const event = await waitForSentinelEvent((event) => event.type === 'otherMouseDragged' && event.button === 2, 'middle drag');
      return pass(`middle drag button=${event.button} seq=${metric.seq}`);
    },
  },
  {
    id: 9,
    name: 'text typing lands in doc',
    features: 'text',
    sequence: 'text',
    run: async (ctx) => {
      const marker = ` Typed-${Date.now()} `;
      const measurement = await measureDocumentInput(
        ctx,
        'typed marker visible in TextEdit',
        `api.text({ target, text: ${JSON.stringify(marker)} }); return true;`,
        marker
      );
      return pass('TextEdit document contains typed marker', measurement);
    },
  },
  {
    id: 10,
    name: 'shortcut Cmd+C -> clipboard == selection',
    features: 'keyboard/shortcut',
    sequence: 'Cmd+A Cmd+C',
    run: async (ctx) => {
      const text = `copy-${Date.now()}`;
      setTextEditDocument(text);
      writeClipboard('not-the-selection');
      const measurement = await measureTargetObservation(
        'clipboard equals selected TextEdit document',
        () => send(ctx, `api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdA)} }); api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdC)} }); return true;`),
        () => waitUntil('clipboard equals selection', () => readClipboard() === text, inputBudgetMs)
      );
      return pass('clipboard equals selected TextEdit document', measurement);
    },
  },
  {
    id: 11,
    name: 'paste Cmd+V -> doc gains clipboard',
    features: 'keyboard/shortcut',
    sequence: 'Cmd+V',
    run: async (ctx) => {
      const marker = `paste-${Date.now()}`;
      writeClipboard(marker);
      const measurement = await measureDocumentInput(
        ctx,
        'pasted marker visible in TextEdit',
        `api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdV)} }); return true;`,
        marker
      );
      return pass('TextEdit document gained clipboard marker', measurement);
    },
  },
  {
    id: 12,
    name: 'Cmd+A select-all',
    features: 'keyboard/shortcut,AX',
    sequence: 'Cmd+A',
    run: async (ctx) => {
      const text = `select all ${Date.now()}`;
      setTextEditDocument(text);
      await send(ctx, `api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdA)} }); return true;`);
      await sleep(caseSettleMs);
      const selected = maybeReadTextEditSelection();
      if (selected === null) return skipCase('AXSelectedText unavailable; Cmd+A selection cannot be observed deterministically');
      if (selected !== text) throw new Error(`Cmd+A selected ${JSON.stringify(selected)} instead of ${JSON.stringify(text)}`);
      return pass('AXSelectedText equals full document');
    },
  },
  {
    id: 13,
    name: 'modifier flag Cmd',
    features: 'keyboard/modifiers',
    sequence: 'Cmd+C',
    run: async (ctx) => {
      const text = `cmd-${Date.now()}`;
      setTextEditDocument(text);
      writeClipboard('not-cmd');
      const measurement = await measureTargetObservation(
        'Cmd+C result visible in clipboard',
        () => send(ctx, `api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdA)} }); api.key({ target, ...${JSON.stringify(REMOTE_CONTROL_SHORTCUTS.cmdC)} }); return true;`),
        () => waitUntil('Cmd+C clipboard', () => readClipboard() === text, inputBudgetMs)
      );
      const metric = await published(ctx, `m.kind === 'key' && m.code === 'KeyC' && m.modifiers?.meta === true`);
      return pass(`Cmd modifier observed through clipboard and metric seq=${metric.seq}`, measurement);
    },
  },
  {
    id: 14,
    name: 'modifier flag Shift',
    features: 'keyboard/modifiers',
    sequence: 'Shift+A',
    run: async (ctx) => {
      const measurement = await measureDocumentInput(
        ctx,
        'uppercase A visible in TextEdit',
        `api.key({ target, key: 'A', code: 'KeyA', modifiers: { shift: true } }); return true;`,
        'A'
      );
      const metric = await published(ctx, `m.kind === 'key' && m.code === 'KeyA' && m.modifiers?.shift === true`);
      return pass(`Shift modifier produced uppercase A and metric seq=${metric.seq}`, measurement);
    },
  },
  {
    id: 15,
    target: 'sentinel',
    name: 'modifier flag Ctrl',
    features: 'keyboard/modifiers',
    sequence: 'Ctrl+A',
    run: async (ctx) => {
      await send(ctx, `api.key({ target, key: 'a', code: 'KeyA', modifiers: { ctrl: true } }); return true;`);
      const metric = await published(ctx, `m.kind === 'key' && m.code === 'KeyA' && m.modifiers?.ctrl === true`);
      const event = await waitForSentinelEvent((event) => event.type === 'keyDown' && sentinelModifier(event, 262144), 'Ctrl key-down');
      return pass(`Ctrl modifierFlags=${event.modifierFlags} seq=${metric.seq}`);
    },
  },
  {
    id: 16,
    target: 'sentinel',
    name: 'modifier flag Alt',
    features: 'keyboard/modifiers',
    sequence: 'Alt+A',
    run: async (ctx) => {
      await send(ctx, `api.key({ target, key: 'a', code: 'KeyA', modifiers: { alt: true } }); return true;`);
      const metric = await published(ctx, `m.kind === 'key' && m.code === 'KeyA' && m.modifiers?.alt === true`);
      const event = await waitForSentinelEvent((event) => event.type === 'keyDown' && sentinelModifier(event, 524288), 'Alt key-down');
      return pass(`Alt modifierFlags=${event.modifierFlags} seq=${metric.seq}`);
    },
  },
  {
    id: 17,
    name: 'unmapped key produces no stray char',
    features: 'keyboard/keymap',
    sequence: 'AudioVolumeUp',
    run: async (ctx) => {
      const before = readTextEditDocument();
      await send(ctx, `api.key({ target, key: 'AudioVolumeUp', code: 'AudioVolumeUp' }); return true;`);
      await sleep(caseSettleMs);
      const after = readTextEditDocument();
      if (after !== before) throw new Error(`unmapped key changed document: before=${JSON.stringify(before)} after=${JSON.stringify(after)}`);
      return pass('TextEdit document unchanged after unmapped key');
    },
  },
  {
    id: 18,
    name: 'keycode map Enter/Arrow/F5',
    features: 'keyboard/keymap',
    sequence: 'Enter ArrowRight document check; F5 metric check',
    run: async (ctx) => {
      await send(ctx, `api.key({ target, key: 'Enter', code: 'Enter' }); api.key({ target, key: 'ArrowRight', code: 'ArrowRight' }); return true;`);
      await waitUntil('Enter inserted newline', () => readTextEditDocument().includes('\n'), acquisitionTimeoutMs);
      const text = readTextEditDocument();
      if (text.includes('ArrowRight')) throw new Error(`ArrowRight leaked into document: ${JSON.stringify(text)}`);
      await send(ctx, `api.key({ target, key: 'F5', code: 'F5' }); return true;`);
      const metric = await published(ctx, `m.kind === 'key' && m.code === 'F5'`);
      await send(ctx, `api.key({ target, key: 'Escape', code: 'Escape' }); return true;`);
      return pass(`Enter inserted newline; ArrowRight produced no stray text; F5 metric seq=${metric.seq}`);
    },
  },
  {
    id: 19,
    name: 'vertical scroll pixel',
    features: 'scroll',
    sequence: 'wheel deltaMode=0 deltaY',
    run: async (ctx) => {
      setTextEditDocument(Array.from({ length: 100 }, (_, index) => `pixel scroll line ${index + 1}`).join('\n'));
      const before = maybeReadTextEditScrollValue();
      await send(ctx, `api.wheel({ target, ...${JSON.stringify(REMOTE_CONTROL_SCROLL_DELTAS.pixel)} }); return true;`);
      await sleep(caseSettleMs + 200);
      const after = maybeReadTextEditScrollValue();
      if (before === null || after === null) return skipCase('AX visible character range unavailable; pixel scroll assertion needs AX or sentinel app');
      if (after === before) throw new Error(`pixel scroll did not change AX visible character range (${before})`);
      return pass(`AX visible character range changed ${before} -> ${after}`);
    },
  },
  {
    id: 20,
    name: 'vertical scroll line',
    features: 'scroll',
    sequence: 'wheel deltaMode=1 deltaY',
    run: async (ctx) => {
      setTextEditDocument(Array.from({ length: 100 }, (_, index) => `line scroll line ${index + 1}`).join('\n'));
      const before = maybeReadTextEditScrollValue();
      await send(ctx, `api.wheel({ target, ...${JSON.stringify(REMOTE_CONTROL_SCROLL_DELTAS.line)} }); return true;`);
      await sleep(caseSettleMs + 200);
      const after = maybeReadTextEditScrollValue();
      if (before === null || after === null) return skipCase('AX visible character range unavailable; line scroll assertion needs AX or sentinel app');
      if (after === before) throw new Error(`line scroll did not change AX visible character range (${before})`);
      return pass(`AX visible character range changed ${before} -> ${after}`);
    },
  },
  {
    id: 21,
    target: 'sentinel',
    name: 'horizontal scroll sign',
    features: 'scroll/horizontal',
    sequence: 'wheel deltaX',
    run: async (ctx) => {
      // #811: the oracle is the sentinel's scroll POSITION, not a scrollWheel
      // NSEvent. Wheel input replays via AXValue on the target's
      // AXHorizontalScrollBar, which scrolls the view without synthesizing any
      // NSEvent -- and SkyLight/CGEvent wheel posting is measured-ineffective
      // (docs/TESTING.md "#446"), so an NSEvent predicate here was
      // structurally unsatisfiable and failed this case on every run.
      await send(ctx, `api.wheel({ target, ...${JSON.stringify(REMOTE_CONTROL_SCROLL_DELTAS.horizontalSentinel)} }); return true;`);
      const metric = await published(ctx, `m.kind === 'wheel' && m.deltaX === 240 && m.deltaY === 0`);
      const event = await waitForSentinelEvent(
        (event) => event.type === 'hscroll' && Number(event.deltaX) > 0,
        'horizontal scroll position'
      );
      return pass(`horizontal scroll strip moved to originX=${event.originX} (delta ${event.deltaX}) seq=${metric.seq}`);
    },
  },
  {
    id: 22,
    name: 'coordinate clamp',
    features: 'pointer/coordinates',
    sequence: 'out-of-range click -> text',
    run: async (ctx) => {
      const marker = ` Clamp-${Date.now()} `;
      const measurement = await measureDocumentInput(
        ctx,
        'post-clamp text marker visible in TextEdit',
        `api.click({ target, x: -5, y: 5, button: 0 }); api.text({ target, text: ${JSON.stringify(marker)} }); return true;`,
        marker
      );
      // #808: same stale shape as case 5 -- `api.click()` publishes
      // `action: 'click'`, never `down`. The clamp assertion itself is the
      // point and is unchanged: x=-5 -> 0 and y=5 -> 1 (`normalizedHarnessPoint`).
      const metric = await published(ctx, `m.kind === 'pointer' && m.action === 'click' && m.x === 0 && m.y === 1`);
      return pass(
        `packet coordinates clamped to x=${metric.x} y=${metric.y}; later text landed but does not prove click placement`,
        measurement
      );
    },
  },
  {
    id: 23,
    target: 'sentinel',
    name: 'Retina/secondary-display mapping',
    features: 'coordinates/displays',
    sequence: 'secondary display coordinate replay',
    run: async (ctx) => {
      if (!displayplacerHasSecondaryDisplay()) return skipCase('displayplacer reports fewer than two displays');
      await send(ctx, `api.click({ target, x: 0.75, y: 0.5, button: 0 }); return true;`);
      const event = await waitForSentinelEvent((event) => event.type === 'leftMouseDown', 'secondary-display sentinel click');
      return pass(`secondary-display click observed at button=${event.button}`);
    },
  },
  {
    id: 24,
    name: 'release drops later input',
    features: 'auth/release',
    sequence: 'release -> text',
    run: async (ctx) => {
      const before = readTextEditDocument();
      await send(ctx, `api.release(target); api.text({ target, text: ' SHOULD_NOT_APPEAR' }); return true;`);
      await sleep(caseSettleMs + 200);
      const after = readTextEditDocument();
      if (after !== before) throw new Error(`post-release text changed document: ${JSON.stringify(after)}`);
      return pass('post-release text was absent from TextEdit document');
    },
  },
  {
    id: 25,
    target: 'sentinel',
    name: 'held-input TTL synthetic release',
    features: 'lifecycle/held-input',
    sequence: 'down -> TTL',
    run: async (ctx) => {
      await send(ctx, `api.pointer({ target, action: 'down', ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteHeldInput)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`);
      await waitUntil('held input tracked', async () => {
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
        return snapshot.pressedInputs?.length ? snapshot : null;
      }, 2000, 50);
      const event = await waitForSentinelEvent((event) => event.type === 'leftMouseUp', 'TTL synthetic mouse-up', 4000);
      const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
      if (snapshot.pressedInputs?.length) throw new Error(`held input remained after TTL: ${JSON.stringify(snapshot)}`);
      return pass(`TTL released button=${event.button}`);
    },
  },
  {
    id: 26,
    target: 'sentinel',
    name: 'controller-disconnect synthetic release',
    features: 'lifecycle/disconnect',
    sequence: 'down -> controller disconnect',
    run: async (ctx) => {
      await send(ctx, `api.pointer({ target, action: 'down', ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteHeldInput)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`);
      await waitUntil('held input tracked before disconnect', async () => {
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
        return snapshot.pressedInputs?.length ? snapshot : null;
      }, 2000, 50);
      await evaluate(ctx.cdp, 'window.__petalHarness?.room?.disconnect();');
      const event = await waitForSentinelEvent((event) => event.type === 'leftMouseUp', 'disconnect synthetic mouse-up', 5000);
      await ctx.rejoinWebHarness();
      return pass(`controller disconnect released button=${event.button}`);
    },
  },
  {
    id: 27,
    name: 'non-focus-stealing',
    features: 'focus',
    sequence: 'text replay',
    run: async (ctx) => {
      const before = readFrontmostProcess();
      await send(ctx, `api.text({ target, text: ' focus-check ' }); return true;`);
      await sleep(caseSettleMs);
      const after = readFrontmostProcess();
      if (after !== before) throw new Error(`frontmost app changed ${JSON.stringify(before)} -> ${JSON.stringify(after)}`);
      return pass(`frontmost app remained ${after}`);
    },
  },
  {
    id: 28,
    target: 'sentinel',
    name: 'disable->immediate revoke',
    features: 'auth/revoke',
    sequence: 'disable mid-stream -> text',
    run: async (ctx) => {
      await send(ctx, `api.pointer({ target, action: 'down', ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteHeldInput)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`);
      await waitUntil('held input tracked before disable', async () => {
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
        return snapshot.pressedInputs?.length ? snapshot : null;
      }, 2000, 50);
      await command(ctx.client, { cmd: 'remote-control-disable', window_id: ctx.target.windowId });
      const event = await waitForSentinelEvent((event) => event.type === 'leftMouseUp', 'disable synthetic mouse-up');
      const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
      if (snapshot.sessions?.length || snapshot.pressedInputs?.length) throw new Error(`disable left control state: ${JSON.stringify(snapshot)}`);
      return pass(`disable revoked control and released button=${event.button}`);
    },
  },
  {
    id: 29,
    target: 'sentinel',
    name: 'reconnect during control preserves grant',
    features: 'lifecycle/reconnect',
    sequence: 'request -> held pointer -> reconnect -> input -> release',
    run: async (ctx) => {
      await send(ctx, `api.pointer({ target, action: 'down', ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteHeldInput)}, button: ${REMOTE_CONTROL_BUTTONS.left}, buttons: 1 }); return true;`);
      const before = await waitUntil('held input tracked before reconnect', async () => {
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
        return snapshot.sessions?.length && snapshot.pressedInputs?.length ? snapshot : null;
      }, 2000, 50);
      await command(ctx.client, { cmd: 'reconnect', mode: process.env.PETAL_REMOTE_CONTROL_RECONNECT_MODE || 'resume' });
      const after = await waitUntil('grant survives reconnect', async () => {
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
        return snapshot.sessions?.length ? snapshot : null;
      }, 5000, 100);
      clearSentinelEventLog();
      // #820: the re-sent click MUST use `suiteHeldInput` (0.5, 0.5). At
      // (0.6, 0.5) the point is inside the sentinel's real AX-pressable
      // "REMOTE CLICK" button, so the AX route handles it and no
      // `leftMouseDown` is ever synthesized -- this assertion then fails
      // however well the click was delivered. Full analysis: issue #820.
      // A click published while the host is still re-establishing its data
      // channel after a resume is lost in transport, so re-send like a real
      // controller would, bounded by the same overall budget.
      const clickDeadline = Date.now() + acquisitionTimeoutMs;
      let sawPostReconnectClick = null;
      while (Date.now() < clickDeadline) {
        await send(ctx, `api.click({ target, ...${JSON.stringify(REMOTE_CONTROL_COORDINATES.suiteHeldInput)}, button: 0 }); return true;`);
        sawPostReconnectClick = await waitForSentinelEvent(
          (event) => event.type === 'leftMouseDown',
          'post-reconnect input',
          1500
        ).catch(() => null);
        if (sawPostReconnectClick) break;
      }
      if (!sawPostReconnectClick) throw new Error(`post-reconnect input never landed within ${acquisitionTimeoutMs}ms of re-sent clicks`);
      const final = await command(ctx.client, { cmd: 'remote-control-status', window_id: ctx.target.windowId });
      if (final.pressedInputs?.length) throw new Error(`orphaned press after reconnect input: ${JSON.stringify(final)}`);
      return pass(`grant survived ${process.env.PETAL_REMOTE_CONTROL_RECONNECT_MODE || 'resume'} reconnect; next input landed`, { targetObservation: after });
    },
  },
  {
    id: 30,
    target: 'sentinel',
    name: 'completed click duplicate returns cached terminal result',
    features: 'dedup/cached-terminal',
    sequence: 'click -> terminal result -> duplicate operation -> cached result',
    run: async (ctx) => {
      ctx.terminalRecovery = {
        duplicateReplayObserved: false,
        sideEffectCount: 0,
        terminalDeliveries: [],
      };
      // #820 investigation outcome: this case exercises the v2
      // cached-terminal-replay path, which requires the host to negotiate a
      // control session -- and a macOS host advertises LEGACY-ONLY by
      // contract (docs/CONTRACTS.md "the same build continues to advertise
      // legacy host behavior when sharing from Mac"; d7a5e267 reverted the
      // attempt to change that). Against such a host the terminal-delivery
      // wait can never resolve, so failing here reported a designed platform
      // posture as a bug on every run. Skip, mirroring case 23's own
      // capability gate.
      const v2 = await evaluate(ctx.cdp, harnessExpression(ctx.target, 'return api.grant(target);'));
      if (!v2?.controlSessionId) {
        return skipCase('host advertises legacy control (macOS by contract; docs/CONTRACTS.md) -- v2 cached-terminal replay unreachable');
      }
      try {
        await send(ctx, 'return api.click({ target, x: 0.75, y: 0.58, button: 0 });');
        const firstRecords = await waitForTerminalDeliveryCount(ctx, 1);
        await waitForSentinelEvent(
          (event) =>
            event.kind === 'axAction'
            && event.action === 'press'
            && event.element === 'Remote click sentinel',
          'initial sentinel accessibility press'
        );
        // The hook does not resolve merely because the cached result arrived:
        // it audits the operation through the original dedup expiry and rejects
        // if any conflicting or extra same-operation terminal is observed.
        await send(ctx, 'return api.replayLastCompletedClick();');
        await waitForTerminalDeliveryCount(ctx, 2);
        const terminalDeliveries = await collectTerminalDeliveries(ctx);
        const sideEffectCount = sentinelEvents().filter(
          (event) =>
            event.kind === 'axAction'
            && event.action === 'press'
            && event.element === 'Remote click sentinel'
        ).length;
        const duplicateReplayObserved = terminalDeliveries.length === 2
          && firstRecords.length === 1
          && sameTerminalDelivery(terminalDeliveries[0], terminalDeliveries[1])
          && terminalDeliveries[1].receivedAt >= terminalDeliveries[0].receivedAt
          && sideEffectCount === 1;
        ctx.terminalRecovery = {
          duplicateReplayObserved,
          sideEffectCount,
          terminalDeliveries,
        };
        if (!duplicateReplayObserved) {
          throw new Error('cached terminal replay did not produce two matching dispositions and one native side effect');
        }
        return pass('duplicate operation returned the cached terminal disposition without a second native side effect');
      } finally {
        if (!ctx.terminalRecovery.duplicateReplayObserved) {
          const terminalDeliveries = await collectTerminalDeliveries(ctx).catch(() => []);
          const sideEffectCount = sentinelEvents().filter(
            (event) =>
              event.kind === 'axAction'
              && event.action === 'press'
              && event.element === 'Remote click sentinel'
          ).length;
          ctx.terminalRecovery = {
            duplicateReplayObserved: false,
            sideEffectCount,
            terminalDeliveries: terminalDeliveries.slice(0, 3),
          };
        }
      }
    },
  },
  // ---- Consent flow (ask policy). The autotest join seeds `auto` so the
  // cases above keep auto-granting (runCase's preamble requests and waits for
  // `active`); these two flip the live policy to `ask` for their own
  // request/answer round trip and restore `auto` in `finally`. The preamble's
  // grant is released first -- a re-request from a controller that already
  // holds a grant is idempotent and would be answered `active` without a
  // prompt, which is the documented non-prompting path, not this case.
  {
    id: 31,
    name: 'consent: request -> awaitingConsent -> allow -> active',
    features: 'auth/status/consent',
    sequence: 'policy ask -> release -> request -> consent-answer approve',
    run: async (ctx) => {
      const windowId = ctx.target.windowId;
      const controllerId = await evaluate(ctx.cdp, harnessExpression(ctx.target, 'return api.identity ? api.identity() : null;')).catch(() => null);
      await command(ctx.client, { cmd: 'remote-control-policy', policy: 'ask' });
      try {
        await send(ctx, `api.release(target); return true;`);
        await waitUntil('grant released before consent request', async () => {
          const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: windowId });
          return snapshot.sessions.length === 0 ? true : null;
        }, statusTimeoutMs);
        const startedAt = Date.now();
        await send(ctx, `api.resetMetrics(); api.request(target); return true;`);
        const awaiting = await waitUntil(
          'awaitingConsent status',
          () => evaluate(ctx.cdp, metricExpression(ctx.target, `return metrics.statuses.find((m) => m.windowId === target.windowId && m.status === 'awaitingConsent' && m.receivedAt >= ${startedAt}) ?? null;`)),
          statusTimeoutMs
        );
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: windowId });
        const pending = snapshot.pending ?? [];
        if (pending.length !== 1) throw new Error(`expected exactly one parked request, host reports ${JSON.stringify(pending)}`);
        if (snapshot.sessions.length !== 0) throw new Error(`a parked request must not hold a grant: ${JSON.stringify(snapshot.sessions)}`);
        const grantBefore = await evaluate(ctx.cdp, harnessExpression(ctx.target, 'return api.grant(target);'));
        if (grantBefore?.granted) throw new Error('controller held a grant token while the request was still awaiting consent');
        const answer = await command(ctx.client, {
          cmd: 'remote-control-consent-answer',
          window_id: windowId,
          controller_id: pending[0].controllerId,
          approve: true,
        });
        if (!answer.answered) throw new Error(`consent-answer reported nothing pending: ${JSON.stringify(answer)}`);
        const active = await waitForActiveStatus(ctx, startedAt);
        return pass(`awaitingConsent seq=${awaiting.seq} -> allow -> active seq=${active.seq} (controller ${controllerId ?? pending[0].controllerId})`);
      } finally {
        await command(ctx.client, { cmd: 'remote-control-policy', policy: 'auto' }).catch(() => null);
      }
    },
  },
  {
    id: 32,
    name: 'consent: request -> deny -> denied, no grant, later input dropped',
    features: 'auth/status/consent',
    sequence: 'policy ask -> release -> request -> consent-answer deny -> input',
    run: async (ctx) => {
      const windowId = ctx.target.windowId;
      await command(ctx.client, { cmd: 'remote-control-policy', policy: 'ask' });
      try {
        await send(ctx, `api.release(target); return true;`);
        await waitUntil('grant released before consent request', async () => {
          const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: windowId });
          return snapshot.sessions.length === 0 ? true : null;
        }, statusTimeoutMs);
        const startedAt = Date.now();
        await send(ctx, `api.resetMetrics(); api.request(target); return true;`);
        await waitUntil(
          'awaitingConsent status',
          () => evaluate(ctx.cdp, metricExpression(ctx.target, `return metrics.statuses.find((m) => m.windowId === target.windowId && m.status === 'awaitingConsent' && m.receivedAt >= ${startedAt}) ?? null;`)),
          statusTimeoutMs
        );
        const snapshot = await command(ctx.client, { cmd: 'remote-control-status', window_id: windowId });
        const pending = snapshot.pending ?? [];
        if (pending.length !== 1) throw new Error(`expected exactly one parked request, host reports ${JSON.stringify(pending)}`);
        await command(ctx.client, {
          cmd: 'remote-control-consent-answer',
          window_id: windowId,
          controller_id: pending[0].controllerId,
          approve: false,
        });
        const denied = await waitUntil(
          'denied status',
          () => evaluate(ctx.cdp, metricExpression(ctx.target, `return metrics.statuses.find((m) => m.windowId === target.windowId && m.status === 'denied' && m.receivedAt >= ${startedAt}) ?? null;`)),
          statusTimeoutMs
        );
        const after = await command(ctx.client, { cmd: 'remote-control-status', window_id: windowId });
        if ((after.pending ?? []).length !== 0) throw new Error(`deny left a parked request: ${JSON.stringify(after.pending)}`);
        if (after.sessions.length !== 0) throw new Error(`deny must never mint a grant: ${JSON.stringify(after.sessions)}`);
        // A late input without a grant is dropped by the host (#580 gate).
        const marker = Date.now();
        await send(ctx, `api.pointer({ target, action: 'move', x: 0.5, y: 0.5, button: -1, buttons: 0 }); return true;`).catch(() => null);
        await sleep(500);
        const afterInput = await command(ctx.client, { cmd: 'remote-control-status', window_id: windowId });
        if (afterInput.sessions.length !== 0) throw new Error('input after deny produced a grant');
        return pass(`denied seq=${denied.seq} reason=${denied.reason ?? 'n/a'}; no grant, no pending, input after deny dropped (marker ${marker})`);
      } finally {
        await command(ctx.client, { cmd: 'remote-control-policy', policy: 'auto' }).catch(() => null);
      }
    },
  },
];

async function runCase(ctx, testCase) {
  const started = performance.now();
  const result = {
    caseId: testCase.id,
    name: testCase.name,
    features: testCase.features,
    sequence: testCase.sequence,
    status: 'fail',
    detail: '',
    caseDurationMs: 0,
    targetObservation: null,
    targetObservationLatencyMs: null,
  };
  try {
    ctx.target = testCase.target === 'sentinel' ? ctx.sentinelTarget : ctx.textTarget;
    if (testCase.target === 'sentinel') clearSentinelEventLog();
    if (testCase.target !== 'sentinel') {
      setTextEditDocument(`case ${testCase.id} ${testCase.name}\n`);
    }
    ctx.caseStartedAt = Date.now();
    await send(ctx, `api.resetMetrics(); api.request(target); return true;`);
    try {
      await waitForActiveStatus(ctx, ctx.caseStartedAt);
    } catch (error) {
      // #808 (case-30 residue): a case that runs right after case 29's
      // host-side reconnect can have its freshly-granted control revoked by a
      // STALE ParticipantDisconnected aftershock of the host's own resume --
      // measured: Request granted at t, `'rc-harness' disconnected -- revoking`
      // at t+1.3s, controller left with no active status at all. Requests are
      // idempotent (#374: grants are shared), so one bounded re-request after
      // the aftershock window is honest test robustness, not a masked product
      // bug -- the underlying host race is filed separately.
      console.log(`case ${testCase.id}: first control acquire failed (${error.message}); re-requesting once`);
      await send(ctx, `api.request(target); return true;`);
      await waitForActiveStatus(ctx, ctx.caseStartedAt);
    }
    const outcome = await testCase.run(ctx);
    result.status = outcome.status;
    result.detail = outcome.detail;
    result.targetObservation = outcome.targetObservation ?? null;
    result.targetObservationLatencyMs = outcome.targetObservationLatencyMs ?? null;
  } catch (error) {
    result.status = 'fail';
    result.detail = error.message;
    result.forensics = await captureCaseFailureForensics(ctx.client, testCase.id);
  } finally {
    try {
      await send(ctx, `api.release(target); return true;`);
    } catch (error) {
      if (result.status !== 'fail') {
        result.status = 'fail';
        result.detail = `release failed: ${error.message}`;
      }
    }
    result.caseDurationMs = roundMs(performance.now() - started);
    console.log(`RESULT ${JSON.stringify(result)}`);
  }
  return result;
}

const client = connectSocket(socketPath);
let cdp;
const results = [];
let recoveries = 0;
try {
  const accessibility = await command(client, { cmd: 'accessibility_status' });
  if (!accessibility.trusted) {
    skip('Accessibility is not trusted for the Petal dev binary; grant it in System Settings > Privacy & Security > Accessibility');
  }

  const room = await waitForCommand(client, { cmd: 'current_room' }, 'native current_room');
  const expectedLiveKitRoom = `petal-room-${room.credential}`;
  if (room.livekitRoom !== expectedLiveKitRoom) {
    throw new Error(`native current_room mismatch: credential=${room.credential} livekitRoom=${room.livekitRoom} expected=${expectedLiveKitRoom}`);
  }
  // Since #104 (Google-Meet-style access codes), the web harness's join field
  // only resolves human-readable access codes (meetingCode.ts's
  // meetingCredentialFromInviteInput) -- it no longer accepts a raw internal
  // credential typed/pasted directly, even though looksLikeRoomCredentialInput
  // still flags one as "looks like a join attempt" for button-label purposes.
  // Passing room.credential here silently fails to join (or worse, creates a
  // stray room literally named after the credential string). The native
  // record's real access code is required instead.
  if (!room.accessCode) {
    throw new Error(`native current_room has no accessCode (credential=${room.credential}) -- cannot join web harness`);
  }

  const { wsUrl, pageUrl } = await cdpPageWebSocket();
  cdp = await connectCdp(wsUrl);
  const webRoom = await joinWebHarness(cdp, pageUrl, room.accessCode, room.livekitRoom);
  console.log(`# joined web harness credential=${room.credential} livekitRoom=${webRoom.name}`);

  if (acceptance446Mode) {
    const { shared, target } = await bootstrapPhotonSentinel(client, cdp);
    try {
      const report = await runAcceptance446Suite({ client, cdp, target, caseStartedAt: 0 });
      if (jsonOutputPath) fs.writeFileSync(jsonOutputPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8');
      // A failed positive control is NOT a test failure -- it is "no result".
      // Exit 2 so it can never be mistaken for either a pass or a real zero.
      // suiteExitCode applies the same rule to the control-passed branch: a
      // suite that executed nothing, or passed nothing, is also "no result".
      process.exitCode = report.controlPassed ? suiteExitCode(report.summary) : 2;
    } finally {
      try { await command(client, { cmd: 'stop_share', window_id: shared.windowId }); } catch {}
      stopPhotonSentinel();
    }
  } else if (rapidClickBurstMode) {
    const { shared, target } = await bootstrapPhotonSentinel(client, cdp);
    const ctx = { client, cdp, target, caseStartedAt: 0 };
    console.log(`# rapid-click-burst suite (#618) windowId=${shared.windowId}`);
    try {
      const report = await runRapidClickBurstSuite(ctx);
      if (jsonOutputPath) fs.writeFileSync(jsonOutputPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8');
      process.exitCode = report.bursts.length === 0 ? 1 : 0;
    } finally {
      try { await send({ client, cdp, target }, 'api.release(target); return true;'); } catch {}
      try { await command(client, { cmd: 'stop_share', window_id: shared.windowId }); } catch {}
      stopPhotonSentinel();
    }
  } else if (cockpitDriveMode) {
    const { shared, target } = await bootstrapPhotonSentinel(client, cdp);
    try {
      const report = await runCockpitDriveSuite({ client, cdp, target });
      // #470 follow-up: an empty `observed` ledger is a real Petal signal
      // (input was driven but never landed on the target -- exactly #446's
      // failure mode), not a harness malfunction. Exiting non-zero here made
      // the Rust caller (run_remote_control_scaled_scenario) short-circuit on
      // `!status.success()` BEFORE ever reading the report file, discarding
      // real drive/observed data into a generic "driver exited with 1"
      // INFRA-FAIL -- when verify_remote_control_drive (mod.rs) already knows
      // how to turn an empty ledger into a proper, informative TEST-FAIL.
      // Only a genuinely empty `driven` array (nothing was even attempted)
      // reflects an actual harness-level failure worth a non-zero exit here.
      process.exitCode = report.driven.length ? 0 : 1;
    } finally {
      try { await command(client, { cmd: 'stop_share', window_id: shared.windowId }); } catch {}
      stopPhotonSentinel();
    }
  } else if (pressToPhotonMode) {
    console.log(`# photon-input-order seed=${photonShuffleSeed}`);
    const prerequisiteFailure = await photonBrowserPrerequisiteFailure(cdp);
    if (prerequisiteFailure) {
      const report = photonInfrastructureReport(prerequisiteFailure);
      console.log(`RESULT ${JSON.stringify(report.results[0])}`);
      console.log(`SUMMARY ${JSON.stringify(report.summary)}`);
      writePhotonReport(report);
      // This branch reports status 'skip' -- it must not also exit 0.
      // suiteExitCode returns 2 here because the report's pass count is zero.
      process.exitCode = suiteExitCode(report.summary);
    } else {
      const { shared, target } = await bootstrapPhotonSentinel(client, cdp);
      const ctx = { client, cdp, target, caseStartedAt: 0 };
      console.log(
        '# press-to-photon suite: web control -> native AppKit sentinel -> ScreenCaptureKit/H264/LiveKit -> browser display callback'
      );
      console.log(
        `# samples-per-input=${photonSamplesPerInput} warmup-per-input=${photonWarmupSamplesPerInput} p95-budget-ms=${photonP95BudgetMs} input-order-seed=${photonShuffleSeed} windowId=${shared.windowId}`
      );
      try {
        const report = await runPressToPhotonSuite(ctx);
        writePhotonReport(report);
        process.exitCode = suiteExitCode(report.summary);
      } finally {
        try {
          await send(ctx, 'api.release(target); return true;');
        } catch {
          // Best-effort teardown; the sentinel process exit also revokes the target.
        }
        try {
          await command(client, { cmd: 'stop_share', window_id: shared.windowId });
        } catch {
          // Best-effort teardown only.
        }
        stopPhotonSentinel();
      }
    }
  } else {
    // Confirmed live, real cumulative degradation across repeated same-day
    // runs (not caused by any single run, and not a Petal bug): TextEdit
    // itself gets progressively less responsive to CGEventPostToPid replay +
    // AppleScript polling the more times it's reused across separate suite
    // invocations in one session -- symptoms escalate from "wedged around
    // case 18" on one run to "wedged around case 9" on the next. Forcing a
    // genuinely fresh TextEdit process at the start of every run (rather than
    // reusing whatever's already running, possibly for the Nth time today)
    // is cheap and removes this whole class of flakiness at the source.
    let { shared, target } = await bootstrapTextEditTarget(client, cdp);
    const textTarget = target;
    const sentinel = await bootstrapPhotonSentinel(client, cdp);

    console.log(
      '# remote-control numbered suite: web controller -> LiveKit data channel -> native host -> CGEventPostToPid -> TextEdit'
    );
    if (inputOnlyMode) {
      // Printed by the runner itself, so a relaxed run can never be quoted as
      // the full gate just because someone lost the wrapper's output.
      for (const line of INPUT_ONLY_SCOPE_LINES) console.log(`# ${line}`);
    }
    console.log(`# cases=${CASES.length} targetUserId=${targetUserId} windowId=${shared.windowId}`);

    const ctx = {
      client,
      cdp,
      target,
      textTarget,
      sentinelTarget: sentinel.target,
      caseStartedAt: 0,
      terminalRecovery: {
        duplicateReplayObserved: false,
        sideEffectCount: 0,
        terminalDeliveries: [],
      },
      rejoinWebHarness: async () => {
        const rejoined = await joinWebHarness(cdp, pageUrl, room.accessCode, room.livekitRoom);
        console.log(`# controller rejoined after disconnect room=${rejoined.name}`);
        return rejoined;
      },
    };
    const terminalDeliveries = [];
    for (const testCase of CASES) {
      textEditWedged = false;
      const result = await runCase(ctx, testCase);
      terminalDeliveries.push(...await collectTerminalDeliveries(ctx));
      if (textEditWedged && recoveries < maxTextEditRecoveries) {
        recoveries += 1;
        textEditWedged = false;
        console.log(`# RECOVERY TextEdit wedged during case ${testCase.id}; restarting target and retrying once (${recoveries}/${maxTextEditRecoveries})`);
        captureTextEditWedgeForensics();
        ({ shared, target } = await bootstrapTextEditTarget(client, cdp));
        ctx.textTarget = target;
        console.log(`# RECOVERY retrying case ${testCase.id} with windowId=${shared.windowId}`);
        results.push(await runCase(ctx, testCase));
        terminalDeliveries.push(...await collectTerminalDeliveries(ctx));
        continue;
      }
      if (textEditWedged) {
        console.log(`# WARN TextEdit wedged during case ${testCase.id}; recovery limit ${maxTextEditRecoveries} reached`);
      }
      results.push(result);
    }

    const measuredTargetLatencies = results
      .map((result) => result.targetObservationLatencyMs)
      .filter((value) => Number.isFinite(value))
      .sort((a, b) => a - b);
    const p95Index = Math.max(0, Math.ceil(measuredTargetLatencies.length * 0.95) - 1);
    // #580: a host-side tokenless drop means the packet never reached any
    // injection route. It is never expected in a healthy run -- once control
    // is released the host has no session at all and returns before the token
    // check, so scenario case 24's deliberate post-release input does NOT
    // produce this line. Any occurrence fails the run.
    const tokenlessDrops = tokenlessDropLines(null).length;
    // SUMMARY-KEYS-PINNED: the keys of this object literal are pinned to
    // SUITE_SUMMARY_KEYS by scripts/test-remote-control-exit-accounting.mjs,
    // which is what stops scripts/cross-machine-rc-suite.sh's reducer
    // allowlist drifting out from under a real run again (#580).
    const summary = {
      total: results.length,
      pass: results.filter((result) => result.status === 'pass').length,
      fail: results.filter((result) => result.status === 'fail').length,
      skip: results.filter((result) => result.status === 'skip').length,
      recoveries,
      tokenlessDrops,
      mode: inputOnlyMode ? 'input-only' : 'numbered',
      shareReadiness: shareReadinessMode(inputOnlyMode),
      targetObservationLatency: {
        budgetMs: inputBudgetMs,
        samples: measuredTargetLatencies.length,
        maxMs: measuredTargetLatencies.at(-1) ?? null,
        p95Ms: measuredTargetLatencies[p95Index] ?? null,
      },
    };
    console.log(`SUMMARY ${JSON.stringify(summary)}`);
    if (tokenlessDrops > 0) {
      console.log(
        `# FAIL host dropped ${tokenlessDrops} tokenless input packet(s) (#580); this run did not inject what it claims to have injected`
      );
    }
    if (jsonOutputPath) {
      fs.writeFileSync(
        jsonOutputPath,
        `${JSON.stringify({ summary, recoveries, results, terminalDeliveries, terminalRecovery: ctx.terminalRecovery }, null, 2)}\n`,
        'utf8'
      );
    }
    // The --input-only pass bar has teeth: a bar case that SKIPS produces no
    // failure count, so without this a relaxed run could exit 0 having proved
    // none of the sentinel-oracle claims.
    let passBarShortfall = 0;
    if (inputOnlyMode) {
      const verdict = inputOnlyPassBarVerdict(results);
      console.log(
        `# INPUT-ONLY PASS BAR required=[${verdict.required.join(',')}] passed=${verdict.passed}/${verdict.required.length}`
        + ` missing=[${verdict.missing.join(',')}] excludedFailures=[${verdict.excludedFailures.join(',')}]`
        + ` verdict=${verdict.met ? 'MET' : 'NOT MET'}`
      );
      for (const line of INPUT_ONLY_SCOPE_LINES) console.log(`# ${line}`);
      passBarShortfall = verdict.met ? 0 : 1;
    }
    // A tokenless drop is a real failure, but it can never upgrade a "no
    // result" run into a mere failure -- 2 stays 2.
    const numberedExitCode = suiteExitCode(summary);
    process.exitCode = numberedExitCode === 0 && (tokenlessDrops > 0 || passBarShortfall > 0) ? 1 : numberedExitCode;
    try { await command(client, { cmd: 'stop_share', window_id: sentinel.shared.windowId }); } catch {}
  }
} finally {
  closeTextEditSacrificialDocument();
  stopPhotonSentinel();
  cdp?.close();
  client.close();
}
