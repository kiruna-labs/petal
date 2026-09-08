// Plugin data-channel topics: `plugin/<id>` or `plugin/<id>/<sub>`. Pinned in
// contracts/petal-contracts.json (`topics.pluginPrefix`, `pluginTopicVectors`)
// and mirrored by apps/desktop/src-tauri/src/plugins/bus.rs -- change all
// three together (docs/CONTRACTS.md "Plugin bus").

import { PLUGIN_ID_RE, isPluginId } from './manifest.ts';

export const PLUGIN_TOPIC_PREFIX = 'plugin/';
const SUB_RE = /^[a-z0-9][a-z0-9-]{0,31}$/;

export interface PluginTopic {
  pluginId: string;
  sub: string | null;
}

export function isPluginSub(value: unknown): value is string {
  return typeof value === 'string' && SUB_RE.test(value);
}

/** `plugin/petal.reactions/emoji` -> { pluginId, sub }; null for anything not a well-formed plugin topic. */
export function parsePluginTopic(topic: string | null | undefined): PluginTopic | null {
  if (!topic || !topic.startsWith(PLUGIN_TOPIC_PREFIX)) return null;
  const rest = topic.slice(PLUGIN_TOPIC_PREFIX.length);
  const slash = rest.indexOf('/');
  const pluginId = slash === -1 ? rest : rest.slice(0, slash);
  const sub = slash === -1 ? null : rest.slice(slash + 1);
  if (!isPluginId(pluginId) || !PLUGIN_ID_RE.test(pluginId)) return null;
  if (sub !== null && !isPluginSub(sub)) return null;
  return { pluginId, sub };
}

export function pluginTopic(pluginId: string, sub: string | null | undefined): string {
  if (!isPluginId(pluginId)) throw new Error(`invalid plugin id: ${pluginId}`);
  if (sub === null || sub === undefined) return `${PLUGIN_TOPIC_PREFIX}${pluginId}`;
  if (!isPluginSub(sub)) throw new Error(`invalid topic sub: ${sub}`);
  return `${PLUGIN_TOPIC_PREFIX}${pluginId}/${sub}`;
}

/** Base64 helpers for carrying bytes through JSON (Tauri IPC). */
export function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

export function base64ToBytes(text: string): Uint8Array {
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}
