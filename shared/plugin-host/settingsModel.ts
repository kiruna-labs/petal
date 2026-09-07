// Pure model behind the Settings "Plugins" section (desktop) and the web
// client's plugins sheet, plus the enabled-state persistence both share in
// M1 (localStorage; the desktop moves installed state to a Rust-owned file
// in I-5a, this module keeps the same shape). Design: plugins/README.md §2.7.

import type { PluginSource } from './broker.ts';
import type { Permission, PluginManifest } from './manifest.ts';

export const PLUGIN_ENABLED_STORAGE_KEY = 'petal.plugins.enabled.v1';
export const PLUGIN_KV_STORAGE_PREFIX = 'petal.plugins.kv.';

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface InstalledPlugin {
  manifest: PluginManifest;
  source: PluginSource;
  /** Built-ins choose their default; everything installed explicitly defaults to enabled. */
  enabledByDefault: boolean;
  /** Bundle entry source (`files[manifest.entry]`). */
  source_js: string;
}

export type EnabledOverrides = Record<string, boolean>;

export function readEnabledOverrides(storage: StorageLike | undefined): EnabledOverrides {
  if (!storage) return {};
  try {
    const raw = storage.getItem(PLUGIN_ENABLED_STORAGE_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) return {};
    const out: EnabledOverrides = {};
    for (const [k, v] of Object.entries(parsed)) if (typeof v === 'boolean') out[k] = v;
    return out;
  } catch {
    return {};
  }
}

export function writeEnabledOverride(storage: StorageLike | undefined, pluginId: string, enabled: boolean): EnabledOverrides {
  const next = { ...readEnabledOverrides(storage), [pluginId]: enabled };
  try {
    storage?.setItem(PLUGIN_ENABLED_STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Storage may be unavailable (private mode); the in-memory value still applies this session.
  }
  return next;
}

export function isPluginEnabled(plugin: Pick<InstalledPlugin, 'manifest' | 'enabledByDefault'>, overrides: EnabledOverrides): boolean {
  const override = overrides[plugin.manifest.id];
  return override === undefined ? plugin.enabledByDefault : override;
}

/** Remove every plugin-owned key (enabled map + per-plugin KV). Used by factory reset. */
export function clearPluginStorage(storage: (StorageLike & { length?: number; key?(i: number): string | null }) | undefined): void {
  if (!storage) return;
  storage.removeItem(PLUGIN_ENABLED_STORAGE_KEY);
  if (typeof storage.key !== 'function' || typeof storage.length !== 'number') return;
  const doomed: string[] = [];
  for (let i = 0; i < storage.length; i++) {
    const k = storage.key(i);
    if (k && k.startsWith(PLUGIN_KV_STORAGE_PREFIX)) doomed.push(k);
  }
  for (const k of doomed) storage.removeItem(k);
}

export const SOURCE_LABELS: Record<PluginSource, string> = {
  builtin: 'Built-in',
  registry: 'Registry',
  dev: 'Dev',
};

/** One plain phrase per permission, written for a 400 px Settings row. */
export const PERMISSION_LABELS: Record<string, string> = {
  'meeting:read': 'See who is in the meeting',
  'data:publish': 'Send messages to everyone in the meeting',
  'state:write': 'Share its status with the meeting',
  storage: 'Save its own settings on this device',
  'ui:toolbar-button': 'Add a button to the meeting toolbar',
  'ui:header-button': 'Add a button to shared-window headers',
  'ui:overlay': 'Draw over the meeting view',
  'ui:popover': 'Open a small panel from its button',
  'ui:panel': 'Open a side panel',
  'ui:settings': 'Add a page to Settings',
  'ui:toast': 'Show short notices',
  'shares:read': 'See which windows are shared',
  'clipboard:write': 'Copy text to your clipboard',
  'net:fetch:user-urls': 'Send data to web addresses you enter in its settings',
};

export function permissionLabel(permission: Permission | string): string {
  const known = PERMISSION_LABELS[permission];
  if (known) return known;
  if (permission.startsWith('net:fetch:')) return `Contact ${permission.slice('net:fetch:'.length)}`;
  return permission;
}

export interface PluginSettingsRow {
  id: string;
  name: string;
  version: string;
  description: string;
  sourceLabel: string;
  source: PluginSource;
  enabled: boolean;
  canUninstall: boolean;
  permissions: { id: string; label: string }[];
}

export function pluginSettingsRows(installed: readonly InstalledPlugin[], overrides: EnabledOverrides): PluginSettingsRow[] {
  return [...installed]
    .sort((a, b) => a.manifest.name.localeCompare(b.manifest.name))
    .map((p) => ({
      id: p.manifest.id,
      name: p.manifest.name,
      version: p.manifest.version,
      description: p.manifest.description,
      sourceLabel: SOURCE_LABELS[p.source],
      source: p.source,
      enabled: isPluginEnabled(p, overrides),
      canUninstall: p.source !== 'builtin',
      permissions: p.manifest.permissions.map((id) => ({ id, label: permissionLabel(id) })),
    }));
}
