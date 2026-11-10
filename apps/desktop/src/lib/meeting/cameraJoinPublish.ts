// Camera publish from the meeting route's Video toggle, across the join seam.
//
// Clicking Camera before the native room commits made `start_camera_publish`
// refuse with "not in room", and the route showed the sticky "publishing to
// others failed" toast although Retry a moment later always worked (#503).
// While the meeting is connecting (first join, Try again, or a connection
// recovery) that refusal is deferred instead: no toast, the toggle stays ON,
// and the publish runs again once the meeting reports connected. Any other
// failure, or a refusal while connected, toasts with Retry as before (a
// refusal while connected also stays deferred for a following recovery).

import type { StartCameraPublishResult } from '#lib/ipc.js';
import type { MeetingPhase } from './meetingSession.svelte';

/** The native refusal for a publish with no committed room (camera_session.rs). */
export const NOT_IN_ROOM = 'not in room';

export interface CameraJoinPublishSeams {
  /** One `start_camera_publish_command` call. Rejects on refusal. */
  startPublish: () => Promise<StartCameraPublishResult>;
  /** The live meeting phase; recovery and Try again report 'connecting'. */
  meetingPhase: () => MeetingPhase;
  /** Publication succeeded: mark the camera on and clear any camera toast. */
  onPublished: () => void;
  /** First attempt failed but native self-heal is retrying. */
  onRetrying: () => void;
  /** A real failure: the sticky toast with Retry. */
  onFailed: (error: unknown) => void;
}

export interface CameraJoinPublish {
  /** Publish now (the Video toggle and its Retry). */
  publish: () => Promise<void>;
  /** Call whenever the meeting phase becomes 'connected'. */
  meetingConnected: () => Promise<void>;
  /** The camera was turned off or released: drop a deferred publish. */
  cancel: () => void;
  /** True while a "not in room" refusal waits for the next connected edge. */
  readonly deferred: boolean;
}

export function isNotInRoomRefusal(error: unknown): boolean {
  const message = typeof error === 'string' ? error : (error as { message?: unknown })?.message;
  return typeof message === 'string' && message.trim() === NOT_IN_ROOM;
}

export function createCameraJoinPublish(seams: CameraJoinPublishSeams): CameraJoinPublish {
  let deferred = false;
  // Bumped by cancel() and each publish, so a refusal that lands after the
  // user turned the camera off (or after a newer attempt) can't re-arm it.
  let request = 0;

  async function publish(): Promise<void> {
    const id = ++request;
    deferred = false;
    const phaseAtStart = seams.meetingPhase();
    let result: StartCameraPublishResult;
    try {
      result = await seams.startPublish();
    } catch (error) {
      if (!isNotInRoomRefusal(error)) {
        seams.onFailed(error);
        return;
      }
      if (id !== request) return;
      // Keep the intent for the next connected edge either way: a refusal
      // while the route still reads 'connected' can be the start of a lost
      // connection, and the recovery should still publish it.
      deferred = true;
      const joining = phaseAtStart === 'connecting' || seams.meetingPhase() === 'connecting';
      if (!joining) {
        seams.onFailed(error);
        return;
      }
      // The meeting connected while this call was in flight: its connected
      // edge has passed, so retry now (a refusal while connected toasts).
      if (seams.meetingPhase() === 'connected') {
        await publish();
      }
      return;
    }
    if (result.published === false) {
      seams.onRetrying();
      return;
    }
    seams.onPublished();
  }

  async function meetingConnected(): Promise<void> {
    if (!deferred) return;
    await publish();
  }

  function cancel() {
    request += 1;
    deferred = false;
  }

  return {
    publish,
    meetingConnected,
    cancel,
    get deferred() {
      return deferred;
    }
  };
}
