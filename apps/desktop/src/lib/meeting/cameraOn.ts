import type { StartCameraPublishResult } from '#lib/ipc.js';

/** What turning the camera on in the meeting route needs from its surroundings. */
export interface CameraOnSteps {
  /** Bring up the local self-view; false if the camera can't be opened. */
  acquireSelfView(): Promise<boolean>;
  /** Resolves when the meeting is connected (true) or has failed (false).
   * Asked after a refusal, so an already-"connected" phase is only trusted
   * once it has held for `settleMs` (it may not have heard room-left yet). */
  whenJoined(settleMs: number): Promise<boolean>;
  /** False once the user turned the camera off (or retried) meanwhile. */
  stillWanted(): boolean;
  /** The native `start_camera_publish` command. */
  startPublish(): Promise<StartCameraPublishResult>;
}

/**
 * - `no-preview`: the self-view failed (it already said why).
 * - `join-failed`: the meeting could not connect (it shows why).
 * - `superseded`: the user turned the camera off meanwhile.
 * - `published`: others can see the camera.
 * - `publishing`: native is still bringing it up in the background; a real
 *   failure arrives later as `camera-publish-state`. Nothing to show.
 * - `failed`: publishing was refused with the meeting connected.
 */
/** How long a refusal waits for a "connected" meeting to report it dropped
 * (room-left trails native's own leave) before treating it as real. */
export const REFUSAL_SETTLE_MS = 1000;

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
 * moment the room is committed. If it still refuses (the meeting isn't
 * connected yet, or is reconnecting), wait for the meeting to connect and
 * try once more — silently. Only a refusal once connected is an error.
 */
export async function turnCameraOnSequence(steps: CameraOnSteps): Promise<CameraOnOutcome> {
  if (!(await steps.acquireSelfView())) return { kind: 'no-preview' };
  let result: StartCameraPublishResult;
  try {
    result = await steps.startPublish();
  } catch {
    if (!steps.stillWanted()) return { kind: 'superseded' };
    if (!(await steps.whenJoined(REFUSAL_SETTLE_MS))) return { kind: 'join-failed' };
    if (!steps.stillWanted()) return { kind: 'superseded' };
    try {
      result = await steps.startPublish();
    } catch (error) {
      if (!steps.stillWanted()) return { kind: 'superseded' };
      return { kind: 'failed', error };
    }
  }
  if (!steps.stillWanted()) return { kind: 'superseded' };
  return result.published ? { kind: 'published' } : { kind: 'publishing' };
}
