<!-- Shared close affordance for modal and secondary-window chrome. -->
<script lang="ts">
  interface Props {
    ariaLabel?: string;
    disabled?: boolean;
    onclick?: (event: MouseEvent) => void;
  }

  let { ariaLabel = 'Close', disabled = false, onclick }: Props = $props();
</script>

<button type="button" class="close-button" aria-label={ariaLabel} {disabled} {onclick}>
  <svg class="close-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    <path d="M4.25 4.25l7.5 7.5M11.75 4.25l-7.5 7.5" />
  </svg>
</button>

<style>
  .close-button {
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
      background-color var(--motion-feedback) var(--ease-standard),
      box-shadow var(--motion-feedback) var(--ease-standard),
      color var(--motion-feedback) var(--ease-standard),
      transform var(--motion-feedback) var(--ease-standard);
  }

  .close-button:hover:not(:disabled) {
    background-color: var(--fill-strong);
    box-shadow: inset 0 0 0 1px var(--hairline);
    color: var(--text-primary);
  }

  .close-button:active:not(:disabled) {
    transform: scale(var(--press-scale, 0.96));
  }

  .close-button:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .close-button:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
  }

  .close-icon {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    pointer-events: none;
  }
</style>
