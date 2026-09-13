// WebView presentation measurement for remote camera tiles.
//
// The native receiver stats describe what the DECODER did; only
// `requestVideoFrameCallback` on the tile's own <video> element describes what
// the WebView actually PRESENTED. `ParticipantTile.svelte` owns the element and
// `galleryBridge.ts` owns the durable receiver-interval record, so this module
// is the seam between them.
//
// The probe is generation-owned: starting a probe for an identity retires any
// previous probe for that identity, and a retired probe can never mutate the
// replacement. That is what makes a slow stop/unmount safe. Registry ownership
// is deliberately one active probe per identity: simultaneous identified tiles
// replace one another, while ParticipantTile gives ownerless tiles unique keys.
// A replacement inherits completed counters but resets frame/FPS baselines, so
// the handoff itself and any open silence are not counted as completed gaps.
//
// It stores fixed counters and timestamps only -- no frame history, no element
// handles, and no per-frame IPC or allocation.

/** The subset of `HTMLVideoElement` this module reads, so tests can drive a
 * plain object. */
export interface CameraPresentationVideo {
  paused: boolean;
  readyState: number;
  addEventListener(type: string, listener: () => void, options?: { once?: boolean }): void;
  removeEventListener(type: string, listener: () => void): void;
  requestVideoFrameCallback?(callback: (now: number) => void): number;
  cancelVideoFrameCallback?(handle: number): void;
}

export interface CameraPresentationProbeOptions {
  identity: string;
  video: CameraPresentationVideo;
  /** Called at most once, on the first presented frame or the `loadeddata`
   * readiness fallback, whichever happens first. */
  onFirstFrame?: () => void;
  /** Injectable clock; defaults to `performance.now`. */
  now?: () => number;
}

export interface CameraPresentationProbe {
  stop(): void;
}

export interface CameraPresentationSnapshot {
  presentedFrames: number;
  presentedFps: number;
  /** How many probes have started for this identity. A number that keeps
   * climbing is tile/stream churn, not a media freeze. */
  probeStarts: number;
  /** Token of the currently registered probe. */
  generation: number;
  /** `false` when the element cannot report the presentation boundary; the
   * counters then stay zero and must not be read as a healthy cadence. */
  rvfcAvailable: boolean;
  readyState: number | null;
  paused: boolean | null;
  hidden: boolean;
  /** `false` while hidden/paused, when gaps are not attributable to media. */
  observing: boolean;
  /** Completed presentation gaps at or above the threshold. */
  gapCount100Ms: number;
  gapCount250Ms: number;
  /** Largest completed gap, or the current silence while one is open. */
  maxGapMs: number;
  /** Cumulative completed-plus-current time beyond 100 ms. */
  excessGapMs: number;
  /** Silence since the last presented frame, or null when none is open. */
  currentGapMs: number | null;
}

const GAP_THRESHOLD_MS = 100;
const SEVERE_GAP_THRESHOLD_MS = 250;
const FPS_WINDOW_MS = 1_000;
/** `HTMLMediaElement.HAVE_CURRENT_DATA` without depending on the DOM global. */
const HAVE_CURRENT_DATA = 2;

interface ProbeRecord {
  identity: string;
  generation: number;
  probeStarts: number;
  video: CameraPresentationVideo;
  now: () => number;
  onFirstFrame?: () => void;
  presentedFrames: number;
  lastPresentedAt: number | null;
  fpsWindowStartAt: number | null;
  fpsWindowFrames: number;
  lastFps: number;
  gapCount100Ms: number;
  gapCount250Ms: number;
  maxGapMs: number;
  excessGapMs: number;
  ready: boolean;
  callbackHandle: number | undefined;
  stopped: boolean;
  detachListeners: () => void;
}

const probes = new Map<string, ProbeRecord>();
/** Counters of retired probes, keyed by identity. A tile unmount/remount (or a
 * stream handoff) retires the probe and immediately starts a replacement; the
 * replacement inherits these so cumulative gap history is never erased. */
const retiredCounters = new Map<string, ProbeCounters>();
let nextGeneration = 1;

interface ProbeCounters {
  presentedFrames: number;
  lastFps: number;
  gapCount100Ms: number;
  gapCount250Ms: number;
  maxGapMs: number;
  excessGapMs: number;
  probeStarts: number;
}

function defaultNow(): number {
  return typeof performance === 'undefined' ? Date.now() : performance.now();
}

function documentHidden(): boolean {
  return typeof document === 'undefined' ? false : document.hidden;
}

/** Presentation gaps are only attributable to media while the element is
 * playing in a visible document. */
function isObservable(record: ProbeRecord): boolean {
  return !record.stopped && !record.video.paused && !documentHidden();
}

/** Drop the gap/FPS baseline so an unobservable stretch is never counted as a
 * media freeze. */
function resetBaseline(record: ProbeRecord): void {
  record.lastPresentedAt = null;
  record.fpsWindowStartAt = null;
  record.fpsWindowFrames = 0;
}

function retireProbe(record: ProbeRecord): void {
  if (record.stopped) return;
  record.stopped = true;
  record.detachListeners();
  if (record.callbackHandle !== undefined) {
    record.video.cancelVideoFrameCallback?.(record.callbackHandle);
    record.callbackHandle = undefined;
  }
  retiredCounters.set(record.identity, {
    presentedFrames: record.presentedFrames,
    lastFps: record.lastFps,
    gapCount100Ms: record.gapCount100Ms,
    gapCount250Ms: record.gapCount250Ms,
    maxGapMs: record.maxGapMs,
    excessGapMs: record.excessGapMs,
    probeStarts: record.probeStarts
  });
  // Only the active probe for this identity owns the map entry.
  if (probes.get(record.identity) === record) probes.delete(record.identity);
}

function countersFor(identity: string, active: ProbeRecord | undefined): ProbeCounters {
  if (active) {
    return {
      presentedFrames: active.presentedFrames,
      lastFps: active.lastFps,
      gapCount100Ms: active.gapCount100Ms,
      gapCount250Ms: active.gapCount250Ms,
      maxGapMs: active.maxGapMs,
      excessGapMs: active.excessGapMs,
      probeStarts: active.probeStarts
    };
  }
  return (
    retiredCounters.get(identity) ?? {
      presentedFrames: 0,
      lastFps: 0,
      gapCount100Ms: 0,
      gapCount250Ms: 0,
      maxGapMs: 0,
      excessGapMs: 0,
      probeStarts: 0
    }
  );
}

function recordPresentedFrame(record: ProbeRecord, now: number): void {
  if (!record.ready) {
    record.ready = true;
    record.onFirstFrame?.();
  }
  if (record.lastPresentedAt === null) {
    record.fpsWindowStartAt = now;
    record.fpsWindowFrames = 0;
  } else {
    const gap = now - record.lastPresentedAt;
    if (gap >= GAP_THRESHOLD_MS) {
      record.gapCount100Ms += 1;
      record.excessGapMs += gap - GAP_THRESHOLD_MS;
      if (gap >= SEVERE_GAP_THRESHOLD_MS) record.gapCount250Ms += 1;
    }
    if (gap > record.maxGapMs) record.maxGapMs = gap;
  }
  record.presentedFrames += 1;
  record.fpsWindowFrames += 1;
  record.lastPresentedAt = now;
  if (record.fpsWindowStartAt !== null && now - record.fpsWindowStartAt >= FPS_WINDOW_MS) {
    record.lastFps = (record.fpsWindowFrames * 1_000) / (now - record.fpsWindowStartAt);
    record.fpsWindowStartAt = now;
    record.fpsWindowFrames = 0;
  }
}

function scheduleCallback(record: ProbeRecord): void {
  if (record.stopped) return;
  const { requestVideoFrameCallback } = record.video;
  if (typeof requestVideoFrameCallback !== 'function') return;
  record.callbackHandle = requestVideoFrameCallback.call(record.video, () => {
    record.callbackHandle = undefined;
    if (record.stopped) return;
    if (!isObservable(record)) {
      resetBaseline(record);
    } else {
      recordPresentedFrame(record, record.now());
    }
    // Exactly one outstanding callback is the probe's steady state.
    scheduleCallback(record);
  });
}

/** Start (or replace) the probe for `identity`. Registration is synchronous,
 * so a frame presented immediately after this call is still counted.
 *
 * A replacement inherits the previous probe's cumulative counters: a tile or
 * stream handoff must not erase gap history, which is what makes the durable
 * record trustworthy. Only the frame/FPS baselines reset, so the handoff
 * itself is never counted as a gap. */
export function startCameraPresentationProbe(
  options: CameraPresentationProbeOptions
): CameraPresentationProbe {
  const previous = probes.get(options.identity);
  // Svelte runs an effect's cleanup (stop) BEFORE the replacement effect body,
  // so the active entry is usually already gone; inherit from the retired
  // counters in that case.
  const inherited = countersFor(options.identity, previous);
  const record: ProbeRecord = {
    identity: options.identity,
    generation: nextGeneration++,
    probeStarts: inherited.probeStarts + 1,
    video: options.video,
    now: options.now ?? defaultNow,
    onFirstFrame: options.onFirstFrame,
    presentedFrames: inherited.presentedFrames,
    lastPresentedAt: null,
    fpsWindowStartAt: null,
    fpsWindowFrames: 0,
    lastFps: inherited.lastFps,
    gapCount100Ms: inherited.gapCount100Ms,
    gapCount250Ms: inherited.gapCount250Ms,
    maxGapMs: inherited.maxGapMs,
    excessGapMs: inherited.excessGapMs,
    ready: false,
    callbackHandle: undefined,
    stopped: false,
    detachListeners: () => {}
  };

  const onReady = () => {
    if (record.stopped || record.ready) return;
    record.ready = true;
    record.onFirstFrame?.();
  };
  const onVisibilityChange = () => resetBaseline(record);
  const onPlaybackStateChange = () => resetBaseline(record);

  record.video.addEventListener('loadeddata', onReady, { once: true });
  record.video.addEventListener('pause', onPlaybackStateChange);
  record.video.addEventListener('play', onPlaybackStateChange);
  record.video.addEventListener('emptied', onPlaybackStateChange);
  if (typeof document !== 'undefined') {
    document.addEventListener('visibilitychange', onVisibilityChange);
  }
  record.detachListeners = () => {
    record.video.removeEventListener('loadeddata', onReady);
    record.video.removeEventListener('pause', onPlaybackStateChange);
    record.video.removeEventListener('play', onPlaybackStateChange);
    record.video.removeEventListener('emptied', onPlaybackStateChange);
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', onVisibilityChange);
    }
  };

  probes.set(options.identity, record);
  // Retire AFTER installing the replacement, so the retired probe cannot delete
  // the new map entry, and a stale callback can never mutate it.
  if (previous) retireProbe(previous);
  scheduleCallback(record);
  // A stream that is already decoded before this effect ran will never fire
  // `loadeddata`; the readiness fallback still has to fire once.
  if (record.video.readyState >= HAVE_CURRENT_DATA) onReady();

  return { stop: () => retireProbe(record) };
}

function snapshotFor(record: ProbeRecord): CameraPresentationSnapshot {
  const now = record.now();
  const hidden = documentHidden();
  const observing = !record.stopped && !record.video.paused && !hidden;
  const currentGapMs =
    observing && record.lastPresentedAt !== null
      ? Math.max(0, now - record.lastPresentedAt)
      : null;
  const openGapMs = currentGapMs ?? 0;
  const windowFps =
    record.fpsWindowStartAt !== null &&
    record.fpsWindowFrames >= 2 &&
    now - record.fpsWindowStartAt > 0
      ? (record.fpsWindowFrames * 1_000) / (now - record.fpsWindowStartAt)
      : record.lastFps;
  return {
    presentedFrames: record.presentedFrames,
    presentedFps: Number.isFinite(windowFps) ? windowFps : 0,
    probeStarts: record.probeStarts,
    generation: record.generation,
    rvfcAvailable: typeof record.video.requestVideoFrameCallback === 'function',
    readyState: typeof record.video.readyState === 'number' ? record.video.readyState : null,
    paused: typeof record.video.paused === 'boolean' ? record.video.paused : null,
    hidden,
    observing,
    gapCount100Ms: record.gapCount100Ms,
    gapCount250Ms: record.gapCount250Ms,
    maxGapMs: Math.max(record.maxGapMs, openGapMs),
    excessGapMs: record.excessGapMs + Math.max(0, openGapMs - GAP_THRESHOLD_MS),
    currentGapMs
  };
}

/** Latest aggregate for one identity, or null when no probe is registered. */
export function takeCameraPresentationSnapshot(
  identity: string
): CameraPresentationSnapshot | null {
  const record = probes.get(identity);
  if (!record || record.stopped) return null;
  return snapshotFor(record);
}

/** Retire every probe (room disconnect) and drop inherited state. */
export function clearAllCameraPresentations(): void {
  for (const record of probes.values()) retireProbe(record);
  probes.clear();
  retiredCounters.clear();
}
