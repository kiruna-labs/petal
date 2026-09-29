import { cameraFit, type CameraFit } from '@petal/shared/logic/cameraCrop';

// ---------------------------------------------------------------------------
// Camera tiles crop to fill their box, within the shared caps (#248): a side
// crop of up to about a third of the width, at most 10% off the top and
// bottom, otherwise letterbox. The decision is per video and per box, so it
// is re-made whenever either changes shape: a new track or a rotated phone
// (the video's `resize` event) and every tile resize (grid repack, spotlight
// hero, strip thumbnail). CSS reads the answer from `data-fit`; shares never
// pass through here and always stay `contain`.
// ---------------------------------------------------------------------------

interface FitBinding {
  /** The tile currently observed -- always re-read from the DOM, never
   * assumed, so a video moved to another tile follows its new box. */
  tile: HTMLElement | null;
  observer: ResizeObserver | null;
}

const bindings = new WeakMap<HTMLVideoElement, FitBinding>();

/** Decide and record `cover` / `contain` for this camera video in its tile. */
export function syncCameraFit(tile: HTMLElement, video: HTMLVideoElement): CameraFit {
  const fit = cameraFit(
    { width: video.videoWidth, height: video.videoHeight },
    { width: tile.clientWidth, height: tile.clientHeight }
  );
  if (video.dataset.fit !== fit) video.dataset.fit = fit;
  return fit;
}

function unbind(binding: FitBinding) {
  binding.observer?.disconnect();
  binding.observer = null;
  binding.tile = null;
}

/** Re-read the video's tile, re-observe it if it changed, then decide. A
 * video that has left the document (its tile was removed) stops observing. */
function refresh(video: HTMLVideoElement, binding: FitBinding) {
  const tile = video.isConnected === false ? null : video.closest<HTMLElement>('.tile');
  if (!tile) {
    unbind(binding);
    return;
  }
  if (tile !== binding.tile) {
    binding.observer?.disconnect();
    binding.tile = tile;
    // Guarded like viewerDemand.ts: the harness also runs under node tests.
    binding.observer =
      typeof ResizeObserver === 'function' ? new ResizeObserver(() => refresh(video, binding)) : null;
    binding.observer?.observe(tile);
  }
  syncCameraFit(tile, video);
}

/** Keep `syncCameraFit` current for a camera video. Idempotent: binding the
 * same element again (a new track) only re-decides. */
export function bindCameraFit(video: HTMLVideoElement) {
  let binding = bindings.get(video);
  if (!binding) {
    const created: FitBinding = { tile: null, observer: null };
    binding = created;
    bindings.set(video, created);
    video.addEventListener('loadedmetadata', () => refresh(video, created));
    video.addEventListener('resize', () => refresh(video, created));
  }
  refresh(video, binding);
}
