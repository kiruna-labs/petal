import { platformKey, type PlatformKey } from '$lib/platform';

export type RectLike = {
  left: number;
  top: number;
  width: number;
  height: number;
};

export type SizeLike = {
  width: number;
  height: number;
};

export type PointLike = {
  x: number;
  y: number;
};

function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}

export function containedMediaRect(bounds: RectLike, media: SizeLike): RectLike {
  if (bounds.width <= 0 || bounds.height <= 0 || media.width <= 0 || media.height <= 0) {
    return bounds;
  }

  const boundsAspect = bounds.width / bounds.height;
  const mediaAspect = media.width / media.height;

  if (mediaAspect > boundsAspect) {
    const height = bounds.width / mediaAspect;
    return {
      left: bounds.left,
      top: bounds.top + (bounds.height - height) / 2,
      width: bounds.width,
      height
    };
  }

  const width = bounds.height * mediaAspect;
  return {
    left: bounds.left + (bounds.width - width) / 2,
    top: bounds.top,
    width,
    height: bounds.height
  };
}

export function normalizedControlPoint(
  bounds: RectLike,
  media: SizeLike,
  point: PointLike
): PointLike | null {
  const content =
    media.width > 0 && media.height > 0 ? containedMediaRect(bounds, media) : bounds;
  if (content.width <= 0 || content.height <= 0) return null;

  return {
    x: clamp01((point.x - content.left) / content.width),
    y: clamp01((point.y - content.top) / content.height)
  };
}

export type KeyChordLike = {
  key?: string;
  code?: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
};

export type RemoteClipboardOperation = 'copy' | 'paste';

/**
 * Match the native Copy/Paste shortcut for the controller platform. Logical
 * `key` wins over physical `code` so non-US layouts do not reinterpret an
 * unrelated shortcut; `code` is only a fallback when the browser reports no
 * logical key. The caller suppresses both raw events when this matches.
 */
export function remoteClipboardChord(
  event: KeyChordLike,
  platform: PlatformKey = platformKey()
): RemoteClipboardOperation | null {
  if (event.altKey || event.shiftKey) return null;
  const mac = platform === 'macos' && event.metaKey && !event.ctrlKey;
  const windows = platform === 'windows' && event.ctrlKey && !event.metaKey;
  if (!mac && !windows) return null;

  const key = event.key ?? '';
  if (key) {
    const logical = key.toLowerCase();
    if (logical === 'c') return 'copy';
    if (logical === 'v') return 'paste';
    return null;
  }
  if (event.code === 'KeyC') return 'copy';
  if (event.code === 'KeyV') return 'paste';
  return null;
}

/** Backwards-compatible predicate for callers/tests that only need Paste. */
export function isPasteChord(event: KeyChordLike): boolean {
  return remoteClipboardChord(event, 'macos') === 'paste';
}
