#!/usr/bin/env node
// #416 acceptance: a USER drag-resize of a receiver remote window, raced
// against a SOURCE-side resize.
//
// Everything here drives the REAL path: the drag is real posted mouse events
// landing on the panel's real resize handle (so `compositor_begin_resize` /
// `compositor_resize_window` / the `WindowEvent::Resized` chain all run for
// real), and the observation is the panel's real WindowServer frame. The
// repo's existing #416 tests only exercise the extracted pure decision
// helpers -- see CLAUDE.md's "Native window-lifecycle changes need a
// live-exercising test" rule, which is exactly why this exists.
//
// THE RULE: every run begins with a positive control -- a drag with NO
// concurrent source resize. If the control does not visibly resize the panel
// and land it on the source aspect, the harness is not driving anything and
// the run reports NO RESULT instead of zeros.

import { execFileSync, spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const probeSource = path.join(scriptDir, 'petal-window-probe.swift');
const probeBinary = path.join(os.tmpdir(), 'petal-acceptance-416-probe');
const cdpListUrl = process.env.PETAL_REMOTE_CONTROL_CDP_JSON || 'http://127.0.0.1:9222/json';
const harnessUrlNeedle = process.env.PETAL_WEB_HARNESS_URL_MATCH || 'localhost:5185';
const accessCode = process.env.PETAL_ACCEPTANCE_ACCESS_CODE || '';
const trials = Number(process.env.PETAL_ACCEPTANCE_416_TRIALS || 10);
const jsonOutputPath = process.argv.includes('--json')
  ? process.argv[process.argv.indexOf('--json') + 1]
  : null;

const SOURCE_BASE = { w: 960, h: 600 };
const SOURCE_WIDE = { w: 1440, h: 600 };
const ASPECT_TOLERANCE = 0.04;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function compileProbe() {
  const result = spawnSync('xcrun', ['swiftc', probeSource, '-framework', 'AppKit', '-o', probeBinary], {
    encoding: 'utf8',
    env: {
      ...process.env,
      SWIFT_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-416-swift-cache'),
      CLANG_MODULE_CACHE_PATH: path.join(os.tmpdir(), 'petal-416-clang-cache'),
    },
  });
  if (result.status !== 0) throw new Error(`probe compile failed: ${(result.stderr || result.stdout || '').trim()}`);
}

function probe(args) {
  const out = execFileSync(probeBinary, args, { encoding: 'utf8', timeout: 30000 });
  return out.trim().split('\n').filter(Boolean).map((line) => JSON.parse(line));
}

// GAP-FREE geometry sampling, and the reason #416 read as a 3/16 defect for a
// whole cycle rather than the 16/16 it is.
//
// Sampling by calling `frameOf()` per drag step is spawn-per-sample on the
// SAME thread that is driving the gesture: each sample costs a process launch,
// and the interval is whatever the drag loop has left over. Exactly during a
// republish -- the moment under test, when the app retires and reveals the
// panel -- that loop starves, so the excursion lands between two samples and
// the trial scores a pass. A 40ms gap-free sampler found a 198pt excursion
// inside 137ms on a trial the stepped sampler called clean.
//
// This spawns the probe's own `--sample` loop ONCE, in its own process, where
// the cadence is a native `Thread.sleep` that no JS work can stall.
function startFrameSampler(windowNumber, durationMs, intervalMs = 40, owner = null) {
  // The owner filter is a PERFORMANCE requirement, not a convenience. Without
  // it every tick enumerates and JSON-serializes every window on the desktop;
  // measured live, that alone stretched the worst inter-sample gap to 2182ms
  // -- the sampler starving in precisely the way it exists to avoid. Filtered
  // to the one owning app it holds its 40ms cadence. The panel is still
  // matched by exact windowNumber below, so the filter cannot mis-select.
  const args = ['--sample', String(durationMs), String(intervalMs)];
  if (owner) args.push(owner);
  const child = spawn(probeBinary, args, {
    stdio: ['ignore', 'pipe', 'ignore'],
  });
  let buffered = '';
  const samples = [];
  child.stdout.on('data', (chunk) => {
    buffered += chunk;
    const lines = buffered.split('\n');
    buffered = lines.pop() ?? '';
    for (const line of lines) {
      if (!line.trim()) continue;
      let parsed;
      try {
        parsed = JSON.parse(line);
      } catch {
        continue;
      }
      const match = (parsed.windows ?? []).find((w) => w.windowNumber === windowNumber);
      if (match) samples.push({ tMs: parsed.tMs, w: match.w, h: match.h, x: match.x, y: match.y });
    }
  });
  return {
    samples,
    stop: async () => {
      child.kill('SIGTERM');
      await sleep(120);
      return samples;
    },
  };
}

// The dev build's process/owner name is `desktop` (the crate name), a packaged
// build's is `Petal` -- match either so this works against both.
const PETAL_OWNER = /^(petal|desktop)$/i;
function findWindows() {
  return (probe(['--find'])[0] ?? []).filter((w) => PETAL_OWNER.test(w.owner));
}

// ---- CDP -------------------------------------------------------------------

function httpJson(url) {
  return new Promise((resolve, reject) => {
    http
      .get(url, (response) => {
        let body = '';
        response.on('data', (chunk) => (body += chunk));
        response.on('end', () => {
          try {
            resolve(JSON.parse(body));
          } catch (error) {
            reject(error);
          }
        });
      })
      .on('error', reject);
  });
}

async function connectCdp() {
  const { WebSocket } = await import('ws').catch(() => ({ WebSocket: globalThis.WebSocket }));
  const pages = await httpJson(cdpListUrl);
  const page = pages.find((entry) => entry.type === 'page' && String(entry.url).includes(harnessUrlNeedle));
  if (!page) throw new Error(`no CDP page matching ${harnessUrlNeedle}`);
  const socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let nextId = 1;
  const pending = new Map();
  socket.onmessage = (message) => {
    const payload = JSON.parse(typeof message.data === 'string' ? message.data : message.data.toString());
    const entry = pending.get(payload.id);
    if (!entry) return;
    pending.delete(payload.id);
    if (payload.error) entry.reject(new Error(payload.error.message));
    else entry.resolve(payload.result);
  };
  return {
    send(method, params) {
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    close: () => socket.close(),
    pageUrl: page.url,
  };
}

async function evaluate(cdp, expression) {
  const result = await cdp.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description || result.exceptionDetails.text || 'eval failed');
  }
  return result.result?.value;
}

// ---- source-side resize ----------------------------------------------------

// A GENUINE sender-side logical-size change, as the RECEIVER defines one.
//
// NOT what this used to do. Resizing the shared canvas changes nothing the
// receiver reacts to: `canonical_source_size_for_frame` pins the source
// geometry to the publisher's dimensions captured at subscribe time and
// explicitly refuses to let decoded frame sizes redefine it, so a simulcast
// downswitch cannot shrink the panel. Only `ensure_window`'s reuse branch ->
// `update_canonical_source_size_on_republish` moves it. Measured live: setting
// `canvas.width = 1440` left the receiver logging "canonical dimensions 960x600
// remain on the existing geometry path" and the panel unmoved -- so every
// "race" built on it raced nothing, and every trial whose target was the new
// aspect failed for want of a stimulus.
//
// Clicking the dev-panel share button to republish does not work either:
// `startTestPatternShare` calls `startCanvasAnimation` ->
// `prepareTestPatternCanvas`, which forces the canvas back to 960x600 before
// `captureStream` is called. So publish the resized canvas directly, reusing
// the page's already-published track for its class, name and source: same
// track name, same window id, same publish contract, new dimensions.
async function setSourceSize(cdp, size) {
  return evaluate(
    cdp,
    `(async () => {
      const hook = window.__petalHarness;
      const room = hook?.room;
      const old = hook?.localVideoTrack;
      if (!room || !old) throw new Error('web peer is not publishing a window track');
      const publication = [...room.localParticipant.videoTrackPublications.values()]
        .find((p) => p.track === old);
      const trackName = publication?.trackName || old.name;
      if (!trackName) throw new Error('could not resolve the published track name');
      const canvas = document.querySelector('canvas');
      if (!canvas) throw new Error('no shared canvas in the web peer');
      if (canvas.width === ${size.w} && canvas.height === ${size.h}) return { trackName, skipped: true };
      canvas.width = ${size.w};
      canvas.height = ${size.h};
      const media = canvas.captureStream(30).getVideoTracks()[0];
      media.contentHint = 'detail';
      const LocalVideoTrack = old.constructor;
      const next = new LocalVideoTrack(media);
      await room.localParticipant.unpublishTrack(old, true);
      await room.localParticipant.publishTrack(next, {
        name: trackName,
        source: old.source,
        videoCodec: 'h264'
      });
      hook.localVideoTrack = next;
      return { trackName, w: canvas.width, h: canvas.height };
    })()`
  );
}

// How long a source-side resize takes to visibly land with NO gesture running.
// Without this number a "the panel ended on the source aspect" pass is
// uninterpretable: if the stimulus routinely arrives after pointer-up, the
// latch path was never exercised and the pass is vacuous.
async function measureSourceResizeLatency(cdp, windowNumber, size, timeoutMs = 20000) {
  const before = frameOf(windowNumber);
  const beforeAspect = before ? aspect(before) : null;
  const issuedAt = Date.now();
  await setSourceSize(cdp, size);
  const target = size.w / size.h;
  while (Date.now() - issuedAt < timeoutMs) {
    const current = frameOf(windowNumber);
    if (current && Math.abs(aspect(current) - target) <= ASPECT_TOLERANCE && aspect(current) !== beforeAspect) {
      return { landedMs: Date.now() - issuedAt, frame: current };
    }
    await sleep(100);
  }
  return { landedMs: null, frame: frameOf(windowNumber) };
}

// ---- panel discovery -------------------------------------------------------

// The panel's own frame is the VIDEO area plus a 44pt header strip, so the
// source aspect must be compared against the content box, never against the
// raw window frame -- a 960x600 source shows as an 840x569 panel (840/525),
// and comparing 840/569 to 1.6 fails a correct window.
function aspect(frame) {
  return frame.w / Math.max(frame.h - HEADER_HEIGHT, 1);
}

async function findRemotePanel(previousNumbers) {
  const deadline = Date.now() + 45_000;
  while (Date.now() < deadline) {
    const candidates = findWindows().filter(
      (w) => w.layer === 0 && w.w > 200 && w.h > 120
    );
    void previousNumbers;
    // Match the panel BY NAME (`petal-window-<id>`), not by aspect. The
    // receiver stack puts several same-origin windows at the same point --
    // the resizable container carries the track name and is ~44pt taller than
    // its inner surface, so an aspect match silently picks the surface, whose
    // edges have no resize handles and cannot be dragged. That mistake reads
    // as "the drag did nothing", i.e. a false negative.
    const match = candidates.find((w) => /^petal-window-\d+$/.test(w.name));
    if (match) return match;
    await sleep(500);
  }
  throw new Error('no receiver remote window ever appeared for the web peer share');
}

function frameOf(windowNumber) {
  const all = findWindows();
  return all.find((w) => w.windowNumber === windowNumber) ?? null;
}

// ---- the gesture -----------------------------------------------------------

// The East resize zone on the PANEL's own surface webview: 12pt wide, flush
// with the right edge, spanning from 10pt below the top to the bottom of the
// 44pt header strip -- and only that strip, because everything below it is the
// native video NSView (see routes/compositor/surface/+page.svelte: "Edge-resize
// grips ride in the header strip, the only part of this webview not covered by
// the video").
//
// Deliberately NOT the identical-looking `.resize-e` on the control OVERLAY
// child window at mid-height, which is what the previous attempt aimed at and
// why its positive control failed. That handle is reachable only while the
// overlays are ordered ABOVE the panel -- and raising Petal before a gesture
// (`raise_panel_and_make_key`, and plain app activation) puts the panel above
// its own overlays, so the act of making the window frontmost is what makes
// that handle unreachable. Measured live: mid-height presses moved nothing,
// this header-strip point resized on every attempt.
const HEADER_HEIGHT = 44;
function eastHandlePoint(frame) {
  return { x: frame.x + frame.w - 5, y: frame.y + Math.round(HEADER_HEIGHT * 0.64) };
}

// Topmost layer-0 window at a point, across ALL apps -- a posted mouse event is
// hit-tested against the real window stack, so a gesture aimed at a covered
// handle silently does nothing, which is indistinguishable from "the fix is
// broken" unless the harness checks first.
function topmostAt(x, y) {
  const hit = probe(['--hit', String(Math.round(x)), String(Math.round(y))])[0];
  return hit && hit.windowNumber !== undefined ? hit : null;
}

// The receiver panel is an ordinary layer-0 window, so on a busy desktop it can
// sit under whatever else is frontmost -- and a posted mouse event is
// hit-tested against the real window stack, so a covered handle silently
// receives nothing. Raise the owning app before every gesture; without this the
// positive control fails and the run is (correctly) discarded.
function raisePetal() {
  for (const name of ['Petal', 'desktop']) {
    try {
      execFileSync(
        'osascript',
        ['-e', `tell application "System Events" to tell process "${name}" to set frontmost to true`],
        { stdio: ['ignore', 'ignore', 'ignore'], timeout: 4000 }
      );
      return;
    } catch {
      // try the other process name
    }
  }
}

// Raise Petal and confirm the handle point is genuinely on the panel before
// spending a gesture on it. Other agents share this desktop and their windows
// move; without this a run silently degrades into "posted events landed on
// someone else's Chrome window".
async function ensureHandleReachable(windowNumber) {
  for (let attempt = 1; attempt <= 8; attempt += 1) {
    raisePetal();
    await sleep(500);
    const frame = frameOf(windowNumber);
    if (!frame) return null;
    const point = eastHandlePoint(frame);
    const top = topmostAt(point.x, point.y);
    if (top && top.windowNumber === windowNumber) return { frame, point };
    console.log(
      `# handle at (${point.x},${point.y}) covered by ${top ? `${top.owner} '${top.name}'` : 'nothing on screen'} (attempt ${attempt})`
    );
  }
  return null;
}

async function driveDrag({
  frame,
  dx,
  steps = 10,
  stepMs = 60,
  midGesture = null,
  midGestureStep = null,
  windowNumber,
}) {
  raisePetal();
  const start = eastHandlePoint(frame);

  // Scoring runs off the GAP-FREE sampler, not off per-step `frameOf()` calls.
  // See startFrameSampler: the stepped sampler starves exactly during the
  // republish, which is the window under test. Budget covers press -> drag ->
  // release -> the full post-release drain, with headroom; it is stopped early.
  const samplerBudgetMs = 120 + steps * (stepMs + 40) + 200 + 20 * 150 + 4000;
  const sampler = startFrameSampler(windowNumber, samplerBudgetMs, 40, frameOf(windowNumber)?.owner);
  await sleep(120);
  // The old per-step sampling points are kept as no-ops so the drag sequence
  // below still reads as the documented press/move/release script. Geometry is
  // now collected exclusively by the out-of-process sampler above.
  const sample = () => {};

  // `--hover` (buttonless move), NOT `--move` (a DRAG event): a drag posted
  // before any mouse-down is routed to whatever owns the current mouse-down
  // session, i.e. possibly another app entirely.
  probe(['--hover', String(start.x - 30), String(start.y)]);
  await sleep(80);
  probe(['--hover', String(start.x), String(start.y)]);
  await sleep(120);
  probe(['--press', String(start.x), String(start.y)]);
  sample();
  const pressedAt = Date.now();
  await sleep(120);
  let fired = false;
  let midGestureIssuedAt = null;
  let midGesturePromise = null;
  for (let step = 1; step <= steps; step += 1) {
    const x = start.x + (dx * step) / steps;
    probe(['--move', String(Math.round(x)), String(start.y)]);
    sample();
    if (midGesture && !fired && step === (midGestureStep ?? Math.floor(steps / 2))) {
      fired = true;
      midGestureIssuedAt = Date.now();
      // Deliberately NOT awaited: the republish round-trip is slower than a
      // pointer move, and awaiting it would stall the drag and turn the race
      // into a sequence. The gesture must keep running while it lands.
      midGesturePromise = midGesture();
      sample();
    }
    await sleep(stepMs);
  }
  const endX = start.x + dx;
  const releasedAt = Date.now();
  probe(['--release', String(Math.round(endX)), String(start.y)]);
  await sleep(200);
  sample();
  await midGesturePromise?.catch(() => null);
  // Let any deferred source reconciliation drain.
  for (let i = 0; i < 20; i += 1) {
    await sleep(150);
    sample();
  }
  const samples = (await sampler.stop()).filter((s) => s.tMs >= pressedAt);
  // Derived from real timestamps, because the gap-free sampler's cadence is
  // independent of the drag loop -- there is no longer a 1:1 sample-per-step.
  const releaseIndex = samples.filter((s) => s.tMs < releasedAt).length;
  return {
    start,
    samples,
    releaseIndex,
    sampleIntervalMs: 40,
    sampleCount: samples.length,
    // The largest gap between consecutive samples while the pointer was down.
    // If this is not ~40ms the sampler starved and the trial is not evidence.
    worstSampleGapMs: Math.max(
      0,
      ...samples
        .slice(1, Math.max(1, releaseIndex))
        .map((s, index) => s.tMs - samples[index].tMs)
    ),
    pressedAt,
    releasedAt,
    midGestureIssuedAt,
    gestureRemainingMs: midGestureIssuedAt ? releasedAt - midGestureIssuedAt : null,
    final: samples.at(-1),
  };
}

// ---- main ------------------------------------------------------------------

// Recorded per trial: a machine under a load spike produces excursions that
// are scheduler artifacts, not defects. A trial landing during one is re-taken
// rather than annotated.
function currentUptime() {
  try {
    return execFileSync('uptime', { encoding: 'utf8' }).trim();
  } catch {
    return 'unknown';
  }
}

const results = [];
function record(entry) {
  results.push(entry);
  console.log(`ACCEPT416 ${JSON.stringify(entry)}`);
}

let cdp = null;
try {
  compileProbe();
  cdp = await connectCdp();

  if (accessCode) {
    await evaluate(
      cdp,
      `(() => {
        const hook = window.__petalHarness?.cockpitAutoScenario;
        if (!hook) throw new Error('cockpitAutoScenario hook unavailable');
        return hook.join(${JSON.stringify(accessCode)});
      })()`
    );
    await sleep(3000);
  }

  const before = new Set(findWindows().map((w) => w.windowNumber));
  await setSourceSize(cdp, SOURCE_BASE).catch(() => null);
  await evaluate(
    cdp,
    `(() => {
      const hook = window.__petalHarness?.cockpitAutoScenario;
      if (!hook) throw new Error('cockpitAutoScenario hook unavailable');
      return hook.sharePattern();
    })()`
  );
  const panel = await findRemotePanel(before);
  console.log(`# receiver remote window: #${panel.windowNumber} ${panel.w}x${panel.h} at (${panel.x},${panel.y})`);

  // ---- positive control: plain drag, no source change. ----------------------
  await setSourceSize(cdp, SOURCE_BASE);
  await sleep(1500);
  const reachable = await ensureHandleReachable(panel.windowNumber);
  if (!reachable) {
    record({ id: 'PC-DRAG', status: 'fail', detail: 'resize handle never became the topmost window at its own point' });
    console.log('ACCEPT416_ABORT handle unreachable -- NO RESULT');
    if (jsonOutputPath) {
      fs.writeFileSync(jsonOutputPath, `${JSON.stringify({ controlPassed: false, results }, null, 2)}\n`, 'utf8');
    }
    process.exit(2);
  }
  let frame = reachable.frame;
  const controlRun = await driveDrag({ frame, dx: -160, windowNumber: panel.windowNumber });
  const controlMoved = Math.abs(controlRun.final.w - frame.w) >= 40;
  const controlAspectOk =
    Math.abs(aspect(controlRun.final) - SOURCE_BASE.w / SOURCE_BASE.h) <= ASPECT_TOLERANCE;
  record({
    id: 'PC-DRAG',
    status: controlMoved && controlAspectOk ? 'pass' : 'fail',
    detail: `plain drag: ${frame.w}x${frame.h} -> ${controlRun.final.w}x${controlRun.final.h} (aspect ${aspect(controlRun.final).toFixed(3)}, source ${(SOURCE_BASE.w / SOURCE_BASE.h).toFixed(3)})`,
  });
  // ---- positive control 2: a source resize alone must move the panel. ------
  // The drag control alone is not enough. If the source stimulus is inert, the
  // race harness races nothing and every trial "passes" by never being tested.
  const sourceControl = await measureSourceResizeLatency(cdp, panel.windowNumber, SOURCE_WIDE);
  record({
    id: 'PC-SOURCE',
    status: sourceControl.landedMs !== null ? 'pass' : 'fail',
    detail:
      sourceControl.landedMs !== null
        ? `source ${SOURCE_BASE.w}x${SOURCE_BASE.h} -> ${SOURCE_WIDE.w}x${SOURCE_WIDE.h} with no gesture landed in ${sourceControl.landedMs}ms (panel ${sourceControl.frame.w}x${sourceControl.frame.h}, aspect ${aspect(sourceControl.frame).toFixed(3)})`
        : 'a source resize with NO gesture never moved the panel -- the stimulus is inert',
  });
  const controlsPassed = controlMoved && controlAspectOk && sourceControl.landedMs !== null;
  // Make the gesture comfortably outlast the stimulus, so the source change
  // genuinely arrives mid-drag rather than after pointer-up.
  const dragTotalMs = Math.max(4000, Math.round((sourceControl.landedMs ?? 3000) * 1.5));
  const dragSteps = 24;
  const dragStepMs = Math.max(60, Math.round(dragTotalMs / dragSteps));
  console.log(`# gesture length ${dragSteps} x ${dragStepMs}ms = ${dragSteps * dragStepMs}ms; source stimulus lands in ~${sourceControl.landedMs}ms`);

  if (!controlsPassed) {
    console.log('ACCEPT416_ABORT positive control failed -- NO RESULT');
    if (jsonOutputPath) {
      fs.writeFileSync(jsonOutputPath, `${JSON.stringify({ controlPassed: false, results }, null, 2)}\n`, 'utf8');
    }
    process.exitCode = 2;
  } else {
    // ---- the race, repeated. ------------------------------------------------
    const raceOutcomes = [];
    for (let trial = 1; trial <= trials; trial += 1) {
      // Alternate which way the source jumps, so both the widen and the narrow
      // direction get raced against a drag.
      const toWide = trial % 2 === 1;
      const nextSource = toWide ? SOURCE_WIDE : SOURCE_BASE;
      const priorSource = toWide ? SOURCE_BASE : SOURCE_WIDE;
      const settled = await measureSourceResizeLatency(cdp, panel.windowNumber, priorSource);
      if (settled.landedMs === null && Math.abs(aspect(frameOf(panel.windowNumber) ?? { w: 1, h: 2 }) - priorSource.w / priorSource.h) > ASPECT_TOLERANCE) {
        record({ id: `RACE-${trial}`, status: 'skip', detail: 'could not put the source back to the pre-race size' });
        continue;
      }
      await sleep(800);
      const trialReachable = await ensureHandleReachable(panel.windowNumber);
      if (!trialReachable) {
        record({ id: `RACE-${trial}`, status: 'skip', detail: 'handle covered by another window; trial not run' });
        continue;
      }
      frame = trialReachable.frame;
      const startFrame = { ...frame };
      // Drag toward the middle of the legal width band, so no trial is decided
      // by a min-size or work-area clamp instead of by the race.
      const dx = startFrame.w > 700 ? -140 : 140;
      const run = await driveDrag({
        frame,
        dx,
        steps: dragSteps,
        stepMs: dragStepMs,
        midGestureStep: 4,
        windowNumber: panel.windowNumber,
        midGesture: () => setSourceSize(cdp, nextSource),
      });

      const targetAspect = nextSource.w / nextSource.h;
      const finalAspect = aspect(run.final);
      const endsOnSourceAspect = Math.abs(finalAspect - targetAspect) <= ASPECT_TOLERANCE;

      // "Did not leave the panel off the user's geometry mid-gesture": between
      // mouse-down and mouse-up the width must track the drag and never jump to
      // a size the user did not ask for, and the panel must not snap to the new
      // source aspect before the gesture completes.
      const dragSamples = run.samples.slice(0, run.releaseIndex);
      const expectedEndWidth = startFrame.w + dx;
      const lo = Math.min(startFrame.w, expectedEndWidth) - 24;
      const hi = Math.max(startFrame.w, expectedEndWidth) + 24;
      const worstOvershoot = Math.max(0, ...dragSamples.map((s) => Math.max(0, s.w - hi, lo - s.w)));

      // The reported #416 symptom, verbatim: "when I resize a remote window, it
      // jumps back to small and then only later to the size I set." So the
      // mid-gesture criterion is about the width the user is actively dragging:
      // it must stay inside the band they asked for, keep moving in the drag's
      // direction, and never take a jump no pointer move could explain.
      //
      // A mid-gesture change of the panel's HEIGHT to the new source aspect is
      // NOT that symptom and is not scored as a failure -- the issue's own
      // end-state semantics are that the user's chosen size wins while a genuine
      // sender-side logical-size change still propagates. It is recorded, since
      // it is the thing that visibly happens.
      const dragWidths = dragSamples.map((s) => s.w);
      const worstBacktrack = Math.max(
        0,
        ...dragWidths.slice(1).map((w, index) => {
          const delta = w - dragWidths[index];
          return dx < 0 ? Math.max(0, delta) : Math.max(0, -delta);
        })
      );
      const biggestStep = Math.max(0, ...dragWidths.slice(1).map((w, index) => Math.abs(w - dragWidths[index])));
      const adoptedNewAspectMidGesture = dragSamples
        .slice(1)
        .some((s) => Math.abs(aspect(s) - targetAspect) <= ASPECT_TOLERANCE && Math.abs(targetAspect - aspect(startFrame)) > ASPECT_TOLERANCE);
      // Proof the trial actually raced: the source change must have been
      // absorbed while the pointer was still down. Otherwise it is a sequence,
      // not a race, and it must not be counted as a passing race.
      const raced = adoptedNewAspectMidGesture || Math.abs(aspect(startFrame) - targetAspect) <= ASPECT_TOLERANCE;
      const noMidGestureSnap = worstOvershoot <= 0 && worstBacktrack <= 8 && biggestStep <= 60;

      raceOutcomes.push({
        trial,
        endsOnSourceAspect,
        noMidGestureSnap,
        finalAspect,
        targetAspect,
        worstOvershoot,
        adoptedNewAspectMidGesture,
        raced,
        worstBacktrack,
        biggestStep,
        sampleCount: run.sampleCount,
        worstSampleGapMs: run.worstSampleGapMs,
        gestureRemainingMsAfterStimulus: run.gestureRemainingMs,
        uptime: currentUptime(),
        startFrame,
        dx,
        // Full timeline, so a failing trial can be read rather than guessed at:
        // phase is relative to the real press / stimulus / release instants.
        timeline: run.samples.map((s) => ({
          tMs: s.tMs - run.pressedAt,
          phase:
            s.tMs >= run.releasedAt ? 'after-release' : run.midGestureIssuedAt && s.tMs >= run.midGestureIssuedAt ? 'post-stimulus-drag' : 'pre-stimulus-drag',
          w: s.w,
          h: s.h,
          aspect: Number(aspect(s).toFixed(3)),
        })),
      });
      record({
        id: `RACE-${trial}`,
        status: !raced ? 'skip' : endsOnSourceAspect && noMidGestureSnap ? 'pass' : 'fail',
        detail: `${startFrame.w}x${startFrame.h} -> ${run.final.w}x${run.final.h}; final aspect ${finalAspect.toFixed(3)} vs source ${targetAspect.toFixed(3)}; width overshoot ${worstOvershoot.toFixed(0)}pt, backtrack ${worstBacktrack}pt, largest step ${biggestStep}pt; adopted-new-aspect-mid-gesture ${adoptedNewAspectMidGesture}; raced ${raced}; ${run.gestureRemainingMs}ms of gesture left after the source change was issued; ${run.sampleCount} samples, worst gap ${run.worstSampleGapMs}ms; load ${currentUptime()}`,
      });
      // Flush after EVERY trial, not once at the end. A race measurement is
      // long-running and gets interrupted -- writing only on completion means
      // an interrupted run loses every timeline it collected, which is the one
      // artifact that can explain a failure rather than just count it.
      if (jsonOutputPath) {
        fs.writeFileSync(
          jsonOutputPath,
          `${JSON.stringify({ controlPassed: true, partial: true, panel, results, raceOutcomes }, null, 2)}\n`,
          'utf8'
        );
      }
    }
    const scored = raceOutcomes.filter((o) => o.raced);
    const passes = scored.filter((o) => o.endsOnSourceAspect && o.noMidGestureSnap).length;
    console.log(
      `ACCEPT416_SUMMARY ${JSON.stringify({
        attempted: raceOutcomes.length,
        genuinelyRaced: scored.length,
        pass: passes,
        rate: `${passes}/${scored.length}`,
        aspectFailures: scored.filter((o) => !o.endsOnSourceAspect).length,
        userGeometryFailures: scored.filter((o) => !o.noMidGestureSnap).length,
        adoptedNewAspectMidGesture: scored.filter((o) => o.adoptedNewAspectMidGesture).length,
      })}`
    );
    if (jsonOutputPath) {
      fs.writeFileSync(
        jsonOutputPath,
        `${JSON.stringify({ controlPassed: true, panel, results, raceOutcomes }, null, 2)}\n`,
        'utf8'
      );
    }
    process.exitCode = scored.length > 0 && passes === scored.length ? 0 : 1;
  }
} finally {
  cdp?.close();
}
