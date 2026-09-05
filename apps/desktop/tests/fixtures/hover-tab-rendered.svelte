<script lang="ts">
  import Pill from '@petal/shared/ui/components/Pill.svelte';

  let shared = $state(false);
  let inset = $state(false);
  let pending = $state(false);
  let shareClicks = $state(0);
  let menuOpens = $state(0);
  let lastMenuInvocation = $state<'pointer' | 'keyboard' | null>(null);
  const keepsNativeTitle = typeof navigator !== 'undefined' && /Windows/i.test(navigator.userAgent);

  function toggleShare() {
    if (pending) return;
    pending = true;
    shareClicks += 1;
    shared = !shared;
    pending = false;
  }

  function openMenu(event: MouseEvent | KeyboardEvent, source: 'pointer' | 'keyboard') {
    event.preventDefault();
    event.stopPropagation();
    menuOpens += 1;
    lastMenuInvocation = source;
  }

  function onKeyDown(event: KeyboardEvent) {
    const menuKey = event.key === 'ContextMenu' || event.key === 'Menu' || event.key === 'Apps' || event.code === 'ContextMenu';
    if ((event.key === 'F10' && event.shiftKey) || menuKey) {
      openMenu(event, 'keyboard');
    }
  }

  $effect(() => {
    document.body.dataset.hoverTabReady = 'true';
  });

  if (typeof window !== 'undefined') {
    (window as Window & { hoverTabFixture?: {
      setInset: (value: boolean) => void;
      setShared: (value: boolean) => void;
      getShareClicks: () => number;
      getMenuOpens: () => number;
      getLastMenuInvocation: () => string | null;
      getShared: () => boolean;
    } }).hoverTabFixture = {
      setInset: (value) => { inset = value; },
      setShared: (value) => { shared = value; },
      getShareClicks: () => shareClicks,
      getMenuOpens: () => menuOpens,
      getLastMenuInvocation: () => lastMenuInvocation,
      getShared: () => shared
    };
  }
</script>

<div class="hover-tab-host" class:inset={inset} class:is-shared={shared}>
  <Pill attach="right">
    <div class="hover-tab-surface">
      <button
        class="hover-tab-action hover-tab-trigger"
        class:is-shared={shared}
        class:pending
        type="button"
        onclick={toggleShare}
        oncontextmenu={(event) => openMenu(event, 'pointer')}
        onkeydown={onKeyDown}
        disabled={pending}
        aria-busy={pending}
        aria-haspopup="menu"
        aria-keyshortcuts="Shift+F10,ContextMenu"
        aria-label={shared ? 'Stop sharing. Drag vertically to move; right-click for options' : 'Share this window. Drag vertically to move; right-click for options'}
        data-allow-native-tooltip={keepsNativeTitle ? 'true' : undefined}
        title={keepsNativeTitle ? (shared ? 'Stop sharing — drag to move; right-click for options' : 'Share this window — drag to move; right-click for options') : undefined}
      >
        <span class="hover-tab-icon" aria-hidden="true">{shared ? '■' : '↗'}</span>
        {#if shared}<span class="hover-tab-live-dot" aria-hidden="true"></span>{/if}
      </button>
    </div>
  </Pill>
</div>

<style>
  .hover-tab-host { width: 40px; height: 40px; display: flex; overflow: hidden; }
  .hover-tab-host :global(.pill.attach) { width: 40px; height: 40px; max-width: 40px; padding: 0; gap: 0; overflow: hidden; border-radius: 0 12px 12px 0; }
  .hover-tab-host.inset :global(.pill.attach-right) { border-radius: 12px 0 0 12px; }
  .hover-tab-surface { width: 40px; height: 40px; display: flex; align-items: stretch; justify-content: flex-end; }
  .hover-tab-action { position: relative; flex: 0 0 40px; width: 40px; height: 40px; min-width: 40px; display: inline-flex; align-items: center; justify-content: center; padding: 0; box-sizing: border-box; border: 1px solid transparent; border-radius: 0 10px 10px 0; color: white; background: rgba(255,255,255,.12); }
  .hover-tab-action:not(.is-shared) { border-color: var(--live-bright, #7ff0a3); }
  .hover-tab-host.inset .hover-tab-action { border-radius: 10px 0 0 10px; }
  .hover-tab-action.is-shared { color: #2b071b; background: #f06cc9; }
  .hover-tab-action:focus-visible { outline: 2px solid white; outline-offset: 2px; }
  .hover-tab-icon { flex: 0 0 auto; }
  .hover-tab-live-dot { position: absolute; right: 7px; bottom: 7px; width: 6px; height: 6px; border-radius: 50%; background: currentColor; }
</style>
