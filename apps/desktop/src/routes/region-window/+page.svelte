<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { invoke } from '@tauri-apps/api/core';
  import {
    COMMANDS,
    EVENTS,
    type AiChatEndReason,
    type AiChatStartOutcome,
    type RegionControlStateChanged,
    type RegionViewOptionsChanged,
    type RegionViewOptionsState,
    type SharePriority
  } from '$lib/ipc';
  import { cursorPosition, getCurrentWindow } from '@tauri-apps/api/window';
  import { fly, fade } from 'svelte/transition';
  import CloseButton from '$lib/components/CloseButton.svelte';
  import { enterDuration, exitDuration } from '$lib/motion';
  import { session } from '$lib/stores/session.svelte';
  import { identityColorCss, identityInkCss } from '$lib/data/identityColor';
  import { aiChatEndReasonMessage } from '$lib/data/aiChat';
  import { buildShareOptionsMenuEntries } from '$lib/data/shareOptionsMenu';
  import { popupShareOptionsMenu } from '$lib/shareOptionsPopup';

  const hasTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
  const appWindow = hasTauri ? getCurrentWindow() : null;
  const regionWindowLabel = appWindow?.label ?? '';
  let clickThroughTimer: ReturnType<typeof setInterval> | undefined;
  let regionFrameSyncTimer: ReturnType<typeof setTimeout> | undefined;
  let regionFrameSyncInFlight = false;
  let regionFrameSyncQueued = false;
  let dragging = false;
  let dragTimer: ReturnType<typeof setTimeout> | undefined;
  const EDGE_ZONE = 12;
  const INITIAL_DPR = typeof window === 'undefined' ? 1 : window.devicePixelRatio || 1;
  const FRAME_INSET = 6;
  const TITLE_BAR_MIN_HEIGHT = 56;
  type PhysicalPositionLike = { x: number; y: number };
  type PhysicalSizeLike = { width: number; height: number };
  let deviceScaleFactor = INITIAL_DPR;
  let edgeZonePx = EDGE_ZONE * INITIAL_DPR;
  let cachedWindowPosition: PhysicalPositionLike | undefined;
  let cachedWindowSize: PhysicalSizeLike | undefined;
  let geometryRevision = 0;
  let titleBar: HTMLDivElement | undefined;
  let titleBottomPx = (FRAME_INSET + TITLE_BAR_MIN_HEIGHT) * INITIAL_DPR;
  let titleBarResizeObserver: ResizeObserver | undefined;
  let ignoreState = false;
  let clickThroughPollInFlight = false;
  let disposed = false;
  let placementActive = $state(
    typeof window !== 'undefined' && new URLSearchParams(window.location.search).get('placing') === '1'
  );
  let placementSettlementPending = $state(false);
  let placementStateVersion = $state(0);
  let placementMouseDown = false;
  let placementMouseUpSeen = false;
  let windowTitle = $state('Petal View');
  let shareActive = $state(false);
  let sharePending = $state(false);
  let shareStateVersion = $state(0);
  let optionsStateVersion = $state(0);
  let optionsPending = $state(false);
  let priority = $state<SharePriority>('automatic');
  let drawActive = $state(false);
  let aiChatEnabled = $state(false);
  let aiChatActive = $state(false);
  let aiChatError = $state<AiChatEndReason | null>(null);
  let controllerName = $state<string | null>(null);
  let outsideDisplay = $state(false);
  let stopWarningListener: (() => void) | undefined;
  const stopWindowListeners: Array<() => void> = [];

  function closeWindow() {
    if (appWindow && regionWindowLabel) {
      void invoke(COMMANDS.closeRegionWindow, { windowLabel: regionWindowLabel }).catch(() => {
        void appWindow.close();
      });
    } else if (appWindow) {
      void appWindow.close();
    } else {
      window.close();
    }
  }

  type RegionShareState = { active: boolean };
  type RegionShareStateEvent = { selectorLabel?: string | null; active: boolean };

  const activeRegionColor = $derived(identityColorCss(session.identity ?? 'slate'));
  const activeRegionInk = $derived(identityInkCss(session.identity ?? 'slate'));
  const controllerStatus = $derived(
    controllerName ? `Controlled by ${controllerName}` : null
  );
  const optionsLabel = $derived(
    drawActive ? 'Stop drawing on Petal View' : 'Petal View options'
  );
  const optionsTitle = $derived(
    drawActive ? optionsLabel : aiChatError ? aiChatEndReasonMessage(aiChatError) : optionsLabel
  );

  async function toggleRegionShare() {
    if (
      !appWindow ||
      sharePending ||
      placementActive ||
      placementSettlementPending ||
      !regionWindowLabel
    ) {
      return;
    }
    sharePending = true;
    const requestVersion = shareStateVersion;
    try {
      const active = await invoke<boolean>(COMMANDS.toggleRegionShare, {
        windowLabel: regionWindowLabel,
        color: activeRegionColor
      });
      if (requestVersion === shareStateVersion) shareActive = active;
    } catch {
      // Native share errors are surfaced through the meeting toast/event
      // path. Keep this control conservative and reconcile from the next
      // authoritative state event rather than guessing a new state.
    } finally {
      sharePending = false;
    }
  }

  function applyRegionOptions(next: RegionViewOptionsState) {
    shareActive = next.shareActive;
    priority = next.priority;
    drawActive = next.drawActive;
    aiChatEnabled = next.aiChatEnabled;
    aiChatActive = next.aiChatActive;
    controllerName = next.controllerName;
  }

  async function seedRegionOptionsState() {
    if (!appWindow || disposed || !regionWindowLabel) return;
    const seedVersion = optionsStateVersion;
    try {
      const state = await invoke<RegionViewOptionsState>(COMMANDS.regionViewOptionsState, {
        windowLabel: regionWindowLabel
      });
      if (!disposed && seedVersion === optionsStateVersion) applyRegionOptions(state);
    } catch {
      // Plain-browser fixtures and older debug hosts may not expose the
      // label-addressed options command; the event path will converge later.
    }
  }

  async function setRegionPriority(next: SharePriority) {
    if (!appWindow || optionsPending || !regionWindowLabel) return;
    const previous = priority;
    optionsPending = true;
    try {
      priority = await invoke<SharePriority>(COMMANDS.setRegionSharePriority, {
        windowLabel: regionWindowLabel,
        priority: next
      });
    } catch {
      priority = previous;
    } finally {
      optionsPending = false;
    }
  }

  async function setRegionDrawActive(next: boolean) {
    if (!appWindow || optionsPending || !regionWindowLabel || !shareActive) return;
    const previous = drawActive;
    optionsPending = true;
    aiChatError = null;
    try {
      drawActive = await invoke<boolean>(COMMANDS.setRegionDrawActive, {
        windowLabel: regionWindowLabel,
        active: next
      });
    } catch {
      drawActive = previous;
    } finally {
      optionsPending = false;
    }
  }

  async function toggleRegionAiChat() {
    if (!appWindow || optionsPending || !regionWindowLabel || !shareActive) return;
    optionsPending = true;
    aiChatError = null;
    try {
      if (aiChatActive) {
        aiChatActive = await invoke<boolean>(COMMANDS.regionAiChatStop, {
          windowLabel: regionWindowLabel
        });
      } else {
        const outcome = await invoke<AiChatStartOutcome>(COMMANDS.regionAiChatStart, {
          windowLabel: regionWindowLabel
        });
        aiChatActive = outcome.started;
        if (!outcome.started) aiChatError = outcome.reason ?? 'error';
      }
    } catch {
      aiChatActive = false;
      aiChatError = 'error';
    } finally {
      optionsPending = false;
    }
  }

  async function openRegionDebugCockpit() {
    try {
      await invoke(COMMANDS.openNetworkCockpitWindow);
    } catch {
      // The options button remains usable if the optional diagnostics window
      // is unavailable in a plain-browser or older debug host.
    }
  }

  async function openRegionOptionsMenu(event: MouseEvent) {
    event.stopPropagation();
    if (!appWindow || optionsPending || placementActive || placementSettlementPending) return;
    const entries = buildShareOptionsMenuEntries(
      priority,
      shareActive,
      drawActive,
      'fullControl',
      false,
      aiChatEnabled,
      aiChatActive,
      true
    );
    await popupShareOptionsMenu(entries, {
      onPriority: (value) => void setRegionPriority(value),
      onControlMode: () => {},
      onDraw: (active) => void setRegionDrawActive(active),
      onAiChat: () => void toggleRegionAiChat(),
      onDebug: () => void openRegionDebugCockpit()
    });
  }

  async function seedRegionShareState() {
    if (!appWindow || disposed || !regionWindowLabel) return;
    const seedVersion = shareStateVersion;
    try {
      const state = await invoke<RegionShareState | null>(COMMANDS.regionShareState, {
        windowLabel: regionWindowLabel
      });
      if (
        !disposed &&
        seedVersion === shareStateVersion &&
        typeof state?.active === 'boolean'
      ) {
        shareActive = state.active;
      }
    } catch {
      // A plain-browser fixture or older debug host may not expose the
      // command; the event listener and next mount provide convergence.
    }
  }

  function cacheWindowPosition(position: PhysicalPositionLike) {
    if (!Number.isFinite(position.x) || !Number.isFinite(position.y)) return;
    cachedWindowPosition = { x: position.x, y: position.y };
    geometryRevision += 1;
  }

  function scheduleRegionFrameSync() {
    if (!appWindow || disposed || !regionWindowLabel) return;
    regionFrameSyncQueued = true;
    if (regionFrameSyncInFlight || regionFrameSyncTimer) return;
    regionFrameSyncTimer = setTimeout(() => {
      regionFrameSyncTimer = undefined;
      regionFrameSyncQueued = false;
      regionFrameSyncInFlight = true;
      void invoke(COMMANDS.syncRegionWindowFrame, {
        windowLabel: regionWindowLabel
      })
        .catch(() => {})
        .finally(() => {
          regionFrameSyncInFlight = false;
          if (regionFrameSyncQueued) scheduleRegionFrameSync();
        });
    }, 0);
  }

  function cacheWindowSize(size: PhysicalSizeLike) {
    if (!Number.isFinite(size.width) || !Number.isFinite(size.height)) return;
    if (size.width <= 0 || size.height <= 0) return;
    cachedWindowSize = { width: size.width, height: size.height };
    geometryRevision += 1;
  }

  function trackUnlisten(listener: Promise<() => void>) {
    void listener
      .then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          stopWindowListeners.push(unlisten);
        }
      })
      .catch(() => {});
  }

  async function seedWindowGeometry() {
    if (!appWindow || disposed) return;
    const revisionBeforeSeed = geometryRevision;
    try {
      const [position, size] = await Promise.all([
        appWindow.outerPosition(),
        appWindow.outerSize()
      ]);
      if (disposed) return;
      // A native move/resize event may have arrived while these initial
      // queries were in flight. Never overwrite event-backed current truth
      // with that older snapshot; fill only fields no event has supplied.
      if (geometryRevision === revisionBeforeSeed) {
        cacheWindowPosition(position);
        cacheWindowSize(size);
      } else {
        if (!cachedWindowPosition) cacheWindowPosition(position);
        if (!cachedWindowSize) cacheWindowSize(size);
      }
    } catch {
      // The first native geometry query can race window creation. The move
      // and resize listeners below will populate the cache when available.
    }
  }

  async function updateClickThrough() {
    if (!appWindow || disposed || clickThroughPollInFlight) return;

    // Follow-cursor placement owns the complete native surface. In
    // particular, do not let the hollow interior become click-through while
    // the settling mouse-down is still travelling through WebView2; that
    // single click must be consumed by Petal rather than reaching the window
    // underneath.
    if (placementActive || placementSettlementPending || dragging) {
      if (!ignoreState) return;
      clickThroughPollInFlight = true;
      try {
        await appWindow.setIgnoreCursorEvents(false);
        if (!disposed) ignoreState = false;
      } catch {
        // Keep retrying while placement/drag owns the surface.
      } finally {
        clickThroughPollInFlight = false;
      }
      return;
    }

    const position = cachedWindowPosition;
    const size = cachedWindowSize;
    if (!position || !size) return;

    const pollPlacementVersion = placementStateVersion;
    clickThroughPollInFlight = true;
    try {
      // Window position and size are event-backed. Only the global cursor
      // crosses the native boundary on each tick, and the whole transaction
      // is single-flight so slow macOS IPC cannot accumulate stale batches.
      const cursor = await cursorPosition();
      if (disposed) return;
      // Placement/drag state may change while the native cursor query is in
      // flight. Re-check before deriving or applying a hit-test result so a
      // stale poll cannot turn the selector click-through in the same frame
      // that the native placement worker settles it.
      if (
        pollPlacementVersion !== placementStateVersion ||
        placementActive ||
        placementSettlementPending ||
        dragging
      ) {
        if (ignoreState) {
          await appWindow.setIgnoreCursorEvents(false);
          if (!disposed) ignoreState = false;
        }
        return;
      }
      const latestPosition = cachedWindowPosition;
      const latestSize = cachedWindowSize;
      if (!latestPosition || !latestSize) return;
      const x = cursor.x - latestPosition.x;
      const y = cursor.y - latestPosition.y;
      const onBorder =
        x <= edgeZonePx ||
        x >= latestSize.width - edgeZonePx ||
        y <= titleBottomPx ||
        y >= latestSize.height - edgeZonePx;
      const wantIgnore = !onBorder;
      // Never flip to click-through while the user is mid-drag.
      const apply = dragging ? false : wantIgnore;
      if (apply !== ignoreState) {
        await appWindow.setIgnoreCursorEvents(apply);
        if (!disposed) ignoreState = apply;
      }
    } catch {
      // Keep the cadence alive after a transient native failure. The next
      // single-flight tick can recover without accumulating requests.
    } finally {
      clickThroughPollInFlight = false;
    }
  }

  function dispose() {
    if (disposed) return;
    disposed = true;
    document.removeEventListener('mousedown', beginDrag);
    document.removeEventListener('mouseup', releaseDrag);
    if (clickThroughTimer) clearInterval(clickThroughTimer);
    clickThroughTimer = undefined;
    if (regionFrameSyncTimer) clearTimeout(regionFrameSyncTimer);
    regionFrameSyncTimer = undefined;
    regionFrameSyncQueued = false;
    if (dragTimer) clearTimeout(dragTimer);
    dragTimer = undefined;
    titleBarResizeObserver?.disconnect();
    titleBarResizeObserver = undefined;
    for (const unlisten of stopWindowListeners.splice(0)) unlisten();
    stopWarningListener?.();
    stopWarningListener = undefined;
  }

  function syncTitleBoundary() {
    const rect = titleBar?.getBoundingClientRect();
    if (rect) titleBottomPx = rect.bottom * deviceScaleFactor;
  }

  function beginDrag() {
    if (placementActive) {
      placementMouseDown = true;
      placementMouseUpSeen = false;
    }
    dragging = true;
    if (dragTimer) clearTimeout(dragTimer);
    dragTimer = setTimeout(() => (dragging = false), 2000);
  }

  function releaseDrag() {
    if (placementMouseDown || placementSettlementPending) placementMouseUpSeen = true;
    placementMouseDown = false;
    dragging = false;
    if (dragTimer) clearTimeout(dragTimer);
    if (placementSettlementPending && placementMouseUpSeen) {
      placementSettlementPending = false;
      void updateClickThrough();
    }
  }

  function handlePlacementSettled(payload: { selectorLabel?: string | null }) {
    if (payload.selectorLabel !== regionWindowLabel || !placementActive) return;
    placementActive = false;
    placementSettlementPending = true;
    placementStateVersion += 1;
    if (placementMouseUpSeen) {
      placementSettlementPending = false;
      void updateClickThrough();
    }
  }

  function handlePlacementReleased(payload: { selectorLabel?: string | null }) {
    if (payload.selectorLabel !== regionWindowLabel) return;
    placementActive = false;
    placementSettlementPending = false;
    placementMouseDown = false;
    placementStateVersion += 1;
    void updateClickThrough();
  }

  function consumePlacementPointer(event: MouseEvent) {
    if (placementActive || placementSettlementPending) event.preventDefault();
  }

  function consumePlacementContextMenu(event: MouseEvent) {
    if (placementActive || placementSettlementPending) event.preventDefault();
  }

  async function initializeWindowTracking() {
    if (!appWindow || disposed) return;
    const placementVersion = placementStateVersion;
    try {
      const active = await invoke<boolean | null>(COMMANDS.regionPlacementActive, {
        windowLabel: appWindow.label
      });
      if (placementVersion === placementStateVersion && typeof active === 'boolean') {
        placementActive = active;
      }
      if (placementActive) {
        // A newly-created native window defaults to hit-testable, but make
        // that invariant explicit in case a host restores a prior window
        // style before this route mounts.
        try {
          await appWindow.setIgnoreCursorEvents(false);
          ignoreState = false;
        } catch {
          // The placement loop remains opaque at the native boundary; the
          // next poll retries the defensive reset if it is needed.
        }
      }
    } catch {
      // The URL flag remains the safe fallback for fixture/dev hosts whose
      // command surface predates the placement-state command.
    }
    if (disposed) return;
    void seedWindowGeometry();
    clickThroughTimer = setInterval(() => void updateClickThrough(), 50);
  }

  onMount(() => {
    syncTitleBoundary();
    if (typeof ResizeObserver !== 'undefined' && titleBar) {
      titleBarResizeObserver = new ResizeObserver(syncTitleBoundary);
      titleBarResizeObserver.observe(titleBar);
    }
    if (!appWindow) return dispose;

    void appWindow
      .title()
      .then((title) => {
        if (!disposed && title.trim()) windowTitle = title;
      })
      .catch(() => {});
    document.addEventListener('mousedown', beginDrag);
    document.addEventListener('mouseup', releaseDrag);

    trackUnlisten(
      appWindow.onMoved(({ payload }) => {
        cacheWindowPosition(payload);
        scheduleRegionFrameSync();
      })
    );
    trackUnlisten(
      appWindow.onResized(({ payload }) => {
        cacheWindowSize(payload);
        scheduleRegionFrameSync();
      })
    );
    trackUnlisten(
      appWindow.onScaleChanged(({ payload }) => {
        if (Number.isFinite(payload.scaleFactor) && payload.scaleFactor > 0) {
          deviceScaleFactor = payload.scaleFactor;
          edgeZonePx = EDGE_ZONE * deviceScaleFactor;
          syncTitleBoundary();
        }
        cacheWindowSize(payload.size);
        scheduleRegionFrameSync();
      })    );
    trackUnlisten(
      listen<{ selectorLabel?: string | null }>(
        EVENTS.regionPlacementSettled,
        (event) => handlePlacementSettled(event.payload)
      )
    );
    trackUnlisten(
      listen<{ selectorLabel?: string | null }>(
        EVENTS.regionPlacementReleased,
        (event) => handlePlacementReleased(event.payload)
      )
    );
    trackUnlisten(
      listen<RegionShareStateEvent>(
        EVENTS.regionShareStateChanged,
        (event) => {
          if (event.payload.selectorLabel !== regionWindowLabel) return;
          shareStateVersion += 1;
          optionsStateVersion += 1;
          shareActive = event.payload.active;
          if (!event.payload.active) {
            sharePending = false;
            drawActive = false;
            aiChatActive = false;
            controllerName = null;
            aiChatError = null;
          }
        }
      )
    );
    trackUnlisten(
      listen<RegionViewOptionsChanged>(
        EVENTS.regionViewOptionsChanged,
        (event) => {
          if (event.payload.selectorLabel !== regionWindowLabel) return;
          optionsStateVersion += 1;
          applyRegionOptions(event.payload.state);
        }
      )
    );
    trackUnlisten(
      listen<RegionControlStateChanged>(
        EVENTS.regionControlStateChanged,
        (event) => {
          if (event.payload.selectorLabel !== regionWindowLabel) return;
          optionsStateVersion += 1;
          controllerName = event.payload.active ? event.payload.controllerName : null;
        }
      )
    );
    void seedRegionShareState();
    void seedRegionOptionsState();
    void initializeWindowTracking();
    void listen<{ windowId: number; selectorLabel?: string | null; outsideDisplay: boolean }>(
      EVENTS.regionWarning,
      (event) => {
        if (handlesRegionWarning(event.payload)) {
          outsideDisplay = event.payload.outsideDisplay;
        }
      }
    )
      .then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          stopWarningListener = unlisten;
        }
      })
      .catch(() => {});
    return dispose;
  });

  onDestroy(dispose);

  function labelFromWindowTitle(title: string): string {
    const match = title.match(/#(\d+)$/);
    return match?.[1] ?? '';
  }

  function handlesRegionWarning(payload: {
    windowId: number;
    selectorLabel?: string | null;
  }): boolean {
    // Authoritative: the backend routes by our native Tauri label. Older
    // senders (token-keyed only) fall back to the title-number heuristic.
    if (payload.selectorLabel) return payload.selectorLabel === regionWindowLabel;
    return payload.windowId === Number(labelFromWindowTitle(windowTitle));
  }
</script>

<svelte:window onkeydown={(event) => event.key === 'Escape' && closeWindow()} />

<!-- The transparent native surface must consume the placement gesture before
     hollow-interior click-through is enabled. It is intentionally an
     application-like hit-test root rather than a normal content container. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
<div
  class="window-container"
  class:shared={shareActive}
  data-placement-active={placementActive}
  data-placement-settlement-pending={placementSettlementPending}
  data-region-window-label={regionWindowLabel}
  data-region-share-pending={sharePending}
  data-region-options-pending={optionsPending}
  role="application"
  tabindex="-1"
  aria-label="Petal View region selector"
  onmousedown={consumePlacementPointer}
  oncontextmenu={consumePlacementContextMenu}
  style={`--region-active-color: ${activeRegionColor}; --region-active-ink: ${activeRegionInk}`}
>
  <div class="hollow-frame">
    <div class="title-bar" bind:this={titleBar} data-tauri-drag-region>
      <span class="title-label" data-tauri-drag-region>{windowTitle}</span>
      {#if controllerStatus}
        <span
          class="controlled-badge"
          role="status"
          aria-label={controllerStatus}
          title={controllerStatus}
        >Controlled</span>
      {/if}
      <div class="title-actions">
        <button
          class="region-options-control"
          data-region-options-control
          type="button"
          aria-label={drawActive ? 'Stop drawing on Petal View' : 'Petal View options'}
          title={optionsTitle}
          aria-haspopup={drawActive ? undefined : 'menu'}
          aria-pressed={drawActive}
          aria-busy={optionsPending}
          disabled={optionsPending || placementActive || placementSettlementPending}
          onclick={(event) => {
            event.stopPropagation();
            if (drawActive) {
              void setRegionDrawActive(false);
            } else {
              void openRegionOptionsMenu(event);
            }
          }}
          onmousedown={(event) => event.stopPropagation()}
        >
          {#if drawActive}
            <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
              <path d="M12 20h9" />
              <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" />
              <path d="m4 4 16 16" />
            </svg>
          {:else}
            <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
              <circle cx="12" cy="12" r="3" />
              <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 0 1-4 0v-.1a1.7 1.7 0 0 0-1-1.5 1.7 1.7 0 0 0-1.9.3l-.1.1A2 2 0 1 1 4.2 17l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 0 1 0-4h.1a1.7 1.7 0 0 0 1.5-1 1.7 1.7 0 0 0-.3-1.9l-.1-.1A2 2 0 1 1 7 4.2l.1.1a1.7 1.7 0 0 0 1.9.3 1.7 1.7 0 0 0 1-1.5V3a2 2 0 0 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1A2 2 0 1 1 19.8 7l-.1.1a1.7 1.7 0 0 0-.3 1.9 1.7 1.7 0 0 0 1.5 1h.1a2 2 0 0 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z" />
            </svg>
          {/if}
        </button>
        <button
          class="region-share-control"
          data-region-share-control
          type="button"
          aria-label={shareActive ? 'Stop sharing Petal View' : 'Share Petal View'}
          title={shareActive ? 'Stop sharing Petal View' : 'Share Petal View'}
          aria-pressed={shareActive}
          aria-busy={sharePending}
          disabled={sharePending || placementActive || placementSettlementPending}
          onclick={(event) => {
            event.stopPropagation();
            void toggleRegionShare();
          }}
          onmousedown={(event) => event.stopPropagation()}
        >
          {#if shareActive}
            <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
              <rect x="7" y="7" width="10" height="10" rx="1.5" />
            </svg>
          {:else}
            <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
              <rect x="4" y="5" width="16" height="12" rx="2" />
              <path d="M8 20h8M12 17v3" />
            </svg>
          {/if}
        </button>
        <CloseButton ariaLabel="Close region selector" onclick={closeWindow} />
      </div>
    </div>
    {#if outsideDisplay}
      <div
        class="warning-banner"
        role="status"
        in:fly={{ y: 4, duration: enterDuration() }}
        out:fade={{ duration: exitDuration() }}
      >
        <svg class="warning-icon" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
          <path d="M12 3.5 22 20.5H2zM12 9.5v5M12 18v.01" />
        </svg>
        <span>Move Petal View back onto its display</span>
      </div>
    {/if}
  </div>
</div>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    background: transparent !important;
    overflow: hidden;
    font-family: var(--font-ui);
  }

  .window-container {
    box-sizing: border-box;
    width: 100%;
    height: 100%;
    background: transparent;
    padding: 0;
    display: flex;
    flex-direction: column;
    position: relative;
  }

  .hollow-frame {
    box-sizing: border-box;
    width: 100%;
    height: 100%;
    border: 3px solid var(--bg-base);
    border-radius: var(--radius-card);
    background: transparent;
    display: flex;
    flex-direction: column;
    position: relative;
    overflow: hidden;
    padding: 3px;
    pointer-events: auto;
  }

  .hollow-frame::before {
    content: '';
    position: absolute;
    inset: 0;
    z-index: 2;
    border: 3px solid var(--text-primary);
    border-radius: calc(var(--radius-card) - 3px);
    pointer-events: none;
    box-sizing: border-box;
  }

  .window-container.shared .hollow-frame {
    border-color: var(--region-active-color);
  }

  .window-container.shared .hollow-frame::before {
    border-color: var(--region-active-ink);
  }

  .title-bar {
    min-height: 56px;
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 4px 8px;
    padding: 8px 8px 8px 10px;
    box-shadow: inset 0 -1px 0 var(--hairline-strong);
    border-radius: var(--radius-input) var(--radius-input) 0 0;
    background: color-mix(in srgb, var(--surface) 92%, transparent);
    color: var(--text-primary);
    font: 600 var(--text-micro) / 16px var(--font-ui);
    user-select: none;
    position: relative;
    z-index: 1;
  }

  .title-label {
    min-width: 0;
    flex: 1 1 140px;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    line-height: 16px;
  }

  .title-actions {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    flex: 0 0 auto;
    margin-left: auto;
  }

  .controlled-badge {
    box-sizing: border-box;
    max-width: 128px;
    flex: 0 1 auto;
    padding: 3px 7px;
    border: 1px solid color-mix(in srgb, var(--region-active-color) 72%, var(--hairline-strong));
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--region-active-color) 18%, var(--surface));
    color: var(--text-primary);
    font: 700 var(--text-micro) / 14px var(--font-ui);
    overflow-wrap: anywhere;
    text-wrap: pretty;
  }

  .region-options-control,
  .region-share-control {
    width: 40px;
    height: 40px;
    flex: 0 0 auto;
    display: inline-grid;
    place-items: center;
    border: 0;
    border-radius: var(--radius-control);
    background: var(--fill-base);
    color: var(--text-soft);
    cursor: pointer;
    padding: 0;
    transition:
      background-color var(--motion-feedback) var(--ease-standard),
      color var(--motion-feedback) var(--ease-standard),
      box-shadow var(--motion-feedback) var(--ease-standard),
      transform var(--motion-feedback) var(--ease-standard);
  }

  .window-container.shared .region-share-control {
    background: var(--region-active-color);
    color: var(--region-active-ink);
  }

  .region-options-control:hover:not(:disabled),
  .region-share-control:hover:not(:disabled) {
    background: var(--fill-strong);
    box-shadow: inset 0 0 0 1px var(--hairline-strong);
    color: var(--text-primary);
  }

  .window-container.shared .region-share-control:hover:not(:disabled) {
    background: color-mix(in srgb, var(--region-active-color) 84%, var(--surface));
    color: var(--region-active-ink);
  }

  .region-options-control:active:not(:disabled),
  .region-share-control:active:not(:disabled) {
    transform: scale(var(--press-scale, 0.96));
  }

  .region-options-control:focus-visible,
  .region-share-control:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .region-options-control:disabled,
  .region-share-control:disabled {
    opacity: var(--disabled-opacity);
    cursor: default;
  }

  .region-options-control svg,
  .region-share-control svg {
    width: 20px;
    height: 20px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
    pointer-events: none;
  }

  .title-bar :global(.close-button) {
    width: 40px;
    height: 40px;
    border-radius: var(--radius-chip);
    color: var(--text-soft);
    position: relative;
    z-index: 3;
  }

  .title-bar :global(.close-button:hover:not(:disabled)) {
    background-color: var(--fill-strong);
    box-shadow: inset 0 0 0 1px var(--hairline-strong);
    color: var(--text-primary);
  }

  .warning-banner {
    display: inline-flex;
    align-items: flex-start;
    justify-content: center;
    gap: 8px;
    margin: 8px 10px;
    padding: 8px;
    border: 1px solid color-mix(in srgb, var(--warning) 72%, var(--hairline-strong));
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--warning-bg) 88%, var(--surface));
    color: var(--text-primary);
    font: 700 var(--text-micro) / 16px var(--font-ui);
    text-align: left;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    pointer-events: none;
  }

  .warning-icon {
    width: 16px;
    height: 16px;
    flex: 0 0 auto;
    fill: none;
    stroke: var(--warning);
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  @media (max-width: 220px) {
    /* Keep the full-size three-button group inside the inner frame at the
       selector's 160px minimum without shrinking any hit target. */
    .title-actions {
      position: relative;
      right: 6px;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .warning-banner {
      animation: none;
    }
  }
</style>
