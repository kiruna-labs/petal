// SINGLE SOURCE OF TRUTH for which identity-palette slot a participant renders
// in (#317). A participant's CHOSEN slot travels as `petalIdentityPaletteIndex`
// in LiveKit participant metadata; the identity hash is only the fallback for
// clients that never published one. Desktop (lib/data/identityColor.ts) and web
// (web-harness/src/telepointer.ts) both resolve through here, and the native
// compositor's `owner_border_color_hex` mirrors the same rule, so the gallery,
// roster, pills, remote-window border/header and telepointer cannot disagree.

/** Number of slots in the identity palette (plum, blue, green, amber, lilac, slate). */
export const IDENTITY_PALETTE_SIZE = 6;

export function isValidIdentityPaletteIndex(index: unknown): index is number {
  return typeof index === 'number' && Number.isInteger(index) && index >= 0 && index < IDENTITY_PALETTE_SIZE;
}

/** Deterministic fallback slot: `h = h * 31 + utf16Unit`, mod the palette size. */
export function hashedIdentityPaletteIndex(identity: string): number {
  let hash = 0;
  for (let i = 0; i < identity.length; i++) {
    hash = (hash * 31 + identity.charCodeAt(i)) >>> 0;
  }
  return hash % IDENTITY_PALETTE_SIZE;
}

/** The chosen palette index when valid, otherwise the identity-hash fallback. */
export function identityPaletteIndex(identity: string, chosenIndex?: number | null): number {
  return isValidIdentityPaletteIndex(chosenIndex) ? chosenIndex : hashedIdentityPaletteIndex(identity);
}
