// #247: local freeze-watchdog decision logic for remote camera tiles,
// extracted from galleryBridge.ts so it's unit-testable without a Tauri/
// livekit-client runtime (this file has zero framework imports).
//
// `weakConnectionIdentities` (galleryBridge.ts) only reflects the SFU's own
// paused/resumed signaling (TrackStreamStateChanged) -- if the far side dies
// mid-stream without a clean disconnect, the SFU can take tens of seconds
// (or longer) to notice, during which a <video> bound to the still-"active"
// MediaStreamTrack just holds its last decoded frame forever with no local
// indication. This mirrors the native compositor's existing local watchdog
// (`NO_FRAME_RETIRE_AFTER` / `no_frame_decision` in transport/subscriber.rs):
// poll each subscribed camera's own decode progress (`framesDecoded` from
// getRTCStatsReport(), independent of any server-driven signal) and flag it
// stale if that counter stops advancing.

export const FREEZE_WATCHDOG_TIMEOUT_MS = 30_000;
export const FREEZE_WATCHDOG_POLL_MS = 2_000;
export const CAMERA_DECODE_HEALTH_LOG_MS = 5_000;

export interface CameraFreezeState {
  lastFramesDecoded: number;
  lastProgressAt: number;
}

export interface CameraDecodeHealthState {
  lastLoggedAt: number;
  lastLoggedFramesDecoded: number;
}

export interface CameraDecodeHealth {
  identity: string;
  framesDecoded: number | null;
  decodedFps: number;
  gapSinceLastFrameMs: number;
}

/** Closed, privacy-safe buckets accepted by the native Sentry bridge. */
export type CameraReceiveCadence = 'reduced' | 'severe' | 'stalled';
export type CameraReceiveDecoderRender = 'decoder_degraded';

export interface CameraReceiveHealthSignal {
  cadence: CameraReceiveCadence;
  decoderRender: CameraReceiveDecoderRender;
}

/**
 * Classify only a confirmed unhealthy receive interval. A missing RTC stats
 * report is not evidence of a media fault, so it deliberately emits nothing.
 * Identity, track information, counts, timestamps, and text stay outside this
 * value and cannot cross the Sentry IPC boundary.
 */
export function classifyCameraReceiveHealth(
  decodedFps: number | null,
  streamPaused: boolean,
  stale: boolean
): CameraReceiveHealthSignal | null {
  // A missing/invalid report is never enough to diagnose a media fault. In
  // particular, do not let a stale UI state turn a stats-read failure into a
  // false `stalled` quality signal.
  if (decodedFps === null || !Number.isFinite(decodedFps) || decodedFps < 0) return null;
  if (streamPaused || stale) {
    return { cadence: 'stalled', decoderRender: 'decoder_degraded' };
  }
  if (decodedFps >= 24) return null;
  if (decodedFps >= 10) return { cadence: 'reduced', decoderRender: 'decoder_degraded' };
  if (decodedFps > 0) return { cadence: 'severe', decoderRender: 'decoder_degraded' };
  return { cadence: 'stalled', decoderRender: 'decoder_degraded' };
}

export function nextCameraDecodeHealthState(
  previous: CameraDecodeHealthState | undefined,
  framesDecoded: number | null,
  now: number,
  logIntervalMs: number = CAMERA_DECODE_HEALTH_LOG_MS
): { state: CameraDecodeHealthState; health: Omit<CameraDecodeHealth, 'identity'> | null } {
  const currentFrames = framesDecoded ?? previous?.lastLoggedFramesDecoded ?? 0;
  if (!previous) {
    return {
      state: { lastLoggedAt: now, lastLoggedFramesDecoded: currentFrames },
      health: null
    };
  }
  const elapsedMs = now - previous.lastLoggedAt;
  if (elapsedMs < logIntervalMs) {
    return { state: previous, health: null };
  }
  return {
    state: { lastLoggedAt: now, lastLoggedFramesDecoded: currentFrames },
    health: {
      framesDecoded,
      decodedFps:
        framesDecoded === null || elapsedMs <= 0
          ? 0
          : (Math.max(0, framesDecoded - previous.lastLoggedFramesDecoded) * 1000) / elapsedMs,
      gapSinceLastFrameMs: 0
    }
  };
}

export function formatCameraDecodeHealth(health: CameraDecodeHealth): string {
  const frames = health.framesDecoded === null ? 'unknown' : String(health.framesDecoded);
  return `gallery bridge: camera decode health for '${health.identity}' -- frames_decoded=${frames} decoded_fps=${health.decodedFps.toFixed(1)} gap_since_last_frame_ms=${health.gapSinceLastFrameMs}`;
}

/** Pure state transition: advances `lastProgressAt` only when
 * `framesDecoded` has genuinely increased since the last observation. A
 * `null` reading (stats temporarily unavailable) preserves the existing
 * state rather than being treated as "no progress" -- a transient stats
 * read failure must never itself trigger a false-positive stale flag. */
export function nextCameraFreezeState(
  previous: CameraFreezeState | undefined,
  framesDecoded: number | null,
  now: number
): CameraFreezeState {
  if (framesDecoded === null) {
    return previous ?? { lastFramesDecoded: -1, lastProgressAt: now };
  }
  if (!previous || framesDecoded > previous.lastFramesDecoded) {
    return { lastFramesDecoded: framesDecoded, lastProgressAt: now };
  }
  return previous;
}

/** Pure decision: has decode progress been stalled for at least
 * `timeoutMs`? Mirrors native's `no_frame_decision`. */
export function isCameraFrameStale(
  state: CameraFreezeState,
  now: number,
  timeoutMs: number = FREEZE_WATCHDOG_TIMEOUT_MS
): boolean {
  return now - state.lastProgressAt >= timeoutMs;
}

/** Extract the decoder's cumulative `framesDecoded` counter for the video
 * inbound-rtp stat in a getRTCStatsReport() result, or null if unavailable
 * (report missing, or no matching video inbound-rtp entry yet). */
export function framesDecodedFromStatsReport(report: RTCStatsReport | undefined): number | null {
  if (!report) return null;
  let framesDecoded: number | null = null;
  report.forEach((stat) => {
    const s = stat as { type?: string; kind?: string; framesDecoded?: unknown };
    if (s.type === 'inbound-rtp' && s.kind === 'video' && typeof s.framesDecoded === 'number') {
      framesDecoded = s.framesDecoded;
    }
  });
  return framesDecoded;
}
