// Shared inline-SVG icon markup (#847). Plain TS string constants, not a
// Svelte component: the desktop app renders this via `{@html}` inside
// Svelte components, and web-harness's plain-DOM (`aiChatPanel.ts`,
// `remoteWindowHeader.ts`) interpolates the same string into template
// literals -- neither client mounts Svelte components from `shared/ui/`
// (only `shared/logic/*` is consumed on the web-harness side today), so a
// `.svelte` wrapper here would not actually be reachable from both.
//
// Stroke style/size matches the existing icon set used across
// RemoteWindowHeader.svelte and friends: `viewBox="0 0 24 24"`, `fill="none"`,
// `stroke="currentColor"`, `stroke-width="2"`, round caps/joins. `{{SIZE}}`
// is substituted by `sparkleIconSvg(size)` so each call site can pick its own
// pixel size without a second copy of the path data drifting out of sync.

const SPARKLE_ICON_TEMPLATE =
  '<svg width="{{SIZE}}" height="{{SIZE}}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3l1.8 5.7L19.5 10.5l-5.7 1.8L12 18l-1.8-5.7L4.5 10.5l5.7-1.8L12 3Z"/></svg>';

/** A four-point sparkle icon, sized in pixels (default 14). */
export function sparkleIconSvg(size = 14): string {
  return SPARKLE_ICON_TEMPLATE.replaceAll('{{SIZE}}', String(size));
}

/**
 * Glyph fallback for surfaces that cannot render inline SVG at all -- native
 * Tauri `CheckMenuItem`s (no icon option; see `CheckMenuItemOptions`) chief
 * among them. Prepend to a label's text, e.g. `${SPARKLE_GLYPH} Start AI
 * chat on this window`.
 */
export const SPARKLE_GLYPH = '✦';
