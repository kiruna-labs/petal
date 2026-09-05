<!--
  Button — simple text/label button primitive, for the two non-circular
  primary actions on the main menu ("+ New room", "Quick meeting") and the
  onboarding footer ("Create a room", "Join {room} — N there now",
  "Open System Settings", "Relaunch now"). Distinct from ControlButton, which
  is specifically the 44px circular icon control (Petal-Build-Map.md §2.1) —
  this is the flat rectangular/pill label button seen in canvas.html's
  `.btn-primary` / `.btn-ghost` classes (main menu §1, onboarding §8).

  Button label type is 13.5px/700 per Petal-Build-Map.md §1/§2.5 ("button
  type corrected to 13.5px/700") — tokens.css already has --text-label /
  --weight-btn for exactly this, reused here rather than a second copy.

  Variants, matching canvas.html:
  - `primary` — filled, `rgba(255,255,255,.08)` background + hairline border
    (the "+ New room" / "Quick meeting" / "Open System Settings" style).
    Kept graphite per the color-rationing rule — canvas.html's plum "Join
    now" button only appears *inside* LiveHero (which already owns the one
    sanctioned bloom of real color for that surface), not as a general
    Button variant, so it is NOT reproduced here as a generic colored
    variant — LiveHero draws its own CTA to keep that color use scoped.
  - `ghost` — transparent / no border, secondary weight (the "Join eng-sync
    — 3 there now" row in the onboarding "Ready" state, `.btn-ghost`).
-->
<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    variant?: 'primary' | 'ghost';
    disabled?: boolean;
    fullWidth?: boolean;
    /** Submit support: a Button inside a <form> with type="submit" makes the
     * form's onsubmit fire (FeedbackModal's Send). Defaults to "button" so
     * plain action buttons never trigger a surrounding form. */
    type?: 'button' | 'submit';
    onclick?: () => void;
    children?: Snippet;
  }

  let {
    variant = 'primary',
    disabled = false,
    fullWidth = false,
    type = 'button',
    onclick,
    children
  }: Props = $props();
</script>

<button
  {type}
  class="btn"
  class:primary={variant === 'primary'}
  class:ghost={variant === 'ghost'}
  class:full-width={fullWidth}
  {disabled}
  {onclick}
>
  {@render children?.()}
</button>

<style>
  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    height: 42px;
    padding: 0 16px;
    border-radius: var(--radius-control);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-btn);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
    box-sizing: border-box;
  }

  .btn.full-width {
    width: 100%;
  }

  .btn.primary {
    background: var(--fill-strong);
    border: 1px solid var(--hairline-strong);
    color: var(--text-primary);
  }

  .btn.primary:hover:not(:disabled) {
    background: var(--fill-bright);
  }

  .btn.ghost {
    background: transparent;
    border: none;
    color: var(--text-soft);
  }

  .btn.ghost:hover:not(:disabled) {
    color: var(--text-primary);
  }

  .btn:active:not(:disabled) {
    opacity: 0.85;
    transform: scale(var(--press-scale, 0.96));
  }

  .btn:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
    pointer-events: none;
  }

  .btn:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }
</style>
