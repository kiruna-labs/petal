<!--
  MainMenu — the pre-meeting home screen. Per Petal-Build-Map.md §2.5:
  stacked hero layout, petal wordmark + user identity chip, LiveHero for the
  room that's currently live, the Start/Join controls, and neutral
  RoomRows for empty rooms
  below.

  Composition + values pulled from canvas.html §7 "Main menu — approved":
  - Card shell: `380x580`, `border-radius:20px`, menu shell background,
    `border:1px solid rgba(255,255,255,.08)`,
    `box-shadow:0 40px 90px -30px rgba(0,0,0,.6)`. This outer chrome is
    specific to the canvas.html *card mockup* frame (a device-style
    preview), not necessarily the real window chrome — kept here since nothing
    else was specified as an alternative and Tauri's real window sizing is a
    separate, later "wire up real navigation" concern per the task framing.
  - Top bar: Petal flower mark + "petal" wordmark (14px) + trailing
    compact user avatar — reuses Wordmark + Avatar
    rather than re-drawing them.
  - Profile menu: the user avatar opens the account dropdown. Settings and
    Quit live there, keeping the top bar to brand + profile instead of
    standalone utility icons.
  - Create/Join controls: one smart field that creates from names and joins
    from short access codes or invite links.
  - Below: LiveHero for the live room, then neutral RoomRows for empty ones.

  Sample room names (design-review, standup) and the live room (eng-sync)
  match Petal-Build-Map.md §2.5's named example exactly.
-->
<script lang="ts">
  import Wordmark from './Wordmark.svelte';
  import Logo from './Logo.svelte';
  import Avatar from './Avatar.svelte';
  import LiveHero from './LiveHero.svelte';
  import MenuItem from './MenuItem.svelte';
  import RoomRow from './RoomRow.svelte';
  import FeedbackModal from './FeedbackModal.svelte';
  import type { IdentityColor } from './Avatar.svelte';
  import {
    meetingCredentialFromInviteInput,
    normalizeMeetingCredential
  } from '$lib/data/meetingCode';
  import { submitMainMenuMeetingAction } from '$lib/data/mainMenuMeetingAction';
  import { hideMainWindow, minimizeMainWindow } from '$lib/data/mainWindowControls';
  import { isFeedbackEnabled } from '$lib/feedback/config';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { isMac } from '$lib/platform';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';

  interface RoomParticipant {
    name: string;
    identity: IdentityColor;
    resolvedColor?: string;
  }

  interface LiveRoom {
    name: string;
    participants: RoomParticipant[];
  }

  interface Props {
    userName: string;
    userIdentity?: IdentityColor;
    /** The one room promoted into the hero, if any is currently live. */
    liveRoom?: LiveRoom;
    /** Room rows below the hero. Includes the current room while joined. */
    emptyRooms?: string[];
    /** The room this process is currently joined to, if any. */
    currentRoom?: string | null;
    /** Occupancy snapshots for row rooms. Missing/empty renders no trailing label. */
    roomParticipantsByName?: Record<string, RoomParticipant[]>;
    /** Server-side headcounts (no identities) keyed by immutable room code. */
    roomOccupancyByName?: Record<string, number>;
    /** Optional local display labels keyed by immutable room code. */
    roomDisplayNamesByName?: Record<string, string>;
    /** Canonical user-facing letter codes keyed by immutable room code. */
    roomAccessCodesByName?: Record<string, string | null | undefined>;
    onJoinLive?: () => void;
    onOpenSettings?: () => void;
    /** Start a new meeting with an internal generated credential and optional
     * user-facing display name. Receives the internal credential; callers own
     * persistence/rename/navigation. */
    onCreateMeeting?: (name: string, displayName: string | null) => void | Promise<void>;
    /** Click a plain (empty) room row -> join it (SPEC.md §4.6: "one-click
     * join... click = join"). Optional so existing callers/dev harnesses
     * that don't pass it (`/dev/main-menu`) keep rendering exactly as
     * before, just without the click wired to anything. */
    onJoinRoom?: (name: string) => void | Promise<void>;
    onCopyRoomLink?: (name: string) => boolean | void | Promise<boolean | void>;
    /** Join a meeting this machine doesn't know yet by typing a code or
     * pasting an invite link (issue #15). Receives the PARSED room name
     * (petal://join/... and web ?code= links already unwrapped — see
     * `$lib/data/joinInput.ts`). Optional, same convention as the other
     * callbacks: without it the join field doesn't render. */
    onJoinByCode?: (name: string, accessCode: string) => void | Promise<void>;
    meetingActionError?: string | null;
    onClearMeetingActionError?: () => void;
    favoriteRooms?: string[];
    onToggleFavoriteRoom?: (name: string) => void;
    onRemoveRoom?: (name: string) => void;
    /** Quit Petal (issue #20) — rendered inside the profile dropdown. The
     * real route wires this to the `quit_app` Tauri command (clean leave_room
     * + app.exit(0)). */
    onQuit?: () => void;
    /** Real routes pass true so the menu IS the window (edge-to-edge, no
     * fixed-width floating card). Default false preserves the canvas.html
     * card-mockup chrome for the /dev/* harnesses. */
    frameless?: boolean;
  }

  let {
    userName,
    userIdentity = 'plum',
    liveRoom,
    emptyRooms = [],
    currentRoom = null,
    roomParticipantsByName = {},
    roomOccupancyByName = {},
    roomDisplayNamesByName = {},
    roomAccessCodesByName = {},
    onJoinLive,
    onOpenSettings,
    onCreateMeeting,
    onJoinRoom,
    onCopyRoomLink,
    onJoinByCode,
    meetingActionError = null,
    onClearMeetingActionError,
    onQuit,
    favoriteRooms = [],
    onToggleFavoriteRoom,
    onRemoveRoom,
    frameless = false
  }: Props = $props();

  // Single smart create/join field (#169/#171): empty creates a random
  // credential; typed names create named credentials; credentials/links join.
  let meetingInput = $state('');
  let joinError = $state<string | null>(null);
  let meetingSubmitting = $state(false);
  let profileOpen = $state(false);
  let profileTriggerEl = $state<HTMLButtonElement>();
  let profileMenuEl = $state<HTMLDivElement>();
  const visibleJoinError = $derived(joinError ?? meetingActionError);

  // UserDispatch feedback modal (#292) -- feature-gated on a build-time
  // public key (see $lib/feedback/config.ts); no trigger renders at all
  // when it's absent, same "compiled off by default" posture as Sentry.
  const feedbackEnabled = isFeedbackEnabled();
  let feedbackOpen = $state(false);

  $effect(() => {
    if (!profileOpen) return;
    return installDismissibleLayer({
      isOpen: () => profileOpen,
      getInsideNodes: () => [profileMenuEl, profileTriggerEl],
      getPopupNodes: () => [profileMenuEl],
      getOpener: () => profileTriggerEl,
      onDismiss: () => {
        profileOpen = false;
      }
    });
  });

  const favoriteNamesLower = $derived(new Set(favoriteRooms.map((n) => n.trim().toLowerCase())));
  const currentRoomLower = $derived(currentRoom?.trim().toLowerCase() ?? null);
  const meetingInputTrimmed = $derived(meetingInput.trim());
  const canCreateFromEmpty = $derived(Boolean(onCreateMeeting));
  const meetingInputCredential = $derived(meetingCredentialFromInviteInput(meetingInputTrimmed));
  const meetingCtaLabel = $derived(
    meetingInputCredential ? 'Join' : meetingInputTrimmed ? 'Create' : 'Create/Join'
  );
  const meetingCtaDisabled = $derived(
    meetingSubmitting || (meetingInputCredential ? !onJoinByCode : !canCreateFromEmpty)
  );

  function displayNameForRoom(room: string): string {
    // Never expose an internal room-<hex> credential when a caller has not
    // resolved a display label yet (#327).
    const fallback = room.trim();
    return (
      roomDisplayNamesByName[room]?.trim() ||
      (fallback && !normalizeMeetingCredential(fallback) ? fallback : 'Petal meeting')
    );
  }

  async function submitMeetingAction() {
    if (meetingSubmitting) return;
    joinError = null;
    onClearMeetingActionError?.();

    try {
      meetingSubmitting = true;
      const result = await submitMainMenuMeetingAction(meetingInput, {
        onCreateMeeting,
        onJoinByCode
      });
      meetingInput = result.nextInput;
      joinError = result.error;
    } finally {
      meetingSubmitting = false;
    }
  }

  function clearJoinError() {
    joinError = null;
    onClearMeetingActionError?.();
  }

  function openSettingsFromProfile() {
    profileOpen = false;
    onOpenSettings?.();
  }

  function quitFromProfile() {
    profileOpen = false;
    onQuit?.();
  }

  // The top bar is covered by an absolute `data-tauri-drag-region` layer, and a
  // native drag session started on mousedown swallows the following click.
  // Stop mousedown at each dot -- same defence as RemoteWindowHeader.svelte.
  function stopMouseDown(event: MouseEvent) {
    event.stopPropagation();
  }

  function mainWindowControlDeps() {
    const win = getCurrentWindow();
    return { hide: () => win.hide(), minimize: () => win.minimize() };
  }

  function onWindowHide() {
    void hideMainWindow(mainWindowControlDeps());
  }

  function onWindowMinimize() {
    void minimizeMainWindow(mainWindowControlDeps());
  }
</script>

<div class="main-menu" class:frameless>
  <div class="top-bar">
    <div class="topbar-drag-layer" data-tauri-drag-region aria-hidden="true"></div>
    <!-- macOS only. Windows has no Reopen handler, no "Open Petal" popover row
         and no tray icon, and hide() removes the taskbar button -- a second
         launch still recovers, but there is no discoverable way back. Re-enable
         deliberately once Windows has one (CLAUDE.md: Windows is in progress). -->
    {#if isMac()}
    <div class="window-controls" role="group" aria-label="Window controls">
      <button
        type="button"
        class="window-dot window-dot-hide"
        aria-label="Hide Petal window"
        onclick={onWindowHide}
        onmousedown={stopMouseDown}
      ></button>
      <button
        type="button"
        class="window-dot window-dot-minimize"
        aria-label="Minimize Petal window"
        onclick={onWindowMinimize}
        onmousedown={stopMouseDown}
      ></button>
    </div>
    {/if}
    <div class="brand-cluster" data-tauri-drag-region>
      <Logo size={13} />
      <Wordmark size={16} />
    </div>
    <div class="spacer" data-tauri-drag-region></div>
    {#if feedbackEnabled}
      <button
        type="button"
        class="profile-button feedback-button"
        aria-label="Send feedback"
        onclick={() => (feedbackOpen = true)}
      >
        <svg
          width="15"
          height="15"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2.1"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <path
            d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"
          ></path>
        </svg>
      </button>
    {/if}
    <div class="profile-menu-wrap">
      <button
        type="button"
        bind:this={profileTriggerEl}
        class="profile-button"
        aria-label="User profile"
        aria-haspopup={onOpenSettings || onQuit ? 'menu' : undefined}
        aria-expanded={onOpenSettings || onQuit ? profileOpen : undefined}
        onclick={() => {
          if (onOpenSettings || onQuit) profileOpen = !profileOpen;
        }}
      >
        <Avatar name={userName} identity={userIdentity} size={24} />
      </button>

      {#if profileOpen && (onOpenSettings || onQuit)}
        <div bind:this={profileMenuEl} class="profile-menu" role="menu" aria-label="User profile menu">
          <div class="profile-summary">
            <Avatar name={userName} identity={userIdentity} size={30} />
            <span>{userName}</span>
          </div>
          {#if onOpenSettings}
            <MenuItem label="Settings" icon="settings" role="menuitem" onclick={openSettingsFromProfile} />
          {/if}
          {#if onQuit}
            <MenuItem label="Quit Petal" icon="quit" tone="danger" role="menuitem" onclick={quitFromProfile} />
          {/if}
        </div>
      {/if}
    </div>
  </div>

  {#if liveRoom}
    <LiveHero
      roomName={displayNameForRoom(liveRoom.name)}
      participants={liveRoom.participants}
      onJoin={onJoinLive}
    />
  {:else}
    <section class="hero-quiet">
      <div class="quiet-bloom" aria-hidden="true"></div>
      <span class="quiet-eyebrow"><span class="quiet-ring" aria-hidden="true"></span>Ready to collaborate?</span>
      <span class="quiet-title">Start a meeting</span>
    </section>
  {/if}

  <div class="body">
    {#if onCreateMeeting || onJoinByCode}
      <div class="meeting-actions">
        <form
          class="meeting-form"
          onsubmit={(e) => {
            e.preventDefault();
            submitMeetingAction();
          }}
        >
          <input
            id="meeting-code"
            class="join-input"
            type="text"
            name="meeting-code"
            autocapitalize="off"
            autocomplete="off"
            placeholder="Enter meeting name or Petal invite"
            aria-label="Meeting name, invite link, or meeting code"
            bind:value={meetingInput}
            oninput={clearJoinError}
          />
          <!-- Custom create/join CTA (mock pt-btn): green when the menu is quiet
               (primary action), light when a room is live (the hero owns Join).
               type="button" so it doesn't submit; Enter in the input submits. -->
          <button
            type="button"
            class="create-btn"
            class:create-btn--green={!liveRoom}
            class:create-btn--light={!!liveRoom}
            onclick={submitMeetingAction}
            disabled={meetingCtaDisabled}
          >
            {meetingCtaLabel}
          </button>
        </form>
        {#if visibleJoinError}
          <p class="join-error" role="alert">{visibleJoinError}</p>
        {/if}
      </div>
    {/if}

    {#if emptyRooms.length}
      <p class="room-list-label">YOUR ROOMS</p>
    {/if}
    <div class="room-list-scroll">
      <div class="room-list">
        {#each emptyRooms as room (room)}
          {@const rowIsCurrent = currentRoomLower === room.trim().toLowerCase()}
          <RoomRow
            name={displayNameForRoom(room)}
            accessCode={roomAccessCodesByName[room] ?? null}
            participants={roomParticipantsByName[room] ?? []}
            occupancy={roomOccupancyByName[room] ?? null}
            current={rowIsCurrent}
            favorite={favoriteNamesLower.has(room.trim().toLowerCase())}
            onJoin={() => onJoinRoom?.(room)}
            onCopyInvite={onCopyRoomLink ? () => onCopyRoomLink(room) : undefined}
            onToggleFavorite={onToggleFavoriteRoom ? () => onToggleFavoriteRoom(room) : undefined}
            onRemove={onRemoveRoom && !rowIsCurrent ? () => onRemoveRoom(room) : undefined}
          />
        {/each}
      </div>
    </div>
  </div>
</div>

{#if feedbackEnabled && feedbackOpen}
  <FeedbackModal onClose={() => (feedbackOpen = false)} />
{/if}

<style>
  .main-menu {
    display: flex;
    flex-direction: column;
    width: 380px;
    border-radius: var(--radius-menu);
    overflow: hidden;
    overscroll-behavior: none;
    background: var(--menu-shell);
    border: 1px solid var(--fill-strong);
    box-shadow: var(--shadow-menu);
  }

  /* Frameless: the menu IS the window — no card frame, fills the route. */
  .main-menu.frameless {
    width: 100%;
    flex: 1;
    min-height: 0;
    border-radius: 0;
    border: none;
    box-shadow: none;
  }

  .top-bar {
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    height: 52px;
    padding: 6px 16px;
    box-sizing: border-box;
    flex-shrink: 0;
  }

  .hero-quiet {
    position: relative;
    height: 122px;
    padding: 20px 22px;
    display: flex;
    flex-direction: column;
    justify-content: center;
    overflow: hidden;
    flex-shrink: 0;
    background: var(--hero-gradient);
  }

  .quiet-bloom {
    position: absolute;
    inset: 0;
    pointer-events: none;
    background: radial-gradient(58% 80% at 82% 24%, rgba(52, 199, 89, 0.1), transparent 70%);
  }

  .quiet-eyebrow,
  .quiet-title {
    position: relative;
  }

  .quiet-eyebrow {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: 500 10.5px var(--font-mono);
    letter-spacing: 0.06em;
    color: var(--text-faint);
    margin-bottom: 8px;
  }

  .quiet-ring {
    width: 7px;
    height: 7px;
    border-radius: var(--radius-pill);
    border: 1.5px solid var(--text-faint);
    box-sizing: border-box;
    flex-shrink: 0;
  }

  .quiet-title {
    font: 600 22px var(--font-ui);
    color: var(--text-primary);
    letter-spacing: -0.01em;
    text-wrap: balance;
  }

  .topbar-drag-layer {
    position: absolute;
    inset: 0;
    z-index: 0;
  }

  /* Own cluster with pointer-events: auto above .topbar-drag-layer (z-index 0).
     Dots inside .brand-cluster would be unclickable: it is pointer-events: none. */
  .window-controls {
    position: relative;
    z-index: 2;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    flex: 0 0 auto;
  }

  .window-dot {
    width: 12px;
    height: 12px;
    flex: 0 0 12px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-pill);
    /* Traffic-dot pressed inset — kept literal (uiConsistency allowlist). */
    box-shadow: inset 0 0 0 0.5px rgba(0, 0, 0, 0.25);
    cursor: pointer;
    pointer-events: auto;
    transition-property: opacity, filter, scale;
    transition-duration: var(--motion-fast);
    transition-timing-function: var(--ease-standard);
  }

  .window-dot-hide {
    background: #ff5f57;
  }

  .window-dot-minimize {
    background: #febc2e;
  }

  .window-dot:active {
    scale: 0.96;
  }

  .window-dot:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .brand-cluster {
    position: relative;
    z-index: 1;
    display: inline-flex;
    align-items: center;
    gap: 10px;
    height: 32px;
    min-width: 0;
    pointer-events: none;
  }

  .spacer {
    position: relative;
    z-index: 1;
    flex: 1;
    align-self: stretch;
    min-width: 12px;
  }

  .profile-menu-wrap {
    position: relative;
    z-index: 3;
    pointer-events: none;
  }

  .profile-button {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-pill);
    border: none;
    background: transparent;
    cursor: pointer;
    flex-shrink: 0;
    padding: 0;
    pointer-events: auto;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .profile-button::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .profile-button:hover,
  .profile-button[aria-expanded='true'] {
    background: var(--fill-strong);
  }

  .profile-button:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .profile-button:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .profile-menu {
    position: absolute;
    z-index: 2;
    top: calc(100% + 8px);
    right: 0;
    width: 178px;
    display: flex;
    flex-direction: column;
    padding: 6px;
    border-radius: var(--radius-popover);
    background: var(--popover-bg);
    box-shadow: var(--shadow-float), var(--shadow-inset-hairline);
    overscroll-behavior: none;
    pointer-events: auto;
    transform-origin: top right;
    animation: profile-menu-in var(--motion-enter) var(--ease-standard) both;
  }

  @keyframes profile-menu-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }

  .profile-summary {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 8px 9px;
    border-bottom: 1px solid var(--hairline);
    margin-bottom: 4px;
    color: var(--text-strong);
    font: 600 12px var(--font-ui);
    min-width: 0;
  }

  .profile-summary span {
    min-width: 0;
    overflow-wrap: anywhere;
    line-height: 1.25;
  }

  .body {
    flex: 1;
    min-height: 0;
    overflow: hidden;
    padding: 16px;
    display: flex;
    flex-direction: column;
  }

  .meeting-actions {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-bottom: 14px;
  }

  .meeting-form {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 8px;
    align-items: center;
  }

  .join-input {
    flex: 1;
    min-width: 0;
    height: 44px;
    padding: 0 12px;
    border-radius: var(--radius-input);
    background: var(--fill-weak);
    border: 1px solid var(--fill-strong);
    color: var(--text-primary);
    font-family: var(--font-ui);
    /* 14px: the shortened placeholder ("Enter meeting name or Petal invite",
       ~189px in Albert Sans) fits the ~238px input area at the 400px window
       width with comfortable margin; text must never truncate (CLAUDE.md). */
    font-size: 14px;
    box-sizing: border-box;
    transition: border-color var(--motion-fast) var(--ease-standard);
  }

  .join-input::placeholder {
    color: var(--text-faint);
  }

  .join-input:focus {
    outline: none;
    /* Focus emphasis border — kept literal (uiConsistency allowlist). */
    border-color: rgba(255, 255, 255, 0.28);
  }

  .join-error {
    margin: -4px 0 0;
    font-size: 12px;
    color: var(--danger);
    text-wrap: pretty;
  }

  .create-btn {
    height: 44px;
    /* Compact so the input keeps enough width for the full placeholder (#171,
       CLAUDE.md "text never truncates"). */
    padding: 0 12px;
    border: 0;
    border-radius: var(--radius-control);
    cursor: pointer;
    font: 600 12px var(--font-ui);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    white-space: nowrap;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .create-btn--green {
    background: var(--cta-live-bg);
    color: var(--cta-live-ink);
    box-shadow: var(--cta-live-shadow);
  }

  @media (prefers-reduced-motion: reduce) {
    .profile-menu {
      animation: none;
    }
  }

  .create-btn--light {
    /* Near-white fill — no token carries a white surface; kept literal (uiConsistency allowlist). */
    background: rgba(255, 255, 255, 0.94);
    color: var(--menu-shell);
  }

  .create-btn:active:not(:disabled) {
    transform: scale(var(--press-scale, 0.96));
  }

  .create-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .create-btn:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .room-list-label {
    margin: 4px 0 8px;
    font: 500 10px var(--font-mono);
    letter-spacing: 0.1em;
    color: var(--text-faint);
  }

  .room-list-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }

  .room-list {
    display: flex;
    flex-direction: column;
  }

</style>
