export const CAMERA_DECODE_HEALTH_LOG_MS = 5_000;

export interface CameraDecodeHealthState {
  lastLoggedAt: number;
  lastLoggedFramesDecoded: number;
  lastProgressAt: number;
}

export interface CameraDecodeHealth {
  identity: string;
  trackName: string;
  framesDecoded: number | null;
  decodedFps: number;
  gapSinceLastFrameMs: number;
}

export function framesDecodedFromStatsReport(report: RTCStatsReport | null | undefined): number | null {
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

export function nextCameraDecodeHealthState(
  previous: CameraDecodeHealthState | undefined,
  framesDecoded: number | null,
  now: number,
  logIntervalMs: number = CAMERA_DECODE_HEALTH_LOG_MS
): { state: CameraDecodeHealthState; health: Omit<CameraDecodeHealth, 'identity' | 'trackName'> | null } {
  const previousFrames = previous?.lastLoggedFramesDecoded ?? 0;
  const currentFrames = framesDecoded ?? previousFrames;
  const lastProgressAt =
    framesDecoded !== null && (!previous || framesDecoded > previous.lastLoggedFramesDecoded)
      ? now
      : (previous?.lastProgressAt ?? now);

  if (!previous) {
    return {
      state: { lastLoggedAt: now, lastLoggedFramesDecoded: currentFrames, lastProgressAt },
      health: null,
    };
  }

  const elapsedMs = now - previous.lastLoggedAt;
  if (elapsedMs < logIntervalMs) {
    // Keep both boundaries at the last emitted health line so decoded_fps
    // covers the same full window as frames_decoded (#623).
    return {
      state: { ...previous, lastProgressAt },
      health: null,
    };
  }

  return {
    state: { lastLoggedAt: now, lastLoggedFramesDecoded: currentFrames, lastProgressAt },
    health: {
      framesDecoded,
      decodedFps:
        framesDecoded === null || elapsedMs <= 0
          ? 0
          : (Math.max(0, framesDecoded - previous.lastLoggedFramesDecoded) * 1000) / elapsedMs,
      gapSinceLastFrameMs: now - lastProgressAt,
    },
  };
}

export function formatCameraDecodeHealth(health: CameraDecodeHealth): string {
  const frames = health.framesDecoded === null ? 'unknown' : String(health.framesDecoded);
  return `camera decode health: ${health.identity} / ${health.trackName || '(unnamed)'} -- frames_decoded=${frames} decoded_fps=${health.decodedFps.toFixed(1)} gap_since_last_frame_ms=${health.gapSinceLastFrameMs}`;
}
