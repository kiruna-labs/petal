// #122: where the room-row roster tooltip goes, as a pure function so the
// placement rule is asserted rather than eyeballed.
//
// The tooltip is `position: fixed`. That is not a style preference: rows
// scroll inside `.room-list-scroll` (overflow-y: auto) under
// `.main-menu { overflow: hidden }`, so an absolutely-positioned tooltip
// inside a row is CLIPPED at the list edge. Fixed escapes both, and
// `.app-shell`'s deliberate `transform: translateZ(0)` makes it the
// containing block for fixed descendants -- it sits at the window origin, so
// viewport coordinates map 1:1.
//
// Trap (do not "simplify" this away): `.room-row-shell.clickable:active`
// applies a `transform: scale(...)`, which makes the PRESSED row the
// containing block instead, snapping the tooltip into row coordinates
// mid-press. RoomRow hides the tooltip while the row is `:active` for exactly
// that reason.

export interface TooltipAnchorRect {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

export interface TooltipSize {
  width: number;
  height: number;
}

export interface TooltipViewport {
  width: number;
  height: number;
}

export interface TooltipPlacement {
  left: number;
  top: number;
  /** Below the anchor when there is room, otherwise flipped above it. */
  placement: 'below' | 'above';
}

/** Gap between the row and the tooltip, and the minimum window margin. */
export const TOOLTIP_GAP = 6;
export const TOOLTIP_MARGIN = 8;

/**
 * Place the tooltip below the row, flipping above when the space below is too
 * small, and clamp it inside the window horizontally AND vertically. The
 * clamp is what keeps a bottom-of-a-scrolled-list row's tooltip fully on
 * screen -- the case a `getBoundingClientRect()`-only check cannot see.
 */
export function placeRowTooltip(
  anchor: TooltipAnchorRect,
  tooltip: TooltipSize,
  viewport: TooltipViewport,
  gap: number = TOOLTIP_GAP,
  margin: number = TOOLTIP_MARGIN
): TooltipPlacement {
  const spaceBelow = viewport.height - anchor.bottom - gap - margin;
  const spaceAbove = anchor.top - gap - margin;
  const placement: 'below' | 'above' =
    spaceBelow >= tooltip.height || spaceBelow >= spaceAbove ? 'below' : 'above';
  const rawTop =
    placement === 'below' ? anchor.bottom + gap : anchor.top - gap - tooltip.height;
  const maxTop = Math.max(margin, viewport.height - margin - tooltip.height);
  const top = Math.min(Math.max(rawTop, margin), maxTop);

  const maxLeft = Math.max(margin, viewport.width - margin - tooltip.width);
  const left = Math.min(Math.max(anchor.left, margin), maxLeft);

  return { left, top, placement };
}
