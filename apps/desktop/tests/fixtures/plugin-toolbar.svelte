<script lang="ts">
  import PluginToolbarButtons from '../../src/lib/plugins/PluginToolbarButtons.svelte';
  import PluginContextMenu from '../../src/lib/plugins/PluginContextMenu.svelte';
  import type { PluginMenuTarget } from '@petal/shared/plugin-host/provenance';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';

  const buttons: ToolbarButtonModel[] = [
    { pluginId: 'petal.reactions', pluginName: 'Reactions', buttonId: 'react', label: 'React', icon: 'smile', badge: null, disabled: false, opens: 'popover:picker', ariaLabel: 'React (Reactions)' },
    { pluginId: 'acme.webhook-notifier-pro', pluginName: 'Webhook Notifier Deluxe', buttonId: 'ping', label: 'Ping everyone', icon: 'bell', badge: 3, disabled: false, opens: null, ariaLabel: 'Ping everyone (Webhook Notifier Deluxe)' }
  ];
  let menu = $state<PluginMenuTarget | null>(null);
  const names: Record<string, string> = Object.fromEntries(buttons.map((b) => [b.pluginId, b.pluginName]));
</script>

<div class="controls-cluster" style="display:flex;gap:12px;justify-content:center;padding:12px 28px;">
  <PluginToolbarButtons
    {buttons}
    onActivate={(p, b) => (window as any).__events.push(`activate:${p}/${b}`)}
    onMenu={(p, at) => (menu = { pluginId: p, name: names[p]!, x: at.x, y: at.y })}
  />
</div>
{#if menu}
  <PluginContextMenu
    target={menu}
    onSelect={(id, p) => {
      (window as any).__events.push(`${id}:${p}`);
      menu = null;
    }}
    onClose={() => (menu = null)}
  />
{/if}
