// "This came from a plugin" affordances shared by both clients: the badge on
// host-drawn plugin controls, the caption on plugin popovers, and the
// right-click menu that lets a user turn a plugin off on the spot. Copy lives
// here so it is measured once against the 400 px main window and never
// truncates.
//
// Every string here is built from the plugin's `name` (plugin-authored, so
// validated as printable text in manifest.ts) AND its `source` (the HOST's
// own record, LoadedPlugin.source -- a plugin cannot set it). Without the
// source, a sideloaded plugin calling itself "Reactions" is indistinguishable
// from the built-in one, which is exactly the question this chrome exists to
// answer (kiruna-labs/petal#71 review, finding 3).

import type { PluginSource } from './broker.ts';
import { PLUGIN_LIMITS } from './rateLimit.ts';

export const PLUGIN_MENU_DISABLE = 'disable';

/**
 * The source, as a word that reads inside a sentence. Settings shows the same
 * fact title-cased in its own column (`SOURCE_LABELS` in settingsModel.ts);
 * these are the in-sentence forms the provenance chrome needs.
 */
export const PLUGIN_SOURCE_WORD: Record<PluginSource, string> = {
  builtin: 'built-in',
  registry: 'installed',
  dev: 'dev',
};

export interface PluginMenuTarget {
  pluginId: string;
  name: string;
  source: PluginSource;
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

/**
 * Caption shown above a plugin popover, and the tooltip on the provenance
 * badge -- one string so both surfaces answer "whose is this?" identically.
 */
export function pluginCaption(name: string, source: PluginSource): string {
  return `${name} · ${PLUGIN_SOURCE_WORD[source]} plugin`;
}

/** Tooltip / accessible name for the provenance badge. */
export function pluginProvenanceTitle(name: string, source: PluginSource): string {
  return pluginCaption(name, source);
}

export function pluginMenuModel(name: string, source: PluginSource): PluginMenuModel {
  return {
    heading: pluginCaption(name, source),
    items: [{ id: PLUGIN_MENU_DISABLE, label: `Turn off ${name}` }],
  };
}

/**
 * Where THIS client's user can turn the plugin back on. Never guess: the
 * desktop has Settings → Plugins, the web client has no plugins sheet yet
 * (I-10), so on the web "turn off" lasts for this page and the copy says so.
 * Pointing a toast at UI a client does not have is a one-way door
 * (kiruna-labs/petal#71 review, finding 4).
 */
export type PluginReEnablePath = 'settings' | 'reload';

const RE_ENABLE_SENTENCE: Record<PluginReEnablePath, string> = {
  settings: 'Turn it back on in Settings → Plugins.',
  reload: 'Reload this page to bring it back.',
};

/** Toast after turning a plugin off. Stays under the toast cap for any valid manifest name. */
export function pluginDisabledToast(name: string, where: PluginReEnablePath): string {
  const tail = RE_ENABLE_SENTENCE[where];
  const text = `${name} is off. ${tail}`;
  return text.length <= PLUGIN_LIMITS.toastMaxChars ? text : `Plugin off. ${tail}`;
}
