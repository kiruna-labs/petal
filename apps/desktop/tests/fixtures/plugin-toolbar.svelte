<script lang="ts">
  import PluginToolbarButtons from '../../src/lib/plugins/PluginToolbarButtons.svelte';
  import PluginContextMenu from '../../src/lib/plugins/PluginContextMenu.svelte';
  import type { PluginMenuTarget } from '@petal/shared/plugin-host/provenance';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';

  // Two plugins with the SAME visible name, from different sources: what the
  // chrome has to keep telling apart (kiruna-labs/petal#71 review, finding 3).
  const buttons: ToolbarButtonModel[] = [
    { pluginId: 'petal.reactions', pluginName: 'Reactions', pluginSource: 'builtin', buttonId: 'react', label: 'React', icon: 'smile', badge: null, disabled: false, opens: 'popover:picker', ariaLabel: 'React (Reactions · built-in plugin)' },
    { pluginId: 'acme.reactions', pluginName: 'Reactions', pluginSource: 'registry', buttonId: 'react', label: 'React', icon: 'smile', badge: null, disabled: false, opens: null, ariaLabel: 'React (Reactions · installed plugin)' },
    { pluginId: 'acme.webhook-notifier-pro', pluginName: 'Webhook Notifier Deluxe', pluginSource: 'registry', buttonId: 'ping', label: 'Ping everyone', icon: 'bell', badge: 3, disabled: false, opens: null, ariaLabel: 'Ping everyone (Webhook Notifier Deluxe · installed plugin)' }
  ];
  let menu = $state<PluginMenuTarget | null>(null);
  const byId = new Map(buttons.map((b) => [b.pluginId, b]));
</script>

<div class="controls-cluster" style="display:flex;gap:12px;justify-content:center;padding:12px 28px;">
  <PluginToolbarButtons
    {buttons}
    onActivate={(p, b) => (window as any).__events.push(`activate:${p}/${b}`)}
    onMenu={(p, at) => {
      const model = byId.get(p)!;
      menu = { pluginId: p, name: model.pluginName, source: model.pluginSource, x: at.x, y: at.y };
    }}
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
