#!/usr/bin/env node

// Bounded local-only coordinator for #613's presentation-inclusive matrix.
// Static/unit review may run `--self-test`; the normal path owns every process
// it starts and writes timing/counter evidence only (never captured pixels).

import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const desktopDir = path.resolve(scriptDir, '..');
const tauriDir = path.join(desktopDir, 'src-tauri');
const repoRoot = path.resolve(desktopDir, '..', '..');
const webDir = path.join(repoRoot, 'web-harness');
const targetScript = path.join(scriptDir, 'latency-target-window.swift');
const observerScript = path.join(scriptDir, 'presentation-latency-observer.swift');
const publisher = path.join(tauriDir, 'target', 'debug', 'examples', 'publish_probe');
const compositor = path.join(tauriDir, 'target', 'debug', 'examples', 'compositor_probe');
const PRESENTATION_CROP = { width: 640, height: 360, margin: 40 };

function parseArgs(argv) {
  const options = {
    direction: 'both', load: 'both', samples: 120, warmup: 30,
    port: 17915, webPort: 5185, cdpPort: 9231,
    output: path.join('/private/tmp', `petal-613-presentation-${new Date().toISOString().replace(/[-:.]/g, '')}`),
    selfTest: false,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--self-test') options.selfTest = true;
    else if (['--direction', '--load', '--samples', '--warmup', '--port', '--web-port', '--cdp-port', '--output'].includes(arg)) {
      const value = argv[++i];
      if (value === undefined) throw new Error(`${arg} requires a value`);
      if (arg === '--direction') options.direction = value;
      else if (arg === '--load') options.load = value;
      else if (arg === '--output') options.output = value;
      else options[{ '--samples': 'samples', '--warmup': 'warmup', '--port': 'port', '--web-port': 'webPort', '--cdp-port': 'cdpPort' }[arg]] = Number(value);
    } else throw new Error(`unknown argument: ${arg}`);
  }
  if (!['n2w', 'w2n', 'both'].includes(options.direction)) throw new Error('direction must be n2w, w2n, or both');
  if (!['idle', 'cpu50', 'both'].includes(options.load)) throw new Error('load must be idle, cpu50, or both');
  for (const key of ['samples', 'port', 'webPort', 'cdpPort']) {
    if (!Number.isInteger(options[key]) || options[key] <= 0) throw new Error(`${key} must be a positive integer`);
  }
  if (!Number.isInteger(options.warmup) || options.warmup < 0) throw new Error('warmup must be a non-negative integer');
  return options;
}

function nearestRank(values, percentile) {
  const sorted = values.filter(Number.isFinite).slice().sort((a, b) => a - b);
  if (!sorted.length) throw new Error('cannot summarize empty samples');
  return sorted[Math.max(0, Math.ceil(sorted.length * percentile) - 1)];
}

function summarize(values) {
  const finite = values.filter(Number.isFinite);
  return {
    samples: finite.length,
    averageMs: finite.reduce((sum, value) => sum + value, 0) / finite.length,
    p50Ms: nearestRank(finite, 0.5),
    p95Ms: nearestRank(finite, 0.95),
  };
}

function validateCell(result, expectedSamples) {
  return result.samples >= expectedSamples
    && result.sourceFps >= 25 && result.sourceFps <= 35
    && result.destinationFps >= 25 && result.destinationFps <= 35
    && result.unpairedDestinationGenerations === 0
    && result.frameStatusErrors === 0
    && result.decodeFailuresAfterReady === 0
    && result.counterRegressions === 0;
}

function validateControl(control, baseline) {
  const delta = control.p50Ms - baseline.p50Ms;
  return { deltaMs: delta, pass: delta >= 150 && delta <= 250 };
}

function directionOwnsNativePublisher(direction) { return direction === 'n2w'; }

function selectedDirections(direction) {
  return direction === 'both' ? ['n2w', 'w2n'] : [direction];
}

function directionPlan(direction, loads) {
  const nativeCapture = direction === 'n2w';
  return {
    nativeCapture,
    nativePublisher: nativeCapture,
    capturePreflight: nativeCapture,
    control: true,
    baseline: true,
    cpu50: loads.includes('cpu50'),
  };
}

function childExitObserved(entry) {
  return entry.observedExit === true || entry.child.exitCode !== null || entry.child.signalCode !== null;
}

function shouldSignalLeaseProcessGroup(entry) {
  return !childExitObserved(entry);
}

function parseProcessTable(output) {
  return output.trim().split('\n').filter(Boolean).map((line) => {
    const match = line.match(/^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.*?)\s*$/);
    if (!match) throw new Error(`unparseable ps row: ${line}`);
    return { pid: Number(match[1]), ppid: Number(match[2]), pgid: Number(match[3]), comm: match[4], fullCommand: match[4] };
  });
}

function systemProcessSnapshot(targetPgid) {
  const result = spawnSync('/bin/ps', ['-axo', 'pid=,ppid=,pgid=,comm='], { encoding: 'utf8' });
  if (result.error || result.status !== 0) throw new Error(`ps snapshot failed: ${result.error?.message ?? result.stderr.trim()}`);
  return parseProcessTable(result.stdout).filter((member) => member.pgid === targetPgid).flatMap((member) => {
    const command = spawnSync('/bin/ps', ['-p', String(member.pid), '-o', 'command='], { encoding: 'utf8' });
    const state = spawnSync('/bin/ps', ['-p', String(member.pid), '-o', 'state='], { encoding: 'utf8' });
    // A member may exit between the group listing and its identity lookup; it
    // is absent for this snapshot rather than an unrelated-host failure.
    if (command.error || command.status !== 0 || !command.stdout.trim() || state.error || state.status !== 0) return [];
    return [{ ...member, fullCommand: command.stdout.trim(), state: state.stdout.trim() }];
  });
}

function exactMemberIdentityMatches(member, identity) {
  return member.pid === identity.pid && member.pgid === identity.pgid
    && member.comm === identity.comm && member.fullCommand === identity.fullCommand;
}

function memberIdentity(member) {
  return `${member.pid}:${member.pgid}:${member.comm}:${member.fullCommand}`;
}

function sleepSynchronously(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

function observeLeaderAfterSpawn(pid, pgid, processSnapshot, retries = 20, retryMs = 25) {
  for (let attempt = 0; attempt < retries; attempt += 1) {
    const leader = processSnapshot(pgid).find((member) => member.pid === pid && member.pgid === pgid);
    if (leader) return leader;
    sleepSynchronously(retryMs);
  }
  throw new Error(`leader ${pid}/${pgid} was not observed after spawn`);
}

function classifyOwnedGroup(entry, members) {
  const group = members.filter((member) => member.pgid === entry.pgid);
  if (!group.length) return { state: 'missing', group: [], recorded: [] };
  const leader = group.find((member) => member.pid === entry.pid);
  // macOS changes a reaped child's comm/command to <defunct>; PID+PGID plus
  // the still-pending Node ChildProcess is the identity proof at that point.
  if (leader && leader.pid === entry.pid && leader.pgid === entry.pgid
      && leader.state?.startsWith('Z') && !childExitObserved(entry)) return { state: 'exited-awaiting-reap', group, recorded: group };
  if (leader && entry.attestedLeader && exactMemberIdentityMatches(leader, entry.attestedLeader)) return { state: leader.state?.startsWith('Z') || leader.fullCommand.includes('<defunct>') ? 'exited-awaiting-reap' : 'verified', group, recorded: group };
  if (leader && !entry.attestedLeader) return { state: 'unattested', group, recorded: [] };
  const known = new Set((entry.knownMembers ?? []).map(memberIdentity));
  const recorded = group.filter((member) => known.has(memberIdentity(member)));
  if (childExitObserved(entry) && recorded.length === group.length) return { state: 'descendants', group, recorded };
  return { state: childExitObserved(entry) ? 'unexpected' : 'mismatch', group, recorded };
}

function cleanupOutcome(primaryError, cleanupErrors) {
  const errors = cleanupErrors.filter(Boolean);
  if (!primaryError) return errors.length <= 1 ? errors[0] : new AggregateError(errors, 'multiple cleanup failures');
  if (!errors.length) return primaryError;
  return new AggregateError([primaryError, ...errors], `primary failure: ${primaryError.message}; cleanup failure: ${errors.map((error) => error.message).join('; ')}`);
}

function reapDisposition(state, childExited) {
  if (state === 'missing') return 'clean';
  if (state === 'exited-awaiting-reap') return childExited ? 'clean' : 'await-reap';
  return 'identity-error';
}

async function runWithCleanup(body, cleanup) {
  let value; let primaryError;
  try {
    value = await body();
  } catch (error) {
    primaryError = error;
  }
  const cleanupErrors = [];
  try {
    await cleanup();
  } catch (error) {
    cleanupErrors.push(error);
  }
  const terminalError = cleanupOutcome(primaryError, cleanupErrors);
  if (terminalError) throw terminalError;
  return value;
}

function oneShotInspectorExitDisposition({ resultParsed, exitObserved, exitCode, signalCode, timedOut }) {
  if (!resultParsed) return 'await-result';
  if (timedOut) return 'timeout';
  if (!exitObserved) return 'await-exit';
  return exitCode === 0 && signalCode === null ? 'clean' : 'failed-exit';
}

function hasCapturePreflightReady(output) {
  return /^CAPTURE_PREFLIGHT_READY\b/m.test(output);
}

function capturePreflightArgs(windowId) {
  return [String(windowId), '--source', 'real', '--expected-capture-width', '960', '--expected-capture-height', '600', '--capture-preflight-only'];
}

function capturePreflightGate({ ready, exitCode, signalCode }) {
  return ready && exitCode === 0 && signalCode === null;
}

function canonicalRoomNameFromHarness(roomName) {
  if (typeof roomName !== 'string' || roomName.length === 0) throw new Error('web harness did not expose a connected canonical room name');
  return roomName;
}

function compositorArgs(roomName, tail) {
  return [roomName, ...tail];
}

function publisherArgs(windowId, roomName, tail) {
  return [String(windowId), roomName, ...tail];
}

function choosePresentationLayout(displays) {
  const needWidth = PRESENTATION_CROP.width * 2 + PRESENTATION_CROP.margin * 3;
  const needHeight = PRESENTATION_CROP.height + PRESENTATION_CROP.margin * 2;
  const display = displays.find((candidate) => candidate.pixelWidth >= needWidth && candidate.pixelHeight >= needHeight);
  if (!display) throw new Error(`no display fits two ${PRESENTATION_CROP.width}x${PRESENTATION_CROP.height} presentation crops plus margin`);
  return { display };
}

function initialPresentationSourceCrop(display) {
  const crop = { x: PRESENTATION_CROP.margin, y: PRESENTATION_CROP.margin, width: PRESENTATION_CROP.width, height: PRESENTATION_CROP.height };
  if (!cropFitsDisplay(crop, display)) throw new Error('selected display cannot contain initial presentation source crop');
  return crop;
}

function cropFitsDisplay(crop, display) {
  return Number.isInteger(crop?.x) && Number.isInteger(crop?.y) && Number.isInteger(crop?.width) && Number.isInteger(crop?.height)
    && crop.x >= 0 && crop.y >= 0 && crop.width > 0 && crop.height > 0
    && crop.x + crop.width <= display.pixelWidth && crop.y + crop.height <= display.pixelHeight;
}

function cropsOverlap(first, second) {
  return first.x < second.x + second.width && first.x + first.width > second.x
    && first.y < second.y + second.height && first.y + first.height > second.y;
}

function requirePresentationCrop(crop, display, label) {
  if (crop?.width !== PRESENTATION_CROP.width || crop?.height !== PRESENTATION_CROP.height || !cropFitsDisplay(crop, display)) {
    throw new Error(`${label} must be an in-display ${PRESENTATION_CROP.width}x${PRESENTATION_CROP.height} physical crop: observed=${JSON.stringify(crop)}`);
  }
  return crop;
}

function deriveDestinationCrop(sourceCrop, display) {
  requirePresentationCrop(sourceCrop, display, 'source presentation crop');
  const right = { x: sourceCrop.x + sourceCrop.width + PRESENTATION_CROP.margin, y: sourceCrop.y, width: PRESENTATION_CROP.width, height: PRESENTATION_CROP.height };
  if (cropFitsDisplay(right, display) && !cropsOverlap(sourceCrop, right)) return right;
  const left = { x: sourceCrop.x - PRESENTATION_CROP.margin - PRESENTATION_CROP.width, y: sourceCrop.y, width: PRESENTATION_CROP.width, height: PRESENTATION_CROP.height };
  if (cropFitsDisplay(left, display) && !cropsOverlap(sourceCrop, left)) return left;
  throw new Error(`no nonoverlapping horizontal destination crop fits beside source=${JSON.stringify(sourceCrop)} on display=${display.id}`);
}

function sameCrop(first, second) {
  return first.x === second.x && first.y === second.y && first.width === second.width && first.height === second.height;
}

function requireActualDestinationCrop(sourceCrop, destinationCrop, display, plannedCrop, label) {
  requirePresentationCrop(destinationCrop, display, `${label} destination crop`);
  if (!sameCrop(destinationCrop, plannedCrop)) throw new Error(`${label} destination crop drifted: observed=${JSON.stringify(destinationCrop)} planned=${JSON.stringify(plannedCrop)}`);
  if (cropsOverlap(sourceCrop, destinationCrop)) throw new Error(`${label} source/destination presentation crops overlap`);
  return destinationCrop;
}

function appKitFrameForCrop(display, crop) {
  return {
    x: display.appkitX + crop.x / display.scale,
    y: display.appkitY + (display.pixelHeight - crop.y - crop.height) / display.scale,
    width: crop.width / display.scale,
    height: crop.height / display.scale,
  };
}

function compositorWindowArgs(roomName, display, destinationCrop, delayMs) {
  const frame = appKitFrameForCrop(display, destinationCrop);
  return compositorArgs(roomName, ['--window-x', String(frame.x), '--window-y', String(frame.y), '--window-width', String(frame.width), '--window-height', String(frame.height), '--enqueue-delay-ms', String(delayMs), '--nonactivating']);
}

function listDisplayLayouts() {
  const result = spawnSync('swift', [targetScript, '--list-displays'], { cwd: repoRoot, encoding: 'utf8' });
  if (result.status !== 0) throw new Error(`display enumeration failed: ${result.stderr.trim()}`);
  const line = result.stdout.split('\n').find((value) => value.startsWith('DISPLAY_LAYOUT_JSON '));
  if (!line) throw new Error('display enumeration produced no layout marker');
  const displays = JSON.parse(line.slice('DISPLAY_LAYOUT_JSON '.length)).map((display) => ({ ...display, pixelWidth: Math.round(display.width * display.scale), pixelHeight: Math.round(display.height * display.scale) }));
  if (!displays.every((display) => Number.isInteger(display.id) && display.scale > 0 && display.pixelWidth > 0 && display.pixelHeight > 0)) throw new Error('display enumeration contains invalid bounds/scale');
  return displays;
}

function physicalBrowserRect(metrics, rect, display = null) {
  const chromeTop = Math.max(0, metrics.outerHeight - metrics.innerHeight);
  const chromeLeft = Math.max(0, (metrics.outerWidth - metrics.innerWidth) / 2);
  const absoluteX = Math.round((metrics.screenX + chromeLeft + rect.left) * metrics.devicePixelRatio);
  const absoluteY = Math.round((metrics.screenY + chromeTop + rect.top) * metrics.devicePixelRatio);
  return {
    x: display ? absoluteX - Math.round(display.cgX * display.scale) : absoluteX,
    y: display ? absoluteY - Math.round(display.cgY * display.scale) : absoluteY,
    width: Math.round(rect.width * metrics.devicePixelRatio),
    height: Math.round(rect.height * metrics.devicePixelRatio),
  };
}

function measuredBrowserPresentationCrop(metrics, display) {
  if (metrics.devicePixelRatio !== display.scale) throw new Error(`browser/device display scale mismatch: browser=${metrics.devicePixelRatio} display=${display.scale}`);
  return requirePresentationCrop(physicalBrowserRect(metrics, metrics.rect, display), display, 'browser presentation crop');
}

function cssPresentationCrop(physicalCrop, dpr) {
  if (!Number.isFinite(dpr) || dpr <= 0) throw new Error('invalid presentation DPR');
  if (!Number.isInteger(physicalCrop?.width) || !Number.isInteger(physicalCrop?.height) || physicalCrop.width <= 0 || physicalCrop.height <= 0) throw new Error('invalid presentation physical crop');
  const crop = { width: physicalCrop.width / dpr, height: physicalCrop.height / dpr };
  if (!Number.isInteger(crop.width) || !Number.isInteger(crop.height)) throw new Error('presentation crop does not map to integral CSS pixels');
  return crop;
}

class LeaseRegistry {
  constructor(root, { processSnapshot = systemProcessSnapshot, signal = process.kill } = {}) {
    this.root = root;
    this.entries = [];
    this.processSnapshot = processSnapshot;
    this.signal = signal;
    this.file = path.join(root, 'owned-process-lease.tsv');
    fs.writeFileSync(this.file, 'event\trole\tpid\tpgid\tcwd\tlog\tcommand_redacted\tdetail\tat\n');
  }

  start(role, command, args, { cwd, env = process.env, log, deadlineSeconds = 180 } = {}) {
    const fd = fs.openSync(log, 'a');
    const child = spawn(command, args, { cwd, env, detached: true, stdio: ['ignore', fd, fd] });
    fs.closeSync(fd);
    if (!Number.isInteger(child.pid)) throw new Error(`${role} started without pid`);
    const entry = { role, child, pid: child.pid, pgid: child.pid, cwd, log, command: path.basename(command), knownMembers: [], deadline: Date.now() + deadlineSeconds * 1000 };
    entry.observedLeader = observeLeaderAfterSpawn(entry.pid, entry.pgid, this.processSnapshot);
    child.once('exit', () => { entry.observedExit = true; });
    this.entries.push(entry);
    this.record('STARTED', entry);
    this.record('OBSERVED_LEADER', entry, `pid=${entry.observedLeader.pid};pgid=${entry.observedLeader.pgid};comm=${entry.observedLeader.comm};command=${entry.observedLeader.fullCommand}`);
    entry.deadlineTimer = setTimeout(() => {
      entry.expired = true; this.record('DEADLINE_EXPIRED', entry);
      void this.stop(entry).catch((error) => fs.appendFileSync(entry.log, `lease cleanup failure: ${error.message}\n`));
    }, deadlineSeconds * 1000);
    return entry;
  }

  attestReady(entry) {
    if (childExitObserved(entry)) throw new Error(`${entry.role} exited before readiness identity could be attested`);
    const leader = this.processSnapshot(entry.pgid).find((member) => member.pid === entry.pid && member.pgid === entry.pgid);
    if (!leader) throw new Error(`${entry.role} leader is absent at readiness attestation`);
    entry.attestedLeader = leader;
    this.record('READINESS_ATTESTED', entry, `pid=${leader.pid};pgid=${leader.pgid};comm=${leader.comm};command=${leader.fullCommand}`);
    return leader;
  }

  record(event, entry, detail = '') {
    fs.appendFileSync(this.file, `${event}\t${entry.role}\t${entry.pid}\t${entry.pgid}\t${entry.cwd}\t${entry.log}\t${entry.command}\t${detail}\t${new Date().toISOString()}\n`);
  }

  snapshot(entry, phase) {
    const decision = classifyOwnedGroup(entry, this.processSnapshot(entry.pgid));
    if (decision.state === 'verified') entry.knownMembers = decision.group;
    const members = decision.group.map((member) => `${member.pid}/${member.ppid}/${member.comm}/${member.fullCommand}`).join(',') || 'none';
    this.record('GROUP_SNAPSHOT', entry, `phase=${phase};state=${decision.state};members=${members}`);
    return decision;
  }

  requireVerifiedGroup(entry, phase) {
    const decision = this.snapshot(entry, phase);
    if (decision.state === 'missing') return null;
    if (decision.state !== 'verified') throw new Error(`${entry.role} group is ${decision.state}; refusing group signal`);
    return decision;
  }

  signalRecordedDescendants(entry, decision, signal) {
    if (decision.state !== 'descendants') throw new Error(`${entry.role} cannot signal descendants from ${decision.state}`);
    for (const member of decision.recorded) {
      try { this.signal(member.pid, signal); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      this.record('DESCENDANT_SIGNAL', entry, `signal=${signal};pid=${member.pid};pgid=${member.pgid};comm=${member.comm};command=${member.fullCommand}`);
    }
  }

  async stop(entry) {
    if (!entry || entry.cleaned) return;
    clearTimeout(entry.deadlineTimer);
    let decision = this.snapshot(entry, 'begin');
    if (decision.state === 'missing') {
      this.record('CLEANED', entry);
      entry.cleaned = true;
      return;
    }
    if (decision.state === 'exited-awaiting-reap') {
      await waitFor(`${entry.role} child reap`, () => {
        const next = this.snapshot(entry, 'await-reap');
        const disposition = reapDisposition(next.state, childExitObserved(entry));
        if (disposition === 'clean') return true;
        if (disposition === 'await-reap') return false;
        throw new Error(`${entry.role} group is ${next.state}; refusing cleanup verification`);
      }, 2_000, 25);
      this.record('CLEANED', entry, 'leader=owned-exited-awaiting-reap');
      entry.cleaned = true;
      return;
    }
    // A one-shot inspector naturally exits without descendants. If a former
    // leader left recorded descendants, signal those exact identities only;
    // an unrecorded/reused group is an explicit cleanup failure, never a
    // broad negative-PGID signal.
    if (decision.state === 'descendants') {
      this.signalRecordedDescendants(entry, decision, 'SIGTERM');
    } else {
      if (decision.state !== 'verified') throw new Error(`${entry.role} group is ${decision.state}; refusing group signal`);
      this.record('TERMINATING', entry, 'signal=SIGTERM;group=verified');
      try { this.signal(-entry.pgid, 'SIGTERM'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
    }
    if (entry.child.exitCode === null && entry.child.signalCode === null) {
      await Promise.race([new Promise((resolve) => entry.child.once('exit', resolve)), new Promise((resolve) => setTimeout(resolve, 2000))]);
    }
    decision = this.snapshot(entry, 'after-term');
    if (decision.state === 'missing') {
      this.record('CLEANED', entry);
      entry.cleaned = true;
      return;
    }
    if (decision.state === 'exited-awaiting-reap') {
      await waitFor(`${entry.role} child reap`, () => {
        const next = this.snapshot(entry, 'await-reap');
        const disposition = reapDisposition(next.state, childExitObserved(entry));
        if (disposition === 'clean') return true;
        if (disposition === 'await-reap') return false;
        throw new Error(`${entry.role} group is ${next.state}; refusing cleanup verification`);
      }, 2_000, 25);
      this.record('CLEANED', entry, 'leader=owned-exited-awaiting-reap'); entry.cleaned = true; return;
    }
    if (decision.state === 'verified') {
      this.record('KILLING', entry, 'signal=SIGKILL;group=verified');
      try { this.signal(-entry.pgid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
    } else if (decision.state === 'descendants') {
      this.signalRecordedDescendants(entry, decision, 'SIGKILL');
    } else {
      throw new Error(`${entry.role} group is ${decision.state}; refusing cleanup signal`);
    }
    await waitFor(`process group ${entry.pgid} exit`, () => {
      const next = this.snapshot(entry, 'await-exit');
      const disposition = reapDisposition(next.state, childExitObserved(entry));
      if (disposition === 'clean') return true;
      if (disposition === 'await-reap') return false;
      if (next.state === 'verified' || next.state === 'descendants') return false;
      throw new Error(`${entry.role} group is ${next.state}; refusing cleanup verification`);
    }, 2_000, 25);
    this.record('CLEANED', entry);
    entry.cleaned = true;
  }

  async cleanup() {
    const errors = [];
    for (const entry of [...this.entries].reverse()) {
      try { await this.stop(entry); } catch (error) { this.record('CLEANUP_FAILED', entry, error.message); errors.push(error); }
    }
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, 'multiple lease cleanup failures');
  }
}

async function runSelfTest() {
  const summary = summarize([10, 20, 30, 40]);
  if (summary.averageMs !== 25 || summary.p50Ms !== 20 || summary.p95Ms !== 40) throw new Error('summary regression');
  if (!validateControl({ p50Ms: 220 }, { p50Ms: 20 }).pass) throw new Error('control pass regression');
  if (validateControl({ p50Ms: 300 }, { p50Ms: 20 }).pass) throw new Error('control upper bound regression');
  const rect = physicalBrowserRect(
    { screenX: 10, screenY: 20, outerWidth: 1000, innerWidth: 980, outerHeight: 800, innerHeight: 740, devicePixelRatio: 2 },
    { left: 30, top: 40, width: 480, height: 300 },
  );
  if (JSON.stringify(rect) !== JSON.stringify({ x: 100, y: 240, width: 960, height: 600 })) throw new Error('browser geometry regression');
  const fake = Object.create(LeaseRegistry.prototype);
  fake.root = '/private/tmp/petal-test'; fake.entries = [];
  const entry = { role: 'browser', pid: 123, pgid: 123, cwd: '/private/tmp/petal-test', log: '/private/tmp/petal-test/browser.log', command: 'Google Chrome' };
  fake.entries.push(entry);
  if (entry.pid !== entry.pgid || entry.command.includes('--')) throw new Error('lease redaction/pgid regression');
  const completedInspector = { child: { exitCode: 0, signalCode: null }, observedExit: true };
  if (shouldSignalLeaseProcessGroup(completedInspector)) throw new Error('completed inspector must never be signaled or probed');
  const runningChild = { child: { exitCode: null, signalCode: null }, observedExit: false };
  if (!shouldSignalLeaseProcessGroup(runningChild)) throw new Error('running child must retain process-group cleanup');
  const primaryFailure = new Error('CDP alignment failed'); const cleanupFailure = new Error('PGID EPERM');
  if (cleanupOutcome(primaryFailure, []).message !== primaryFailure.message || cleanupOutcome(null, [cleanupFailure]).message !== cleanupFailure.message) throw new Error('primary/cleanup error preservation regression');
  const combined = cleanupOutcome(primaryFailure, [cleanupFailure]);
  if (!(combined instanceof AggregateError) || combined.errors[0] !== primaryFailure || combined.errors[1] !== cleanupFailure || !combined.message.includes('CDP alignment failed') || !combined.message.includes('PGID EPERM')) throw new Error('combined error preservation regression');
  if (await runWithCleanup(async () => 'body-value', async () => {}) !== 'body-value') throw new Error('both-pass wrapper regression');
  try { await runWithCleanup(async () => { throw primaryFailure; }, async () => {}); throw new Error('body-only wrapper did not throw'); } catch (error) { if (error !== primaryFailure) throw error; }
  try { await runWithCleanup(async () => 'unused', async () => { throw cleanupFailure; }); throw new Error('cleanup-only wrapper did not throw'); } catch (error) { if (error !== cleanupFailure) throw error; }
  try { await runWithCleanup(async () => { throw primaryFailure; }, async () => { throw cleanupFailure; }); throw new Error('both-error wrapper did not throw'); } catch (error) { if (!(error instanceof AggregateError) || error.errors[0] !== primaryFailure || error.errors[1] !== cleanupFailure) throw error; }
  const leader = { pid: 70, ppid: 1, pgid: 70, comm: 'worker', fullCommand: '/opt/petal/worker' };
  const descendant = { pid: 71, ppid: 70, pgid: 70, comm: 'helper', fullCommand: '/opt/petal/helper' };
  const groupEntry = { pid: 70, pgid: 70, command: 'worker', attestedLeader: leader, knownMembers: [], child: { exitCode: null, signalCode: null }, observedExit: false };
  if (classifyOwnedGroup(groupEntry, [leader, descendant]).state !== 'verified') throw new Error('verified group identity regression');
  const defunctLeader = { ...leader, comm: '<defunct>', fullCommand: '<defunct>', state: 'Z' };
  if (classifyOwnedGroup(groupEntry, [defunctLeader]).state !== 'exited-awaiting-reap') throw new Error('defunct leader must await reap without signal');
  if (classifyOwnedGroup(groupEntry, [{ ...defunctLeader, pgid: 71 }]).state === 'exited-awaiting-reap') throw new Error('wrong-PGID zombie must not be accepted');
  if (reapDisposition('verified', false) !== 'identity-error' || reapDisposition('exited-awaiting-reap', false) !== 'await-reap' || reapDisposition('exited-awaiting-reap', true) !== 'clean' || reapDisposition('missing', false) !== 'clean') throw new Error('zombie reap transition regression');
  if (classifyOwnedGroup(groupEntry, []).state !== 'missing') throw new Error('missing group cleanup regression');
  if (classifyOwnedGroup(groupEntry, [descendant]).state !== 'mismatch') throw new Error('mismatched leader regression');
  if (classifyOwnedGroup(groupEntry, [{ ...leader, pgid: 71 }]).state !== 'missing') throw new Error('mismatched PGID regression');
  if (classifyOwnedGroup(groupEntry, [{ ...leader, fullCommand: '/opt/petal/reused-worker' }]).state !== 'mismatch') throw new Error('reused leader identity regression');
  const exitedEntry = { ...groupEntry, knownMembers: [descendant], child: { exitCode: 0, signalCode: null }, observedExit: true };
  if (classifyOwnedGroup(exitedEntry, [descendant]).state !== 'descendants') throw new Error('recorded descendant cleanup regression');
  if (classifyOwnedGroup(exitedEntry, [{ ...descendant, pid: 72 }]).state !== 'unexpected') throw new Error('unexpected descendant must fail closed');
  if (classifyOwnedGroup({ ...exitedEntry, knownMembers: [] }, [{ ...leader, fullCommand: '/opt/petal/reused-worker' }]).state !== 'unexpected') throw new Error('reused group must fail closed');
  const parsedProcesses = parseProcessTable(' 70 1 70 /opt/petal/worker\n 71 70 70 /opt/petal/helper\n');
  if (parsedProcesses.length !== 2 || parsedProcesses[1].comm !== '/opt/petal/helper') throw new Error('ps snapshot parser regression');
  const npmToNode = { pid: 80, ppid: 1, pgid: 80, comm: 'node', fullCommand: '/usr/local/bin/node /usr/local/lib/node_modules/npm/bin/npm-cli.js run dev' };
  const npmEntry = { pid: 80, pgid: 80, command: 'npm', attestedLeader: npmToNode, knownMembers: [], child: { exitCode: null, signalCode: null }, observedExit: false };
  if (classifyOwnedGroup(npmEntry, [npmToNode]).state !== 'verified') throw new Error('npm-to-node observed identity regression');
  if (classifyOwnedGroup(npmEntry, [{ ...npmToNode, fullCommand: `${npmToNode.fullCommand} --reused` }]).state !== 'mismatch') throw new Error('npm-to-node reuse mismatch regression');
  const preExecNpm = { pid: 80, ppid: 1, pgid: 80, comm: 'npm', fullCommand: '/usr/local/bin/npm run dev' };
  const startupSnapshots = [[preExecNpm], [preExecNpm]];
  const observedNpmLeader = observeLeaderAfterSpawn(80, 80, () => startupSnapshots.shift(), 2, 0);
  if (!exactMemberIdentityMatches(observedNpmLeader, preExecNpm)) throw new Error('npm startup observation regression');
  const attestedNpmEntry = { pid: 80, pgid: 80, command: 'npm', observedLeader: observedNpmLeader, knownMembers: [], child: { exitCode: null, signalCode: null }, observedExit: false };
  const attestationSnapshots = [[npmToNode]];
  const attestFake = Object.create(LeaseRegistry.prototype);
  attestFake.processSnapshot = () => attestationSnapshots.shift(); attestFake.record = () => {};
  attestFake.attestReady(attestedNpmEntry);
  if (!exactMemberIdentityMatches(attestedNpmEntry.attestedLeader, npmToNode) || classifyOwnedGroup(attestedNpmEntry, [npmToNode]).state !== 'verified') throw new Error('npm-to-node readiness attestation regression');
  if (classifyOwnedGroup(attestedNpmEntry, [{ ...npmToNode, fullCommand: `${npmToNode.fullCommand} --reused` }]).state !== 'mismatch') throw new Error('post-ready command mutation must fail closed');
  if (oneShotInspectorExitDisposition({ resultParsed: true, exitObserved: false, exitCode: null, signalCode: null, timedOut: false }) !== 'await-exit') throw new Error('inspector result-before-exit race regression');
  if (oneShotInspectorExitDisposition({ resultParsed: true, exitObserved: false, exitCode: null, signalCode: null, timedOut: true }) !== 'timeout') throw new Error('inspector timeout regression');
  if (oneShotInspectorExitDisposition({ resultParsed: true, exitObserved: true, exitCode: 0, signalCode: null, timedOut: false }) !== 'clean') throw new Error('inspector clean-exit regression');
  if (!validateCpuUtilization({ percentOfOneCore: 50, wallMs: 1000 })) throw new Error('CPU utilization pass regression');
  if (validateCpuUtilization({ percentOfOneCore: 55.1, wallMs: 1000 })) throw new Error('CPU utilization upper bound regression');
  if (!directionOwnsNativePublisher('n2w') || directionOwnsNativePublisher('w2n')) throw new Error('W2N publisher ownership regression');
  if (JSON.stringify(selectedDirections('both')) !== JSON.stringify(['n2w', 'w2n']) || JSON.stringify(selectedDirections('w2n')) !== JSON.stringify(['w2n'])) throw new Error('direction selector regression');
  const w2n = directionPlan('w2n', ['idle', 'cpu50']);
  if (w2n.nativeCapture || w2n.nativePublisher || w2n.capturePreflight) throw new Error('W2N must never launch native capture/publisher/preflight');
  if (!w2n.control || !w2n.baseline || !w2n.cpu50) throw new Error('W2N must retain control, baseline, and CPU-50 gates');
  const n2w = directionPlan('n2w', ['idle']);
  if (!n2w.nativeCapture || !n2w.nativePublisher || !n2w.capturePreflight || n2w.cpu50) throw new Error('N2W plan regression');
  if (!hasCapturePreflightReady('CAPTURE_PREFLIGHT_READY window_id=7 frame=960x600\n')) throw new Error('capture preflight marker regression');
  if (hasCapturePreflightReady('CAPTURE_PREFLIGHT_RESULT {"status":"failed"}\n')) throw new Error('failed capture preflight must not pass');
  if (!capturePreflightGate({ ready: true, exitCode: 0, signalCode: null })) throw new Error('capture preflight gate pass regression');
  if (capturePreflightGate({ ready: false, exitCode: 0, signalCode: null }) || capturePreflightGate({ ready: true, exitCode: 1, signalCode: null })) throw new Error('capture preflight gate ordering regression');
  if (capturePreflightArgs(7).at(-1) !== '--capture-preflight-only') throw new Error('capture preflight argument regression');
  const browserCredential = 'room-p613-static';
  const browserCanonicalRoom = `petal-room-${browserCredential}`;
  const compositorRoomArgs = compositorArgs(canonicalRoomNameFromHarness(browserCanonicalRoom), ['--enqueue-delay-ms', '200']);
  if (compositorRoomArgs[0] !== 'petal-room-room-p613-static' || compositorRoomArgs[0] === 'petal-room-p613-static') throw new Error('canonical browser room regression');
  if (publisherArgs(7, browserCanonicalRoom, ['--source', 'real'])[1] !== browserCanonicalRoom) throw new Error('publisher must receive the browser canonical room unchanged');
  const primary = { id: 1, appkitX: 0, appkitY: 0, width: 1440, height: 900, scale: 2, pixelWidth: 2880, pixelHeight: 1800 };
  const secondary = { id: 2, appkitX: -1280, appkitY: 0, width: 1280, height: 720, scale: 1, pixelWidth: 1280, pixelHeight: 720 };
  const observerDescriptor = observerDisplayArgs({ ...primary, cgX: 0, cgY: 0 });
  if (!observerDescriptor.includes('--display-scale') || !observerDescriptor.includes('2880') || !observerDescriptor.includes('--display-cg-x')) throw new Error('observer display descriptor regression');
  const layout = choosePresentationLayout([secondary, primary]);
  if (layout.display.id !== 1) throw new Error('same-display fit selector regression');
  const initialSource = initialPresentationSourceCrop(primary);
  const compositorFrame = appKitFrameForCrop(primary, deriveDestinationCrop(initialSource, primary));
  if (compositorFrame.y !== 700 || compositorWindowArgs(browserCanonicalRoom, primary, deriveDestinationCrop(initialSource, primary), 200)[0] !== browserCanonicalRoom) throw new Error('AppKit top/bottom conversion regression');
  if (!(() => { try { choosePresentationLayout([secondary]); return false; } catch { return true; } })()) throw new Error('same-display no-fit regression');
  const observedBrowser = measuredBrowserPresentationCrop({ screenX: 0, screenY: 0, outerWidth: 340, innerWidth: 320, outerHeight: 240, innerHeight: 180, devicePixelRatio: 2, rect: { left: 0, top: 0, width: 320, height: 180 } }, { ...primary, cgX: 0, cgY: 0 });
  if (JSON.stringify(observedBrowser) !== JSON.stringify({ x: 20, y: 120, width: 640, height: 360 })) throw new Error('browser chrome offset crop regression');
  const rightDestination = deriveDestinationCrop({ x: 80, y: 120, width: 640, height: 360 }, primary);
  if (JSON.stringify(rightDestination) !== JSON.stringify({ x: 760, y: 120, width: 640, height: 360 })) throw new Error('horizontal right-fit destination regression');
  const leftDestination = deriveDestinationCrop({ x: 2000, y: 120, width: 640, height: 360 }, primary);
  if (JSON.stringify(leftDestination) !== JSON.stringify({ x: 1320, y: 120, width: 640, height: 360 })) throw new Error('horizontal left-fit destination regression');
  if (!(() => { try { deriveDestinationCrop({ x: 320, y: 120, width: 640, height: 360 }, secondary); return false; } catch { return true; } })()) throw new Error('horizontal no-fit destination regression');
  const observedSecondary = measuredBrowserPresentationCrop({ screenX: -1200, screenY: 0, outerWidth: 1000, innerWidth: 980, outerHeight: 240, innerHeight: 180, devicePixelRatio: 1, rect: { left: 0, top: 0, width: 640, height: 360 } }, { ...secondary, cgX: -1280, cgY: 0 });
  if (JSON.stringify(observedSecondary) !== JSON.stringify({ x: 90, y: 60, width: 640, height: 360 })) throw new Error('negative-origin display crop regression');
  if (observerInvalidClassification('OBSERVER_DISPLAY_CANDIDATES available=[]\nINVALID_OBSERVER_DISPLAY_UNAVAILABLE zero_cells=1\n') !== 'INVALID_OBSERVER_DISPLAY_UNAVAILABLE') throw new Error('observer display-unavailable classification regression');
  if (observerInvalidClassification('OBSERVER_DISPLAY_CANDIDATES available=[id=1]\n') !== null) throw new Error('nonempty observer display path regression');
  const cssCropAtScaleOne = cssPresentationCrop({ width: 640, height: 360 }, 1);
  const cssCropAtScaleTwo = cssPresentationCrop({ width: 640, height: 360 }, 2);
  if (JSON.stringify(cssCropAtScaleOne) !== JSON.stringify({ width: 640, height: 360 }) || JSON.stringify(cssCropAtScaleTwo) !== JSON.stringify({ width: 320, height: 180 })) throw new Error('presentation CSS crop conversion regression');
  if (!(() => { try { cssPresentationCrop({ width: 640, height: 360 }, 0); return false; } catch { return true; } })()) throw new Error('invalid presentation DPR must fail closed');
  if (!(() => { try { cssPresentationCrop({ width: 0, height: 360 }, 1); return false; } catch { return true; } })()) throw new Error('invalid presentation physical crop must fail closed');
  console.log('SELF_TEST_PASS reducer control geometry process-lease ownership-errors');
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(label, fn, timeoutMs = 15_000, intervalMs = 100) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await fn();
    if (last) return last;
    await sleep(intervalMs);
  }
  throw new Error(`${label} timed out; last=${JSON.stringify(last)}`);
}

async function portOpen(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ host: '127.0.0.1', port });
    socket.once('connect', () => { socket.destroy(); resolve(true); });
    socket.once('error', () => resolve(false));
    socket.setTimeout(250, () => { socket.destroy(); resolve(false); });
  });
}

function readFreeGiB() {
  const result = spawnSync('df', ['-k', '/System/Volumes/Data'], { encoding: 'utf8' });
  if (result.status !== 0) throw new Error('df failed');
  const fields = result.stdout.trim().split('\n').at(-1).trim().split(/\s+/);
  return Number(fields[3]) / 1024 / 1024;
}

async function connectCdp(port) {
  const pages = await waitFor('CDP page', async () => {
    try { return await (await fetch(`http://127.0.0.1:${port}/json`)).json(); } catch { return null; }
  });
  const page = pages.find((candidate) => candidate.type === 'page');
  if (!page?.webSocketDebuggerUrl) throw new Error('CDP page has no websocket URL');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  const pending = new Map(); let nextId = 1;
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }); });
  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data); const waiter = pending.get(message.id);
    if (!waiter) return; pending.delete(message.id);
    message.error ? waiter.reject(new Error(message.error.message)) : waiter.resolve(message.result);
  });
  return {
    call(method, params = {}) {
      const id = nextId++; ws.send(JSON.stringify({ id, method, params }));
      return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
    },
    close() { ws.close(); },
  };
}

async function evaluate(cdp, expression) {
  const result = await cdp.call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
  return result.result?.value;
}

async function joinBrowser(cdp, url, meetingCode) {
  await cdp.call('Page.enable');
  await cdp.call('Page.navigate', { url });
  await waitFor('web harness bootstrap', () => evaluate(cdp, `!!window.__petalHarness && !!document.querySelector('#join-btn')`));
  await evaluate(cdp, `(() => {
    localStorage.setItem('petal-harness-name', 'p613-browser');
    const name = document.querySelector('#display-name'); const code = document.querySelector('#meeting-code');
    name.value = 'p613-browser'; code.value = ${JSON.stringify(meetingCode)};
    name.dispatchEvent(new Event('input', { bubbles: true })); code.dispatchEvent(new Event('input', { bubbles: true }));
    document.querySelector('#join-btn').click(); return true;
  })()`);
  const roomName = await waitFor('web harness room connected', () => evaluate(cdp, `window.__petalHarness?.room?.state === 'connected' ? window.__petalHarness.room.name : null`), 20_000);
  return canonicalRoomNameFromHarness(roomName);
}

async function prepareBrowserStage(cdp, mode, display) {
  const cssCrop = cssPresentationCrop(PRESENTATION_CROP, display.scale);
  const stage = await evaluate(cdp, `(async () => {
    const old = document.getElementById('p613-stage'); if (old) old.remove();
    window.__p613PresentationGeneration = (window.__p613PresentationGeneration || 0) + 1;
    const token = window.__p613PresentationGeneration;
    const stage = ${JSON.stringify(mode)} === 'source' ? null : document.createElement('div');
    if (stage) { stage.id = 'p613-stage'; Object.assign(stage.style, { position:'fixed', inset:'0', zIndex:'2147483647', background:'#08080a', pointerEvents:'none' }); document.body.appendChild(stage); }
    const cssCrop = ${JSON.stringify(cssCrop)};
    let measured;
    let sourceHost = null;
    if (${JSON.stringify(mode)} === 'source') {
      measured = document.querySelector('#test-canvas.p613-presentation-source');
      sourceHost = document.getElementById('p613-presentation-source-host');
      if (!measured || measured !== document.querySelector('#test-canvas') || measured.parentElement !== sourceHost || sourceHost?.parentElement !== document.body) {
        return { metrics: { source: 'unavailable', expectedCss: cssCrop }, validationError: 'exact top-level captureStream canvas unavailable' };
      }
      // Presentation sizing only: retain this exact canvas and its 960x600
      // drawing buffer/captureStream source, but map the selected physical
      // observer crop through the verified DPR.
      Object.assign(measured.style, { width:cssCrop.width+'px', height:cssCrop.height+'px', maxWidth:'none' });
      Object.assign(sourceHost.style, { width:cssCrop.width+'px', height:cssCrop.height+'px' });
    } else {
      const video = Array.from(document.querySelectorAll('#tiles video')).find((candidate) => candidate.readyState >= 2 && candidate.videoWidth > 0);
      if (!video) throw new Error('live remote video unavailable');
      // #613 measures the actual product tile.  Never hide it or copy it to
      // a derived canvas: either would turn this into a browser-side proxy.
      measured = video;
    }
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const rect = measured.getBoundingClientRect();
    const inspect = (element) => {
      const ancestry = [];
      for (let current = element; current; current = current.parentElement) {
        const style = getComputedStyle(current); const bounds = current.getBoundingClientRect();
        ancestry.push({ tag: current.tagName, id: current.id, display: style.display, visibility: style.visibility, opacity: style.opacity, width: bounds.width, height: bounds.height });
      }
      const style = getComputedStyle(element);
      return { connected: element.isConnected, display: style.display, visibility: style.visibility, opacity: style.opacity, ancestors: ancestry };
    };
    const metrics = { rect:{left:rect.left,top:rect.top,width:rect.width,height:rect.height}, presentation:inspect(measured), screenX,screenY,outerWidth,outerHeight,innerWidth,innerHeight,devicePixelRatio };
    const visiblyLaidOut = metrics.presentation.connected && metrics.presentation.display !== 'none' && metrics.presentation.visibility !== 'hidden' && Number(metrics.presentation.opacity) > 0 && metrics.presentation.ancestors.every((ancestor) => ancestor.display !== 'none' && ancestor.visibility !== 'hidden' && Number(ancestor.opacity) > 0);
    return { metrics, validationError: visiblyLaidOut && rect.width === cssCrop.width && rect.height === cssCrop.height ? null : 'presentation CSS crop mismatch observed='+JSON.stringify(metrics)+' expectedCss='+JSON.stringify(cssCrop) };
  })()`);
  if (stage.validationError) throw new Error(stage.validationError);
  return { metrics: stage.metrics, crop: measuredBrowserPresentationCrop(stage.metrics, display) };
}

async function positionBrowserPresentation(cdp, mode, display, plannedCrop) {
  requirePresentationCrop(plannedCrop, display, 'planned browser presentation crop');
  const observed = await prepareBrowserStage(cdp, mode, display);
  const windowInfo = await cdp.call('Browser.getWindowForTarget');
  const bounds = windowInfo.bounds;
  if (!Number.isFinite(bounds.left) || !Number.isFinite(bounds.top)) throw new Error('Chrome did not report concrete window bounds');
  await cdp.call('Browser.setWindowBounds', { windowId: windowInfo.windowId, bounds: {
    left: Math.round(bounds.left + (plannedCrop.x - observed.crop.x) / display.scale),
    top: Math.round(bounds.top + (plannedCrop.y - observed.crop.y) / display.scale),
    width: Math.max(bounds.width ?? 480, cssPresentationCrop(PRESENTATION_CROP, display.scale).width), height: Math.max(bounds.height ?? 300, cssPresentationCrop(PRESENTATION_CROP, display.scale).height),
  } });
  await sleep(100);
  const positioned = await prepareBrowserStage(cdp, mode, display);
  if (!sameCrop(positioned.crop, plannedCrop)) throw new Error(`browser presentation placement drifted: observed=${JSON.stringify(positioned.crop)} planned=${JSON.stringify(plannedCrop)}`);
  return positioned.crop;
}

function parseMarker(logPath, marker) {
  if (!fs.existsSync(logPath)) return null;
  const line = fs.readFileSync(logPath, 'utf8').split('\n').findLast((value) => value.startsWith(`${marker} `));
  return line ? line.slice(marker.length + 1).trim().split(/\s+/).map(Number) : null;
}

function readResult(logPath) {
  const line = fs.readFileSync(logPath, 'utf8').split('\n').findLast((value) => value.startsWith('PRESENTATION_RESULT_JSON '));
  if (!line) throw new Error(`observer produced no result: ${logPath}`);
  return JSON.parse(line.slice('PRESENTATION_RESULT_JSON '.length));
}

const INVALID_OBSERVER_DISPLAY_UNAVAILABLE = 'INVALID_OBSERVER_DISPLAY_UNAVAILABLE';

function observerInvalidClassification(log) {
  return String(log).split('\n').find((line) => line.startsWith(INVALID_OBSERVER_DISPLAY_UNAVAILABLE)) ? INVALID_OBSERVER_DISPLAY_UNAVAILABLE : null;
}

function writeZeroCellInvalidEvidence(root, label, classification, log) {
  const file = path.join(root, `${label}-invalid.json`);
  const diagnostic = String(log).split('\n').find((line) => line.startsWith(classification)) ?? classification;
  fs.writeFileSync(file, `${JSON.stringify({ classification, cells: 0, validResult: false, resumeCondition: 'retry only after SCShareableContent reports one matching display', diagnostic })}\n`);
  return file;
}

function readCpuUtilization(logPath) {
  if (!fs.existsSync(logPath)) return null;
  const line = fs.readFileSync(logPath, 'utf8').split('\n').findLast((value) => value.startsWith('CPU50_UTILIZATION '));
  return line ? JSON.parse(line.slice('CPU50_UTILIZATION '.length)) : null;
}

function validateCpuUtilization(sample) {
  return !!sample && Number.isFinite(sample.percentOfOneCore)
    && sample.percentOfOneCore >= 45 && sample.percentOfOneCore <= 55 && sample.wallMs >= 900;
}

function observerDisplayArgs(display) {
  if (!Number.isInteger(display.id) || !Number.isFinite(display.cgX) || !Number.isFinite(display.cgY)
    || !Number.isFinite(display.width) || !Number.isFinite(display.height) || !Number.isFinite(display.scale)
    || !Number.isInteger(display.pixelWidth) || !Number.isInteger(display.pixelHeight)) throw new Error('observer display descriptor is invalid');
  return ['--display-id', String(display.id), '--display-cg-x', String(display.cgX), '--display-cg-y', String(display.cgY), '--display-width-points', String(display.width), '--display-height-points', String(display.height), '--display-scale', String(display.scale), '--display-pixel-width', String(display.pixelWidth), '--display-pixel-height', String(display.pixelHeight)];
}

async function runObserver(leases, root, label, sourceRect, destinationRect, display, options) {
  const log = path.join(root, `${label}-observer.log`); const csv = path.join(root, `${label}.csv`);
  const args = [observerScript, '--source-rect', sourceRect.x, sourceRect.y, sourceRect.width, sourceRect.height, '--destination-rect', destinationRect.x, destinationRect.y, destinationRect.width, destinationRect.height, '--source-window-id', String(sourceRect.windowId), '--destination-window-id', String(destinationRect.windowId), ...observerDisplayArgs(display), '--samples', String(options.samples), '--warmup', String(options.warmup), '--timeout-seconds', '45', '--output', csv];
  const entry = leases.start(`${label}-observer`, 'swift', args, { cwd: repoRoot, log, deadlineSeconds: 60 });
  const exitCode = await new Promise((resolve) => { entry.child.once('exit', resolve); });
  if (exitCode !== 0) {
    const observerLog = fs.existsSync(log) ? fs.readFileSync(log, 'utf8') : '';
    const classification = observerInvalidClassification(observerLog);
    if (classification) {
      if (fs.existsSync(csv)) throw new Error(`${label} emitted invalid observer classification with a CSV`);
      const invalidEvidence = writeZeroCellInvalidEvidence(root, label, classification, observerLog);
      throw new Error(`${label} ${classification} zero-cell invalid apparatus; evidence=${invalidEvidence}; resume=retry only after SCShareableContent reports one matching display`);
    }
    throw new Error(`${label} observer exited ${exitCode}`);
  }
  return readResult(log);
}

async function inspectWindowId(leases, root, label, ownerPid) {
  const log = path.join(root, `${label}-window-inspector.log`);
  const entry = leases.start(`${label}-window-inspector`, 'swift', [targetScript, '--inspect-window-owner-pid', String(ownerPid)], { cwd: repoRoot, log, deadlineSeconds: 15 });
  const id = await waitFor(`${label} concrete CGWindow id`, () => parseMarker(log, 'WINDOW_ID'), 10_000);
  const remainingMs = Math.max(1, entry.deadline - Date.now());
  let disposition;
  try {
    disposition = await waitFor(`${label} window inspector exit`, () => {
      const next = oneShotInspectorExitDisposition({
        resultParsed: true,
        exitObserved: childExitObserved(entry),
        exitCode: entry.child.exitCode,
        signalCode: entry.child.signalCode,
        timedOut: false,
      });
      return next === 'await-exit' ? null : next;
    }, remainingMs, 25);
  } catch (error) {
    await leases.stop(entry);
    throw new Error(`${label} window inspector did not exit before its deadline: ${error.message}`);
  }
  await leases.stop(entry);
  if (disposition !== 'clean') throw new Error(`${label} window inspector exited before completion: ${disposition}`);
  return id[0];
}

async function runCpu50(leases, root, label) {
  const script = `let previous=process.cpuUsage(),previousWall=performance.now();function report(){const now=performance.now(),usage=process.cpuUsage(previous),wall=now-previousWall;previous=process.cpuUsage();previousWall=now;const pct=100*((usage.user+usage.system)/1000)/wall;console.log('CPU50_UTILIZATION '+JSON.stringify({percentOfOneCore:pct,wallMs:wall,cpuMs:(usage.user+usage.system)/1000}));}setInterval(()=>{const end=performance.now()+50;while(performance.now()<end){}},100);setInterval(report,1000);process.on('SIGTERM',()=>{report();process.exit(0)});`;
  return leases.start(`${label}-cpu50`, process.execPath, ['-e', script], { cwd: root, log: path.join(root, `${label}-cpu50.log`), deadlineSeconds: 120 });
}

async function main(options) {
  if (readFreeGiB() < 20) throw new Error('less than 20 GiB free');
  for (const candidate of [options.port, options.webPort, options.cdpPort]) if (await portOpen(candidate)) throw new Error(`port ${candidate} already in use`);
  for (const executable of [publisher, compositor]) if (!fs.existsSync(executable)) throw new Error(`build required: ${executable}`);
  fs.mkdirSync(options.output, { recursive: true });
  const leases = new LeaseRegistry(options.output); let cdp;
  const env = { ...process.env, LIVEKIT_URL: `ws://127.0.0.1:${options.port}`, LIVEKIT_API_KEY: 'devkey', LIVEKIT_API_SECRET: 'secret', PETAL_BACKEND_URL: '', VITE_SENTRY_DSN: '' };
  const stamp = Date.now().toString(36); const meetingCode = `p613-${stamp}`;
  const layout = choosePresentationLayout(listDisplayLayouts());
  return runWithCleanup(async () => {
    const sfu = leases.start('sfu', 'livekit-server', ['--dev', '--bind', '127.0.0.1', '--port', String(options.port)], { cwd: repoRoot, env, log: path.join(options.output, 'sfu.log'), deadlineSeconds: 900 });
    await waitFor('SFU listen', () => portOpen(options.port));
    leases.attestReady(sfu);
    const vite = leases.start('vite', 'npm', ['run', 'dev', '--', '--host', '127.0.0.1', '--port', String(options.webPort)], { cwd: webDir, env, log: path.join(options.output, 'vite.log'), deadlineSeconds: 900 });
    await waitFor('Vite listen', () => portOpen(options.webPort), 20_000);
    leases.attestReady(vite);
    const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
    const browserX = layout.display.cgX + PRESENTATION_CROP.margin / layout.display.scale;
    const browserY = layout.display.cgY + PRESENTATION_CROP.margin / layout.display.scale;
    const browser = leases.start('browser', chrome, [`--remote-debugging-port=${options.cdpPort}`, `--user-data-dir=${path.join(options.output, 'chrome-profile')}`, `--window-position=${browserX},${browserY}`, '--window-size=480,300', '--no-first-run', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows', `http://127.0.0.1:${options.webPort}/`], { cwd: repoRoot, env, log: path.join(options.output, 'chrome.log'), deadlineSeconds: 900 });
    cdp = await connectCdp(options.cdpPort);
    leases.attestReady(browser);
    const room = await joinBrowser(cdp, `http://127.0.0.1:${options.webPort}/`, meetingCode);
    const browserWindowId = await inspectWindowId(leases, options.output, 'browser', browser.pid);
    const directions = selectedDirections(options.direction);
    const loads = options.load === 'both' ? ['idle', 'cpu50'] : [options.load];
    const report = { metric: 'same-display source-presentation to destination-presentation', glassToPhoton: false, samplesPerCell: options.samples, directions: {} };
    for (const direction of directions) {
      const dirRoot = path.join(options.output, direction); fs.mkdirSync(dirRoot, { recursive: true });
      let sourceRect; let persistentPublisher; let nativeSource;
      const plan = directionPlan(direction, loads);
      if (plan.nativeCapture) {
        const targetLog = path.join(dirRoot, 'native-source.log');
        const sourceFrame = appKitFrameForCrop(layout.display, initialPresentationSourceCrop(layout.display));
        nativeSource = leases.start('native-source', 'swift', [targetScript, '--presentation-pattern', '--display-id', String(layout.display.id), '--width', '640', '--height', '360', '--fps', '30', '--seconds', '900', '--origin-x', String(sourceFrame.x - layout.display.appkitX), '--origin-y', String(sourceFrame.y - layout.display.appkitY)], { cwd: repoRoot, log: targetLog, deadlineSeconds: 900 });
        const crop = await waitFor('native source crop', () => parseMarker(targetLog, 'SOURCE_CROP_PX'));
        leases.attestReady(nativeSource);
        sourceRect = { x: crop[0], y: crop[1], width: crop[2], height: crop[3], windowId: await waitFor('native source concrete window id', () => parseMarker(targetLog, 'WINDOW_ID')?.[0]) };
        requirePresentationCrop(sourceRect, layout.display, 'native source presentation crop');
        const windowId = await waitFor('native source window id', () => parseMarker(targetLog, 'WINDOW_ID')?.[0]);
        if (plan.capturePreflight) {
          const preflightLog = path.join(dirRoot, 'capture-preflight.log');
          const preflight = leases.start('n2w-capture-preflight', publisher, capturePreflightArgs(windowId), { cwd: tauriDir, env, log: preflightLog, deadlineSeconds: 10 });
          const preflightReady = await waitFor('N2W capture preflight ready', () => fs.existsSync(preflightLog) && hasCapturePreflightReady(fs.readFileSync(preflightLog, 'utf8')), 7_000);
          await waitFor('N2W capture preflight exit', () => preflight.child.exitCode !== null || preflight.child.signalCode !== null, 2_000);
          if (!capturePreflightGate({ ready: preflightReady, exitCode: preflight.child.exitCode, signalCode: preflight.child.signalCode })) throw new Error(`N2W capture preflight failed (exit=${preflight.child.exitCode}, signal=${preflight.child.signalCode})`);
        }
        const publisherFor = async (delayMs, label) => {
          const entry = leases.start(`${label}-native-publisher`, publisher, publisherArgs(windowId, room, ['--source', 'real', '--seconds', '900', '--expected-capture-width', '960', '--expected-capture-height', '600', '--presentation-delay-ms', String(delayMs)]), { cwd: tauriDir, env, log: path.join(dirRoot, `${label}-publisher.log`), deadlineSeconds: 900 });
          await waitFor('browser product remote tile', () => evaluate(cdp, `Array.from(document.querySelectorAll('#tiles video')).some(v=>v.readyState>=2&&v.videoWidth>0)`), 20_000);
          leases.attestReady(entry);
          return entry;
        };
        persistentPublisher = { publisherFor };
      } else {
        await evaluate(cdp, `document.querySelector('#share-btn').click(); true`);
        await waitFor('browser pattern publish', () => evaluate(cdp, `!!window.__petalHarness?.localVideoTrack`), 20_000);
        sourceRect = { ...(await prepareBrowserStage(cdp, 'source', layout.display)).crop, windowId: browserWindowId };
      }
      const destinationCrop = deriveDestinationCrop(sourceRect, layout.display);

      async function destinationFor(delayMs, label) {
        if (plan.nativePublisher) {
          if (delayMs !== 0) throw new Error('N2W control delay belongs in the example publisher, not the product video surface');
          const actual = await positionBrowserPresentation(cdp, 'destination', layout.display, destinationCrop);
          return { rect: { ...requireActualDestinationCrop(sourceRect, actual, layout.display, destinationCrop, label), windowId: browserWindowId }, entry: null };
        }
        const log = path.join(dirRoot, `${label}-compositor.log`);
        const entry = leases.start(`${label}-compositor`, compositor, compositorWindowArgs(room, layout.display, destinationCrop, delayMs), { cwd: tauriDir, env, log, deadlineSeconds: 90 });
        const crop = await waitFor(`${label} compositor crop`, () => parseMarker(log, 'DESTINATION_CROP_PX'));
        await waitFor(`${label} compositor frames`, () => fs.readFileSync(log, 'utf8').includes('display_enqueued='), 20_000);
        const windowId = await waitFor(`${label} compositor concrete window id`, () => parseMarker(log, 'WINDOW_ID')?.[0]);
        leases.attestReady(entry);
        const actual = { x: crop[0], y: crop[1], width: crop[2], height: crop[3] };
        return { rect: { ...requireActualDestinationCrop(sourceRect, actual, layout.display, destinationCrop, label), windowId }, entry };
      }

      if (plan.nativePublisher) persistentPublisher = await persistentPublisher.publisherFor(200, `${direction}-control`);
      const controlDestination = await destinationFor(plan.nativePublisher ? 0 : 200, `${direction}-control`);
      const control = await runObserver(leases, dirRoot, `${direction}-control`, sourceRect, controlDestination.rect, layout.display, options);
      await leases.stop(controlDestination.entry);
      if (plan.nativePublisher) await leases.stop(persistentPublisher);
      if (plan.nativePublisher) persistentPublisher = await (async () => {
        const entry = leases.start('n2w-idle-native-publisher', publisher, publisherArgs(parseMarker(path.join(dirRoot, 'native-source.log'), 'WINDOW_ID')?.[0], room, ['--source', 'real', '--seconds', '900', '--expected-capture-width', '960', '--expected-capture-height', '600', '--presentation-delay-ms', '0']), { cwd: tauriDir, env, log: path.join(dirRoot, 'n2w-idle-publisher.log'), deadlineSeconds: 900 });
        await waitFor('browser product baseline tile', () => evaluate(cdp, `Array.from(document.querySelectorAll('#tiles video')).some(v=>v.readyState>=2&&v.videoWidth>0)`), 20_000);
        leases.attestReady(entry);
        return entry;
      })();
      const baselineDestination = await destinationFor(0, `${direction}-idle`);
      const idle = await runObserver(leases, dirRoot, `${direction}-idle`, sourceRect, baselineDestination.rect, layout.display, options);
      const controlGate = validateControl(control, idle);
      if (!validateCell(control, options.samples) || !validateCell(idle, options.samples) || !controlGate.pass) throw new Error(`${direction} positive control/validity gate failed`);
      const cells = { control, idle, controlGate };
      if (plan.cpu50) {
        const cpu = await runCpu50(leases, dirRoot, `${direction}-cpu50`);
        const cpuLog = path.join(dirRoot, `${direction}-cpu50-cpu50.log`);
        await waitFor(`${direction} CPU50 utilization`, () => readCpuUtilization(cpuLog), 5_000, 100);
        leases.attestReady(cpu);
        if (!validateCpuUtilization(readCpuUtilization(cpuLog))) throw new Error(`${direction} CPU50 worker was not within 45-55% of one core`);
        cells.cpu50 = await runObserver(leases, dirRoot, `${direction}-cpu50`, sourceRect, baselineDestination.rect, layout.display, options);
        if (!validateCpuUtilization(readCpuUtilization(cpuLog))) throw new Error(`${direction} CPU50 utilization drifted outside 45-55% of one core`);
        await leases.stop(cpu);
        if (!validateCell(cells.cpu50, options.samples)) throw new Error(`${direction} cpu50 validity gate failed`);
      }
      await leases.stop(baselineDestination.entry);
      // Do not let the native publisher remain subscribed when the next
      // direction starts: compositor_probe intentionally accepts remote video
      // tracks, so retaining it could make W2N measure N2W's old source.
      if (plan.nativePublisher) {
        if (!persistentPublisher || persistentPublisher.child.exitCode !== null) throw new Error('N2W publisher exited before direction completed');
        await leases.stop(persistentPublisher);
        await leases.stop(nativeSource);
      }
      report.directions[direction] = cells;
    }
    report.pass = Object.values(report.directions).every((direction) => ['idle', 'cpu50'].filter((cell) => direction[cell]).every((cell) => direction[cell].p95Ms < 100));
    fs.writeFileSync(path.join(options.output, 'result.json'), `${JSON.stringify(report, null, 2)}\n`);
    console.log(`PRESENTATION_MATRIX_RESULT ${JSON.stringify(report)}`);
  }, async () => {
    const cleanupErrors = [];
    try { cdp?.close(); } catch (error) { cleanupErrors.push(error); }
    try { await leases.cleanup(); } catch (error) { cleanupErrors.push(error); }
    if (cleanupErrors.length === 1) throw cleanupErrors[0];
    if (cleanupErrors.length > 1) throw new AggregateError(cleanupErrors, 'multiple main cleanup failures');
  });
}

const options = parseArgs(process.argv);
if (options.selfTest) await runSelfTest();
else await main(options);
