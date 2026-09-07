// Desktop HostAdapter: plugin requests -> the meeting route's state and Tauri
// plugins. M1 scope: meeting snapshot from the route's presence, storage in
// localStorage (moves to a Rust-owned file with the registry in I-5a),
// clipboard via the Tauri clipboard plugin, toast via the route. Publish and
// state report `unavailable` until the Rust data bus lands (I-3).
// Design: plugins/README.md §2.3.

import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import type { Json, MeetingPhase, Participant } from '@petal/shared/plugin-host/api';
import { bridgeFailure } from '@petal/shared/plugin-host/broker';
import type { PluginHostAdapter } from '@petal/shared/plugin-host/host';
import { PLUGIN_KV_STORAGE_PREFIX } from '@petal/shared/plugin-host/settingsModel';
import { browserStorage } from '$lib/data/storageKeys';

export interface TauriAdapterDeps {
  participants(): Participant[];
  roomLabel(): string;
  phase(): MeetingPhase;
  toast(text: string, variant: 'info' | 'degraded'): void;
}

export function createTauriAdapter(deps: TauriAdapterDeps): PluginHostAdapter {
  const storage = browserStorage();

  function kvKey(pluginId: string): string {
    return `${PLUGIN_KV_STORAGE_PREFIX}${pluginId}.v1`;
  }
  function readKv(pluginId: string): Record<string, Json> {
    try {
      const raw = storage?.getItem(kvKey(pluginId));
      const parsed: unknown = raw ? JSON.parse(raw) : {};
      return typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, Json>) : {};
    } catch {
      return {};
    }
  }
  function writeKv(pluginId: string, value: Record<string, Json>): void {
    const text = JSON.stringify(value);
    if (text.length > 64 * 1024) throw bridgeFailure('invalid', 'plugin storage is full (64 KB)');
    storage?.setItem(kvKey(pluginId), text);
  }

  return {
    meeting: {
      self: () => deps.participants().find((p) => p.isLocal) ?? null,
      participants: () => deps.participants(),
      room: () => ({ label: deps.roomLabel(), phase: deps.phase() }),
    },
    async publishData() {
      throw bridgeFailure('unavailable', 'meeting-wide plugin messages are not wired on this host yet (M2)');
    },
    async setState() {
      throw bridgeFailure('unavailable', 'plugin state sharing is not wired on this host yet (M2)');
    },
    storage: {
      async get(pluginId, key) {
        return readKv(pluginId)[key];
      },
      async set(pluginId, key, value) {
        const kv = readKv(pluginId);
        kv[key] = value;
        writeKv(pluginId, kv);
      },
      async delete(pluginId, key) {
        const kv = readKv(pluginId);
        delete kv[key];
        writeKv(pluginId, kv);
      },
      async keys(pluginId) {
        return Object.keys(readKv(pluginId));
      },
    },
    toast(_pluginId, text, variant) {
      deps.toast(text, variant);
    },
    async fetch() {
      throw bridgeFailure('unavailable', 'plugin network access is not wired on this host yet (M4)');
    },
    async clipboardWriteText(text) {
      await writeText(text);
    },
    log(pluginId, level, args) {
      const line = `[plugin ${pluginId}] ${args.join(' ')}`;
      if (level === 'error') console.error(line);
      else if (level === 'warn') console.warn(line);
      else console.info(line);
    },
    onFrameEvent(pluginId, event, payload) {
      if (event === 'error') {
        console.error(`[plugin ${pluginId}] failed to start:`, (payload as { message?: string } | undefined)?.message);
      }
    },
  };
}
