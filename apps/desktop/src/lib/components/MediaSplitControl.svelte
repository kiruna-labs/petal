<script lang="ts">
  import ControlButton, { type ControlSize } from './ControlButton.svelte';

  type SplitSize = 'gallery' | 'pill' | 'menubar';

  interface Props {
    icon: 'mic' | 'camera';
    active: boolean;
    actionLabel: string;
    optionsLabel: string;
    optionsOpen?: boolean;
    optionsEnabled?: boolean;
    size?: SplitSize;
    visibleLabel?: string;
    liveBackground?: string;
    liveColor?: string;
    onToggle?: (event: MouseEvent) => void;
    onOptions?: (trigger: HTMLElement) => void;
  }

  let {
    icon,
    active,
    actionLabel,
    optionsLabel,
    optionsOpen = false,
    optionsEnabled = true,
    size = 'gallery',
    visibleLabel,
    liveBackground,
    liveColor,
    onToggle,
    onOptions
  }: Props = $props();

  let optionsButton = $state<HTMLButtonElement>();

  const buttonSize = $derived<ControlSize>(size === 'gallery' ? 44 : size === 'pill' ? 'pill' : 'compact');
</script>

<div
  class="media-split-control"
  class:media-split-gallery={size === 'gallery'}
  class:media-split-pill={size === 'pill'}
  class:media-split-menubar={size === 'menubar'}
>
  <div class="meeting-split">
    <ControlButton
      icon={icon}
      kind="toggle"
      tone="neutral"
      size={buttonSize}
      active={active}
      label={actionLabel}
      liveBackground={liveBackground}
      liveColor={liveColor}
      onclick={onToggle}
    />
    {#if optionsEnabled}
    <button
      bind:this={optionsButton}
      type="button"
      class="meeting-split-options"
      aria-label={optionsLabel}
      aria-haspopup="dialog"
      aria-expanded={optionsOpen}
      title={optionsLabel}
      onclick={() => onOptions?.(optionsButton!)}
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="m6 9 6 6 6-6"></path>
      </svg>
    </button>
    {/if}
  </div>
  {#if visibleLabel}
    <span class="meeting-control-label">{visibleLabel}</span>
  {/if}
</div>

<style>
  .media-split-control {
    display: inline-flex;
    flex-direction: column;
    align-items: center;
    flex: 0 0 auto;
  }

  .media-split-gallery {
    --meeting-split-height: 52px;
    --meeting-split-options-width: 24px;
  }

  .media-split-pill {
    --meeting-split-height: 40px;
    --meeting-split-options-width: 22px;
  }

  .media-split-menubar {
    --meeting-split-height: 32px;
    --meeting-split-options-width: 20px;
  }

  .media-split-gallery :global(.control-button) {
    width: 52px !important;
    height: 52px !important;
  }

  .media-split-pill :global(.control-button) {
    width: 40px !important;
    height: 40px !important;
  }

  .media-split-menubar :global(.control-button) {
    width: 32px !important;
    height: 32px !important;
  }

  .media-split-control :global(.control-button) {
    flex: 0 0 auto;
  }
</style>
