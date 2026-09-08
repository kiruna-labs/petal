// Which plugins this desktop install has, and which are enabled: the
// built-ins compiled into the app plus registry installs recorded by the
// Rust store (plugins::store, `plugins.json`), whose verified bundles are
// re-validated here before they can boot. Enabled state: built-ins use the
// localStorage override map (shared/plugin-host/settingsModel.ts); registry
// installs carry `enabled` in their store record (mirrored into the same map
// so the shared rows model needs one source).

import { invoke } from '@tauri-apps/api/core';
import { builtinPlugins } from '@petal/shared/plugin-host/builtins';
import type { LoadedPlugin } from '@petal/shared/plugin-host/broker';
import { validateManifest } from '@petal/shared/plugin-host/manifest';
import { isPluginEnabled, readEnabledOverrides, type InstalledPlugin } from '@petal/shared/plugin-host/settingsModel';
import { browserStorage } from '$lib/data/storageKeys';
import { COMMANDS, hasTauriBridge, type CommandArgs, type CommandReturns } from '$lib/ipc';

export interface CatalogEntry extends InstalledPlugin {
  /** Store record fields for registry installs; undefined for built-ins. */
  installedVersion?: string;
  grantedPermissions?: string[];
}

/** Registry installs from the Rust store, each bundle re-validated. Empty outside Tauri. */
export async function registryInstalledPlugins(warn: (message: string) => void = (m) => console.warn(m)): Promise<CatalogEntry[]> {
  if (!hasTauriBridge()) return [];
  let state: CommandReturns[typeof COMMANDS.pluginListInstalled];
  try {
    state = await invoke<CommandReturns[typeof COMMANDS.pluginListInstalled]>(COMMANDS.pluginListInstalled);
  } catch (e) {
    warn(`plugins: could not list installed plugins: ${String(e)}`);
    return [];
  }
  const out: CatalogEntry[] = [];
  for (const [id, record] of Object.entries(state.plugins)) {
    try {
      const text = await invoke<string>(COMMANDS.pluginReadBundle, { pluginId: id } satisfies CommandArgs[typeof COMMANDS.pluginReadBundle]);
      const raw = JSON.parse(text) as { manifest?: unknown; files?: Record<string, unknown> };
      const validated = validateManifest(raw.manifest);
      if (!validated.ok) throw new Error(validated.errors[0]);
      const manifest = validated.manifest;
      if (manifest.id !== id || manifest.version !== record.version) throw new Error('stored bundle does not match its record');
      const source = raw.files?.[manifest.entry];
      if (typeof source !== 'string' || source.length === 0) throw new Error('stored bundle lacks its entry file');
      out.push({
        manifest,
        source: record.source === 'dev' ? 'dev' : 'registry',
        enabledByDefault: record.enabled,
        source_js: source,
        installedVersion: record.version,
        grantedPermissions: record.grantedPermissions,
      });
    } catch (e) {
      warn(`plugins: installed plugin ${id} is unusable and was skipped: ${String((e as Error).message ?? e)}`);
    }
  }
  return out;
}

export async function installedPlugins(warn: (message: string) => void = (m) => console.warn(m)): Promise<CatalogEntry[]> {
  const builtins: CatalogEntry[] = builtinPlugins(warn);
  const registry = await registryInstalledPlugins(warn);
  // A registry install of a built-in's id (a newer version) replaces the built-in.
  const registryIds = new Set(registry.map((p) => p.manifest.id));
  return [...builtins.filter((b) => !registryIds.has(b.manifest.id)), ...registry];
}

export async function enabledPlugins(): Promise<{ plugin: LoadedPlugin; source: string }[]> {
  const overrides = readEnabledOverrides(browserStorage());
  return (await installedPlugins())
    .filter((p) => isPluginEnabled(p, overrides))
    .map((p) => ({
      plugin: { manifest: p.manifest, granted: (p.grantedPermissions as LoadedPlugin['granted']) ?? p.manifest.permissions, source: p.source },
      source: p.source_js,
    }));
}

/** Persist an enable/disable for a registry install in the Rust store (built-ins live in the override map only). */
export async function setInstalledEnabled(pluginId: string, enabled: boolean): Promise<void> {
  if (!hasTauriBridge()) return;
  await invoke(COMMANDS.pluginSetInstalledEnabled, { pluginId, enabled } satisfies CommandArgs[typeof COMMANDS.pluginSetInstalledEnabled]);
}

export async function uninstallPlugin(pluginId: string): Promise<void> {
  if (!hasTauriBridge()) return;
  await invoke(COMMANDS.pluginUninstall, { pluginId } satisfies CommandArgs[typeof COMMANDS.pluginUninstall]);
}
