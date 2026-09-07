// Browser-client HostAdapter: how plugin requests reach the web client's own
// state. Meeting facts come from the live LiveKit Room; storage is
// localStorage; toast is the shared pill; the meeting-wide transport
// (publish/state) reports `unavailable` until M2 wires the data bus.
// Design: plugins/README.md §2.3.

import type { Participant as LkParticipant, Room } from 'livekit-client';
import type { Json, Participant, RoomInfo, MeetingPhase } from '@petal/shared/plugin-host/api';
import { bridgeFailure, type LoadedPlugin } from '@petal/shared/plugin-host/broker';
import type { PluginHostAdapter } from '@petal/shared/plugin-host/host';
import { PLUGIN_KV_STORAGE_PREFIX } from '@petal/shared/plugin-host/settingsModel';
import type { FetchParams, FetchResponse } from '@petal/shared/plugin-host/protocol';
import { pluginTopic } from '@petal/shared/plugin-host/topics';
import { mergePluginMetadata, pluginsFromMetadata } from '@petal/shared/plugin-host/metadata';
import { displayNameForParticipant } from '../tiles.ts';

export interface WebAdapterDeps {
  room(): Room | null;
  roomLabel(): string;
  toast(text: string): void;
  log(line: string, kind?: 'info' | 'ok' | 'warn' | 'error'): void;
  storage?: Storage;
}

export function participantFromLiveKit(p: LkParticipant, isLocal: boolean): Participant {
  return {
    identity: p.identity,
    name: displayNameForParticipant(p),
    isLocal,
    speaking: p.isSpeaking,
    micMuted: !p.isMicrophoneEnabled,
  };
}

function phaseOf(room: Room | null): MeetingPhase {
  if (!room) return 'disconnected';
  switch (room.state) {
    case 'connected':
    case 'reconnecting':
      return 'connected';
    case 'connecting':
      return 'connecting';
    default:
      return 'disconnected';
  }
}

export function createWebAdapter(deps: WebAdapterDeps): PluginHostAdapter {
  const storage = deps.storage ?? (typeof localStorage === 'undefined' ? undefined : localStorage);

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

  const room = (): RoomInfo => ({ label: deps.roomLabel(), phase: phaseOf(deps.room()) });

  return {
    meeting: {
      self() {
        const r = deps.room();
        return r ? participantFromLiveKit(r.localParticipant, true) : null;
      },
      participants() {
        const r = deps.room();
        if (!r) return [];
        return [participantFromLiveKit(r.localParticipant, true), ...[...r.remoteParticipants.values()].map((p) => participantFromLiveKit(p, false))];
      },
      room,
    },
    async publishData(plugin: LoadedPlugin, params) {
      const room = deps.room();
      if (!room || room.state !== 'connected') throw bridgeFailure('unavailable', 'not connected to a meeting');
      // Topic is derived from the plugin's own id here, never taken from the plugin.
      const topic = pluginTopic(plugin.manifest.id, params.sub);
      await room.localParticipant.publishData(params.payload, {
        reliable: params.reliable,
        topic,
        destinationIdentities: params.to && params.to.length > 0 ? params.to : undefined,
      });
    },
    async publishPluginEntry(pluginId, entry) {
      const room = deps.room();
      if (!room || room.state !== 'connected') throw bridgeFailure('unavailable', 'not connected to a meeting');
      const local = room.localParticipant as { metadata?: string; setMetadata?: (metadata: string) => Promise<void> };
      if (typeof local.setMetadata !== 'function') throw bridgeFailure('unavailable', 'metadata is not writable here');
      let merged: string;
      try {
        merged = mergePluginMetadata(local.metadata, pluginId, entry);
      } catch (e) {
        throw bridgeFailure('invalid', (e as Error).message);
      }
      await local.setMetadata(merged);
    },
    stateSnapshot(pluginId) {
      const room = deps.room();
      const out: Record<string, Json> = {};
      if (!room) return out;
      for (const p of room.remoteParticipants.values()) {
        const state = pluginsFromMetadata(p.metadata)[pluginId]?.state;
        if (state !== undefined) out[p.identity] = state;
      }
      return out;
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
    toast(_pluginId, text) {
      deps.toast(text);
    },
    async fetch(_plugin: LoadedPlugin, _params: FetchParams): Promise<FetchResponse> {
      throw bridgeFailure('unavailable', 'plugin network access is not wired on this host yet (M4)');
    },
    async clipboardWriteText(text) {
      await navigator.clipboard.writeText(text);
    },
    log(pluginId, level, args) {
      const line = `[plugin ${pluginId}] ${args.join(' ')}`;
      deps.log(line, level === 'error' ? 'error' : level === 'warn' ? 'warn' : 'info');
    },
    onFrameEvent(pluginId, event, payload) {
      if (event === 'error') {
        const message = (payload as { message?: string } | undefined)?.message ?? 'unknown error';
        deps.log(`[plugin ${pluginId}] failed to start: ${message}`, 'error');
      }
    },
  };
}
