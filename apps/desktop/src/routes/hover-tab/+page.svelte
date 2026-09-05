<!--
  Fixed right-edge sharing rail. The native window is always 40x40; this
  route owns one direct Share/Stop button and delegates secondary choices to
  the shared OS-native menu.

  The Rust side owns visibility and compact positioning. Passive presentation
  never steals focus, while the focused button supports the keyboard menu
  shortcuts without changing the native window geometry.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { emit, listen } from '@tauri-apps/api/event';
  import { LogicalPosition } from '@tauri-apps/api/dpi';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { onMount } from 'svelte';
  import Pill from '@petal/shared/ui/components/Pill.svelte';
  import type { IdentityColor } from '$lib/components/Avatar.svelte';
  import { identityColorCss, identityInkCss } from '$lib/data/identityColor';
  import { session } from '$lib/stores/session.svelte';
  import { COMMANDS, EVENTS } from '$lib/ipc';
  import type {
    AiChatEndReason,
    AiChatRefusedEvent,
    AiChatSettings,
    AiChatStartOutcome,
    AiChatStateEvent,
    HoverTabUpdate,
    RemoteControlStatus,
    ShareControlModeChanged,
    SharePriority,
    WindowFrame
  } from '$lib/ipc';
  import {
    buildShareOptionsMenuEntries,
    type HoverTabPosition
  } from '$lib/data/shareOptionsMenu';
  import { popupShareOptionsMenu } from '$lib/shareOptionsPopup';
  import { isMac, isWindows } from '$lib/platform';
  import {
    beginHoverTabGesture,
    cancelHoverTabGesture,
    clearHoverTabPreview,
    createHoverTabPreviewState,
    isHoverTabDragging,
    moveHoverTabGesture,
    offerHoverTabPreview,
    settleHoverTabPreview,
    takeHoverTabPreview,
    type HoverTabGesture,
    type HoverTabPreviewState
  } from '$lib/hoverTabDrag';
  import {
    aiChatHoverTabOptionsTitle,
    hoverTabAiChatNextActiveState
  } from '$lib/data/aiChat';

  let visible = $state(false);
  let attachment = $state<HoverTabUpdate['attachment']>('outside');
  let actionButton = $state<HTMLButtonElement | undefined>(undefined);
  let currentWindowId = $state<number | null>(null);
  let currentFrame = $state<WindowFrame | null>(null);
  let displayLike = $state(false);
  // Local rendering cache. Starts update optimistically; stops reconcile at
  // the native capture boundary before their network teardown finishes.
  let sharedWindows = $state(new Set<number>());
  // Sharer-side remote-control context. It is deliberately exposed through
  // the button's truthful accessible name/title rather than a badge that
  // would add clutter to the fixed 40x40 surface.
  let controlledWindows = $state(new Map<number, string>());
  let pending = $state(false);
  let priority = $state<SharePriority>('automatic');
  // Sharer-side live control-mode for the hovered shared window. Ordinary
  // HWND shares default to cursor-preserving; display-like shares are forced
  // to full control by the backend and UI.
  let shareControlMode = $state<'cursorPreserving' | 'fullControl'>('cursorPreserving');
  let menuPending = $state(false);
  let drawActive = $state(false);
  let verticalOffset = $state(0.5);
  let dragGesture = $state<HoverTabGesture | null>(null);
  let dragPreviewState: HoverTabPreviewState | null = null;
  let dragFrameRequest: number | undefined;
  let suppressNextClick = false;
  let dragCommandQueue: Promise<unknown> = Promise.resolve();
  // Master switch for AI chat (#656, #736). Seeded on mount and re-read
  // on settings changes, because Settings lives in a DIFFERENT webview.
  let aiChatEnabled = $state(false);
  let aiChatActive = $state(false);
  let aiChatError = $state<{ windowId: number; reason: AiChatEndReason } | null>(null);
  let errorDismissTimer: ReturnType<typeof setTimeout> | undefined;
  const AUTO_DISMISS_MS = 4000;

  const isShared = $derived(currentWindowId !== null && sharedWindows.has(currentWindowId));
  const isBeingControlled = $derived(
    currentWindowId !== null && controlledWindows.has(currentWindowId)
  );
  const localIdentityColor = $derived<IdentityColor>(session.identity ?? 'plum');
  const sharedTabBackground = $derived(`var(--id-${localIdentityColor}, var(--id-plum, #f06cc9))`);
  const sharedTabColor = $derived(identityColorCss(localIdentityColor));
  const sharedTabInk = $derived(identityInkCss(localIdentityColor) ?? 'var(--gallery-frame)');
  const currentAiChatError = $derived(
    currentWindowId !== null && aiChatError?.windowId === currentWindowId
      ? aiChatError.reason
      : null
  );
  const shareActionLabel = $derived(isShared ? 'Stop sharing' : 'Share this window');
  const shareActionContext = $derived(
    currentAiChatError
      ? aiChatHoverTabOptionsTitle(currentAiChatError)
      : drawActive
        ? 'Drawing is active on this shared window.'
        : isBeingControlled
          ? `A participant is controlling this window in ${shareControlMode === 'fullControl' ? 'full-control' : 'cursor-preserving'} mode.`
          : ''
  );
  const isDragging = $derived(isHoverTabDragging(dragGesture));
  const shareActionAriaLabel = $derived(
    `${shareActionLabel}. Drag vertically to move; right-click for options${shareActionContext ? `. ${shareActionContext}` : ''}`
  );
  const shareActionTooltip = $derived(
    `${shareActionLabel} — drag to move; right-click for options`
  );

  // Native AppKit tooltips are more reliable than WKWebView's HTML `title`
  // tracking in the non-key panel. Keep the title as a fallback and mirror its
  // current text onto the real WKWebView without changing the panel geometry.
  $effect(() => {
    if (!isMac()) return;
    const tooltip = shareActionTooltip;
    void invoke(COMMANDS.setHoverTabTooltip, { tooltip }).catch(() => {});
  });

  function setAiChatError(windowId: number, reason: AiChatEndReason) {
    clearTimeout(errorDismissTimer);
    aiChatError = { windowId, reason };
    errorDismissTimer = setTimeout(() => {
      if (aiChatError?.windowId === windowId) {
        aiChatError = null;
      }
    }, AUTO_DISMISS_MS);
  }

  function clearAiChatError() {
    clearTimeout(errorDismissTimer);
    aiChatError = null;
  }

  function stopDrawForWindow(windowId: number) {
    void invoke(COMMANDS.shareOverlaySetDrawActive, {
      windowId,
      active: false
    }).catch(() => {});
  }

  function applyUpdate(update: HoverTabUpdate) {
    void refreshAiChatEnabled();
    const { windowId, frame, shared } = update;
    const previousDisplayLike = displayLike;
    const nextDisplayLike = update.displayLike;
    const previousWindowId = currentWindowId;
    if (previousWindowId !== windowId) {
      if (dragGesture) cancelActionDrag();
      clearAiChatError();
    }
    if (previousWindowId !== windowId && drawActive && previousWindowId !== null) {
      // Full-display Draw has no useful hover target, so keep its state
      // latched. Ordinary window changes still release their overlay Draw
      // state when the hover target changes.
      if (previousDisplayLike) {
        visible = false;
        return;
      }
      stopDrawForWindow(previousWindowId);
      drawActive = false;
    }
    displayLike = nextDisplayLike;
    currentWindowId = windowId;
    currentFrame = frame;
    attachment = update.attachment;
    if (!dragGesture && Number.isFinite(update.verticalOffset)) {
      verticalOffset = Math.min(1, Math.max(0, update.verticalOffset));
    }
    visible = true;
    if (previousWindowId !== windowId) {
      shareControlMode = displayLike ? 'fullControl' : 'cursorPreserving';
      void checkAiChatActive(windowId);
    }
    // Reconcile with backend-reported shared state for this window (in
    // case it changed via some other path) without clobbering an
    // in-flight optimistic toggle.
    if (!pending) {
      sharedWindows = windowShareSet(sharedWindows, windowId, shared);
      if (!shared) drawActive = false;
    }
  }

  function windowShareSet(source: Set<number>, windowId: number, shared: boolean): Set<number> {
    const next = new Set(source);
    if (shared) {
      next.add(windowId);
    } else {
      next.delete(windowId);
    }
    return next;
  }

  type DragCommandPhase = 'begin' | 'update' | 'commit' | 'cancel';

  function enqueueHoverTabDrag(
    phase: DragCommandPhase,
    windowId: number,
    frame: WindowFrame,
    offset: number
  ): Promise<number> {
    const next = dragCommandQueue.then(() =>
      invoke<number>(COMMANDS.hoverTabDrag, {
        phase,
        windowId,
        frame,
        verticalOffset: offset
      })
    );
    dragCommandQueue = next.catch(() => undefined);
    return next;
  }

  function releasePointerCapture(pointerId: number) {
    try {
      if (actionButton?.hasPointerCapture(pointerId)) actionButton.releasePointerCapture(pointerId);
    } catch {
      // Pointer capture can already be gone when the native window moves.
    }
  }

  function clearDragAnimationFrame() {
    if (dragFrameRequest !== undefined) {
      cancelAnimationFrame(dragFrameRequest);
      dragFrameRequest = undefined;
    }
    if (dragPreviewState) clearHoverTabPreview(dragPreviewState);
  }

  function flushDragUpdate() {
    dragFrameRequest = undefined;
    const gesture = dragGesture;
    const preview = dragPreviewState;
    if (!gesture || gesture.phase !== 'dragging' || !preview) return;

    const requestedOffset = takeHoverTabPreview(preview);
    if (requestedOffset === null) return;
    const offset = offerHoverTabPreview(preview, requestedOffset);
    if (offset === null) return;

    const windowId = currentWindowId;
    const frame = currentFrame;
    if (windowId === null || frame === null) {
      settleHoverTabPreview(preview);
      if (dragGesture === gesture) cancelActionDrag();
      return;
    }

    void enqueueHoverTabDrag('update', windowId, frame, offset)
      .catch(() => {
        if (dragGesture === gesture) cancelActionDrag();
      })
      .finally(() => {
        const nextOffset = settleHoverTabPreview(preview);
        if (
          nextOffset === null ||
          dragPreviewState !== preview ||
          dragGesture !== gesture ||
          gesture.phase !== 'dragging'
        ) {
          return;
        }
        preview.pendingOffset = nextOffset;
        scheduleDragUpdate();
      });
  }

  function scheduleDragUpdate() {
    if (dragFrameRequest !== undefined) return;
    dragFrameRequest = requestAnimationFrame(flushDragUpdate);
  }

  function queueDragPreview(offset: number) {
    const preview = dragPreviewState;
    if (!preview) return;
    preview.pendingOffset = offset;
    if (!preview.inFlight) scheduleDragUpdate();
  }

  function cancelActionDrag(event?: Event) {
    const gesture = dragGesture;
    if (event) event.preventDefault();
    clearDragAnimationFrame();
    dragPreviewState = null;
    dragGesture = null;
    if (!gesture) return;
    const wasDragging = gesture.phase === 'dragging';
    const windowId = currentWindowId;
    const frame = currentFrame;
    releasePointerCapture(gesture.pointerId);
    if (!wasDragging) return;

    const restored = cancelHoverTabGesture(gesture);
    if (restored === null) return;
    verticalOffset = restored;
    if (windowId !== null && frame !== null) {
      void enqueueHoverTabDrag('cancel', windowId, frame, restored).catch(() => {});
    }
  }

  function onActionPointerDown(event: PointerEvent) {
    if (
      event.button !== 0 ||
      pending ||
      menuPending ||
      currentWindowId === null ||
      currentFrame === null
    ) {
      return;
    }
    // Some WebViews do not synthesize a click after a prevented drag
    // pointer-up. Clear the one-shot guard when the next real gesture begins
    // so that missing compatibility events cannot eat the next Share/Stop.
    suppressNextClick = false;
    clearDragAnimationFrame();
    dragPreviewState = createHoverTabPreviewState();
    dragGesture = beginHoverTabGesture(
      event.pointerId,
      event.screenX,
      event.screenY,
      verticalOffset
    );
    try {
      actionButton?.setPointerCapture(event.pointerId);
    } catch {
      // Browsers/native webviews may reject capture during teardown.
    }
  }

  function onActionPointerMove(event: PointerEvent) {
    const gesture = dragGesture;
    if (!gesture || gesture.pointerId !== event.pointerId || currentFrame === null) return;
    const moved = moveHoverTabGesture(
      gesture,
      event.screenX,
      event.screenY,
      currentFrame.height
    );
    if (moved.offset === null) return;
    event.preventDefault();
    dragGesture = moved.gesture;
    verticalOffset = moved.offset;
    if (moved.started) {
      const windowId = currentWindowId;
      const frame = currentFrame;
      if (windowId === null || frame === null) return;
      void enqueueHoverTabDrag('begin', windowId, frame, gesture.originalOffset).catch(() => {
        if (dragGesture === moved.gesture) cancelActionDrag();
      });
    }
    queueDragPreview(moved.offset);
  }

  function onActionPointerUp(event: PointerEvent) {
    const gesture = dragGesture;
    if (!gesture || gesture.pointerId !== event.pointerId) return;
    clearDragAnimationFrame();
    dragPreviewState = null;
    dragGesture = null;
    releasePointerCapture(event.pointerId);
    if (gesture.phase !== 'dragging') return;

    event.preventDefault();
    const windowId = currentWindowId;
    const frame = currentFrame;
    const committed = verticalOffset;
    const restored = cancelHoverTabGesture(gesture) ?? 0.5;
    suppressNextClick = true;
    if (windowId !== null && frame !== null) {
      void enqueueHoverTabDrag('commit', windowId, frame, committed).catch(() => {
        // The native command may have failed before it could clear its drag
        // session (for example, an IPC interruption). A best-effort cancel is
        // idempotent after backend rollback and prevents a frozen follower.
        verticalOffset = restored;
        void enqueueHoverTabDrag('cancel', windowId, frame, restored).catch(() => {});
      });
    }
  }

  function onActionPointerCancel(event: PointerEvent) {
    cancelActionDrag(event);
  }

  function onActionLostPointerCapture(event: PointerEvent) {
    if (dragGesture) cancelActionDrag(event);
  }

  onMount(() => {
    // Diagnostic breadcrumb (issue #22): proves in petal.log that this
    // webview loaded the route AND has a live IPC bridge. Catch-ignored so a
    // plain-browser preview (no Tauri bridge) doesn't error.
    invoke<HoverTabUpdate | null>(COMMANDS.hoverTabPageMounted)
      .then((update) => {
        if (update) applyUpdate(update);
      })
      .catch(() => {});

    invoke<SharePriority>(COMMANDS.getSharePriority)
      .then((saved) => {
        priority = saved;
      })
      .catch(() => {});

    void refreshAiChatEnabled().then(() => {
      void checkAiChatActive(currentWindowId);
    });

    const unUpdate = listen<HoverTabUpdate>(EVENTS.hoverTabUpdate, (event) => {
      applyUpdate(event.payload);
    });

    const unShareState = listen<{ windowId: number; shared: boolean }>(EVENTS.shareStateChanged, (event) => {
      sharedWindows = windowShareSet(sharedWindows, event.payload.windowId, event.payload.shared);
      if (!event.payload.shared && currentWindowId === event.payload.windowId) {
        drawActive = false;
        shareControlMode = displayLike ? 'fullControl' : 'cursorPreserving';
      }
      if (!event.payload.shared) {
        controlledWindows = new Map(
          [...controlledWindows].filter(([id]) => id !== event.payload.windowId)
        );
      }
    });

    const unShareControlMode = listen<ShareControlModeChanged>(EVENTS.shareControlModeChanged, (event) => {
      if (currentWindowId === event.payload.windowId) {
        shareControlMode = event.payload.controlMode;
      }
    });

    const unRemoteStatus = listen<RemoteControlStatus>(EVENTS.remoteControlStatus, (event) => {
      const { windowId, controllerId, status } = event.payload;
      if (status === 'active') {
        // Sharer-side: a remote participant is controlling a window this
        // process shares. On the host's active-grant status, controllerId
        // names the remote controller (ownerIdentity is null locally).
        if (controllerId !== session.identity) {
          controlledWindows = new Map(controlledWindows).set(windowId, controllerId);
        }
      } else if (status === 'stopped') {
        controlledWindows = new Map(
          [...controlledWindows].filter(([id]) => id !== windowId)
        );
      }
    });

    const unHide = listen(EVENTS.hoverTabHide, () => {
      // Native hide is a cancellation boundary too: pointer capture may be
      // lost while the singleton panel leaves the screen. The backend also
      // clears its drag session, so cancel locally before releasing the UI.
      cancelActionDrag();
      // Hiding the hover pill only means the cursor left the target or the
      // native menu changed focus. Sharer Draw belongs to the shared-window
      // overlay, not to the pill, so keep it active until the user explicitly
      // toggles Draw off, changes target, or unshares the window.
      if (!drawActive) {
        visible = false;
      }
    });

    const unAiChatState = listen<AiChatStateEvent>(EVENTS.aiChatState, (event) => {
      aiChatActive = hoverTabAiChatNextActiveState(aiChatActive, event.payload, currentWindowId);
    });

    return () => {
      unUpdate.then((u) => u()).catch(() => {});
      unShareState.then((u) => u()).catch(() => {});
      unShareControlMode.then((u) => u()).catch(() => {});
      unRemoteStatus.then((u) => u()).catch(() => {});
      unHide.then((u) => u()).catch(() => {});
      unAiChatState.then((u) => u()).catch(() => {});
      cancelActionDrag();
      clearTimeout(errorDismissTimer);
    };
  });

  async function onToggleShare() {
    if (suppressNextClick) {
      suppressNextClick = false;
      return;
    }
    if (pending || currentWindowId === null || currentFrame === null) return;
    const windowId = currentWindowId;
    const frame = currentFrame;
    const wasShared = sharedWindows.has(windowId);
    const targetShared = !wasShared;

    pending = true;
    // Starts remain optimistic. Stops wait for the native capture boundary;
    // the backend emits share-state-changed immediately after capture.stop()
    // so the pill changes with the border, before unpublish/metadata cleanup.
    if (targetShared) sharedWindows = windowShareSet(sharedWindows, windowId, true);

    try {
      const nowShared = await invoke<boolean>(COMMANDS.toggleWindowShare, {
        windowId,
        frame,
        color: sharedTabColor
      });
      sharedWindows = windowShareSet(sharedWindows, windowId, nowShared);
    } catch {
      // A failed start rolls back its optimism. A failed stop is already a
      // completed local teardown and must stay unshared (#420).
      if (targetShared) sharedWindows = windowShareSet(sharedWindows, windowId, wasShared);
    } finally {
      pending = false;
    }
  }

  // Per-share remote-control lock. Optimistic like `shareControlMode`, but
  // rolled back on failure: leaving the menu showing "allowed" when the host
  // still has it locked (or vice versa) misrepresents a permission.
  let shareRemoteControlAllowed = $state(true);

  async function onSetShareRemoteControlAllowed(allowed: boolean) {
    const windowId = currentWindowId;
    if (windowId === null || !sharedWindows.has(windowId)) return;
    const previous = shareRemoteControlAllowed;
    shareRemoteControlAllowed = allowed;
    try {
      await invoke(COMMANDS.setShareRemoteControlAllowed, { windowId, allowed });
    } catch (e) {
      shareRemoteControlAllowed = previous;
      console.error(`set_share_remote_control_allowed(${windowId}) failed`, e);
    }
  }

  async function onSetShareMode(mode: 'cursorPreserving' | 'fullControl') {
    const windowId = currentWindowId;
    if (windowId === null || !sharedWindows.has(windowId)) return;
    const effectiveMode = displayLike ? 'fullControl' : mode;
    shareControlMode = effectiveMode;
    try {
      await invoke(COMMANDS.setShareControlMode, { windowId, controlMode: effectiveMode });
    } catch (e) {
      console.error(`set_share_control_mode(${windowId}) failed`, e);
    }
  }

  async function selectDraw(next: boolean) {
    if (menuPending || pending || currentWindowId === null || !isShared) return;
    const previous = drawActive;
    drawActive = next;
    menuPending = true;
    try {
      await invoke(COMMANDS.shareOverlaySetDrawActive, {
        windowId: currentWindowId,
        active: next
      });
    } catch {
      drawActive = previous;
    } finally {
      menuPending = false;
    }
  }

  async function selectPriority(next: SharePriority) {
    if (menuPending) return;
    const previous = priority;
    priority = next;
    menuPending = true;
    try {
      priority = await invoke<SharePriority>(COMMANDS.setSharePriority, {
        priority: next,
        windowId: currentWindowId
      });
    } catch {
      priority = previous;
    } finally {
      menuPending = false;
    }
  }

  async function selectPosition(next: HoverTabPosition) {
    if (menuPending || currentWindowId === null || currentFrame === null) return;
    const previous = verticalOffset;
    const nextOffset = next === 'top' ? 0 : next === 'bottom' ? 1 : 0.5;
    verticalOffset = nextOffset;
    menuPending = true;
    try {
      verticalOffset = await enqueueHoverTabDrag(
        'commit',
        currentWindowId,
        currentFrame,
        nextOffset
      );
    } catch {
      verticalOffset = previous;
    } finally {
      menuPending = false;
    }
  }

  // Same command + fallback shape as Gallery.svelte / meeting/[room]/+page.svelte's
  // openNetworkCockpit (#361): reuses the existing sender-side diagnostics
  // window rather than inventing a new debug concept.
  async function openDebugCockpit() {
    try {
      await invoke(COMMANDS.openNetworkCockpitWindow);
    } catch (e) {
      console.error('open_network_cockpit_window failed', e);
    }
  }

  // Re-read the master switch. On failure the cached value stands: a transient
  // IPC error must not silently hide a feature the user turned on.
  async function refreshAiChatEnabled() {
    try {
      const settings = await invoke<AiChatSettings>(COMMANDS.aiChatSettings);
      aiChatEnabled = settings.enabled;
    } catch {
      // Plain-browser preview or a transient failure — keep what we have.
    }
  }

  async function checkAiChatActive(windowId: number | null) {
    if (!aiChatEnabled || windowId === null) {
      aiChatActive = false;
      return;
    }
    try {
      aiChatActive = await invoke<boolean>(COMMANDS.aiChatIsActive, { windowId });
    } catch {
      aiChatActive = false;
    }
  }

  async function onToggleAiChat() {
    if (currentWindowId === null) return;
    if (aiChatActive) {
      await stopAiChat();
    } else {
      await startAiChat(currentWindowId);
    }
  }

  // Start a session for the hovered window (#656, #736). The button itself
  // enters an amber warning state and exposes the reason through its
  // accessible name/title; the reason is also re-emitted for the main window.
  async function startAiChat(windowId: number) {
    if (menuPending) return;
    menuPending = true;
    try {
      const outcome = await invoke<AiChatStartOutcome>(COMMANDS.aiChatStart, { windowId });
      if (outcome.started) {
        aiChatActive = true;
        clearAiChatError();
      } else {
        aiChatActive = false;
        const payload: AiChatRefusedEvent = { windowId, reason: outcome.reason ?? 'error' };
        setAiChatError(windowId, payload.reason);
        await emit(EVENTS.aiChatRefused, payload).catch(() => {});
      }
    } catch {
      aiChatActive = false;
      setAiChatError(windowId, 'error');
      const payload: AiChatRefusedEvent = { windowId, reason: 'error' };
      await emit(EVENTS.aiChatRefused, payload).catch(() => {});
    } finally {
      menuPending = false;
    }
  }

  // Stop a running session. Reachable from the hover tab over ANY window at
  // any time, which the meeting-window panel is not.
  async function stopAiChat() {
    if (menuPending) return;
    menuPending = true;
    try {
      await invoke(COMMANDS.aiChatStop);
      aiChatActive = false;
    } catch {
      // The session's own state event is the source of truth.
    } finally {
      menuPending = false;
    }
  }

  async function onOpenQualityMenu(
    event: MouseEvent | KeyboardEvent,
    keyboardInvocation = false
  ) {
    // The root layout suppresses the browser context menu globally. Stop both
    // default behavior and propagation before handing ownership to Tauri.
    event.preventDefault();
    event.stopPropagation();
    if (menuPending) return;

    const remoteControlSupported = isWindows();
    await invoke(COMMANDS.setHoverTabMenuOpen, { open: true }).catch(() => {});

    try {
      const entries = buildShareOptionsMenuEntries(
        priority,
        isShared && !pending,
        drawActive,
        shareControlMode,
        remoteControlSupported,
        aiChatEnabled,
        aiChatActive,
        displayLike,
        true,
        verticalOffset,
        shareRemoteControlAllowed
      );
      const placement = keyboardInvocation && actionButton
        ? (() => {
            const rect = actionButton.getBoundingClientRect();
            return {
              position: new LogicalPosition(rect.left, rect.bottom),
              window: getCurrentWindow()
            };
          })()
        : undefined;
      await popupShareOptionsMenu(entries, {
        onPriority: (value) => void selectPriority(value),
        onControlMode: (value) => void onSetShareMode(value),
        onDraw: (active) => void selectDraw(active),
        onAiChat: () => void onToggleAiChat(),
        onDebug: () => void openDebugCockpit(),
        onPosition: (value) => void selectPosition(value),
        onRemoteControlAllowed: (allowed) => void onSetShareRemoteControlAllowed(allowed)
      }, placement);
    } finally {
      await invoke(COMMANDS.setHoverTabMenuOpen, { open: false }).catch(() => {});
    }
  }

  function onActionContextMenu(event: MouseEvent) {
    void onOpenQualityMenu(event);
  }

  function onActionKeyDown(event: KeyboardEvent) {
    if (event.key === 'Escape' && dragGesture) {
      cancelActionDrag(event);
      return;
    }
    const menuKey = event.key === 'ContextMenu' || event.key === 'Menu' || event.key === 'Apps' || event.code === 'ContextMenu';
    const shiftF10 = event.key === 'F10' && event.shiftKey;
    if (!menuKey && !shiftF10) return;
    void onOpenQualityMenu(event, true);
  }
</script>

<div
  class="hover-tab-host"
  class:is-shared={isShared}
  class:inset={attachment === 'inset'}
  class:hidden={!visible}
  style:--share-tab-bg={sharedTabBackground}
  style:--share-tab-fg={sharedTabInk}
  role="group"
  aria-label="Window sharing controls"
>
  <Pill attach="right">
    <div class="hover-tab-surface">
      <button
        bind:this={actionButton}
        type="button"
        class="hover-tab-action hover-tab-trigger"
        class:is-shared={isShared}
        class:pending
        class:dragging={isDragging}
        onclick={onToggleShare}
        oncontextmenu={onActionContextMenu}
        onkeydown={onActionKeyDown}
        onpointerdown={onActionPointerDown}
        onpointermove={onActionPointerMove}
        onpointerup={onActionPointerUp}
        onpointercancel={onActionPointerCancel}
        onlostpointercapture={onActionLostPointerCapture}
        disabled={pending}
        aria-busy={pending}
        aria-haspopup="menu"
        aria-keyshortcuts="Shift+F10,ContextMenu"
        aria-label={shareActionAriaLabel}
        data-allow-native-tooltip={isWindows() ? 'true' : undefined}
        title={isWindows() ? shareActionTooltip : undefined}
      >
        <svg class="hover-tab-icon" aria-hidden="true" focusable="false" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          {#if isShared}
            <rect x="5" y="5" width="14" height="14" rx="2"></rect>
          {:else}
            <path d="M13 3H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-3"></path>
            <path d="M8 21h8M12 17v4"></path>
            <path d="m17 8 5-5M17 3h5v5"></path>
          {/if}
        </svg>
        {#if isShared}<span class="hover-tab-live-dot" aria-hidden="true"></span>{/if}
      </button>
    </div>
  </Pill>
</div>

<style>
  /* Keep the standalone overlay document transparent even if global body
     styles load after the route CSS. */
  :global(html),
  :global(body) {
    background: transparent !important;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .hover-tab-host {
    width: 40px;
    height: 40px;
    display: flex;
    align-items: stretch;
    justify-content: flex-start;
    position: relative;
    overflow: hidden;
    font-family: var(--font-ui, -apple-system, system-ui, sans-serif);
  }

  .hover-tab-host.hidden {
    visibility: hidden;
  }

  .hover-tab-host :global(.pill.attach) {
    display: inline-flex;
    align-items: center;
    width: 40px;
    max-width: 40px;
    height: 40px;
    flex: 0 0 40px;
    gap: 0;
    padding: 0;
    border-radius: 0 12px 12px 0;
    overflow: hidden;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.2),
      inset 1px 0 0 rgba(255, 255, 255, 0.09),
      inset -1px 0 0 rgba(255, 255, 255, 0.09),
      inset 0 1px 0 rgba(255, 255, 255, 0.07),
      0 8px 20px -12px rgba(0, 0, 0, 0.72);
  }

  .hover-tab-host.inset :global(.pill.attach-right) {
    border-radius: 12px 0 0 12px;
  }

  .hover-tab-host.is-shared :global(.pill.attach) {
    background: var(--share-tab-bg);
    color: var(--share-tab-fg);
    box-shadow:
      inset 0 0 0 1px color-mix(in srgb, var(--share-tab-fg) 24%, transparent),
      inset 1px 0 0 color-mix(in srgb, var(--share-tab-fg) 18%, transparent),
      inset -1px 0 0 color-mix(in srgb, var(--share-tab-fg) 18%, transparent),
      inset 0 1px 0 rgba(255, 255, 255, 0.22),
      0 8px 20px -12px rgba(0, 0, 0, 0.72);
  }

  .hover-tab-surface {
    display: flex;
    align-items: stretch;
    justify-content: flex-end;
    width: 40px;
    height: 40px;
    min-width: 40px;
  }

  .hover-tab-action {
    position: relative;
    z-index: 2;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex: 0 0 40px;
    width: 40px;
    height: 40px;
    min-width: 40px;
    padding: 0;
    box-sizing: border-box;
    border: 1px solid transparent;
    border-radius: 0 10px 10px 0;
    background: color-mix(in srgb, var(--text-primary, #f5f6f7) 10%, transparent);
    color: var(--text-primary, #f5f6f7);
    cursor: pointer;
    touch-action: none;
    transition:
      background-color var(--motion-fast, 120ms) var(--ease-standard, ease),
      border-color var(--motion-fast, 120ms) var(--ease-standard, ease);
  }

  .hover-tab-host.inset .hover-tab-action {
    border-radius: 10px 0 0 10px;
  }

  .hover-tab-action:hover:not(:disabled) {
    background: color-mix(in srgb, var(--text-primary, #f5f6f7) 20%, transparent);
  }

  .hover-tab-action:disabled {
    cursor: default;
    opacity: 0.68;
  }

  .hover-tab-action.is-shared {
    background: var(--share-tab-bg);
    color: var(--share-tab-fg);
  }

  .hover-tab-action:not(.is-shared) {
    border-color: var(--live-bright, #7ff0a3);
  }

  .hover-tab-action.pending {
    opacity: 0.72;
  }

  .hover-tab-action:active:not(:disabled) {
    transform: scale(0.96);
  }

  .hover-tab-action.dragging {
    cursor: grabbing;
    background: color-mix(in srgb, var(--text-primary, #f5f6f7) 24%, transparent);
    transform: none;
  }

  .hover-tab-action:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .hover-tab-icon {
    flex: 0 0 auto;
    width: 16px;
    height: 16px;
  }

  .hover-tab-live-dot {
    position: absolute;
    right: 7px;
    bottom: 7px;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: currentColor;
    box-shadow: 0 0 0 2px color-mix(in srgb, currentColor 18%, transparent);
  }

  @media (prefers-reduced-motion: reduce) {
    .hover-tab-action {
      transition: none;
    }

    .hover-tab-action:active:not(:disabled) {
      transform: none;
    }
  }
</style>
