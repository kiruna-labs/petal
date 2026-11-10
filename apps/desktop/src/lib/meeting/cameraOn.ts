import type { StartCameraPublishResult } from '#lib/ipc.js';
import type { RefusalPhase } from '#lib/meeting/joinGate.js';

/** What turning the camera on in the meeting route needs from its surroundings. */
export interface CameraOnSteps {
  /** Bring up the local self-view; false if the camera can't be opened. */
  acquireSelfView(): Promise<boolean>;
  /** Resolves when the meeting is connected (true) or has failed (false). */
  whenJoined(): Promise<boolean>;
  /** After a refusal: wait for the meeting to (re)connect, or report that it
   * stayed connected for `settleMs` (a real refusal) or disconnected. */
  afterRefusal(settleMs: number): Promise<RefusalPhase>;
  /** False once the user turned the camera off (or retried) meanwhile. */
  stillWanted(): boolean;
  /** The native `start_camera_publish` command. */
  startPublish(): Promise<StartCameraPublishResult>;
}

/** How long a refusal waits for a "connected" meeting to report it dropped
 * (room-left trails native's own leave) before treating it as real. */
export const REFUSAL_SETTLE_MS = 5000;

/**
 * - `no-preview`: the self-view failed (it already said why).
 * - `join-failed`: the meeting could not connect (it shows why).
 * - `superseded`: the user turned the camera off meanwhile.
 * - `published`: others can see the camera.
 * - `publishing`: native is bringing it up in the background in a connected
 *   meeting; a real failure arrives later as `camera-publish-state`.
 * - `failed`: publishing was refused with the meeting connected.
 */
export type CameraOnOutcome =
  | { kind: 'no-preview' }
  | { kind: 'join-failed' }
  | { kind: 'superseded' }
  | { kind: 'published' }
  | { kind: 'publishing' }
  | { kind: 'failed'; error: unknown };

/**
 * Turn the camera on: preview immediately, publish straight away. Native
 * keeps a camera turned on while a join is connecting and publishes it the
 * moment the room is committed. If it refuses (the meeting isn't connected
 * yet, or is dropping), wait for the meeting to (re)connect and try again —
 * silently. Every retry needs the phase to have moved, so this ends; only a
 * refusal while the meeting stayed connected is an error.
 */
export async function turnCameraOnSequence(steps: CameraOnSteps): Promise<CameraOnOutcome> {
  if (!(await steps.acquireSelfView())) return { kind: 'no-preview' };
  let result: StartCameraPublishResult;
  for (;;) {
    try {
      result = await steps.startPublish();
      break;
    } catch (error) {
      if (!steps.stillWanted()) return { kind: 'superseded' };
      const phase = await steps.afterRefusal(REFUSAL_SETTLE_MS);
      if (!steps.stillWanted()) return { kind: 'superseded' };
      if (phase === 'disconnected') return { kind: 'join-failed' };
      if (phase === 'held') return { kind: 'failed', error };
    }
  }
  if (!steps.stillWanted()) return { kind: 'superseded' };
  if (result.published) return { kind: 'published' };
  // Deferred to a join (or reconnect) still under way: if it fails, native
  // drops the camera, so the preview must go too (PR #503 F1). The route's
  // camera-publish-state listener isn't up yet during the first join.
  if (!(await steps.whenJoined())) {
    return steps.stillWanted() ? { kind: 'join-failed' } : { kind: 'superseded' };
  }
  return steps.stillWanted() ? { kind: 'publishing' } : { kind: 'superseded' };
}
