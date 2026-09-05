import type { IdentityColor } from '$lib/components/Avatar.svelte';

// Keep these hex values in lockstep with shared/ui/tokens.css's
// --id-* custom properties. Identity color is intentionally this fixed set.
export const PALETTE: IdentityColor[] = ['plum', 'blue', 'green', 'amber', 'lilac', 'slate'];

export const IDENTITY_COLOR_HEX: Record<IdentityColor, string> = {
  plum: '#f06cc9',
  blue: '#6e8bff',
  green: '#7ff0a3',
  amber: '#e8b84b',
  lilac: '#d6b8f0',
  slate: '#8fa6b8'
};

const IDENTITY_INK_HEX: Record<IdentityColor, string> = {
  plum: '#2b071b',
  blue: '#081129',
  green: '#062013',
  amber: '#271b04',
  lilac: '#1f102b',
  slate: '#071018'
};

export function colorForIdentity(identity: string): IdentityColor {
  let hash = 0;
  for (let i = 0; i < identity.length; i++) {
    hash = (hash * 31 + identity.charCodeAt(i)) >>> 0;
  }
  return PALETTE[hash % PALETTE.length];
}

export function paletteIndexForIdentityColor(color: IdentityColor): number {
  return PALETTE.indexOf(color);
}

export function identityColorFromPaletteIndex(index: number | null | undefined): IdentityColor | null {
  return Number.isInteger(index) && index !== null && index !== undefined && index >= 0 && index < PALETTE.length
    ? PALETTE[index]
    : null;
}

export function identityColorCss(color: IdentityColor): string {
  return IDENTITY_COLOR_HEX[color];
}

export function identityInkCss(color: IdentityColor): string {
  return IDENTITY_INK_HEX[color];
}

function isIdentityColor(value: string): value is IdentityColor {
  return PALETTE.includes(value as IdentityColor);
}

function paletteColorForCss(value: string): IdentityColor | null {
  const normalized = value.trim().toLowerCase();
  for (const color of PALETTE) {
    if (IDENTITY_COLOR_HEX[color].toLowerCase() === normalized) return color;
  }
  return null;
}

export function identityHeaderCss(color: IdentityColor | string): string {
  const trimmed = color.trim();
  const paletteColor = isIdentityColor(trimmed) ? trimmed : paletteColorForCss(trimmed);
  const background = paletteColor ? identityColorCss(paletteColor) : trimmed || identityColorCss('plum');
  const ink = identityInkCss(paletteColor ?? 'plum');
  return `--identity-header-bg: ${background}; --identity-header-ink: ${ink};`;
}
