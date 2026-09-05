// Icon set plugins may reference by name in their manifest (`icon: "smile"`).
// Plain SVG strings, same conventions as shared/ui/icons.ts: 24-unit viewBox,
// currentColor stroke, round caps. Keeping the set closed means a plugin can
// never inject markup through an icon field.

const ICONS: Record<string, string> = {
  smile:
    '<circle cx="12" cy="12" r="9"/><path d="M8.5 14.5a4.5 4.5 0 0 0 7 0"/><path d="M9 9.5h.01M15 9.5h.01"/>',
  chat: '<path d="M21 12a8 8 0 0 1-8 8H8l-4 3v-6.5A8 8 0 1 1 21 12Z"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>',
  bell: '<path d="M6 16V11a6 6 0 0 1 12 0v5l1.5 2h-15Z"/><path d="M10 21a2 2 0 0 0 4 0"/>',
  plug: '<path d="M9 2v5M15 2v5M6 7h12v3a6 6 0 0 1-12 0V7Z"/><path d="M12 16v6"/>',
  sparkle: '<path d="M12 3l1.8 5.7L19.5 10.5l-5.7 1.8L12 18l-1.8-5.7L4.5 10.5l5.7-1.8L12 3Z"/>',
  timer: '<circle cx="12" cy="13" r="8"/><path d="M12 9v4l2.5 2.5M9 2h6"/>',
  puzzle:
    '<path d="M10 3a2 2 0 0 1 4 0v1h3a1 1 0 0 1 1 1v3h1a2 2 0 0 1 0 4h-1v3a1 1 0 0 1-1 1h-3v1a2 2 0 0 1-4 0v-1H7a1 1 0 0 1-1-1v-3H5a2 2 0 0 1 0-4h1V5a1 1 0 0 1 1-1h3V3Z"/>',
};

export const PLUGIN_ICON_NAMES: readonly string[] = Object.keys(ICONS);

/** Inline SVG for a known icon name; unknown names fall back to the puzzle piece. */
export function pluginIconSvg(name: string, size = 20): string {
  const body = ICONS[name] ?? ICONS.puzzle!;
  return (
    `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" ` +
    `stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`
  );
}
