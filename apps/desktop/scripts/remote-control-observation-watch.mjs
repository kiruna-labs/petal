// High-resolution target observation for the remote-control latency gate.
//
// The old observation polled TextEdit with one `osascript` PROCESS per read
// every 250ms, so every latency sample was quantized to ~250ms plus a ~50ms
// process spawn: a 30ms input and a 280ms input could report the same number.
// This arms ONE in-process JXA watcher BEFORE the action is sent. It reads the
// same oracle (TextEdit's document text over Apple Events, or the general
// pasteboard) every WATCH_INTERVAL_MS and prints the wall-clock instant it
// first saw the expected value, so the sample is that instant minus the
// action's start -- no poll quantization, no spawn cost inside the timing.
//
// It is a TIMER, not the verdict: callers still run their original TextEdit /
// clipboard assertion afterwards, and a watcher that cannot start returns null
// so the caller falls back to polling (reported as method `poll`).
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

// JXA's own delay()/NSThread sleeps are coalesced to ~70ms for a background
// osascript (measured: 14 iterations/s asking for 100), so the loop waits by
// spinning on the clock between reads. Reads stay at this interval -- TextEdit
// is never hammered faster than ~65 Apple Events/s, and only while armed.
export const WATCH_INTERVAL_MS = 15;
const READY_TIMEOUT_MS = 5000;

export const WATCH_SCRIPT = String.raw`
ObjC.import('AppKit');
function nowMs() { return $.NSDate.date.timeIntervalSince1970 * 1000; }
function run(argv) {
  const mode = argv[0];
  const needle = argv[1];
  const docMarker = argv[2];
  const intervalMs = Number(argv[3]);
  const timeoutMs = Number(argv[4]);
  let read;
  if (mode === 'clipboard') {
    read = () => ObjC.unwrap($.NSPasteboard.generalPasteboard.stringForType($.NSPasteboardTypeString)) || '';
  } else {
    const te = Application('TextEdit');
    read = () => {
      const doc = docMarker ? te.documents.whose({ name: { _contains: docMarker } })[0] : te.documents[0];
      return doc.text();
    };
  }
  const matches = (text) => (mode === 'clipboard' ? text === needle : text.includes(needle));
  let lastError = '';
  const sample = () => {
    try { return matches(read()); } catch (error) { lastError = String(error); return false; }
  };
  console.log('READY ' + nowMs().toFixed(1) + ' present=' + sample());
  const deadline = nowMs() + timeoutMs;
  let next = nowMs();
  while (nowMs() < deadline) {
    next += intervalMs;
    while (nowMs() < next) {}
    if (sample()) { console.log('SEEN ' + nowMs().toFixed(1)); return; }
  }
  console.log('TIMEOUT ' + JSON.stringify(lastError));
}
`;

// Protocol: one line per state, on stderr (JXA console.log).
export function parseWatchLine(line) {
  const text = String(line ?? '').trim();
  let match;
  if ((match = /^READY (\d+(?:\.\d+)?) present=(true|false)$/.exec(text))) {
    return { type: 'ready', atMs: Number(match[1]), present: match[2] === 'true' };
  }
  if ((match = /^SEEN (\d+(?:\.\d+)?)$/.exec(text))) {
    return { type: 'seen', atMs: Number(match[1]) };
  }
  if ((match = /^TIMEOUT(?: (.*))?$/.exec(text))) {
    return { type: 'timeout', lastError: match[1] ?? '' };
  }
  return null;
}

let scriptPath = null;
function watchScriptPath() {
  if (scriptPath && fs.existsSync(scriptPath)) return scriptPath;
  scriptPath = path.join(os.tmpdir(), `petal-rc-observation-watch-${process.pid}.js`);
  fs.writeFileSync(scriptPath, WATCH_SCRIPT, 'utf8');
  return scriptPath;
}

// Resolves once the watcher has taken its first read, to
//   { waitSeen(): Promise<number|null>, cancel() }
// or to null when no watcher could be armed (caller falls back to polling).
// Rejects only when the needle is ALREADY present before the action -- that
// is a re-arm bug in the case, and timing it would clock a meaningless ~0ms.
export function armObservationWatch({
  mode,
  needle,
  docMarker = '',
  timeoutMs,
  intervalMs = WATCH_INTERVAL_MS,
  spawnImpl = spawn,
  readyTimeoutMs = READY_TIMEOUT_MS,
  log = () => {},
}) {
  if (mode !== 'textedit' && mode !== 'clipboard') throw new Error(`unknown watch mode ${mode}`);
  return new Promise((resolve, reject) => {
    let child;
    try {
      child = spawnImpl(
        'osascript',
        ['-l', 'JavaScript', watchScriptPath(), mode, needle, docMarker ?? '', String(intervalMs), String(timeoutMs)],
        { stdio: ['ignore', 'ignore', 'pipe'] }
      );
    } catch (error) {
      log(`observation watch unavailable: ${error.message}`);
      resolve(null);
      return;
    }
    let armed = false;
    let settledSeen = false;
    let seenResolve;
    const seen = new Promise((r) => { seenResolve = r; });
    const finishSeen = (value) => {
      if (settledSeen) return;
      settledSeen = true;
      seenResolve(value);
    };
    const kill = () => { try { child.kill('SIGKILL'); } catch { /* already gone */ } };
    const readyTimer = setTimeout(() => {
      if (armed) return;
      log(`observation watch did not report READY within ${readyTimeoutMs}ms`);
      kill();
      resolve(null);
    }, readyTimeoutMs);
    let buffer = '';
    child.stderr?.setEncoding?.('utf8');
    child.stderr?.on('data', (chunk) => {
      buffer += chunk;
      let newline;
      while ((newline = buffer.indexOf('\n')) >= 0) {
        const parsed = parseWatchLine(buffer.slice(0, newline));
        buffer = buffer.slice(newline + 1);
        if (!parsed) continue;
        if (parsed.type === 'ready' && !armed) {
          armed = true;
          clearTimeout(readyTimer);
          if (parsed.present) {
            kill();
            reject(new Error(`observation target already shows ${JSON.stringify(needle)} before the action; the case did not re-arm it`));
            return;
          }
          resolve({ waitSeen: () => seen, cancel: () => { kill(); finishSeen(null); } });
        } else if (parsed.type === 'seen') {
          finishSeen(parsed.atMs);
        } else if (parsed.type === 'timeout') {
          if (parsed.lastError) log(`observation watch timed out; last read error ${parsed.lastError}`);
          finishSeen(null);
        }
      }
    });
    child.on('error', (error) => {
      clearTimeout(readyTimer);
      log(`observation watch failed: ${error.message}`);
      finishSeen(null);
      if (!armed) resolve(null);
    });
    child.on('close', () => {
      clearTimeout(readyTimer);
      finishSeen(null);
      if (!armed) resolve(null);
    });
  });
}
