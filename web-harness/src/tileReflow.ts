/**
 * Interruptible FLIP motion for the persistent browser tile DOM.
 *
 * Layout code moves tiles between grid, hero, and spotlight-rail parents. This
 * controller captures the currently painted bounds, cancels any superseded
 * animation, lets layout settle, then animates only transform (and, when a
 * tile changes shape, its clip) back to the new bounds. A WeakMap shares one
 * controller per tile surface so participant insertion/removal and
 * layout-mode changes cannot fight with separate WAAPI handles.
 *
 * #248: camera tiles change shape as they crop, so the inverted frame comes
 * from the shared `uniformFlip` -- one uniform scale plus a clip of the box --
 * never a `scale(x, y)` that would squash the live video mid-move.
 */
import {
  uniformFlip,
  uniformFlipKeyframes,
  visibleFlipRect,
  type FlipRect,
} from '@petal/shared/logic/tileFlip';

export const TILE_REFLOW_ANIMATION_MS = 220;
const TILE_REFLOW_EASING = 'cubic-bezier(0.2, 0, 0, 1)';

type TileSurface = HTMLElement;

interface TileReflowController {
  withAnimation<T>(mutate: () => T): T;
  withoutAnimation<T>(mutate: () => T): T;
}

const controllers = new WeakMap<TileSurface, TileReflowController>();

function animationsAllowed(): boolean {
  return (
    typeof requestAnimationFrame === 'function' &&
    typeof window !== 'undefined' &&
    !window.matchMedia?.('(prefers-reduced-motion: reduce)').matches
  );
}

/** The tile's own corner radius, so a shape-changing clip keeps it rounded. */
function tileCornerRadius(tile: HTMLElement): number {
  if (typeof getComputedStyle !== 'function') return 0;
  const radius = parseFloat(getComputedStyle(tile).borderTopLeftRadius);
  return Number.isFinite(radius) ? radius : 0;
}

function captureRects(
  surface: TileSurface,
  inFlight: ReadonlyMap<HTMLElement, Animation>
): Map<HTMLElement, FlipRect> {
  const rects = new Map<HTMLElement, FlipRect>();
  if (!animationsAllowed()) return rects;
  surface.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
    const painted = tile.getBoundingClientRect();
    // A tile mid-FLIP may be clipped to a smaller box than it paints; retarget
    // from what is visible, or the next move starts with a jump.
    const rect = inFlight.has(tile) && typeof getComputedStyle === 'function'
      ? visibleFlipRect(painted, tile.offsetWidth, getComputedStyle(tile).clipPath)
      : painted;
    if (rect.width > 0 && rect.height > 0) rects.set(tile, rect);
  });
  return rects;
}

export function getTileReflowController(surface: TileSurface): TileReflowController {
  const existing = controllers.get(surface);
  if (existing) return existing;

  let depth = 0;
  let generation = 0;
  const activeAnimations = new Map<HTMLElement, Animation>();

  function cancelActiveAnimations() {
    for (const [tile, animation] of activeAnimations) {
      animation.cancel();
      activeAnimations.delete(tile);
    }
  }

  function animateFrom(previousRects: Map<HTMLElement, FlipRect>, expectedGeneration: number) {
    if (previousRects.size === 0 || !animationsAllowed()) return;

    requestAnimationFrame(() => {
      if (expectedGeneration !== generation || !animationsAllowed()) return;

      surface.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
        const previous = previousRects.get(tile);
        if (!previous || typeof tile.animate !== 'function') return;

        const flip = uniformFlip(previous, tile.getBoundingClientRect());
        if (!flip) return;

        const animation = tile.animate(
          uniformFlipKeyframes(flip, tileCornerRadius(tile)),
          {
            duration: TILE_REFLOW_ANIMATION_MS,
            easing: TILE_REFLOW_EASING,
            fill: 'none',
          }
        );
        activeAnimations.set(tile, animation);
        void animation.finished
          .then(() => {
            if (activeAnimations.get(tile) === animation) activeAnimations.delete(tile);
          })
          .catch(() => {
            if (activeAnimations.get(tile) === animation) activeAnimations.delete(tile);
          });
      });
    });
  }

  const controller: TileReflowController = {
    withAnimation<T>(mutate: () => T): T {
      if (depth > 0) return mutate();

      depth += 1;
      const previousRects = captureRects(surface, activeAnimations);
      // Capture first, then cancel: getBoundingClientRect() includes the
      // currently painted WAAPI transform, which is the correct retargeting
      // origin for a rapid second layout request.
      generation += 1;
      cancelActiveAnimations();
      try {
        return mutate();
      } finally {
        depth -= 1;
        animateFrom(previousRects, generation);
      }
    },

    withoutAnimation<T>(mutate: () => T): T {
      depth += 1;
      try {
        return mutate();
      } finally {
        depth -= 1;
      }
    },
  };

  controllers.set(surface, controller);
  return controller;
}
