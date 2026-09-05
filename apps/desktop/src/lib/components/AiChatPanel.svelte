<!--
  AiChatPanel — the in-meeting surface for one AI chat session (#656 phase 1).

  This is deliberately an in-webview panel inside the EXISTING main/meeting
  window, not a new native NSPanel. Petal's dynamic panels carry a documented
  crash class (CLAUDE.md "Crash classes" 1/2: AppKit off the main thread, and
  `window.close()` on a `tauri_nspanel` aborting in deferred dealloc), which is
  why every dynamic panel in this app needs a hide-and-retire lifecycle. A
  transcript + a push-to-talk button do not justify buying into that for
  phase 1.

  Everything the panel shows arrives on two events (`ai_chat/session.rs`):
  - `ai-chat-state`  — either a phase change (`state`) or a countdown tick
                       (`secondsLeft`); exactly one per emission.
  - `ai-chat-transcript` — role-tagged deltas, coalesced into bubbles by
                       `$lib/data/aiChat`'s `appendTranscriptDelta`.

  PUSH-TO-TALK IS THE RISK SURFACE HERE. A mic that stays open because the
  pointer was released outside the button, or because the window lost focus
  mid-press, is the worst failure this panel can produce — it keeps streaming
  audio to Google with no visible indication. So the end path is redundant on
  purpose: the button's own pointerup/pointerleave/pointercancel/lostpointercapture/blur, plus
  window-level pointerup/pointercancel/blur and a document visibilitychange,
  plus onDestroy. `endPtt` is idempotent (guarded by `pttActive`), so firing it
  repeatedly costs one command call.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onDestroy } from 'svelte';
  import { COMMANDS, EVENTS, listenUntilDestroy } from '$lib/ipc';
  import type {
    AiChatControlRequestEvent,
    AiChatControlResolvedEvent,
    AiChatControlStatus,
    AiChatEndReason,
    AiChatPanelInfo,
    AiChatStateEvent,
    AiChatTranscriptEvent
  } from '$lib/ipc';
  import {
    aiChatControlDetailRows,
    aiChatStatusLabel,
    appendTranscriptDelta,
    closeOpenTurns,
    formatAiChatCountdown,
    AI_CHAT_CONTROL_ALLOW_ONCE_LABEL,
    AI_CHAT_CONTROL_ALLOW_SESSION_LABEL,
    AI_CHAT_CONTROL_GRANTED_NOTE,
    AI_CHAT_CONTROL_HEADING,
    AI_CHAT_CONTROL_REJECT_LABEL,
    AI_CHAT_CONTROL_REJECTED_NOTE,
    AI_CHAT_CONTROL_REVOKE_LABEL,
    AI_CHAT_CONTROL_RESUME_LABEL,
    AI_CHAT_CONTROL_STALE_NOTE,
    AI_CHAT_ACTIVE_DISCLOSURE,
    AI_CHAT_PTT_REFUSED_NOTE,
    AI_CHAT_TEXT_MAX_CHARS,
    AI_CHAT_TEXT_SEND_FAILED_NOTE,
    type AiChatTranscriptTurn
  } from '$lib/data/aiChat';
  import type { UnlistenFn } from '@tauri-apps/api/event';

  interface Props {
    /** Called when a session reaches its terminal phase, with the reason token. */
    onEnded?: (reason: AiChatEndReason) => void;
    /** Host-rendered copy for the terminal reason. */
    endMessage?: string | null;
    /**
     * Suppress the visual without unmounting.
     */
    suppressed?: boolean;
  }

  let { onEnded, endMessage = null, suppressed = false }: Props = $props();

  let phase = $state<'connecting' | 'live' | 'ended' | null>(null);
  let windowId = $state<number | null>(null);
  let ownerAppName = $state<string | null>(null);
  let secondsLeft = $state<number | null>(null);
  let turns = $state<AiChatTranscriptTurn[]>([]);
  let pttActive = $state(false);
  let activeSpeaker = $state<string | null>(null);
  let actionNotice = $state<string | null>(null);
  let stopping = $state(false);
  let transcriptEl = $state<HTMLDivElement | null>(null);
  let textDraft = $state('');
  let sendingText = $state(false);

  const sessionActive = $derived(phase === 'connecting' || phase === 'live' || phase === 'ended');
  const visible = $derived(sessionActive && !suppressed);
  const countdown = $derived(secondsLeft === null ? null : formatAiChatCountdown(secondsLeft));
  const statusText = $derived.by(() => {
    const target = ownerAppName ? `${ownerAppName} window` : 'window';
    return phase ? `${aiChatStatusLabel(phase)} · AI chat about ${target}` : `AI chat about ${target}`;
  });

  // #658: the pending window-control request, if any. Exactly one card at a
  // time — the engine only ever holds one pending request, and a newer tool
  // call replaces it rather than stacking.
  let controlRequest = $state<AiChatControlRequestEvent | null>(null);
  let controlAnswering = $state(false);
  // Refusal is sticky for the rest of the session, so the panel keeps saying so
  // and offers the deliberate way back rather than silently going quiet.
  let controlRefusedSessionId = $state<number | null>(null);
  let controlStandingSessionId = $state<number | null>(null);
  const controlDetailRows = $derived(
    controlRequest ? aiChatControlDetailRows(controlRequest.detail) : []
  );

  let destroyed = false;
  let unlistenState: UnlistenFn | undefined;
  let unlistenTranscript: UnlistenFn | undefined;
  let unlistenControlRequest: UnlistenFn | undefined;
  let unlistenControlResolved: UnlistenFn | undefined;
  let sessionGeneration = $state(0);

  listenUntilDestroy<AiChatStateEvent>(
    EVENTS.aiChatState,
    (event) => handleState(event.payload),
    (fn) => (unlistenState = fn),
    () => destroyed
  );

  listenUntilDestroy<AiChatTranscriptEvent>(
    EVENTS.aiChatTranscript,
    (event) => handleTranscript(event.payload),
    (fn) => (unlistenTranscript = fn),
    () => destroyed
  );

  listenUntilDestroy<AiChatControlRequestEvent>(
    EVENTS.aiChatControlRequest,
    (event) => {
      controlRequest = event.payload;
      controlAnswering = false;
      controlStandingSessionId = null;
    },
    (fn) => (unlistenControlRequest = fn),
    () => destroyed
  );

  listenUntilDestroy<AiChatControlResolvedEvent>(
    EVENTS.aiChatControlResolved,
    (event) => {
      // Only the card this resolution names goes away. A resolution for an
      // older request must not dismiss a newer prompt the human has not seen.
      if (controlRequest && controlRequest.requestId === event.payload.requestId) {
        controlRequest = null;
        controlAnswering = false;
      }
      void refreshControlStatus();
    },
    (fn) => (unlistenControlResolved = fn),
    () => destroyed
  );

  function handleState(payload: AiChatStateEvent) {
    if (payload.state) {
      switch (payload.state.phase) {
        case 'connecting':
          // A fresh session — never inherit the previous one's transcript.
          const connectingGeneration = ++sessionGeneration;
          const connectingWindowId = payload.windowId;
          windowId = payload.windowId;
          phase = 'connecting';
          ownerAppName = null;
          secondsLeft = null;
          turns = [];
          stopping = false;
          activeSpeaker = null;
          actionNotice = null;
          controlStandingSessionId = null;
          controlRefusedSessionId = null;
          invoke<AiChatPanelInfo>(COMMANDS.aiChatPanelPresent, { windowId: payload.windowId })
            .then((info) => {
              if (
                sessionGeneration !== connectingGeneration ||
                windowId !== connectingWindowId
              ) return;
              ownerAppName = info?.ownerAppName ?? null;
            })
            .catch(() => {});
          break;
        case 'live':
          if (windowId !== payload.windowId || phase === null || phase === 'ended') {
            sessionGeneration += 1;
          }
          const liveGeneration = sessionGeneration;
          const liveWindowId = payload.windowId;
          windowId = payload.windowId;
          phase = 'live';
          invoke<AiChatPanelInfo>(COMMANDS.aiChatPanelPresent, { windowId: payload.windowId })
            .then((info) => {
              if (sessionGeneration !== liveGeneration || windowId !== liveWindowId) return;
              if (info?.ownerAppName) ownerAppName = info.ownerAppName;
            })
            .catch(() => {});
          void refreshControlStatus();
          break;
        case 'ended':
          endPtt();
          sessionGeneration += 1;
          phase = 'ended';
          secondsLeft = null;
          stopping = false;
          // A control prompt cannot outlive its session: answering it later
          // could only authorize something that no longer exists.
          controlRequest = null;
          controlAnswering = false;
          controlRefusedSessionId = null;
          controlStandingSessionId = null;
          activeSpeaker = null;
          actionNotice = null;
          onEnded?.(payload.state.reason);
          break;
      }
      return;
    }
    if ('activeSpeaker' in payload) {
      if (windowId === null) windowId = payload.windowId;
      if (payload.windowId === windowId) activeSpeaker = payload.activeSpeaker ?? null;
      return;
    }
    if (typeof payload.secondsLeft === 'number') {
      // Ticks can arrive fractionally before the phase event on a fast connect.
      if (windowId === null) windowId = payload.windowId;
      if (payload.windowId === windowId) secondsLeft = payload.secondsLeft;
    }
  }

  function handleTranscript(payload: AiChatTranscriptEvent) {
    if (windowId !== null && payload.windowId !== windowId) return;
    turns = appendTranscriptDelta(turns, {
      role: payload.role,
      text: payload.text,
      final: payload.final
    });
  }

  // Keep the newest turn in view as deltas stream in — but only while the
  // user is ALREADY reading the newest content (within ~40px of the bottom).
  // Scrolling up to re-read history mid-reply must not be yanked back down
  // on every non-final delta (the transcript can get hundreds of deltas per
  // reply; unconditional scrollTop made history unreadable while streaming).
  $effect(() => {
    // Touch `turns` so this re-runs on every delta.
    void turns.length;
    const el = transcriptEl;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    if (nearBottom) el.scrollTop = el.scrollHeight;
  });

  async function startPtt() {
    if (pttActive || phase !== 'live' || activeSpeaker !== null) return;
    pttActive = true;
    actionNotice = null;
    // A new spoken turn: close whatever bubble was still open so two presses
    // don't merge into one.
    turns = closeOpenTurns(turns);
    addGlobalPttGuards();
    try {
      const accepted = await invoke<boolean>(COMMANDS.aiChatPttStart);
      if (accepted) return;
      actionNotice = AI_CHAT_PTT_REFUSED_NOTE;
      endPtt();
    } catch {
      actionNotice = AI_CHAT_PTT_REFUSED_NOTE;
      endPtt();
    }
  }

  function endPtt() {
    if (!pttActive) return;
    pttActive = false;
    removeGlobalPttGuards();
    invoke(COMMANDS.aiChatPttEnd).catch(() => {});
  }

  // Belt-and-braces: a pointerup that lands outside the button, or the window
  // losing focus mid-press, still has to end the turn.
  function addGlobalPttGuards() {
    if (typeof window === 'undefined') return;
    window.addEventListener('pointerup', endPtt);
    window.addEventListener('pointercancel', endPtt);
    window.addEventListener('blur', endPtt);
    document.addEventListener('visibilitychange', endPttIfHidden);
  }

  function removeGlobalPttGuards() {
    if (typeof window === 'undefined') return;
    window.removeEventListener('pointerup', endPtt);
    window.removeEventListener('pointercancel', endPtt);
    window.removeEventListener('blur', endPtt);
    document.removeEventListener('visibilitychange', endPttIfHidden);
  }

  function endPttIfHidden() {
    if (document.visibilityState === 'hidden') endPtt();
  }

  function handlePttKeydown(event: KeyboardEvent) {
    if (event.repeat) return;
    if (event.key !== ' ' && event.key !== 'Enter') return;
    event.preventDefault();
    startPtt();
  }

  function handlePttKeyup(event: KeyboardEvent) {
    if (event.key !== ' ' && event.key !== 'Enter') return;
    event.preventDefault();
    endPtt();
  }

  const trimmedDraft = $derived(textDraft.trim());
  // Unlike PTT, text is refused while a spoken turn is open (a clientContent
  // turn and an open realtimeInput activity window are undefined together —
  // see session::send_text) — so disable Send rather than let it silently
  // fail while someone is talking.
  const canSendText = $derived(
    phase === 'live' && !pttActive && activeSpeaker === null && !sendingText && trimmedDraft.length > 0 && trimmedDraft.length <= AI_CHAT_TEXT_MAX_CHARS
  );

  async function refreshControlStatus() {
    const requestedWindowId = windowId;
    const requestedGeneration = sessionGeneration;
    if (requestedWindowId === null || phase !== 'live') return;
    try {
      const status = await invoke<AiChatControlStatus | null>(COMMANDS.aiChatControlStatus);
      if (
        phase !== 'live' ||
        windowId !== requestedWindowId ||
        sessionGeneration !== requestedGeneration
      ) return;
      controlStandingSessionId = status?.standing === 'session' ? status.sessionId : null;
      controlRefusedSessionId = status?.standing === 'refused' ? status.sessionId : null;
    } catch {
      // Keep the last Rust-confirmed state. A failed query is not evidence that
      // a standing grant or refusal disappeared.
    }
  }

  async function sendTypedText() {
    if (!canSendText) return;
    const text = trimmedDraft;
    sendingText = true;
    // A typed turn: close whatever bubble was still open, same as starting a
    // new spoken turn, so this message's reply doesn't merge into an older one.
    turns = closeOpenTurns(turns);
    try {
      const accepted = await invoke<boolean>(COMMANDS.aiChatSendText, { text });
      if (accepted) textDraft = '';
    } catch {
      // Leave the draft in place so the user can retry rather than losing
      // it — but never silently: a Send that appears to do nothing is the
      // exact failure mode this notice exists to prevent.
      actionNotice = AI_CHAT_TEXT_SEND_FAILED_NOTE;
    } finally {
      sendingText = false;
    }
  }

  function handleTextKeydown(event: KeyboardEvent) {
    if (event.key !== 'Enter' || event.shiftKey) return;
    event.preventDefault();
    void sendTypedText();
  }

  async function handleStop() {
    if (stopping) return;
    stopping = true;
    endPtt();
    try {
      if (phase === 'live' || phase === 'connecting') {
        await invoke(COMMANDS.aiChatStop);
      }
    } catch {
    } finally {
      try {
        await invoke(COMMANDS.aiChatPanelDismiss);
      } catch {}
      phase = null;
      windowId = null;
      turns = [];
      ownerAppName = null;
      stopping = false;
    }
  }

  // #658 approval answers. Each one names BOTH the session epoch and the
  // request id, so a click on a card the model had already replaced cannot
  // authorize whatever replaced it — the Rust side rejects the mismatch.
  async function answerControl(sessionScope: boolean) {
    const request = controlRequest;
    if (!request || controlAnswering) return;
    controlAnswering = true;
    try {
      const applied = await invoke<boolean>(COMMANDS.aiChatControlApprove, {
        sessionId: request.sessionId,
        requestId: request.requestId,
        sessionScope
      });
      if (!applied) {
        actionNotice = AI_CHAT_CONTROL_STALE_NOTE;
        controlRequest = null;
      }
      await refreshControlStatus();
    } catch {
      // The engine emits `ai-chat-control-resolved` on every real outcome; if
      // the command itself failed there will be none, so clear the card here
      // rather than leaving a dead prompt on screen.
      controlRequest = null;
    } finally {
      controlAnswering = false;
    }
  }

  async function rejectControl() {
    const request = controlRequest;
    if (!request || controlAnswering) return;
    controlAnswering = true;
    try {
      // Only claim the session is refused once Rust says the answer landed —
      // a stale answer changes nothing, and showing the sticky note for it
      // would tell the user control is off when it is not.
      const applied = await invoke<boolean>(COMMANDS.aiChatControlReject, {
        sessionId: request.sessionId
      });
      if (applied) {
        controlRefusedSessionId = request.sessionId;
        controlStandingSessionId = null;
      } else {
        actionNotice = AI_CHAT_CONTROL_STALE_NOTE;
      }
      await refreshControlStatus();
    } catch {
      controlRequest = null;
    } finally {
      controlAnswering = false;
    }
  }

  async function resumeControl() {
    const sessionId = controlRefusedSessionId;
    if (sessionId === null) return;
    try {
      const applied = await invoke<boolean>(COMMANDS.aiChatControlResume, { sessionId });
      if (applied) {
        controlRefusedSessionId = null;
        actionNotice = null;
      } else {
        actionNotice = AI_CHAT_CONTROL_STALE_NOTE;
      }
      await refreshControlStatus();
    } catch {
      // Leave the note up: the refusal is still in force.
    }
  }

  async function revokeStandingControl() {
    const sessionId = controlStandingSessionId;
    if (sessionId === null || controlAnswering) return;
    controlAnswering = true;
    try {
      const applied = await invoke<boolean>(COMMANDS.aiChatControlReject, { sessionId });
      if (applied) {
        controlStandingSessionId = null;
        controlRefusedSessionId = sessionId;
      } else {
        actionNotice = AI_CHAT_CONTROL_STALE_NOTE;
      }
      await refreshControlStatus();
    } finally {
      controlAnswering = false;
    }
  }

  onDestroy(() => {
    destroyed = true;
    endPtt();
    unlistenState?.();
    unlistenTranscript?.();
    unlistenControlRequest?.();
    unlistenControlResolved?.();
  });
</script>

{#if visible}
  <section class="ai-chat" aria-label="AI chat session">
    <header class="ai-chat-header">
      <span class="status" class:live={phase === 'live'}>
        <span class="dot" aria-hidden="true"></span>
        {statusText}
      </span>
      {#if countdown}
        <span class="countdown" aria-label="Time left">{countdown}</span>
      {/if}
    </header>

    {#if phase === 'live'}
      <p class="disclosure">{AI_CHAT_ACTIVE_DISCLOSURE}</p>
    {:else if phase === 'ended' && endMessage}
      <p class="end-status" role="status">{endMessage}</p>
    {/if}

    {#if actionNotice}
      <p class="action-notice" role="status">{actionNotice}</p>
    {/if}

    <div class="transcript" bind:this={transcriptEl} aria-live="polite">
      {#if turns.length === 0}
        <p class="empty">
          {phase === 'connecting'
            ? 'Connecting to the AI…'
            : phase === 'ended'
              ? 'This AI chat session has ended.'
              : 'Hold Talk and ask about this window.'}
        </p>
      {:else}
        {#each turns as turn (turn.id)}
          <p class="turn" class:assistant={turn.role === 'assistant'}>
            <span class="who">{turn.role === 'assistant' ? 'AI' : 'You'}</span>
            <span class="said">{turn.text}</span>
          </p>
        {/each}
      {/if}
    </div>

    {#if controlRequest}
      <section class="control-card" aria-label="Window control request">
        <p class="control-heading">{AI_CHAT_CONTROL_HEADING}</p>
        <p class="control-summary">{controlRequest.detail.summary}</p>
        {#each controlDetailRows as row (row.label)}
          <p class="control-row">
            <span class="control-label">{row.label}</span>
            <span class="control-value">{row.value}</span>
          </p>
        {/each}
        <div class="control-actions">
          <button
            type="button"
            class="control-allow"
            disabled={controlAnswering}
            onclick={() => void answerControl(false)}
          >
            {AI_CHAT_CONTROL_ALLOW_ONCE_LABEL}
          </button>
          <button
            type="button"
            class="control-reject"
            disabled={controlAnswering}
            onclick={() => void rejectControl()}
          >
            {AI_CHAT_CONTROL_REJECT_LABEL}
          </button>
        </div>
        <!-- The escalation sits on its own row, below the per-action answer,
             so it can never be mistaken for the default. -->
        <button
          type="button"
          class="control-session"
          disabled={controlAnswering}
          onclick={() => void answerControl(true)}
        >
          {AI_CHAT_CONTROL_ALLOW_SESSION_LABEL}
        </button>
      </section>
    {:else if controlRefusedSessionId !== null}
      <section class="control-card refused" aria-label="Window control refused">
        <p class="control-summary">{AI_CHAT_CONTROL_REJECTED_NOTE}</p>
        <button type="button" class="control-session" disabled={controlAnswering} onclick={() => void resumeControl()}>
          {AI_CHAT_CONTROL_RESUME_LABEL}
        </button>
      </section>
    {:else if controlStandingSessionId !== null}
      <section class="control-card granted" aria-label="AI standing window access">
        <p class="control-summary">{AI_CHAT_CONTROL_GRANTED_NOTE}</p>
        <button
          type="button"
          class="control-reject"
          disabled={controlAnswering}
          onclick={() => void revokeStandingControl()}
        >
          {AI_CHAT_CONTROL_REVOKE_LABEL}
        </button>
      </section>
    {/if}

    <button
      type="button"
      class="ptt"
      class:talking={pttActive}
      disabled={phase !== 'live' || (activeSpeaker !== null && !pttActive)}
      aria-pressed={pttActive}
      onpointerdown={startPtt}
      onpointerup={endPtt}
      onpointerleave={endPtt}
      onpointercancel={endPtt}
      onlostpointercapture={endPtt}
      onblur={endPtt}
      onkeydown={handlePttKeydown}
      onkeyup={handlePttKeyup}
      oncontextmenu={(event) => event.preventDefault()}
    >
      {pttActive
        ? 'Listening — release to send'
        : activeSpeaker
          ? `${activeSpeaker} is talking`
          : 'Hold to talk'}
    </button>

    <form
      class="text-row"
      onsubmit={(event) => {
        event.preventDefault();
        void sendTypedText();
      }}
    >
      <input
        class="text-input"
        type="text"
        placeholder="Type a message…"
        bind:value={textDraft}
        disabled={phase !== 'live' || activeSpeaker !== null || sendingText}
        maxlength={AI_CHAT_TEXT_MAX_CHARS}
        onkeydown={handleTextKeydown}
        aria-label="Type a message to the AI"
      />
      <button type="submit" class="text-send" disabled={!canSendText}>Send</button>
      <button type="button" class="end" onclick={() => void handleStop()} disabled={stopping}>
        End
      </button>
    </form>
  </section>
{/if}

<style>
  .ai-chat {
    display: flex;
    flex-direction: column;
    gap: 8px;
    box-sizing: border-box;
    width: 100%;
    max-height: 260px;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding: 10px;
    border-radius: var(--radius-tile);
    background: var(--surface);
    box-shadow:
      var(--shadow-inset-hairline),
      var(--shadow-float);
    font-family: var(--font-ui);
    pointer-events: auto;
  }

  .ai-chat-header {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .disclosure,
  .end-status,
  .action-notice {
    flex-shrink: 0;
    margin: 0;
    font: 500 10px/1.35 var(--font-ui);
    color: var(--text-muted);
    text-wrap: pretty;
    overflow-wrap: anywhere;
  }

  .action-notice {
    color: var(--warning);
  }

  .end-status {
    color: var(--text-primary);
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    flex: 1 1 auto;
    min-width: 0;
    font: 600 var(--text-caption) var(--font-ui);
    color: var(--text-muted);
    /* Never clip the status word — wrap instead (CLAUDE.md: UI text must
       never truncate). */
    overflow-wrap: anywhere;
  }

  .status.live {
    color: var(--text-primary);
  }

  .dot {
    width: 7px;
    height: 7px;
    flex-shrink: 0;
    border-radius: var(--radius-pill);
    background: var(--text-faint);
  }

  .status.live .dot {
    background: var(--live-bright);
  }

  .countdown {
    flex-shrink: 0;
    font: 500 var(--text-micro) var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--text-faint);
  }

  .end {
    flex-shrink: 0;
    height: 24px;
    padding: 0 10px;
    border: 1px solid var(--danger-tint-25);
    border-radius: var(--radius-chip);
    background: var(--danger-tint-12);
    color: var(--danger);
    font: 700 var(--text-caption) var(--font-ui);
    cursor: pointer;
    transition: background var(--motion-fast) var(--ease-standard);
  }

  .end:hover:not(:disabled) {
    background: var(--danger-tint-16);
  }

  .end:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .end:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .transcript {
    display: flex;
    flex-direction: column;
    gap: 6px;
    flex: 1 1 auto;
    min-height: 48px;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding-right: 2px;
  }

  .empty {
    margin: 0;
    font: 500 var(--text-caption)/1.4 var(--font-ui);
    color: var(--text-faint);
    text-wrap: pretty;
  }

  .turn {
    margin: 0;
    padding: 6px 8px;
    border-radius: var(--radius-chip);
    background: var(--fill-weak);
    font: 500 var(--text-caption)/1.4 var(--font-ui);
    color: var(--text-primary);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    /* Transcript text is arbitrary and unbounded — it must wrap, including
       inside a long unbroken token, and must never be clipped. */
  }

  .turn.assistant {
    background: rgba(110, 139, 255, 0.1);
  }

  .who {
    display: block;
    margin-bottom: 2px;
    font: 600 var(--text-micro) var(--font-mono);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-faint);
  }

  .said {
    display: block;
  }

  /* #658 approval card. Every string here is user-facing consent copy, so
     nothing in this block may clip: labels wrap, and the literal text the model
     wants typed scrolls inside its own box rather than being cut off. */
  .control-card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    flex-shrink: 0;
    padding: 8px;
    border: 1px solid var(--danger-tint-25);
    border-radius: var(--radius-tile);
    background: var(--danger-tint-12);
  }

  .control-heading {
    margin: 0;
    font: 700 var(--text-caption)/1.3 var(--font-ui);
    color: var(--text-primary);
    text-wrap: pretty;
    overflow-wrap: anywhere;
  }

  .control-summary {
    margin: 0;
    font: 500 var(--text-caption)/1.4 var(--font-ui);
    color: var(--text-muted);
    text-wrap: pretty;
    overflow-wrap: anywhere;
  }

  .control-row {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin: 0;
  }

  .control-label {
    font: 600 var(--text-micro) var(--font-mono);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-faint);
  }

  .control-value {
    /* The exact string the AI would type can be up to 2000 characters. It is
       never abbreviated — the box scrolls so all of it stays reachable. */
    max-height: 84px;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding: 4px 6px;
    border-radius: var(--radius-chip);
    background: rgba(0, 0, 0, 0.25);
    font: 500 var(--text-micro)/1.4 var(--font-mono);
    color: var(--text-primary);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .control-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .control-allow,
  .control-reject,
  .control-session {
    /* min-height, never height: these labels wrap on a narrow panel and a
       fixed height would clip them. */
    min-height: 26px;
    padding: 4px 10px;
    border-radius: var(--radius-chip);
    font: 700 var(--text-caption)/1.3 var(--font-ui);
    cursor: pointer;
    white-space: normal;
    text-wrap: balance;
  }

  .control-allow {
    flex: 1 1 auto;
    border: 1px solid var(--hairline-strong);
    background: var(--fill-bright);
    color: var(--text-primary);
  }

  .control-reject {
    flex: 0 1 auto;
    border: 1px solid var(--danger-tint-25);
    background: var(--danger-tint-16);
    color: var(--danger);
  }

  .control-session {
    /* Secondary by weight as well as by position: a session-wide grant is a
       much larger thing to hand out than one action. */
    width: 100%;
    border: 1px dashed var(--hairline-strong);
    background: transparent;
    color: var(--text-muted);
    font-weight: 500;
  }

  .control-allow:hover:not(:disabled) {
    background: var(--surface-raised);
  }

  .control-reject:hover:not(:disabled) {
    background: var(--danger-tint-25);
  }

  .control-session:hover:not(:disabled) {
    color: var(--text-primary);
  }

  .control-allow:focus-visible,
  .control-reject:focus-visible,
  .control-session:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .control-allow:disabled,
  .control-reject:disabled,
  .control-session:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .control-card.refused {
    border-color: var(--hairline-strong);
    background: var(--fill-weak);
  }

  .control-card.granted {
    border-color: color-mix(in srgb, var(--warning) 25%, transparent);
    background: color-mix(in srgb, var(--warning) 10%, transparent);
  }

  .control-card.granted .control-reject {
    width: 100%;
  }

  .ptt {
    flex-shrink: 0;
    /* min-height, not height: the label swaps to a longer sentence while
       talking, and a fixed height would clip it if it ever wrapped. */
    min-height: 34px;
    padding: 6px 12px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-tile);
    background: var(--fill-base);
    color: var(--text-primary);
    font: 700 var(--text-caption) var(--font-ui);
    cursor: pointer;
    /* The label swaps between two lengths; both must stay fully visible. */
    white-space: normal;
    text-wrap: balance;
    touch-action: none;
    user-select: none;
    transition:
      background var(--motion-fast) var(--ease-standard),
      color var(--motion-fast) var(--ease-standard);
  }

  .ptt:hover:not(:disabled) {
    background: var(--fill-bright);
  }

  .ptt:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .ptt:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .ptt.talking {
    background: var(--live-tint);
    border-color: var(--live-bright);
    color: var(--live-bright);
  }

  .text-row {
    display: flex;
    align-items: center;
    flex-shrink: 0;
    gap: 6px;
  }

  .text-input {
    flex: 1 1 auto;
    min-width: 0;
    height: 32px;
    padding: 0 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-tile);
    background: var(--fill-weak);
    color: var(--text-primary);
    font: 500 var(--text-caption) var(--font-ui);
  }

  .text-input::placeholder {
    color: var(--text-faint);
  }

  .text-input:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .text-input:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .text-send {
    flex-shrink: 0;
    height: 32px;
    padding: 0 12px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-tile);
    background: var(--fill-base);
    color: var(--text-primary);
    font: 700 var(--text-caption) var(--font-ui);
    cursor: pointer;
    transition: background var(--motion-fast) var(--ease-standard);
  }

  .text-send:hover:not(:disabled) {
    background: var(--fill-bright);
  }

  .text-send:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .text-send:disabled {
    cursor: default;
    opacity: 0.5;
  }
</style>
