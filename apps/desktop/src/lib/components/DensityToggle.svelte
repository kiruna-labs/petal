<!--
  DensityToggle — the chevron-driven two-position switch that collapses the
  full in-meeting control row into the Pill compact state and back. Per
  Petal-Build-Map.md §2.2 / DESIGN.md §6 "Density toggle": chevron-only
  expand/collapse for v1 (approved default) — no drag-to-expand interaction
  yet, but the prop/hook shape (`ondragstart`) is left open for it later.

  Two usage shapes, matching the approved comps:
  1. The segmented "Comfortable / Compact" switch (§6 density toggle board) —
     rendered when `variant="segmented"`.
  2. A single chevron affordance sitting on the control bar itself
     (§2 "1a — Explicit expand button") — rendered when `variant="chevron"`,
     the one actually used in the in-meeting bar per the approved gallery.
-->
<script lang="ts">
  type Density = 'comfortable' | 'compact';

  interface Props {
    density?: Density;
    variant?: 'segmented' | 'chevron';
    onchange?: (density: Density) => void;
    /** Reserved hook for a future drag-to-expand gesture — not implemented in v1. */
    ondragstart?: (event: PointerEvent) => void;
  }

  let { density = $bindable('comfortable'), variant = 'chevron', onchange, ondragstart }: Props = $props();

  function toggle() {
    density = density === 'comfortable' ? 'compact' : 'comfortable';
    onchange?.(density);
  }

  function select(next: Density) {
    if (density === next) return;
    density = next;
    onchange?.(density);
  }
</script>

{#if variant === 'segmented'}
  <div class="segmented" role="group" aria-label="Density">
    <button
      type="button"
      class="segment"
      class:selected={density === 'comfortable'}
      onclick={() => select('comfortable')}
    >
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <rect x="3" y="4" width="7" height="7" rx="1.5"></rect>
        <rect x="14" y="4" width="7" height="7" rx="1.5"></rect>
        <rect x="3" y="14" width="7" height="7" rx="1.5"></rect>
        <rect x="14" y="14" width="7" height="7" rx="1.5"></rect>
      </svg>
      <span>Comfortable</span>
    </button>
    <button
      type="button"
      class="segment"
      class:selected={density === 'compact'}
      onclick={() => select('compact')}
    >
      <span class="compact-dots" aria-hidden="true">
        <span class="dot"></span><span class="dot"></span>
      </span>
      <span>Compact</span>
    </button>
  </div>
{:else}
  <button
    type="button"
    class="chevron-toggle"
    onclick={toggle}
    onpointerdown={ondragstart}
    aria-label={density === 'comfortable' ? 'Collapse to compact' : 'Expand to full controls'}
    aria-expanded={density === 'comfortable'}
  >
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2.6"
      stroke-linecap="round"
      stroke-linejoin="round"
      class:flipped={density === 'compact'}
    >
      <path d="M6 9l6 6 6-6"></path>
    </svg>
  </button>
{/if}

<style>
  .segmented {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    padding: 4px;
    border-radius: var(--radius-pill);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
  }

  .segment {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 8px 14px;
    border-radius: var(--radius-pill);
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 11.5px;
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .segment::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 0;
    right: 0;
    height: 40px;
    transform: translateY(-50%);
  }

  .segment:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .segment.selected {
    background: var(--fill-bright);
    color: var(--text-primary);
  }

  .compact-dots {
    display: flex;
  }

  .compact-dots .dot {
    width: 10px;
    height: 10px;
    border-radius: var(--radius-pill);
    background: currentColor;
  }

  .compact-dots .dot + .dot {
    margin-left: -3px;
  }

  .chevron-toggle {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    border-radius: var(--radius-pill);
    background: var(--fill-strong);
    border: 1px solid var(--hairline-strong);
    color: var(--text-strong);
    cursor: pointer;
    flex-shrink: 0;
    transition:
      background var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .chevron-toggle::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .chevron-toggle:hover {
    background: var(--fill-bright);
  }

  .chevron-toggle:active {
    transform: scale(var(--press-scale, 0.96));
  }

  .segment:focus-visible,
  .chevron-toggle:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .chevron-toggle svg {
    transition: transform var(--motion-base) var(--ease-standard);
  }

  .chevron-toggle svg.flipped {
    transform: rotate(180deg);
  }
</style>
