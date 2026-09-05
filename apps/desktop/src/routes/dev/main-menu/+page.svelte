<!--
  Dev-only visual QA harness for MainMenu (Petal-Build-Map.md §2.5). Renders
  the composition with the realistic sample data named in the build map:
  a live `eng-sync` room (Jordan Kim signed in) plus empty `design-review`/
  `standup` rows. Throwaway scaffolding, matches the /dev/components pattern.
-->
<script lang="ts">
  import MainMenu from '$lib/components/MainMenu.svelte';
  import type { IdentityColor } from '$lib/components/Avatar.svelte';

  interface RoomParticipant {
    name: string;
    identity: IdentityColor;
  }

  const engSyncParticipants: RoomParticipant[] = [
    { name: 'Marco', identity: 'blue' },
    { name: 'Devin', identity: 'lilac' },
    { name: 'Sana', identity: 'green' },
    { name: 'Priya', identity: 'amber' },
    { name: 'Owen', identity: 'slate' }
  ];

  const manyRooms = Array.from({ length: 15 }, (_, index) => `room-${index + 1}`);
</script>

<div class="harness">
  <h1>Petal — main menu dev harness</h1>
  <p class="intro">MainMenu composition, per Petal-Build-Map.md §2.5. Dev-only route.</p>

  <div class="row">
    <div class="cell">
      <MainMenu
        userName="Jordan Kim"
        userIdentity="plum"
        liveRoom={{ name: 'eng-sync', participants: engSyncParticipants }}
        emptyRooms={['design-review', 'standup']}
        onJoinLive={() => console.log('join eng-sync')}
        onOpenSettings={() => console.log('open settings')}
        onQuit={() => console.log('quit')}
      />
      <span class="caption">Live room (eng-sync, avatar stack with overflow) + empty rows — profile menu has Settings + Quit (#20)</span>
    </div>

    <div class="cell">
      <MainMenu
        userName="Jordan Kim"
        userIdentity="plum"
        emptyRooms={['eng-sync', 'design-review', 'standup']}
      />
      <span class="caption">No live room — all rows neutral/empty</span>
    </div>

    <div class="cell">
      <div class="fixed-window-preview">
        <MainMenu
          userName="Jordan Kim"
          userIdentity="plum"
          emptyRooms={manyRooms}
          onCreateMeeting={(name, displayName) => console.log('create meeting', name, displayName)}
          onJoinByCode={(name) => console.log('join by code', name)}
          frameless
        />
      </div>
      <span class="caption">Frameless fixed 400x640 window — long room list scrolls under pinned controls</span>
    </div>
  </div>
</div>

<style>
  .harness {
    min-height: 100%;
    background: var(--bg-base);
    color: var(--text-primary);
    font-family: var(--font-ui);
    padding: 32px 40px 80px;
    overflow-y: auto;
  }

  h1 {
    font-family: var(--font-display);
    font-weight: 700;
    font-size: 28px;
    margin: 0 0 4px;
  }

  .intro {
    color: var(--text-muted);
    font-size: var(--text-body);
    margin: 0 0 32px;
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 40px;
    align-items: flex-start;
  }

  .cell {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
  }

  .fixed-window-preview {
    width: 400px;
    height: 640px;
    display: flex;
    overflow: hidden;
    background: var(--menu-shell);
  }

  .caption {
    font-size: 10.5px;
    font-family: var(--font-mono);
    color: var(--text-muted);
    text-align: center;
    max-width: 380px;
  }
</style>
