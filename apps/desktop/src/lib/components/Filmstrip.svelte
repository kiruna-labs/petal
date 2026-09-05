<!--
  Filmstrip — the compact companion to the full Gallery, per SPEC.md §4.7:
  "Faces are secondary furniture; shared windows are the content. Default
  layout: a slim filmstrip of camera tiles (top or side); shared windows live
  as free-floating native windows on the desktop." This is that slim strip —
  NOT a replacement for Gallery (which stays the full-screen/expanded view,
  e.g. reached via DensityToggle or a future "expand" affordance); Filmstrip
  is the small, floating-over-your-real-work chrome per SPEC.md's "faces are
  secondary" framing.

  No filmstrip/compact-camera-strip component existed anywhere in the repo
  before this (checked `apps/desktop/src/lib/components/` and both dev-route
  harnesses) -- this is a new build, not a rename/extension of something a
  frontend-only agent already shipped.

  Reuses ParticipantTile as-is (same tile visuals/states as the full
  Gallery -- video/camera-off/speaking/muted/weak-connection), just at a
  much smaller fixed tile size and in a `row` (top strip) or `column` (side
  strip) layout instead of Gallery's reflowing grid. No control bar here --
  SPEC.md §4.7 keeps the filmstrip itself minimal ("small floating control
  bar" is described as a separate, even smaller companion chrome piece, not
  folded into the filmstrip).

  Undesigned surface (no filmstrip mock exists in canvas.html/DESIGN.md, same
  as RemoteWindowHeader/Settings/RosterPopover in earlier phases) -- built
  functional-but-plain per the bundle's established default for undesigned
  pieces: quiet graphite chrome, no new color introduced, small drop shadow
  to read as "floating over the desktop" per SPEC.md's framing.
-->
<script lang="ts">
  import { fade } from 'svelte/transition';
  import ParticipantTile from './ParticipantTile.svelte';
  import { tileTransitionDuration } from '$lib/motion';

  export interface FilmstripParticipant {
    /** Stable identity key (the room identity). Two participants can share
     * a display name (e.g. two "Guest"s) — keying by name alone collided as
     * Svelte keys and mis-reused slots on order shifts. */
    id?: string;
    name: string;
    videoOn?: boolean;
    /** Live MediaStream for this tile (local self-view). */
    videoStream?: MediaStream;
    /** Mirror the video (local self-view convention only). */
    mirrored?: boolean;
    speaking?: boolean;
    muted?: boolean;
    weakConnection?: boolean;
    /** #875: not surfaced in the filmstrip's own layout -- the tile still
     * receives these so ParticipantTile's markup is consistent, but the
     * pill is hidden entirely at this tile size (see `.slot
     * :global(.share-count-pill)` below). */
    shareCount?: number;
    sharingLiveBackground?: string;
    sharingLiveColor?: string;
    isLocal?: boolean;
  }

  interface Props {
    participants?: FilmstripParticipant[];
    /** "row" = top/bottom strip (default), "column" = side strip. */
    orientation?: 'row' | 'column';
  }

  let { participants = [], orientation = 'row' }: Props = $props();
</script>

<div class="filmstrip" class:column={orientation === 'column'}>
  {#each participants as p (p.id ?? p.name)}
    <!-- Participant join/leave animate in/out (DESIGN.md §6) — same
         transition + duration source as Gallery's tile grid, so the two
         in-meeting surfaces feel consistent. -->
    <div
      class="slot"
      in:fade={{ duration: tileTransitionDuration() }}
      out:fade={{ duration: tileTransitionDuration() }}
    >
      <ParticipantTile
        name={p.name}
        videoOn={p.videoOn ?? true}
        videoStream={p.videoStream}
        mirrored={p.mirrored ?? false}
        speaking={p.speaking ?? false}
        muted={p.muted ?? false}
        weakConnection={p.weakConnection ?? false}
        shareCount={p.shareCount ?? 0}
        sharingLiveBackground={p.sharingLiveBackground}
        sharingLiveColor={p.sharingLiveColor}
        isLocal={p.isLocal ?? false}
      />
    </div>
  {/each}
</div>

<style>
  /* Slim by construction: fixed small tile size + a hard max on the cross
     axis (height for a row strip, width for a column strip) rather than
     letting tiles grow -- this is meant to read as thin chrome floating
     over the user's real work (shared windows), never as a second gallery. */
  .filmstrip {
    display: flex;
    flex-direction: row;
    gap: 8px;
    padding: 8px;
    max-height: 96px;
    overflow-x: auto;
    overflow-y: hidden;
    border-radius: var(--radius-control);
    background: var(--glass-filmstrip);
    backdrop-filter: blur(14px);
    border: 1px solid var(--hairline-strong);
    box-shadow:
      var(--shadow-float),
      0 0 0 1px var(--fill-weak);
  }

  .filmstrip.column {
    flex-direction: column;
    max-height: none;
    max-width: 112px;
    overflow-x: hidden;
    overflow-y: auto;
  }

  .slot {
    position: relative;
    flex-shrink: 0;
    width: 128px;
    height: 80px;
  }

  .filmstrip.column .slot {
    width: 96px;
    height: 64px;
  }

  /* Smaller tiles need a smaller name chip / mute glyph than the full
     Gallery's -- ParticipantTile's own styles are written in absolute px,
     so scale the ones that would otherwise overwhelm a tile this size. */
  .slot :global(.name-chip) {
    left: 6px;
    bottom: 6px;
    padding: 3px 6px;
    font-size: 9.5px;
    max-width: calc(100% - 14px);
  }

  .slot :global(.muted-chip) {
    right: 6px;
    bottom: 6px;
    transform: scale(0.7);
    transform-origin: bottom right;
  }

  .slot :global(.weak-dot) {
    right: 6px;
    bottom: 8px;
  }

  /* #875: hide entirely at filmstrip size (96x64 column / 128x80 row tiles)
     -- same "drop, never shrink past legibility" rule as Gallery's `tiny`
     breakpoint; there's no room for a legible pill at this size. */
  .slot :global(.share-count-pill) {
    display: none;
  }
</style>
