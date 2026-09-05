<!--
  RemoteWindowHeader — header bar for a received shared window. The visual
  contract is decoded in docs/design/shared-window-header.html: 44px graphite
  bar, functional macOS-style traffic dots, the "<source> by <owner>" title,
  fixed 86px View / Control / Draw segments, and transient state in a status
  chip. (The source app avatar/logo was removed per user request.)
-->
<script lang="ts">
  import type { IdentityColor } from './Avatar.svelte';
  import { identityHeaderCss } from '$lib/data/identityColor';
  import { isWindows } from '$lib/platform';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { formatRemoteWindowHeaderTitle, remoteWindowSourceLabel } from '$lib/ipc';
  import {
    remoteControlFeedbackLabel,
    remoteControlFeedbackTitle,
    type RemoteControlFeedbackStatus,
    remoteControlFeedbackIsNeutral
  } from '$lib/remoteControlFeedback';
  import type { GlassToGlassChip } from '$lib/data/remoteWindowDebug';
  import {
    AI_CHAT_ACTIVE_BADGE_LABEL,
    AI_CHAT_ACTIVE_DISCLOSURE,
    AI_CHAT_UNAVAILABLE_LABEL,
    aiChatEndReasonMessage,
    aiChatHeaderAction,
    aiChatHeaderLabel,
    aiChatHeaderTitle,
    aiChatHeaderWarning,
    aiChatPttFloorTakenByOther,
    aiChatRemotePttDisabled,
    aiChatRemotePttLabel
  } from '$lib/data/aiChat';
  import type { AiChatEndReason } from '$lib/ipc';
  import { sparkleIconSvg } from '@petal/shared/ui/icons';

  interface Props {
    ownerName: string;
    /** Deprecated visual input retained for existing call sites; this header now uses the design's generic app avatar. */
    identity: IdentityColor;
    /** e.g. "main.rs — vscode" */
    sourceTitle: string;
    mediaPaused?: boolean;
    sourceUrl?: string | null;
    /** The spotlighted/full-rate window (SPEC.md §4.7) stays revealed, with
     * no separate focused visual treatment in the decoded header design. */
    focused?: boolean;
    /** When false, the header never idle-collapses to the accent sliver —
     * it stays fully visible. The real compositor windows use this (the
     * header is the user's ONLY handle to move/resize a borderless remote
     * window, so a near-invisible 2.5px idle sliver made the window look
     * like it "has no top bar" in real use — reported by the user); the M4
     * idle-autohide remains the default for previews/dev harnesses. */
    autoHide?: boolean;
    onOpenSourceUrl?: () => void;
    onHideWindow?: () => void;
    onFitToSource?: () => void;
    onOpenModeMenu?: (event: MouseEvent) => void;
    remoteControlActive?: boolean;
    remoteControlRequesting?: boolean;
    remoteControlStatus?: RemoteControlFeedbackStatus;
    remoteControlStatusMessage?: string | null;
    remoteControlAvailable?: boolean;
    /** The sharer denied control of this window. Permanent, unlike
     *  `!remoteControlAvailable`, which only means metadata is still in
     *  flight -- so this HIDES the segment rather than showing 'Preparing…'. */
    remoteControlDisallowed?: boolean;
    onToggleRemoteControl?: () => void;
    /** Sharer-selected host mode for this share (read-only on the controller).
     * Absent => cursor-preserving. Drives the read-only mode chip and whether
     * a "Request full control" affordance is shown. */
    controlMode?: 'cursorPreserving' | 'fullControl';
    /** Controller asks the sharer for full control (host-authoritative; the
     * sharer approves/denies; Petal never auto-escalates). */
    onRequestEscalation?: () => void;
    /** Windows-only: opens a native control menu (mode + request full
     * control) from the Control segment. A native menu floats above the
     * native video layer, unlike an in-DOM popover. */
    onOpenControlMenu?: (event: MouseEvent) => void;
    /** Remote-control modes are a WINDOWS-only feature; the caller supplies
     * this so macOS never renders the mode chip or escalation affordance. */
    controlModesSupported?: boolean;
    onToggleDebug?: () => void;
    /** Debug mode (#669): the host computes this from the user's setting
     * composed with the AI-chat-live and narrow-viewport suppressors via the
     * shared `debugHeaderControlVisible` predicate. Absent — not disabled —
     * when off, exactly like `aiChatVisible`; defaults to false so a caller
     * that omits it fails closed rather than showing the button. */
    debugShown?: boolean;
    drawActive?: boolean;
    onToggleDraw?: () => void;
    /** #376 item 4: shown as a small muted chip on the Control pill, only
     * while `remoteControlActive`. Null hides the chip entirely -- never
     * show a placeholder in place of real data. */
    remoteControlLatency?: GlassToGlassChip | null;
    /**
     * AI chat (#657 receiver half). Absent — not disabled — whenever the master
     * switch is off or the sharer is a browser peer that cannot host a session;
     * the host decides that, via `aiChatHeaderControlVisible`.
     */
    aiChatVisible?: boolean;
    /** A session is running on the OWNER's machine for this window. */
    aiChatActive?: boolean;
    /** Last refusal/end reason from the owner, for the button's tooltip. */
    aiChatError?: AiChatEndReason | null;
    onToggleAiChat?: () => void;
    /**
     * #844: show/hide the receiver-side AI-chat transcript/input overlay (a
     * separate native window layered above the video -- the header webview
     * itself only covers the 44px strip, so a transcript/input popover drawn
     * here would render behind the video and be unclickable). Called with the
     * NEXT open/closed state on every badge click.
     */
    onToggleAiChatOverlay?: (open: boolean) => void;
    /**
     * #844: the overlay's REAL current open/closed state, as Rust reports it
     * (`COMMANDS.compositorAiChatOverlayIsOpen` on mount +
     * `EVENTS.aiChatOverlayOpenChanged` thereafter -- the host owns both).
     * Deliberately NOT local state here: an earlier version toggled a local
     * copy on click, which desynced from the real overlay in two ways --
     * the overlay's own Escape-to-close went straight to the Rust command,
     * leaving the badge stuck showing "open" until a wasted reconciling
     * click; and a retired-window restore reloads this whole header webview,
     * resetting any local copy to a hardcoded `false` that could already be
     * wrong. Absent reads as closed, matching every other boolean prop here.
     */
    aiChatOverlayOpen?: boolean;
    /** Who currently holds the push-to-talk floor, if anyone -- from the
     * owner's own `state` broadcast (`AiChatRemoteSessionState.activeSpeaker`). */
    aiChatActiveSpeaker?: string | null;
    /** This client's own room identity, to tell "I hold the floor" apart
     * from "someone else does". */
    localIdentity?: string | null;
    /** Claim the push-to-talk floor on this window. Fire-and-forget --
     * there is no optimistic local floor state; the button reflects
     * `aiChatActiveSpeaker` once the owner's `state` broadcasts it. */
    onPttStart?: () => void;
    onPttEnd?: () => void;
  }

  type HeaderMode = 'view' | 'control' | 'draw';

  let {
    ownerName,
    identity,
    sourceTitle,
    mediaPaused = false,
    sourceUrl = null,
    focused = false,
    autoHide = true,
    onOpenSourceUrl,
    onHideWindow,
    onFitToSource,
    onOpenModeMenu,
    remoteControlActive = false,
    remoteControlRequesting = false,
    remoteControlStatus = null,
    remoteControlStatusMessage = null,
    remoteControlAvailable = false,
    remoteControlDisallowed = false,
    onToggleRemoteControl,
    controlMode = 'cursorPreserving',
    onRequestEscalation,
    onOpenControlMenu,
    controlModesSupported = false,
    onToggleDebug,
    debugShown = false,
    drawActive = false,
    onToggleDraw,
    remoteControlLatency = null,
    aiChatVisible = false,
    aiChatActive = false,
    aiChatError = null,
    onToggleAiChat,
    onToggleAiChatOverlay,
    aiChatOverlayOpen = false,
    aiChatActiveSpeaker = null,
    localIdentity = null,
    onPttStart,
    onPttEnd
  }: Props = $props();

  let aiChatPttPressed = $state(false);

  // No optimistic local floor state (unlike the LOCAL panel's own PTT
  // button): the owner's `state` broadcast is the only source of truth for
  // who holds a REMOTE floor, so this reflects it directly rather than
  // guessing ahead of the round trip.
  const aiChatFloorTakenByOther = $derived(
    aiChatPttFloorTakenByOther(aiChatActiveSpeaker, localIdentity)
  );
  const aiChatPttLabel = $derived(
    aiChatRemotePttLabel(aiChatPttPressed, aiChatFloorTakenByOther, aiChatActiveSpeaker)
  );

  const aiChatPttDisabled = $derived(
    aiChatRemotePttDisabled(aiChatActive, aiChatFloorTakenByOther, aiChatPttPressed)
  );

  function startAiChatPtt() {
    if (aiChatPttPressed || aiChatFloorTakenByOther || !aiChatActive) return;
    aiChatPttPressed = true;
    addGlobalPttGuards();
    onPttStart?.();
  }

  function endAiChatPtt() {
    if (!aiChatPttPressed) return;
    aiChatPttPressed = false;
    removeGlobalPttGuards();
    onPttEnd?.();
  }

  function endAiChatPttIfHidden() {
    if (document.visibilityState === 'hidden') endAiChatPtt();
  }

  // Belt-and-braces, mirroring AiChatPanel.svelte's addGlobalPttGuards: a
  // pointerup that lands outside the button, or the window losing focus or
  // visibility mid-press, still has to end the turn. This is not redundant
  // with the element's own onpointerup/onpointerleave/onblur -- a compositor
  // window RETIRE (source republish) hides this panel's NSPanel without
  // destroying the webview, so the button's own handlers never fire, but
  // `visibilitychange` does.
  function addGlobalPttGuards() {
    if (typeof window === 'undefined') return;
    window.addEventListener('pointerup', endAiChatPtt);
    window.addEventListener('pointercancel', endAiChatPtt);
    window.addEventListener('blur', endAiChatPtt);
    document.addEventListener('visibilitychange', endAiChatPttIfHidden);
  }

  function removeGlobalPttGuards() {
    if (typeof window === 'undefined') return;
    window.removeEventListener('pointerup', endAiChatPtt);
    window.removeEventListener('pointercancel', endAiChatPtt);
    window.removeEventListener('blur', endAiChatPtt);
    document.removeEventListener('visibilitychange', endAiChatPttIfHidden);
  }

  // A lost floor race: someone else's grant arrived while we were pressed
  // (or our own grant arrived under an unresolved localIdentity, which reads
  // the same way). Force a local release rather than staying wedged behind
  // a disabled button whose own pointerup this webview may never deliver.
  $effect(() => {
    if (aiChatPttPressed && aiChatFloorTakenByOther) endAiChatPtt();
  });

  // #844: don't leave the overlay open on a dead session -- the session
  // ending is the owner stopping it, the sharer disabling the setting, or the
  // window itself going away, none of which the overlay's own controls can
  // detect. Belt-and-braces release the floor too: a session ending while
  // this client held it must not leave a phantom `pttEnd` unsent, even though
  // the owner's own teardown already clears the floor on its side.
  $effect(() => {
    if (aiChatActive) return;
    // Rust is the single source of truth for `aiChatOverlayOpen` (see the
    // prop's own doc comment) -- this only REQUESTS the close; the prop
    // itself updates once Rust processes it and emits the change back.
    if (aiChatOverlayOpen) onToggleAiChatOverlay?.(false);
    if (aiChatPttPressed) endAiChatPtt();
  });

  function toggleAiChatOverlay() {
    onToggleAiChatOverlay?.(!aiChatOverlayOpen);
  }

  const sourceLabel = $derived(remoteWindowSourceLabel(sourceTitle));
  const ownerLabel = $derived(ownerName.trim() || 'Someone');
  const displayTitle = $derived(formatRemoteWindowHeaderTitle(sourceTitle, ownerName));
  const remoteControlFeedback = $derived(remoteControlFeedbackLabel(remoteControlStatus));
  const remoteControlFeedbackWarning = $derived(
    !!remoteControlFeedback &&
      remoteControlStatus !== 'requestUnavailable' &&
      !remoteControlFeedbackIsNeutral(remoteControlStatus)
  );
  // Consent flow: while the sharer is being asked we are still "requesting"
  // (button pending), but the chip must say so -- "Waiting for approval"
  // beats a 30-second "Requesting control".
  const remoteControlAwaitingConsent = $derived(remoteControlStatus === 'awaitingConsent');
  const remoteControlFeedbackTitleText = $derived(
    remoteControlFeedbackTitle(remoteControlStatus, remoteControlStatusMessage)
  );
  const activeMode = $derived<HeaderMode>(
    drawActive ? 'draw' : remoteControlActive || remoteControlRequesting ? 'control' : 'view'
  );
  // The sharer forbade control: don't offer it at all. Hiding beats a
  // disabled button, which invites clicking and then explains nothing.
  const controlSegmentShown = $derived(!remoteControlDisallowed);
  const activeModeIndex = $derived(
    activeMode === 'control' ? 1 : activeMode === 'draw' ? (controlSegmentShown ? 2 : 1) : 0
  );
  // #376 item 2: `remoteControlAvailable` is false only while the sender's
  // source-scale metadata hasn't arrived yet (transport/subscriber.rs) -- it
  // is never a permanent "this window can't be controlled" signal, so the
  // segment stays disabled but frames it as transient ("Preparing...")
  // rather than a flat, alarming "unavailable". The prop flips true (and
  // this button re-enables, with no user action needed) as soon as the
  // metadata lands and the surface webview picks up the refreshed query
  // string (compositor.rs `update_window_metadata` -> `refresh_header_webview`).
  const remoteControlPreparing = $derived(
    !remoteControlAvailable &&
      !remoteControlDisallowed &&
      !remoteControlActive &&
      !remoteControlRequesting
  );
  const remoteControlTitle = $derived(
    remoteControlRequesting
      ? 'Requesting control'
      : remoteControlActive
        ? 'Remote control active'
        : remoteControlAvailable
          ? (remoteControlFeedbackTitleText ?? 'Request control')
          : 'Preparing remote control… it will enable automatically.'
  );
  // The control is only ever rendered when the HOST says so; it never
  // re-derives visibility from an unrelated prop, so the button and the badge
  // can never disagree about whether this window has AI chat at all.
  const aiChatShown = $derived(aiChatVisible && !!onToggleAiChat);
  const aiChatLabel = $derived(aiChatHeaderLabel(aiChatActive));
  const aiChatActionLabel = $derived(aiChatHeaderAction(aiChatActive));
  const aiChatTitleText = $derived(aiChatHeaderTitle(aiChatActive, aiChatError));
  const aiChatWarning = $derived(aiChatHeaderWarning(aiChatActive, aiChatError));
  // A live session must stay disclosed for its WHOLE duration. This webview
  // renders only the 44px header strip (the decoded video NSView covers the
  // rest), so the header itself is the only surface a badge can live on — which
  // means the header must not be allowed to idle-collapse away while a session
  // runs. The web client puts its badge on the tile for the same reason.
  const aiChatDisclosureHeld = $derived(aiChatShown && aiChatActive);

  const statusText = $derived(
    remoteControlRequesting && !remoteControlAwaitingConsent
      ? 'Requesting control'
      : remoteControlFeedback
      ? remoteControlFeedback
      : mediaPaused
        ? 'Video paused'
        : null
  );
  const statusTitle = $derived(
    remoteControlRequesting && !remoteControlAwaitingConsent
      ? 'Requesting control from the shared Mac'
      : (remoteControlFeedbackTitleText ?? statusText)
  );

  // Hosts may wrap this header in a drag-handle that starts a NATIVE window
  // drag on mousedown (see routes/compositor/surface/+page.svelte) — a native
  // drag session swallows the following mouseup/click, killing button clicks.
  // Stop mousedown propagation at each button so no wrapper can regress this.
  function stopMouseDown(event: MouseEvent) {
    event.stopPropagation();
  }

  function selectMode(mode: HeaderMode) {
    if (mode === 'draw') {
      if (remoteControlRequesting) return;
      if (!drawActive) onToggleDraw?.();
      return;
    }
    if (mode === 'control') {
      if (!remoteControlAvailable || remoteControlRequesting || remoteControlActive) return;
      onToggleRemoteControl?.();
      return;
    }
    if (drawActive) {
      onToggleDraw?.();
      return;
    }
    if (remoteControlActive && !remoteControlRequesting) {
      onToggleRemoteControl?.();
    }
  }

  // #669 bonus a11y fix: the web client's Debug button already tracks
  // pressed/active state (aria-pressed + a Show/Hide label) --
  // web-harness/src/remoteWindowHeader.ts's `debugActive`. Native's toggle
  // command (`compositor_toggle_debug_panel`) is fire-and-forget into a
  // SEPARATE control-overlay webview that owns the real panel, so there is no
  // downstream truth to query here -- this is a local optimistic mirror of
  // the last click, the same shape web-harness's own `debugActive` already
  // is (also local, also optimistic, never a query into some other truth).
  let debugActive = $state(false);
  const debugActionLabel = $derived(debugActive ? 'Hide debug stats' : 'Show debug stats');

  function onDebugClick() {
    debugActive = !debugActive;
    onToggleDebug?.();
  }

  function onAiChatClick() {
    if (!aiChatShown) return;
    onToggleAiChat?.();
  }

  function onTrafficHide() {
    onHideWindow?.();
  }

  function onTrafficFit() {
    onFitToSource?.();
  }

  function onWinMinimize() {
    getCurrentWindow()
      .minimize()
      .catch(() => {
        console.error('compositor header: minimize failed');
      });
  }

  function onWinMaximize() {
    getCurrentWindow()
      .toggleMaximize()
      .catch(() => {
        console.error('compositor header: toggleMaximize failed');
      });
  }

  function onOverflowClick(event: MouseEvent) {
    onOpenModeMenu?.(event);
  }

  const IDLE_DELAY_MS = 1800;
  let revealed = $state(true);
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let headerEl = $state<HTMLElement>();

  function scheduleIdle() {
    clearTimeout(idleTimer);
    // Focused headers stay legible, but focus does not add any visual chrome.
    // `autoHide === false` opts a host out of idle-collapse entirely.
    // A live AI chat session pins the header open no matter what the host asked
    // for: the disclosure that this window's content and the room's voice are
    // going to a third party has to be visible for the whole session, and this
    // strip is the only pixels this webview owns.
    if (focused || !autoHide || aiChatDisclosureHeld) return;
    idleTimer = setTimeout(() => {
      // Never collapse out from under a focused field: the AI-chat text input
      // can be mid-composition when the delay elapses, and the `focused` PROP
      // (window focus) does not track DOM focus inside the strip.
      if (headerEl?.contains(document.activeElement)) return;
      revealed = false;
    }, IDLE_DELAY_MS);
  }

  function reveal() {
    revealed = true;
    scheduleIdle();
  }

  $effect(() => {
    // Re-evaluate whenever focus changes: focused cancels pending hide.
    // A live session does the same, and additionally RE-REVEALS an already
    // idle-collapsed header — a session that starts while the strip is hidden
    // must not stay undisclosed until the pointer happens to come back.
    if (focused || aiChatDisclosureHeld) {
      clearTimeout(idleTimer);
      revealed = true;
    } else {
      scheduleIdle();
    }
    return () => clearTimeout(idleTimer);
  });
</script>

<div
  bind:this={headerEl}
  class="header"
  class:idle={!revealed}
  class:ai-chat-live={aiChatDisclosureHeld}
  class:focused
  onpointerenter={reveal}
  onpointermove={reveal}
  onfocusin={reveal}
  data-identity={identity}
  style={identityHeaderCss(identity)}
  role="group"
  aria-label="Remote window header"
>
  <div class="left-cluster">
    <div class="traffic-lights" role="group" aria-label="Window controls">
      {#if isWindows()}
        <button
          type="button"
          class="win-ctl win-min"
          onclick={onWinMinimize}
          aria-label="Minimize remote window"
          onmousedown={stopMouseDown}
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 5h10" stroke="currentColor" stroke-width="1.5"></path>
          </svg>
        </button>
        <button
          type="button"
          class="win-ctl win-max"
          onclick={onWinMaximize}
          aria-label="Maximize remote window"
          onmousedown={stopMouseDown}
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect x="0.75" y="0.75" width="8.5" height="8.5" fill="none" stroke="currentColor" stroke-width="1.5"></rect>
          </svg>
        </button>
      {:else}
        <button
          type="button"
          class="traffic-dot traffic-hide"
          onclick={onTrafficHide}
          disabled={!onHideWindow}
          aria-label="Hide remote window"
          onmousedown={stopMouseDown}
        ></button>
        <button
          type="button"
          class="traffic-dot traffic-fit"
          onclick={onTrafficFit}
          disabled={!onFitToSource}
          aria-label="Fit to source size"
          onmousedown={stopMouseDown}
        ></button>
      {/if}
    </div>

    <div class="title-cluster" title={displayTitle}>
      <span class="title">
        <!-- {' by '} as an expression: Svelte strips a literal leading space at
             the start of element content, which collapsed "Finder by Bob" to
             "Finderby Bob". An expression string is emitted verbatim. -->
        <span class="source-label">{sourceLabel}</span><span class="owner-label">{' by '}{ownerLabel}</span>
      </span>
    </div>
  </div>

  <div class="right-cluster">
    <!-- The session badge. Rendered for the WHOLE session at every width (no
         breakpoint may hide it): it is the disclosure that this window's
         content and the room's voice are leaving for a third-party API, and a
         disclosure that disappears when the window gets narrow is not one.
         The full sentence shows when the bar is wide enough for all of it; the
         short label replaces it otherwise. Neither is ever clipped. -->
    {#if aiChatDisclosureHeld}
      <!-- Compact hold-to-talk, rendered INSIDE the header strip. This
           webview covers only the 44px header (the decoded video is layered
           in front of the rest on both platforms), so the PTT button must
           live where the video can never reach. The transcript/typed-input
           overlay (#844) solves the same problem for the rest of the AI-chat
           UI differently: it is its own native window, layered ABOVE the
           video (see create_ai_chat_overlay in compositor.rs), rather than
           being squeezed into this strip. -->
      {#if onPttStart && onPttEnd}
        <button
          type="button"
          class="ai-chat-header-ptt"
          class:talking={aiChatPttPressed}
          disabled={aiChatPttDisabled}
          aria-pressed={aiChatPttPressed}
          title={aiChatPttLabel}
          onpointerdown={startAiChatPtt}
          onpointerup={endAiChatPtt}
          onpointerleave={endAiChatPtt}
          onpointercancel={endAiChatPtt}
          onblur={endAiChatPtt}
          oncontextmenu={(event) => event.preventDefault()}
          onkeydown={(event) => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              startAiChatPtt();
            }
          }}
          onkeyup={(event) => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              endAiChatPtt();
            }
          }}
        >
          {aiChatPttLabel}
        </button>
      {/if}
      {#if onToggleAiChatOverlay}
        <div class="ai-chat-badge-wrap">
          <button
            type="button"
            class="ai-chat-badge"
            class:open={aiChatOverlayOpen}
            aria-label={AI_CHAT_ACTIVE_DISCLOSURE}
            aria-expanded={aiChatOverlayOpen}
            title={AI_CHAT_ACTIVE_DISCLOSURE}
            onclick={toggleAiChatOverlay}
          >
            <span class="ai-chat-badge-dot" aria-hidden="true">{@html sparkleIconSvg(10)}</span>
            <span class="ai-chat-badge-full" aria-hidden="true">{AI_CHAT_ACTIVE_DISCLOSURE}</span>
            <span class="ai-chat-badge-short" aria-hidden="true">{AI_CHAT_ACTIVE_BADGE_LABEL}</span>
          </button>
        </div>
      {:else}
        <span
          class="ai-chat-badge"
          role="status"
          aria-label={AI_CHAT_ACTIVE_DISCLOSURE}
          title={AI_CHAT_ACTIVE_DISCLOSURE}
        >
          <span class="ai-chat-badge-dot" aria-hidden="true">{@html sparkleIconSvg(10)}</span>
          <span class="ai-chat-badge-full" aria-hidden="true">{AI_CHAT_ACTIVE_DISCLOSURE}</span>
          <span class="ai-chat-badge-short" aria-hidden="true">{AI_CHAT_ACTIVE_BADGE_LABEL}</span>
        </span>
      {/if}
    {/if}

    {#if statusText}
      <span
        class="status-chip"
        class:warning={remoteControlFeedbackWarning}
        class:paused={(!!remoteControlFeedback && !remoteControlFeedbackWarning) || (mediaPaused && !remoteControlFeedback)}
        role="status"
        title={statusTitle}
      >
        <span class="status-chip-dot" aria-hidden="true"></span>
        <span class="status-chip-text">{statusText}</span>
      </span>
    {/if}

    {#if debugShown}
      <button
        type="button"
        class="header-btn debug-btn"
        class:active={debugActive}
        onclick={onDebugClick}
        onmousedown={stopMouseDown}
        aria-label={debugActionLabel}
        aria-pressed={debugActive}
      >
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="m8 2 1.88 1.88"></path>
          <path d="M14.12 3.88 16 2"></path>
          <path d="M9 7.13v-1a3.003 3.003 0 1 1 6 0v1"></path>
          <path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6Z"></path>
          <path d="M12 20v-9"></path>
          <path d="M6.53 9C4.6 8.8 3 7.1 3 5"></path>
          <path d="M6 13H2"></path>
          <path d="M3 21c0-2.1 1.7-3.9 3.8-4"></path>
          <path d="M20.97 5c0 2.1-1.6 3.8-3.5 4"></path>
          <path d="M22 13h-4"></path>
          <path d="M17.2 17c2.1.1 3.8 1.9 3.8 4"></path>
        </svg>
        <span>Debug</span>
      </button>
    {/if}

    {#if sourceUrl && onOpenSourceUrl}
      <button
        type="button"
        class="header-btn open-url-btn"
        onclick={onOpenSourceUrl}
        onmousedown={stopMouseDown}
        aria-label="Open URL"
      >
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M7 7h10v10"></path>
          <path d="M7 17 17 7"></path>
        </svg>
        <span>Open URL</span>
      </button>
    {/if}

    <!-- AI chat (#657 receiver half). The session runs on the OWNER's machine —
         this only ever asks, and the owner answers over `petal.ai-chat`. It
         TOGGLES: a viewer who started a session that streams someone's window
         to a third party must be able to end it from the same control. Below
         470px the labelled native overflow menu carries the action instead
         (same rule as the mode switcher, #497), so the label never squeezes. -->
    {#if aiChatShown}
      {#if aiChatWarning}
        <!-- Compact chip: the SHORT label stays narrow on the top bar; the
             full reason is the native tooltip (an OS popup, so it is neither
             clipped by the window nor covered by the video layer). -->
        <span
          class="ai-chat-error-note"
          title={aiChatEndReasonMessage(aiChatError!)}
        >
          {AI_CHAT_UNAVAILABLE_LABEL}
        </span>
      {/if}
      <button
        type="button"
        class="header-btn ai-chat-btn"
        class:active={aiChatActive}
        class:warning={aiChatWarning}
        onclick={onAiChatClick}
        onmousedown={stopMouseDown}
        aria-label={aiChatActionLabel}
        aria-pressed={aiChatActive}
        title={aiChatTitleText}
      >
        <!-- #847: sparkle, not the old chat-bubble glyph -- distinguishes an AI
             session from ordinary chat/messaging affordances at a glance. -->
        {@html sparkleIconSvg(15)}
        <span>{aiChatLabel}</span>
      </button>
    {/if}

    {#if remoteControlActive && remoteControlLatency}
      <span class="latency-chip" title={remoteControlLatency.title}>{remoteControlLatency.text}</span>
    {/if}

    <div
      class="mode-switcher"
      role="group"
      aria-label="Remote window mode"
      style="--active-mode-index:{activeModeIndex}"
    >
      <span class="active-indicator" aria-hidden="true"></span>
      <button
        type="button"
        class="mode-segment"
        class:active={activeMode === 'view'}
        onclick={() => selectMode('view')}
        onmousedown={stopMouseDown}
        aria-pressed={activeMode === 'view'}
        disabled={remoteControlRequesting}
      >
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z"></path>
          <circle cx="12" cy="12" r="3"></circle>
        </svg>
        <span>View</span>
      </button>
      {#if controlSegmentShown}
      <button
        type="button"
        class="mode-segment control"
        class:active={activeMode === 'control'}
        class:requesting={remoteControlRequesting}
        class:preparing={remoteControlPreparing}
        onclick={(event) => {
          // Windows: the Control segment opens a native menu (mode + request
          // full control) that floats above the native video layer. On other
          // platforms it keeps the direct request-control activation.
          if (controlModesSupported && onOpenControlMenu) {
            onOpenControlMenu(event);
          } else {
            selectMode('control');
          }
        }}
        onmousedown={stopMouseDown}
        aria-label={remoteControlTitle}
        aria-pressed={activeMode === 'control'}
        aria-haspopup={controlModesSupported && onOpenControlMenu ? 'menu' : undefined}
        disabled={!remoteControlAvailable || remoteControlRequesting}
      >
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="m3 3 7.07 16.97 2.51-7.39 7.39-2.51L3 3Z"></path>
          <path d="m13 13 6 6"></path>
        </svg>
        <span>Control</span>
      </button>
      {/if}
      <button
        type="button"
        class="mode-segment draw"
        class:active={activeMode === 'draw'}
        onclick={() => selectMode('draw')}
        onmousedown={stopMouseDown}
        aria-label={drawActive ? 'Drawing on shared window' : 'Draw on shared window'}
        aria-pressed={activeMode === 'draw'}
        disabled={remoteControlRequesting}
      >
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M12 20h9"></path>
          <path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"></path>
        </svg>
        <span>Draw</span>
      </button>
    </div>

    <button
      type="button"
      class="header-btn overflow-btn"
      onclick={onOverflowClick}
      onmousedown={stopMouseDown}
      disabled={!onOpenModeMenu}
      aria-label="More remote window modes"
      aria-haspopup="menu"
    >
      <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
        <circle cx="5" cy="12" r="1.8"></circle>
        <circle cx="12" cy="12" r="1.8"></circle>
        <circle cx="19" cy="12" r="1.8"></circle>
      </svg>
    </button>
  </div>
</div>

<style>
  .header {
    container-type: inline-size;
    position: relative;
    /* container-type implies layout containment, which makes .header its own
       stacking context: the z-index:4 on .right-cluster / .traffic-lights
       below can no longer escape to beat the surface page's .resize-zones
       (z-index:3). The header as a whole stacks above them instead.
       Refs #674; caught by remoteWindowHeaderResizeGripStacking.test.ts
       after #918 introduced the container. */
    z-index: 4;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    height: 44px;
    min-height: 44px;
    max-height: 44px;
    padding: 0 14px 0 16px;
    box-sizing: border-box;
    overflow: hidden;
    color: var(--identity-header-ink);
    background: var(--identity-header-bg);
    border-bottom: 1px solid color-mix(in srgb, var(--identity-header-ink) 22%, transparent);
    box-shadow: inset 0 1px 0 color-mix(in srgb, var(--identity-header-ink) 18%, transparent);
    cursor: grab;
    user-select: none;
    transition-property: opacity;
    transition-duration: var(--motion-enter);
    transition-timing-function: var(--ease-standard);
  }

  .header.idle {
    height: 4px;
    min-height: 4px;
    max-height: 4px;
    padding: 0;
    border-bottom: 0;
  }

  .header.idle > * {
    opacity: 0;
    pointer-events: none;
    transform: translateY(-100%);
  }

  @media (prefers-reduced-motion: reduce) {
    .header {
      transition: none;
    }
  }

  .left-cluster,
  .right-cluster,
  .traffic-lights,
  .title-cluster {
    display: flex;
    align-items: center;
  }

  .left-cluster {
    flex: 1 1 auto;
    min-width: 0;
    gap: 14px;
  }

  .right-cluster {
    flex: 0 0 auto;
    gap: 8px;
    min-width: 0;
    /* .header creates no stacking context of its own (position:relative,
       z-index:auto), so this escapes it and stacks directly against the
       surface page's `.resize-zones` (z-index:3) -- without this the NE
       resize-grip hit zone wins hit-testing over .overflow-btn's top-right
       corner. Refs #674. */
    position: relative;
    z-index: 4;
  }

  .traffic-lights {
    flex: 0 0 auto;
    gap: 8px;
    /* Same stacking-context escape as .right-cluster above: without this the
       NW resize-grip hit zone (28x28, z-index:3) wins hit-testing over the
       traffic dots / Windows win-ctl buttons at every pixel. Refs #674. */
    position: relative;
    z-index: 4;
  }

  .traffic-dot {
    width: 12px;
    height: 12px;
    flex: 0 0 12px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-pill);
    /* Traffic-dot pressed inset — kept literal (uiConsistency allowlist). */
    box-shadow: inset 0 0 0 0.5px rgba(0, 0, 0, 0.25);
    cursor: pointer;
    transition-property: opacity, filter, scale;
    transition-duration: var(--motion-fast);
    transition-timing-function: var(--ease-standard);
  }

  /* Windows-style minimize/maximize buttons (Windows remote windows).
     Square, ink-on-transparent, matching the native Windows caption glyphs. */
  .win-ctl {
    width: 24px;
    height: 24px;
    flex: 0 0 24px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-check);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--identity-header-ink);
    background: transparent;
    cursor: pointer;
    transition-property: opacity, background-color, scale;
    transition-duration: var(--motion-fast);
    transition-timing-function: var(--ease-standard);
  }

  .win-ctl:hover {
    background: color-mix(in srgb, var(--identity-header-ink) 12%, transparent);
  }

  .win-ctl:active:not(:disabled) {
    scale: 0.96;
  }

  .win-ctl:focus-visible {
    outline: 1px solid var(--identity-header-ink);
    outline-offset: 2px;
  }

  .win-ctl svg {
    display: block;
    pointer-events: none;
  }

  .traffic-hide {
    background: #febc2e;
  }

  .traffic-fit {
    background: #28c840;
  }

  .traffic-dot:disabled {
    cursor: not-allowed;
    opacity: 0.56;
    filter: saturate(0.35);
  }

  .traffic-dot:active:not(:disabled) {
    scale: 0.96;
  }

  .traffic-dot:focus-visible {
    outline: 1px solid var(--text-strong);
    outline-offset: 2px;
  }

  .title-cluster {
    min-width: 0;
    gap: 9px;
  }

  .title {
    display: block;
    min-width: 0;
    font: 600 14px / 1.15 var(--font-ui);
    color: var(--identity-header-ink);
    /* Wrap rather than truncate when names are long: user-facing text must
       never be clipped or ellipsized (CLAUDE.md hard rule; guarded by
       uiConsistency.test.ts). #918 briefly made this nowrap+ellipsis. */
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .owner-label {
    color: color-mix(in srgb, var(--identity-header-ink) 62%, transparent);
    font-weight: 500;
  }

  .status-chip {
    flex: 0 1 auto;
    min-height: 24px;
    height: auto;
    display: inline-flex;
    align-items: flex-start;
    gap: 6px;
    min-width: 0;
    max-width: 220px;
    padding: 4px 9px;
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--identity-header-ink) 9%, transparent);
    border: 1px solid color-mix(in srgb, var(--identity-header-ink) 16%, transparent);
    color: color-mix(in srgb, var(--identity-header-ink) 78%, transparent);
    font-family: var(--font-ui);
    font-size: var(--text-micro);
    font-weight: 600;
    line-height: 1.2;
    letter-spacing: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .status-chip-dot {
    width: 6px;
    height: 6px;
    flex-shrink: 0;
    border-radius: 50%;
    background: currentColor;
  }

  .status-chip-text {
    min-width: 0;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
  }

  .status-chip.warning {
    color: var(--warning);
    background: color-mix(in srgb, var(--warning) 11%, transparent);
    border-color: color-mix(in srgb, var(--warning) 24%, transparent);
  }

  .status-chip.paused {
    color: color-mix(in srgb, var(--identity-header-ink) 72%, transparent);
  }

  /* #376 item 4: muted, compact -- only shown while remoteControlActive.
     Estimated values are always "~"-prefixed in the text itself (see
     formatGlassToGlassLatencyChip), so the chip never implies more
     precision than the underlying data actually has. */
  .latency-chip {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    height: 20px;
    padding: 0 7px;
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--identity-header-ink) 6%, transparent);
    border: 1px solid color-mix(in srgb, var(--identity-header-ink) 12%, transparent);
    color: color-mix(in srgb, var(--identity-header-ink) 56%, transparent);
    font-family: var(--font-mono, var(--font-ui));
    font-size: 10px;
    font-weight: 600;
    line-height: 1;
    letter-spacing: 0.2px;
    white-space: nowrap;
  }

  /* ---- AI chat (#657 receiver half) ----------------------------------------
     The badge is a DISCLOSURE, not decoration: no breakpoint below may hide
     it, it never shrinks (`flex: 0 0 auto`), and its text never truncates —
     the two labels are two complete sentences, and exactly one is rendered at
     any width, never a clipped version of the other. */
  .ai-chat-badge {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    height: 24px;
    padding: 0 10px;
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--live-bright) 16%, transparent);
    border: 1px solid color-mix(in srgb, var(--live-bright) 38%, transparent);
    color: var(--identity-header-ink);
    font-family: var(--font-ui);
    font-size: var(--text-micro);
    font-weight: 700;
    line-height: 1;
    letter-spacing: 0;
    white-space: nowrap;
  }

  /* The badge is a <button> only when a caller wires up the overlay toggle
     (onToggleAiChatOverlay); appearance reset so it still reads as the same
     chip the always-<span> form renders, not a native button. */
  button.ai-chat-badge {
    appearance: none;
    cursor: pointer;
  }

  button.ai-chat-badge:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  button.ai-chat-badge.open {
    background: color-mix(in srgb, var(--live-bright) 24%, transparent);
  }

  .ai-chat-badge-wrap {
    position: relative;
    flex: 0 0 auto;
    display: inline-flex;
  }

  /* Compact hold-to-talk in the header strip -- the transcript/typed-input
     overlay (#844) is a SEPARATE native window layered above the video; only
     this compact PTT button lives in the header webview itself. */
  .ai-chat-header-ptt {
    flex-shrink: 0;
    height: 28px;
    padding: 0 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-tile);
    background: rgba(255, 255, 255, 0.06);
    color: var(--text-primary);
    font: 700 12px var(--font-ui);
    cursor: pointer;
    white-space: nowrap;
    touch-action: none;
    user-select: none;
    transition:
      background var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard);
  }

  .ai-chat-header-ptt:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.10);
  }

  .ai-chat-header-ptt:focus-visible {
    outline: var(--focus-ring-width) solid var(--focus-ring);
    outline-offset: var(--focus-ring-offset);
  }

  .ai-chat-header-ptt:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .ai-chat-header-ptt.talking {
    background: var(--live-tint);
    border-color: var(--live-bright);
    color: var(--live-bright);
  }

  /* #847: was a plain pulsing dot; now a sparkle icon (same pulse), sized
     just under the badge's own 24px line-height so it doesn't grow the chip
     — see the 1252px breakpoint's measured margin note below. */
  .ai-chat-badge-dot {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 10px;
    height: 10px;
    flex: 0 0 10px;
    color: var(--live-bright);
    animation: ai-chat-live-pulse 1.8s ease-in-out infinite;
  }

  @keyframes ai-chat-live-pulse {
    0%,
    100% {
      opacity: 0.55;
    }
    50% {
      opacity: 1;
    }
  }

  .ai-chat-badge-short {
    display: none;
  }

  /* The badge has to come from somewhere, and it must not come from the window
     TITLE — measured at 620px, adding the badge with everything else still
     present left the title exactly 0px, so the user could no longer tell which
     window or whose they were looking at. Debug is a diagnostic affordance and
     Open URL a convenience; a live third-party disclosure outranks both, and
     they come back the moment the session ends. Do NOT solve this by letting
     the title absorb it instead. */
  .header.ai-chat-live .debug-btn,
  .header.ai-chat-live .open-url-btn {
    display: none;
  }

  /* MEASURED, not guessed (headless Chromium, real font/sizes, 2026-08-05;
     re-measured 2026-08-07 when #675 removed the Collapse button, same
     method -- a standalone render of the real markup/CSS at real font
     weight/size): the full sentence is a 408px chip, and with Debug/Open URL
     yielded above, the rest of a live-session right cluster (Stop AI chat +
     one 8px gap + the mode switcher) measures 394.9px, down from the old
     502px (collapse 99 + Stop AI chat 121 + switcher 266 + gaps) now that
     the Collapse button and its own 8px gap to the next item are gone
     entirely. Leaving a 120px floor for the window title, the full sentence
     now needs roughly a 1007px header (down ~107px from the old 1114px) --
     it appears at 1253px and up, preserving the original's ~246px margin,
     and the short label (a 100px chip that still fits at the 300px
     `MIN_RESIZE_CONTENT_WIDTH` floor) takes over below. Re-measure if any of
     this copy changes. (#847: the badge's dot grew 7px -> 10px for the
     sparkle icon, a +3px chip-width delta -- unmeasured but trivially inside
     the ~246px margin above; re-measure for real if that margin is ever
     trimmed close.) */
  @container (max-width: 1252px) {
    .ai-chat-badge-full {
      display: none;
    }

    .ai-chat-badge-short {
      display: inline;
    }
  }

  .ai-chat-btn.active {
    background: color-mix(in srgb, var(--identity-header-ink) 18%, transparent);
    border-color: color-mix(in srgb, var(--identity-header-ink) 26%, transparent);
    color: var(--identity-header-ink);
  }

  /* #669 bonus a11y fix: pressed-state styling to match .ai-chat-btn.active,
     now that the Debug button also tracks pressed state (aria-pressed +
     Show/Hide label) instead of being fire-and-forget. */
  .debug-btn.active {
    background: color-mix(in srgb, var(--identity-header-ink) 18%, transparent);
    border-color: color-mix(in srgb, var(--identity-header-ink) 26%, transparent);
    color: var(--identity-header-ink);
  }

  .ai-chat-btn.warning {
    color: var(--warning);
    border-color: color-mix(in srgb, var(--warning) 26%, transparent);
  }

  /* Compact refusal-reason chip on the header: the SHORT label stays narrow;
     the full reason is the native tooltip (an OS popup, neither clipped by
     the window nor covered by the video layer). */
  .ai-chat-error-note {
    flex-shrink: 0;
    max-width: 220px;
    overflow: visible;
    overflow-wrap: anywhere;
    text-wrap: pretty;
    white-space: normal;
    color: var(--warning);
    font: 600 11px var(--font-ui);
    border: 1px solid color-mix(in srgb, var(--warning) 26%, transparent);
    border-radius: var(--radius-tile);
    padding: 3px 8px;
    background: color-mix(in srgb, var(--warning) 8%, transparent);
  }

  .header-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
    height: 30px;
    padding: 0 12px;
    border-radius: var(--radius-chip);
    border: 1px solid transparent;
    background: transparent;
    color: color-mix(in srgb, var(--identity-header-ink) 82%, transparent);
    cursor: pointer;
    flex-shrink: 0;
    user-select: none;
    font-family: var(--font-ui);
    font-size: var(--text-caption);
    font-weight: 600;
    line-height: 1;
    transition-property: background-color, border-color, box-shadow, color, transform, scale;
    transition-duration: var(--motion-fast);
    transition-timing-function: var(--ease-standard);
  }

  .header-btn:hover {
    background: color-mix(in srgb, var(--identity-header-ink) 12%, transparent);
    border-color: color-mix(in srgb, var(--identity-header-ink) 18%, transparent);
    color: var(--identity-header-ink);
  }

  .header-btn:active {
    background: color-mix(in srgb, var(--identity-header-ink) 18%, transparent);
    /* Pressed inset — kept literal (uiConsistency allowlist). */
    box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.35);
    transform: translateY(0.5px);
  }

  .mode-switcher {
    /* Full labels stay visible until #497's <=470px native popup menu
       replaces the entire segmented control. */
    --segment-width: 86px;
    position: relative;
    display: flex;
    align-items: stretch;
    height: 30px;
    padding: 0;
    border-radius: var(--radius-chip);
    background: color-mix(in srgb, var(--identity-header-ink) 8%, transparent);
    border: 1px solid color-mix(in srgb, var(--identity-header-ink) 14%, transparent);
    margin-left: 2px;
    flex-shrink: 0;
    box-sizing: border-box;
    overflow: hidden;
  }

  .overflow-btn {
    display: none;
    position: relative;
    width: 40px;
    padding: 0;
  }

  .overflow-btn::after {
    content: '';
    position: absolute;
    top: 50%;
    left: 50%;
    width: 40px;
    height: 40px;
    transform: translate(-50%, -50%);
  }

  .overflow-btn:active:not(:disabled) {
    scale: 0.96;
  }


  .mode-segment {
    position: relative;
    z-index: 1;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    width: var(--segment-width);
    align-self: stretch;
    padding: 0;
    border: none;
    border-radius: 0;
    background: transparent;
    color: color-mix(in srgb, var(--identity-header-ink) 58%, transparent);
    font-family: var(--font-ui);
    font-size: var(--text-micro);
    font-weight: 600;
    line-height: 1;
    cursor: pointer;
    user-select: none;
    transition-property: color, background-color, box-shadow, scale;
    transition-duration: var(--motion-feedback);
    transition-timing-function: var(--ease-standard);
  }

  .mode-segment:hover:not(:disabled) {
    color: color-mix(in srgb, var(--identity-header-ink) 82%, transparent);
  }

  .active-indicator {
    position: absolute;
    top: 0;
    left: 0;
    width: var(--segment-width);
    height: 100%;
    background: color-mix(in srgb, var(--identity-header-ink) 14%, transparent);
    box-shadow:
      0 1px 2px color-mix(in srgb, var(--identity-header-ink) 12%, transparent),
      inset 0 1px 0 color-mix(in srgb, var(--identity-header-ink) 10%, transparent);
    transform: translateX(calc(var(--active-mode-index, 0) * var(--segment-width)));
    transition: transform var(--motion-feedback) var(--ease-standard);
    pointer-events: none;
  }

  .mode-segment.active {
    color: var(--identity-header-ink);
  }

  .mode-segment.requesting {
    color: var(--identity-header-ink);
    cursor: wait;
  }

  .mode-segment:disabled {
    color: color-mix(in srgb, var(--identity-header-ink) 38%, transparent);
    cursor: not-allowed;
  }

  /* #376 item 2: "preparing" reads as transient/in-progress (a soft pulse),
     distinct from the flatter, static dimming a plain :disabled state gets
     elsewhere in this control -- so it doesn't look like a dead end. */
  .mode-segment.preparing {
    animation: preparing-pulse 1.6s ease-in-out infinite;
  }

  @keyframes preparing-pulse {
    0%,
    100% {
      opacity: 0.62;
    }
    50% {
      opacity: 1;
    }
  }

  .mode-segment.active:disabled {
    color: color-mix(in srgb, var(--identity-header-ink) 92%, transparent);
  }

  .header-btn:focus-visible,
  .mode-segment:focus-visible {
    outline: 1px solid color-mix(in srgb, var(--identity-header-ink) 86%, transparent);
    outline-offset: 1px;
  }

  .mode-segment:active {
    scale: var(--press-scale, 0.96);
  }

  .header-btn svg {
    display: block;
    transform: translateY(0.25px);
    flex-shrink: 0;
  }

  .header-btn span {
    white-space: nowrap;
  }

  .mode-segment span {
    white-space: nowrap;
  }

  @container (max-width: 720px) {
    /* Never vanish: the chip carries transient state ("Requesting control",
       "Video paused"). Under 720px it collapses to an icon-only dot with
       the full text kept for screen readers and available on hover (title). */
    .status-chip {
      padding: 0 7px;
    }

    .status-chip-text {
      position: absolute;
      width: 1px;
      height: 1px;
      padding: 0;
      margin: -1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
      border: 0;
    }
  }

  @container (max-width: 640px) {
    .debug-btn {
      display: none;
    }

    /* Icon-only, not a shortened word: the full action stays in `aria-label`
       and `title`, so nothing is truncated. */
    .ai-chat-btn span {
      display: none;
    }

    .ai-chat-btn {
      width: 40px;
      padding: 0;
    }

    /* Decorative once the window is this tight; the essential affordance is
       the mode switcher itself (icons-only from 600px, replaced by the overflow
       menu below 470px -- see below), not the latency readout. */
    .latency-chip {
      display: none;
    }
  }

  /* Step 1: mode segment text hidden — icons only. */
  @container (max-width: 600px) {
    .mode-switcher {
      --segment-width: 36px;
    }

    .mode-segment span {
      display: none;
    }
  }

  @container (max-width: 560px) {
    .open-url-btn {
      display: none;
    }
  }

  /* Step 2: "by user" hidden. Title still visible and truncating. */
  @container (max-width: 480px) {
    .owner-label {
      display: none;
    }
  }

  /* #497: below 470px the segmented switcher is REPLACED by the overflow
     button, which opens the same labelled native popup (View / Control /
     Draw / AI chat) so every action keeps its full label. #918's ladder
     shrinks the segments to icons first (<=600px) but must still hand off
     here; without this step the overflow button never appears and the
     native menu path is dormant. Guarded by remoteWindowHeader.test.ts and
     remoteWindowHeaderResizeGripStacking.test.ts (420px render). */
  @container (max-width: 470px) {
    .mode-switcher {
      display: none;
    }

    .ai-chat-btn {
      display: none;
    }

    .overflow-btn {
      display: inline-flex;
    }
  }

  /* Step 3: title hidden entirely. */
  @container (max-width: 360px) {
    .title-cluster {
      display: none;
    }
  }

  @container (max-width: 300px) {
    .header {
      gap: 10px;
      padding-right: 10px;
      padding-left: 12px;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .header-btn,
    .mode-segment,
    .active-indicator,
    .traffic-dot {
      transition: none;
    }

    .header-btn:active,
    .mode-segment:active,
    .traffic-dot:active:not(:disabled) {
      transform: none;
      scale: 1;
    }

    .mode-segment.preparing {
      animation: none;
    }

    /* The dot stops pulsing but never stops being visible — the badge itself
       is what discloses the session, not its motion. */
    .ai-chat-badge-dot {
      animation: none;
      opacity: 1;
    }
  }
</style>
