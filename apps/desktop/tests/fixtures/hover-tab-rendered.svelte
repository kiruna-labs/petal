<script lang="ts">
  import Pill from '@petal/shared/ui/components/Pill.svelte';

  type HoverTabSide = 'top' | 'right' | 'bottom' | 'left';

  let shared = $state(false);
  let inset = $state(false);
  let side = $state<HoverTabSide>('right');
  let pending = $state(false);
  let shareClicks = $state(0);
  let menuOpens = $state(0);
  let lastMenuInvocation = $state<'pointer' | 'keyboard' | null>(null);
  const keepsNativeTitle = typeof navigator !== 'undefined' && /Windows/i.test(navigator.userAgent);
  const dragInstruction = 'around the window border';

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
      setSide: (value: HoverTabSide) => void;
      setShared: (value: boolean) => void;
      getShareClicks: () => number;
      getMenuOpens: () => number;
      getLastMenuInvocation: () => string | null;
      getShared: () => boolean;
    } }).hoverTabFixture = {
      setInset: (value) => { inset = value; },
      setSide: (value) => { side = value; },
      setShared: (value) => { shared = value; },
      getShareClicks: () => shareClicks,
      getMenuOpens: () => menuOpens,
      getLastMenuInvocation: () => lastMenuInvocation,
      getShared: () => shared
    };
  }
</script>

<div
  class="hover-tab-host"
  class:inset={inset}
  class:side-top={side === 'top'}
  class:side-right={side === 'right'}
  class:side-bottom={side === 'bottom'}
  class:side-left={side === 'left'}
  class:is-shared={shared}
>
  <Pill attach={side}>
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
        aria-label={shared ? `Stop sharing. Drag ${dragInstruction} to move; right-click for options` : `Share this window. Drag ${dragInstruction} to move; right-click for options`}
        data-allow-native-tooltip={keepsNativeTitle ? 'true' : undefined}
        title={keepsNativeTitle ? (shared ? `Stop sharing — drag ${dragInstruction} to move; right-click for options` : `Share this window — drag ${dragInstruction} to move; right-click for options`) : undefined}
      >
        <span class="hover-tab-icon" aria-hidden="true">{shared ? '■' : '↗'}</span>
        {#if shared}<span class="hover-tab-live-dot" aria-hidden="true"></span>{/if}
      </button>
    </div>
  </Pill>
</div>

<style>
  .hover-tab-host { width: 40px; height: 40px; display: flex; position: relative; overflow: hidden; }
  .hover-tab-host::after { content: ''; position: absolute; inset: 0; z-index: 3; border: 1px solid transparent; border-radius: 0 12px 12px 0; pointer-events: none; }
  .hover-tab-host:not(.is-shared)::after { border-color: var(--live-bright, #7ff0a3); }
  .hover-tab-host.side-top:not(.inset)::after { border-radius: 12px 12px 0 0; }
  .hover-tab-host.side-top.inset::after { border-radius: 0 0 12px 12px; }
  .hover-tab-host.side-right:not(.inset)::after { border-radius: 0 12px 12px 0; }
  .hover-tab-host.side-right.inset::after { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-bottom:not(.inset)::after { border-radius: 0 0 12px 12px; }
  .hover-tab-host.side-bottom.inset::after { border-radius: 12px 12px 0 0; }
  .hover-tab-host.side-left:not(.inset)::after { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-left.inset::after { border-radius: 0 12px 12px 0; }
  .hover-tab-host :global(.pill.attach) { width: 40px; height: 40px; max-width: 40px; padding: 0; gap: 0; overflow: hidden; border-radius: 0 12px 12px 0; }
  .hover-tab-host.inset :global(.pill.attach-right) { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-top:not(.inset) :global(.pill.attach) { border-radius: 12px 12px 0 0; }
  .hover-tab-host.side-top.inset :global(.pill.attach) { border-radius: 0 0 12px 12px; }
  .hover-tab-host.side-right:not(.inset) :global(.pill.attach) { border-radius: 0 12px 12px 0; }
  .hover-tab-host.side-right.inset :global(.pill.attach) { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-bottom:not(.inset) :global(.pill.attach) { border-radius: 0 0 12px 12px; }
  .hover-tab-host.side-bottom.inset :global(.pill.attach) { border-radius: 12px 12px 0 0; }
  .hover-tab-host.side-left:not(.inset) :global(.pill.attach) { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-left.inset :global(.pill.attach) { border-radius: 0 12px 12px 0; }
  .hover-tab-surface { width: 40px; height: 40px; display: flex; align-items: stretch; justify-content: flex-end; }
  .hover-tab-action { position: relative; flex: 0 0 40px; width: 40px; height: 40px; min-width: 40px; display: inline-flex; align-items: center; justify-content: center; padding: 0; box-sizing: border-box; border: 1px solid transparent; border-radius: 0 12px 12px 0; color: white; background: rgba(255,255,255,.12); }
  .hover-tab-action:not(.is-shared) { border-color: transparent; }
  .hover-tab-host.inset .hover-tab-action { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-top:not(.inset) .hover-tab-action { border-radius: 12px 12px 0 0; }
  .hover-tab-host.side-top.inset .hover-tab-action { border-radius: 0 0 12px 12px; }
  .hover-tab-host.side-right:not(.inset) .hover-tab-action { border-radius: 0 12px 12px 0; }
  .hover-tab-host.side-right.inset .hover-tab-action { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-bottom:not(.inset) .hover-tab-action { border-radius: 0 0 12px 12px; }
  .hover-tab-host.side-bottom.inset .hover-tab-action { border-radius: 12px 12px 0 0; }
  .hover-tab-host.side-left:not(.inset) .hover-tab-action { border-radius: 12px 0 0 12px; }
  .hover-tab-host.side-left.inset .hover-tab-action { border-radius: 0 12px 12px 0; }
  .hover-tab-action.is-shared { color: #2b071b; background: #f06cc9; }
  .hover-tab-action:active:not(:disabled) { transform: scale(0.96); }
  .hover-tab-action:focus-visible { outline: 2px solid white; outline-offset: 2px; }
  .hover-tab-icon { flex: 0 0 auto; }
  .hover-tab-live-dot { position: absolute; right: 7px; bottom: 7px; width: 6px; height: 6px; border-radius: 50%; background: currentColor; }
  .hover-tab-host.side-top .hover-tab-live-dot { top: 7px; bottom: auto; }
  .hover-tab-host.side-left .hover-tab-live-dot { left: 7px; right: auto; }
</style>
