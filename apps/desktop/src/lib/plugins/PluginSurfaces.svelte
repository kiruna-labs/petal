<!--
  Mounts the shared plugin host inside the meeting route (plugins/README.md
  §2.3, §2.7): the hidden logic-frame container, the overlay layer that sits
  over the gallery/pill (pointer-events: none), and the fixed popover layer.
  Exposes the toolbar-button models (bindable) and `activate()` for the
  route's snippet to call. Meeting facts are props; the component diffs the
  presence list into meeting.* plugin events.
-->
<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { MeetingPhase, Participant } from '@petal/shared/plugin-host/api';
  import { createPluginHost, type PluginHost } from '@petal/shared/plugin-host/host';
  import { hostCompatibility } from '@petal/shared/plugin-host/manifest';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';
  import { enabledPlugins } from './pluginCatalog';
  import { createTauriAdapter } from './tauriAdapter';

  interface Props {
    participants: Participant[];
    roomLabel: string;
    phase: MeetingPhase;
    hostVersion: string;
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

  onMount(() => {
    host = createPluginHost({
      document,
      adapter: createTauriAdapter({
        participants: () => participants,
        roomLabel: () => roomLabel,
        phase: () => phase,
        toast: onToast
      }),
      hostVersion,
      mounts: { logic: logicEl, overlay: overlayEl, popoverLayer: popoverEl },
      onButtonsChanged: (next) => (buttons = next),
      warn: (message) => console.warn(message)
    });
    for (const { plugin, source } of enabledPlugins()) {
      const compat = hostCompatibility(plugin.manifest, hostVersion);
      if (!compat.ok && /^\d/.test(hostVersion)) {
        console.warn(`plugin ${plugin.manifest.id} skipped: ${compat.reason}`);
        continue;
      }
      host.load(plugin, source);
    }
  });

  onDestroy(() => {
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
    for (const [identity, p] of before) if (!after.has(identity)) host.broadcast('meeting.participant-left', p);
    previous = next;
  });

  $effect(() => {
    const info = { label: roomLabel, phase };
    host?.broadcast('meeting.phase', info);
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
