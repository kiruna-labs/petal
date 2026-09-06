<!--
  Switch — the one Petal on/off control for settings rows (#923). A real
  `<input type="checkbox" role="switch">` stays the interactive element so
  keyboard + screen-reader semantics are intact and callers keep the exact
  `onchange={(e) => ...e.currentTarget.checked}` shape Checkbox.svelte uses;
  the track/knob are decorative CSS on the same element.

  Colour rationing (Petal-Build-Map.md §1/§3): the on-state is a brighter
  GRAPHITE track, not the live/success green and not an identity colour —
  a settings panel is exactly the surface that must stay monochrome.
-->
<script lang="ts">
  interface Props {
    checked?: boolean;
    disabled?: boolean;
    /** Accessible name when the switch is not wrapped by a labelling element. */
    label?: string;
    /** The native `change` event from the input — `event.currentTarget.checked`
     * is the new value, the same shape Checkbox.svelte callers pass. */
    onchange?: (event: Event & { currentTarget: HTMLInputElement }) => void;
  }

  // One-way on purpose: `checked` is rendered from the caller's state and the
  // caller reacts to `onchange`. A `bind:checked` here would keep a local copy
  // that Svelte writes back to the DOM after the caller's handler ran — which
  // broke Settings' AI-chat consent step (the switch flipped on while the
  // feature stayed off). With no local copy, a handler that resets
  // `event.currentTarget.checked` is honoured.
  let { checked = false, disabled = false, label, onchange = undefined }: Props = $props();
</script>

<input
  class="petal-switch"
  type="checkbox"
  role="switch"
  {checked}
  {disabled}
  aria-label={label}
  {onchange}
/>

<style>
  .petal-switch {
    appearance: none;
    -webkit-appearance: none;
    flex-shrink: 0;
    position: relative;
    width: 38px;
    height: 22px;
    margin: 0;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-pill);
    background: var(--fill-base);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      border-color var(--motion-fast) var(--ease-standard);
  }

  .petal-switch::after {
    content: '';
    position: absolute;
    top: 2px;
    left: 2px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--text-dim);
    transition:
      transform var(--motion-fast) var(--ease-standard),
      background-color var(--motion-fast) var(--ease-standard);
  }

  .petal-switch:hover:not(:disabled) {
    background: var(--fill-strong);
  }

  .petal-switch:checked {
    background: var(--fill-bright);
    border-color: transparent;
  }

  .petal-switch:checked::after {
    transform: translateX(16px);
    background: var(--text-primary);
  }

  .petal-switch:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
    pointer-events: none;
  }

  .petal-switch:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }
</style>
