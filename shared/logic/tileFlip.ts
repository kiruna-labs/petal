// SINGLE SOURCE OF TRUTH for the tile FLIP frame (#248). Shared by the web
// grid's reflow controller (web-harness/src/tileReflow.ts) and the desktop
// gallery's layout pass + keyed-list animation (apps/desktop/src/lib/
// components/Gallery.svelte via $lib/motion.ts). Pure: callers measure the
// rects and hand the strings to WAAPI or a Svelte `animate:` function.
//
// A classic FLIP inverts a move with `scale(previous.width / next.width,
// previous.height / next.height)`. While every tile was 16:9 those two
// factors were equal; #204 kept tiles fixed at 16:9 partly because cropping
// makes them differ, and a non-uniform scale visibly squashes a live face for
// the length of the transition. With camera tiles now free to change shape
// (#248), the FLIP instead scales UNIFORMLY -- by the larger factor, so the
// scaled tile covers the old box on both axes -- and animates the tile's box
// with a centred `clip-path: inset()` that starts at exactly the old box and
// opens to the new one. Content is never distorted; the box shape is
// continuous from the first frame.
//
// Known cost, accepted: while a shape-changing FLIP runs (220 ms) the clip
// also trims whatever the tile paints outside its box or near the clipped
// edges -- the speaking ring and sharing ring (box-shadow), the desktop
// tile's hairline outline, and a name chip sitting in a clipped band. A
// negative inset on the unclipped axis would keep the ring on two sides only
// (lopsided), and on the clipped axis the old box is by definition smaller
// than the scaled tile, so anything painted past it has to go for the first
// frame to match the old rect. A plain move (same shape) gets no clip at all,
// so rings and chips are only ever affected while a tile changes shape.

export interface FlipRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** One inverted frame: `translate(dx, dy) scale(scale)` from the tile's top
 * left, clipped by `insetX`/`insetY` on each side (in the tile's own unscaled
 * px). The final frame is the identity. */
export interface UniformFlip {
  dx: number;
  dy: number;
  scale: number;
  insetX: number;
  insetY: number;
}

/** Below this the move is invisible and not worth an animation. */
const FLIP_EPSILON_PX = 0.5;
const FLIP_EPSILON_SCALE = 0.01;
/** A clip thinner than this is rounding noise, not a shape change -- leaving
 * it off keeps the speaking ring (a box-shadow) unclipped on a plain move. */
const CLIP_EPSILON_PX = 0.5;

function validRect(rect: FlipRect): boolean {
  return Number.isFinite(rect.width) && Number.isFinite(rect.height) && rect.width > 0 && rect.height > 0;
}

/**
 * The inverted frame that makes a tile laid out at `next` paint exactly over
 * `previous`, or null when there is nothing to animate.
 */
export function uniformFlip(previous: FlipRect, next: FlipRect): UniformFlip | null {
  if (!validRect(previous) || !validRect(next)) return null;
  const scaleX = previous.width / next.width;
  const scaleY = previous.height / next.height;
  if (
    Math.abs(previous.left - next.left) < FLIP_EPSILON_PX &&
    Math.abs(previous.top - next.top) < FLIP_EPSILON_PX &&
    Math.abs(scaleX - 1) < FLIP_EPSILON_SCALE &&
    Math.abs(scaleY - 1) < FLIP_EPSILON_SCALE
  ) {
    return null;
  }
  const scale = Math.max(scaleX, scaleY);
  // Local px trimmed off each side so the scaled box shows exactly
  // previous.width x previous.height, centred like object-fit's crop.
  const rawInsetX = (next.width - previous.width / scale) / 2;
  const rawInsetY = (next.height - previous.height / scale) / 2;
  const insetX = rawInsetX >= CLIP_EPSILON_PX ? rawInsetX : 0;
  const insetY = rawInsetY >= CLIP_EPSILON_PX ? rawInsetY : 0;
  return {
    dx: previous.left - next.left - scale * insetX,
    dy: previous.top - next.top - scale * insetY,
    scale,
    insetX,
    insetY
  };
}

function px(value: number): string {
  return `${Number(value.toFixed(3))}px`;
}

/**
 * The frame `remaining` of the way back from the end (1 = the inverted first
 * frame, 0 = at rest). WAAPI interpolates the two endpoint keyframes itself;
 * Svelte's `animate:` css(t, u) passes `u` here every frame. Both land on the
 * same linear blend of translate, scale and inset.
 */
export function uniformFlipTransform(flip: UniformFlip, remaining = 1): string {
  const scale = 1 + (flip.scale - 1) * remaining;
  return `translate(${px(flip.dx * remaining)}, ${px(flip.dy * remaining)}) scale(${Number(scale.toFixed(5))})`;
}

/** The matching clip, or null when the move keeps the tile's shape. `radius`
 * is the tile's own corner radius, so the clipped box keeps its rounding. */
export function uniformFlipClipPath(flip: UniformFlip, remaining = 1, radius = 0): string | null {
  if (flip.insetX === 0 && flip.insetY === 0) return null;
  const x = px(flip.insetX * remaining);
  const y = px(flip.insetY * remaining);
  const round = radius > 0 ? ` round ${px(radius)}` : '';
  return `inset(${y} ${x} ${y} ${x}${round})`;
}

export interface UniformFlipKeyframe {
  /** Structurally a WAAPI `Keyframe`, without this module needing the DOM lib. */
  [property: string]: string | undefined;
  transform: string;
  transformOrigin: string;
  clipPath?: string;
}

/**
 * WAAPI keyframes for one tile: from the inverted frame to rest, always from
 * the top-left origin the maths above assumes. Clip keyframes are only
 * present when the tile changes shape.
 */
export function uniformFlipKeyframes(flip: UniformFlip, radius = 0): UniformFlipKeyframe[] {
  const clipPath = uniformFlipClipPath(flip, 1, radius);
  const restClip = uniformFlipClipPath(flip, 0, radius);
  return [
    {
      transform: uniformFlipTransform(flip, 1),
      transformOrigin: 'top left',
      ...(clipPath ? { clipPath } : {})
    },
    {
      transform: uniformFlipTransform(flip, 0),
      transformOrigin: 'top left',
      ...(restClip ? { clipPath: restClip } : {})
    }
  ];
}

/**
 * What an interrupted FLIP actually shows: the painted (transformed) box
 * minus its in-flight clip. `getBoundingClientRect()` includes transforms but
 * ignores clip-path, so retargeting from it alone would start the next move
 * from the larger, unclipped box. `layoutWidth` is the tile's untransformed
 * width (`offsetWidth`); `clipPath` its computed `clip-path`.
 */
export function visibleFlipRect(painted: FlipRect, layoutWidth: number, clipPath: string | null | undefined): FlipRect {
  const match = /^inset\(([^)]*)\)$/.exec((clipPath ?? '').trim());
  if (!match || !(layoutWidth > 0)) return painted;
  const lengths = match[1].split(/\s+round\s+/)[0].trim().split(/\s+/).map((value) => parseFloat(value));
  if (lengths.length === 0 || lengths.length > 4 || lengths.some((value) => !Number.isFinite(value))) return painted;
  // CSS box shorthand: 1-4 values expand to top/right/bottom/left.
  const [top, right = top, bottom = top, left = right] = lengths;
  const scale = painted.width / layoutWidth;
  return {
    left: painted.left + left * scale,
    top: painted.top + top * scale,
    width: Math.max(0, painted.width - (left + right) * scale),
    height: Math.max(0, painted.height - (top + bottom) * scale)
  };
}

/**
 * `visibleFlipRect` for a FLIP whose frame is known rather than read back
 * from computed style: `painted` is the tile's transformed box `remaining` of
 * the way back through `flip` (what getBoundingClientRect() reported), and
 * the clip that frame drew is taken off it. Svelte's keyed-list `animate:`
 * cancels the running animation before it hands over the rect it measured,
 * so by then there is no clip-path left to read.
 */
export function uniformFlipVisibleRect(painted: FlipRect, flip: UniformFlip, remaining: number): FlipRect {
  const u = Math.min(1, Math.max(0, remaining));
  // The inset is in the tile's own px; on screen it is scaled with the tile.
  const scale = 1 + (flip.scale - 1) * u;
  const insetX = flip.insetX * u * scale;
  const insetY = flip.insetY * u * scale;
  return {
    left: painted.left + insetX,
    top: painted.top + insetY,
    width: Math.max(0, painted.width - 2 * insetX),
    height: Math.max(0, painted.height - 2 * insetY)
  };
}
