// A sharer overlay can be inset from its shared source: on macOS a Petal View
// overlay keeps clear of the selector's edges so it never sits over the
// selector's resize band. The page then draws on a "stage" the size of the
// whole shared region, shifted by minus the inset, and the window clips it.
// Normalized 0..1 points map over the stage, so a pointer keeps its spot on
// the shared region and one in the inset band is clipped.

export interface OverlayStage {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The inset from the overlay URL: whole points, never negative. */
export function parseOverlayInset(raw: string | null | undefined): number {
  const value = Number(raw ?? '');
  if (!raw || !Number.isFinite(value) || value <= 0) return 0;
  return Math.round(value);
}

/** The shared region in the overlay window's CSS pixels. */
export function overlayStage(inset: number, viewport: { width: number; height: number }): OverlayStage {
  const pad = Math.max(0, inset);
  return {
    left: 0 - pad,
    top: 0 - pad,
    width: Math.max(1, viewport.width) + 2 * pad,
    height: Math.max(1, viewport.height) + 2 * pad
  };
}

/** Where a normalized point of the shared region falls in the overlay window. */
export function stagePoint(stage: OverlayStage, x: number, y: number): { x: number; y: number } {
  return { x: stage.left + x * stage.width, y: stage.top + y * stage.height };
}
