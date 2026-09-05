<!--
  Checkbox — the one Petal checkbox control. Native `<input type="checkbox">`
  cannot follow the token surfaces (fills, hairlines, --radius-check), and its
  `accent-color` treatment previously leaked the live/success green into
  settings surfaces that must stay graphite (Petal-Build-Map.md §1/§3 color
  rationing; Settings.svelte's own panel is documented graphite-only). The
  check glyph reuses the exact path/stroke convention of Toast.svelte's +
  PermissionRow.svelte's success checkmarks (stroke-width 2.4, viewBox 24),
  colored --text-strong — monochrome; green stays reserved for live/success.

  The real `<input type="checkbox">` stays the interactive element (keyboard +
  screen-reader semantics intact); the SVG check is decorative (aria-hidden).
-->
<script lang="ts">
  interface Props {
    checked?: boolean;
    disabled?: boolean;
    /** The native `change` event from the input — `event.currentTarget.checked`
     * is the new value, the same shape callers pass today. */
    onchange?: (event: Event & { currentTarget: HTMLInputElement }) => void;
  }

  let { checked = $bindable(false), disabled = false, onchange = undefined }: Props = $props();
</script>

<span class="petal-checkbox">
  <input type="checkbox" bind:checked {disabled} {onchange} />
  <svg
    class="check"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2.4"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="M5 12.5 10 17.5 19 7"></path>
  </svg>
</span>

<style>
  .petal-checkbox {
    position: relative;
    display: inline-flex;
    flex-shrink: 0;
    width: 18px;
    height: 18px;
  }

  .petal-checkbox input {
    appearance: none;
    width: 18px;
    height: 18px;
    margin: 0;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-check);
    background: var(--fill-weak);
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      border-color var(--motion-fast) var(--ease-standard);
  }

  .petal-checkbox input:hover:not(:disabled) {
    background: var(--fill-base);
  }

  .petal-checkbox input:checked {
    background: var(--fill-strong);
  }

  .petal-checkbox input:checked:hover:not(:disabled) {
    background: var(--fill-bright);
  }

  .petal-checkbox input:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
    pointer-events: none;
  }

  .petal-checkbox input:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .check {
    position: absolute;
    inset: 0;
    width: 12px;
    height: 12px;
    margin: auto;
    color: var(--text-strong);
    opacity: 0;
    pointer-events: none;
    transition: opacity var(--motion-fast) var(--ease-standard);
  }

  .petal-checkbox input:checked ~ .check {
    opacity: 1;
  }
</style>
