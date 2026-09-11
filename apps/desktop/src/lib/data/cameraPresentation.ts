// Shared, bounded store for the WebView's own presentation measurements.
//
// The receiver stats in `cameraFreezeWatchdog.ts` describe what the DECODER
// did; they say nothing about whether the browser actually presented those
// frames. `ParticipantTile.svelte` is the only place that can observe the
// presentation boundary (`requestVideoFrameCallback`), and `galleryBridge.ts`
// is the only place that owns the durable receiver-interval record. This tiny
// module is the seam between them so one record can carry both.
//
// It deliberately holds only the latest aggregate per identity: no frames, no
// element handles, no unbounded history.

export interface CameraPresentationSample {
  presentedFrames: number;
  presentedFps: number;
  /** `HTMLMediaElement.readyState` at sample time, or null when unknown. */
  readyState: number | null;
  videoPaused: boolean | null;
}

const samples = new Map<string, CameraPresentationSample>();

/** Record the latest presentation aggregate for one remote identity. */
export function recordCameraPresentation(identity: string, sample: CameraPresentationSample): void {
  samples.set(identity, sample);
}

/** Latest presentation aggregate for one remote identity, if it has been
 * sampled since the tile mounted. */
export function cameraPresentationFor(identity: string): CameraPresentationSample | null {
  return samples.get(identity) ?? null;
}

/** Drop one identity's presentation state (tile unmount, unsubscribe, or
 * participant departure). */
export function clearCameraPresentation(identity: string): void {
  samples.delete(identity);
}

/** Drop every entry (room disconnect). */
export function clearAllCameraPresentations(): void {
  samples.clear();
}
