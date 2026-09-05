<!--
  RoomRow — a single room entry in the main-menu list, below the hero. Per
  Petal-Build-Map.md §2.5: an empty room just shows the name, neutral; a
  live room (people currently in it) shows identity-colored avatar stack +
  "N people are talking" + "Join now" — but note the *headline* live room
  (e.g. eng-sync) is promoted into LiveHero instead of appearing as a row
  here, per canvas.html §7 which only ever shows ONE hero + plain empty rows
  below it (`design-review`, `standup`, both "empty"). This component still
  supports a live variant so a *second* concurrently-live room (not the
  hero) has a real presentational state to render, rather than silently
  reusing the empty look for something that isn't actually empty.

  Empty rows intentionally avoid negative labels like "empty" / "unknown";
  the explicit affordance is the positive action: Join.

  Live-row look is NOT separately shown in canvas.html for a *row* (only for
  the hero) — approximated here by reusing the hero's identity-avatar-stack
  treatment (canvas.html §8c "Ready" state's overlapping avatar row) at row
  scale, plus the live-green tokens (--live / --live-tint) for the "N people
  are talking" caption color, since that's the established "live" semantic
  color elsewhere in this system (ControlButton's active-share state,
  Gallery). Flagged as an approximation, not an invented new color.
-->
<script lang="ts">
  import { onDestroy } from 'svelte';
  import Avatar from './Avatar.svelte';
  import type { IdentityColor } from './Avatar.svelte';

  interface RoomParticipant {
    name: string;
    identity: IdentityColor;
    resolvedColor?: string;
  }

  interface Props {
    name: string;
    subtitle?: string;
    /** Empty room: no one present. Live room: participants currently in it. */
    participants?: RoomParticipant[];
    /**
     * Server-side headcount from `POST /api/rooms/status` for a room this
     * machine holds a credential for. Identities are not included (only
     * joined rooms have a roster), so a non-zero count with no participants
     * renders as a count, not avatars.
     */
    occupancy?: number | null;
    /** This process is currently joined to this room. */
    current?: boolean;
    favorite?: boolean;
    onJoin?: () => void;
    /** Canonical letter code; technical room credentials are never accepted. */
    accessCode?: string | null;
    onToggleFavorite?: () => void;
    onCopyInvite?: () => boolean | void | Promise<boolean | void>;
    onRemove?: () => void;
  }

  let {
    name,
    subtitle,
    participants = [],
    occupancy = null,
    current = false,
    accessCode = null,
    favorite = false,
    onJoin,
    onToggleFavorite,
    onCopyInvite,
    onRemove
  }: Props = $props();

  // A headcount without a roster (status lookup for a room we're not in).
  const headcount = $derived(participants.length === 0 && !current && (occupancy ?? 0) > 0 ? occupancy! : 0);
  const isLive = $derived(participants.length > 0 || current || headcount > 0);
  const headline = $derived(
    current
      ? 'In meeting'
      : headcount > 0
        ? headcount === 1
          ? '1 person in the room'
          : `${headcount} people in the room`
        : participants.length === 1
          ? '1 person is talking'
          : `${participants.length} people are talking`
  );
  const joinLabel = $derived(current ? 'Return' : 'Join now');
  const joinButtonLabel = $derived(current ? 'Return' : 'Join');

  function targetIsControl(event: Event): boolean {
    const target = event.target;
    return target instanceof HTMLElement && Boolean(target.closest('button'));
  }

  function joinFromRow(event: MouseEvent) {
    if (targetIsControl(event)) return;
    onJoin?.();
  }

  function joinFromKeyboard(event: KeyboardEvent) {
    if (targetIsControl(event)) return;
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    onJoin?.();
  }

  function joinFromButton(event: MouseEvent) {
    event.stopPropagation();
    onJoin?.();
  }

  function toggleFavorite(event: MouseEvent) {
    event.stopPropagation();
    onToggleFavorite?.();
  }

  let copiedAccessCode = $state(false);
  let accessCodeCopyTimer: ReturnType<typeof setTimeout> | undefined;
  async function copyAccessCode(event: MouseEvent) {
    event.stopPropagation();
    clearTimeout(accessCodeCopyTimer);
    const copied = await onCopyInvite?.();
    if (copied === false) return;
    copiedAccessCode = true;
    accessCodeCopyTimer = setTimeout(() => {
      copiedAccessCode = false;
    }, 1400);
  }

  onDestroy(() => {
    clearTimeout(accessCodeCopyTimer);
  });

  function removeRoom(event: MouseEvent) {
    event.stopPropagation();
    onRemove?.();
  }
</script>

{#snippet rowContents()}
  {#if isLive}
    <div class="room-row live">
      {#if participants.length}
        <div class="avatar-stack" aria-hidden="true">
          {#each participants.slice(0, 4) as p (p.name)}
            <div class="stack-item"><Avatar name={p.name} identity={p.identity} resolvedColor={p.resolvedColor} size={26} /></div>
          {/each}
        </div>
      {:else}
        <span class="current-dot" aria-hidden="true"></span>
      {/if}
      <div class="room-info">
        <span class="room-name">{name}</span>
        {#if current}
          <span class="room-status live-status">{headline}</span>
        {/if}
        {#if accessCode}
          {#if onCopyInvite}
            <button
              type="button"
              class="room-access-code"
              class:copied={copiedAccessCode}
              aria-label={copiedAccessCode ? `Invite link copied for ${name}, room ID ${accessCode}` : `Room ID ${accessCode}, click to copy invite`}
              data-testid="room-access-code"
              onclick={copyAccessCode}
            >{accessCode}</button>
          {:else}
            <span class="room-access-code" title={`Access code: ${accessCode}`} aria-label={`Access code: ${accessCode}`} data-testid="room-access-code">{accessCode}</span>
          {/if}
          {#if copiedAccessCode}
            <span class="room-access-code-status" role="status" aria-live="polite">Copied</span>
          {/if}
        {/if}
        {#if !current}
          <span class="room-status live-status">{headline}</span>
        {/if}
        {#if subtitle}
          <span class="room-status">{subtitle}</span>
        {/if}
      </div>
      <span class="join-label">{joinLabel}</span>
    </div>
  {:else}
    <div class="room-row-content">
      <span class="room-name">{name}</span>
      {#if accessCode}
        {#if onCopyInvite}
          <button
            type="button"
            class="room-access-code"
            class:copied={copiedAccessCode}
            aria-label={copiedAccessCode ? `Invite link copied for ${name}, room ID ${accessCode}` : `Room ID ${accessCode}, click to copy invite`}
            data-testid="room-access-code"
            onclick={copyAccessCode}
          >{accessCode}</button>
        {:else}
          <span class="room-access-code" title={`Access code: ${accessCode}`} aria-label={`Access code: ${accessCode}`} data-testid="room-access-code">{accessCode}</span>
        {/if}
        {#if copiedAccessCode}
          <span class="room-access-code-status" role="status" aria-live="polite">Copied</span>
        {/if}
      {/if}
      {#if subtitle}
        <span class="room-status">{subtitle}</span>
      {/if}
    </div>
    {#if onJoin}
      <button type="button" class="join-button" onclick={joinFromButton}>{joinButtonLabel}</button>
    {/if}
  {/if}

  {#if onToggleFavorite}
    <button
      type="button"
      class="favorite-button"
      class:active={favorite}
      aria-label={favorite ? `Remove ${name} from favorites` : `Favorite ${name}`}
      aria-pressed={favorite}
      onclick={toggleFavorite}
    >
      <svg width="15" height="15" viewBox="0 0 24 24" fill={favorite ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M11.48 3.5a.58.58 0 0 1 1.04 0l2.13 4.31a.58.58 0 0 0 .44.32l4.76.69a.58.58 0 0 1 .32.99l-3.44 3.36a.58.58 0 0 0-.17.51l.81 4.74a.58.58 0 0 1-.84.61l-4.26-2.24a.58.58 0 0 0-.54 0l-4.26 2.24a.58.58 0 0 1-.84-.61l.81-4.74a.58.58 0 0 0-.17-.51L3.83 9.81a.58.58 0 0 1 .32-.99l4.76-.69a.58.58 0 0 0 .44-.32z"></path>
      </svg>
    </button>
  {/if}

  {#if onRemove}
    <button
      type="button"
      class="remove-button"
      aria-label={`Remove ${name}`}
      onclick={removeRoom}
    >
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M3 6h18"></path>
        <path d="M8 6V4h8v2"></path>
        <path d="M19 6l-1 14H6L5 6"></path>
        <path d="M10 11v5M14 11v5"></path>
      </svg>
    </button>
  {/if}
{/snippet}

{#if onJoin}
  <!-- The row is a JOIN surface that also contains real controls (access-
       code copy, favorite, remove). role="button" here was wrong: a
       "button containing buttons" announced conflicting activations.
       role="group" keeps the row's mouse/keyboard join behavior while
       letting each inner control keep its own accessible name and action.
       The focusable-group pattern is deliberate (the row is the only join
       affordance for live rooms), hence the a11y-rule suppression — same
       precedent as Modal's backdrop / DevicePicker's host. -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
  <div
    class="room-row-shell clickable"
    class:live={isLive}
    role="group"
    tabindex="0"
    aria-label={current ? `Return to ${name}` : `Join ${name}`}
    onclick={joinFromRow}
    onkeydown={joinFromKeyboard}
  >
    {@render rowContents()}
  </div>
{:else}
  <div class="room-row-shell" class:live={isLive}>
    {@render rowContents()}
  </div>
{/if}

<style>
  .room-row-shell {
    display: flex;
    align-items: center;
    gap: 4px;
    border-radius: var(--radius-tile);
    transition:
      background var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .room-row-shell.clickable {
    cursor: pointer;
  }

  .room-row-shell:hover,
  .room-row-shell:has(:focus-visible) {
    background: var(--fill-base);
  }

  .room-row-shell.clickable:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .room-row-shell.clickable:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .room-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 9px 4px;
    width: 100%;
    background: transparent;
    border: none;
    text-align: left;
    border-radius: inherit;
    min-width: 0;
  }

  .room-row.live {
    gap: 10px;
    padding: 8px 4px;
  }

  .room-row-content {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    justify-content: center;
    min-width: 0;
    flex: 1;
    padding: 9px 4px 9px 10px;
  }

  .join-button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    height: 32px;
    padding: 0 12px;
    /* A button, not a tile — control radius (pre-sweep value). */
    border-radius: var(--radius-control);
    border: 1px solid var(--hairline-strong);
    background: var(--fill-base);
    color: var(--text-soft);
    font: 700 12px var(--font-display);
    cursor: pointer;
    opacity: 0;
    flex-shrink: 0;
    transition:
      opacity var(--motion-fast) var(--ease-standard),
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .room-row-shell:hover .join-button,
  .room-row-shell:has(:focus-visible) .join-button {
    opacity: 1;
  }

  .join-button:hover {
    background: var(--fill-bright);
    color: var(--text-primary);
  }

  .join-button:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .join-button:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .favorite-button,
  .remove-button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 40px;
    height: 40px;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--text-faint);
    cursor: pointer;
    opacity: 0;
    flex-shrink: 0;
    transition:
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .room-row-shell:hover .favorite-button,
  .room-row-shell:has(:focus-visible) .favorite-button,
  .favorite-button.active {
    opacity: 1;
  }

  .room-row-shell:hover .remove-button,
  .room-row-shell:has(:focus-visible) .remove-button {
    opacity: 1;
  }

  .favorite-button:hover,
  .favorite-button.active {
    color: var(--id-lilac);
  }

  .remove-button:hover {
    color: var(--danger);
  }

  .favorite-button:active,
  .remove-button:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .favorite-button:focus-visible,
  .remove-button:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .avatar-stack {
    display: flex;
    flex-shrink: 0;
  }

  .stack-item {
    margin-left: -8px;
    border-radius: var(--radius-pill);
    box-shadow: 0 0 0 2px var(--bg-base);
  }

  .stack-item:first-child {
    margin-left: 0;
  }

  .current-dot {
    width: 26px;
    height: 26px;
    border-radius: var(--radius-pill);
    flex-shrink: 0;
    background: var(--live);
    box-shadow:
      0 0 0 2px var(--bg-base),
      0 0 18px var(--live-tint);
  }

  .room-info {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }

  .room-name {
    max-width: 100%;
    font: 700 14px / 1.2 var(--font-display);
    color: var(--text-strong);
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  /* The code is a hover/focus disclosure, but never ellipsized: people must
     be able to read and verify every character before copying it. */
  .room-access-code {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: 0;
    padding: 0;
    background: transparent;
    color: inherit;
    font: inherit;
    text-align: left;
    width: max-content;
    max-width: none;
    margin-top: 3px;
    color: var(--text-dim);
    font: 600 10px var(--font-mono);
    letter-spacing: 0.02em;
    white-space: nowrap;
    flex-shrink: 0;
    overflow: visible;
    opacity: 0;
    cursor: default;
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  button.room-access-code {
    cursor: pointer;
  }

  .room-row-shell:hover .room-access-code,
  .room-row-shell:has(:focus-visible) .room-access-code {
    opacity: 1;
  }

  .room-access-code:hover,
  .room-access-code.copied {
    color: var(--live-bright);
  }

  .room-access-code-status {
    color: var(--live-bright);
    font: 700 10px var(--font-display);
    white-space: nowrap;
  }

  .room-access-code:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .room-row.live .room-name {
    color: var(--text-primary);
  }

  .room-status {
    margin-top: 2px;
    max-width: 100%;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    font: 500 10.5px / 1.25 var(--font-mono);
    color: var(--text-faint);
    flex-shrink: 0;
  }

  .live-status {
    color: var(--live-bright);
    font-variant-numeric: tabular-nums;
  }

  .join-label {
    flex-shrink: 0;
    font: 700 11px var(--font-display);
    color: var(--text-primary);
    background: var(--fill-bright);
    padding: 6px 12px;
    border-radius: var(--radius-pill);
  }

</style>
