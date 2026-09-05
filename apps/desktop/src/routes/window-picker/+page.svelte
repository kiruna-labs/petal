<!-- Standalone native window for choosing a real desktop window to share. -->
<script lang="ts">
  import { emit } from '@tauri-apps/api/event';
  import { onMount } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import WindowPicker, { prewarmWindowPicker } from '$lib/components/WindowPicker.svelte';
  import { EVENTS, hasTauriBridge } from '$lib/ipc';

  const hasTauri = hasTauriBridge();

  if (hasTauri) {
    void prewarmWindowPicker();
  }

  function notifyChanged() {
    if (!hasTauri) return;
    void emit(EVENTS.sharePickerChanged);
  }

  function notifyVisibility(open: boolean) {
    if (!hasTauri) return;
    void emit(EVENTS.sharePickerVisibilityChanged, { open });
  }

  onMount(() => {
    notifyVisibility(true);
  });

  function closeWindow() {
    notifyChanged();
    notifyVisibility(false);
    if (hasTauri) {
      void getCurrentWindow().close();
    } else {
      window.close();
    }
  }
</script>

<main class="secondary-window">
  <div class="secondary-content">
    <!-- The picker stays open after each share toggle so concurrent sharing
         feels like flipping switches; the header CloseButton is the explicit
         "Done" (see WindowPicker.svelte's per-card toggle). -->
    <WindowPicker
      standalone
      entryMotion={false}
      onChanged={notifyChanged}
      onClose={closeWindow}
    />
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
