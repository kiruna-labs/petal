<script lang="ts">
  import MeetingChrome from '$lib/components/MeetingChrome.svelte';
  import ChatDrawer from '@petal/shared/ui/components/ChatDrawer.svelte';
  import type { ChatMessage } from '@petal/shared/logic/chat';
  import PluginToolbarButtons from '$lib/plugins/PluginToolbarButtons.svelte';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';

  // The real desktop meeting chrome with the chat drawer open beside the
  // tiles: what `meetingChatLayoutRendered.test.ts` measures at every gallery
  // size (the window becomes the pill below GALLERY_BREAKPOINT, 520 px).
  const base = Date.UTC(2026, 8, 29, 16, 0, 0);
  const person = (identity: string, name: string) => ({ identity, name });
  // A long history for the scroll test: `window.__chatCount` extra lines.
  const extraCount = (window as unknown as { __chatCount?: number }).__chatCount ?? 0;
  let messages = $state<ChatMessage[]>([
    ...Array.from({ length: extraCount }, (_, i) => ({
      id: `m-long-${String(i).padStart(4, '0')}`,
      text: i % 4 === 0 ? 'A longer history line so the list wraps and keeps growing past the panel height.' : `History ${i + 1}`,
      t: base - 3_600_000 + i * 30_000,
      sender: i % 2 ? person('theo-1', 'Theo') : person('mira-1', 'Mira'),
      self: false,
      relayed: false,
      via: null,
      local: false
    })),
    ...Array.from({ length: 8 }, (_, i) => ({
      id: `m-layout-${String(i).padStart(4, '0')}`,
      text: i % 3 === 0 ? 'A longer line so the list has something to wrap at narrow widths.' : `Message ${i + 1}`,
      t: base + i * 30_000,
      sender: i % 2 ? person('theo-1', 'Theo') : person('mira-1', 'Mira'),
      self: false,
      relayed: false,
      via: null,
      local: false
    })),
    // A plugin post for Theo (his avatar, "via Timer"), then Timer's private
    // answer to me (the plugin's puzzle, no person's color).
    { id: 'm-layout-0100', text: '⏱ Timer started: 5 min', t: base + 300_000, sender: person('theo-1', 'Theo'), self: false, relayed: false, via: { id: 'petal.timer', name: 'Timer' }, local: false },
    { id: 'm-layout-0101', text: 'Three timers are already running.', t: base + 310_000, sender: person('me-1', 'Max'), self: true, relayed: false, via: { id: 'petal.timer', name: 'Timer' }, local: true }
  ]);
  // Meeting colors as the host resolves them (identityColor.ts hex values).
  const meetingColors: Record<string, string> = { 'me-1': '#7ff0a3', 'mira-1': '#f06cc9', 'theo-1': '#6e8bff' };
  (window as unknown as { __controls: string[] }).__controls = [];
  const participants = [
    { id: 'me-1', name: 'Max' },
    { id: 'mira-1', name: 'Mira' },
    { id: 'theo-1', name: 'Theo' }
  ];
  (window as unknown as { __chatSent: string[] }).__chatSent = [];
  const fixtureWindow = window as unknown as { __chatOpen?: boolean; __chatUnread?: number };
  // The default-on built-in Reactions button: the real control row carries it,
  // and it is what pushed the row past the 520 px gallery minimum.
  const pluginButtons: ToolbarButtonModel[] = [
    {
      pluginId: 'petal.reactions',
      pluginName: 'Reactions',
      pluginSource: 'builtin',
      buttonId: 'react',
      label: 'React',
      icon: 'smile',
      badge: null,
      disabled: false,
      opens: 'popover:picker',
      ariaLabel: 'React (Reactions)'
    }
  ];
</script>

{#snippet pluginActions()}
  <PluginToolbarButtons buttons={pluginButtons} onActivate={() => {}} />
{/snippet}

{#snippet chatDrawer()}
  <ChatDrawer
    {messages}
    onSend={(text) => {
      (window as unknown as { __chatSent: string[] }).__chatSent.push(text);
      messages = [...messages, { id: `m-sent-${messages.length}`, text, t: base + 999_000, sender: person('me-1', 'Max'), self: true, relayed: false, via: null, local: false }];
    }}
    onClose={() => {}}
    colorFor={(identity) => meetingColors[identity] ?? null}
    now={() => base + 1_000_000}
    locale="en-US"
  />
{/snippet}

<!-- The meeting route's own wrappers (routes/meeting/[room]/+page.svelte). -->
<main>
  <div class="frame">
    <div class="chrome-shell">
<MeetingChrome
  roomName="meeting-chat-fixture"
  elapsed="12:00"
  {participants}
  expanded={true}
  frameless={true}
  chatOpen={fixtureWindow.__chatOpen ?? true}
  chatUnread={fixtureWindow.__chatUnread ?? 0}
  {chatDrawer}
  {pluginActions}
  onControl={(icon) => (window as unknown as { __controls: string[] }).__controls.push(icon)}
/>
      <!-- Same box and stacking as lib/plugins/PluginSurfaces.svelte's overlay and the host's
           overlay frame (shared/plugin-host/host.ts): full-viewport, pointer-events none, z 5. -->
      <div class="plugin-overlay">
        <iframe title="Reactions overlay" sandbox="allow-scripts" srcdoc="<!doctype html><body></body>"></iframe>
      </div>
    </div>
  </div>
</main>

<style>
  main { display: flex; height: 100%; width: 100%; background: var(--bg-base-2); box-sizing: border-box; overscroll-behavior: none; }
  .frame { position: relative; width: 100%; height: 100%; overflow: hidden; overscroll-behavior: none; }
  .chrome-shell { position: relative; height: 100%; overflow-y: auto; overscroll-behavior: none; }
  .plugin-overlay { position: absolute; inset: 0; pointer-events: none; overflow: hidden; z-index: 5; }
  .plugin-overlay iframe { position: absolute; inset: 0; width: 100%; height: 100%; border: 0; background: transparent; pointer-events: none; }
</style>
