<!--
  IdentitySetup — name input + identity color picker, the second step of the
  single-view Onboarding checklist (Petal-Build-Map.md §2.7). Reuses the
  exact 6-color identity palette Avatar/Pointer/NamePill already draw from
  (tokens.css --id-*) — no second copy of these hex values.

  Layout pulled from canvas.html §8c "Ready" state's identity card:
  card shell `border-radius:14px`, `background:rgba(255,255,255,.05)`,
  `border:1px solid rgba(255,255,255,.1)`, `16px` padding; avatar (44px,
  graphite gradient `linear-gradient(160deg,#2a2c30,#1a1c1f)`) with a
  slate-color ring (`0 0 0 2px #8FA6B8` in the comp — the currently-selected
  identity color's ring, shown here as reactive to the actual selection
  rather than hardcoded to slate); name field (36px, rounded 10px,
  `rgba(255,255,255,.06)` bg + hairline border); swatch row of six 26px
  circles, selected swatch getting a double ring
  (`0 0 0 2px var(--surface-2), 0 0 0 3px {color}` in the comp, e.g.
  `0 0 0 2px #17181c,0 0 0 3px #8FA6B8` for the selected slate swatch).

  Note: canvas.html's swatch row literally renders five extra one-off colors
  (#9C8FB8, #B88F97, #8FB89B, #B8A98F alongside #8FA6B8) that do NOT match
  tokens.css's --id-* values — those look like an earlier/different palette
  draft. Per this task's explicit instruction to reuse the *same* tokens
  Avatar/Pointer/NamePill already draw from (not hardcode a second palette),
  this component uses the real --id-* six (plum/blue/green/amber/lilac/slate)
  instead of reproducing those comp-only one-off hex values.
-->
<script lang="ts">
  import Avatar from './Avatar.svelte';
  import type { IdentityColor } from './Avatar.svelte';

  const PALETTE: IdentityColor[] = ['plum', 'blue', 'green', 'amber', 'lilac', 'slate'];

  interface Props {
    name?: string;
    identity?: IdentityColor;
    onNameChange?: (name: string) => void;
    onIdentityChange?: (identity: IdentityColor) => void;
  }

  let {
    name = $bindable(''),
    identity = $bindable('slate'),
    onNameChange,
    onIdentityChange
  }: Props = $props();

  function selectIdentity(color: IdentityColor) {
    identity = color;
    onIdentityChange?.(color);
  }

  function handleSwatchKeydown(event: KeyboardEvent) {
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const current = PALETTE.indexOf(identity);
    let next: number;
    if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = PALETTE.length - 1;
    else {
      const delta = event.key === 'ArrowRight' || event.key === 'ArrowDown' ? 1 : -1;
      next = (current + delta + PALETTE.length) % PALETTE.length;
    }
    selectIdentity(PALETTE[next]);
    // Roving tabindex: move focus with the selection so Tab continues from
    // the newly selected swatch.
    const buttons = (event.currentTarget as HTMLElement).querySelectorAll<HTMLElement>('.swatch');
    buttons[next]?.focus();
  }

  function handleInput(event: Event) {
    name = (event.target as HTMLInputElement).value;
    onNameChange?.(name);
  }
</script>

<div class="identity-setup">
  <div class="name-row">
    <Avatar {name} {identity} size={44} emptyPlaceholder="person" />
    <input
      type="text"
      class="name-input"
      autocomplete="off"
      placeholder="Your name"
      value={name}
      oninput={handleInput}
      aria-label="Your name"
    />
  </div>

  <div class="swatch-row" role="radiogroup" aria-label="Identity color" tabindex="-1" onkeydown={handleSwatchKeydown}>
    {#each PALETTE as color (color)}
      <button
        type="button"
        class="swatch"
        class:selected={identity === color}
        style:background="var(--id-{color})"
        style:--swatch-color="var(--id-{color})"
        role="radio"
        aria-checked={identity === color}
        aria-label={color}
        tabindex={identity === color ? 0 : -1}
        onclick={() => selectIdentity(color)}
      ></button>
    {/each}
  </div>
</div>

<style>
  .identity-setup {
    border-radius: var(--radius-popover);
    background: var(--fill-base);
    border: 1px solid var(--hairline-strong);
    padding: 16px;
  }

  .name-row {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 14px;
  }

  .name-input {
    flex: 1;
    height: 36px;
    border-radius: var(--radius-input);
    background: var(--fill-base);
    border: 1px solid var(--hairline-strong);
    padding: 0 12px;
    font: 500 13px var(--font-ui);
    color: var(--text-primary);
    box-sizing: border-box;
  }

  .name-input::placeholder {
    color: var(--text-faint);
  }

  .name-input:focus {
    outline: none;
    /* Focus emphasis border — kept literal (uiConsistency allowlist). */
    border-color: rgba(255, 255, 255, 0.25);
  }

  .swatch-row {
    display: flex;
    gap: 14px;
  }

  .swatch {
    position: relative;
    width: 26px;
    height: 26px;
    border-radius: var(--radius-pill);
    border: none;
    cursor: pointer;
    box-shadow: none;
    padding: 0;
    transition:
      box-shadow var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .swatch::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .swatch.selected {
    box-shadow:
      0 0 0 2px var(--surface-2),
      0 0 0 3px var(--swatch-color);
  }

  .swatch:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .swatch:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 3px;
  }
</style>
