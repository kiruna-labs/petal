/**
 * Interruptible FLIP motion for the persistent browser tile DOM.
 *
 * Layout code moves tiles between grid, hero, and spotlight-rail parents. This
 * controller captures the currently painted bounds, cancels any superseded
 * animation, lets layout settle, then animates only transform back to the new
 * bounds. A WeakMap shares one controller per tile surface so participant
 * insertion/removal and layout-mode changes cannot fight with separate WAAPI
 * handles.
 */

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

function captureRects(surface: TileSurface): Map<HTMLElement, DOMRect> {
  const rects = new Map<HTMLElement, DOMRect>();
  if (!animationsAllowed()) return rects;
  surface.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
    const rect = tile.getBoundingClientRect();
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

  function animateFrom(previousRects: Map<HTMLElement, DOMRect>, expectedGeneration: number) {
    if (previousRects.size === 0 || !animationsAllowed()) return;

    requestAnimationFrame(() => {
      if (expectedGeneration !== generation || !animationsAllowed()) return;

      surface.querySelectorAll<HTMLElement>('.tile').forEach((tile) => {
        const previous = previousRects.get(tile);
        if (!previous || typeof tile.animate !== 'function') return;

        const next = tile.getBoundingClientRect();
        if (next.width <= 0 || next.height <= 0) return;

        const deltaX = previous.left - next.left;
        const deltaY = previous.top - next.top;
        const scaleX = previous.width / next.width;
        const scaleY = previous.height / next.height;
        if (
          Math.abs(deltaX) < 0.5 &&
          Math.abs(deltaY) < 0.5 &&
          Math.abs(scaleX - 1) < 0.01 &&
          Math.abs(scaleY - 1) < 0.01
        ) {
          return;
        }

        const animation = tile.animate(
          [
            {
              transform: `translate(${deltaX}px, ${deltaY}px) scale(${scaleX}, ${scaleY})`,
              transformOrigin: 'top left',
            },
            { transform: 'translate(0, 0) scale(1, 1)', transformOrigin: 'top left' },
          ],
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
      const previousRects = captureRects(surface);
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
