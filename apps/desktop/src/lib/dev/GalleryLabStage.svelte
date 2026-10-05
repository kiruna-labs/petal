<!--
  One native meeting window at one size, for the gallery layout lab
  ($lib/dev/galleryLab.ts). Renders what /meeting/[room] renders -- the
  frameless MeetingChrome (and through it Gallery, the view switcher, the
  chat drawer and plugin toolbar buttons) inside the route's own
  main/.frame/.chrome-shell boxes -- at exactly `scenario.width` x
  `scenario.height`, with synthetic cameras. Nothing here lays out tiles:
  every number the lab measures comes from the shipped components.
-->
<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import MeetingChrome from '$lib/components/MeetingChrome.svelte';
  import PluginToolbarButtons from '$lib/plugins/PluginToolbarButtons.svelte';
  import ChatDrawer from '@petal/shared/ui/components/ChatDrawer.svelte';
  import type { ChatMessage } from '@petal/shared/logic/chat';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';
  import type { GalleryParticipant } from '$lib/components/Gallery.svelte';
  import {
    labGalleryParticipants,
    labParticipantSpecs,
    stopLabStreams,
    type GalleryLabScenario
  } from './galleryLab';

  interface Props {
    scenario: GalleryLabScenario;
    /** Called once the requested layout mode is showing and tiles are mounted. */
    onSettled?: () => void;
  }

  let { scenario, onSettled }: Props = $props();

  // Streams are expensive (a canvas + timer each): rebuild them only when the
  // people change, never on a pure window resize.
  const peopleKey = $derived(
    `${scenario.count}|${scenario.camerasOff}|${scenario.aspect}|${scenario.longNames}|${scenario.sharing}`
  );
  let participants = $state<GalleryParticipant[]>([]);
  let builtFor = '';
  $effect(() => {
    const key = peopleKey;
    if (key === builtFor) return;
    builtFor = key;
    const previous = participants;
    participants = labGalleryParticipants(labParticipantSpecs(scenario));
    stopLabStreams(previous);
  });
  onDestroy(() => stopLabStreams(participants));

  const pluginButtons = $derived<ToolbarButtonModel[]>(
    Array.from({ length: scenario.plugins }, (_, index) => ({
      pluginId: index === 0 ? 'petal.reactions' : `lab.plugin-${index}`,
      pluginName: index === 0 ? 'Reactions' : `Lab Plugin ${index}`,
      pluginSource: index === 0 ? 'builtin' : 'registry',
      buttonId: 'go',
      label: index === 0 ? 'React' : `Tool ${index}`,
      icon: index === 0 ? 'smile' : 'puzzle',
      badge: null,
      disabled: false,
      opens: null,
      ariaLabel: index === 0 ? 'React (Reactions · built-in plugin)' : `Tool ${index} (Lab Plugin ${index} · installed plugin)`
    }))
  );

  const messages: ChatMessage[] = [
    { id: 'lab-m1', text: 'Can everyone see the build window?', t: Date.UTC(2026, 9, 5, 12, 1), sender: { identity: 'lab-1', name: 'Ada' }, self: false, relayed: false, via: null, local: false },
    { id: 'lab-m2', text: 'Yes, the left pane is the one that fails.', t: Date.UTC(2026, 9, 5, 12, 2), sender: { identity: 'lab-0', name: 'You' }, self: true, relayed: false, via: null, local: false }
  ];

  let chromeEl = $state<HTMLDivElement>();
  let expanded = $state(true);

  // The layout mode lives inside Gallery; reach it the way a person does,
  // through the top-bar view toggle, so the lab exercises the real path.
  async function syncLayoutMode(mode: GalleryLabScenario['mode']) {
    await tick();
    const toggle = chromeEl?.querySelector<HTMLButtonElement>('.layout-toggle');
    if (!toggle) return;
    const showing = chromeEl?.querySelector('.tiles.spotlight') ? 'spotlight' : 'grid';
    if (showing !== mode) toggle.click();
    await tick();
    onSettled?.();
  }

  $effect(() => {
    const mode = scenario.mode;
    void participants.length;
    void syncLayoutMode(mode);
  });
</script>

{#snippet chatDrawer()}
  <ChatDrawer {messages} onSend={() => {}} onClose={() => {}} now={() => Date.UTC(2026, 9, 5, 12, 5)} locale="en-GB" />
{/snippet}

{#snippet pluginActions(hidden: ReadonlySet<string>)}
  <PluginToolbarButtons buttons={pluginButtons} {hidden} onActivate={() => {}} />
{/snippet}

<!-- The route's own boxes (routes/meeting/[room]/+page.svelte: main, .frame,
     .chrome-shell), so overflow and scrolling behave as they do there. -->
<div class="lab-window" style:width="{scenario.width}px" style:height="{scenario.height}px" data-lab-window>
  <main class="lab-main">
    <div class="lab-frame">
      <div class="lab-chrome-shell" bind:this={chromeEl}>
        <MeetingChrome
          frameless
          roomName={scenario.longNames ? 'Quarterly platform reliability review' : 'eng-sync'}
          elapsed="12:01"
          {participants}
          micMuted={false}
          cameraOn
          sharingActive={false}
          bind:expanded
          stateTitle={scenario.stateCard ? 'Reconnecting…' : null}
          stateDetail={scenario.stateCard ? 'Your video will come back on its own.' : null}
          stateTone="warning"
          onControl={() => {}}
          onInviteLinkCopy={() => {}}
          onOpenNetwork={() => {}}
          onOpenSettings={() => {}}
          onRenameRoom={() => {}}
          onReportBug={() => {}}
          pluginActions={scenario.plugins > 0 ? pluginActions : undefined}
          chatOpen={scenario.chatOpen}
          chatUnread={scenario.chatOpen ? 0 : 2}
          {chatDrawer}
        />
      </div>
    </div>
  </main>
</div>

<style>
  .lab-window {
    position: relative;
    flex: none;
    overflow: hidden;
    contain: layout paint;
  }

  /* routes/meeting/[room]/+page.svelte `main`, `.frame`, `.chrome-shell`. */
  .lab-main {
    display: flex;
    height: 100%;
    width: 100%;
    background: var(--bg-base-2);
    box-sizing: border-box;
    overscroll-behavior: none;
  }

  .lab-frame {
    position: relative;
    width: 100%;
    height: 100%;
    overflow: hidden;
    overscroll-behavior: none;
  }

  .lab-chrome-shell {
    position: relative;
    height: 100%;
    overflow-y: auto;
    overscroll-behavior: none;
  }
</style>
