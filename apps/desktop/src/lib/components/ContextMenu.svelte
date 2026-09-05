<script lang="ts">
  // Custom right-click menu replacing the webview engine's default on both
  // platforms: WebView2's browser chrome (Search with Bing, Reload, Save as,
  // Print, Inspect) and WKWebView's menu. Petal is a desktop app, so the
  // menu is the same Petal surface everywhere -- editing actions only.
  // (macOS Services/Look Up stay reachable via the app menu bar.)
  //
  // Editing actions run against the element right-clicked, whose focus and
  // selection are preserved while the menu is open (mousedown preventDefault
  // on the menu container stops the click from blurring the input). Clipboard
  // ops use document.execCommand('copy'|'cut'|'paste'), which operate on the
  // focused element's selection and work inside a user gesture in both
  // engines (WebView2 and WKWebView). Non-editable surfaces with a live text
  // selection get a bare Copy; empty chrome surfaces swallow the engine menu
  // and show nothing.
  import { onMount } from 'svelte';
  import { tick } from 'svelte';
  import { isMac } from '$lib/platform';
  import { installDismissibleLayer } from '@petal/shared/ui/dismissibleLayer';

  interface MenuItem {
    label: string;
    shortcut: string;
    enabled: boolean;
    onSelect: () => void;
  }

  interface EditableSelection {
    el: HTMLInputElement | HTMLTextAreaElement;
    start: number;
    end: number;
  }

  let open = $state(false);
  let x = $state(0);
  let y = $state(0);
  let items = $state<MenuItem[]>([]);
  let menuEl = $state<HTMLElement | null>(null);
  let editableSel = $state<EditableSelection | null>(null);
  /** The element focused when the menu opened; focus returns to it on
   * close (for the editable case it is the field the menu edits). */
  let openerEl = $state<HTMLElement | null>(null);

  const mod = isMac() ? '⌘' : 'Ctrl+';

  function close() {
    open = false;
    items = [];
    editableSel = null;
    if (openerEl?.isConnected) openerEl.focus();
    openerEl = null;
  }

  function onMenuMouseDown(event: MouseEvent) {
    // Keep focus in the editable the menu was opened on so cut/paste/select
    // can still act on it after the menu button is clicked.
    event.preventDefault();
  }

  function copySelection() {
    if (editableSel) {
      const { el, start, end } = editableSel;
      el.focus();
      el.setSelectionRange(start, end);
    }
    document.execCommand('copy');
  }

  function cutEditable() {
    if (!editableSel) return;
    const { el, start, end } = editableSel;
    el.focus();
    el.setSelectionRange(start, end);
    if (!document.execCommand('cut')) {
      // Same engine-refusal fallback as paste: copy the cut text, then
      // remove it manually and dispatch 'input' for Svelte bindings.
      const text = el.value.slice(start, end);
      void navigator.clipboard
        .writeText(text)
        .then(() => {
          el.setRangeText('', start, end, 'end');
          el.dispatchEvent(new Event('input', { bubbles: true }));
        })
        .catch(() => {});
    }
  }

  function pasteEditable() {
    if (!editableSel) return;
    const { el, start, end } = editableSel;
    el.focus();
    el.setSelectionRange(start, end);
    if (!document.execCommand('paste')) {
      // execCommand('paste') needs a real OS clipboard path (fine in
      // WebView2); fall back to the async API so paste still works where
      // the engine refuses (e.g. sandboxed preview). Dispatch 'input' to
      // keep Svelte bindings in sync.
      void navigator.clipboard
        .readText()
        .then((text) => {
          el.setRangeText(text, start, end, 'end');
          el.dispatchEvent(new Event('input', { bubbles: true }));
        })
        .catch(() => {});
    }
  }

  function selectAllEditable() {
    if (!editableSel) return;
    const { el } = editableSel;
    el.focus();
    el.select();
  }

  function buildItems(target: Element | null): MenuItem[] {
    const editable = target?.closest<HTMLInputElement | HTMLTextAreaElement>('input, textarea');
    if (editable) {
      const start = editable.selectionStart ?? editable.value.length;
      const end = editable.selectionEnd ?? editable.value.length;
      const hasSelection = end > start;
      editableSel = { el: editable, start, end };
      return [
        { label: 'Cut', shortcut: `${mod}X`, enabled: hasSelection, onSelect: cutEditable },
        { label: 'Copy', shortcut: `${mod}C`, enabled: hasSelection, onSelect: copySelection },
        { label: 'Paste', shortcut: `${mod}V`, enabled: true, onSelect: pasteEditable },
        { label: 'Select all', shortcut: `${mod}A`, enabled: true, onSelect: selectAllEditable }
      ];
    }
    editableSel = null;
    const selection = window.getSelection();
    if (selection && !selection.isCollapsed && selection.toString().trim()) {
      return [{ label: 'Copy', shortcut: `${mod}C`, enabled: true, onSelect: copySelection }];
    }
    return [];
  }

  function onSelect(item: MenuItem) {
    item.onSelect();
    // Native menus dismiss after an action; do the same so the menu never
    // lingers over the surface it just edited.
    close();
  }

  function onContextMenu(event: MouseEvent) {
    // Swallow the engine's default menu unconditionally — even where we show
    // nothing, the default menu must not appear.
    event.preventDefault();
    const built = buildItems(event.target as Element | null);
    if (!built.length) {
      close();
      return;
    }
    items = built;
    open = true;
    x = event.clientX;
    y = event.clientY;
    openerEl = document.activeElement as HTMLElement | null;
    void clampPosition();
  }

  async function clampPosition() {
    await tick();
    const el = menuEl;
    if (!el || !open) return;
    const rect = el.getBoundingClientRect();
    x = Math.min(x, Math.max(0, window.innerWidth - rect.width - 8));
    y = Math.min(y, Math.max(0, window.innerHeight - rect.height - 8));
    // Keyboard operability: once placed, move focus to the first item so
    // ArrowUp/Down/Tab operate the menu (role=menu) instead of the page.
    (el.querySelector<HTMLElement>('button:not(:disabled)'))?.focus();
  }

  function onKeyDown(event: KeyboardEvent) {
    if (event.key === 'Escape' && open) {
      event.preventDefault();
      close();
      return;
    }
    if (!open || (event.key !== 'ArrowDown' && event.key !== 'ArrowUp')) return;
    const buttons = Array.from(
      menuEl?.querySelectorAll<HTMLElement>('button:not(:disabled)') ?? []
    );
    if (buttons.length === 0) return;
    event.preventDefault();
    const current = buttons.indexOf(document.activeElement as HTMLElement);
    const delta = event.key === 'ArrowDown' ? 1 : -1;
    const next = (current + delta + buttons.length) % buttons.length;
    buttons[next].focus();
  }

  onMount(() => {
    window.addEventListener('contextmenu', onContextMenu);
    const cleanupDismissibleLayer = installDismissibleLayer({
      isOpen: () => open,
      getInsideNodes: () => [menuEl],
      getPopupNodes: () => [menuEl],
      getOpener: () => openerEl,
      onDismiss: close
    });
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener('scroll', close, true);
    window.addEventListener('resize', close);
    window.addEventListener('blur', close);
    return () => {
      window.removeEventListener('contextmenu', onContextMenu);
      cleanupDismissibleLayer();
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('scroll', close, true);
      window.removeEventListener('resize', close);
      window.removeEventListener('blur', close);
    };
  });
</script>

{#if open}
  <div
    class="context-menu"
    bind:this={menuEl}
    role="menu"
    tabindex="-1"
    style:left="{x}px"
    style:top="{y}px"
    onmousedown={onMenuMouseDown}
  >
    {#each items as item}
      <button
        type="button"
        class="context-menu-item"
        class:disabled={!item.enabled}
        role="menuitem"
        disabled={!item.enabled}
        onclick={() => onSelect(item)}
      >
        <span class="context-menu-label">{item.label}</span>
        <span class="context-menu-shortcut">{item.shortcut}</span>
      </button>
    {/each}
  </div>
{/if}

<style>
  .context-menu {
    position: fixed;
    z-index: 100;
    min-width: 180px;
    max-width: min(360px, calc(100vw - 16px));
    /* 6px padding matches the profile-menu popover pairing so item corners
       nest concentrically inside the 14px container (14 - 6 = 8 = chip). */
    padding: 6px;
    box-sizing: border-box;
    background: var(--popover-bg);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-popover);
    box-shadow: var(--shadow-float), var(--shadow-inset-hairline);
    user-select: none;
  }

  .context-menu-item {
    display: flex;
    align-items: flex-start;
    width: 100%;
    min-height: 40px;
    padding: 7px 10px;
    border: 0;
    border-radius: var(--radius-chip);
    background: transparent;
    color: var(--text-soft);
    font: 600 12px var(--font-ui);
    text-align: left;
    cursor: pointer;
  }

  .context-menu-item:hover:not(:disabled),
  .context-menu-item:focus-visible {
    background: var(--fill-strong);
    color: var(--text-primary);
  }

  .context-menu-item:disabled {
    color: var(--text-faint);
    cursor: default;
  }

  .context-menu-label {
    flex: 1 1 auto;
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .context-menu-shortcut {
    flex: 0 0 auto;
    margin-left: 24px;
    font: 500 11px var(--font-mono);
    color: var(--text-faint);
  }
</style>
