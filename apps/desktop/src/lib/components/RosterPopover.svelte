<!--
  RosterPopover — the participants/roster popover. Per DESIGN.md §9
  ("the roster/participants popover" — flagged as needed, not designed)
  and Petal-Build-Map.md §4 ("Menubar popover — referenced... but not
  designed"). EXPLICITLY UNDESIGNED: no canvas.html markup exists for a
  roster/participants list (only the in-meeting gallery's live tiles and
  the main-menu's avatar stacks touch participant identity elsewhere).
  Built functional-but-plain, same standard as `RemoteWindowHeader` in the
  prior phase: a simple popover-shaped panel (Pill-adjacent surface
  treatment, not the Pill shell itself — a list needs its own scrollable
  body, which Pill's fixed-height single-row shell isn't shaped for) with
  a plain row list.

  Reuses:
  - `Avatar` with identity rings (the one place identity color is allowed
    to persist per Petal-Build-Map.md §3, same as MainMenu's stack).
  - The muted-mic glyph chip pattern from `ParticipantTile` (neutral slashed
    `ControlButton` in a non-interactive wrapper) for the mic-muted
    indicator, rather than inventing a second mic-off treatment.
  - A quiet speaking indicator matching `ParticipantTile`'s neutral
    (non-identity-colored) speaking ring philosophy — here rendered as a
    small dim pulse dot next to the name rather than a ring around a
    small avatar (a full box-shadow ring reads poorly at 28px), same
    "quiet, not colored" spirit, flagged as an adapted-not-copied pattern.

  This is also the panel conceptually referenced by the future menubar
  popover (Petal-Build-Map.md §2.3 "clicking the body opens a popover:
  full controls + roster + leave") — not literally wired to a menubar yet
  (native Swift work, out of scope here), just the shared roster content.

  Empty case (no other participants) uses the new `EmptyState` rather than
  a bespoke message, for consistency with the other secondary surfaces
  built in this same phase.
-->
<script lang="ts">
  import Avatar from './Avatar.svelte';
  import ControlButton from './ControlButton.svelte';
  import EmptyState from './EmptyState.svelte';
  import type { IdentityColor } from './Avatar.svelte';

  export interface RosterParticipant {
    name: string;
    identity: IdentityColor;
    resolvedColor?: string;
    muted?: boolean;
    speaking?: boolean;
    isYou?: boolean;
  }

  interface Props {
    roomName: string;
    participants?: RosterParticipant[];
    onInvite?: () => void;
    embedded?: boolean;
  }

  let { roomName, participants = [], onInvite, embedded = false }: Props = $props();
</script>

<div class="roster-popover" class:embedded>
  <div class="header">
    <span class="title">In {roomName}</span>
    <span class="count">{participants.length}</span>
  </div>

  <div class="list">
    {#if participants.length === 0}
      <EmptyState title="No one here yet" detail="Invite teammates to get started." />
    {:else}
      {#each participants as p (p.name)}
        <div class="row">
          <Avatar
            name={p.name}
            identity={p.identity}
            resolvedColor={p.resolvedColor}
            size={28}
            speaking={p.speaking}
          />
          <span class="name">
            {p.name}{#if p.isYou}<span class="you"> (you)</span>{/if}
          </span>
          <div class="spacer"></div>
          {#if p.muted}
            <!-- Status glyph, not a control: pointer-events suppressed AND
                 removed from the tab order so it never becomes a dead focus
                 stop per muted participant (one per row). -->
            <div class="muted-chip" title="Muted">
              <ControlButton
                icon="mic"
                kind="toggle"
                active
                size="menubar"
                tabindex={-1}
                label={`${p.name} is muted`}
              />
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  </div>

  <div class="footer">
    <!-- One real button, one accessible name. The old inner ControlButton
         nested a second <button> (invalid HTML) that was a dead tab stop —
         the glyph is now a plain span. -->
    <button type="button" class="invite" onclick={onInvite} disabled={!onInvite}>
      <span class="invite-glyph" aria-hidden="true">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="9" cy="8" r="3.5"></circle>
          <path d="M3 20a6 6 0 0 1 12 0"></path>
          <path d="M16 5.5a3.5 3.5 0 0 1 0 7"></path>
          <path d="M19 20a6 6 0 0 0-4-5.6"></path>
        </svg>
      </span>
      <span>Invite people</span>
    </button>
  </div>
</div>

<style>
  .roster-popover {
    display: flex;
    flex-direction: column;
    width: 260px;
    max-height: 360px;
    border-radius: var(--radius-card);
    background: linear-gradient(180deg, var(--surface-raised), var(--surface));
    border: 1px solid var(--hairline);
    box-shadow: var(--shadow-panel);
    overflow: hidden;
    overscroll-behavior: none;
    transform-origin: top center;
    animation: roster-popover-in var(--motion-enter) var(--ease-standard) both;
  }

  .roster-popover.embedded {
    width: 100%;
    max-height: none;
    border-radius: 0;
    background: transparent;
    border: 0;
    box-shadow: none;
    animation: none;
  }

  @keyframes roster-popover-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }

  .header {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    min-height: 42px;
    height: auto;
    padding: 10px 14px;
    border-bottom: 1px solid var(--hairline);
    flex-shrink: 0;
  }

  .title {
    font: 600 12.5px / 1.25 var(--font-ui);
    color: var(--text-primary);
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .count {
    font: 500 10.5px var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--text-faint);
    background: var(--fill-base);
    border-radius: var(--radius-chip);
    padding: 2px 7px;
  }

  .list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: none;
    padding: 6px;
  }

  .row {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 7px 8px;
    border-radius: var(--radius-tile);
  }

  .row:hover {
    background: var(--fill-weak);
  }

  .name {
    min-width: 0;
    font: 500 12.5px / 1.25 var(--font-ui);
    color: var(--text-strong);
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .you {
    color: var(--text-faint);
    font-weight: 400;
  }

  .spacer {
    flex: 1;
    min-width: 4px;
  }

  .muted-chip :global(.control-button) {
    pointer-events: none;
  }

  .footer {
    flex-shrink: 0;
    border-top: 1px solid var(--hairline);
    padding: 8px;
  }

  .invite {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    min-height: 40px;
    padding: 6px 8px;
    /* A button, not a tile — control radius (pre-sweep value). */
    border-radius: var(--radius-control);
    background: transparent;
    border: none;
    cursor: pointer;
    text-align: left;
    color: var(--text-strong);
    font: 500 12.5px var(--font-ui);
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      scale var(--motion-fast) var(--ease-standard);
  }

  .invite:disabled {
    cursor: default;
    opacity: 0.55;
  }

  .invite:disabled:hover {
    background: transparent;
  }

  /* The invite glyph as a plain span (the old nested ControlButton was a
     button-in-button): same compact-circle register. */
  .invite-glyph {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    flex-shrink: 0;
    border-radius: var(--radius-pill);
    background: var(--fill-strong);
    color: var(--text-strong);
  }

  .invite:hover {
    background: var(--fill-base);
  }

  .invite:active {
    scale: var(--press-scale, 0.96);
  }

  @media (prefers-reduced-motion: reduce) {
    .roster-popover {
      animation: none;
    }

    .invite {
      transition: none;
    }

    .invite:active {
      scale: 1;
    }
  }
</style>
