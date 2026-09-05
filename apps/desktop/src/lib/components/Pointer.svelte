<!--
  Pointer — the telepointer overlay glyph. Per Petal-Build-Map.md §2.6 / §5
  ("no changes — carried forward") / SPEC.md §4.5: a pointer shape
  deliberately distinct from the OS arrow, paired with a NamePill in the
  owner's identity color.

  Shape: pulled VERBATIM from the approved design canvas's approved
  telepointer board (§5 "Telepointer"), NOT invented — exact path data:
    <path d="M5 3l5 16 2.5-6.5L19 10z" viewBox="0 0 24 24">
  now rendered without a permanent outline/halo. Activity states carry the
  temporary emphasis so resting remote pointers stay visually quiet (#23).

  States (Build-Map §2.6):
  - moving: full opacity, no animation.
  - idle: pointer dims (this component only reacts to the `idle` prop via
    CSS opacity — actual idle-timeout detection is later wiring, per the
    task brief).
  - click/emphasis: a quick CSS ripple, triggered by bumping the `pulseKey`
    prop (any change replays the animation) — a simple prop-driven trigger
    is sufficient for this phase; wiring a real click/mousedown event over
    the data channel is later work.
-->
<script lang="ts">
  import NamePill from './NamePill.svelte';
  import type { IdentityColor } from './Avatar.svelte';

  interface Props {
    name: string;
    identity: IdentityColor;
    /** Normalized 0-1 position within the owning surface (SPEC.md §4.5). */
    x?: number;
    y?: number;
    idle?: boolean;
    /** Bump this (e.g. increment a counter) to replay the click ripple. */
    pulseKey?: number;
    controlActive?: boolean;
    typing?: boolean;
  }

  let {
    name,
    identity,
    x = 0.5,
    y = 0.5,
    idle = false,
    pulseKey = 0,
    controlActive = false,
    typing = false
  }: Props = $props();
</script>

<div
  class="pointer-layer"
  style:left="{x * 100}%"
  style:top="{y * 100}%"
  style:--id-color="var(--id-{identity})"
>
  <div class="pointer-glyph" class:idle class:controlActive={controlActive}>
    {#key pulseKey}
      {#if pulseKey > 0}
        <span class="ripple" aria-hidden="true"></span>
      {/if}
    {/key}
    <svg class="pointer-svg" width="22" height="22" viewBox="0 0 24 24" aria-hidden="true">
      <path class="pointer-fill" d="M5 3l5 16 2.5-6.5L19 10z"></path>
    </svg>
  </div>
  <div class="pointer-label" class:idle>
    <NamePill {name} {identity} {idle} />
    {#if typing}
      <span class="typing-indicator" aria-hidden="true">
        <span></span>
        <span></span>
        <span></span>
      </span>
    {/if}
  </div>
</div>

<style>
  .pointer-layer {
    position: absolute;
    display: flex;
    align-items: flex-start;
    gap: 6px;
    /* Anchor the ARROW TIP exactly on the reported (x, y) point. The arrow
       glyph is a 24-unit viewBox drawn at 22px with its tip at (5,3), i.e.
       (5/24*22, 3/24*22) = (4.583px, 2.75px) from the layer origin. The old
       -2,-2 approximation left the tip ~2.6px RIGHT of the cursor — a
       constant, position-independent shift that was only visible because the
       sharer's own cursor is captured in the shared content. */
    transform: translate(-4.583px, -2.75px);
    pointer-events: none;
    /* No position transition here (was `left/top 60ms`). Upstream smoothing
       + the ~45Hz sender already provide the glide; a CSS transition on a
       position that updates faster than its own duration never settles, so
       it AMPLIFIES sub-pixel input jitter into a visible continuous wobble
       (017: input constant to 6dp, tag still shimmering ~0.4px). Snapping
       position directly (Pointer.svelte + the overlay's device-pixel snap)
       keeps it rock-stable for a stationary cursor and low-latency for real
       movement. */
    transition: none;
  }

  .pointer-glyph {
    position: relative;
    display: inline-flex;
    transition: opacity var(--motion-base) var(--ease-standard);
  }

  .pointer-svg {
    overflow: visible;
  }

  .pointer-fill {
    stroke-linecap: round;
    stroke-linejoin: round;
    vector-effect: non-scaling-stroke;
  }

  .pointer-fill {
    fill: var(--id-color);
    stroke: none;
  }

  .pointer-glyph.idle {
    opacity: 0.4;
  }

  .pointer-glyph.controlActive {
    filter: drop-shadow(0 0 5px color-mix(in srgb, var(--id-color) 56%, transparent));
  }

  .pointer-label {
    margin-top: 2px;
    display: flex;
    align-items: center;
    gap: 4px;
    transition: opacity var(--motion-base) var(--ease-standard);
  }

  /* Click/emphasis ripple — simple CSS animation triggered by the {#key}
     block above remounting a fresh .ripple element each time pulseKey changes. */
  .ripple {
    position: absolute;
    left: 3px;
    top: 3px;
    width: 14px;
    height: 14px;
    border-radius: var(--radius-pill);
    border: 1px solid color-mix(in srgb, var(--id-color) 72%, transparent);
    background: transparent;
    opacity: 0.36;
    animation: pointer-ripple 480ms var(--ease-standard) forwards;
  }

  .typing-indicator {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    height: 14px;
    padding: 0 5px;
    border-radius: var(--radius-pill);
    background: rgba(8, 8, 10, 0.72);
    box-shadow:
      inset 0 0 0 1px color-mix(in srgb, var(--id-color) 36%, transparent),
      0 2px 8px rgba(0, 0, 0, 0.25);
  }

  .typing-indicator span {
    width: 3px;
    height: 3px;
    border-radius: var(--radius-pill);
    background: var(--id-color);
    animation: typing-dot 780ms var(--ease-standard) infinite;
  }

  .typing-indicator span:nth-child(2) {
    animation-delay: 110ms;
  }

  .typing-indicator span:nth-child(3) {
    animation-delay: 220ms;
  }

  @keyframes pointer-ripple {
    from {
      transform: scale(0.4);
      opacity: 0.36;
    }
    to {
      transform: scale(3.2);
      opacity: 0;
    }
  }

  @keyframes typing-dot {
    0%,
    80%,
    100% {
      transform: translateY(0);
      opacity: 0.48;
    }
    35% {
      transform: translateY(-2px);
      opacity: 1;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .pointer-layer,
    .pointer-glyph,
    .pointer-label {
      transition: none;
    }

    .ripple {
      animation: none;
      opacity: 0;
    }

    .typing-indicator span {
      animation: none;
    }
  }
</style>
