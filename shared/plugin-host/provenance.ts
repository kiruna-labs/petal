// "This came from a plugin" affordances shared by both clients: the badge on
// host-drawn plugin controls, the caption on plugin popovers, and the
// right-click menu that lets a user turn a plugin off on the spot (it can be
// turned back on in Settings -> Plugins). Copy lives here so it is measured
// once against the 400 px main window and never truncates.

import { MANIFEST_LIMITS } from './manifest.ts';

export const PLUGIN_MENU_DISABLE = 'disable';

export interface PluginMenuTarget {
  pluginId: string;
  name: string;
  x: number;
  y: number;
}

export interface PluginMenuItem {
  id: typeof PLUGIN_MENU_DISABLE;
  label: string;
}

export interface PluginMenuModel {
  heading: string;
  items: PluginMenuItem[];
}

/** Tooltip / accessible name for the provenance badge. */
export function pluginProvenanceTitle(name: string): string {
  return `${name} plugin`;
}

/** Caption shown above a plugin popover. */
export function pluginCaption(name: string): string {
  return `${name} · plugin`;
}

export function pluginMenuModel(name: string): PluginMenuModel {
  return {
    heading: pluginCaption(name),
    items: [{ id: PLUGIN_MENU_DISABLE, label: `Turn off ${name}` }],
  };
}

/** Toast after turning a plugin off. Stays under the 80-char toast cap for any valid manifest name. */
export function pluginDisabledToast(name: string): string {
  const text = `${name} is off. Turn it back on in Settings → Plugins.`;
  return text.length <= 80 ? text : `Plugin off. Turn it back on in Settings → Plugins.`;
}

/** Guard used by tests: the longest allowed name still yields an in-budget toast. */
export const LONGEST_NAME_TOAST_LENGTH = pluginDisabledToast('x'.repeat(MANIFEST_LIMITS.nameMaxLength)).length;
