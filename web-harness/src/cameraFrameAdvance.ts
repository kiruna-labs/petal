/**
 * CAM-N2W's verdict logic (journey CAM-05, #815): given one window of
 * measurements off a received camera tile, did the web peer actually SEE the
 * native camera?
 *
 * Two rules shape everything here.
 *
 * "Arrived" is not "visible" (#806): a subscribed track, or even advancing
 * decode counters, says nothing about what is on screen. The verdict needs the
 * drawn pixels.
 *
 * An instrument that cannot see must never report black (#821): a canvas that
 * refuses to read back (tainted, no context, a compositor path that hands out
 * empty buffers) produces exactly the same all-zero pixels as a genuinely
 * black tile. That ambiguity is what turned a blind audio receiver into a P0
 * filed against a working product. So the caller draws a known white rectangle
 * into the SAME canvas first and reports whether it read back white; only with
 * that positive control green is "the pixels are black" allowed to become a
 * product verdict.
 */

/** Luma at or below this counts as black. */
export const BLACK_FLOOR = 16;

/**
 * The readback canvas is deliberately much smaller than the tile: the verdict
 * needs "is anything lit, and did it change", not fidelity, and a full-size
 * `getImageData` per sample is expensive enough to disturb the frame cadence
 * this same window is measuring.
 */
export const CAMERA_SAMPLE_WIDTH = 320;
export const CAMERA_SAMPLE_HEIGHT = 240;

/** At least this fraction of sampled pixels must be above the black floor. */
export const MIN_NON_BLACK_RATIO = 0.02;

/**
 * Below this mean luma delta between the first and last sample, the picture is
 * frozen.
 *
 * Deliberately just above numerical noise rather than above sensor noise. A
 * genuinely held frame is the SAME decoded picture twice and reads exactly 0,
 * so the bar does not need to be high to catch one — while a high bar would
 * call a real camera pointed at a still, well-lit scene "frozen", which is a
 * product failure the product did not commit.
 */
export const MIN_INTER_FRAME_DIFF = 0.05;

/** Frames (callbacks or decoded) needed inside the window to call the stream advancing. */
export const MIN_ADVANCING_FRAMES = 2;

/**
 * Pixel readbacks a verdict needs. Two, never one: the freeze question is a
 * question about the DIFFERENCE between samples, so a single readback cannot
 * answer it however good the pixels in it look.
 */
export const REQUIRED_SAMPLED_FRAMES = 2;

export interface RemoteCameraSample {
  /** `HTMLVideoElement.readyState` at the end of the window. */
  readyState: number;
  videoWidth: number;
  videoHeight: number;
  /** `requestVideoFrameCallback` firings during the window. */
  frameCallbackCount: number;
  /** inbound-rtp `framesDecoded` delta over the window; null when stats were unavailable. */
  framesDecodedDelta: number | null;
  windowMs: number;
  /**
   * Positive control: a known white rectangle drawn into the SAME canvas read
   * back as white. False means the readback path is blind, so a black reading
   * proves nothing about the product.
   */
  canvasControlOk: boolean;
  /** How many frames were actually read back off the video element. */
  sampledFrames: number;
  /** Highest luma seen across all sampled pixels, 0..255. */
  maxLuma: number;
  /** Fraction of sampled pixels above the black floor, 0..1. */
  nonBlackRatio: number;
  /** Mean absolute luma difference between the first and last sampled frame, 0..255. */
  interFrameDiff: number;
}

export type RemoteCameraVerdict =
  | { ok: true; classification: 'PASS'; detail: string }
  | { ok: false; classification: 'TEST-FAIL' | 'INFRA-FAIL'; detail: string };

function fpsOf(sample: RemoteCameraSample): number {
  const seconds = Math.max(sample.windowMs / 1000, 0.001);
  return sample.frameCallbackCount / seconds;
}

function decodedLabel(sample: RemoteCameraSample): string {
  return sample.framesDecodedDelta === null ? 'unavailable' : String(sample.framesDecodedDelta);
}

export function evaluateRemoteCameraTile(sample: RemoteCameraSample): RemoteCameraVerdict {
  if (sample.sampledFrames === 0) {
    return {
      ok: false,
      classification: 'INFRA-FAIL',
      detail: `no frame could be read back off the video element in ${sample.windowMs}ms -- there is no verdict to give about the tile`,
    };
  }

  // One readback cannot answer the question this scenario is FOR. A held last
  // frame and a live picture are identical in a single sample and in every
  // counter; only the difference between two samples separates them. Passing
  // off one would be the weaker instrument quietly reporting success.
  if (sample.sampledFrames < REQUIRED_SAMPLED_FRAMES) {
    return {
      ok: false,
      classification: 'INFRA-FAIL',
      detail: `only ${sample.sampledFrames} of ${REQUIRED_SAMPLED_FRAMES} pixel readbacks succeeded -- a live picture and a held frame are indistinguishable from a single sample`,
    };
  }

  if (!sample.canvasControlOk) {
    return {
      ok: false,
      classification: 'INFRA-FAIL',
      detail:
        'the canvas positive control failed (a known white rectangle did not read back white) -- this viewer cannot see, so its pixel reading is not evidence either way',
    };
  }

  if (sample.framesDecodedDelta !== null && sample.framesDecodedDelta < 0) {
    return {
      ok: false,
      classification: 'INFRA-FAIL',
      detail: `receiver stats went backwards mid-window (framesDecoded delta ${sample.framesDecodedDelta}, track resubscribed) -- the window is not measurable`,
    };
  }

  if (sample.videoWidth <= 0 || sample.videoHeight <= 0 || sample.readyState < 2) {
    return {
      ok: false,
      classification: 'TEST-FAIL',
      detail: `the camera tile has no decoded picture: ${sample.videoWidth}x${sample.videoHeight} readyState=${sample.readyState}`,
    };
  }

  const advancing =
    sample.frameCallbackCount >= MIN_ADVANCING_FRAMES ||
    (sample.framesDecodedDelta ?? 0) >= MIN_ADVANCING_FRAMES;
  if (!advancing) {
    return {
      ok: false,
      classification: 'TEST-FAIL',
      detail: `the camera tile is not advancing: ${sample.frameCallbackCount} frame callback(s) and framesDecoded delta ${decodedLabel(sample)} over ${sample.windowMs}ms`,
    };
  }

  if (sample.maxLuma <= BLACK_FLOOR || sample.nonBlackRatio < MIN_NON_BLACK_RATIO) {
    return {
      ok: false,
      classification: 'TEST-FAIL',
      detail: `the camera tile is BLACK: maxLuma=${sample.maxLuma} nonBlackRatio=${sample.nonBlackRatio.toFixed(4)} over ${sample.sampledFrames} sampled frame(s), with the canvas control green -- frames are arriving but nothing is visible`,
    };
  }

  if (sample.interFrameDiff <= MIN_INTER_FRAME_DIFF) {
    return {
      ok: false,
      classification: 'TEST-FAIL',
      detail: `the camera tile is FROZEN: decoded frames advanced (${sample.frameCallbackCount} callbacks, framesDecoded delta ${decodedLabel(sample)}) but the picture did not change (mean luma delta ${sample.interFrameDiff.toFixed(3)} between the first and last sample)`,
    };
  }

  return {
    ok: true,
    classification: 'PASS',
    detail: `native camera visible in the web tile: ${sample.videoWidth}x${sample.videoHeight} at ${fpsOf(sample).toFixed(1)}fps, framesDecoded delta ${decodedLabel(sample)}, nonBlackRatio=${sample.nonBlackRatio.toFixed(3)}, interFrameDiff=${sample.interFrameDiff.toFixed(2)}`,
  };
}
