<!--
  WindowPicker - in-meeting window sharing picker. It lists real shareable
  windows via `list_shareable_windows`, loads real previews through
  `capture_window_thumbnail`, and toggles the same real share path used by
  the hover-tab pill (`share_window`).
-->
<script lang="ts" module>
  import { invoke } from '@tauri-apps/api/core';
  import { COMMANDS, EVENTS, hasTauriBridge, listenUntilDestroy } from '$lib/ipc';
  import { STORAGE_KEYS } from '$lib/data/storageKeys';
  import type { ShareableWindow, ShareStateChanged } from '$lib/ipc';

  const thumbnailMemory = new Map<number, string>();
  const failedThumbnails = new Set<number>();
  // Monotonic epoch bumped on every FORCED refresh. `loadThumbnail` captures
  // the epoch before its await and only applies the result if no newer forced
  // refresh superseded it — so a forced refresh can never be clobbered by an
  // in-flight load from an older refresh, and focus/mount soft refreshes never
  // wipe a load that is still completing.
  let thumbnailEpoch = 0;
  const SKELETON_ROWS = Array.from({ length: 5 });
  // Each thumbnail is a full one-shot capture session (Windows: a fresh WGC
  // pool + capture session + up-to-3s frame wait; macOS: a `screencapture`
  // subprocess). Windows.Graphics.Capture caps concurrent capture sessions
  // OS-side, so running more than a couple in parallel doesn't speed loading
  // up -- it queues sessions behind the OS limit and makes the picker look
  // frozen while every thumbnail waits out its deadline. 2 is the
  // WGC-safe ceiling; the backend's own prewarm thread adds one more.
  const THUMBNAIL_CONCURRENCY = 2;
  const WINDOW_CACHE_KEY = STORAGE_KEYS.windowPickerSnapshot;
  const WINDOW_CACHE_TTL_MS = 5_000;

  type WindowPickerSnapshot = {
    windows: ShareableWindow[];
    sharedIds: number[];
    fetchedAt: number;
  };

  let windowMemory: WindowPickerSnapshot | null = readStoredSnapshot();
  let windowFetch: Promise<WindowPickerSnapshot> | null = null;

  export function prewarmWindowPicker(): Promise<WindowPickerSnapshot> {
    return fetchWindowSnapshot();
  }

  function cachedWindowSnapshot(): WindowPickerSnapshot | null {
    if (!windowMemory || Date.now() - windowMemory.fetchedAt > WINDOW_CACHE_TTL_MS) return null;
    return windowMemory;
  }

  function readStoredSnapshot(): WindowPickerSnapshot | null {
    if (typeof window === 'undefined') return null;
    try {
      const raw = window.localStorage.getItem(WINDOW_CACHE_KEY);
      if (!raw) return null;
      const parsed = JSON.parse(raw) as Partial<WindowPickerSnapshot>;
      if (
        !Array.isArray(parsed.windows) ||
        !Array.isArray(parsed.sharedIds) ||
        typeof parsed.fetchedAt !== 'number'
      ) {
        return null;
      }
      if (Date.now() - parsed.fetchedAt > WINDOW_CACHE_TTL_MS) return null;
      return {
        windows: parsed.windows as ShareableWindow[],
        sharedIds: parsed.sharedIds.filter((id): id is number => typeof id === 'number'),
        fetchedAt: parsed.fetchedAt
      };
    } catch {
      return null;
    }
  }

  function writeStoredSnapshot(snapshot: WindowPickerSnapshot) {
    if (typeof window === 'undefined') return;
    try {
      window.localStorage.setItem(WINDOW_CACHE_KEY, JSON.stringify(snapshot));
    } catch {
      // Cache persistence is best effort; the picker still works from live IPC.
    }
  }

  async function fetchWindowSnapshot(): Promise<WindowPickerSnapshot> {
    if (windowFetch) return windowFetch;
    windowFetch = Promise.all([
      invoke<ShareableWindow[]>(COMMANDS.listShareableWindows),
      invoke<number[]>(COMMANDS.sharedWindowIds)
    ])
      .then(([windows, sharedIds]) => {
        const snapshot = { windows, sharedIds, fetchedAt: Date.now() };
        windowMemory = snapshot;
        writeStoredSnapshot(snapshot);
        return snapshot;
      })
      .finally(() => {
        windowFetch = null;
      });
    return windowFetch;
  }

  function rememberSharedIds(sharedIds: number[]) {
    if (!windowMemory) return;
    windowMemory = { ...windowMemory, sharedIds, fetchedAt: Date.now() };
    writeStoredSnapshot(windowMemory);
  }
</script>

<script lang="ts">
  import { onMount } from 'svelte';
  import { CheckMenuItem, Menu, MenuItem, PredefinedMenuItem } from '@tauri-apps/api/menu';
  import { LogicalPosition } from '@tauri-apps/api/dpi';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import CloseButton from './CloseButton.svelte';
  import RefreshButton from './RefreshButton.svelte';
  import { identityColorCss } from '$lib/data/identityColor';
  import { isWindows } from '$lib/platform';
  import { session } from '$lib/stores/session.svelte';

  type ThumbnailState =
    | { status: 'idle' }
    | { status: 'loading' }
    | { status: 'ready'; src: string }
    | { status: 'failed' };

  interface Props {
    entryMotion?: boolean;
    /** True when hosted as its own Tauri window instead of an embedded panel. */
    standalone?: boolean;
    onChanged?: () => void;
    onClose?: () => void;
  }

  let {
    entryMotion = true,
    standalone = false,
    onChanged,
    onClose
  }: Props = $props();
  const initialSnapshot = cachedWindowSnapshot();

  let windows = $state<ShareableWindow[]>(initialSnapshot?.windows ?? []);
  let sharedIds = $state<Set<number>>(new Set(initialSnapshot?.sharedIds ?? []));
  let thumbnails = $state<Record<number, ThumbnailState>>(
    initialThumbnailState(initialSnapshot?.windows ?? [])
  );
  let loading = $state(!initialSnapshot);
  let errorTitle = $state('Could not list windows.');
  let error = $state<string | null>(null);
  let pending = $state<Set<number>>(new Set());
  let displaySettingsMenu: Menu | null = null;
  const localShareColor = $derived(identityColorCss(session.identity ?? 'plum'));

  let refreshSeq = 0;
  let disposed = false;
  let activeThumbnailLoads = 0;
  const queuedThumbnailIds = new Set<number>();
  const thumbnailQueue: ShareableWindow[] = [];

  onMount(() => {
    void refresh({ showLoading: !initialSnapshot });
    window.addEventListener('focus', refreshOnFocus);
    // Event-driven auto-refresh: the Rust window-change watcher
    // (window_change_watcher.rs) emits `desktop-windows-changed` (debounced)
    // when a desktop window was created/closed/minimized/restored and already
    // invalidated its list cache, so a SOFT refresh sees the current window
    // set without wiping in-flight thumbnail loads (same policy as the focus
    // refresh). Browser fallback has no Tauri bridge and thus no events.
    let unlistenDesktop: (() => void) | undefined;
    let unlistenShareState: (() => void) | undefined;
    let unlistenPickerOpened: (() => void) | undefined;
    if (hasTauriBridge()) {
      listenUntilDestroy(
        EVENTS.desktopWindowsChanged,
        () => refresh({ showLoading: false, force: false }),
        (unlisten) => {
          unlistenDesktop = unlisten;
        },
        () => disposed
      );
      // Shares toggled from OTHER surfaces (hover pill, meeting controls)
      // must be reflected in the open picker's chips immediately — without
      // this the grid showed stale share state until the window refocused.
      listenUntilDestroy<ShareStateChanged>(
        EVENTS.shareStateChanged,
        (event) => {
          const { windowId, shared } = event.payload;
          const next = new Set(sharedIds);
          if (shared) next.add(windowId);
          else {
            next.delete(windowId);
            void closeDisplaySettingsMenu();
          }
          sharedIds = next;
          rememberSharedIds([...next]);
        },
        (unlisten) => {
          unlistenShareState = unlisten;
        },
        () => disposed
      );
      listenUntilDestroy(
        EVENTS.sharePickerOpened,
        () => void refresh({ showLoading: false, force: false }),
        (unlisten) => {
          unlistenPickerOpened = unlisten;
        },
        () => disposed
      );
    }
    return () => {
      window.removeEventListener('focus', refreshOnFocus);
      // Set `disposed` BEFORE unlistening: if the `listen()` promise resolves
      // between the two statements, listenUntilDestroy's isDestroyed() check
      // must already see the picker as torn down (it self-unlistens then);
      // otherwise a stored unlisten is never invoked and the listener leaks
      // for the session.
      disposed = true;
      unlistenDesktop?.();
      unlistenShareState?.();
      unlistenPickerOpened?.();
      void closeDisplaySettingsMenu();
      refreshSeq += 1;
    };
  });

  function refreshOnFocus() {
    if (pending.size > 0) return;
    // Focus is a SOFT refresh: serve the thumbnail/list caches so bringing
    // the picker to the front never wipes in-flight thumbnail loads. Only
    // the explicit refresh button forces a re-capture (force: true).
    void refresh({ showLoading: windows.length === 0, force: false });
  }

  async function refresh({
    showLoading = true,
    force = false
  }: { showLoading?: boolean; force?: boolean } = {}) {
    const seq = ++refreshSeq;
    if (showLoading) loading = true;
    errorTitle = 'Could not list windows.';
    error = null;
    // A forced refresh (explicit refresh button) must show CURRENT window
    // content: bump the thumbnail epoch so in-flight loads from older
    // refreshes are discarded, drop the frontend thumbnail cache so every
    // window re-requests, and pass `force` through so the backend bypasses
    // its own 8s TTL cache too. Mount-time and focus refreshes keep
    // `force: false` so they serve the caches and never wipe in-flight
    // loads.
    if (force) {
      thumbnailEpoch += 1;
      thumbnailMemory.clear();
      failedThumbnails.clear();
      // Drop queued-but-unstarted work from the previous epoch: its window
      // ids would otherwise make the re-request loop below skip those cards
      // (`requestThumbnail` dedups on queuedThumbnailIds), leaving them stuck
      // at `idle` after their in-flight loads are epoch-discarded. In-flight
      // loads are safe to leave running — their results are discarded by the
      // epoch bump and their `finally` only deletes a (now irrelevant) set
      // entry.
      queuedThumbnailIds.clear();
      thumbnailQueue.length = 0;
    }
    try {
      const snapshot = await fetchWindowSnapshot();
      if (disposed || seq !== refreshSeq) return;
      windows = snapshot.windows;
      sharedIds = new Set(snapshot.sharedIds);
      seedThumbnailState(snapshot.windows, force);
      // A forced refresh re-seeds every card to `idle`, but the per-card
      // IntersectionObserver fired once on mount and disconnected — it will
      // never re-request. Explicitly re-request all thumbnails so the forced
      // capture actually starts (the queue + concurrency bound it, and
      // `requestThumbnail` skips cards already ready/loading/failed).
      if (force) {
        for (const win of snapshot.windows) {
          requestThumbnail(win);
        }
      }
    } catch (e) {
      if (disposed || seq !== refreshSeq) return;
      // Raw backend error strings are developer text (Rust Display impls);
      // the user gets the friendly title + a Retry, the detail goes to the
      // log.
      console.error('list_shareable_windows failed', e);
      error = 'Could not load the window list.';
    } finally {
      if (!disposed && seq === refreshSeq) loading = false;
    }
  }

  function seedThumbnailState(list: ShareableWindow[], force = false) {
    thumbnails = initialThumbnailState(list, force);
  }

  function initialThumbnailState(list: ShareableWindow[], force = false): Record<number, ThumbnailState> {
    const next: Record<number, ThumbnailState> = {};
    for (const win of list) {
      const cached = force ? undefined : thumbnailMemory.get(win.windowId);
      if (cached) next[win.windowId] = { status: 'ready', src: cached };
      else if (failedThumbnails.has(win.windowId)) next[win.windowId] = { status: 'failed' };
      else next[win.windowId] = { status: 'idle' };
    }
    return next;
  }

  function thumbnailFor(windowId: number): ThumbnailState {
    return thumbnails[windowId] ?? { status: 'idle' };
  }

  function setThumbnail(windowId: number, state: ThumbnailState) {
    thumbnails = { ...thumbnails, [windowId]: state };
  }

  function requestThumbnail(win: ShareableWindow) {
    const cached = thumbnailMemory.get(win.windowId);
    if (cached) {
      setThumbnail(win.windowId, { status: 'ready', src: cached });
      return;
    }
    const current = thumbnailFor(win.windowId).status;
    if (current === 'ready' || current === 'loading' || current === 'failed') return;
    if (queuedThumbnailIds.has(win.windowId)) return;

    queuedThumbnailIds.add(win.windowId);
    thumbnailQueue.push(win);
    setThumbnail(win.windowId, { status: 'loading' });
    drainThumbnailQueue();
  }

  function drainThumbnailQueue() {
    while (activeThumbnailLoads < THUMBNAIL_CONCURRENCY && thumbnailQueue.length > 0) {
      const win = thumbnailQueue.shift()!;
      activeThumbnailLoads += 1;
      void loadThumbnail(win).finally(() => {
        activeThumbnailLoads -= 1;
        drainThumbnailQueue();
      });
    }
  }

  async function loadThumbnail(win: ShareableWindow) {
    // Capture the epoch this load belongs to; if a newer forced refresh runs
    // while we await, our result is stale and must not clobber the newer
    // state (the newer refresh re-requests every window anyway).
    const epoch = thumbnailEpoch;
    try {
      // Force (bypass the backend 8s TTL) only when this window's thumbnail
      // is NOT already in the frontend cache — i.e. a re-request after a
      // forced refresh cleared it, or a genuinely new window. Soft refreshes
      // after the forced wave repopulated the cache serve from memory and
      // never reach the backend, so they don't force.
      const force = epoch > 0 && !thumbnailMemory.has(win.windowId);
      const base64 = await invoke<string>(COMMANDS.captureWindowThumbnail, {
        windowId: win.windowId,
        force
      });
      if (epoch !== thumbnailEpoch) return;
      const src = base64.startsWith('data:') ? base64 : `data:image/jpeg;base64,${base64}`;
      thumbnailMemory.set(win.windowId, src);
      failedThumbnails.delete(win.windowId);
      if (!disposed) setThumbnail(win.windowId, { status: 'ready', src });
    } catch (e) {
      if (epoch !== thumbnailEpoch) return;
      failedThumbnails.add(win.windowId);
      if (!disposed) setThumbnail(win.windowId, { status: 'failed' });
      console.debug(`capture_window_thumbnail(${win.windowId}) failed`, e);
    } finally {
      queuedThumbnailIds.delete(win.windowId);
    }
  }

  function lazyThumbnail(node: HTMLElement, win: ShareableWindow) {
    let observer: IntersectionObserver | undefined;
    const start = () => requestThumbnail(win);

    if ('IntersectionObserver' in window) {
      observer = new IntersectionObserver(
        (entries) => {
          if (entries.some((entry) => entry.isIntersecting)) {
            start();
            observer?.disconnect();
          }
        },
        { rootMargin: '120px' }
      );
      observer.observe(node);
    } else {
      start();
    }

    return {
      update(next: ShareableWindow) {
        win = next;
      },
      destroy() {
        observer?.disconnect();
      }
    };
  }

  async function toggleShare(win: ShareableWindow) {
    if (pending.has(win.windowId)) return;
    const visibleShared = sharedIds.has(win.windowId);
    pending = new Set(pending).add(win.windowId);
    try {
      const currentSharedIds = await invoke<number[]>(COMMANDS.sharedWindowIds);
      const currentShared = currentSharedIds.includes(win.windowId);
      if (currentShared !== visibleShared) {
        sharedIds = new Set(currentSharedIds);
        rememberSharedIds(currentSharedIds);
        onChanged?.();
        return;
      }
      const nowShared = await invoke<boolean>(COMMANDS.shareWindow, {
        windowId: win.windowId,
        color: localShareColor
      });
      const next = new Set(sharedIds);
      if (nowShared) next.add(win.windowId);
      else next.delete(win.windowId);
      sharedIds = next;
      rememberSharedIds([...next]);
      onChanged?.();
      // The picker stays open after a toggle so concurrent sharing is
      // flipping switches; closing is the caller's explicit action (the
      // standalone route's CloseButton / "Done").
    } catch (e) {
      // A failed toggle (e.g. the 4-share cap) must not disrupt the picker
      // layout: the backend already emits the global `share-error` event,
      // which the root layout's ToastHost surfaces as the standard toast.
      // Never set the full-window error state here -- that would replace the
      // window grid the user is still working with.
      console.error(`share_window(${win.windowId}) failed`, e);
    } finally {
      const p = new Set(pending);
      p.delete(win.windowId);
      pending = p;
    }
  }

  async function closeDisplaySettingsMenu() {
    const menu = displaySettingsMenu;
    displaySettingsMenu = null;
    await menu?.close().catch(() => {});
  }

  async function setDisplayDraw(windowId: number, active: boolean) {
    try {
      await invoke(COMMANDS.shareOverlaySetDrawActive, { windowId, active });
    } catch (e) {
      console.error(`share_overlay_set_draw_active(${windowId}) failed`, e);
    }
  }

  async function openDisplaySettings(event: MouseEvent, win: ShareableWindow) {
    event.stopPropagation();
    if (!hasTauriBridge() || !isWindows() || win.kind !== 'display' || !sharedIds.has(win.windowId)) {
      return;
    }
    const target = event.currentTarget as HTMLElement;
    const rect = target.getBoundingClientRect();
    const active = await invoke<boolean>(COMMANDS.shareOverlayDrawActive, {
      windowId: win.windowId
    }).catch(() => false);
    await closeDisplaySettingsMenu();
    const fullControl = await MenuItem.new({
      text: 'Full control enabled',
      enabled: false
    });
    const draw = await CheckMenuItem.new({
      id: `display-draw-${win.windowId}`,
      text: active ? 'Stop drawing on display' : 'Draw on shared display',
      checked: active,
      action: () => void setDisplayDraw(win.windowId, !active)
    });
    const menu = await Menu.new({
      items: [fullControl, await PredefinedMenuItem.new({ item: 'Separator' }), draw]
    });
    displaySettingsMenu = menu;
    try {
      await menu.popup(new LogicalPosition(rect.left, rect.bottom), getCurrentWindow());
    } finally {
      displaySettingsMenu = null;
      await menu.close();
      await Promise.all([fullControl.close(), draw.close()]);
    }
  }

  function label(win: ShareableWindow): string {
    const t = win.title?.trim();
    if (t && t.length > 0) return t;
    return win.appName;
  }

  const headerDragRegion = $derived(standalone ? '' : undefined);
</script>

<section class="picker" class:no-entry-motion={!entryMotion} aria-label="Share a window">
  {#if onClose}
    <header class="picker-head" data-tauri-drag-region={headerDragRegion}>
      <div class="title-stack" data-tauri-drag-region={headerDragRegion}>
        <span class="eyebrow" data-tauri-drag-region={headerDragRegion}>Share</span>
        <h2 data-tauri-drag-region={headerDragRegion}>Choose a window</h2>
      </div>
      <RefreshButton
        ariaLabel="Refresh window list"
        disabled={loading || pending.size > 0}
        onclick={() => refresh({ showLoading: false, force: true })}
      />
      <CloseButton onclick={() => onClose?.()} />
    </header>
  {/if}
  {#if loading}
    <ul class="window-grid loading-grid" aria-label="Loading windows">
      {#each SKELETON_ROWS as _, i (i)}
        <li class="skeleton-card">
          <span class="skeleton-preview"></span>
          <span class="skeleton-copy">
            <span></span>
            <span></span>
          </span>
          <span class="skeleton-action"></span>
        </li>
      {/each}
    </ul>
  {:else if error}
    <div class="picker-state error">
      <p class="state-title">{errorTitle}</p>
      <p class="state-detail">{error}</p>
      <button type="button" class="retry" onclick={() => refresh()}>Try again</button>
    </div>
  {:else if windows.length === 0}
    <div class="picker-state">
      <p class="state-title">No shareable windows found.</p>
      <p class="state-detail">Open a window on this desktop and try again.</p>
    </div>
  {:else}
    <ul class="window-grid" aria-label="Shareable windows">
      {#each windows as win (win.windowId)}
        {@const isShared = sharedIds.has(win.windowId)}
        {@const thumb = thumbnailFor(win.windowId)}
        <li use:lazyThumbnail={win}>
          <div class="window-card-shell">
            <button
              type="button"
              class="window-card"
              class:shared={isShared}
              class:has-settings={isShared && win.kind === 'display' && isWindows()}
              class:pending={pending.has(win.windowId)}
              aria-pressed={isShared}
              disabled={pending.has(win.windowId)}
              onclick={() => toggleShare(win)}
            >
            <span class="preview" class:loading={thumb.status === 'idle' || thumb.status === 'loading'}>
              {#if thumb.status === 'ready'}
                <img class="preview-image" src={thumb.src} alt="" loading="lazy" />
              {:else if thumb.status === 'failed' && win.appIconBase64}
                <span class="preview-fallback">
                  <img class="fallback-icon" src={win.appIconBase64} alt="" />
                </span>
              {:else if thumb.status === 'failed'}
                <span class="preview-fallback app-placeholder" aria-hidden="true">{win.appName.slice(0, 1)}</span>
              {:else}
                <span class="preview-skeleton"></span>
              {/if}
            </span>
            <span class="window-meta">
              <span class="app-line">
                {#if win.appIconBase64}
                  <img class="app-icon" src={win.appIconBase64} alt="" />
                {:else}
                  <span class="app-icon placeholder" aria-hidden="true">{win.appName.slice(0, 1)}</span>
                {/if}
                <span class="app-name">{win.appName}</span>
              </span>
              <span class="window-title">{label(win)}</span>
            </span>
              <span class="action-chip">{isShared ? 'Stop sharing' : 'Share'}</span>
            </button>
            {#if isShared && win.kind === 'display' && isWindows()}
              <button
                type="button"
                class="display-settings"
                aria-label={`Open sharing settings for ${label(win)}`}
                title="Sharing settings"
                onclick={(event) => void openDisplaySettings(event, win)}
              >
                <svg aria-hidden="true" focusable="false" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <circle cx="12" cy="12" r="3"></circle>
                  <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1-1.5 1.7 1.7 0 0 0-1.9.3l-.1.1A2 2 0 1 1 4.2 17l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1 1.7 1.7 0 0 0-.3-1.9l-.1-.1A2 2 0 1 1 7 4.2l.1.1a1.7 1.7 0 0 0 1.9.3 1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1A2 2 0 1 1 19.8 7l-.1.1a1.7 1.7 0 0 0-.3 1.9 1.7 1.7 0 0 0-1.5 1Z"></path>
                </svg>
              </button>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .picker {
    min-height: 100%;
    height: 100%;
    width: 100%;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    overscroll-behavior: none;
    background:
      linear-gradient(180deg, var(--fill-weak), transparent 180px),
      var(--surface);
    color: var(--text-primary);
    font-family: var(--font-ui);
  }

  .picker-head {
    display: flex;
    align-items: center;
    gap: 16px;
    min-height: 58px;
    padding: 13px 14px 13px 18px;
    border-bottom: 1px solid var(--hairline);
    flex-shrink: 0;
    background: var(--fill-weak);
  }

  .title-stack {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin-right: auto;
  }

  .eyebrow {
    font: 700 10px var(--font-mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-faint);
  }

  h2 {
    margin: 0;
    color: var(--text-primary);
    font: 700 14px var(--font-ui);
    letter-spacing: 0;
    text-wrap: balance;
  }

  .window-grid {
    list-style: none;
    margin: 0;
    padding: 18px;
    overflow-y: auto;
    min-height: 0;
    flex: 1;
    overscroll-behavior: none;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(210px, 1fr));
    align-content: start;
    gap: 14px;
  }

  .window-grid > li {
    min-width: 0;
    animation: picker-row-in var(--motion-enter) var(--ease-standard) both;
  }

  .window-grid.loading-grid {
    align-content: stretch;
    grid-auto-rows: minmax(224px, 1fr);
  }

  .window-grid.loading-grid > li {
    min-height: 0;
  }

  .picker.no-entry-motion .window-grid > li {
    animation: none;
  }

  .window-card-shell {
    position: relative;
    min-width: 0;
  }

  .window-card,
  .skeleton-card {
    min-height: 224px;
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 10px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-tile);
    background: var(--fill-weak);
    color: inherit;
    text-align: left;
    box-shadow:
      0 14px 34px rgba(0, 0, 0, 0.16),
      inset 0 1px 0 var(--fill-weak);
    box-sizing: border-box;
  }

  .window-card {
    cursor: pointer;
    transition:
      background var(--motion-fast) var(--ease-standard),
      border-color var(--motion-fast) var(--ease-standard),
      box-shadow var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard),
      opacity var(--motion-fast) var(--ease-standard);
  }

  .window-card:hover {
    border-color: var(--hairline-strong);
    background: var(--fill-base);
    box-shadow:
      0 18px 44px rgba(0, 0, 0, 0.2),
      inset 0 1px 0 var(--fill-base);
  }

  .window-card:active {
    transform: scale(var(--press-scale));
  }

  .window-card:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .window-card.shared {
    background: rgba(52, 199, 89, 0.08);
    border-color: rgba(127, 240, 163, 0.24);
  }

  .window-card.has-settings {
    padding-right: 54px;
  }

  .display-settings {
    position: absolute;
    right: 10px;
    bottom: 10px;
    width: 32px;
    height: 32px;
    display: inline-grid;
    place-items: center;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-chip);
    background: var(--fill-strong);
    color: var(--text-strong);
    cursor: pointer;
  }

  .display-settings:hover {
    background: var(--fill-base);
  }

  .display-settings:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .window-card.pending {
    opacity: 0.62;
    cursor: wait;
  }

  .window-card:disabled {
    color: inherit;
  }

  .preview,
  .skeleton-preview {
    width: 100%;
    aspect-ratio: 16 / 9;
    display: grid;
    place-items: center;
    overflow: hidden;
    flex-shrink: 0;
    border-radius: calc(var(--radius-tile) - 2px);
    background: var(--bg-base-2);
    box-shadow:
      inset 0 0 0 1px var(--hairline-strong),
      0 12px 28px -22px rgba(0, 0, 0, 0.9);
  }

  .preview-image {
    width: 100%;
    height: 100%;
    object-fit: cover;
    outline: 1px solid var(--hairline-strong);
    outline-offset: -1px;
  }

  .preview-skeleton,
  .skeleton-preview,
  .skeleton-copy span,
  .skeleton-action {
    position: relative;
    overflow: hidden;
    background: var(--fill-base);
  }

  .preview-skeleton {
    width: 100%;
    height: 100%;
  }

  .preview-skeleton::after,
  .skeleton-preview::after,
  .skeleton-copy span::after,
  .skeleton-action::after {
    content: '';
    position: absolute;
    inset: 0;
    transform: translateX(-100%);
    background: linear-gradient(90deg, transparent, var(--fill-strong), transparent);
    animation: shimmer 1.2s var(--ease-standard) infinite;
  }

  .preview-fallback {
    width: 100%;
    height: 100%;
    display: grid;
    place-items: center;
    background:
      linear-gradient(180deg, var(--fill-weak), var(--fill-weak)),
      var(--bg-base-2);
  }

  .fallback-icon,
  .app-icon {
    width: 28px;
    height: 28px;
    border-radius: var(--radius-chip);
    object-fit: contain;
    outline: 1px solid var(--hairline-strong);
    outline-offset: -1px;
  }

  .app-placeholder {
    color: var(--text-soft);
    font: 700 20px var(--font-ui);
  }

  .window-meta {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
    flex: 1;
  }

  .app-line {
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .app-icon {
    width: 20px;
    height: 20px;
    flex-shrink: 0;
    border-radius: var(--radius-badge);
  }

  .app-icon.placeholder {
    display: grid;
    place-items: center;
    background: var(--fill-strong);
    color: var(--text-muted);
    font: 700 10px var(--font-ui);
  }

  .app-name {
    min-width: 0;
    color: var(--text-muted);
    font: 600 11.5px / 1.25 var(--font-ui);
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .window-title {
    min-width: 0;
    color: var(--text-strong);
    font: 700 13px / 1.25 var(--font-ui);
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .action-chip {
    align-self: flex-start;
    min-width: 64px;
    height: 32px;
    margin-top: auto;
    padding: 0 12px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-chip);
    background: var(--fill-strong);
    color: var(--text-strong);
    font: 700 11.5px var(--font-ui);
  }

  .window-card.shared .action-chip {
    background: rgba(52, 199, 89, 0.16);
    color: var(--live-bright);
  }

  .skeleton-copy {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .skeleton-copy span {
    display: block;
    height: 12px;
    border-radius: var(--radius-pill);
  }

  .skeleton-copy span:first-child {
    width: 45%;
  }

  .skeleton-copy span:last-child {
    width: 76%;
  }

  .skeleton-action {
    width: 86px;
    height: 32px;
    margin-top: auto;
    border-radius: var(--radius-chip);
  }

  .picker-state {
    flex: 1;
    min-height: 280px;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    padding: 32px 24px;
    text-align: center;
  }

  .state-title {
    margin: 0;
    color: var(--text-primary);
    font: 700 13px var(--font-ui);
    text-wrap: balance;
  }

  .state-detail {
    max-width: 440px;
    margin: 0;
    color: var(--text-muted);
    font: 500 12px/1.45 var(--font-ui);
    text-wrap: pretty;
  }

  .picker-state.error .state-title {
    color: var(--warning);
  }

  .retry {
    min-height: 40px;
    margin-top: 8px;
    padding: 0 14px;
    border: 1px solid var(--hairline-strong);
    /* A button, not a tile — control radius (pre-sweep value). */
    border-radius: var(--radius-control);
    background: var(--fill-strong);
    color: var(--text-primary);
    cursor: pointer;
    font: 700 12.5px var(--font-ui);
    transition:
      background var(--motion-fast) var(--ease-standard),
      border-color var(--motion-fast) var(--ease-standard),
      transform var(--motion-fast) var(--ease-standard);
  }

  .retry:hover {
    background: var(--fill-bright);
  }

  .retry:active {
    transform: scale(var(--press-scale));
  }

  .retry:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  @keyframes picker-row-in {
    from {
      opacity: 0;
      transform: translateY(var(--motion-distance));
    }
  }

  @keyframes shimmer {
    100% {
      transform: translateX(100%);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .preview-skeleton::after,
    .skeleton-preview::after,
    .skeleton-copy span::after,
    .skeleton-action::after {
      animation: none;
    }

    .window-grid > li {
      animation: none;
    }
  }

  @media (max-width: 620px) {
    .window-grid {
      grid-template-columns: 1fr;
      padding: 12px;
    }

    .window-card,
    .skeleton-card {
      min-height: 0;
    }
  }
</style>
