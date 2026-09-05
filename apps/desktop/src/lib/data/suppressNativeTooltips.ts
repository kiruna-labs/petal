/**
 * suppressNativeTooltips.ts
 *
 * Windows WebView2 is Chromium, and Chromium renders a native OS tooltip for
 * any element carrying a `title` attribute. macOS WKWebView has historically
 * ignored `title` tooltips, while Windows WebView2 renders them. Petal
 * normally owns tooltips itself (`aria-label` for the accessible name plus
 * styled tooltip spans), so only deliberately marked controls may retain a
 * native `title` tooltip.
 *
 * The fix is a capture-phase `pointerover` handler (wired in +layout.svelte,
 * which every window mounts) that strips `title` from the hovered element
 * before Chromium's tooltip delay elapses. The ancestor walk matters:
 * Chromium shows the nearest titled ancestor's tooltip when the hovered leaf
 * itself has no `title`, so the strip must cover the whole ancestry chain,
 * not just the event target.
 */

/** Minimal structural surface — the real caller passes a DOM `Element`; tests
 * pass a stub so this module stays importable under node:test without a DOM.
 * A real `Element` satisfies this interface structurally. */
export interface TitleAttrElement {
  hasAttribute(name: string): boolean;
  removeAttribute(name: string): void;
  parentElement: TitleAttrElement | null;
}

/** Remove `title` from `element` and every unmarked titled ancestor up to
 * the root. A deliberately marked element may retain its native tooltip.
 * Safe to call on every pointerover: removing an already-absent attribute is
 * a no-op, so repeated hovers over the same node cost one cheap walk. */
export function stripNativeTooltipTitles(element: TitleAttrElement | null): void {
  let current: TitleAttrElement | null = element;
  while (current) {
    const allowNativeTooltip = current.hasAttribute('data-allow-native-tooltip');
    if (!allowNativeTooltip && current.hasAttribute('title')) current.removeAttribute('title');
    current = current.parentElement;
  }
}
