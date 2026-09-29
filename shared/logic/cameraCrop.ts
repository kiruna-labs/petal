// SINGLE SOURCE OF TRUTH for how far a camera tile may crop its video (#248).
// Shared by the desktop gallery (apps/desktop/src/lib/components/
// ParticipantTile.svelte, Gallery.svelte) and the web client
// (web-harness/src/cameraFit.ts, tileLayout.ts), so both render the same
// frame for the same tile. Pure: no DOM -- callers pass the video's intrinsic
// size and the tile's box.
//
// #204 kept every tile a letterboxed 16:9 box, which left two people on a
// landscape phone as two small tiles with a band of black around them. The
// maintainer is fine with cropping camera video to fill the tile, within
// limits: the sides may lose up to about a third of the width, but the top
// and bottom at most 10% (heads live there). Past either limit the tile
// letterboxes instead -- a portrait phone camera in a landscape tile keeps
// the whole face rather than being cut to a strip. Shared windows are never
// cropped by this module; they always fit (`contain`).

export type CameraFit = 'cover' | 'contain';

export interface CropSize {
  width: number;
  height: number;
}

export interface CropRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** At most this fraction of the video's width may be cropped off the sides. */
export const CAMERA_MAX_SIDE_CROP = 1 / 3;
/** At most this fraction of the video's height may be cropped off the top
 * and bottom together. */
export const CAMERA_MAX_VERTICAL_CROP = 0.1;
/** Layout rounds tile boxes to device pixels, so a tile the packer sized at
 * exactly the side cap can come out a fraction of a pixel narrower. Without
 * this slack that tile would flip to a letterbox on a rounding error. */
const CROP_CAP_TOLERANCE = 0.005;

/** The camera shape the packer plans for: a landscape 16:9 webcam. Other
 * shapes still render correctly -- `cameraFit` decides per video -- they just
 * do not steer the grid. */
export const CAMERA_PLANNING_ASPECT = 16 / 9;

/**
 * The tile aspect range camera-only layouts may pack at
 * (`computeGalleryLayout`'s `tileAspectRange`). The narrow end is exactly
 * where a 16:9 camera reaches the side cap (16:9 x 2/3 = 32:27, about 7:6);
 * the wide end stays 16:9 so a planned camera is never cropped top/bottom.
 * Every packed camera tile therefore shows a 16:9 camera with `cover`.
 */
export const CAMERA_TILE_ASPECT_RANGE = {
  min: CAMERA_PLANNING_ASPECT * (1 - CAMERA_MAX_SIDE_CROP),
  max: CAMERA_PLANNING_ASPECT
} as const;

function validSize(size: CropSize): boolean {
  return Number.isFinite(size.width) && Number.isFinite(size.height) && size.width > 0 && size.height > 0;
}

/**
 * The fraction of the video `object-fit: cover` would crop in `box`:
 * `sides` of its width, `vertical` of its height (only one is ever non-zero).
 */
export function coverCropFractions(media: CropSize, box: CropSize): { sides: number; vertical: number } {
  if (!validSize(media) || !validSize(box)) return { sides: 0, vertical: 0 };
  const mediaAspect = media.width / media.height;
  const boxAspect = box.width / box.height;
  if (mediaAspect > boxAspect) return { sides: 1 - boxAspect / mediaAspect, vertical: 0 };
  return { sides: 0, vertical: 1 - mediaAspect / boxAspect };
}

/**
 * `cover` when filling `box` stays within both crop caps, else `contain`.
 * An unknown video size (no frame decoded yet) answers `contain`: nothing is
 * cropped until the real shape is known.
 */
export function cameraFit(media: CropSize, box: CropSize): CameraFit {
  if (!validSize(media) || !validSize(box)) return 'contain';
  const crop = coverCropFractions(media, box);
  if (crop.sides > CAMERA_MAX_SIDE_CROP + CROP_CAP_TOLERANCE) return 'contain';
  if (crop.vertical > CAMERA_MAX_VERTICAL_CROP + CROP_CAP_TOLERANCE) return 'contain';
  return 'cover';
}

/**
 * Where the video's picture actually lands for `fit` inside `box` (same
 * coordinate space as `box`). For `cover` the rect is larger than the box
 * and centred on it -- the tile clips the overhang. Overlays drawn in video
 * coordinates (camera-tile drawings) must map through THIS rect, not the
 * tile, or they drift off the picture as soon as it is cropped.
 */
export function renderedMediaRect(box: CropRect, media: CropSize, fit: CameraFit): CropRect {
  if (!validSize(media) || !validSize(box)) return box;
  const scale =
    fit === 'cover'
      ? Math.max(box.width / media.width, box.height / media.height)
      : Math.min(box.width / media.width, box.height / media.height);
  const width = media.width * scale;
  const height = media.height * scale;
  return {
    left: box.left + (box.width - width) / 2,
    top: box.top + (box.height - height) / 2,
    width,
    height
  };
}
