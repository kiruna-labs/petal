type SegmenterLike = {
  segment(input: string): Iterable<{ segment: string }>;
};

type SegmenterConstructor = new (
  locales?: string | string[],
  options?: { granularity?: 'grapheme' }
) => SegmenterLike;

export function firstGrapheme(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) return '';

  const Segmenter = (Intl as typeof Intl & { Segmenter?: SegmenterConstructor }).Segmenter;
  if (Segmenter) {
    const first = new Segmenter(undefined, { granularity: 'grapheme' })
      .segment(trimmed)
      [Symbol.iterator]()
      .next();
    if (!first.done) return first.value.segment;
  }

  return Array.from(trimmed)[0] ?? '';
}

/** Tolerance once the full name is already showing: only fall back to the
 * compact label once it measurably no longer fits (preserves the original
 * 0.5px sub-pixel-rounding tolerance). */
const SHRINK_TOLERANCE_PX = 0.5;

/** Extra headroom required before switching FROM the compact label back TO
 * the full name. Deliberately wider than the shrink tolerance -- see #676's
 * hysteresis note below. */
const GROW_HEADROOM_PX = 4;

/**
 * `nameChipLabelForFit`/`cameraOffNameLabelForFit` are called on every
 * ResizeObserver tick (ParticipantTile.svelte's `scheduleMeasuredLabels`),
 * so a `fullNameWidth`/`availableWidth` pair that lands within a fraction of
 * a pixel of the fit boundary -- sub-pixel layout rounding during a live
 * resize, a container query recalculating on each frame -- can flip the
 * label between the full name and the compact fallback on consecutive
 * measurements even though nothing meaningfully changed. #676 investigated
 * (and ruled out) two specific flicker mechanisms upstream of this function;
 * this hysteresis band is hardening against a THIRD, more generic one this
 * function is directly exposed to, not a confirmed fix for what was
 * reported -- see #676 for what was and wasn't verified.
 *
 * The fix is a standard hysteresis band: shrinking (full name -> compact)
 * uses a tight tolerance so a genuine overflow still collapses promptly, but
 * growing back (compact -> full name) requires `GROW_HEADROOM_PX` of real
 * extra room, not just technically fitting again. A value oscillating by a
 * sub-pixel amount right at the boundary now settles on one label instead of
 * flipping every measurement. `previousLabel` is the label most recently
 * rendered (pass the current $state value) -- omit it (or pass anything
 * other than `name` itself) to get the conservative "not currently showing
 * the full name" branch, e.g. on first measurement.
 */
export function nameChipLabelForFit(
  name: string,
  fullNameWidth: number,
  availableWidth: number,
  previousLabel?: string
): string {
  if (!name) return '';
  if (!Number.isFinite(fullNameWidth) || !Number.isFinite(availableWidth) || availableWidth <= 0) {
    return firstGrapheme(name);
  }

  const showingFullName = previousLabel === name;
  const threshold = showingFullName ? SHRINK_TOLERANCE_PX : -GROW_HEADROOM_PX;
  return fullNameWidth <= availableWidth + threshold ? name : firstGrapheme(name);
}

export function cameraOffNameLabelForFit(
  name: string,
  fullNameWidth: number,
  availableWidth: number,
  previousLabel?: string
): string {
  return nameChipLabelForFit(name, fullNameWidth, availableWidth, previousLabel);
}
