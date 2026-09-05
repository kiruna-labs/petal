// On-disk process-lease ledger for the remote-control harness (plan Item 7).
//
// The leak this closes: neither remote-control-scenario.mjs nor
// remote-control-local-loopback.mjs registered ANY process.on('exit'|'SIGINT'|
// 'SIGTERM') handler, so an external `timeout` kill -- the normal way these
// runs end -- killed Node without running any `finally`, orphaning the AppKit
// photon sentinel. `stopPhotonSentinel` also sent one SIGTERM and immediately
// nulled its handle: no wait, no SIGKILL escalation, no verification. And no
// PID was recorded anywhere, so a later run had nothing to clean up FROM.
//
// This is the remote-control sibling of run-issue613-presentation-latency.mjs's
// LeaseRegistry and keeps its TSV schema and its two load-bearing ideas: spawn
// into its own process group, and re-identify by `ps` before signalling so a
// recycled PID is never signalled. It deliberately drops that registry's
// deadline timers, readiness attestation and descendant classification, which
// the sentinel does not need, and it is fully SYNCHRONOUS -- a process.on
// ('exit') handler cannot await anything, and that handler is the whole point.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { spawnSync } from 'node:child_process';

export const DEFAULT_LEASE_LEDGER_PATH = path.join(os.tmpdir(), 'petal-rc-process-leases.tsv');

const LEDGER_COLUMNS = ['event', 'role', 'pid', 'pgid', 'cwd', 'log', 'command', 'detail', 'at'];

// Events after which a ledger row needs no further action from a later run.
const SETTLED_EVENTS = new Set([
  'CLEANED',
  'ALREADY_GONE',
  'IDENTITY_MISMATCH',
  'SWEPT',
  'SWEPT_ALREADY_GONE',
  'SWEPT_IDENTITY_MISMATCH',
]);
// Deliberately NOT settled: a process that survived SIGKILL must be retried by
// the next run, so ORPHANED rows are carried across ledgers by startRun().

export function sleepSynchronously(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

// `null` means "no process with that pid". Combined with the recorded pgid and
// command name, that is what makes signalling a recycled PID impossible.
export function psIdentity(pid) {
  if (!Number.isInteger(pid) || pid <= 1) return null;
  const result = spawnSync('/bin/ps', ['-p', String(pid), '-o', 'pgid=,comm='], { encoding: 'utf8' });
  if (result.error || result.status !== 0) return null;
  const line = (result.stdout ?? '').trim();
  const match = line.match(/^(\d+)\s+(.*?)$/);
  if (!match) return null;
  return { pid, pgid: Number(match[1]), command: path.basename(match[2].trim()) };
}

function sameProcess(identity, entry) {
  return !!identity && identity.pgid === entry.pgid && identity.command === entry.command;
}

export class ProcessLeaseLedger {
  constructor({
    file = DEFAULT_LEASE_LEDGER_PATH,
    identify = psIdentity,
    kill = (pid, signal) => process.kill(pid, signal),
    sleep = sleepSynchronously,
    log = (message) => console.log(message),
    waitMs = 2000,
    pollMs = 25,
  } = {}) {
    this.file = file;
    this.identify = identify;
    this.kill = kill;
    this.sleep = sleep;
    this.log = log;
    this.waitMs = waitMs;
    this.pollMs = pollMs;
    this.entries = [];
    this.trapsInstalled = false;
  }

  record(event, entry, detail = '') {
    const row = [
      event,
      entry.role,
      entry.pid,
      entry.pgid,
      entry.cwd ?? '',
      entry.log ?? '',
      entry.command,
      detail,
      new Date().toISOString(),
    ];
    fs.appendFileSync(this.file, `${row.join('\t')}\n`);
  }

  readRows() {
    let raw;
    try {
      raw = fs.readFileSync(this.file, 'utf8');
    } catch {
      return [];
    }
    return raw
      .split('\n')
      .filter((line) => line && !line.startsWith('event\t'))
      .map((line) => {
        const cells = line.split('\t');
        return Object.fromEntries(LEDGER_COLUMNS.map((column, index) => [column, cells[index] ?? '']));
      })
      .filter((row) => /^\d+$/.test(row.pid) && /^\d+$/.test(row.pgid));
  }

  // Re-identify every lease a previous run left unsettled and clean it up.
  // Returns one report per lease so the caller can print exactly what it
  // killed -- the user asked for every PID to be reported.
  sweepStaleLeases() {
    const latest = new Map();
    for (const row of this.readRows()) {
      latest.set(`${row.pid}:${row.pgid}:${row.command}`, row);
    }
    const reports = [];
    for (const row of latest.values()) {
      if (SETTLED_EVENTS.has(row.event)) continue;
      const entry = {
        role: row.role,
        pid: Number(row.pid),
        pgid: Number(row.pgid),
        cwd: row.cwd,
        log: row.log,
        command: row.command,
        groupLeader: Number(row.pid) === Number(row.pgid),
      };
      const identity = this.identify(entry.pid);
      if (!identity) {
        this.record('SWEPT_ALREADY_GONE', entry);
        reports.push({ ...entry, outcome: 'already-gone' });
        continue;
      }
      if (!sameProcess(identity, entry)) {
        // The PID was recycled by an unrelated process. Never signal it.
        this.record(
          'SWEPT_IDENTITY_MISMATCH',
          entry,
          `observed=pgid:${identity.pgid},command:${identity.command}`
        );
        reports.push({ ...entry, outcome: 'identity-mismatch' });
        continue;
      }
      const outcome = this.terminate(entry, 'SWEPT');
      reports.push({ ...entry, outcome });
    }
    return reports;
  }

  // Start a fresh ledger for this run. Call AFTER sweepStaleLeases(). Rows that
  // are still unsettled (in practice: ORPHANED, survived SIGKILL) are carried
  // forward so a later run tries again rather than forgetting them.
  startRun() {
    const latest = new Map();
    for (const row of this.readRows()) {
      latest.set(`${row.pid}:${row.pgid}:${row.command}`, row);
    }
    const carried = [...latest.values()].filter((row) => !SETTLED_EVENTS.has(row.event));
    const lines = [LEDGER_COLUMNS.join('\t')];
    for (const row of carried) {
      lines.push(LEDGER_COLUMNS.map((column) => row[column]).join('\t'));
    }
    fs.writeFileSync(this.file, `${lines.join('\n')}\n`);
  }

  register(role, child, { command, cwd = process.cwd(), log = '' } = {}) {
    if (!Number.isInteger(child?.pid)) throw new Error(`${role} started without a pid`);
    const identity = this.identify(child.pid);
    if (!identity) throw new Error(`${role} pid ${child.pid} was not observed after spawn`);
    const entry = {
      role,
      child,
      pid: child.pid,
      pgid: identity.pgid,
      cwd,
      log,
      command: command ?? identity.command,
      groupLeader: identity.pgid === child.pid,
    };
    child.once('exit', () => {
      entry.childExited = true;
    });
    this.entries.push(entry);
    this.record('STARTED', entry, `groupLeader=${entry.groupLeader}`);
    this.record(
      'OBSERVED_LEADER',
      entry,
      `pid=${identity.pid};pgid=${identity.pgid};command=${identity.command}`
    );
    if (!entry.groupLeader) {
      // Not spawned detached: signalling -pgid here would hit this process and
      // its siblings. Fall back to signalling the single pid.
      this.record('NOT_GROUP_LEADER', entry, `pgid=${entry.pgid};self=${process.pid}`);
    }
    return entry;
  }

  signalTarget(entry) {
    return entry.groupLeader ? -entry.pgid : entry.pid;
  }

  send(entry, signal) {
    try {
      this.kill(this.signalTarget(entry), signal);
      return true;
    } catch (error) {
      if (error?.code === 'ESRCH') return false;
      throw error;
    }
  }

  waitForExit(entry) {
    const deadline = Date.now() + this.waitMs;
    for (;;) {
      const identity = this.identify(entry.pid);
      if (!identity || !sameProcess(identity, entry)) return true;
      if (Date.now() >= deadline) return false;
      this.sleep(this.pollMs);
    }
  }

  // SIGTERM, wait, verify; escalate to SIGKILL, wait, verify again. An app
  // stuck in a modal or AX prompt used to survive the single unverified
  // SIGTERM silently.
  terminate(entry, cleanedEvent = 'CLEANED') {
    this.record('TERMINATING', entry, `signal=SIGTERM;target=${this.signalTarget(entry)}`);
    this.send(entry, 'SIGTERM');
    if (this.waitForExit(entry)) {
      this.record(cleanedEvent, entry, 'signal=SIGTERM');
      return 'terminated';
    }
    this.record('KILLING', entry, `signal=SIGKILL;target=${this.signalTarget(entry)}`);
    this.send(entry, 'SIGKILL');
    if (this.waitForExit(entry)) {
      this.record(cleanedEvent, entry, 'signal=SIGKILL');
      return 'killed';
    }
    // Deliberately NOT recorded as cleaned: a later run must try again.
    this.record('ORPHANED', entry, 'survived SIGKILL');
    this.log(`# WARN process lease ${entry.role} pid=${entry.pid} survived SIGKILL`);
    return 'orphaned';
  }

  release(entry) {
    if (!entry || entry.released) return 'already-released';
    entry.released = true;
    const identity = this.identify(entry.pid);
    if (!identity) {
      this.record('ALREADY_GONE', entry);
      return 'already-gone';
    }
    if (!sameProcess(identity, entry)) {
      this.record(
        'IDENTITY_MISMATCH',
        entry,
        `observed=pgid:${identity.pgid},command:${identity.command}`
      );
      return 'identity-mismatch';
    }
    return this.terminate(entry);
  }

  releaseAll() {
    const outcomes = [];
    for (const entry of this.entries) {
      try {
        outcomes.push({ role: entry.role, pid: entry.pid, outcome: this.release(entry) });
      } catch (error) {
        // Teardown must never throw out of a signal or exit handler.
        this.log(`# WARN process lease ${entry.role} pid=${entry.pid} teardown failed: ${error.message}`);
      }
    }
    return outcomes;
  }

  // The signal path is the leak. `finally` blocks do not run when Node is
  // killed; these handlers do.
  installSignalTraps({ exit = (code) => process.exit(code), on = (event, handler) => process.on(event, handler) } = {}) {
    if (this.trapsInstalled) return;
    this.trapsInstalled = true;
    for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
      on(signal, () => {
        this.log(`# ${signal} received; releasing ${this.entries.length} process lease(s)`);
        this.releaseAll();
        // A killed run proved nothing: 2 is this harness's "no result" code.
        exit(2);
      });
    }
    on('exit', () => {
      this.releaseAll();
    });
  }
}
