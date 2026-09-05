// View models for the declarative surfaces the HOST draws on a plugin's
// behalf (toolbar buttons today; header buttons in I-9). Both clients render
// these models; the fit rules here are what keeps "UI text must never
// truncate" true for text a third party wrote. Design: plugins/README.md §2.7.

import type { ButtonPatch } from './api.ts';
import type { LoadedPlugin } from './broker.ts';
import { MANIFEST_LIMITS } from './manifest.ts';

export interface ToolbarButtonModel {
  pluginId: string;
  buttonId: string;
  /** Visible label, already clamped to the manifest limit. */
  label: string;
  icon: string;
  badge: number | null;
  disabled: boolean;
  /** `"<kind>:<id>"` of the surface the host toggles on click, or null for a plain action. */
  opens: string | null;
  /** Accessible name: "<label> (<plugin name>)" so screen readers attribute it. */
  ariaLabel: string;
}

export function buttonKey(pluginId: string, buttonId: string): string {
  return `${pluginId}/${buttonId}`;
}

/** Clamp a label to the manifest limit; the validator already enforces it for manifests. */
export function fitButtonLabel(label: string): string {
  const trimmed = label.trim();
  return trimmed.length <= MANIFEST_LIMITS.buttonLabelMaxLength ? trimmed : trimmed.slice(0, MANIFEST_LIMITS.buttonLabelMaxLength);
}

/** Badge text: 1..99 shown as-is, more as "99+", 0/null hidden. */
export function badgeText(badge: number | null | undefined): string | null {
  if (badge === null || badge === undefined || !(badge > 0)) return null;
  return badge > 99 ? '99+' : String(Math.floor(badge));
}

export function toolbarButtonModels(plugins: readonly LoadedPlugin[], patches: ReadonlyMap<string, ButtonPatch>): ToolbarButtonModel[] {
  const out: ToolbarButtonModel[] = [];
  for (const plugin of plugins) {
    if (!plugin.granted.includes('ui:toolbar-button')) continue;
    for (const button of plugin.manifest.contributes?.toolbarButtons ?? []) {
      const patch = patches.get(buttonKey(plugin.manifest.id, button.id)) ?? {};
      const label = fitButtonLabel(patch.label ?? button.label);
      out.push({
        pluginId: plugin.manifest.id,
        buttonId: button.id,
        label,
        icon: patch.icon ?? button.icon,
        badge: patch.badge ?? null,
        disabled: patch.disabled ?? false,
        opens: button.opens ?? null,
        ariaLabel: `${label} (${plugin.manifest.name})`,
      });
    }
  }
  return out;
}

export interface PopoverPlacement {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * Place a popover of `width`x`height` against an anchor rect inside a
 * viewport, preferring above the anchor (the toolbar sits at the bottom),
 * falling back below, always clamped 8 px inside the viewport.
 */
export function placePopover(
  anchor: { left: number; top: number; width: number; height: number },
  size: { width: number; height: number },
  viewport: { width: number; height: number },
  margin = 8,
): PopoverPlacement {
  const width = Math.min(size.width, Math.max(viewport.width - margin * 2, 0));
  const height = Math.min(size.height, Math.max(viewport.height - margin * 2, 0));
  const centered = anchor.left + anchor.width / 2 - width / 2;
  const left = Math.min(Math.max(centered, margin), Math.max(viewport.width - width - margin, margin));
  const above = anchor.top - height - margin;
  const below = anchor.top + anchor.height + margin;
  const top = above >= margin ? above : Math.min(below, Math.max(viewport.height - height - margin, margin));
  return { left, top, width, height };
}
