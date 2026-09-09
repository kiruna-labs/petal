#!/usr/bin/env node
// Evidence capture for a wedged live-harness run (#102).
//
// The defect this exists for: on 2026-09-08 the `live-loopback` job sat in
// `Run live loopback harness` for 81 minutes (normal is ~2) holding the only
// self-hosted Tart VM, a release e2e gate queued behind it, and that release
// lost its version number to the queue watchdog. The run produced NO evidence
// about the wedge at all -- `petal-dev.log` stopped dead mid-stream, and the
// harness log ended at its `==> Live loopback` header.
//
// A bounded failure is diagnosable and frees the VM; an unbounded one is
// neither. Bounding alone would still have left the next reader with nothing
// to work from, so every timeout path calls this first: a `sample` backtrace
// of every live `desktop` process (which is what actually distinguishes
// "deadlocked" from "alive but no longer answering the socket") plus the tail
// of the app log, written where the workflow's artifact upload will find them.
//
// Never throws. An evidence-capture failure must not mask, replace, or delay
// the wedge report it is attached to.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';

/// How long `sample` profiles each wedged process. Long enough to show a
/// spinning thread as spinning, short enough that capturing evidence for a
/// few processes cannot itself become the thing that holds the runner.
const SAMPLE_SECONDS = 3;
const SAMPLE_EXEC_TIMEOUT_MS = 20_000;
/// Tail of the app log kept beside the backtrace. The interesting part of a
/// wedge is always the last thing the app managed to say.
const LOG_TAIL_BYTES = 256 * 1024;

function safe(fn, fallback = null) {
  try {
    return fn();
  } catch {
    return fallback;
  }
}

/// Where evidence is written. The workflow points this at the directory it
/// uploads as artifacts; a local run falls back to the temp dir.
export function wedgeEvidenceDir() {
  return process.env.PETAL_WEDGE_EVIDENCE_DIR || os.tmpdir();
}

/// pids of every running `desktop` (the Petal dev binary).
export function desktopPids() {
  const out = safe(
    () => execFileSync('pgrep', ['-x', 'desktop'], { encoding: 'utf8', timeout: 5_000 }),
    ''
  );
  return (out ?? '')
    .split(/\s+/)
    .map((s) => s.trim())
    .filter((s) => /^\d+$/.test(s));
}

/// Captures what a reader needs to act on `reason`. Returns the list of files
/// written, and prints a `#`-prefixed line per file so the wedge is visible in
/// the harness log itself, not only in the artifacts.
export function captureWedgeEvidence(reason, options = {}) {
  const dir = options.dir ?? wedgeEvidenceDir();
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const prefix = path.join(dir, `wedge-${stamp}`);
  const written = [];

  safe(() => fs.mkdirSync(dir, { recursive: true }));

  const pids = options.pids ?? desktopPids();
  const header = [
    `# harness wedge evidence (#102)`,
    `reason: ${reason}`,
    `captured: ${new Date().toISOString()}`,
    `desktop pids: ${pids.length ? pids.join(', ') : '(none running)'}`,
    '',
  ];
  for (const pid of pids) {
    const ps = safe(
      () =>
        execFileSync('ps', ['-o', 'pid,stat,%cpu,%mem,etime,command', '-p', pid], {
          encoding: 'utf8',
          timeout: 5_000,
        }),
      `(ps failed for ${pid})\n`
    );
    header.push(ps.trimEnd(), '');
  }
  const reasonFile = `${prefix}-reason.txt`;
  if (safe(() => fs.writeFileSync(reasonFile, `${header.join('\n')}\n`), false) !== false) {
    written.push(reasonFile);
  }

  // The backtrace is the whole point: it is what says whether the process is
  // deadlocked on a lock, spinning, or perfectly healthy and simply no longer
  // answering the autotest socket. Without it a wedge is unfalsifiable.
  for (const pid of pids) {
    const file = `${prefix}-sample-${pid}.txt`;
    const ok = safe(() => {
      execFileSync('sample', [pid, String(SAMPLE_SECONDS), '-file', file], {
        encoding: 'utf8',
        timeout: SAMPLE_EXEC_TIMEOUT_MS,
        stdio: 'ignore',
      });
      return true;
    }, false);
    if (ok && safe(() => fs.statSync(file).size > 0, false)) written.push(file);
  }

  const devLog = options.devLog ?? process.env.PETAL_DEV_LOG;
  if (devLog) {
    const tail = safe(() => {
      const { size } = fs.statSync(devLog);
      const start = Math.max(0, size - LOG_TAIL_BYTES);
      const fd = fs.openSync(devLog, 'r');
      try {
        const buf = Buffer.alloc(size - start);
        fs.readSync(fd, buf, 0, buf.length, start);
        return buf.toString('utf8');
      } finally {
        fs.closeSync(fd);
      }
    });
    if (tail !== null) {
      const file = `${prefix}-petal-dev-tail.log`;
      if (safe(() => fs.writeFileSync(file, tail), false) !== false) written.push(file);
    }
  }

  console.log(`# WEDGE EVIDENCE (#102): ${reason}`);
  if (!pids.length) {
    console.log('# WEDGE EVIDENCE: no `desktop` process was running -- the app died rather than hung');
  }
  for (const file of written) console.log(`# WEDGE EVIDENCE: wrote ${file}`);
  if (!written.length) console.log('# WEDGE EVIDENCE: nothing could be captured');
  return written;
}
