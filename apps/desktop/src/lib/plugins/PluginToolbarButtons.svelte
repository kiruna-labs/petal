<!--
  Host-drawn toolbar buttons for plugins (plugins/README.md §2.7). One
  .control-cell per button, matching Gallery's own cells (52px circle, 20px
  glyph, label underneath) so plugin buttons sit in the same row without
  looking bolted on. Labels come pre-clamped from the shared button model.
-->
<script lang="ts">
  import { pluginIconSvg } from '@petal/shared/plugin-host/icons';
  import { badgeText, type ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';

  interface Props {
    buttons: ToolbarButtonModel[];
    onActivate: (pluginId: string, buttonId: string, anchor: HTMLElement) => void;
  }

  let { buttons, onActivate }: Props = $props();
</script>

{#each buttons as button (button.pluginId + '/' + button.buttonId)}
  <div class="control-cell plugin-cell" data-plugin={button.pluginId} data-button={button.buttonId}>
    <button
      type="button"
      class="plugin-button"
      aria-label={button.ariaLabel}
      aria-haspopup={button.opens ? 'dialog' : undefined}
      disabled={button.disabled}
      onclick={(event) => onActivate(button.pluginId, button.buttonId, event.currentTarget)}
    >
      <span class="glyph" aria-hidden="true">{@html pluginIconSvg(button.icon, 20)}</span>
      {#if badgeText(button.badge) !== null}
        <span class="badge">{badgeText(button.badge)}</span>
      {/if}
    </button>
    <span class="meeting-control-label">{button.label}</span>
  </div>
{/each}

<style>
  .control-cell {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    min-width: 52px;
  }

  .plugin-button {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 52px;
    height: 52px;
    border-radius: var(--radius-pill);
    border: none;
    padding: 0;
    background: var(--fill-strong);
    color: var(--text-strong);
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
    flex-shrink: 0;
  }

  .plugin-button:hover:not(:disabled) {
    background: var(--fill-bright);
  }

  .plugin-button:active:not(:disabled) {
    opacity: 0.8;
    transform: scale(var(--press-scale, 0.96));
  }

  .plugin-button:disabled {
    background: var(--fill-weak);
    color: var(--text-faint);
    opacity: var(--disabled-opacity);
    cursor: default;
  }

  .plugin-button:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .glyph {
    display: inline-flex;
  }

  .badge {
    position: absolute;
    top: -2px;
    right: -2px;
    min-width: 16px;
    height: 16px;
    padding: 0 4px;
    border-radius: var(--radius-pill);
    background: var(--live-bright);
    color: var(--bg-base);
    font: 600 10px/16px var(--font-ui);
    text-align: center;
  }

  .meeting-control-label {
    margin-top: 6px;
    font: 500 11px var(--font-ui);
    color: var(--text-muted);
    white-space: nowrap;
  }
</style>
