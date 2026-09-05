<!--
  The remote compositor window's PANEL webview (see
  src-tauri/src/compositor.rs's `ensure_window`). This is the FULL-WINDOW,
  transparent webview of a remote screenshare window; the real decoded video
  is a native `AVSampleBufferDisplayLayer` NSView added IN FRONT of this
  webview, sized to only the content area BELOW the 44px header strip
  (`attach_display_layer` + `resize_to_content_on_main`). So the exposed top
  strip of this page renders the window's header chrome, and the video covers
  the rest.

  Folding the header into THIS window (instead of a separate `addChildWindow`
  child) is deliberate: a separate header child detached to the screen corner
  after the first-frame resize (macOS child auto-follow overriding its
  explicit position) and fell BEHIND other windows on click (a non-floating
  child loses the z-battle on activation). As part of the panel itself it can
  never detach or fall behind. The floating, click-through control/pointer
  overlays stay separate — they must sit OVER the video, which this
  single-window webview cannot.

  Wiring (working behaviors, not visual-only):
  - Drag: `onmousedown` on the header strip calls `compositor_start_drag`,
    which starts a native OS window-drag on THIS panel. Mousedowns over the
    video area hit the native video NSView (in front), so only the exposed
    header strip starts a drag.
  - Open URL / remote-control / debug / draw: real Tauri commands, same as the
    old header route.

  `ownerName`/`sourceTitle`/etc. come from this window's URL query params
  (see `header_query_string` in compositor.rs). The colored window border +
  rounded corners are painted natively by `apply_remote_window_border`, so
  this page draws no border of its own.
-->
<script lang="ts">
  import { page } from '$app/state';
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { LogicalPosition } from '@tauri-apps/api/dpi';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { CheckMenuItem, Menu, MenuItem, PredefinedMenuItem } from '@tauri-apps/api/menu';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { beginCompositorResizeDrag, type CompositorResizeDirection } from '$lib/compositorResize';
  import RemoteWindowHeader from '$lib/components/RemoteWindowHeader.svelte';
  import { colorForIdentity, identityColorFromPaletteIndex } from '$lib/data/identityColor';
  import { isWindows } from '$lib/platform';
  import {
    findRemoteWindowDebugTrack,
    formatGlassToGlassLatencyChip
  } from '$lib/data/remoteWindowDebug';
  import { aiChatHeaderAction, aiChatHeaderControlVisible } from '$lib/data/aiChat';
  import { debugHeaderControlVisible } from '$lib/data/debugMode';
  import {
    COMMANDS,
    EVENTS,
    type AiChatOverlayOpenChangedEvent,
    type AiChatRemoteSessionState,
    type AiChatSettings,
    type DebugModeSettings,
    type NetworkSnapshot,
    type PresentParticipant,
    type RemoteControlStatus,
    type RemoteWindowDebugStats
  } from '$lib/ipc';
  import {
    REMOTE_CONTROL_REQUEST_TIMEOUT_MS,
    REMOTE_CONTROL_CONSENT_TIMEOUT_MS,
    REMOTE_CONTROL_CONSENT_TIMEOUT_MESSAGE,
    REMOTE_CONTROL_TIMEOUT_MESSAGE,
    remoteControlStatusEffect,
    type RemoteControlFeedbackStatus
  } from '$lib/remoteControlFeedback';

  const windowId = $derived(Number(page.url.searchParams.get('windowId') ?? '0'));
  const ownerName = $derived(page.url.searchParams.get('owner') ?? 'Someone');
  const ownerIdentity = $derived(page.url.searchParams.get('ownerIdentity') ?? '');
  const ownerPaletteIndex = $derived(
    page.url.searchParams.has('ownerPaletteIndex') ? Number(page.url.searchParams.get('ownerPaletteIndex')) : null
  );
  const sourceTitle = $derived(page.url.searchParams.get('title') ?? 'Shared window');
  const initialSourceUrl = $derived(page.url.searchParams.get('url'));
  const remoteControlAvailable = $derived(page.url.searchParams.get('remoteControl') === '1');
  // Distinct from merely-absent `remoteControl`: the sharer said no, which is
  // permanent, so the header hides the segment instead of showing 'Preparing…'.
  const remoteControlDisallowed = $derived(
    page.url.searchParams.get('remoteControlDisallowed') === '1'
  );
  let controlMode = $state<'cursorPreserving' | 'fullControl'>(
    page.url.searchParams.get('controlMode') === 'fullControl' ? 'fullControl' : 'cursorPreserving'
  );
  // Remote-control modes are Windows-only (macOS does not support them yet);
  // gate the receiver chip + escalation affordance off on other platforms.
  const controlModesSupported = $derived(isWindows());

  const identity = $derived(identityColorFromPaletteIndex(ownerPaletteIndex) ?? colorForIdentity(ownerIdentity || ownerName));

  let sourceUrl = $state<string | null>(null);
  let remoteControlActive = $state(false);
  let remoteControlRequesting = $state(false);
  let remoteControlStatus = $state<RemoteControlFeedbackStatus>(null);
  let remoteControlStatusMessage = $state<string | null>(null);
  let drawActive = $state(false);
  let mediaPaused = $state(false);
  let lastFrameReceivedMs = $state<number | null>(null);
  let freshnessTimer: ReturnType<typeof setInterval> | undefined;
  let unlistenRemoteControlStatus: UnlistenFn | undefined;
  let remoteControlTimeout: ReturnType<typeof setTimeout> | undefined;
  let remoteControlFeedbackTimeout: ReturnType<typeof setTimeout> | undefined;
  let remoteControlRequestSerial = 0;
  let modeMenu: Menu | null = null;
  let viewModeItem: CheckMenuItem | null = null;
  let controlModeItem: CheckMenuItem | null = null;
  let drawModeItem: CheckMenuItem | null = null;
  let aiChatModeItem: CheckMenuItem | null = null;
  let menuHasAiChat = false;
  let unlistenAiChatRemoteState: UnlistenFn | undefined;
  let unlistenAiChatOverlayOpenChanged: UnlistenFn | undefined;
  let unlistenDebugModeChanged: UnlistenFn | undefined;

  // ---- AI chat (#657 receiver half) -----------------------------------------
  // ONE master switch gates the whole feature; when it is off nothing about AI
  // chat is rendered here, exactly as the hover-tab entry is absent rather than
  // disabled. The session itself always runs on the OWNER's machine — this
  // window only ever publishes a request on `petal.ai-chat`.
  let aiChatEnabled = $state(false);
  // One shape for both the mount-time read and the event stream, so this holds
  // whichever arrived last with no conversion step.
  let aiChatSession = $state<AiChatRemoteSessionState | null>(null);
  // #844: Rust's CompositorWindow.ai_chat_overlay_open, mirrored here --
  // seeded by refreshAiChatOverlayOpen on mount, kept live by
  // EVENTS.aiChatOverlayOpenChanged. RemoteWindowHeader.svelte reads this as
  // a prop rather than keeping its own local copy (see that prop's doc
  // comment for why: local state desynced from the real overlay).
  let aiChatOverlayOpen = $state(false);
  // `remoteControl=1` is set from the sharer's `petalWindowScales` participant
  // metadata, which ONLY the native publisher writes — a browser peer never
  // does. So this doubles as "the owner is a native client that could host a
  // session at all", which is the precondition AI chat needs. See
  // `aiChatHeaderControlVisible` for why keying on it fails safe.
  const aiChatNativeSource = $derived(remoteControlAvailable);
  const aiChatVisible = $derived(
    aiChatHeaderControlVisible({
      settingEnabled: aiChatEnabled,
      nativeSource: aiChatNativeSource,
      windowId,
      ownerIdentity
    })
  );
  const aiChatActive = $derived(aiChatVisible && aiChatSession?.active === true);
  const aiChatError = $derived(aiChatSession?.error ?? null);
  const aiChatActiveSpeaker = $derived(aiChatSession?.activeSpeaker ?? null);
  // Fetched once on mount (below), independent of remoteControl state --
  // unlike `latencySnapshot`, AI chat's PTT floor check needs this whenever
  // a session might be active, not only while remote-controlling.
  let aiChatLocalIdentity = $state<string | null>(null);

  // ---- Debug mode (#669) -----------------------------------------------
  // The remote-window header's Debug button is gated on a Rust-owned user
  // setting (default OFF), not localStorage -- this webview is its own JS
  // realm, so a Settings-window toggle stored in localStorage would never
  // reach an already-open window like this one. `debugHeaderControlVisible`
  // is the single shared predicate (also consumed by web-harness) composing
  // the setting with the two existing layout suppressors: the AI chat live
  // disclosure (`aiChatActive` already implies `aiChatVisible`, so it alone
  // is the right "AI chat live" signal here) and the header's own measured
  // `@media (max-width: 640px)` breakpoint, which stays CSS-enforced --
  // `viewportWidth` is passed as always-satisfied so the JS predicate only
  // adds the two NEW gates without duplicating that already-working rule.
  let debugModeEnabled = $state(false);
  const debugShown = $derived(
    debugHeaderControlVisible({
      debugModeEnabled,
      aiChatLive: aiChatActive,
      viewportWidth: Number.POSITIVE_INFINITY
    })
  );

  // #376 item 4: only polled while actively controlling -- this is a
  // "while controlling" affordance on the Control pill, not a standing debug
  // feature, so there's no reason to pay the network-snapshot cost outside
  // an active session.
  let latencySnapshot = $state<NetworkSnapshot | null>(null);
  let latencyTimer: ReturnType<typeof setInterval> | undefined;
  const latencyTrack = $derived(findRemoteWindowDebugTrack(latencySnapshot, ownerIdentity, windowId));
  const remoteControlLatency = $derived(
    remoteControlActive ? formatGlassToGlassLatencyChip(latencyTrack) : null
  );

  $effect(() => {
    sourceUrl = initialSourceUrl && isWebUrl(initialSourceUrl) ? initialSourceUrl : null;
  });

  function isWebUrl(url: string): boolean {
    return url.startsWith('https://') || url.startsWith('http://');
  }

  function onMouseDown(event: MouseEvent) {
    // Only the left button starts a drag; ignore right-click/context-menu
    // clicks. Also refuse to start a drag when the mousedown landed on one
    // of the header's own interactive controls: `compositor_start_drag`
    // begins a NATIVE window-drag session immediately on mousedown, which
    // swallows the subsequent mouseup/click — so starting it over a button
    // makes that button dead (dragging worked, clicks never fired). The
    // header's icon buttons also stop mousedown propagation themselves
    // (belt-and-braces, see RemoteWindowHeader.svelte), so this only fires
    // for background-of-header drags — "the header is the drag handle" per
    // SPEC.md §4.4, same as a native titlebar.
    if (event.button !== 0) return;
    if ((event.target as HTMLElement).closest('button, a, input, select')) return;
    invoke(COMMANDS.compositorStartDrag, { windowId, ownerIdentity }).catch(() => {});
  }

  function onOpenSourceUrl() {
    if (!sourceUrl || !isWebUrl(sourceUrl)) return;
    openUrl(sourceUrl).catch(() => {});
  }

  function clearRemoteControlTimeout() {
    clearTimeout(remoteControlTimeout);
    remoteControlTimeout = undefined;
  }

  function clearRemoteControlFeedbackTimeout() {
    clearTimeout(remoteControlFeedbackTimeout);
    remoteControlFeedbackTimeout = undefined;
  }

  function setRemoteControlFeedback(status: RemoteControlFeedbackStatus, message: string | null) {
    remoteControlStatus = status === 'active' || status === 'stopped' ? null : status;
    remoteControlStatusMessage = remoteControlStatus ? message : null;
  }

  function applyRemoteControlStatus(status: RemoteControlStatus) {
    const effect = remoteControlStatusEffect(status.status);
    // Consent flow: the host parked our request and is asking the sharer.
    // Stay in "requesting" (the button keeps its pending state) but swap the
    // chip to "Waiting for approval" and give the sharer their full 30 s
    // window instead of tripping the 8 s no-answer timeout.
    if (status.status === 'awaitingConsent' && remoteControlRequesting) {
      clearRemoteControlFeedbackTimeout();
      setRemoteControlFeedback(status.status, status.message);
      startRemoteControlTimeout(REMOTE_CONTROL_CONSENT_TIMEOUT_MS, REMOTE_CONTROL_CONSENT_TIMEOUT_MESSAGE);
      return;
    }
    if (remoteControlActive && effect === 'feedback') {
      clearRemoteControlFeedbackTimeout();
      setRemoteControlFeedback(status.status, status.message);
      const shownStatus = status.status;
      remoteControlFeedbackTimeout = setTimeout(() => {
        if (remoteControlStatus === shownStatus) setRemoteControlFeedback(null, null);
      }, 3000);
      return;
    }
    clearRemoteControlTimeout();
    clearRemoteControlFeedbackTimeout();
    remoteControlRequestSerial += 1;
    remoteControlActive = effect === 'activate';
    remoteControlRequesting = false;
    setRemoteControlFeedback(status.status, status.message);
    // A consent deny is an answer, not a permanent state: clear it after the
    // same 3 s the other transient feedback gets so the chip returns to
    // "Request control".
    if (status.status === 'denied') {
      remoteControlFeedbackTimeout = setTimeout(() => {
        if (remoteControlStatus === 'denied') setRemoteControlFeedback(null, null);
      }, 3000);
    }
  }

  function startRemoteControlTimeout(
    timeoutMs: number = REMOTE_CONTROL_REQUEST_TIMEOUT_MS,
    timeoutMessage: string = REMOTE_CONTROL_TIMEOUT_MESSAGE
  ) {
    clearRemoteControlTimeout();
    const serial = ++remoteControlRequestSerial;
    remoteControlTimeout = setTimeout(() => {
      if (serial !== remoteControlRequestSerial || !remoteControlRequesting) return;
      remoteControlActive = false;
      remoteControlRequesting = false;
      setRemoteControlFeedback('requestFailed', timeoutMessage);
      void invoke(COMMANDS.remoteControlRequestTimedOut, { windowId, ownerIdentity }).catch(() => {});
    }, timeoutMs);
  }

  async function onToggleRemoteControl() {
    const next = !remoteControlActive;
    if (remoteControlRequesting) return;
    if (next && drawActive) {
      drawActive = false;
      setDrawActive(false);
    }
    clearRemoteControlTimeout();
    remoteControlRequestSerial += 1;
    setRemoteControlFeedback(null, null);
    remoteControlRequesting = next;
    if (next) startRemoteControlTimeout();
    try {
      const confirmedActive = await invoke<boolean>(COMMANDS.remoteControlSetActive, {
        windowId,
        ownerIdentity,
        active: next
      });
      if (!next) {
        clearRemoteControlTimeout();
        remoteControlActive = false;
        remoteControlRequesting = false;
      } else if (confirmedActive) {
        clearRemoteControlTimeout();
        remoteControlActive = true;
        remoteControlRequesting = false;
      }
    } catch {
      clearRemoteControlTimeout();
      remoteControlActive = false;
      remoteControlRequesting = false;
      setRemoteControlFeedback('requestFailed', 'Remote control request could not be sent.');
    }
  }

  function onHideWindow() {
    if (!Number.isFinite(windowId) || windowId <= 0) return;
    invoke(COMMANDS.compositorHideWindow, { windowId, ownerIdentity }).catch(() => {});
  }

  async function onFitToSource() {
    if (!Number.isFinite(windowId) || windowId <= 0) return;
    try {
      await invoke(COMMANDS.compositorFitToSource, { windowId, ownerIdentity });
    } catch {
      // The share may retire while its header click is being delivered.
    }
  }

  function selectModeFromMenu(mode: 'view' | 'control' | 'draw') {
    if (mode === 'draw') {
      if (!remoteControlRequesting) void onToggleDraw();
      return;
    }
    if (mode === 'control') {
      if (remoteControlAvailable && !remoteControlRequesting) void onToggleRemoteControl();
      return;
    }
    if (drawActive) {
      void onToggleDraw();
    } else if (remoteControlActive && !remoteControlRequesting) {
      void onToggleRemoteControl();
    }
  }

  /**
   * The cached menu is rebuilt when AI chat's visibility flips, never merely
   * disabled: an opted-out user must not see a greyed-out AI chat entry any
   * more than they see a greyed-out hover-tab entry. `@tauri-apps/api/menu`
   * items have no `setVisible`, so absence means a different menu.
   */
  async function ensureModeMenu(withAiChat: boolean) {
    if (modeMenu && menuHasAiChat === withAiChat) return;
    await closeModeMenu();
    const [view, control, draw, aiChat] = await Promise.all([
      CheckMenuItem.new({
        id: 'remote-window-view',
        text: 'View shared window',
        action: () => selectModeFromMenu('view')
      }),
      CheckMenuItem.new({
        id: 'remote-window-control',
        text: 'Request remote control',
        action: () => selectModeFromMenu('control')
      }),
      CheckMenuItem.new({
        id: 'remote-window-draw',
        text: 'Draw on shared window',
        action: () => selectModeFromMenu('draw')
      }),
      // Below 470px the header's AI chat button is replaced by this entry, so
      // the STOP path never disappears just because the window got narrow.
      CheckMenuItem.new({
        id: 'remote-window-ai-chat',
        text: aiChatHeaderAction(false),
        action: () => void onToggleAiChat()
      })
    ]);
    viewModeItem = view;
    controlModeItem = control;
    drawModeItem = draw;
    aiChatModeItem = withAiChat ? aiChat : null;
    menuHasAiChat = withAiChat;
    if (!withAiChat) await aiChat.close();
    modeMenu = await Menu.new({
      items: withAiChat ? [view, control, draw, aiChat] : [view, control, draw]
    });
  }

  async function closeModeMenu() {
    const menu = modeMenu;
    modeMenu = null;
    viewModeItem = null;
    controlModeItem = null;
    drawModeItem = null;
    aiChatModeItem = null;
    menuHasAiChat = false;
    await menu?.close().catch(() => {});
  }

  // Windows-only native control menu on the header's Control segment. A native
  // menu pops above the native video layer (an in-DOM popover is always behind
  // it), so the mode read-out and the "Request full control" action live here.
  let controlMenu: Menu | null = null;
  let ctrlMenuModeItem: MenuItem | null = null;
  let ctrlMenuRequestItem: MenuItem | null = null;

  async function ensureControlMenu() {
    await closeControlMenu();
    const mode = await MenuItem.new({
      id: 'remote-window-control-mode',
      text: remoteControlActive
        ? controlMode === 'fullControl'
          ? 'Mode: full control'
          : 'Mode: cursor-preserving'
        : 'Not controlling',
      enabled: false
    });
    const request = await MenuItem.new({
      id: 'remote-window-request-full-control',
      text: 'Request full control',
      enabled: remoteControlActive && controlMode === 'cursorPreserving',
      action: () => void onRequestEscalation()
    });
    const toggle = await MenuItem.new({
      text: remoteControlActive ? 'Stop remote control' : 'Request remote control',
      action: () => void onToggleRemoteControl()
    });
    ctrlMenuModeItem = mode;
    ctrlMenuRequestItem = request;
    controlMenu = await Menu.new({
      items: [
        mode,
        await PredefinedMenuItem.new({ item: 'Separator' }),
        toggle,
        request
      ]
    });
  }

  async function closeControlMenu() {
    const menu = controlMenu;
    controlMenu = null;
    ctrlMenuModeItem = null;
    ctrlMenuRequestItem = null;
    await menu?.close().catch(() => {});
  }

  async function onOpenControlMenu(event: MouseEvent) {
    const target = event.currentTarget as HTMLElement | null;
    if (!target || !controlModesSupported) return;
    const rect = target.getBoundingClientRect();
    await ensureControlMenu();
    if (!controlMenu) return;
    // Reflect the latest mode/control state at open time.
    await Promise.all([
      ctrlMenuModeItem?.setEnabled(false),
      ctrlMenuModeItem?.setText(
        remoteControlActive
          ? controlMode === 'fullControl'
            ? 'Mode: full control'
            : 'Mode: cursor-preserving'
          : 'Not controlling'
      ),
      ctrlMenuRequestItem?.setEnabled(remoteControlActive && controlMode === 'cursorPreserving')
    ]);
    await controlMenu.popup(new LogicalPosition(rect.left, rect.bottom), getCurrentWindow());
  }

  async function onOpenModeMenu(event: MouseEvent) {
    const target = event.currentTarget as HTMLElement | null;
    if (!target) return;
    const rect = target.getBoundingClientRect();
    // The switch can be flipped in the main window while this panel is open, so
    // re-read it at menu-open time rather than trusting the mount-time value.
    await refreshAiChatEnabled();
    await ensureModeMenu(aiChatVisible);
    if (!modeMenu || !viewModeItem || !controlModeItem || !drawModeItem) return;

    if (aiChatModeItem) {
      await Promise.all([
        aiChatModeItem.setChecked(aiChatActive),
        aiChatModeItem.setText(aiChatHeaderAction(aiChatActive))
      ]);
    }

    await Promise.all([
      viewModeItem.setChecked(!drawActive && !remoteControlActive && !remoteControlRequesting),
      controlModeItem.setChecked(remoteControlActive || remoteControlRequesting),
      drawModeItem.setChecked(drawActive),
      viewModeItem.setEnabled(!remoteControlRequesting),
      controlModeItem.setEnabled(remoteControlAvailable && !remoteControlRequesting),
      drawModeItem.setEnabled(!remoteControlRequesting),
      controlModeItem.setText(
        remoteControlRequesting
          ? 'Requesting remote control'
          : remoteControlActive
            ? 'Stop remote control'
            : remoteControlAvailable
              ? 'Request remote control'
              : 'Preparing remote control…'
      ),
      drawModeItem.setText(drawActive ? 'Stop drawing' : 'Draw on shared window')
    ]);

    await modeMenu.popup(new LogicalPosition(rect.left, rect.bottom), getCurrentWindow());
  }

  async function refreshAiChatEnabled() {
    try {
      const settings = await invoke<AiChatSettings>(COMMANDS.aiChatSettings);
      aiChatEnabled = settings.enabled;
    } catch {
      // A read that fails must fail CLOSED: an unknown setting is not consent.
      aiChatEnabled = false;
    }
  }

  async function refreshDebugModeEnabled() {
    try {
      const settings = await invoke<DebugModeSettings>(COMMANDS.debugModeSettings);
      debugModeEnabled = settings.enabled;
    } catch {
      // Fail closed, same reasoning as refreshAiChatEnabled.
      debugModeEnabled = false;
    }
  }

  async function refreshAiChatSession() {
    if (!Number.isFinite(windowId) || windowId <= 0 || !ownerIdentity) return;
    try {
      aiChatSession = await invoke<AiChatRemoteSessionState | null>(COMMANDS.aiChatRemoteSession, {
        windowId,
        ownerIdentity
      });
    } catch {
      // Keep whatever the event stream last told us; a transient failure must
      // not blank a live-session badge.
    }
  }

  // #844: seed the REAL current overlay open/closed state on mount -- this
  // webview can (re)mount long after the overlay was toggled (a retired-
  // window restore reloads this whole page), so waiting for the next
  // ai-chat-overlay-open-changed event would leave the badge showing a
  // hardcoded `false` that could already be wrong. Same "ask AND listen"
  // shape as refreshAiChatSession above.
  async function refreshAiChatOverlayOpen() {
    if (!Number.isFinite(windowId) || windowId <= 0) return;
    try {
      aiChatOverlayOpen = await invoke<boolean>(COMMANDS.compositorAiChatOverlayIsOpen, {
        windowId,
        ownerIdentity
      });
    } catch {
      // Keep whatever the event stream last told us; a transient failure
      // must not blank a live-open badge.
    }
  }

  async function onRequestEscalation() {
    if (!Number.isFinite(windowId) || windowId <= 0 || !ownerIdentity) return;
    try {
      await invoke(COMMANDS.remoteControlRequestEscalation, { windowId, ownerIdentity });
    } catch (e) {
      console.error(`remote_control_request_escalation(${windowId}) failed`, e);
    }
  }

  // Not `latencySnapshot` (that command's own polling only runs while
  // `remoteControlActive`, so it's `null` most of the time AI chat cares
  // about). This client's own identity never realistically changes
  // mid-session, so one mount-time read is enough.
  // A failed or empty read (most plausible right when this panel opens during
  // a reconnect, while the presence cache is still rebuilding) must not leave
  // `aiChatLocalIdentity` permanently null -- an unresolved identity reads as
  // "someone else holds the floor" even for our OWN grant (see
  // aiChatPttFloorTakenByOther), which can wedge a press. Identity itself is
  // durable for the life of a session once resolved, so a few retries here
  // are the whole fix; nothing re-derives it later.
  async function refreshAiChatLocalIdentity(attemptsLeft = 5) {
    try {
      const roster = await invoke<PresentParticipant[]>(COMMANDS.roomPresence);
      const identity = roster.find((p) => p.isLocal)?.identity ?? null;
      if (identity) {
        aiChatLocalIdentity = identity;
        return;
      }
      throw new Error('local participant not yet in the roster');
    } catch {
      if (attemptsLeft <= 0) return;
      setTimeout(() => void refreshAiChatLocalIdentity(attemptsLeft - 1), 500);
    }
  }

  function onAiChatPttStart() {
    if (!Number.isFinite(windowId) || windowId <= 0 || !ownerIdentity) return;
    invoke(COMMANDS.aiChatRequestPttStart, { windowId, ownerIdentity }).catch(() => {});
  }

  function onAiChatPttEnd() {
    if (!Number.isFinite(windowId) || windowId <= 0 || !ownerIdentity) return;
    invoke(COMMANDS.aiChatRequestPttEnd, { windowId, ownerIdentity }).catch(() => {});
  }

  async function onToggleAiChat() {
    if (!aiChatVisible) return;
    const stopping = aiChatActive;
    // Optimistic, and deliberately only in the STOP direction: showing the
    // badge before the owner confirms would disclose a session that may never
    // start, while clearing it early only ever under-claims.
    if (stopping && aiChatSession) aiChatSession = { ...aiChatSession, active: false };
    try {
      await invoke(stopping ? COMMANDS.aiChatRequestStop : COMMANDS.aiChatRequestStart, {
        windowId,
        ownerIdentity
      });
    } catch {
      // The owner answers over `petal.ai-chat`; a publish that never left is
      // reconciled by the next state event or the next mount.
      await refreshAiChatSession();
    }
  }

  // #844: the transcript/typed-input UI itself now lives in a separate
  // native overlay window (routes/compositor/ai-chat/+page.svelte), created
  // alongside the control/pointer overlays in compositor.rs's ensure_window.
  // This just show/hides it -- sending is that route's own concern now.
  function onToggleAiChatOverlay(open: boolean) {
    if (!Number.isFinite(windowId) || windowId <= 0) return;
    invoke(COMMANDS.compositorSetAiChatOverlayOpen, { windowId, ownerIdentity, open }).catch(() => {});
  }

  function onToggleDebug() {
    if (!Number.isFinite(windowId) || windowId <= 0) return;
    invoke(COMMANDS.compositorToggleDebugPanel, { windowId, ownerIdentity }).catch(() => {});
  }

  function setDrawActive(value: boolean) {
    invoke(COMMANDS.compositorSetDrawActive, { windowId, ownerIdentity, active: value }).catch(() => {});
  }

  async function onToggleDraw() {
    if (!Number.isFinite(windowId) || windowId <= 0 || remoteControlRequesting) return;
    const next = !drawActive;
    if (next && remoteControlActive) {
      try {
        await invoke<boolean>(COMMANDS.remoteControlSetActive, {
          windowId,
          ownerIdentity,
          active: false
        });
      } catch {
        // Draw mode is local to the viewer; still flip the local capture layer.
      }
      clearRemoteControlTimeout();
      remoteControlActive = false;
      remoteControlRequesting = false;
      setRemoteControlFeedback(null, null);
    }
    drawActive = next;
    setDrawActive(next);
  }

  function onResizePointerDown(event: PointerEvent, direction: CompositorResizeDirection) {
    void beginCompositorResizeDrag(event, windowId, ownerIdentity, direction);
  }

  async function refreshFreshness() {
    if (!Number.isFinite(windowId) || windowId <= 0) return;
    try {
      const stats = await invoke<RemoteWindowDebugStats>(COMMANDS.compositorWindowDebugStats, {
        windowId,
        ownerIdentity
      });
      lastFrameReceivedMs = stats.lastFrameReceivedMs;
    } catch {
      // A window can retire while this panel is unwinding; retain the last
      // known value until the panel itself is destroyed.
    }
  }

  async function refreshLatency() {
    if (!remoteControlActive) return;
    try {
      latencySnapshot = await invoke<NetworkSnapshot>(COMMANDS.getNetworkSnapshot);
    } catch {
      // Keep the last known value; a transient failure shouldn't blank the chip.
    }
  }

  function startLatencyPolling() {
    clearInterval(latencyTimer);
    latencyTimer = setInterval(() => void refreshLatency(), 1000);
    void refreshLatency();
  }

  function stopLatencyPolling() {
    clearInterval(latencyTimer);
    latencyTimer = undefined;
    latencySnapshot = null;
  }

  $effect(() => {
    if (remoteControlActive) startLatencyPolling();
    else stopLatencyPolling();
  });

  onMount(() => {
    void refreshFreshness();
    freshnessTimer = setInterval(() => void refreshFreshness(), 1000);
    // Ask AND listen. This panel can mount long after a session went live (a
    // source republish retires and re-reveals the window, and the surface
    // webview is re-navigated whenever metadata changes), so waiting for the
    // next event would leave a running session undisclosed until it happened
    // to tick.
    void refreshAiChatEnabled();
    void refreshAiChatSession();
    void refreshAiChatOverlayOpen();
    void refreshAiChatLocalIdentity();
    void refreshDebugModeEnabled();
    // Belt-and-braces: `set_debug_mode` emits this so a toggle in an already-
    // open Settings window reaches this window's Debug button live, unlike
    // `ai_chat_set_enabled` (a known, documented gap this setting fixes).
    listen<DebugModeSettings>(EVENTS.debugModeChanged, (event) => {
      debugModeEnabled = event.payload.enabled;
    })
      .then((unlisten) => {
        unlistenDebugModeChanged = unlisten;
      })
      .catch(() => {});
    listen<AiChatRemoteSessionState>(EVENTS.aiChatRemoteState, (event) => {
      if (event.payload.windowId !== windowId) return;
      if (event.payload.ownerIdentity !== ownerIdentity) return;
      aiChatSession = event.payload;
    })
      .then((unlisten) => {
        unlistenAiChatRemoteState = unlisten;
      })
      .catch(() => {});
    listen<AiChatOverlayOpenChangedEvent>(EVENTS.aiChatOverlayOpenChanged, (event) => {
      if (event.payload.windowId !== windowId) return;
      if (event.payload.ownerIdentity !== ownerIdentity) return;
      aiChatOverlayOpen = event.payload.open;
    })
      .then((unlisten) => {
        unlistenAiChatOverlayOpenChanged = unlisten;
      })
      .catch(() => {});
    // #844: the transcript itself is no longer accumulated here -- the
    // ai-chat overlay route owns it directly (see that route's doc comment
    // for why it can't use `listen` the way this panel webview can).
    const surfaceWindow = window as typeof window & {
      __petalRemoteControlHeaderActive?: (value: boolean) => void;
      __petalRemoteControlMode?: (value: string) => void;
    };
    surfaceWindow.__petalRemoteControlMode = (value: string) => {
      // Control-mode metadata can change while the grant is active. The
      // Windows compositor updates this one field in-place so a live surface
      // does not navigate and reset the controller's active/grant UI.
      controlMode = value === 'fullControl' ? 'fullControl' : 'cursorPreserving';
    };
    surfaceWindow.__petalRemoteControlHeaderActive =
      (value: boolean) => {
        clearRemoteControlTimeout();
        remoteControlRequestSerial += 1;
        remoteControlActive = value;
        if (value && drawActive) {
          drawActive = false;
          setDrawActive(false);
        }
        remoteControlRequesting = false;
        setRemoteControlFeedback(null, null);
      };
    (window as typeof window & { __petalRemoteControlHeaderStatus?: (status: string) => void }).__petalRemoteControlHeaderStatus =
      (status: string) => {
        applyRemoteControlStatus({
          windowId,
          controllerId: ownerName,
          status,
          message: 'Remote control status changed'
        });
      };
    (window as typeof window & { __petalRemoteWindowMediaPaused?: (value: boolean) => void }).__petalRemoteWindowMediaPaused =
      (value: boolean) => {
        mediaPaused = value;
      };
    listen<RemoteControlStatus>(EVENTS.remoteControlStatus, (event) => {
      if (event.payload.windowId !== windowId) return;
      if (ownerIdentity && event.payload.ownerIdentity && event.payload.ownerIdentity !== ownerIdentity) return;
      applyRemoteControlStatus(event.payload);
    })
      .then((unlisten) => {
        unlistenRemoteControlStatus = unlisten;
      })
      .catch(() => {});

    return () => {
      unlistenRemoteControlStatus?.();
      unlistenRemoteControlStatus = undefined;
      delete surfaceWindow.__petalRemoteControlMode;
      unlistenAiChatRemoteState?.();
      unlistenAiChatRemoteState = undefined;
      unlistenAiChatOverlayOpenChanged?.();
      unlistenAiChatOverlayOpenChanged = undefined;
      unlistenDebugModeChanged?.();
      unlistenDebugModeChanged = undefined;
      clearRemoteControlTimeout();
      clearRemoteControlFeedbackTimeout();
      clearInterval(freshnessTimer);
      freshnessTimer = undefined;
      stopLatencyPolling();
      void closeModeMenu();
      void closeControlMenu();
    };
  });
</script>

<!--
  The header strip lives at the top; the region below it is left entirely to
  the native video NSView (layered in front of this webview). The drag handle
  is strip-height so a mousedown over the video area can never be intercepted
  here to start a spurious window-drag. Edge-resize grips ride in the header
  strip, the only part of this webview not covered by the video.
-->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="remote-window-chrome">
  <div class="drag-handle" onmousedown={onMouseDown}>
    <RemoteWindowHeader
      {ownerName}
      {identity}
      {sourceTitle}
      {mediaPaused}
      {sourceUrl}
      autoHide={false}
      {onOpenSourceUrl}
      {onHideWindow}
      {onFitToSource}
      {onOpenModeMenu}
      {remoteControlActive}
      {remoteControlRequesting}
      {remoteControlStatus}
      {remoteControlStatusMessage}
      {remoteControlAvailable}
      {remoteControlDisallowed}
      {onToggleRemoteControl}
      {controlMode}
      {onRequestEscalation}
      {controlModesSupported}
      {onOpenControlMenu}
      {onToggleDebug}
      {debugShown}
      {drawActive}
      {onToggleDraw}
      {remoteControlLatency}
      {aiChatVisible}
      {aiChatActive}
      {aiChatError}
      {onToggleAiChat}
      {onToggleAiChatOverlay}
      {aiChatOverlayOpen}
      {aiChatActiveSpeaker}
      localIdentity={aiChatLocalIdentity}
      onPttStart={onAiChatPttStart}
      onPttEnd={onAiChatPttEnd}
    />
    <div class="resize-zones">
      <button type="button" tabindex="-1" aria-label="Resize north" class="resize-zone resize-n" onpointerdown={(event) => onResizePointerDown(event, 'North')}></button>
      <button type="button" tabindex="-1" aria-label="Resize west" class="resize-zone resize-w" onpointerdown={(event) => onResizePointerDown(event, 'West')}></button>
      <button type="button" tabindex="-1" aria-label="Resize east" class="resize-zone resize-e" onpointerdown={(event) => onResizePointerDown(event, 'East')}></button>
      <button type="button" tabindex="-1" aria-label="Resize north west" class="resize-zone resize-nw" onpointerdown={(event) => onResizePointerDown(event, 'NorthWest')}></button>
      <button type="button" tabindex="-1" aria-label="Resize north east" class="resize-zone resize-ne" onpointerdown={(event) => onResizePointerDown(event, 'NorthEast')}></button>
    </div>
  </div>
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent !important;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .remote-window-chrome {
    width: 100vw;
    height: 100vh;
    overflow: hidden;
    background: transparent;
    box-sizing: border-box;
  }

  .drag-handle {
    position: relative;
    width: 100%;
    background: transparent;
    box-sizing: border-box;
  }

  .resize-zones {
    position: absolute;
    inset: 0;
    z-index: 3;
    pointer-events: none;
  }

  .resize-zone {
    position: absolute;
    border: 0;
    padding: 0;
    background: transparent;
    pointer-events: auto;
  }

  .resize-n::after,
  .resize-e::after,
  .resize-w::after {
    content: '';
    position: absolute;
    opacity: 0.58;
    /* Overlay-chrome resize handle — kept literal (uiConsistency allowlist). */
    background: rgba(255, 255, 255, 0.55);
    border-radius: var(--radius-pill);
  }

  .resize-n {
    top: 0;
    left: 28px;
    right: 28px;
    height: 8px;
    cursor: ns-resize;
  }

  .resize-n::after {
    top: 2px;
    left: 50%;
    width: 54px;
    height: 2px;
    transform: translateX(-50%);
  }

  .resize-e,
  .resize-w {
    top: 10px;
    bottom: 0;
    width: 12px;
    cursor: ew-resize;
  }

  .resize-e {
    right: 0;
  }

  .resize-w {
    left: 0;
  }

  .resize-e::after,
  .resize-w::after {
    top: 9px;
    width: 2px;
    height: 16px;
  }

  .resize-e::after {
    right: 3px;
  }

  .resize-w::after {
    left: 3px;
  }

  .resize-ne,
  .resize-nw {
    top: 0;
    width: 28px;
    height: 28px;
    cursor: nesw-resize;
  }

  .resize-nw {
    left: 0;
    cursor: nwse-resize;
  }

  .resize-ne {
    right: 0;
  }

</style>
