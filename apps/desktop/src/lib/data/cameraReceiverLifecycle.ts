// Durable gallery-bridge receiver LIFECYCLE evidence (framework-free, so it is
// directly unit-testable without a Tauri/livekit-client runtime -- same reason
// cameraFreezeWatchdog.ts is separate).
//
// Why this exists: the periodic `CameraReceiverInterval` record
// (galleryBridge.ts -> diagnostics.rs) only fires while the webview is actively
// sampling a SUBSCRIBED camera. An absent interval is therefore ambiguous -- it
// can mean "no camera was published", "the bridge never subscribed", or "the
// bridge never even connected". A real macOS session produced zero receiver
// records while the user was watching a remote camera, and nothing in the log
// could say which of those three boundaries failed, because WebView console
// output never reaches petal.log. This module writes one bounded line per
// lifecycle edge, so the FIRST MISSING PHASE names the boundary that failed.
//
// Observational only, same contract as cameraFreezeWatchdog.ts: it never
// mutates stream state, never creates a journal entry, and every field that
// crosses the IPC boundary is bounded and control-character stripped.

export const CAMERA_RECEIVER_LIFECYCLE_ROUTE = 'gallery-webview';

/** Closed phase list. The webview is the only caller, so the closure is what
 * keeps a typo from silently writing an unattributable record. */
export const CAMERA_RECEIVER_LIFECYCLE_PHASES = [
  'bridge_connecting',
  'bridge_connected',
  'subscribe_requested',
  'subscribed',
  'first_decode',
  'unsubscribed',
  'bridge_disconnected',
  'interval_failed',
  'lifecycle_failed'
] as const;

export type CameraReceiverLifecyclePhase = (typeof CAMERA_RECEIVER_LIFECYCLE_PHASES)[number];

/** Hard caps applied on this side too: the native sink bounds again, but a
 * bounded payload also keeps one pathological track name from blowing up the
 * invoke it arrives on. */
export const LIFECYCLE_FIELD_LIMITS = {
  participantIdentity: 64,
  trackName: 64,
  trackSid: 64,
  detail: 96
} as const;

export interface CameraReceiverLifecycleInput {
  phase: CameraReceiverLifecyclePhase;
  participantIdentity?: string | null;
  trackName?: string | null;
  trackSid?: string | null;
  /** Bounded, non-remote diagnostic context. Never a raw error message. */
  detail?: string | null;
  bridgeAgeMs?: number | null;
}

/** Exactly the camelCase shape `record_camera_receiver_lifecycle` accepts. */
export interface CameraReceiverLifecyclePayload {
  phase: string;
  participantIdentity: string | null;
  trackName: string | null;
  trackSid: string | null;
  route: string;
  detail: string | null;
  bridgeAgeMs: number | null;
}

function boundedText(value: string | null | undefined, max: number): string | null {
  if (typeof value !== 'string') return null;
  const cleaned = value.replace(/[\u0000-\u001f\u007f]/g, '').trim();
  if (cleaned.length === 0) return null;
  return cleaned.slice(0, max);
}

function boundedPhase(phase: string): string {
  return (CAMERA_RECEIVER_LIFECYCLE_PHASES as readonly string[]).includes(phase)
    ? phase
    : 'unknown_phase';
}

/** Build the closed, allowlisted payload at this boundary. */
export function buildCameraReceiverLifecyclePayload(
  input: CameraReceiverLifecycleInput
): CameraReceiverLifecyclePayload {
  const age = input.bridgeAgeMs;
  return {
    phase: boundedPhase(input.phase),
    participantIdentity: boundedText(input.participantIdentity, LIFECYCLE_FIELD_LIMITS.participantIdentity),
    trackName: boundedText(input.trackName, LIFECYCLE_FIELD_LIMITS.trackName),
    trackSid: boundedText(input.trackSid, LIFECYCLE_FIELD_LIMITS.trackSid),
    route: CAMERA_RECEIVER_LIFECYCLE_ROUTE,
    detail: boundedText(input.detail, LIFECYCLE_FIELD_LIMITS.detail),
    bridgeAgeMs: typeof age === 'number' && Number.isFinite(age) && age >= 0 ? Math.round(age) : null
  };
}

export interface CameraReceiverLifecycleFailure {
  phase: string;
  attempt: number;
}

export interface CameraReceiverLifecycleRecorder {
  /** Await, do not fire-and-forget: the invoke's own resolve/reject IS the
   * success signal, and the previous `.catch(() => {})` is what made a silently
   * absent receiver boundary indistinguishable from a healthy one. */
  record(input: CameraReceiverLifecycleInput): Promise<boolean>;
  attempts(): number;
  failures(): number;
}

/**
 * Wrap the native lifecycle sink so a rejected invoke is VISIBLE instead of
 * swallowed, without ever placing the rejection text in a durable record (a
 * rejected Tauri invoke can carry a path or a token). A failure is reported as
 * a second, closed-list attempt with `phase: 'lifecycle_failed'`; that attempt
 * is terminal, so a dead IPC bridge can never recurse.
 */
export function createCameraReceiverLifecycleRecorder(options: {
  invoke: (payload: CameraReceiverLifecyclePayload) => Promise<unknown>;
  onFailure?: (failure: CameraReceiverLifecycleFailure) => void;
}): CameraReceiverLifecycleRecorder {
  let attemptCount = 0;
  let failureCount = 0;

  const attempt = async (payload: CameraReceiverLifecyclePayload): Promise<boolean> => {
    attemptCount += 1;
    try {
      await options.invoke(payload);
      return true;
    } catch {
      failureCount += 1;
      options.onFailure?.({ phase: payload.phase, attempt: attemptCount });
      return false;
    }
  };

  return {
    async record(input: CameraReceiverLifecycleInput): Promise<boolean> {
      const payload = buildCameraReceiverLifecyclePayload(input);
      if (await attempt(payload)) return true;
      // One terminal retry. `phase` here is always from the closed list, so the
      // diagnostic context we keep is bounded and never remote text.
      if (payload.phase === 'lifecycle_failed') return false;
      return attempt({
        ...payload,
        phase: 'lifecycle_failed',
        detail: `after=${payload.phase}`
      });
    },
    attempts: () => attemptCount,
    failures: () => failureCount
  };
}
