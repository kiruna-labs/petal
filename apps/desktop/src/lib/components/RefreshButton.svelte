<!-- Shared refresh affordance for secondary-window chrome (e.g. the window
     picker's manual refresh of the shareable-window grid). Mirrors
     CloseButton's visual contract. -->
<script lang="ts">
  interface Props {
    ariaLabel?: string;
    disabled?: boolean;
    onclick?: (event: MouseEvent) => void;
  }

  let { ariaLabel = 'Refresh', disabled = false, onclick }: Props = $props();
</script>

<button type="button" class="refresh-button" aria-label={ariaLabel} {disabled} {onclick}>
  <svg class="refresh-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    <path d="M13.5 8a5.5 5.5 0 1 1-1.61-3.89" />
    <path d="M13.5 1.75v2.75h-2.75" />
  </svg>
</button>

<style>
  .refresh-button {
    width: 40px;
    height: 40px;
    display: inline-grid;
    place-items: center;
    flex: 0 0 auto;
    border: 0;
    border-radius: var(--radius-control);
    background-color: transparent;
    color: var(--text-muted);
    cursor: pointer;
    box-shadow: inset 0 0 0 1px transparent;
    transition:
      background-color var(--motion-fast, 120ms) var(--ease-standard, cubic-bezier(0.2, 0, 0, 1)),
      box-shadow var(--motion-fast, 120ms) var(--ease-standard, cubic-bezier(0.2, 0, 0, 1)),
      color var(--motion-fast, 120ms) var(--ease-standard, cubic-bezier(0.2, 0, 0, 1)),
      transform var(--motion-fast, 120ms) var(--ease-standard, cubic-bezier(0.2, 0, 0, 1));
  }

  .refresh-button:hover:not(:disabled) {
    background-color: var(--fill-strong);
    box-shadow: inset 0 0 0 1px var(--hairline);
    color: var(--text-primary);
  }

  .refresh-button:active:not(:disabled) {
    transform: scale(var(--press-scale, 0.96));
  }

  .refresh-button:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .refresh-button:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
  }

  .refresh-icon {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
    pointer-events: none;
  }
</style>
