/**
 * Hold-last-frame for remote share video (#627).
 *
 * A WebRTC `<video>` has no frame to present the moment its source is swapped
 * (republish), muted, resubscribed, or its decoder reconfigures -- and
 * `.tile video { background: #000 }` turns every one of those gaps into a black
 * flash. CLAUDE.md's "Never show a black frame" rule requires a disruption to
 * render as a FROZEN frame instead.
 *
 * Two layers of defence, deliberately:
 *
 *  1. STRUCTURAL (zero latency). A canvas holding a copy of the last presented
 *     frame sits *underneath* the share video, and the share video's own
 *     background is transparent. The instant the video has nothing to present
 *     it paints nothing and the held frame shows through. No event, no timer,
 *     no detection -- so there is no window in which black can appear.
 *  2. DETECTED (bounded latency). Some compositors paint a video layer as
 *     opaque black rather than transparent when it has no frame. So a stall
 *     watchdog, plus the callers that know a gap is coming (source swap, track
 *     mute, stream pause), also raise the canvas ABOVE the video. Layer 1
 *     alone should make this invisible; it exists because "the video layer is
 *     transparent when empty" is a rendering-stack assumption, and this repo
 *     has paid for unverified rendering assumptions before.
 *
 * `HOLD_STALL_MS` must stay above one frame interval at the lowest cadence we
 * publish and below anything a human reads as a flash.
 */

/** Refresh the held copy at most this often. A slightly stale held frame is
 *  indistinguishable from a fresh one during a freeze, and this keeps the
 *  steady-state cost to a few GPU blits per second instead of one per frame. */
export const HOLD_REFRESH_MS = 200;

/** Declare a gap once frames have stopped for longer than this. Above two
 *  frame intervals at 30fps (66ms) so normal jitter never trips it. */
export const HOLD_STALL_MS = 90;

/** Watchdog poll period. */
export const HOLD_POLL_MS = 30;

export type HoldReason = 'stall' | 'source-swap' | 'muted' | 'paused';

/**
 * The pure decision core: given frame arrivals, explicit gap notices and the
 * clock, decide whether the held frame should currently be covering the video
 * and whether a fresh copy should be taken.
 *
 * Kept free of DOM so it can be driven directly by tests. The DOM binding
 * below owns only canvas/class mechanics.
 */
export class HoldLastFrameState {
  private lastFrameAt: number | null = null;
  private lastCaptureAt: number | null = null;
  private held = false;
  private hasFrame = false;
  private reason: HoldReason | null = null;

  private readonly stallMs: number;
  private readonly refreshMs: number;

  constructor(stallMs: number = HOLD_STALL_MS, refreshMs: number = HOLD_REFRESH_MS) {
    this.stallMs = stallMs;
    this.refreshMs = refreshMs;
  }

  /** True once a frame has ever been captured, i.e. there is something to hold. */
  get canHold(): boolean {
    return this.hasFrame;
  }

  /** True when the held copy should be covering the video right now. */
  get isHolding(): boolean {
    return this.held;
  }

  /** Why the hold is currently engaged, for diagnostics. */
  get holdReason(): HoldReason | null {
    return this.reason;
  }

  /**
   * A frame was presented. Returns true when the caller should re-capture the
   * held copy from the video.
   */
  noteFrame(now: number, usable: boolean): boolean {
    // An UNUSABLE frame is not evidence of liveness. It must neither release
    // the hold (that would uncover an empty video -- the black frame this
    // exists to prevent) nor advance the stall clock (that would keep the
    // watchdog permanently disarmed, since a dry element keeps reporting).
    if (!usable) return false;
    this.lastFrameAt = now;
    // A real frame is presenting again: stop covering.
    if (this.held) {
      this.held = false;
      this.reason = null;
    }
    if (this.lastCaptureAt !== null && now - this.lastCaptureAt < this.refreshMs) return false;
    this.lastCaptureAt = now;
    this.hasFrame = true;
    return true;
  }

  /**
   * A gap is known to be starting (source swap, mute, pause). Engages the hold
   * immediately -- this is the path that keeps the common cases at zero
   * latency rather than one watchdog period.
   */
  noteGap(reason: HoldReason): boolean {
    if (!this.hasFrame || this.held) return false;
    this.held = true;
    this.reason = reason;
    return true;
  }

  /** Watchdog tick. Returns true when it just engaged the hold. */
  poll(now: number): boolean {
    if (!this.hasFrame || this.held) return false;
    if (this.lastFrameAt === null) return false;
    if (now - this.lastFrameAt < this.stallMs) return false;
    this.held = true;
    this.reason = 'stall';
    return true;
  }
}

/** The subset of `HTMLVideoElement` this module needs. */
export interface HoldVideoLike {
  videoWidth: number;
  videoHeight: number;
  readyState: number;
}

/**
 * A frame is worth copying only once the element genuinely has one decoded.
 *
 * Note this deliberately keys on "the element HAS a presentable frame", not on
 * "a NEW frame arrived" (which is what `requestVideoFrameCallback` reports).
 * The distinction matters and is the right way round: a stream that stalls
 * while the decoder keeps holding its last frame is already rendering a frozen
 * picture, which is the outcome we want -- covering it would be pointless work.
 * The hold is needed only when the element has NOTHING to present, and that is
 * exactly what this predicate detects (`videoWidth` collapses to 0 and/or
 * `readyState` drops below HAVE_CURRENT_DATA on a source swap or mute).
 */
export function frameIsUsable(video: HoldVideoLike): boolean {
  return video.videoWidth > 0 && video.videoHeight > 0 && video.readyState >= 2;
}

export const HOLD_CANVAS_CLASS = 'share-hold-canvas';
export const HOLD_ACTIVE_CLASS = 'share-hold-canvas--holding';

export interface HoldLastFrameHandle {
  /** Engage the hold now, ahead of a disruption the caller is about to cause. */
  noteGap(reason: HoldReason): void;
  /** For tests/diagnostics. */
  readonly state: HoldLastFrameState;
  stop(): void;
}

interface HoldDeps {
  requestAnimationFrame: (cb: () => void) => number;
  cancelAnimationFrame: (handle: number) => void;
  setInterval: (cb: () => void, ms: number) => unknown;
  clearInterval: (handle: unknown) => void;
  now: () => number;
}

function defaultDeps(): HoldDeps | null {
  if (
    typeof requestAnimationFrame !== 'function' ||
    typeof document === 'undefined' ||
    typeof setInterval !== 'function'
  ) {
    return null;
  }
  return {
    requestAnimationFrame: (cb) => requestAnimationFrame(cb),
    cancelAnimationFrame: (handle) => cancelAnimationFrame(handle),
    setInterval: (cb, ms) => setInterval(cb, ms),
    clearInterval: (handle) => clearInterval(handle as ReturnType<typeof setInterval>),
    now: () => Date.now(),
  };
}

/**
 * Bind hold-last-frame to one share tile's video. Idempotent per video: a
 * second call returns the existing handle rather than stacking loops.
 */
export function attachHoldLastFrame(
  tile: HTMLElement,
  video: HTMLVideoElement,
  registry: WeakMap<HTMLVideoElement, HoldLastFrameHandle>,
  injected?: Partial<HoldDeps>
): HoldLastFrameHandle | null {
  const existing = registry.get(video);
  if (existing) return existing;
  const base = defaultDeps();
  if (!base) return null;
  const deps: HoldDeps = { ...base, ...injected };

  const canvas = document.createElement('canvas');
  canvas.className = HOLD_CANVAS_CLASS;
  // Degrade, never throw: a runtime without a 2D canvas context (or a test
  // double standing in for one) simply gets no hold rather than a broken tile.
  if (typeof canvas.getContext !== 'function') return null;
  const context = canvas.getContext('2d');
  if (!context || typeof context.drawImage !== 'function') return null;
  // Underneath the video (layer 1). CSS gives the video a transparent
  // background so this shows through the instant the video has no frame.
  // Bail rather than register a handle whose canvas is detached: that would
  // run a capture loop forever and paint nothing, which reads as "held" to
  // callers while the user still sees black.
  const parent = video.parentElement;
  if (!parent) return null;
  parent.insertBefore(canvas, video);

  const state = new HoldLastFrameState();
  let rafId: number | null = null;
  let pollId: unknown = null;
  let stopped = false;

  // Change-only: this runs on every animation frame, and unconditional
  // classList/dataset writes per frame per tile is real work for no reason.
  let appliedHeld: string | null = null;
  const applyHeld = () => {
    const desired = state.isHolding ? state.holdReason ?? 'stall' : null;
    if (desired === appliedHeld) return;
    appliedHeld = desired;
    if (desired !== null) {
      canvas.classList.add(HOLD_ACTIVE_CLASS);
      tile.dataset.shareHoldingFrame = desired;
    } else {
      canvas.classList.remove(HOLD_ACTIVE_CLASS);
      delete tile.dataset.shareHoldingFrame;
    }
  };

  const capture = () => {
    const width = video.videoWidth;
    const height = video.videoHeight;
    if (width <= 0 || height <= 0) return;
    if (canvas.width !== width) canvas.width = width;
    if (canvas.height !== height) canvas.height = height;
    try {
      context.drawImage(video, 0, 0, width, height);
    } catch {
      // A tainted or not-yet-decodable frame: keep whatever we already hold.
    }
  };

  const tick = () => {
    if (stopped) return;
    if ('isConnected' in tile && !tile.isConnected) {
      handle.stop();
      return;
    }
    if (frameIsUsable(video) && state.noteFrame(deps.now(), true)) capture();
    applyHeld();
    rafId = deps.requestAnimationFrame(tick);
  };

  const handle: HoldLastFrameHandle = {
    state,
    noteGap: (reason: HoldReason) => {
      if (state.noteGap(reason)) applyHeld();
    },
    stop: () => {
      if (stopped) return;
      stopped = true;
      if (rafId !== null) deps.cancelAnimationFrame(rafId);
      if (pollId !== null) deps.clearInterval(pollId);
      canvas.remove();
      registry.delete(video);
      delete tile.dataset.shareHoldingFrame;
    },
  };

  registry.set(video, handle);
  pollId = deps.setInterval(() => {
    if (stopped) return;
    if (state.poll(deps.now())) applyHeld();
  }, HOLD_POLL_MS);
  rafId = deps.requestAnimationFrame(tick);
  return handle;
}
