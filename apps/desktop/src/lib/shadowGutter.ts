// Transparent shadow gutter for macOS card panels (#406).
//
// The native window shadow of these transparent panels is off (it outlined
// the CSS shadow's alpha as a black hairline on macOS 26), so the card's CSS
// shadow is the only elevation. The native side makes the window larger than
// the card by the shadow's extent and passes `?shadowGutter=<kind>` in the
// route URL; the page pads its card by the same amount so the shadow is not
// clipped. The gutter is click-through (src-tauri/src/shadow_gutter.rs).
//
// Mirrors `shadow_gutter.rs` (FLOAT / PANEL) and the extents of
// `--shadow-float` / `--shadow-panel` in shared/ui/tokens.css;
// tests/shadowGutter.test.ts keeps all three in lockstep.

export type ShadowGutterKind = 'float' | 'panel';

export interface ShadowGutter {
  x: number;
  top: number;
  bottom: number;
}

export const SHADOW_GUTTERS: Record<ShadowGutterKind, ShadowGutter> = {
  float: { x: 26, top: 8, bottom: 44 },
  panel: { x: 52, top: 22, bottom: 82 },
};

/** The gutter the native side sized this window for, or null (no gutter). */
export function shadowGutterFromSearch(
  search: string = typeof location !== 'undefined' ? location.search : '',
): ShadowGutter | null {
  const kind = new URLSearchParams(search).get('shadowGutter');
  return kind === 'float' || kind === 'panel' ? SHADOW_GUTTERS[kind] : null;
}

/** Inline `padding` for the element that wraps the card, or '' without a gutter. */
export function shadowGutterPadding(gutter: ShadowGutter | null): string {
  if (!gutter) return '';
  return `padding: ${gutter.top}px ${gutter.x}px ${gutter.bottom}px;`;
}
