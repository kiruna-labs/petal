// View models for the declarative surfaces the HOST draws on a plugin's
// behalf (toolbar buttons today; header buttons in I-9). Both clients render
// these models. Labels are validated to MANIFEST_LIMITS.buttonLabelMaxLength
// where they enter (manifest validator, broker `ui.setButton`) and are NEVER
// clipped here -- "UI text must never truncate". Design: plugins/README.md §2.7.

import type { ButtonPatch } from './api.ts';
import type { LoadedPlugin, PluginSource } from './broker.ts';
import type { SurfaceContribution } from './manifest.ts';
import { pluginCaption } from './provenance.ts';

export interface ToolbarButtonModel {
  pluginId: string;
  /** Manifest name, for the provenance badge tooltip and the right-click menu heading. */
  pluginName: string;
  /** Where the HOST loaded this plugin from. Plugin-authored text cannot claim it. */
  pluginSource: PluginSource;
  buttonId: string;
  /** Visible label, exactly as declared or patched (validated at the boundary, never clipped). */
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
      const label = patch.label ?? button.label;
      out.push({
        pluginId: plugin.manifest.id,
        pluginName: plugin.manifest.name,
        pluginSource: plugin.source,
        buttonId: button.id,
        label,
        icon: patch.icon ?? button.icon,
        badge: patch.badge ?? null,
        disabled: patch.disabled ?? false,
        opens: button.opens ?? null,
        ariaLabel: `${label} (${pluginCaption(plugin.manifest.name, plugin.source)})`,
      });
    }
  }
  return out;
}

/**
 * Popover geometry. `width`/`height` in a manifest are plugin-authored and
 * `validateManifest` accepts ANY positive number, so they are CLAMPED here:
 * the host draws its own provenance caption on this box, and a plugin that
 * declares `width: 100` must not be able to push the host's "· plugin" out
 * of view (UI text must never truncate -- kiruna-labs/petal#71, finding 1).
 * The floor is deliberately generous: the caption also wraps, so the two
 * together survive the longest name a manifest may declare.
 */
export const POPOVER_SIZE = {
  defaultWidth: 280,
  defaultHeight: 200,
  minWidth: 200,
  minHeight: 64,
} as const;

/** Height of one caption line (plugin-provenance.css `.petal-plugin-caption`). */
export const POPOVER_CAPTION_HEIGHT = 22;

/** The content box the plugin's frame gets, after clamping what it declared. */
export function popoverContentSize(spec: Pick<SurfaceContribution, 'width' | 'height'>): { width: number; height: number } {
  return {
    width: Math.max(spec.width ?? POPOVER_SIZE.defaultWidth, POPOVER_SIZE.minWidth),
    height: Math.max(spec.height ?? POPOVER_SIZE.defaultHeight, POPOVER_SIZE.minHeight),
  };
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
