<!--
  Drives the REAL desktop PluginSurfaces component the way the meeting route
  does: mounted before the host version is known (null), then handed the
  version asynchronously. The test flips `hostVersion` from the outside and
  watches the bound toolbar-button models and the logic frames in the DOM.
-->
<script lang="ts">
  import PluginSurfaces from '$lib/plugins/PluginSurfaces.svelte';
  import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces';

  let hostVersion = $state<string | null>(null);
  let buttons = $state<ToolbarButtonModel[]>([]);
  const toasts: string[] = [];

  if (typeof window !== 'undefined') {
    (window as Window & { pluginSurfacesFixture?: unknown }).pluginSurfacesFixture = {
      setHostVersion: (v: string | null) => (hostVersion = v),
      buttons: () => $state.snapshot(buttons),
      toasts
    };
  }

  $effect(() => {
    document.body.dataset.ready = 'true';
  });
</script>

<PluginSurfaces
  bind:buttons
  participants={[{ identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false }]}
  roomLabel="Eng sync"
  phase="connected"
  {hostVersion}
  onToast={(text) => toasts.push(text)}
/>
