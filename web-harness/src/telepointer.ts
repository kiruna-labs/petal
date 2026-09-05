import { isAiTrackName, type TelepointerMessage } from './trackNames.ts';

export interface RectLike {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface SizeLike {
  width: number;
  height: number;
}

export interface PointLike {
  x: number;
  y: number;
}

const WINDOW_TRACK_PREFIX = 'petal-window-';
const DARK_IDENTITY_INK = '#071018';
const LIGHT_IDENTITY_INK = '#ffffff';

export const IDENTITY_COLOR_PALETTE = [
  '#f06cc9',
  '#6e8bff',
  '#7ff0a3',
  '#e8b84b',
  '#d6b8f0',
  '#8fa6b8',
] as const;

const IDENTITY_INK_PALETTE = [
  '#2b071b',
  '#081129',
  '#062013',
  '#271b04',
  '#1f102b',
  DARK_IDENTITY_INK,
] as const;

function paletteIndexForIdentity(identity: string): number {
  let hash = 0;
  for (let i = 0; i < identity.length; i++) {
    hash = (hash * 31 + identity.charCodeAt(i)) >>> 0;
  }
  return hash % IDENTITY_COLOR_PALETTE.length;
}

function validPaletteIndex(index: number | null | undefined): index is number {
  return (
    typeof index === 'number' &&
    Number.isInteger(index) &&
    index >= 0 &&
    index < IDENTITY_COLOR_PALETTE.length
  );
}

function selectedPaletteIndex(identity: string, paletteIndex?: number | null): number {
  return validPaletteIndex(paletteIndex) ? paletteIndex : paletteIndexForIdentity(identity);
}

export function windowIdFromTrackName(trackName: string | undefined | null): number | null {
  // #657: an assistant voice track (`petal-ai-window-<id>`) is not a shared
  // window publication. Rejected explicitly rather than relying on the prefix
  // test alone -- every prefix parser in this client classifies the
  // `petal-ai-*` namespace, never lets it fall through.
  if (isAiTrackName(trackName)) return null;
  if (!trackName?.startsWith(WINDOW_TRACK_PREFIX)) return null;
  const raw = trackName.slice(WINDOW_TRACK_PREFIX.length);
  if (!/^\d+$/.test(raw)) return null;
  const id = Number(raw);
  if (!Number.isSafeInteger(id) || id < 1 || id > 0xffff_ffff) return null;
  return id;
}

export function telepointerKey(message: Pick<TelepointerMessage, 'userId' | 'windowId'>): string {
  return `${message.userId}:${message.windowId}`;
}

export function parseTelepointerPayload(payload: Uint8Array | string): TelepointerMessage | null {
  let text: string;
  try {
    text = typeof payload === 'string' ? payload : new TextDecoder().decode(payload);
  } catch {
    return null;
  }

  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }

  if (!raw || typeof raw !== 'object') return null;
  const candidate = raw as Partial<TelepointerMessage>;
  const windowId = candidate.windowId;
  const userId = candidate.userId;
  const visible = candidate.visible;
  const x = candidate.x;
  const y = candidate.y;
  const activity = candidate.activity;
  const surfaceOwnerId = candidate.surfaceOwnerId;
  if (
    typeof windowId !== 'number' ||
    !Number.isSafeInteger(windowId) ||
    windowId < 1 ||
    windowId > 0xffff_ffff ||
    typeof userId !== 'string' ||
    typeof visible !== 'boolean' ||
    typeof x !== 'number' ||
    typeof y !== 'number' ||
    !Number.isFinite(x) ||
    !Number.isFinite(y)
  ) {
    return null;
  }
  if (activity !== undefined && activity !== 'click' && activity !== 'type') {
    return null;
  }

  return {
    windowId,
    userId: userId.trim(),
    x,
    y,
    visible,
    ...(activity ? { activity } : {}),
    ...(typeof surfaceOwnerId === 'string' && surfaceOwnerId.trim()
      ? { surfaceOwnerId: surfaceOwnerId.trim() }
      : {}),
  };
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
      height,
    };
  }

  const width = bounds.height * mediaAspect;
  return {
    left: bounds.left + (bounds.width - width) / 2,
    top: bounds.top,
    width,
    height: bounds.height,
  };
}

export function telepointerPosition(
  bounds: RectLike,
  media: SizeLike,
  point: PointLike
): PointLike {
  const content = containedMediaRect(bounds, media);
  const x = Math.min(1, Math.max(0, point.x));
  const y = Math.min(1, Math.max(0, point.y));
  return {
    x: content.left + content.width * x,
    y: content.top + content.height * y,
  };
}

function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}

/**
 * #892: the ONE normalize-a-viewport-point-against-contained-media helper.
 * Was duplicated (drawSender.ts's `normalizedDrawPointInTile`, remoteControl.ts's
 * own copy) -- both called `containedMediaRect` but a caller could still pick
 * the wrong `bounds`, which is exactly how the draw-offset bug happened.
 * `remoteControl.ts` re-exports this rather than redefining it.
 */
export function normalizedPointInContainedMedia(
  bounds: RectLike,
  media: SizeLike,
  point: PointLike,
  options: { clamp?: boolean } = {}
): PointLike | null {
  const content = containedMediaRect(bounds, media);
  if (content.width <= 0 || content.height <= 0) return null;

  const x = (point.x - content.left) / content.width;
  const y = (point.y - content.top) / content.height;
  if (options.clamp === false && (x < 0 || x > 1 || y < 0 || y > 1)) return null;

  return { x: clamp01(x), y: clamp01(y) };
}

export interface MediaTileLike {
  querySelector: HTMLDivElement['querySelector'];
  getBoundingClientRect: HTMLDivElement['getBoundingClientRect'];
}

/**
 * #892: the ONE "which rect is the media content box" answer -- the tile's
 * `<video>` when present (its own bounding rect already excludes any docked
 * header/border chrome, e.g. `.has-remote-window-header video { top: 44px }`),
 * else the tile itself. Every draw/telepointer coordinate consumer (capture,
 * local echo, and receive-side render) must go through this, not
 * `tile.getBoundingClientRect()` directly -- that was the #892 bug.
 * Viewport-absolute, matching `event.clientX/clientY`.
 */
export function mediaContentRect(tile: MediaTileLike): { bounds: RectLike; media: SizeLike } {
  const video = tile.querySelector<HTMLVideoElement>('video');
  const rect = (video ?? tile).getBoundingClientRect();
  return {
    bounds: { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
    media: { width: video?.videoWidth ?? 0, height: video?.videoHeight ?? 0 },
  };
}

/**
 * Same rect choice as `mediaContentRect`, but expressed relative to the
 * tile's own top-left -- what a tile-anchored overlay layer (the draw SVG,
 * the telepointer layer) needs, since those layers are positioned/viewboxed
 * in the tile's own coordinate space, not the viewport's.
 */
export function mediaContentRectRelativeToTile(tile: MediaTileLike): { bounds: RectLike; media: SizeLike } {
  const tileRect = tile.getBoundingClientRect();
  const { bounds, media } = mediaContentRect(tile);
  return {
    bounds: { left: bounds.left - tileRect.left, top: bounds.top - tileRect.top, width: bounds.width, height: bounds.height },
    media,
  };
}

export function colorForIdentity(identity: string, paletteIndex?: number | null): string {
  return IDENTITY_COLOR_PALETTE[selectedPaletteIndex(identity, paletteIndex)];
}

export function inkForIdentity(identity: string, paletteIndex?: number | null): string {
  return IDENTITY_INK_PALETTE[selectedPaletteIndex(identity, paletteIndex)] ?? LIGHT_IDENTITY_INK;
}

export function identityHeaderCss(identity: string, paletteIndex?: number | null): { background: string; ink: string } {
  return {
    background: colorForIdentity(identity, paletteIndex),
    ink: inkForIdentity(identity, paletteIndex),
  };
}
