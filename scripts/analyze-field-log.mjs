#!/usr/bin/env node
//
// analyze-field-log.mjs -- read a `petal.log` and report the two measurements
// that #76 and #159 were still asking a person to take by hand.
//
// Both issues ended the same way: "run this on a real Mac, grep the log, paste
// it here, and someone who knows the codebase will interpret it." The
// interpretation is mechanical. This script does it, so the remaining step is
// one command instead of a paste-and-interpret round trip.
//
//   #76  camera-intent margin -- does the meeting camera's FIRST publish
//        attempt beat the Settings preview's release of the device, or does
//        only the bounded self-heal retry recover? Verdict is exactly the one
//        the issue's definition of done asks for: first-attempt-wins, or
//        retry-needed.
//
//   #159 picker memory -- what one source-picker episode costs, SPLIT into the
//        enumeration half (`list_begin` -> `list_done`, which #148 fixed) and
//        the thumbnail half (`list_done` -> `prewarm_done`, which has never
//        been priced).
//
// PRIVACY. These logs come from users. This script reads only lines whose
// values are numbers, stages and build identifiers, and it NEVER echoes a log
// line: no room names, identities, access codes, file paths or window titles
// reach the output. `logging.rs`'s `redact_for_export` is the standard that
// defines what counts as sensitive; note a log that arrived via "Export logs"
// has already been through it, which changes none of the numbers below.
// Absolute wall-clock timestamps are withheld too (they are an activity
// record); episodes are located by offset from the log's first line.
//
// Pure log reading: no network, no Tauri, no GUI, no side effects.
//
// Usage:
//   node scripts/analyze-field-log.mjs ~/Library/Logs/Petal/petal.log
//   node scripts/analyze-field-log.mjs ~/Library/Logs/Petal          # whole dir
//   node scripts/analyze-field-log.mjs --json petal.log.gz
//
// Unit tests: scripts/test-analyze-field-log.mjs (wired into ci-local.sh).

import { createReadStream, readdirSync, statSync } from 'node:fs';
import { createInterface } from 'node:readline';
import { basename, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createGunzip } from 'node:zlib';

// ---------------------------------------------------------------------------
// Which build first emitted each instrumentation set. A log older than these
// cannot answer the question, and saying so is the difference between "no
// evidence" and "evidence of absence" (CLAUDE.md rule 2).
// ---------------------------------------------------------------------------

/** `session: camera-intent intended=` first shipped here (#78). */
export const CAMERA_INTENT_FIRST_VERSION = '0.9.10';
/** `window_source: picker memory mark` first shipped here (#146). */
export const PICKER_MARKS_FIRST_VERSION = '0.9.20';
/** #148's per-window icon fix first shipped here; before it the marks price the OLD path. */
export const PICKER_ICON_FIX_VERSION = '0.9.21';

/** Pre-#148 cost per enumerated window, from #106's measurement (1370 MB / 18 windows). */
const PRE_FIX_MB_PER_WINDOW = 76;
/** Above this, a picker episode is "hundreds of MB", which #159 says reopens #106. */
const PICKER_HUNDREDS_MB_THRESHOLD = 100;
/** A backwards timestamp jump bigger than this is not thread jitter. */
const CLOCK_REGRESSION_TOLERANCE_MS = 1000;
/** Shared wall-clock beyond this is two instances, not a rotation handover. */
const OVERLAP_TOLERANCE_MS = 2000;

// ---------------------------------------------------------------------------
// Line parsing. Format is `YYYY-MM-DD HH:MM:SS.mmm [LEVEL] [target] message`,
// stamped in UTC by `logging.rs`'s `chrono_like_timestamp`.
// ---------------------------------------------------------------------------

export function parseTimestampMs(line) {
  if (typeof line !== 'string' || line.length < 23) return null;
  if (
    line[4] !== '-' ||
    line[7] !== '-' ||
    line[10] !== ' ' ||
    line[13] !== ':' ||
    line[16] !== ':' ||
    line[19] !== '.'
  ) {
    return null;
  }
  const year = Number(line.slice(0, 4));
  const month = Number(line.slice(5, 7));
  const day = Number(line.slice(8, 10));
  const hour = Number(line.slice(11, 13));
  const minute = Number(line.slice(14, 16));
  const second = Number(line.slice(17, 19));
  const millis = Number(line.slice(20, 23));
  if (
    !Number.isInteger(year) ||
    !Number.isInteger(month) ||
    !Number.isInteger(day) ||
    !Number.isInteger(hour) ||
    !Number.isInteger(minute) ||
    !Number.isInteger(second) ||
    !Number.isInteger(millis)
  ) {
    return null;
  }
  return Date.UTC(year, month - 1, day, hour, minute, second) + millis;
}

/** The message body, with the timestamp/level/target prefix removed. */
export function parseMessage(line) {
  const levelEnd = line.indexOf('] ', 23);
  if (levelEnd < 0) return null;
  const targetEnd = line.indexOf('] ', levelEnd + 2);
  if (targetEnd < 0) return null;
  return line.slice(targetEnd + 2);
}

export function compareVersions(a, b) {
  const parse = (value) => {
    const match = /^(\d+)\.(\d+)\.(\d+)/.exec(String(value).trim());
    return match ? [Number(match[1]), Number(match[2]), Number(match[3])] : null;
  };
  const left = parse(a);
  const right = parse(b);
  if (!left || !right) return null;
  for (let i = 0; i < 3; i += 1) {
    if (left[i] !== right[i]) return left[i] < right[i] ? -1 : 1;
  }
  return 0;
}

// ---------------------------------------------------------------------------
// Marker classification. Every literal below must match a real log line; the
// comments name the emitting site so a rename is traceable.
// ---------------------------------------------------------------------------

const CAMERA_MARKERS = [
  // camera_session.rs emit_camera_intent
  { test: (m) => m === 'session: camera-intent intended=true', kind: 'intent-on' },
  { test: (m) => m === 'session: camera-intent intended=false', kind: 'intent-off' },
  // start_camera_publish_with_device
  { test: (m) => m.startsWith('session: start_camera_publish succeeded'), kind: 'succeeded' },
  {
    test: (m) => m.startsWith('session: start_camera_publish track publish failed:'),
    kind: 'track-publish-failed',
  },
  { test: (m) => m.startsWith('session: start_camera_publish failed:'), kind: 'failed' },
  {
    test: (m) => m.startsWith('session: start_camera_publish capture running'),
    kind: 'capture-running',
  },
  {
    test: (m) => m.startsWith('session: start_camera_publish -- already publishing'),
    kind: 'already-publishing',
  },
  {
    test: (m) => m.startsWith('session: start_camera_publish -- room left'),
    kind: 'cancelled-mid-start',
  },
  { test: (m) => m.startsWith('session: start_camera_publish begin'), kind: 'begin' },
  {
    test: (m) => m.startsWith('session: start_camera_publish_command immediate attempt failed'),
    kind: 'heal-handoff',
  },
  // stop_camera_publish
  { test: (m) => m.startsWith('session: stop_camera_publish begin'), kind: 'stop-begin' },
  { test: (m) => m.startsWith('session: stop_camera_publish done'), kind: 'stop-done' },
  // drive_camera_publish_attempts / ensure_camera_published
  { test: (m) => m.startsWith('session: camera self-heal attempt'), kind: 'heal-attempt-failed' },
  { test: (m) => m.startsWith('session: camera self-heal cancelled'), kind: 'heal-cancelled' },
  { test: (m) => m.startsWith('session: camera self-heal already running'), kind: 'heal-duplicate' },
  { test: (m) => m.startsWith('session: camera publish self-heal exhausted'), kind: 'heal-exhausted' },
  // camera_session.rs log_camera_preview_state -- the Settings webview's own
  // getUserMedia edges, the only way the log can see whether the preview was
  // holding the device when the intent fired (#76).
  { test: (m) => m.startsWith('settings: camera preview acquired'), kind: 'preview-acquired' },
  { test: (m) => m.startsWith('settings: camera preview released'), kind: 'preview-released' },
  { test: (m) => m.startsWith('settings: camera preview failed'), kind: 'preview-failed' },
];

function classifyCamera(message) {
  for (const marker of CAMERA_MARKERS) {
    if (marker.test(message)) return marker.kind;
  }
  return null;
}

const PICKER_PREFIX = 'window_source: picker memory mark -- ';

/**
 * Pull the numeric fields off a picker mark. Only `stage`, the footprint, the
 * source counts and the capture counter are read -- the `vm_top=` owner
 * breakdown is deliberately not surfaced (it is a per-episode DIFFERENCE, never
 * a decomposition of the footprint beside it, #142, and reporting it as one
 * would be worse than not reporting it).
 */
export function parsePickerMark(message) {
  if (!message.startsWith(PICKER_PREFIX)) return null;
  const fields = new Map();
  for (const token of message.slice(PICKER_PREFIX.length).trim().split(/\s+/)) {
    const eq = token.indexOf('=');
    if (eq > 0) fields.set(token.slice(0, eq), token.slice(eq + 1));
  }
  const stage = fields.get('stage');
  if (stage !== 'list_begin' && stage !== 'list_done' && stage !== 'prewarm_done') return null;
  const footprintRaw = fields.get('phys_footprint_mb');
  const footprintMb = /^\d+$/.test(footprintRaw ?? '') ? Number(footprintRaw) : null;
  const capturesRaw = fields.get('thumbnail_captures');
  const thumbnailCaptures = /^\d+$/.test(capturesRaw ?? '') ? Number(capturesRaw) : null;
  const sourcesMatch = /^(\d+)d\/(\d+)w$/.exec(fields.get('sources') ?? '');
  return {
    stage,
    footprintMb,
    thumbnailCaptures,
    displays: sourcesMatch ? Number(sourcesMatch[1]) : null,
    windows: sourcesMatch ? Number(sourcesMatch[2]) : null,
    prewarm: fields.get('prewarm') ?? null,
  };
}

const BUILD_IDENTITY_PREFIX = 'petal: startup build identity -- ';

export function parseBuildIdentity(message) {
  if (!message.startsWith(BUILD_IDENTITY_PREFIX)) return null;
  const version = /\bversion=([0-9][0-9A-Za-z.\-+]*)/.exec(message);
  const commit = /\bcommit=([0-9a-f]{4,40})/.exec(message);
  return {
    version: version ? version[1] : null,
    commit: commit ? commit[1] : null,
  };
}

// ---------------------------------------------------------------------------
// The analyzer.
// ---------------------------------------------------------------------------

function newAnalysis() {
  return {
    lines: 0,
    timestampedLines: 0,
    firstMs: null,
    lastMs: null,
    builds: [],
    startupCount: 0,
    clockRegressions: 0,
    cameraEpisodes: [],
    cameraLineCount: 0,
    previewLineCount: 0,
    pickerEpisodes: [],
    pickerMarkCount: 0,
  };
}

function recordBuild(analysis, identity) {
  analysis.startupCount += 1;
  const key = `${identity.version ?? '?'}@${identity.commit ?? '?'}`;
  const existing = analysis.builds.find((build) => build.key === key);
  if (existing) {
    existing.count += 1;
  } else {
    analysis.builds.push({ key, version: identity.version, commit: identity.commit, count: 1 });
  }
}

function openCameraEpisode(state, analysis, kind, atMs) {
  closeCameraEpisode(state, analysis);
  state.camera = {
    kind,
    atMs,
    attempts: [],
    healHandoff: false,
    healAttemptsFailed: 0,
    healExhausted: false,
    healCancelled: false,
    intentClearedWhileUnresolved: false,
    crossedRestart: false,
    // Contention, from the Settings preview's own lines: was it holding the
    // device when the intent fired, and when did it let go? `null` when the
    // run carries no preview line at all (Settings never open, or a build
    // predating the line), which is "unknown", never "uncontended".
    previewLiveAtIntent: null,
    previewReleaseMs: null,
    previewReleasedBeforeBegin: null,
  };
}

function closeCameraEpisode(state, analysis) {
  if (!state.camera) return;
  analysis.cameraEpisodes.push(finalizeCameraEpisode(state.camera));
  state.camera = null;
}

function finalizeCameraEpisode(episode) {
  const attempts = episode.attempts;
  const first = attempts[0] ?? null;
  const resolvedIndex = attempts.findIndex((attempt) => attempt.outcome === 'succeeded');
  const resolved = resolvedIndex >= 0 ? attempts[resolvedIndex] : null;
  const retryEvidence =
    episode.healHandoff || episode.healAttemptsFailed > 0 || episode.healExhausted;

  let verdict;
  let note = null;
  if (attempts.length === 0) {
    verdict = 'no-publish-attempt';
    note = 'the intent line has no `start_camera_publish` after it';
  } else if (resolved && resolvedIndex === 0 && !retryEvidence) {
    verdict = 'first-attempt-wins';
  } else if (resolved) {
    verdict = 'retry-needed';
    note = `the first attempt lost; attempt ${resolvedIndex + 1} won`;
  } else if (episode.healExhausted) {
    verdict = 'failed';
    note = 'the bounded self-heal loop exhausted its retries';
  } else if (episode.intentClearedWhileUnresolved || episode.healCancelled) {
    verdict = 'cancelled';
    note = 'the camera was turned off (or the room left) before the publish resolved';
  } else if (episode.crossedRestart) {
    verdict = 'incomplete';
    note = 'the app restarted before the episode reached an outcome';
  } else if (attempts.some((attempt) => attempt.outcome === null)) {
    verdict = 'incomplete';
    note = 'the log ends (or is truncated) mid-attempt';
  } else {
    verdict = 'inconclusive';
    note = 'every attempt failed and no self-heal outcome was logged';
  }

  const marginMs = resolved && resolved.endMs !== null ? resolved.endMs - episode.atMs : null;
  const releaseWindowMs = first ? first.beginMs - episode.atMs : null;
  const firstAttemptMs = first && first.endMs !== null ? first.endMs - first.beginMs : null;

  return {
    kind: episode.kind,
    atMs: episode.atMs,
    stopReleaseMs: episode.stopReleaseMs ?? null,
    contended: episode.previewLiveAtIntent ?? null,
    previewReleaseMs: episode.previewReleaseMs ?? null,
    previewReleasedBeforeBegin: episode.previewReleasedBeforeBegin ?? null,
    previewReacquireMs: episode.previewReacquireMs ?? null,
    attempts: attempts.map((attempt, index) => ({
      index: index + 1,
      outcome: attempt.outcome ?? 'no outcome logged',
      durationMs: attempt.endMs !== null ? attempt.endMs - attempt.beginMs : null,
      sawCaptureRunning: attempt.sawCaptureRunning,
    })),
    attemptCount: attempts.length,
    winningAttempt: resolvedIndex >= 0 ? resolvedIndex + 1 : null,
    marginMs,
    releaseWindowMs,
    firstAttemptMs,
    healHandoff: episode.healHandoff,
    healAttemptsFailed: episode.healAttemptsFailed,
    healExhausted: episode.healExhausted,
    healCancelled: episode.healCancelled,
    cancelledByUser: Boolean(episode.intentClearedWhileUnresolved),
    crossedRestart: episode.crossedRestart,
    verdict,
    note,
  };
}

function lastOpenAttempt(episode) {
  for (let i = episode.attempts.length - 1; i >= 0; i -= 1) {
    if (episode.attempts[i].outcome === null) return episode.attempts[i];
  }
  return null;
}

function handleCamera(state, analysis, kind, atMs) {
  analysis.cameraLineCount += 1;
  switch (kind) {
    case 'stop-begin':
      state.pendingStop = { beginMs: atMs, doneMs: null };
      return;
    case 'stop-done':
      if (state.pendingStop) state.pendingStop.doneMs = atMs;
      return;
    case 'intent-on': {
      // `stop_camera_publish` re-announces the intent as it STANDS, so a live
      // device switch reaches here with the intent still ON right after its
      // stop pair -- that, and only that, is what separates a switch from a
      // fresh ON toggle in the log.
      const isSwitch = Boolean(state.pendingStop && state.pendingStop.doneMs !== null);
      openCameraEpisode(state, analysis, isSwitch ? 'device-switch' : 'on', atMs);
      if (isSwitch) {
        state.camera.stopReleaseMs = state.pendingStop.doneMs - state.pendingStop.beginMs;
      }
      state.camera.previewLiveAtIntent = state.previewLive ?? null;
      state.pendingStop = null;
      state.awaitingPreviewReturn = null;
      return;
    }
    case 'intent-off': {
      const unresolved =
        state.camera && !state.camera.attempts.some((attempt) => attempt.outcome === 'succeeded');
      if (state.camera && unresolved) state.camera.intentClearedWhileUnresolved = true;
      closeCameraEpisode(state, analysis);
      if (state.pendingStop && state.pendingStop.doneMs !== null) {
        const off = finalizeCameraEpisode({
          kind: 'off',
          atMs,
          attempts: [],
          healHandoff: false,
          healAttemptsFailed: 0,
          healExhausted: false,
          healCancelled: false,
          intentClearedWhileUnresolved: false,
          crossedRestart: false,
          stopReleaseMs: state.pendingStop.doneMs - state.pendingStop.beginMs,
        });
        analysis.cameraEpisodes.push(off);
        // The OFF direction's own number: how long the preview took to come
        // back once told the device was free. Filled in by the next
        // `preview acquired`, if one arrives before another intent edge.
        state.awaitingPreviewReturn = off;
      }
      state.pendingStop = null;
      return;
    }
    case 'preview-acquired': {
      analysis.previewLineCount += 1;
      state.previewLive = true;
      if (state.awaitingPreviewReturn) {
        state.awaitingPreviewReturn.previewReacquireMs = atMs - state.awaitingPreviewReturn.atMs;
        state.awaitingPreviewReturn = null;
      }
      return;
    }
    case 'preview-released':
    case 'preview-failed': {
      analysis.previewLineCount += 1;
      state.previewLive = false;
      const episode = state.camera;
      if (
        kind === 'preview-released' &&
        episode &&
        episode.previewLiveAtIntent === true &&
        episode.previewReleaseMs === null
      ) {
        episode.previewReleaseMs = atMs - episode.atMs;
        // Did the preview let go before the native side even reached for the
        // device, or only while the acquisition was already under way?
        episode.previewReleasedBeforeBegin = episode.attempts.length === 0;
      }
      return;
    }
    case 'begin': {
      // A `begin` joins the OPEN episode only when it is a retry -- the
      // self-heal loop re-enters `start_camera_publish_with_device` without a
      // new intent line, and only ever after a failure. A `begin` that follows
      // a WON attempt is a separate publish (on a build predating #78, hours
      // or days later), and merging them would invent an episode with three
      // "attempts" that never raced each other. That is exactly what a real
      // 0.9.4 field log produced before this guard existed.
      const lastAttempt = state.camera
        ? state.camera.attempts[state.camera.attempts.length - 1]
        : null;
      if (state.camera && lastAttempt && lastAttempt.outcome === 'succeeded') {
        closeCameraEpisode(state, analysis);
      }
      if (!state.camera) {
        // A publish with no intent line before it: either the log starts
        // mid-episode or this build predates #78. Attribute it rather than
        // dropping it, and label it so nobody reads it as an ON toggle.
        openCameraEpisode(state, analysis, 'unattributed', atMs);
      }
      state.camera.attempts.push({
        beginMs: atMs,
        endMs: null,
        outcome: null,
        sawCaptureRunning: false,
      });
      return;
    }
    case 'capture-running': {
      const attempt = state.camera && lastOpenAttempt(state.camera);
      if (attempt) attempt.sawCaptureRunning = true;
      return;
    }
    case 'succeeded':
    case 'failed':
    case 'track-publish-failed':
    case 'cancelled-mid-start': {
      const attempt = state.camera && lastOpenAttempt(state.camera);
      if (!attempt) return;
      attempt.endMs = atMs;
      attempt.outcome =
        kind === 'succeeded'
          ? 'succeeded'
          : kind === 'failed'
            ? 'failed (capture never delivered a first frame)'
            : kind === 'track-publish-failed'
              ? 'failed (track publish)'
              : 'cancelled mid-start (room left or camera raced on)';
      return;
    }
    case 'already-publishing':
      if (state.camera) {
        state.camera.attempts.push({
          beginMs: atMs,
          endMs: atMs,
          outcome: 'no-op (already publishing)',
          sawCaptureRunning: false,
        });
      }
      return;
    case 'heal-handoff':
      if (state.camera) state.camera.healHandoff = true;
      return;
    case 'heal-attempt-failed':
      if (state.camera) state.camera.healAttemptsFailed += 1;
      return;
    case 'heal-exhausted':
      if (state.camera) state.camera.healExhausted = true;
      return;
    case 'heal-cancelled':
      if (state.camera) state.camera.healCancelled = true;
      return;
    default:
  }
}

function closePickerEpisode(state, analysis, reason) {
  if (!state.picker) return;
  const episode = state.picker;
  state.picker = null;
  const begin = episode.begin;
  const done = episode.listDone;
  const prewarm = episode.prewarmDone;
  const counts = prewarm ?? done ?? {};
  const delta = (from, to) =>
    from && to && from.footprintMb !== null && to.footprintMb !== null
      ? to.footprintMb - from.footprintMb
      : null;
  const complete = Boolean(begin && done && prewarm);
  const windows = counts.windows ?? null;
  const totalMb = delta(begin, prewarm);
  analysis.pickerEpisodes.push({
    atMs: begin ? begin.atMs : (done ?? prewarm).atMs,
    complete,
    incompleteReason: complete ? null : reason,
    displays: counts.displays ?? null,
    windows,
    prewarm: prewarm ? prewarm.prewarm : null,
    enumerationMb: delta(begin, done),
    enumerationDurationMs: begin && done ? done.atMs - begin.atMs : null,
    thumbnailMb: delta(done, prewarm),
    thumbnailDurationMs: done && prewarm ? prewarm.atMs - done.atMs : null,
    totalMb,
    totalDurationMs: begin && prewarm ? prewarm.atMs - begin.atMs : null,
    captures:
      begin && prewarm && begin.thumbnailCaptures !== null && prewarm.thumbnailCaptures !== null
        ? prewarm.thumbnailCaptures - begin.thumbnailCaptures
        : null,
    mbPerWindow: totalMb !== null && windows ? totalMb / windows : null,
    beginFootprintMb: begin ? begin.footprintMb : null,
    peakFootprintMb: prewarm ? prewarm.footprintMb : (done ? done.footprintMb : null),
  });
}

function handlePickerMark(state, analysis, mark, atMs) {
  analysis.pickerMarkCount += 1;
  const stamped = { ...mark, atMs };
  if (mark.stage === 'list_begin') {
    closePickerEpisode(state, analysis, 'a new enumeration began before this one finished');
    state.picker = { begin: stamped, listDone: null, prewarmDone: null };
    return;
  }
  if (!state.picker) {
    // The log starts mid-episode (a rotated file, or a truncated paste).
    state.picker = { begin: null, listDone: null, prewarmDone: null };
  }
  if (mark.stage === 'list_done') {
    state.picker.listDone = stamped;
    return;
  }
  state.picker.prewarmDone = stamped;
  closePickerEpisode(state, analysis, 'the opening `list_begin` mark is not in this log');
}

/** Feed one already-read line. Exported so tests can drive the state machine directly. */
export function feedLine(state, analysis, line) {
  analysis.lines += 1;
  const atMs = parseTimestampMs(line);
  if (atMs === null) return; // continuation line of a multi-line message
  analysis.timestampedLines += 1;
  if (analysis.firstMs === null) analysis.firstMs = atMs;
  if (analysis.lastMs !== null && atMs < analysis.lastMs - CLOCK_REGRESSION_TOLERANCE_MS) {
    analysis.clockRegressions += 1;
  }
  analysis.lastMs = atMs;

  const hasCamera = line.includes('camera');
  const hasPicker = line.includes('picker memory mark');
  const hasStartup = line.includes('startup build identity');
  if (!hasCamera && !hasPicker && !hasStartup) return;

  const message = parseMessage(line);
  if (message === null) return;

  if (hasStartup) {
    const identity = parseBuildIdentity(message);
    if (identity) {
      recordBuild(analysis, identity);
      // A restart ends whatever was in flight; an episode must never be
      // reported as spanning one.
      if (state.camera) state.camera.crossedRestart = true;
      closeCameraEpisode(state, analysis);
      closePickerEpisode(state, analysis, 'the app restarted mid-episode');
      state.pendingStop = null;
      state.previewLive = null;
      state.awaitingPreviewReturn = null;
      return;
    }
  }
  if (hasPicker) {
    const mark = parsePickerMark(message);
    if (mark) {
      handlePickerMark(state, analysis, mark, atMs);
      return;
    }
  }
  if (hasCamera) {
    const kind = classifyCamera(message);
    if (kind) handleCamera(state, analysis, kind, atMs);
  }
}

function newState() {
  return {
    camera: null,
    picker: null,
    pendingStop: null,
    previewLive: null,
    awaitingPreviewReturn: null,
  };
}

export function analyzeLines(lines) {
  const analysis = newAnalysis();
  const state = newState();
  for (const line of lines) feedLine(state, analysis, line);
  closeCameraEpisode(state, analysis);
  closePickerEpisode(state, analysis, 'the log ends before `prewarm_done`');
  return analysis;
}

export async function analyzeFile(path) {
  const analysis = newAnalysis();
  const state = newState();
  let stream = createReadStream(path);
  if (path.endsWith('.gz')) stream = stream.pipe(createGunzip());
  const reader = createInterface({ input: stream, crlfDelay: Infinity });
  for await (const line of reader) feedLine(state, analysis, line);
  closeCameraEpisode(state, analysis);
  closePickerEpisode(state, analysis, 'the log ends before `prewarm_done`');
  analysis.file = basename(path);
  return analysis;
}

// ---------------------------------------------------------------------------
// Verdicts.
// ---------------------------------------------------------------------------

// #76 asks for the margin from the INTENT edge, so only an episode that has one
// can answer it. A publish with no intent line before it (the log starts
// mid-episode, or the build predates #78) still says whether the attempt
// succeeded, but not what it raced -- it is reported separately, never folded
// into the verdict.
const MEASURABLE_KINDS = new Set(['on', 'device-switch']);

export function cameraVerdict(analysis) {
  const measurable = analysis.cameraEpisodes.filter(
    (episode) => MEASURABLE_KINDS.has(episode.kind) && episode.marginMs !== null
  );
  const unattributed = analysis.cameraEpisodes.filter((episode) => episode.kind === 'unattributed');
  const wins = measurable.filter((episode) => episode.verdict === 'first-attempt-wins');
  const retries = measurable.filter((episode) => episode.verdict === 'retry-needed');
  const failures = analysis.cameraEpisodes.filter((episode) => episode.verdict === 'failed');
  // #76 is about the CONTENDED case. Only an episode whose run carries the
  // Settings preview's own lines can be labelled either way; the rest are
  // "unknown", which is not evidence in either direction.
  const contended = measurable.filter((episode) => episode.contended === true);
  const uncontended = measurable.filter((episode) => episode.contended === false);
  const unknownContention = measurable.filter((episode) => episode.contended === null);
  const summary = {
    measurable,
    unattributed,
    wins,
    retries,
    failures,
    contended,
    uncontended,
    unknownContention,
  };

  if (analysis.cameraEpisodes.length === 0) return { verdict: 'no-data', ...summary };
  if (measurable.length === 0) return { verdict: 'inconclusive', ...summary };
  if (retries.length > 0 || failures.length > 0) return { verdict: 'retry-needed', ...summary };
  return { verdict: 'first-attempt-wins', ...summary };
}

export function pickerVerdict(analysis) {
  // `skipped_in_flight` / `no_sources` episodes captured nothing, so their
  // footprint delta is not a burst's cost -- never let one set the verdict.
  const priced = analysis.pickerEpisodes.filter(
    (episode) => episode.complete && episode.prewarm === 'completed' && episode.totalMb !== null
  );
  if (analysis.pickerEpisodes.length === 0) return { verdict: 'no-data', priced };
  if (priced.length === 0) return { verdict: 'inconclusive', priced };
  const worst = Math.max(...priced.map((episode) => episode.totalMb));
  return {
    verdict: worst >= PICKER_HUNDREDS_MB_THRESHOLD ? 'hundreds-of-mb' : 'tens-of-mb',
    priced,
    worstMb: worst,
  };
}

/**
 * Signals that the file does not describe one app instance running alone.
 * Concurrent instances writing near each other is a real hazard: two builds'
 * numbers averaged together are not a measurement of either.
 */
export function concurrencyWarnings(analysis) {
  const warnings = [];
  if (analysis.builds.length > 1) {
    // Same version, different commits is the ordinary dev-loop rebuild; name
    // the commits so it does not read as an upgrade or a second instance.
    const versions = new Set(analysis.builds.map((build) => build.version ?? '?'));
    const names = analysis.builds
      .map((build) =>
        versions.size === 1 && build.commit
          ? `${build.version ?? '?'}@${build.commit}`
          : (build.version ?? '?')
      )
      .join(', ');
    warnings.push(
      `${analysis.builds.length} DIFFERENT builds appear in this log ` +
        `(${versions.size === 1 ? 'same version, commits' : 'versions'}: ${names}). ` +
        'That is either an upgrade mid-file or two app instances writing to the same path. ' +
        'Episodes are reported per instance and never span a restart, but do not average across builds.'
    );
  } else if (analysis.startupCount > 1) {
    warnings.push(
      `${analysis.startupCount} app startups in this log. Episodes are attributed to the run they ` +
        'occurred in and none is reported as spanning a restart.'
    );
  }
  if (analysis.clockRegressions > 0) {
    warnings.push(
      `${analysis.clockRegressions} timestamp(s) jump backwards by more than ` +
        `${CLOCK_REGRESSION_TOLERANCE_MS} ms. Interleaved writers or a clock change; durations ` +
        'crossing such a point are not trustworthy.'
    );
  }
  return warnings;
}

export function buildVersion(analysis) {
  const versions = analysis.builds.map((build) => build.version).filter(Boolean);
  if (versions.length === 0) return null;
  return versions[versions.length - 1];
}

// ---------------------------------------------------------------------------
// Reporting. Numbers, stages and verdicts only.
// ---------------------------------------------------------------------------

function fmtOffset(analysis, atMs) {
  if (analysis.firstMs === null || atMs === null) return 'unknown';
  let delta = Math.max(0, atMs - analysis.firstMs);
  const ms = delta % 1000;
  delta = Math.floor(delta / 1000);
  const s = delta % 60;
  delta = Math.floor(delta / 60);
  const m = delta % 60;
  const h = Math.floor(delta / 60);
  const pad = (n, width = 2) => String(n).padStart(width, '0');
  return `+${pad(h)}:${pad(m)}:${pad(s)}.${pad(ms, 3)}`;
}

function fmtDuration(ms) {
  if (ms === null || ms === undefined) return 'n/a';
  if (ms < 1000) return `${ms} ms`;
  if (ms < 120_000) return `${(ms / 1000).toFixed(2)} s`;
  const totalMinutes = Math.round(ms / 60_000);
  if (totalMinutes < 120) return `${totalMinutes} min`;
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours < 48) return `${hours}h ${minutes}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

/** Wrap a warning so a long one does not become an unreadable single line. */
function wrap(text, width, indent) {
  const words = text.split(' ');
  const lines = [];
  let line = '';
  for (const word of words) {
    if (line && line.length + 1 + word.length > width) {
      lines.push(line);
      line = word;
    } else {
      line = line ? `${line} ${word}` : word;
    }
  }
  if (line) lines.push(line);
  return lines.map((entry, index) => (index === 0 ? entry : indent + entry));
}

function fmtSignedMb(mb) {
  if (mb === null || mb === undefined) return 'n/a';
  return `${mb >= 0 ? '+' : ''}${mb} MB`;
}

const KIND_LABEL = {
  on: 'camera ON',
  off: 'camera OFF',
  'device-switch': 'device switch',
  unattributed: 'publish with no intent line',
};

/**
 * The contended case is the one #76 is about. The Settings preview holds the
 * device from the WEBVIEW (getUserMedia), invisible to the native log until
 * Settings started reporting its own edges (`settings: camera preview ...`).
 * With those lines, an episode is labelled contended or not; without them it
 * is UNKNOWN, and saying "the preview released in time" about a log that may
 * describe an uncontended publish would be exactly the kind of inference this
 * project has burned cycles on.
 */
function reportContention(summary, analysis, out) {
  const { contended, uncontended, unknownContention, measurable } = summary;
  if (contended.length > 0) {
    const releases = contended
      .map((episode) => episode.previewReleaseMs)
      .filter((ms) => ms !== null)
      .sort((a, b) => a - b);
    const beforeBegin = contended.filter((episode) => episode.previewReleasedBeforeBegin).length;
    out.push(
      `    CONTENDED: ${contended.length} of ${measurable.length} measured episode(s) had the Settings`
    );
    out.push('    preview holding the device when the intent fired, and still won first try.');
    if (releases.length > 0) {
      out.push(
        `    The preview let go ${fmtDuration(releases[0])}..` +
          `${fmtDuration(releases[releases.length - 1])} after the intent; ${beforeBegin} of ` +
          `${contended.length} released before the publish began.`
      );
    }
    if (uncontended.length > 0) {
      out.push(
        `    ${uncontended.length} other(s) were uncontended (preview not holding the device).`
      );
    }
    if (unknownContention.length > 0) {
      out.push(`    ${unknownContention.length} carry no preview line, so contention is unknown there.`);
    }
    return;
  }
  if (uncontended.length > 0 && unknownContention.length === 0) {
    out.push('    NOT THE RUNBOOK: the Settings preview was not holding the device in any');
    out.push('    of these episodes, so they prove the publish path is healthy and nothing');
    out.push('    about the contention #76 asks about. Re-run with the preview live.');
    return;
  }
  out.push('    CAVEAT: nothing in a log says whether the Settings camera preview was');
  out.push('    live at the time -- it holds the device from the webview -- unless the');
  out.push('    build logs the preview\'s own edges (`settings: camera preview ...`), and');
  if (analysis.previewLineCount === 0) {
    out.push('    this log has none: Settings was never open, or the build predates them.');
  } else {
    out.push('    none fell inside these episodes.');
  }
  out.push('    A win here proves the publish path is healthy; it proves the preview');
  out.push('    yields in time only if this log came from the #76 runbook');
  out.push('    (Settings open with the preview running, then toggle the meeting camera).');
}

function contentionLabel(episode, analysis) {
  if (episode.contended === true) {
    const when = episode.previewReleasedBeforeBegin ? 'before' : 'after';
    return episode.previewReleaseMs === null
      ? 'live at intent; no release seen'
      : `live at intent; released ${fmtDuration(episode.previewReleaseMs)} later (${when} the publish began)`;
  }
  if (episode.contended === false) {
    return episode.kind === 'device-switch'
      ? 'not holding the device (it did not grab it in the switch gap)'
      : 'not holding the device (uncontended)';
  }
  return analysis.previewLineCount === 0
    ? 'unknown (no preview line in this log)'
    : 'unknown (no preview line in this run)';
}

function reportCamera(analysis, out) {
  const version = buildVersion(analysis);
  out.push('Measurement 1 -- camera-intent margin (#76)');
  out.push('-'.repeat(72));

  const summary = cameraVerdict(analysis);
  if (summary.verdict === 'no-data') {
    out.push('  NO DATA -- this log carries no camera publish episode.');
    out.push('');
    out.push('  Expected `session: camera-intent intended=<bool>` plus the surrounding');
    out.push('  `session: start_camera_publish ...` lines. Nothing here matched them.');
    if (version && compareVersions(version, CAMERA_INTENT_FIRST_VERSION) < 0) {
      out.push('');
      out.push(
        `  This log is from build ${version}; the intent line first shipped in ` +
          `${CAMERA_INTENT_FIRST_VERSION} (#78), so it CANNOT be here.`
      );
      out.push(`  Re-take the measurement on ${CAMERA_INTENT_FIRST_VERSION} or later.`);
    } else if (analysis.cameraLineCount === 0) {
      out.push('');
      out.push('  No camera line of any kind is present: the camera was most likely never');
      out.push('  turned on during this log. This is a missing measurement, not a failure.');
    }
    out.push('');
    out.push('  To produce one (about two minutes, needs a Mac with a camera):');
    out.push('    1. Open Settings so the camera preview is live.');
    out.push('    2. Join a meeting, leaving Settings open.');
    out.push('    3. Toggle the meeting camera ON, then OFF, then switch camera device.');
    out.push('    4. Re-run this command against ~/Library/Logs/Petal/petal.log');
    out.push('  Or unattended, against a debug build launched with PETAL_AUTOTEST_SOCK:');
    out.push('    node apps/desktop/scripts/measure-camera-intent.mjs   (docs/TESTING.md)');
    out.push('');
    return;
  }

  const verdictLine = {
    'first-attempt-wins': 'VERDICT: first-attempt-wins',
    'retry-needed': 'VERDICT: retry-needed',
    inconclusive: 'VERDICT: inconclusive',
  }[summary.verdict];
  out.push(`  ${verdictLine}`);

  if (summary.verdict === 'first-attempt-wins') {
    out.push(
      `    ${summary.measurable.length} publish episode(s) measured; every one succeeded on its`
    );
    out.push('    FIRST attempt, with no self-heal retry.');
    out.push('');
    reportContention(summary, analysis, out);
  } else if (summary.verdict === 'retry-needed') {
    const tail =
      summary.failures.length > 0
        ? `, and ${summary.failures.length} exhausted the bounded retries entirely.`
        : '.';
    out.push(
      `    ${summary.retries.length} of ${summary.measurable.length} publish episode(s) needed a ` +
        `self-heal retry${tail}`
    );
    out.push('    The first attempt does NOT reliably win the race, so #76 step 3 applies:');
    out.push('    the publisher should wait on a bounded acknowledgement of the preview');
    out.push('    releasing the device rather than relying on retry convergence.');
  } else if (summary.unattributed.length > 0) {
    out.push('    Camera publishes are present, but none carries the `camera-intent` edge');
    out.push('    the margin is measured from. See NOT COUNTED below.');
  } else {
    out.push('    Camera lines are present but no episode reached an outcome. See below.');
  }

  const margins = summary.measurable.map((episode) => episode.marginMs).sort((a, b) => a - b);
  if (margins.length > 0) {
    const median = margins[Math.floor((margins.length - 1) / 2)];
    out.push('');
    out.push(
      `  margin (intent -> outcome): min ${fmtDuration(margins[0])}, median ` +
        `${fmtDuration(median)}, max ${fmtDuration(margins[margins.length - 1])}`
    );
  }

  if (summary.unattributed.length > 0) {
    const acquisitions = summary.unattributed
      .flatMap((episode) => episode.attempts)
      .filter((attempt) => attempt.outcome === 'succeeded' && attempt.durationMs !== null)
      .map((attempt) => fmtDuration(attempt.durationMs));
    out.push('');
    for (const line of wrap(
      `NOT COUNTED: ${summary.unattributed.length} publish attempt(s) have no ` +
        '`camera-intent` line before them' +
        (version && compareVersions(version, CAMERA_INTENT_FIRST_VERSION) < 0
          ? ` -- build ${version} predates #78, which shipped that line in ` +
            `${CAMERA_INTENT_FIRST_VERSION}`
          : '') +
        '. Without the intent edge there is no margin to measure, so they cannot ' +
        'answer #76 and are excluded from the verdict above.' +
        (acquisitions.length > 0
          ? ` Their acquisition times, for reference only: ${acquisitions.join(', ')}.`
          : ''),
      70,
      '  '
    )) {
      out.push(`  ${line}`);
    }
  }

  const byKind = new Map();
  for (const episode of analysis.cameraEpisodes) {
    byKind.set(episode.kind, (byKind.get(episode.kind) ?? 0) + 1);
  }
  out.push(
    `  cases present: ${[...byKind]
      .map(([kind, count]) => `${KIND_LABEL[kind] ?? kind} x${count}`)
      .join(', ')}`
  );
  out.push('');

  analysis.cameraEpisodes.forEach((episode, index) => {
    out.push(
      `  episode ${index + 1}  ${KIND_LABEL[episode.kind] ?? episode.kind}  at ` +
        `${fmtOffset(analysis, episode.atMs)}`
    );
    if (episode.kind === 'off') {
      out.push(`    device released in ${fmtDuration(episode.stopReleaseMs)} (no publish attempt)`);
      if (episode.previewReacquireMs !== null) {
        out.push(
          `    preview came back in       ${fmtDuration(episode.previewReacquireMs)}` +
            '   (intent cleared -> preview acquired)'
        );
      }
      out.push('');
      return;
    }
    if (episode.kind === 'unattributed') {
      for (const attempt of episode.attempts) {
        out.push(
          `    attempt ${attempt.index}: ${attempt.outcome}` +
            (attempt.durationMs !== null ? ` after ${fmtDuration(attempt.durationMs)}` : '')
        );
      }
      out.push('    no `camera-intent` line precedes it, so there is no margin to report');
      out.push('');
      return;
    }
    if (episode.stopReleaseMs !== null) {
      out.push(`    old capture released in    ${fmtDuration(episode.stopReleaseMs)}`);
    }
    out.push(
      `    intent -> publish begin    ${fmtDuration(episode.releaseWindowMs)}` +
        '   (the window the Settings preview has to release in)'
    );
    out.push(`    Settings preview           ${contentionLabel(episode, analysis)}`);
    if (episode.firstAttemptMs !== null) {
      out.push(`    first attempt took         ${fmtDuration(episode.firstAttemptMs)}`);
    }
    out.push(
      `    margin (intent -> outcome) ${fmtDuration(episode.marginMs)}` +
        (episode.winningAttempt ? `   (won on attempt ${episode.winningAttempt})` : '')
    );
    for (const attempt of episode.attempts) {
      out.push(
        `      attempt ${attempt.index}: ${attempt.outcome}` +
          (attempt.durationMs !== null ? ` after ${fmtDuration(attempt.durationMs)}` : '')
      );
    }
    if (episode.healHandoff || episode.healAttemptsFailed > 0 || episode.healExhausted) {
      out.push(
        `      self-heal: handoff=${episode.healHandoff} failed_attempts=` +
          `${episode.healAttemptsFailed} exhausted=${episode.healExhausted}`
      );
    }
    out.push(`    verdict: ${episode.verdict}${episode.note ? ` -- ${episode.note}` : ''}`);
    out.push('');
  });
}

function reportPicker(analysis, out) {
  const version = buildVersion(analysis);
  out.push('Measurement 2 -- picker memory (#159)');
  out.push('-'.repeat(72));

  const summary = pickerVerdict(analysis);
  if (summary.verdict === 'no-data') {
    out.push('  NO DATA -- this log carries no source-picker episode.');
    out.push('');
    out.push('  Expected `window_source: picker memory mark -- stage=list_begin|list_done|');
    out.push('  prewarm_done ...`. Nothing here matched them.');
    if (version && compareVersions(version, PICKER_MARKS_FIRST_VERSION) < 0) {
      out.push('');
      out.push(
        `  This log is from build ${version}; the picker marks first shipped in ` +
          `${PICKER_MARKS_FIRST_VERSION} (#146), so they CANNOT be here.`
      );
      out.push(
        `  Re-take the measurement on ${PICKER_ICON_FIX_VERSION} or later -- that is the ` +
          'first build'
      );
      out.push("  carrying #148's icon fix, which is what #159 asks to confirm.");
    } else {
      out.push('');
      out.push('  The picker was most likely never opened during this log, or every');
      out.push('  enumeration was a cache hit (a cache hit is deliberately not marked).');
    }
    out.push('');
    out.push('  To produce one (about one minute):');
    out.push('    1. On 0.9.21+, with several applications open, join a room.');
    out.push('    2. Open the source picker and leave it open a few seconds.');
    out.push('    3. Re-run this command against ~/Library/Logs/Petal/petal.log');
    out.push('');
    return;
  }

  if (version) {
    const cmp = compareVersions(version, PICKER_ICON_FIX_VERSION);
    if (cmp !== null && cmp < 0) {
      out.push(
        `  BUILD ${version} PREDATES #148's icon fix (${PICKER_ICON_FIX_VERSION}) -- these are ` +
          'PRE-FIX numbers.'
      );
    } else {
      out.push(`  build ${version} -- at or after #148's icon fix (${PICKER_ICON_FIX_VERSION}).`);
    }
  } else {
    out.push('  build version not stated in this log; #148 shipped in ' + PICKER_ICON_FIX_VERSION);
    out.push('  and only that build or later carries the fix.');
  }

  if (summary.verdict === 'inconclusive') {
    out.push('  VERDICT: inconclusive -- every episode reports prewarm=skipped_in_flight or');
    out.push('  no_sources, so its footprint delta is not a burst\'s cost. Re-open the picker');
    out.push('  after a 60s gap (PICKER_MEMORY_MARK_COOLDOWN) so a real burst is marked.');
  } else if (summary.verdict === 'tens-of-mb') {
    out.push(
      `  VERDICT: tens of MB (worst episode ${fmtSignedMb(summary.worstMb)}) -- consistent with the`
    );
    out.push("  #148 fix. The elevation #106 reported is not present in this log's picker.");
  } else {
    out.push(
      `  VERDICT: hundreds of MB or more (worst episode ${fmtSignedMb(summary.worstMb)}).`
    );
    out.push('  Per #159, the icon path was not the whole story: reopen #106 with these marks.');
  }
  out.push('');

  analysis.pickerEpisodes.forEach((episode, index) => {
    const sources =
      episode.displays !== null && episode.windows !== null
        ? `${episode.displays}d/${episode.windows}w`
        : 'sources unknown';
    out.push(
      `  episode ${index + 1}  at ${fmtOffset(analysis, episode.atMs)}  ${sources}  prewarm=` +
        `${episode.prewarm ?? 'n/a'}`
    );
    if (!episode.complete) {
      out.push(`    INCOMPLETE -- ${episode.incompleteReason}`);
      out.push('');
      return;
    }
    out.push(
      `    enumeration (list_begin -> list_done)    ${fmtSignedMb(episode.enumerationMb).padEnd(9)}` +
        ` over ${fmtDuration(episode.enumerationDurationMs)}`
    );
    out.push(
      `    thumbnails  (list_done -> prewarm_done)  ${fmtSignedMb(episode.thumbnailMb).padEnd(9)}` +
        ` over ${fmtDuration(episode.thumbnailDurationMs)}` +
        (episode.captures !== null ? `, ${episode.captures} capture(s)` : '')
    );
    out.push(
      `    TOTAL       (list_begin -> prewarm_done) ${fmtSignedMb(episode.totalMb).padEnd(9)}` +
        ` over ${fmtDuration(episode.totalDurationMs)}`
    );
    if (episode.mbPerWindow !== null) {
      out.push(
        `    per enumerated window                    ${episode.mbPerWindow.toFixed(1)} MB` +
          `   (pre-#148 was ~${PRE_FIX_MB_PER_WINDOW} MB/window)`
      );
    }
    if (episode.totalMb !== null && episode.totalMb > 0 && episode.thumbnailMb !== null) {
      const share = Math.round((episode.thumbnailMb / episode.totalMb) * 100);
      out.push(
        `    the thumbnail half -- the one #159 says has never been priced -- is ${share}% of it`
      );
    }
    out.push('');
  });

  const priced = summary.priced ?? [];
  if (priced.length > 0) {
    const thumbs = priced.map((episode) => episode.thumbnailMb).filter((mb) => mb !== null);
    const enums = priced.map((episode) => episode.enumerationMb).filter((mb) => mb !== null);
    if (thumbs.length > 0) {
      out.push(
        `  across ${priced.length} priced episode(s): enumeration ` +
          `${fmtSignedMb(Math.min(...enums))}..${fmtSignedMb(Math.max(...enums))}, ` +
          `thumbnails ${fmtSignedMb(Math.min(...thumbs))}..${fmtSignedMb(Math.max(...thumbs))}`
      );
      out.push('');
    }
  }
}

export function formatReport(analysis) {
  const out = [];
  out.push(`Petal field-log analysis -- ${analysis.file ?? 'log'}`);
  out.push('='.repeat(72));
  out.push(
    `  ${analysis.lines.toLocaleString('en-US')} lines, spanning ` +
      `${fmtDuration(analysis.lastMs !== null ? analysis.lastMs - analysis.firstMs : null)}`
  );
  const version = buildVersion(analysis);
  out.push(
    `  build: ${
      version
        ? `${version}${analysis.builds[analysis.builds.length - 1].commit ? ` (commit ${analysis.builds[analysis.builds.length - 1].commit})` : ''}`
        : 'not stated in this log'
    }`
  );
  out.push('  times are offsets from the first line; absolute timestamps are withheld.');
  out.push('  no room name, identity, access code, path or window title is read or printed.');
  for (const warning of concurrencyWarnings(analysis)) {
    out.push('');
    for (const line of wrap(`! ${warning}`, 70, '  ')) out.push(`  ${line}`);
  }
  out.push('');
  reportCamera(analysis, out);
  reportPicker(analysis, out);
  return out.join('\n');
}

/**
 * The JSON view -- same numbers, same privacy rules, no free text from the log.
 * Absolute epoch stamps are converted to offsets here for the same reason the
 * text report withholds wall-clock times: when a person used their computer is
 * an activity record, and this output is meant to be paste-safe.
 */
export function toJson(analysis) {
  const relative = (episode) => {
    const { atMs, ...rest } = episode;
    return { offsetMs: analysis.firstMs === null ? null : atMs - analysis.firstMs, ...rest };
  };
  return {
    file: analysis.file ?? null,
    lines: analysis.lines,
    spanMs: analysis.lastMs !== null ? analysis.lastMs - analysis.firstMs : null,
    build: buildVersion(analysis),
    builds: analysis.builds.map((build) => ({ version: build.version, commit: build.commit })),
    warnings: concurrencyWarnings(analysis),
    camera: {
      verdict: cameraVerdict(analysis).verdict,
      previewLines: analysis.previewLineCount,
      episodes: analysis.cameraEpisodes.map(relative),
    },
    picker: {
      verdict: pickerVerdict(analysis).verdict,
      episodes: analysis.pickerEpisodes.map(relative),
    },
  };
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

const USAGE = `Usage: node scripts/analyze-field-log.mjs [--json] <petal.log | directory> ...

Reports two measurements from a Petal log:
  #76   camera-intent margin  -- first-attempt-wins, or retry-needed
  #159  picker memory         -- enumeration half vs thumbnail half

Accepts .log and .log.gz, and a directory (every log inside it).
Prints numbers, stages and verdicts only -- never log text.`;

function expandInputs(paths) {
  const files = [];
  for (const path of paths) {
    let stat;
    try {
      stat = statSync(path);
    } catch {
      throw new Error(`cannot read ${basename(path)} (no such file or directory)`);
    }
    if (stat.isDirectory()) {
      const names = readdirSync(path).filter((name) => name.includes('.log'));
      // A rotated log is kept alongside its own gzip. Analyzing both would
      // report every episode twice and flag the pair as concurrent instances.
      const plain = new Set(names.filter((name) => !name.endsWith('.gz')));
      const entries = names
        .filter((name) => !(name.endsWith('.gz') && plain.has(name.slice(0, -3))))
        .sort();
      if (entries.length === 0) throw new Error(`no *.log files in ${basename(path)}`);
      for (const name of entries) files.push(join(path, name));
    } else {
      files.push(path);
    }
  }
  return files;
}

async function main(argv) {
  const asJson = argv.includes('--json');
  const paths = argv.filter((arg) => !arg.startsWith('--'));
  if (paths.length === 0 || argv.includes('--help') || argv.includes('-h')) {
    console.log(USAGE);
    return paths.length === 0 ? 2 : 0;
  }

  let files;
  try {
    files = expandInputs(paths);
  } catch (error) {
    console.error(`analyze-field-log: ${error.message}`);
    console.error('');
    console.error(USAGE);
    return 2;
  }

  const analyses = [];
  for (const file of files) {
    try {
      analyses.push(await analyzeFile(file));
    } catch (error) {
      // Never throw a stack at a user who just wanted to read their own log.
      console.error(`analyze-field-log: could not read ${basename(file)} -- ${error.message}`);
      return 2;
    }
  }

  if (asJson) {
    console.log(JSON.stringify(analyses.map(toJson), null, 2));
  } else {
    for (const analysis of analyses) {
      console.log(formatReport(analysis));
    }
    for (const warning of crossFileWarnings(analyses)) {
      for (const line of wrap(`! ${warning}`, 72, '  ')) console.log(line);
    }
  }
  return 0;
}

/**
 * Hazards that only show up across several files: more than one build version
 * in the directory, and two files whose wall-clock spans genuinely overlap
 * (which means two app instances ran at once). Reading either set as one
 * timeline silently mixes two instances' state -- the exact thing that makes a
 * hand-read of a log directory wrong rather than merely noisy. A rotation
 * handover shares one instant between two files and is NOT an overlap.
 */
export function crossFileWarnings(analyses) {
  const warnings = [];
  const versions = new Set();
  for (const analysis of analyses) {
    for (const build of analysis.builds) if (build.version) versions.add(build.version);
  }
  if (versions.size > 1) {
    warnings.push(
      `these files carry ${versions.size} different build versions ` +
        `(${[...versions].sort((a, b) => compareVersions(a, b) ?? 0).join(', ')}). A log directory can hold more than one app ` +
        "instance's output -- check each report's own build line before comparing numbers " +
        'across them.'
    );
  }
  const usable = analyses.filter((analysis) => analysis.firstMs !== null);
  for (let i = 0; i < usable.length; i += 1) {
    for (let j = i + 1; j < usable.length; j += 1) {
      const a = usable[i];
      const b = usable[j];
      // A rotation boundary shares one timestamp between two files; that is a
      // handover, not two instances. Require a real shared span.
      const shared = Math.min(a.lastMs, b.lastMs) - Math.max(a.firstMs, b.firstMs);
      if (shared > OVERLAP_TOLERANCE_MS) {
        warnings.push(
          `${a.file} and ${b.file} cover overlapping wall-clock time: two app instances were ` +
            'running at once. Read each report on its own; do not merge their numbers.'
        );
      }
    }
  }
  return warnings;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).then((code) => {
    process.exitCode = code;
  });
}
