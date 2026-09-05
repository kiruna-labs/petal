<script lang="ts">
  import { tick } from 'svelte';
  import { nextDeviceOptionIndex } from './deviceSelect';
  import {
    restrainedSurfaceEnterTransition,
    restrainedSurfaceExitTransition
  } from '$lib/motion';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';

  export interface DeviceSelectOption {
    id: string;
    label: string;
  }

  interface Props {
    id: string;
    label: string;
    value: string;
    options: DeviceSelectOption[];
    emptyLabel: string;
    disabled?: boolean;
    /** Per-option disabled set (greyed-out presets, e.g. camera modes this
     * camera can't deliver). Disabled options are not selectable or
     * keyboard-focusable. */
    disabledOptions?: Set<string>;
    onchange: (value: string) => void;
  }

  let {
    id,
    label,
    value,
    options,
    emptyLabel,
    disabled = false,
    disabledOptions = new Set(),
    onchange
  }: Props = $props();
  let root: HTMLDivElement;
  let open = $state(false);
  let trigger: HTMLButtonElement;
  let optionsEl = $state<HTMLDivElement>();
  let optionButtons: HTMLButtonElement[] = [];
  let openAbove = $state(false);
  let menuMaxHeight = $state(220);

  const selectedIndex = $derived(options.findIndex((option) => option.id === value));
  const selectedLabel = $derived(options[selectedIndex]?.label ?? emptyLabel);

  $effect(() => {
    if (!open) return;
    return installDismissibleLayer({
      isOpen: () => open,
      getInsideNodes: () => [root],
      getPopupNodes: () => [optionsEl],
      getOpener: () => trigger,
      onDismiss: () => close()
    });
  });

  function close(restoreFocus = false) {
    open = false;
    if (restoreFocus) void tick().then(() => trigger.focus());
  }

  function updatePlacement() {
    const triggerBounds = trigger.getBoundingClientRect();
    const scrollport = trigger.closest('.settings-body')?.getBoundingClientRect();
    const top = scrollport?.top ?? 0;
    const bottom = scrollport?.bottom ?? window.innerHeight;
    const spaceAbove = triggerBounds.top - top - 6;
    const spaceBelow = bottom - triggerBounds.bottom - 6;
    const desiredHeight = Math.min(220, options.length * 38 + 12);
    openAbove = spaceBelow < desiredHeight && spaceAbove > spaceBelow;
    menuMaxHeight = Math.max(72, Math.min(220, Math.floor(openAbove ? spaceAbove : spaceBelow)));
  }

  function toggle() {
    if (open) {
      close();
      return;
    }
    // Never open an empty popover: with zero options the trigger already
    // shows `emptyLabel` and there is nothing to pick.
    if (options.length === 0) return;
    updatePlacement();
    open = true;
  }

  async function focusOption(index: number) {
    updatePlacement();
    open = true;
    await tick();
    optionButtons[index]?.focus();
  }

  /** Keyboard navigation that skips disabled options (greyed presets are
   * not focusable). Home/End walk in the relevant direction. */
  function nextEnabledIndex(current: number, key: string): number | null {
    const first = nextDeviceOptionIndex(current, key, options.length);
    if (first === null) return null;
    if (!disabledOptions.has(options[first].id)) return first;
    const stepKey = key === 'Home' ? 'ArrowDown' : key === 'End' ? 'ArrowUp' : key;
    let probe: number | null = first;
    for (let steps = 0; steps < options.length; steps++) {
      probe = nextDeviceOptionIndex(probe, stepKey, options.length);
      if (probe === null || probe === first) break;
      if (!disabledOptions.has(options[probe].id)) return probe;
    }
    return null;
  }

  function handleTriggerKeydown(event: KeyboardEvent) {
    if (open && event.key === 'Escape') {
      event.preventDefault();
      close(true);
      return;
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const current = selectedIndex >= 0 ? selectedIndex : 0;
    const next = nextEnabledIndex(current, event.key);
    if (next !== null) void focusOption(next);
  }

  function handleOptionKeydown(event: KeyboardEvent, index: number) {
    if (event.key === 'Escape') {
      event.preventDefault();
      close(true);
      return;
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const next = nextEnabledIndex(index, event.key);
    if (next !== null) optionButtons[next]?.focus();
  }

  function handleFocusOut(event: FocusEvent) {
    if (open && (!(event.relatedTarget instanceof Node) || !root.contains(event.relatedTarget))) {
      close();
    }
  }

  function select(value: string) {
    if (disabledOptions.has(value)) return;
    onchange(value);
    close(true);
  }
</script>

<div class="device-select" bind:this={root} onfocusout={handleFocusOut}>
  <button
    bind:this={trigger}
    type="button"
    class="trigger"
    aria-haspopup="listbox"
    aria-label={`${label}: ${selectedLabel}`}
    aria-expanded={open}
    aria-controls={`${id}-options`}
    {disabled}
    onclick={toggle}
    onkeydown={handleTriggerKeydown}
  >
    <span>{selectedLabel}</span>
    <svg class:open width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="m6 9 6 6 6-6"></path>
    </svg>
  </button>

  {#if open}
    <div
      bind:this={optionsEl}
      id={`${id}-options`}
      class="options"
      class:above={openAbove}
      in:restrainedSurfaceEnterTransition
      out:restrainedSurfaceExitTransition
      role="listbox"
      aria-label={`${label} devices`}
      style={`max-height: ${menuMaxHeight}px`}
    >
      {#each options as option, index (option.id)}
        <button
          bind:this={optionButtons[index]}
          type="button"
          class="option"
          class:selected={option.id === value}
          class:disabled={disabledOptions.has(option.id)}
          role="option"
          aria-selected={option.id === value}
          aria-disabled={disabledOptions.has(option.id)}
          disabled={disabledOptions.has(option.id)}
          onclick={() => select(option.id)}
          onkeydown={(event) => handleOptionKeydown(event, index)}
        >
          <span>{option.label}</span>
          {#if option.id === value}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="m5 12 4 4L19 6"></path>
            </svg>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .device-select {
    position: relative;
    min-width: 0;
  }

  .trigger {
    position: relative;
    z-index: 1;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    min-height: 40px;
    height: auto;
    box-sizing: border-box;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-input);
    padding: 8px 11px 8px 12px;
    background: var(--fill-base);
    color: var(--text-primary);
    font: 500 13px var(--font-ui);
    text-align: left;
    cursor: pointer;
    transition:
      border-color var(--motion-fast) var(--ease-standard),
      background-color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .trigger span,
  .option span {
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .trigger:hover:not(:disabled),
  .trigger[aria-expanded='true'] {
    /* Open-state emphasis border — kept literal (uiConsistency allowlist). */
    border-color: rgba(255, 255, 255, 0.18);
    background: var(--fill-strong);
  }

  .trigger:active:not(:disabled) {
    transform: scale(var(--press-scale, 0.98));
  }

  .trigger:focus-visible,
  .option:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .trigger:disabled {
    color: var(--text-faint);
    cursor: not-allowed;
    opacity: 0.58;
  }

  .trigger svg {
    flex: 0 0 auto;
    color: var(--text-faint);
    transition: transform var(--motion-fast) var(--ease-standard);
  }

  .trigger svg.open {
    transform: rotate(180deg);
  }

  .options {
    position: absolute;
    z-index: 41;
    top: calc(100% + 6px);
    left: 0;
    right: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 220px;
    overflow-y: auto;
    overscroll-behavior: contain;
    box-sizing: border-box;
    padding: 6px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-popover);
    background: var(--popover-bg);
    box-shadow: var(--shadow-panel);
  }

  .options.above {
    top: auto;
    bottom: calc(100% + 6px);
  }

  .option {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    min-height: 40px;
    flex: 0 0 auto;
    border: 0;
    border-radius: var(--radius-chip);
    padding: 8px 9px;
    background: transparent;
    color: var(--text-soft);
    font: 500 12.5px var(--font-ui);
    text-align: left;
    cursor: pointer;
    transition:
      background-color var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard);
  }

  .option:hover,
  .option:focus-visible {
    background: var(--fill-strong);
    color: var(--text-primary);
  }

  .option.selected {
    color: var(--text-primary);
    background: var(--fill-weak);
  }

  .option.disabled {
    color: var(--text-faint);
    cursor: not-allowed;
    opacity: 0.58;
  }

  .option.disabled:hover,
  .option.disabled:focus-visible {
    background: transparent;
    color: var(--text-faint);
  }

  .option svg {
    flex: 0 0 auto;
    color: var(--id-blue);
  }


  @media (prefers-reduced-motion: reduce) {
    .trigger,
    .trigger svg,
    .option {
      transition: none;
    }
  }
</style>
