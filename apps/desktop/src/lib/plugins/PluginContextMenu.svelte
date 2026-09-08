<!--
  Right-click menu for a plugin surface (plugins/README.md §2.7): names the
  plugin and lets the user turn it off on the spot. Re-enabling lives in
  Settings → Plugins, which the confirmation toast says. Styles come from the
  shared plugin-provenance.css so the web client renders the identical menu.
-->
<script lang="ts">
  import { onMount } from 'svelte';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';
  import { pluginIconSvg } from '@petal/shared/plugin-host/icons';
  import { pluginMenuModel, type PluginMenuTarget } from '@petal/shared/plugin-host/provenance';

  interface Props {
    target: PluginMenuTarget;
    onSelect: (itemId: string, pluginId: string) => void;
    onClose: () => void;
  }

  let { target, onSelect, onClose }: Props = $props();

  const model = $derived(pluginMenuModel(target.name, target.source));
  let menuEl = $state<HTMLElement | null>(null);
  let left = $state(0);
  let top = $state(0);

  onMount(() => {
    const el = menuEl!;
    const rect = el.getBoundingClientRect();
    left = Math.max(8, Math.min(target.x, window.innerWidth - rect.width - 8));
    top = Math.max(8, Math.min(target.y, window.innerHeight - rect.height - 8));
    el.querySelector<HTMLElement>('button')?.focus();
    const cleanup = installDismissibleLayer({
      isOpen: () => true,
      getInsideNodes: () => [el],
      getPopupNodes: () => [el],
      onDismiss: onClose,
      document
    });
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => {
      cleanup();
      window.removeEventListener('keydown', onKey);
    };
  });
</script>

<div class="plugin-menu" role="menu" aria-label={model.heading} bind:this={menuEl} style:left="{left}px" style:top="{top}px">
  <div class="plugin-menu-label">
    <span aria-hidden="true">{@html pluginIconSvg('puzzle', 12)}</span>
    <span>{model.heading}</span>
  </div>
  {#each model.items as item (item.id)}
    <button type="button" class="plugin-menu-row" role="menuitem" onclick={() => onSelect(item.id, target.pluginId)}>
      {item.label}
    </button>
  {/each}
</div>
