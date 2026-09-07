// Which plugins this desktop install has, and which are enabled. M1: the
// built-ins plus a localStorage enabled map (shared/plugin-host/
// settingsModel.ts). The registry client (I-5a) extends this with installed
// bundles from the Rust-owned store; the shape stays the same.

import { builtinPlugins } from '@petal/shared/plugin-host/builtins';
import type { LoadedPlugin } from '@petal/shared/plugin-host/broker';
import { isPluginEnabled, readEnabledOverrides, type InstalledPlugin } from '@petal/shared/plugin-host/settingsModel';
import { browserStorage } from '$lib/data/storageKeys';

export function installedPlugins(warn: (message: string) => void = (m) => console.warn(m)): InstalledPlugin[] {
  return builtinPlugins(warn);
}

export function enabledPlugins(): { plugin: LoadedPlugin; source: string }[] {
  const overrides = readEnabledOverrides(browserStorage());
  return installedPlugins()
    .filter((p) => isPluginEnabled(p, overrides))
    .map((p) => ({ plugin: { manifest: p.manifest, granted: p.manifest.permissions, source: p.source }, source: p.source_js }));
}
