<!--
  ChatDrawer — the meeting chat panel both clients mount (plugins/README.md
  §2.7 "Panel (drawer)"). Presentational: the host owns the store
  (shared/logic/chat.ts), the transport, and open/closed state; this renders
  `messages`, calls `onSend` with the raw composer text, and `onClose`.

  Fit rules: designed for a 320 px column and verified at the 400 px window
  and the 360 px-tall one (apps/desktop/tests/chatDrawerRendered.test.ts)
  and on a landscape phone (web-harness/tests/chatDrawerLayoutRendered.test.ts):
  long words break, long names ellipsize, nothing scrolls horizontally. The
  composer is one row, Send beside the input, and the character count only
  shows near the limit, so short viewports keep their height for messages
  (#246). Enter sends, Shift+Enter inserts a line break. The list pins to the
  bottom while the reader is at the bottom and stays put when they have
  scrolled up to read.
-->
<script lang="ts">
  import { tick } from 'svelte';
  import { CHAT_LIMITS, formatChatTime, normalizeChatText, type ChatMessage } from '../../logic/chat.ts';

  interface Props {
    messages: readonly ChatMessage[];
    /** Whether we can send right now (connected). The composer stays visible but disabled otherwise. */
    canSend?: boolean;
    onSend: (text: string) => void | Promise<void>;
    onClose: () => void;
    /** Overrides for tests / fixed clocks. */
    now?: () => number;
    locale?: string;
  }

  let { messages, canSend = true, onSend, onClose, now = () => Date.now(), locale }: Props = $props();

  let draft = $state('');
  let listEl = $state<HTMLElement | null>(null);
  let composerEl = $state<HTMLTextAreaElement | null>(null);
  let stuckToBottom = $state(true);

  const remaining = $derived(CHAT_LIMITS.maxTextChars - draft.length);
  const overLimit = $derived(remaining < 0);
  const sendable = $derived(canSend && !overLimit && normalizeChatText(draft) !== null);

  function onListScroll(): void {
    if (!listEl) return;
    stuckToBottom = listEl.scrollHeight - listEl.scrollTop - listEl.clientHeight < 24;
  }

  // New messages: follow the bottom only if the reader was already there.
  $effect(() => {
    void messages.length;
    if (!stuckToBottom) return;
    void tick().then(() => {
      if (listEl) listEl.scrollTop = listEl.scrollHeight;
    });
  });

  // The list also shrinks without a new message: the composer grows while
  // typing, or the soft keyboard resizes the page. Keep the newest line in
  // view for a reader who was at the bottom.
  $effect(() => {
    const el = listEl;
    if (!el) return;
    const observer = new ResizeObserver(() => {
      if (stuckToBottom) el.scrollTop = el.scrollHeight;
    });
    observer.observe(el);
    return () => observer.disconnect();
  });

  export function focusComposer(): void {
    composerEl?.focus();
  }

  async function send(): Promise<void> {
    const text = normalizeChatText(draft);
    if (!text || !canSend) return;
    draft = '';
    stuckToBottom = true;
    await onSend(text);
    composerEl?.focus();
  }

  function onComposerKeydown(event: KeyboardEvent): void {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void send();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      onClose();
    }
  }

  /** Group consecutive messages from one sender within two minutes under one name line. */
  function continues(index: number): boolean {
    if (index === 0) return false;
    const prev = messages[index - 1];
    const cur = messages[index];
    return prev.sender.identity === cur.sender.identity && cur.t - prev.t < 2 * 60_000;
  }

  function nameFor(m: ChatMessage): string {
    if (m.self) return 'You';
    return m.sender.name?.trim() || 'Someone';
  }
</script>

<section class="chat-drawer" aria-label="Meeting chat" data-testid="chat-drawer">
  <header class="chat-head">
    <h2 class="chat-title">Chat</h2>
    <button type="button" class="chat-close" aria-label="Close chat" onclick={onClose}>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true">
        <path d="M6 6l12 12M18 6L6 18" />
      </svg>
    </button>
  </header>

  <div class="chat-list" bind:this={listEl} onscroll={onListScroll} role="log" aria-live="polite" aria-relevant="additions" data-testid="chat-list">
    {#if messages.length === 0}
      <p class="chat-empty">No messages yet. Everyone in the meeting sees what you send here.</p>
    {/if}
    {#each messages as m, i (m.id)}
      <article class="chat-msg" class:self={m.self} class:continues={continues(i)} data-testid="chat-msg" data-relayed={m.relayed ? 'true' : undefined}>
        {#if !continues(i)}
          <div class="chat-meta">
            <span class="chat-name" title={m.sender.name ?? m.sender.identity}>{nameFor(m)}</span>
            <time class="chat-time" datetime={new Date(m.t).toISOString()}>{formatChatTime(m.t, now(), locale)}</time>
          </div>
        {/if}
        <p class="chat-text">{m.text}</p>
      </article>
    {/each}
  </div>

  <form
    class="chat-compose"
    onsubmit={(e) => {
      e.preventDefault();
      void send();
    }}
  >
    <textarea
      bind:this={composerEl}
      bind:value={draft}
      class="chat-input"
      rows="1"
      placeholder={canSend ? 'Message everyone' : 'Reconnecting…'}
      aria-label="Message"
      disabled={!canSend}
      maxlength={CHAT_LIMITS.maxTextChars + 200}
      onkeydown={onComposerKeydown}
      data-testid="chat-input"
    ></textarea>
    <div class="chat-compose-side">
      <span class="chat-count" class:over={overLimit} aria-live="polite">{remaining < 200 ? remaining : ''}</span>
      <!-- Keep focus in the input on tap, so the soft keyboard stays up
           between messages; click still submits. -->
      <button type="submit" class="chat-send" disabled={!sendable} data-testid="chat-send" onpointerdown={(e) => e.preventDefault()}>Send</button>
    </div>
  </form>
</section>

<style>
  .chat-drawer {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    min-width: 0;
    background: var(--surface, #16171a);
    color: var(--text-strong, rgba(255, 255, 255, 0.88));
    border-left: 1px solid var(--hairline, rgba(255, 255, 255, 0.07));
    font-size: 13px;
    line-height: 1.45;
  }
  .chat-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 10px 12px 10px 14px;
    border-bottom: 1px solid var(--hairline, rgba(255, 255, 255, 0.07));
    flex: none;
  }
  .chat-title {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .chat-close {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border: 0;
    border-radius: 7px;
    background: transparent;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    cursor: pointer;
  }
  .chat-close:hover {
    background: var(--fill-base, rgba(255, 255, 255, 0.06));
    color: var(--text-strong, rgba(255, 255, 255, 0.88));
  }
  .chat-close:focus-visible,
  .chat-send:focus-visible,
  .chat-input:focus-visible {
    outline: var(--focus-ring-width, 2px) solid var(--focus-ring, #4c8dff);
    outline-offset: 1px;
  }

  .chat-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overflow-x: hidden;
    padding: 10px 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    scrollbar-width: thin;
  }
  .chat-empty {
    margin: auto 0;
    text-align: center;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    padding: 24px 8px;
  }
  .chat-msg {
    min-width: 0;
  }
  .chat-msg.continues {
    margin-top: -6px;
  }
  .chat-meta {
    display: flex;
    align-items: baseline;
    gap: 8px;
    min-width: 0;
    margin-bottom: 2px;
  }
  .chat-name {
    font-weight: 600;
    color: var(--text-strong, rgba(255, 255, 255, 0.88));
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .chat-msg.self .chat-name {
    color: var(--text-soft, rgba(255, 255, 255, 0.75));
  }
  .chat-time {
    flex: none;
    font-size: 11px;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    font-variant-numeric: tabular-nums;
  }
  .chat-text {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    word-break: break-word;
    color: var(--text-soft, rgba(255, 255, 255, 0.75));
  }
  .chat-msg.self .chat-text {
    color: var(--text-strong, rgba(255, 255, 255, 0.88));
  }

  .chat-compose {
    flex: none;
    padding: 10px 12px 12px;
    border-top: 1px solid var(--hairline, rgba(255, 255, 255, 0.07));
    display: flex;
    align-items: flex-end;
    gap: 8px;
  }
  .chat-input {
    flex: 1;
    min-width: 0;
    box-sizing: border-box;
    resize: none;
    min-height: 36px;
    /* Grows while typing, to about a third of a short window but always
       two whole lines. */
    max-height: clamp(55px, 30vh, 120px);
    padding: 8px 10px;
    border-radius: 9px;
    border: 1px solid var(--hairline-strong, rgba(255, 255, 255, 0.1));
    background: var(--fill-weak, rgba(255, 255, 255, 0.04));
    color: var(--text-strong, rgba(255, 255, 255, 0.88));
    font: inherit;
    line-height: 1.4;
    field-sizing: content;
  }
  .chat-input::placeholder {
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
  }
  .chat-input:disabled {
    opacity: var(--disabled-opacity, 0.38);
  }
  /* Send beside the input. Near the limit (past 1800 characters) the count
     stacks above Send, in the space the grown input leaves. */
  .chat-compose-side {
    flex: none;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
  }
  .chat-count {
    font-size: 11px;
    line-height: 1;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    font-variant-numeric: tabular-nums;
  }
  .chat-count:empty {
    display: none;
  }
  .chat-count.over {
    color: var(--warning, #f0b429);
  }
  .chat-send {
    height: 36px;
    border: 0;
    border-radius: 8px;
    padding: 0 14px;
    font: inherit;
    font-weight: 600;
    background: var(--text-strong, rgba(255, 255, 255, 0.88));
    color: var(--bg-base, #0a0a0b);
    cursor: pointer;
  }
  .chat-send:disabled {
    opacity: var(--disabled-opacity, 0.38);
    cursor: default;
  }

  /* Short viewports (a phone in landscape, #246): a compact header, list and
     composer, so the height goes to messages. */
  @media (max-height: 500px) {
    .chat-head {
      padding-block: 4px;
    }
    .chat-list {
      padding-block: 6px;
      gap: 6px;
    }
    .chat-msg.continues {
      margin-top: -4px;
    }
    .chat-compose {
      padding: 8px 10px 10px;
    }
  }
</style>
