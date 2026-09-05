#!/usr/bin/env node
// Score a PETAL_TRACE_PANEL_GEOMETRY=1 run against #416's acceptance
// criterion, straight from petal.log.
//
// The criterion is deliberately NOT "the panel ended up the right size" --
// five fixes in this class shipped green on end-state checks and failed live.
// It is a statement about the ORDERED writer stream:
//
//   ZERO `programmatic-source-driven` writes carrying `gesture=idle`
//   while `drag` writes are still arriving on both sides of them.
//
// A `drag` line after a source-driven line is proof the pointer was still
// down when that source-driven write landed -- regardless of what the app
// believed its own gesture bit was. That is exactly the #416 signature:
//
//   seq=39 reason=drag                       w=925  gesture=active
//   seq=41 reason=drag                       w=884  gesture=idle   <- bit lost
//   seq=48 reason=programmatic-source-driven w=720  gesture=idle   <- guard opens
//   seq=50 reason=drag                       w=867  gesture=idle   <- yanks back
//
// Usage: acceptance-416-trace.mjs <petal.log> [--from-line N]

import fs from 'node:fs';
import process from 'node:process';

const [logPath] = process.argv.slice(2);
const fromLine = process.argv.includes('--from-line')
  ? Number(process.argv[process.argv.indexOf('--from-line') + 1])
  : 0;

if (!logPath) {
  console.error('usage: acceptance-416-trace.mjs <petal.log> [--from-line N]');
  process.exit(2);
}

// e.g. `PANELGEO seq=48 t=1785228888790 window=766811
//       reason=programmatic-source-driven w=720.00 h=494.00 gesture=idle`
const TRACE = /PANELGEO seq=(\d+) t=(\d+) window=(\d+) reason=(\S+) w=([\d.]+) h=[\d.]+ gesture=(\S+)/;

const lines = fs.readFileSync(logPath, 'utf8').split('\n').slice(fromLine);
const events = [];
for (const line of lines) {
  const m = TRACE.exec(line);
  if (!m) continue;
  events.push({
    seq: Number(m[1]),
    tMs: Number(m[2]),
    window: m[3],
    reason: m[4],
    w: Number(m[5]),
    gesture: m[6],
    raw: line.trim(),
  });
}

// Per window, so two panels cannot alias into one another's gesture stream.
const byWindow = new Map();
for (const event of events) {
  if (!byWindow.has(event.window)) byWindow.set(event.window, []);
  byWindow.get(event.window).push(event);
}

const violations = [];
let sourceDrivenTotal = 0;
let dragTotal = 0;

for (const [windowId, stream] of byWindow) {
  for (let i = 0; i < stream.length; i += 1) {
    const event = stream[i];
    if (event.reason === 'drag') dragTotal += 1;
    if (!event.reason.startsWith('programmatic-source-driven')) continue;
    sourceDrivenTotal += 1;

    // Pointer-still-down evidence, delimited by `drag-final` -- the app's own
    // pointer-up marker.
    //
    // "Some drag line appears later in the log" is NOT sufficient: across a
    // multi-trial run the next drag can belong to the NEXT trial, seconds
    // later, which would score an entirely legitimate between-trials resize
    // as a violation. The gesture must be bounded on both sides:
    //   - the nearest preceding drag-ish event is a `drag`, not a `drag-final`
    //   - a further `drag` follows BEFORE the next `drag-final`
    // i.e. the write lands strictly inside one pointer-down..pointer-up span.
    const isDragish = (e) => e.reason === 'drag' || e.reason === 'drag-final';
    const before = stream.slice(0, i).filter(isDragish).at(-1);
    if (!before || before.reason !== 'drag') continue;

    const after = stream.slice(i + 1).find(isDragish);
    if (!after || after.reason !== 'drag') continue;

    // Belt and braces for a LOST drag-final: a real gesture's next pointer
    // move lands within a fraction of a second, never many seconds later.
    const gapMs = after.tMs - event.tMs;
    if (gapMs > 2000) continue;

    violations.push({
      windowId,
      seq: event.seq,
      w: event.w,
      gesture: event.gesture,
      precedingDragSeq: before.seq,
      nextDragSeq: after.seq,
      nextDragAfterMs: gapMs,
      raw: event.raw,
    });
  }
}

const report = {
  logPath,
  fromLine,
  traceEnv: process.env.PETAL_TRACE_PANEL_GEOMETRY ?? null,
  windows: [...byWindow.keys()],
  totals: { traceEvents: events.length, dragWrites: dragTotal, sourceDrivenWrites: sourceDrivenTotal },
  midGestureSourceDrivenWrites: violations.length,
  violations: violations.slice(0, 20),
  verdict: null,
};

const missingEvidence = [];
if (process.env.PETAL_TRACE_PANEL_GEOMETRY !== '1') {
  missingEvidence.push('PETAL_TRACE_PANEL_GEOMETRY=1 was not confirmed');
}
if (events.length === 0) missingEvidence.push('no PANELGEO trace events matched');
if (dragTotal === 0) missingEvidence.push('no drag write was observed');

// Zero-evidence control (#622), run against an empty log:
// `PETAL_TRACE_PANEL_GEOMETRY=1 node acceptance-416-trace.mjs empty.log`
// emits `"verdict": "INSUFFICIENT DATA"` and exits 1, never PASS.
report.verdict = missingEvidence.length > 0
  ? 'INSUFFICIENT DATA'
  : violations.length === 0 ? 'PASS' : 'FAIL';
if (missingEvidence.length > 0) report.insufficientData = missingEvidence;

console.log(JSON.stringify(report, null, 2));
process.exit(report.verdict === 'PASS' ? 0 : 1);
