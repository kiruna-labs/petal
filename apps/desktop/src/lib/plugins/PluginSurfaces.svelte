<!--
  Mounts the shared plugin host inside the meeting route (plugins/README.md
  §2.3, §2.7): the hidden logic-frame container, the overlay layer that sits
  over the gallery/pill (pointer-events: none), and the fixed popover layer.
  Exposes the toolbar-button models (bindable) and `activate()` for the
  route's snippet to call. Meeting facts are props; the component diffs the
  presence list into meeting.* plugin events.
-->
<script lang="ts">
  import { onDestroy, untrack } from 'svelte';
  import type { MeetingPhase, Participant } from '@petal/shared/plugin-host/api';
  import { createPluginHost, type PluginHost } from '@petal/shared/plugin-host/host';
  import { hostCompatibility } from '@petal/shared/plugin-host/manifest';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { EVENTS, hasTauriBridge, type PluginDataEvent, type PluginStateChangedEvent } from '$lib/ipc';
  import { base64ToBytes } from '@petal/shared/plugin-host/topics';
  import { diffPluginState, pluginsFromMetadata, type PluginAdverts } from '@petal/shared/plugin-host/metadata';
  import { enabledPlugins } from './pluginCatalog';
  import { createTauriAdapter } from './tauriAdapter';

  interface Props {
    participants: Participant[];
    roomLabel: string;
    phase: MeetingPhase;
    /** The client's release version, or null until it is known. Plugins boot the moment it arrives. */
    hostVersion: string | null;
    onToast: (text: string, variant: 'info' | 'degraded') => void;
    buttons?: ToolbarButtonModel[];
  }

  let { participants, roomLabel, phase, hostVersion, onToast, buttons = $bindable([]) }: Props = $props();

  let logicEl: HTMLDivElement;
  let overlayEl: HTMLDivElement;
  let popoverEl: HTMLDivElement;
  let host: PluginHost | null = null;

  export function activate(pluginId: string, buttonId: string, anchor: HTMLElement) {
    host?.activateButton(pluginId, buttonId, anchor);
  }

  // Boot once, when the real host version is known. The route resolves
  // `getVersion()` asynchronously, so on desktop the version arrives AFTER
  // this component mounts; booting in onMount against a placeholder made
  // `hostCompatibility` fail and skipped every built-in (PR #4 review).
  let booted = false;
  $effect(() => {
    const version = hostVersion;
    if (version === null || booted) return;
    booted = true;
    untrack(() => boot(version));
  });

  function boot(version: string) {
    host = createPluginHost({
      document,
      adapter: createTauriAdapter({
        participants: () => participants,
        roomLabel: () => roomLabel,
        phase: () => phase,
        toast: onToast,
        adverts: () => advertsByIdentity
      }),
      hostVersion: version,
      mounts: { logic: logicEl, overlay: overlayEl, popoverLayer: popoverEl },
      onButtonsChanged: (next) => (buttons = next),
      warn: (message) => console.warn(message)
    });
    for (const { plugin, source } of enabledPlugins()) {
      const compat = hostCompatibility(plugin.manifest, version);
      // Dev builds report a non-numeric version ("dev"); run built-ins anyway there.
      if (!compat.ok && /^\d/.test(version)) {
        console.warn(`plugin ${plugin.manifest.id} skipped: ${compat.reason}`);
        continue;
      }
      host.load(plugin, source);
    }
    listenForPluginData();
  }

  // Inbound plugin packets (Rust plugins::bus already validated topic, size,
  // sender, and rate). Resolve the sender from presence when known so the
  // plugin sees speaking/mute state; otherwise a minimal participant.
  // Remote participants' `plugins` adverts (metadata.ts), fed by the Rust
  // bus's plugin-state-changed event; diffed into per-plugin state.changed.
  const advertsByIdentity = new Map<string, PluginAdverts>();
  function applyAdverts(identity: string, next: PluginAdverts) {
    const previous = advertsByIdentity.get(identity) ?? {};
    if (Object.keys(next).length === 0) advertsByIdentity.delete(identity);
    else advertsByIdentity.set(identity, next);
    for (const change of diffPluginState(previous, next)) {
      if (host?.isLoaded(change.pluginId)) host.emit(change.pluginId, 'state.changed', { identity, value: change.value });
    }
  }

  let unlistenData: UnlistenFn | undefined;
  let unlistenState: UnlistenFn | undefined;
  let destroyed = false;
  function listenForPluginData() {
    if (!hasTauriBridge()) return;
    listen<PluginStateChangedEvent>(EVENTS.pluginStateChanged, (event) => {
      // Re-validate through the shared parser so both clients apply identical rules.
      applyAdverts(event.payload.identity, pluginsFromMetadata(JSON.stringify({ plugins: event.payload.plugins })));
    })
      .then((un) => {
        if (destroyed) un();
        else unlistenState = un;
      })
      .catch(() => {});
    listen<PluginDataEvent>(EVENTS.pluginData, (event) => {
      const p = event.payload;
      if (!host || !host.isLoaded(p.pluginId)) return;
      const known = participants.find((x) => x.identity === p.senderIdentity);
      const sender: Participant = known ?? {
        identity: p.senderIdentity,
        name: p.senderName ?? p.senderIdentity,
        isLocal: false,
        speaking: false,
        micMuted: false
      };
      host.deliverData(p.pluginId, { sub: p.sub, sender, payload: base64ToBytes(p.payloadBase64) });
    })
      .then((un) => {
        if (destroyed) un();
        else unlistenData = un;
      })
      .catch(() => {});
  }

  onDestroy(() => {
    destroyed = true;
    unlistenData?.();
    unlistenState?.();
    host?.dispose();
    host = null;
  });

  // Presence diff -> meeting.* events. Snapshot reads inside the effect
  // track `participants`; the previous list is plain state.
  let previous: Participant[] = [];
  $effect(() => {
    const next = participants;
    if (!host) return;
    const before = new Map(previous.map((p) => [p.identity, p]));
    const after = new Map(next.map((p) => [p.identity, p]));
    for (const [identity, p] of after) {
      const old = before.get(identity);
      if (!old) host.broadcast('meeting.participant-joined', p);
      else if (old.name !== p.name || old.speaking !== p.speaking || old.micMuted !== p.micMuted) {
        host.broadcast('meeting.participant-changed', p);
      }
    }
    for (const [identity, p] of before) {
      if (!after.has(identity)) {
        host.broadcast('meeting.participant-left', p);
        applyAdverts(identity, {});
      }
    }
    previous = next;
  });

  let lastPhase: MeetingPhase | null = null;
  $effect(() => {
    const info = { label: roomLabel, phase };
    host?.broadcast('meeting.phase', info);
    // Advertisements published before the room existed were refused; redo them now.
    if (phase === 'connected' && lastPhase !== 'connected') host?.readvertise();
    lastPhase = phase;
  });
</script>

<div class="plugin-logic" bind:this={logicEl} hidden aria-hidden="true"></div>
<div class="plugin-overlay" bind:this={overlayEl}></div>
<div class="plugin-popover-layer" bind:this={popoverEl}></div>

<style>
  .plugin-overlay {
    position: absolute;
    inset: 0;
    pointer-events: none;
    overflow: hidden;
    z-index: 5;
  }

  .plugin-popover-layer {
    position: fixed;
    inset: 0;
    pointer-events: none;
    z-index: 40;
  }

  .plugin-popover-layer > :global(*) {
    pointer-events: auto;
  }
</style>
