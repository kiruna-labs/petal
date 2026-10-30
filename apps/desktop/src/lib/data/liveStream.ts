// Pure helpers for the live-stream surfaces: the go-live dialog's preview
// line and bitrate field, the LIVE notice's elapsed time and stats, which
// shares carry a LIVE badge, and streamer mode's masking. No Tauri
// dependencies, so they are unit-tested.

import type { StreamLiveState, StreamPreview, StreamSessionSnapshot } from '../ipc.ts';

export const STREAM_BITRATE_MIN_MBPS = 1;
export const STREAM_BITRATE_MAX_MBPS = 50;

function mbps(bps: number): string {
  const value = bps / 1_000_000;
  return Number.isInteger(value) ? `${value}` : value.toFixed(1);
}

/** "2560×1440 source → 1920×1080 @ 30 fps, ~6 Mbps" */
export function streamPreviewLine(preview: StreamPreview): string {
  const { captureWidth, captureHeight, plan } = preview;
  return (
    `${captureWidth}×${captureHeight} source → ${plan.width}×${plan.height} @ ${plan.fps} fps, ` +
    `~${mbps(plan.bitrateBps)} Mbps`
  );
}

/** True when the share is scaled down to fit the stream's canvas. */
export function streamIsDownscaled(preview: StreamPreview): boolean {
  const { captureWidth, captureHeight, plan } = preview;
  return Math.min(plan.width / captureWidth, plan.height / captureHeight) < 1;
}

/**
 * The bitrate field: blank means Auto (`null`); otherwise whole or decimal
 * Mbps within 1–50. Returns `undefined` for invalid input.
 */
export function parseMaxBitrateMbps(input: string): number | null | undefined {
  const trimmed = input.trim();
  if (trimmed === '' || trimmed.toLowerCase() === 'auto') return null;
  if (!/^\d+(\.\d+)?$/.test(trimmed)) return undefined;
  const value = Number(trimmed);
  if (value < STREAM_BITRATE_MIN_MBPS || value > STREAM_BITRATE_MAX_MBPS) return undefined;
  return Math.round(value * 1_000_000);
}

/** "4:05" or "1:02:03" */
export function formatStreamElapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const ss = String(seconds).padStart(2, '0');
  return hours > 0 ? `${hours}:${String(minutes).padStart(2, '0')}:${ss}` : `${minutes}:${ss}`;
}

/** The sharer's notice line while live: "LIVE 4:05 · 5.8 Mbps · 3 dropped". */
export function streamStatusLine(snapshot: StreamSessionSnapshot, nowMs: number): string {
  const elapsed = formatStreamElapsed(nowMs - snapshot.startedAt);
  switch (snapshot.phase) {
    case 'connecting':
      return 'Connecting to the streaming server…';
    case 'reconnecting':
      return `Reconnecting (attempt ${snapshot.reconnectAttempt ?? 1} of 3)…`;
    case 'paused':
      return `LIVE ${elapsed} · Be right back${
        snapshot.slateRemainingS !== null ? ` · ends in ${snapshot.slateRemainingS}s` : ''
      }`;
    case 'live': {
      const parts = [`LIVE ${elapsed}`, `${mbps(snapshot.actualBitrateBps)} Mbps`];
      if (snapshot.droppedFrames > 0) parts.push(`${snapshot.droppedFrames} dropped`);
      return parts.join(' · ');
    }
  }
}

/**
 * Whether a share's surface carries the LIVE badge: it is one of the room's
 * live streams and not paused on its slate (off air).
 */
export function isShareOnAir(
  state: StreamLiveState | null,
  ownerIdentity: string | null | undefined,
  windowId: number
): boolean {
  if (!state?.live || !ownerIdentity) return false;
  return (state.streams ?? []).some(
    (stream) => !stream.paused && stream.windowId === windowId && stream.ownerIdentity === ownerIdentity
  );
}

/** Streamer mode's stand-in for a meeting's name or code. */
export const LIVE_MEETING_LABEL = 'Live meeting';
/** Streamer mode's stand-in for a recent room's name on the home window. */
export const HIDDEN_MEETING_LABEL = 'Hidden meeting';

/** The meeting's name as shown (top bar, window title, plugins). */
export function meetingLabelForDisplay(label: string, streamerMode: boolean): string {
  return streamerMode ? LIVE_MEETING_LABEL : label;
}

/** A recent room's name on the home window. */
export function recentRoomLabelForDisplay(label: string, streamerMode: boolean): string {
  return streamerMode ? HIDDEN_MEETING_LABEL : label;
}

/**
 * A room code's button (or label) on the home window: its text and accessible
 * labels. With `hidden` (streamer mode) the code is never shown, but the
 * button still copies the invite.
 */
export function roomCodeCopy(
  roomName: string,
  accessCode: string,
  hidden: boolean
): { text: string; label: string; copiedLabel: string; staticLabel: string } {
  if (hidden) {
    return {
      text: 'Copy invite',
      label: `${roomName}: click to copy invite`,
      copiedLabel: `Invite link copied for ${roomName}`,
      staticLabel: 'Room ID hidden'
    };
  }
  return {
    text: accessCode,
    label: `Room ID ${accessCode}, click to copy invite`,
    copiedLabel: `Invite link copied for ${roomName}, room ID ${accessCode}`,
    staticLabel: `Room ID ${accessCode}`
  };
}

/**
 * The LIVE notice's opt-out toggle is for people who aren't streaming: the
 * streamer's own setting never applies to their own stream.
 */
export function showsOptOutToggle(roomLive: boolean, streamingHere: boolean): boolean {
  return roomLive && !streamingHere;
}

/** Toast copy for a stream that ended. */
export function streamEndedMessage(reason: string, error?: string): string {
  switch (reason) {
    case 'slate-timeout':
      return 'Live stream ended: the shared window didn’t come back within 2 minutes.';
    case 'failed':
      return `Live stream failed${error ? `: ${error}` : ''}`;
    case 'left':
      return 'Live stream ended when you left the meeting.';
    default:
      return 'Live stream ended.';
  }
}
