<!--
  Receiver-side AI-chat transcript + typed-message overlay (#844) -- a native
  child webview, layered ABOVE the remote window's decoded video NSView,
  replacing the old in-webview popover
  (`RemoteWindowHeader.svelte`'s former `.ai-chat-remote-panel`) that the
  video always covered and made unclickable. Shown/hidden by
  `compositor_set_ai_chat_overlay_open` when the header's "AI chat live"
  badge is toggled -- see `create_ai_chat_overlay`/`ai_chat_label_for_key` in
  src-tauri/src/compositor.rs, and every retire/reveal/teardown site that
  function's doc comment points at.

  This is a CHILD overlay webview (same `create_chrome_webview` family as
  control.html/pointer.html), so the Tauri EVENT bus does not reliably reach
  it on macOS (see pointer/+page.svelte's doc comment for the measured
  reason). State/transcript updates are instead pushed directly via
  `webview.eval` from `ai_chat/topic.rs`'s `push_remote_state_to_overlay`/
  `push_remote_transcript_to_overlay`, landing here as
  `window.__petalAiChatRemoteState`/`window.__petalAiChatRemoteTranscript` --
  the same eval-injection pattern telepointer.rs already uses for the pointer
  overlay's `window.__petalTelepointer`. SENDING (typed text) still goes
  through a plain `invoke` call, which works fine from any webview -- only
  the event *subscription* side needed the workaround.

  windowId/owner come from this route's own URL query params (baked in once
  at `create_ai_chat_overlay` and never re-navigated for a different window,
  unlike control/pointer -- see `ai_chat_route_url`'s doc comment) rather
  than from any override mechanism.

  The in-strip hold-to-talk button and refusal chip (fc4d8ec0, #844
  predecessor) stay in RemoteWindowHeader.svelte -- only the transcript and
  the typed-message input, which need the video-covered area, live here.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    AI_CHAT_TEXT_MAX_CHARS,
    AI_CHAT_TRANSCRIPT_MAX_TURNS,
    aiChatEndReasonMessage,
    appendTranscriptDelta,
    type AiChatTranscriptTurn
  } from '$lib/data/aiChat';
  import { COMMANDS, type AiChatRemoteSessionState, type AiChatRemoteTranscriptEvent } from '$lib/ipc';

  const windowId = $derived(Number(page.url.searchParams.get('windowId') ?? '0'));
  const ownerIdentity = $derived(page.url.searchParams.get('owner') ?? '');

  let session = $state<AiChatRemoteSessionState | null>(null);
  let turns = $state<AiChatTranscriptTurn[]>([]);
  let transcriptEl = $state<HTMLDivElement | null>(null);
  let draft = $state('');
  let sending = $state(false);

  const active = $derived(session?.active === true);
  const errorMessage = $derived(session?.error ? aiChatEndReasonMessage(session.error) : null);
  const trimmedDraft = $derived(draft.trim());
  const canSend = $derived(
    active && !sending && trimmedDraft.length > 0 && trimmedDraft.length <= AI_CHAT_TEXT_MAX_CHARS
  );

  // Keep the newest turn in view as deltas stream in, matching the local
  // panel's transcript (AiChatPanel.svelte) and the old in-header popover.
  $effect(() => {
    void turns.length;
    const el = transcriptEl;
    if (el) el.scrollTop = el.scrollHeight;
  });

  async function refreshSession() {
    if (!Number.isFinite(windowId) || windowId <= 0 || !ownerIdentity) return;
    try {
      session = await invoke<AiChatRemoteSessionState | null>(COMMANDS.aiChatRemoteSession, {
        windowId,
        ownerIdentity
      });
    } catch {
      session = null;
    }
  }

  function applyRemoteState(payload: AiChatRemoteSessionState) {
    if (payload.windowId !== windowId || payload.ownerIdentity !== ownerIdentity) return;
    // A fresh session start (inactive -> active) begins a new conversation --
    // clear whatever the previous session left behind rather than mixing an
    // old exchange into a new one (matches the old header popover's rule).
    if (payload.active && session?.active !== true) turns = [];
    session = payload;
  }

  function applyRemoteTranscript(payload: AiChatRemoteTranscriptEvent) {
    if (payload.windowId !== windowId || payload.ownerIdentity !== ownerIdentity) return;
    turns = appendTranscriptDelta(turns, payload, AI_CHAT_TRANSCRIPT_MAX_TURNS);
  }

  async function sendText() {
    if (!canSend) return;
    const text = trimmedDraft;
    sending = true;
    try {
      await invoke(COMMANDS.aiChatRequestSendText, { windowId, ownerIdentity, text });
      draft = '';
    } catch {
      // Fire-and-forget, matching the old header popover: the owner's
      // machine validates for real; a rejected request just leaves the
      // draft in the box for the user to retry.
    } finally {
      sending = false;
    }
  }

  function onComposerKeydown(event: KeyboardEvent) {
    if (event.key !== 'Enter' || event.shiftKey) return;
    event.preventDefault();
    void sendText();
  }

  function closeOverlay() {
    invoke(COMMANDS.compositorSetAiChatOverlayOpen, { windowId, ownerIdentity, open: false }).catch(
      () => {}
    );
  }

  function onWindowKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') closeOverlay();
  }

  onMount(() => {
    void refreshSession();
    (
      window as typeof window & {
        __petalAiChatRemoteState?: (state: AiChatRemoteSessionState) => void;
      }
    ).__petalAiChatRemoteState = applyRemoteState;
    (
      window as typeof window & {
        __petalAiChatRemoteTranscript?: (delta: AiChatRemoteTranscriptEvent) => void;
      }
    ).__petalAiChatRemoteTranscript = applyRemoteTranscript;
    window.addEventListener('keydown', onWindowKeydown);
    return () => {
      window.removeEventListener('keydown', onWindowKeydown);
    };
  });
</script>

<div class="overlay">
  {#if turns.length > 0}
    <div class="transcript" bind:this={transcriptEl} aria-live="polite">
      {#each turns as turn (turn.id)}
        <div class="turn is-{turn.role}">
          <span class="turn-role">{turn.role === 'assistant' ? 'AI' : 'You & room'}</span>
          <span class="turn-text">{turn.text}</span>
        </div>
      {/each}
    </div>
  {:else if errorMessage}
    <p class="empty-state">{errorMessage}</p>
  {:else}
    <p class="empty-state">No messages yet.</p>
  {/if}

  <form
    class="composer"
    onsubmit={(event) => {
      event.preventDefault();
      void sendText();
    }}
  >
    <input
      class="composer-input"
      type="text"
      placeholder="Type a message…"
      bind:value={draft}
      disabled={!active || sending}
      maxlength={AI_CHAT_TEXT_MAX_CHARS}
      onkeydown={onComposerKeydown}
      aria-label="Type a message to the AI on this window"
    />
    <button type="submit" class="composer-send" disabled={!canSend}>Send</button>
  </form>
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .overlay {
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    gap: 6px;
    width: 100%;
    height: 100%;
    padding: 8px;
    border-radius: var(--radius-tile);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--shadow-float);
  }

  .transcript {
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding-right: 2px;
  }

  .turn {
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px 8px;
    border-radius: var(--radius-chip);
    background: rgba(255, 255, 255, 0.04);
    font: 500 12px var(--font-ui);
  }

  .turn.is-assistant {
    background: color-mix(in srgb, var(--live-bright) 10%, transparent);
  }

  .turn-role {
    font: 700 10px var(--font-ui);
    color: var(--text-faint);
    text-transform: uppercase;
    letter-spacing: 0.02em;
  }

  .turn-text {
    color: var(--text-primary);
    /* Never truncate a transcript line (CLAUDE.md: UI text must never
       truncate) -- wrap, preserving any breaks the sender sent. */
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .empty-state {
    flex: 1 1 auto;
    min-height: 0;
    margin: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    text-align: center;
    color: var(--text-faint);
    font: 500 12px var(--font-ui);
    overflow-wrap: anywhere;
  }

  .composer {
    flex-shrink: 0;
    display: flex;
    gap: 6px;
  }

  .composer-input {
    flex: 1 1 auto;
    min-width: 0;
    height: 28px;
    padding: 0 8px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-tile);
    background: rgba(255, 255, 255, 0.04);
    color: var(--text-primary);
    font: 500 12px var(--font-ui);
  }

  .composer-input::placeholder {
    color: var(--text-faint);
  }

  .composer-input:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .composer-input:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .composer-send {
    flex-shrink: 0;
    height: 28px;
    padding: 0 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-tile);
    background: rgba(255, 255, 255, 0.06);
    color: var(--text-primary);
    font: 700 12px var(--font-ui);
    cursor: pointer;
    transition: background var(--motion-fast) var(--ease-standard);
  }

  .composer-send:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.10);
  }

  .composer-send:focus-visible {
    outline: 2px solid var(--id-blue);
    outline-offset: 2px;
  }

  .composer-send:disabled {
    cursor: default;
    opacity: 0.5;
  }
</style>
