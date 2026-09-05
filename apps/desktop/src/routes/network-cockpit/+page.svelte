<!-- Persistent diagnostics window opened from the in-meeting Gallery topbar. -->
<script lang="ts">
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import NetworkCockpit from '$lib/components/NetworkCockpit.svelte';

  function closeWindow() {
    try {
      void getCurrentWindow().close();
    } catch {
      // Browser preview: no native window to close.
    }
  }
</script>

<main class="secondary-window">
  <div class="secondary-content">
    <NetworkCockpit standalone onClose={closeWindow} />
  </div>
</main>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    width: 100%;
    height: 100%;
    background: transparent;
    overscroll-behavior: none;
  }

  .secondary-window {
    min-height: 100vh;
    height: 100vh;
    position: relative;
    display: flex;
    box-sizing: border-box;
    background: var(--bg-base, var(--gallery-frame));
    overscroll-behavior: none;
    overflow: hidden;
  }

  .secondary-content {
    min-width: 0;
    min-height: 0;
    flex: 1;
    display: flex;
    overflow: hidden;
  }
</style>
