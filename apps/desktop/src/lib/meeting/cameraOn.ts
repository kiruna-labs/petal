import type { CameraPublishState, StartCameraPublishResult } from '#lib/ipc.js';

/** What turning the camera on in the meeting route needs from its surroundings. */
export interface CameraOnSteps {
  /** Bring up the local self-view; false if the camera can't be opened. */
  acquireSelfView(): Promise<boolean>;
  /** Resolves when the meeting is connected (true) or has failed (false). */
  whenJoined(): Promise<boolean>;
  /** False once the user turned the camera off (or retried) meanwhile. */
  stillWanted(): boolean;
  /** The native `start_camera_publish` command. */
  startPublish(): Promise<StartCameraPublishResult>;
}

/**
 * - `no-preview`: the self-view failed (it already said why).
 * - `superseded`: the user turned the camera off meanwhile.
 * - `requested`: native published it, or owns it now (a heal, or a join it
 *   was deferred to); any later outcome arrives as `camera-publish-state`.
 * - `no-room`: native had no meeting to keep it for. Never a publish failure.
 */
export type CameraOnOutcome =
  | { kind: 'no-preview' }
  | { kind: 'superseded' }
  | { kind: 'requested'; result: StartCameraPublishResult }
  | { kind: 'no-room' };

/**
 * Turn the camera on: preview immediately, publish straight away. Native
 * owns a camera turned on while a join is pending (keyed to that join's room
 * generation) and publishes it at the commit, so a mid-join click is never
 * refused. Native refuses only when no join is pending at all: this route's
 * join hasn't reached native yet (wait for it and ask once more), or the
 * meeting is over. Neither is a publish failure, so neither is a toast.
 */
export async function turnCameraOnSequence(steps: CameraOnSteps): Promise<CameraOnOutcome> {
  if (!(await steps.acquireSelfView())) return { kind: 'no-preview' };
  for (let attempt = 0; ; attempt += 1) {
    try {
      const result = await steps.startPublish();
      return steps.stillWanted() ? { kind: 'requested', result } : { kind: 'superseded' };
    } catch {
      if (!steps.stillWanted()) return { kind: 'superseded' };
      if (attempt > 0 || !(await steps.whenJoined())) return { kind: 'no-room' };
      if (!steps.stillWanted()) return { kind: 'superseded' };
    }
  }
}

/**
 * What a `camera-publish-state` event does to the route, given the room
 * generation its own camera request is bound to. An outcome from an older
 * generation answers a request that is gone: it must neither stop the
 * current camera nor show a toast (PR #503).
 */
export function cameraPublishStateAction(
  event: CameraPublishState,
  boundGeneration: number
): 'ignore' | 'on' | 'off' | 'failed' {
  if (event.generation < boundGeneration) return 'ignore';
  if (event.publishing) return 'on';
  return event.error ? 'failed' : 'off';
}
