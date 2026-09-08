// The `plugins` participant-metadata key: how a running meeting-scoped plugin
// advertises itself (and optionally a small shared state) to everyone in the
// room. LOCKSTEP with apps/desktop/src-tauri/src/plugins/bus.rs
// (`plugins_from_metadata`) and contracts/petal-contracts.json
// (`pluginStateMetadata`) -- docs/CONTRACTS.md "Plugin bus".
//
//   "plugins": { "<pluginId>": { "v": "1.0.0", "src": "builtin", "state": <json>? } }
//
// Written by merging into the participant's existing metadata blob (which
// also carries petalWindowKinds, petalIdentityPaletteIndex, ...); never by
// replacing it. Self-set metadata is a discovery signal, not a security
// boundary: receivers still attribute data packets to the authenticated
// LiveKit sender.

import type { Json } from './api.ts';
import type { PluginSource } from './broker.ts';
import { isPluginId, isReleaseVersion } from './manifest.ts';
import { jsonByteLength } from './rateLimit.ts';

export const PLUGINS_METADATA_KEY = 'plugins';

export const PLUGIN_STATE_LIMITS = {
  /** `state` per plugin, JSON bytes. */
  perPluginStateBytes: 2048,
  /** The whole `plugins` object, JSON bytes. */
  totalBytes: 8192,
} as const;

export interface PluginAdvert {
  v: string;
  src: PluginSource;
  state?: Json;
}

export type PluginAdverts = Record<string, PluginAdvert>;

function parseMetadataObject(metadata: string | null | undefined): Record<string, unknown> {
  if (!metadata) return {};
  try {
    const parsed: unknown = JSON.parse(metadata);
    return typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

function isSource(value: unknown): value is PluginSource {
  return value === 'builtin' || value === 'registry' || value === 'dev';
}

/** Read and validate the `plugins` key. Malformed entries are dropped individually. */
export function pluginsFromMetadata(metadata: string | null | undefined): PluginAdverts {
  const root = parseMetadataObject(metadata);
  const raw = root[PLUGINS_METADATA_KEY];
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return {};
  const out: PluginAdverts = {};
  for (const [id, entry] of Object.entries(raw as Record<string, unknown>)) {
    if (!isPluginId(id) || typeof entry !== 'object' || entry === null || Array.isArray(entry)) continue;
    const e = entry as Record<string, unknown>;
    if (!isReleaseVersion(e.v) || !isSource(e.src)) continue;
    const advert: PluginAdvert = { v: e.v, src: e.src };
    if (e.state !== undefined && e.state !== null && jsonByteLength(e.state) <= PLUGIN_STATE_LIMITS.perPluginStateBytes) {
      advert.state = e.state as Json;
    }
    out[id] = advert;
  }
  return out;
}

/**
 * Return the participant metadata with `plugins[pluginId]` set (or removed
 * when `entry` is null), every other key untouched. Throws when the result
 * would exceed the total budget so the caller can surface a typed error.
 */
export function mergePluginMetadata(currentMetadata: string | null | undefined, pluginId: string, entry: PluginAdvert | null): string {
  if (!isPluginId(pluginId)) throw new Error(`invalid plugin id: ${pluginId}`);
  const root = parseMetadataObject(currentMetadata);
  const rawPlugins = root[PLUGINS_METADATA_KEY];
  const plugins: Record<string, unknown> =
    typeof rawPlugins === 'object' && rawPlugins !== null && !Array.isArray(rawPlugins) ? { ...(rawPlugins as Record<string, unknown>) } : {};
  if (entry === null) {
    delete plugins[pluginId];
  } else {
    if (entry.state !== undefined && jsonByteLength(entry.state) > PLUGIN_STATE_LIMITS.perPluginStateBytes) {
      throw new Error(`plugin state exceeds ${PLUGIN_STATE_LIMITS.perPluginStateBytes} bytes`);
    }
    const clean: PluginAdvert = { v: entry.v, src: entry.src };
    if (entry.state !== undefined && entry.state !== null) clean.state = entry.state;
    plugins[pluginId] = clean;
  }
  if (Object.keys(plugins).length === 0) delete root[PLUGINS_METADATA_KEY];
  else {
    if (jsonByteLength(plugins) > PLUGIN_STATE_LIMITS.totalBytes) {
      throw new Error(`plugins metadata exceeds ${PLUGIN_STATE_LIMITS.totalBytes} bytes`);
    }
    root[PLUGINS_METADATA_KEY] = plugins;
  }
  return JSON.stringify(root);
}

export interface PluginStateChange {
  pluginId: string;
  value: Json | undefined;
}

/** Per-plugin state deltas between two adverts maps for one participant (adverts that vanished report `undefined`). */
export function diffPluginState(previous: PluginAdverts, next: PluginAdverts): PluginStateChange[] {
  const out: PluginStateChange[] = [];
  const ids = new Set([...Object.keys(previous), ...Object.keys(next)]);
  for (const id of ids) {
    const before = previous[id]?.state;
    const after = next[id]?.state;
    if (JSON.stringify(before) !== JSON.stringify(after)) out.push({ pluginId: id, value: after });
  }
  return out;
}
