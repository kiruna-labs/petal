#!/usr/bin/env node
// Process-lease ledger tests (plan Item 7).
//
// The leak: an external `timeout` kill of the runner orphaned the AppKit photon
// sentinel, because no signal handler existed and `finally` does not run when
// Node is killed. The end-to-end test at the bottom reproduces exactly that --
// it TERMs a real runner process BY PID, mid-run, and asserts the whole spawned
// process group is gone afterwards.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { ProcessLeaseLedger, psIdentity } from './process-lease-ledger.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const ledgerModulePath = path.join(scriptDir, 'process-lease-ledger.mjs');

function withTempLedger(fn) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'petal-lease-test-'));
  try {
    return fn(path.join(dir, 'leases.tsv'), dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function ledgerEvents(file) {
  return readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line && !line.startsWith('event\t'))
    .map((line) => line.split('\t')[0]);
}

// A scripted `ps` stand-in: each call shifts the next identity off the queue.
function fakeIdentities(sequence) {
  const queue = [...sequence];
  return () => (queue.length > 1 ? queue.shift() : queue[0]);
}

const SENTINEL = { pid: 4242, pgid: 4242, command: 'PetalRCPhotonSentinel' };

function registeredEntry(ledger, overrides = {}) {
  return {
    role: 'photon-sentinel',
    pid: SENTINEL.pid,
    pgid: SENTINEL.pgid,
    command: SENTINEL.command,
    groupLeader: true,
    ...overrides,
  };
}

test('a SIGTERM that works is verified, and never escalates', () => {
  withTempLedger((file) => {
    const signals = [];
    const ledger = new ProcessLeaseLedger({
      file,
      identify: fakeIdentities([SENTINEL, SENTINEL, null]),
      kill: (pid, signal) => signals.push([pid, signal]),
      sleep: () => {},
      log: () => {},
    });
    ledger.startRun();
    assert.equal(ledger.release(registeredEntry(ledger)), 'terminated');
    assert.deepEqual(signals, [[-4242, 'SIGTERM']]);
    assert.deepEqual(ledgerEvents(file), ['TERMINATING', 'CLEANED']);
  });
});

test('a sentinel that ignores SIGTERM is escalated to SIGKILL and re-verified', () => {
  withTempLedger((file) => {
    const signals = [];
    // Alive through the whole SIGTERM wait, gone once SIGKILL lands.
    let killed = false;
    const ledger = new ProcessLeaseLedger({
      file,
      identify: () => (killed ? null : SENTINEL),
      kill: (pid, signal) => {
        signals.push([pid, signal]);
        if (signal === 'SIGKILL') killed = true;
      },
      sleep: () => {},
      log: () => {},
      waitMs: 5,
    });
    ledger.startRun();
    assert.equal(ledger.release(registeredEntry(ledger)), 'killed');
    assert.deepEqual(signals, [
      [-4242, 'SIGTERM'],
      [-4242, 'SIGKILL'],
    ]);
    assert.deepEqual(ledgerEvents(file), ['TERMINATING', 'KILLING', 'CLEANED']);
  });
});

test('a process that survives SIGKILL is recorded ORPHANED, never CLEANED', () => {
  withTempLedger((file) => {
    const warnings = [];
    const ledger = new ProcessLeaseLedger({
      file,
      identify: () => SENTINEL,
      kill: () => {},
      sleep: () => {},
      log: (message) => warnings.push(message),
      waitMs: 5,
    });
    ledger.startRun();
    assert.equal(ledger.release(registeredEntry(ledger)), 'orphaned');
    const events = ledgerEvents(file);
    assert.deepEqual(events, ['TERMINATING', 'KILLING', 'ORPHANED']);
    // Not settled: the next run's sweep must try again.
    assert.ok(!events.includes('CLEANED'));
    assert.match(warnings.join('\n'), /survived SIGKILL/);
  });
});

test('a recycled PID is never signalled', () => {
  withTempLedger((file) => {
    const signals = [];
    const ledger = new ProcessLeaseLedger({
      file,
      // Same pid, different process entirely.
      identify: () => ({ pid: 4242, pgid: 991, command: 'Finder' }),
      kill: (pid, signal) => signals.push([pid, signal]),
      sleep: () => {},
      log: () => {},
    });
    ledger.startRun();
    assert.equal(ledger.release(registeredEntry(ledger)), 'identity-mismatch');
    assert.deepEqual(signals, []);
    assert.deepEqual(ledgerEvents(file), ['IDENTITY_MISMATCH']);
  });
});

test('a lease with no live process is settled without signalling', () => {
  withTempLedger((file) => {
    const signals = [];
    const ledger = new ProcessLeaseLedger({
      file,
      identify: () => null,
      kill: (pid, signal) => signals.push([pid, signal]),
      sleep: () => {},
      log: () => {},
    });
    ledger.startRun();
    assert.equal(ledger.release(registeredEntry(ledger)), 'already-gone');
    assert.deepEqual(signals, []);
    assert.deepEqual(ledgerEvents(file), ['ALREADY_GONE']);
  });
});

test('a non-group-leader lease signals its pid, never the shared group', () => {
  withTempLedger((file) => {
    const signals = [];
    const ledger = new ProcessLeaseLedger({
      file,
      identify: fakeIdentities([{ pid: 4242, pgid: 77, command: 'PetalRCPhotonSentinel' }, null]),
      kill: (pid, signal) => signals.push([pid, signal]),
      sleep: () => {},
      log: () => {},
    });
    ledger.startRun();
    const entry = registeredEntry(ledger, { pgid: 77, groupLeader: false });
    assert.equal(ledger.release(entry), 'terminated');
    // -77 would have signalled this process's own group.
    assert.deepEqual(signals, [[4242, 'SIGTERM']]);
  });
});

test('the startup sweep re-identifies every unsettled lease from a previous run', () => {
  withTempLedger((file) => {
    const at = new Date().toISOString();
    writeFileSync(
      file,
      [
        'event\trole\tpid\tpgid\tcwd\tlog\tcommand\tdetail\tat',
        // Still running and still itself: must be killed.
        `STARTED\tphoton-sentinel\t101\t101\t/tmp\t\tPetalRCPhotonSentinel\t\t${at}`,
        // PID recycled by something else: must NOT be killed.
        `STARTED\tphoton-sentinel\t202\t202\t/tmp\t\tPetalRCPhotonSentinel\t\t${at}`,
        // Already gone.
        `STARTED\tphoton-sentinel\t303\t303\t/tmp\t\tPetalRCPhotonSentinel\t\t${at}`,
        // Already cleaned by its own run: must be skipped entirely.
        `STARTED\tphoton-sentinel\t404\t404\t/tmp\t\tPetalRCPhotonSentinel\t\t${at}`,
        `CLEANED\tphoton-sentinel\t404\t404\t/tmp\t\tPetalRCPhotonSentinel\t\t${at}`,
      ].join('\n') + '\n'
    );
    const signals = [];
    const dead = new Set();
    const ledger = new ProcessLeaseLedger({
      file,
      identify: (pid) => {
        if (dead.has(pid)) return null;
        if (pid === 101) return { pid, pgid: 101, command: 'PetalRCPhotonSentinel' };
        if (pid === 202) return { pid, pgid: 909, command: 'Terminal' };
        return null;
      },
      kill: (pid, signal) => {
        signals.push([pid, signal]);
        if (signal === 'SIGTERM') dead.add(101);
      },
      sleep: () => {},
      log: () => {},
      waitMs: 5,
    });
    const reports = ledger.sweepStaleLeases();
    assert.deepEqual(signals, [[-101, 'SIGTERM']]);
    assert.deepEqual(
      reports.map((report) => [report.pid, report.outcome]),
      [
        [101, 'terminated'],
        [202, 'identity-mismatch'],
        [303, 'already-gone'],
      ]
    );
  });
});

test('startRun carries an unsettled lease forward so a later run retries it', () => {
  withTempLedger((file) => {
    const at = new Date().toISOString();
    writeFileSync(
      file,
      [
        'event\trole\tpid\tpgid\tcwd\tlog\tcommand\tdetail\tat',
        // Survived SIGKILL: unfinished business, must survive the new ledger.
        `ORPHANED\tphoton-sentinel\t505\t505\t/tmp\t\tPetalRCPhotonSentinel\tsurvived SIGKILL\t${at}`,
        // Settled by its own run: must not be carried forward.
        `CLEANED\tphoton-sentinel\t606\t606\t/tmp\t\tPetalRCPhotonSentinel\t\t${at}`,
      ].join('\n') + '\n'
    );
    const ledger = new ProcessLeaseLedger({ file, identify: () => null, kill: () => {}, sleep: () => {}, log: () => {} });
    ledger.startRun();
    const carried = readFileSync(file, 'utf8');
    assert.match(carried, /\t505\t/);
    assert.ok(!carried.includes('\t606\t'), carried);
    // And the next sweep still acts on it.
    assert.deepEqual(
      ledger.sweepStaleLeases().map((report) => [report.pid, report.outcome]),
      [[505, 'already-gone']]
    );
  });
});

test('psIdentity resolves a real process and returns null for a dead one', () => {
  const child = spawn('/bin/sleep', ['30'], { detached: true, stdio: 'ignore' });
  try {
    const identity = psIdentity(child.pid);
    assert.equal(identity?.pid, child.pid);
    assert.equal(identity?.pgid, child.pid, 'a detached child leads its own group');
    assert.equal(identity?.command, 'sleep');
  } finally {
    process.kill(-child.pid, 'SIGKILL');
  }
  assert.equal(psIdentity(2_147_483_600), null);
});

// ---------------------------------------------------------------------------
// The real leak, reproduced: kill the runner mid-run and count survivors.

function groupMembers(pgid) {
  const result = spawnSync('/bin/ps', ['-axo', 'pid=,pgid='], { encoding: 'utf8' });
  return result.stdout
    .split('\n')
    .map((line) => line.trim().split(/\s+/).map(Number))
    .filter(([pid, group]) => Number.isInteger(pid) && group === pgid)
    .map(([pid]) => pid);
}

function waitForPidExit(pid, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!psIdentity(pid)) return true;
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 25);
  }
  return false;
}

function withTempLedgerAsync(fn) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'petal-lease-test-'));
  return fn(path.join(dir, 'leases.tsv'), dir).finally(() =>
    rmSync(dir, { recursive: true, force: true })
  );
}

test('SIGTERM to the runner tears down its whole spawned group -- the actual leak', async () => {
  await withTempLedgerAsync(async (file, dir) => {
    const runnerPath = path.join(dir, 'runner.mjs');
    // Stands in for remote-control-scenario.mjs: it spawns a detached child
    // that itself has a child (the sentinel's own descendants), registers the
    // lease, installs the traps, and then blocks the way a live suite does.
    writeFileSync(
      runnerPath,
      `import { spawn } from 'node:child_process';
import { ProcessLeaseLedger } from ${JSON.stringify(ledgerModulePath)};

const ledger = new ProcessLeaseLedger({ file: ${JSON.stringify(file)} });
ledger.startRun();
const child = spawn('/bin/sh', ['-c', 'sleep 120 & sleep 120'], { detached: true, stdio: 'ignore' });
ledger.register('fixture-sentinel', child, { command: 'sh' });
ledger.installSignalTraps();
console.log('CHILD_PID ' + child.pid);
setInterval(() => {}, 1000);
`
    );

    const runner = spawn(process.execPath, [runnerPath], { stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    runner.stdout.setEncoding('utf8');
    runner.stderr.setEncoding('utf8');
    runner.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    runner.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    const runnerExited = new Promise((resolve) => runner.once('exit', (code, signal) => resolve({ code, signal })));

    // Must be an ASYNC wait: a synchronous Atomics.wait spin blocks the event
    // loop, so the 'data' event never fires and stdout stays empty forever.
    const announceDeadline = Date.now() + 10_000;
    while (!stdout.includes('CHILD_PID') && Date.now() < announceDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    const childPid = Number(stdout.match(/CHILD_PID (\d+)/)?.[1]);
    assert.ok(Number.isInteger(childPid), `runner never reported a child pid: ${stdout}${stderr}`);

    const before = groupMembers(childPid);
    assert.ok(before.length >= 2, `expected a multi-process group, saw ${JSON.stringify(before)}`);

    try {
      // BY PID -- never `pkill -f`, which matches the killer's own command line.
      process.kill(runner.pid, 'SIGTERM');
      const exit = await Promise.race([
        runnerExited,
        new Promise((resolve) => setTimeout(() => resolve(null), 15_000)),
      ]);
      assert.ok(exit, 'runner did not exit after SIGTERM');
      assert.ok(waitForPidExit(childPid, 10_000), 'the leased child survived the runner');
      assert.deepEqual(groupMembers(childPid), [], 'process group members survived the runner');
      assert.ok(ledgerEvents(file).includes('CLEANED'), readFileSync(file, 'utf8'));
    } finally {
      for (const pid of [runner.pid, ...before]) {
        try {
          process.kill(pid, 'SIGKILL');
        } catch {
          // Already gone -- the point of the test.
        }
      }
    }
  });
});
