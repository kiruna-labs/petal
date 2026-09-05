import type { NetworkSnapshot, TrackHealth } from '$lib/ipc';

export type { TrackHealth } from '$lib/ipc';

export const DEBUG_LAST_FRAME_STALE_MS = 3000;

export interface LastFrameAgeLabel {
  label: string;
  stale: boolean;
}

export interface GlassToGlassLabel {
  label: string;
  caveat: string;
}

export interface GlassToGlassChip {
  /** Compact text for a header chip, e.g. "42 ms" (measured) or "~42 ms" (estimate). */
  text: string;
  /** True when `text` is derived from the RTT/jitter-buffer estimate, not a clock-calibrated measurement. */
  estimate: boolean;
  title: string;
}

export function findRemoteWindowDebugTrack(
  snapshot: Pick<NetworkSnapshot, 'tracks'> | null | undefined,
  ownerIdentity: string,
  windowId: number
): TrackHealth | null {
  return (
    snapshot?.tracks.find(
      (track) =>
        track.direction === 'recv' &&
        track.kind === 'video' &&
        track.ownerIdentity === ownerIdentity &&
        track.windowId === windowId
    ) ?? null
  );
}

export function findLocalWindowDebugTrack(
  snapshot: Pick<NetworkSnapshot, 'tracks' | 'localIdentity'> | null | undefined,
  windowId: number
): TrackHealth | null {
  return (
    snapshot?.tracks.find(
      (track) =>
        track.direction === 'send' &&
        track.kind === 'video' &&
        track.windowId === windowId &&
        (!snapshot.localIdentity || !track.ownerIdentity || track.ownerIdentity === snapshot.localIdentity)
    ) ?? null
  );
}

export function formatDebugNumber(value: number | null | undefined, digits = 0): string {
  return typeof value === 'number' && Number.isFinite(value) ? value.toFixed(digits) : 'n/a';
}

export function formatDebugMetric(value: number | null | undefined, digits: number, unit: string): string {
  const formatted = formatDebugNumber(value, digits);
  return formatted === 'n/a' ? 'n/a' : `${formatted} ${unit}`;
}

export function formatSharedBy(displayName: string | null | undefined, identity: string | null | undefined): string {
  const cleanName = displayName?.trim();
  const cleanIdentity = identity?.trim();
  if (cleanName && cleanIdentity && cleanName !== cleanIdentity) return `${cleanName} (${cleanIdentity})`;
  return cleanName || cleanIdentity || 'Unknown';
}

export function formatDebugResolution(track: TrackHealth | null): string {
  if (!track || track.width <= 0 || track.height <= 0) return 'n/a';
  return `${track.width}x${track.height}`;
}

export function formatPacketLossCumulative(track: TrackHealth | null): string {
  return `${formatDebugNumber(track?.packetsLost, 0)} cumulative`;
}

/**
 * "FPS shared" while capture is idle is not comparable to "FPS captured": a
 * once-per-second keepalive re-pushes the last frame into the encoder while
 * the source is unchanged (session/share.rs `idle_refresh_frame_at`), so the
 * encoder's frame counter ticks up even though nothing new was captured.
 * Label that explicitly instead of presenting it as an ordinary encode rate.
 */
export function formatSharedFps(track: TrackHealth | null): string {
  const fps = track?.encodedSent?.fps ?? track?.fps ?? null;
  const formatted = formatDebugNumber(fps, 1);
  const idle = track?.captureState?.state === 'idle';
  if (idle && typeof fps === 'number' && fps > 0) return `${formatted} (idle keepalive)`;
  return formatted;
}

export function formatFrameCounters(track: TrackHealth | null, framesReceived: number | null | undefined): string {
  const received = formatDebugNumber(framesReceived, 0);
  const decoded = formatDebugNumber(track?.framesDecoded, 0);
  const dropped = formatDebugNumber(track?.framesDropped, 0);
  return `${received} pushed / ${decoded} decoded / ${dropped} dropped`;
}

export function formatLastFrameAge(
  lastFrameReceivedMs: number | null | undefined,
  nowMs = Date.now()
): LastFrameAgeLabel {
  if (!lastFrameReceivedMs) return { label: 'n/a', stale: true };
  const ageMs = Math.max(0, nowMs - lastFrameReceivedMs);
  const label = ageMs < 1000 ? `${Math.round(ageMs)} ms ago` : `${(ageMs / 1000).toFixed(1)} s ago`;
  return {
    label,
    stale: ageMs >= DEBUG_LAST_FRAME_STALE_MS
  };
}

export function formatGlassToGlassLatency(track: TrackHealth | null): GlassToGlassLabel {
  if (!track) {
    return {
      label: 'n/a',
      caveat: 'Measured values need comparable sender and receiver clocks; estimate appears when available.'
    };
  }
  if (track.glassToGlassMs !== null && track.glassToGlassMs !== undefined) {
    return {
      label: `${formatDebugNumber(track.glassToGlassMs, 1)} ms measured`,
      caveat: 'Measured uses data-channel clock calibration before comparing sender and receiver timestamps.'
    };
  }
  if (track.glassToGlassEstimateMs !== null && track.glassToGlassEstimateMs !== undefined) {
    return {
      label: `${formatDebugNumber(track.glassToGlassEstimateMs, 1)} ms estimate`,
      caveat:
        track.glassToGlassStatus === 'clock-sync-pending'
          ? 'Exact measurement is waiting for clock calibration; estimate uses RTT/2 plus receiver jitter buffer and render budget.'
          : 'Estimate uses RTT/2 plus receiver jitter buffer and render budget.'
    };
  }
  return {
    label: 'n/a',
    caveat: 'Measured values need comparable sender and receiver clocks; estimate appears when available.'
  };
}

/**
 * Compact variant of `formatGlassToGlassLatency` for the header's Control
 * pill (#376 item 4): no track/no value returns null so callers hide the
 * chip entirely rather than showing a fake "n/a" -- this is a "while
 * controlling" affordance, not a debug field, so there's nothing useful to
 * show when latency isn't known. Estimated values are ALWAYS prefixed with
 * "~" per the never-show-estimates-as-precise rule; measured values never
 * are.
 */
export function formatGlassToGlassLatencyChip(track: TrackHealth | null): GlassToGlassChip | null {
  if (!track) return null;
  if (track.glassToGlassMs !== null && track.glassToGlassMs !== undefined) {
    return {
      text: `${Math.round(track.glassToGlassMs)} ms`,
      estimate: false,
      title: 'Measured glass-to-glass latency (data-channel clock calibration).'
    };
  }
  if (track.glassToGlassEstimateMs !== null && track.glassToGlassEstimateMs !== undefined) {
    return {
      text: `~${Math.round(track.glassToGlassEstimateMs)} ms`,
      estimate: true,
      title: 'Estimated glass-to-glass latency (RTT/2 plus receiver jitter buffer and render budget) -- not an exact measurement.'
    };
  }
  return null;
}
