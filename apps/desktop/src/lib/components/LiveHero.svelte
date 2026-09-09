<!--
  LiveHero — the stacked hero on the main menu for a room that is currently
  live. Redesign 2026-07-03 (petal-main-menu.html mock, "State A — a room is
  live"): a green "live" band. The room name IS the headline; a small presence
  face-stack + a green "Join now" CTA sit under it. Values are taken verbatim
  from the mock's `.pt-hero--live` / `.pt-face` / `.pt-btn--green`.

  Mock constants (inline hero-only, not promoted to tokens.css):
  - Panel: 152px tall, padding 20px 22px, bg linear-gradient(165deg,#123021,#0b140e 78%).
  - Bloom: radial-gradient(58% 80% at 82% 24%, rgba(52,199,89,.30), transparent 68%).
  - Eyebrow "● LIVE NOW": JetBrains Mono 500 10.5px, letter-spacing .06em, #5fe084.
  - Title (room name): 600 23px, #fff, letter-spacing -.01em.
  - Face: 28px circle, bg #274031, ring 0 0 0 2px #0e1a12, initials #9fe6b4 600 10.5px, overlap -9px.
  - Join CTA (pt-btn--green): 38px tall, padding 0 22px, radius 12px, 600 13.5px,
    bg #34C759, ink #06280f, shadow 0 6px 22px -6px rgba(52,199,89,.6).
-->
<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { IdentityColor } from './Avatar.svelte';

  interface RoomParticipant {
    name: string;
    identity: IdentityColor;
    resolvedColor?: string;
  }

  interface Props {
    roomName: string;
    participants?: RoomParticipant[];
    /** Canonical letter code (#123): shown under the name, never on hover. */
    accessCode?: string | null;
    onJoin?: () => void;
    onCopyInvite?: () => boolean | void | Promise<boolean | void>;
  }

  let { roomName, participants = [], accessCode = null, onJoin, onCopyInvite }: Props = $props();

  const visibleParticipants = $derived(participants.slice(0, 4));
  const overflowCount = $derived(Math.max(0, participants.length - visibleParticipants.length));
  const participantSummary = $derived(
    participants.length === 0
      ? 'No participants listed'
      : `${participants.map((p) => p.name).join(', ')} in this room`
  );

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

  function initials(name: string): string {
    const parts = name.trim().split(/\s+/).filter(Boolean);
    if (parts.length === 0) return '?';
    if (parts.length === 1) return parts[0]!.slice(0, 2).toUpperCase();
    return (parts[0]![0]! + parts[parts.length - 1]![0]!).toUpperCase();
  }
</script>

<section class="hero hero--live">
  <div class="bloom" aria-hidden="true"></div>
  <span class="eyebrow"><span class="dot" aria-hidden="true"></span>LIVE NOW</span>
  <span class="title" title={roomName}>{roomName}</span>
  {#if accessCode}
    <div class="code-line">
      {#if onCopyInvite}
        <button
          type="button"
          class="room-access-code"
          class:copied={copiedAccessCode}
          aria-label={copiedAccessCode ? `Invite link copied for ${roomName}, room ID ${accessCode}` : `Room ID ${accessCode}, click to copy invite`}
          data-testid="hero-access-code"
          onclick={copyAccessCode}
        >{accessCode}</button>
      {:else}
        <span class="room-access-code" aria-label={`Room ID ${accessCode}`} data-testid="hero-access-code">{accessCode}</span>
      {/if}
      {#if copiedAccessCode}
        <span class="room-access-code-status" role="status" aria-live="polite">Copied</span>
      {/if}
    </div>
  {/if}
  <div class="actions">
    {#if participants.length > 0}
      <div class="faces" role="img" aria-label={participantSummary}>
        {#each visibleParticipants as participant, i (`${participant.name}-${i}`)}
          <span class="face">{initials(participant.name)}</span>
        {/each}
        {#if overflowCount > 0}
          <span class="face face--overflow">+{overflowCount}</span>
        {/if}
      </div>
    {/if}
    <button type="button" class="join-cta" onclick={onJoin}>Join now</button>
  </div>
</section>

<style>
  .hero {
    position: relative;
    /* #123: min-height, not height -- the hero now carries a room-ID line
       under the name, and a fixed height + `overflow: hidden` would clip
       user-facing text (a wrapped long room name already risked it). */
    min-height: 152px;
    padding: 20px 22px;
    display: flex;
    flex-direction: column;
    justify-content: center;
    overflow: hidden;
  }

  .hero--live {
    background: var(--hero-gradient-live);
  }

  .bloom {
    position: absolute;
    inset: 0;
    pointer-events: none;
    background: var(--hero-bloom-live);
  }

  .eyebrow,
  .title,
  .code-line,
  .actions {
    position: relative;
  }

  .eyebrow {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: 500 10.5px var(--font-mono);
    letter-spacing: 0.06em;
    color: var(--live-soft);
    margin-bottom: 8px;
  }

  .dot {
    width: 7px;
    height: 7px;
    border-radius: var(--radius-pill);
    background: var(--live-soft);
    flex-shrink: 0;
  }

  .title {
    display: block;
    font: 600 23px / 1.15 var(--font-ui);
    color: var(--text-primary);
    letter-spacing: -0.01em;
    font-variant-numeric: tabular-nums;
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  /* #123: the hero's room ID is always visible and never ellipsized. */
  .code-line {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 6px;
    min-width: 0;
  }

  .room-access-code {
    display: inline-flex;
    align-items: center;
    border: 0;
    padding: 0;
    background: transparent;
    text-align: left;
    width: max-content;
    max-width: 100%;
    color: var(--live-soft);
    font: 600 10.5px var(--font-mono);
    letter-spacing: 0.02em;
    white-space: nowrap;
    flex-shrink: 0;
    overflow: visible;
    cursor: default;
  }

  button.room-access-code {
    cursor: pointer;
  }

  .room-access-code:hover,
  .room-access-code.copied {
    color: var(--live-bright);
  }

  .room-access-code:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .room-access-code-status {
    color: var(--live-bright);
    font: 700 10px var(--font-display);
    white-space: nowrap;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 14px;
    min-width: 0;
  }

  .faces {
    display: flex;
    align-items: center;
    min-width: 0;
  }

  .face {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    box-sizing: border-box;
    border-radius: var(--radius-pill);
    background: var(--live-face-bg);
    box-shadow: 0 0 0 2px var(--live-face-ring);
    color: var(--live-face-ink);
    font: 600 10.5px var(--font-ui);
    font-variant-numeric: tabular-nums;
  }

  .face + .face {
    margin-left: -9px;
  }

  .face--overflow {
    min-width: 28px;
    padding: 0 7px;
    width: auto;
  }

  .join-cta {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    height: 38px;
    padding: 0 22px;
    border: 0;
    border-radius: var(--radius-control);
    background: var(--cta-live-bg);
    color: var(--cta-live-ink);
    font: 600 13.5px var(--font-ui);
    cursor: pointer;
    box-shadow: var(--cta-live-shadow);
    transition:
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .join-cta:hover {
    opacity: 0.92;
  }

  .join-cta:active {
    opacity: 0.85;
    transform: scale(var(--press-scale, 0.96));
  }

  .join-cta:focus-visible {
    outline: 2px solid var(--live-soft);
    outline-offset: 2px;
  }
</style>
