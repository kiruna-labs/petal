<!--
  ChatDrawer — the meeting chat panel both clients mount (plugins/README.md
  §2.7 "Panel (drawer)"). Presentational: the host owns the store
  (shared/logic/chat.ts), the transport, the plugin command registry, and
  open/closed state; this renders `messages`, calls `onSend` with a
  normalized message, `onCommand` with a parsed `/name args`, and `onClose`.

  Plugins (I-7b): a message a plugin posted shows "<person> · via <plugin>"
  with the puzzle badge, never as the person alone; a plugin's private answer
  to your own command shows as the plugin, marked "Only you". Typing `/`
  lists the commands installed plugins own; an unknown `/word` is refused
  under the composer instead of being sent to everyone, and `//` sends a
  message that starts with `/`.

  Fit rules: designed for a 320 px column and verified at the 400 px window
  (apps/desktop/tests/chatDrawerRendered.test.ts): long words break, long
  names ellipsize, plugin names and command text wrap, nothing scrolls
  horizontally. Enter sends, Shift+Enter inserts a line break. The list pins
  to the bottom while the reader is at the bottom and stays put when they
  have scrolled up to read.
-->
<script lang="ts">
  import { tick } from 'svelte';
  import {
    CHAT_LIMITS,
    classifyChatInput,
    formatChatTime,
    matchingChatCommands,
    type ChatCommandOption,
    type ChatCommandResult,
    type ChatMessage,
  } from '../../logic/chat.ts';
  import { pluginIconSvg } from '../../plugin-host/icons.ts';

  interface Props {
    messages: readonly ChatMessage[];
    /** Whether we can send right now (connected). The composer stays visible but disabled otherwise. */
    canSend?: boolean;
    onSend: (text: string) => void | Promise<void>;
    onClose: () => void;
    /** Slash commands installed plugins own (host.chatCommands()). */
    commands?: readonly ChatCommandOption[];
    /** Run `/name args`; a failure's message is shown under the composer and the draft is kept. */
    onCommand?: (name: string, args: string) => ChatCommandResult | Promise<ChatCommandResult>;
    /** Overrides for tests / fixed clocks. */
    now?: () => number;
    locale?: string;
  }

  let { messages, canSend = true, onSend, onClose, commands = [], onCommand, now = () => Date.now(), locale }: Props = $props();

  let draft = $state('');
  let listEl = $state<HTMLElement | null>(null);
  let composerEl = $state<HTMLTextAreaElement | null>(null);
  let stuckToBottom = $state(true);
  let composerError = $state<string | null>(null);
  let highlighted = $state(0);
  /** The draft the suggestion list was dismissed for (Escape); it reopens as soon as the draft changes. */
  let dismissedFor = $state<string | null>(null);

  const remaining = $derived(CHAT_LIMITS.maxTextChars - draft.length);
  const overLimit = $derived(remaining < 0);
  const input = $derived(classifyChatInput(draft));
  const sendable = $derived(canSend && !overLimit && input.kind !== 'empty');
  const suggestions = $derived(dismissedFor === draft ? [] : matchingChatCommands(commands, draft));
  const suggestionsId = 'chat-command-suggestions';

  // Any edit clears the last refusal and re-targets the first suggestion.
  $effect(() => {
    void draft;
    composerError = null;
    highlighted = 0;
  });

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

  export function focusComposer(): void {
    composerEl?.focus();
  }

  function complete(option: ChatCommandOption): void {
    draft = `/${option.name} `;
    composerEl?.focus();
  }

  async function send(): Promise<void> {
    if (!canSend) return;
    const current = input;
    switch (current.kind) {
      case 'empty':
        return;
      case 'invalid':
        composerError = current.reason;
        return;
      case 'invalid-command':
        composerError = `/${current.token} is not a command. To send a message that starts with /, type // first.`;
        return;
      case 'command': {
        if (!onCommand) {
          composerError = 'Commands are not available here.';
          return;
        }
        const sentDraft = draft;
        const result = await onCommand(current.name, current.args);
        if (!result.ok) {
          composerError = result.message;
          return;
        }
        if (draft === sentDraft) draft = '';
        stuckToBottom = true;
        composerEl?.focus();
        return;
      }
      case 'message':
        draft = '';
        stuckToBottom = true;
        await onSend(current.text);
        composerEl?.focus();
        return;
    }
  }

  function onComposerKeydown(event: KeyboardEvent): void {
    if (suggestions.length > 0) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const step = event.key === 'ArrowDown' ? 1 : -1;
        highlighted = (highlighted + step + suggestions.length) % suggestions.length;
        return;
      }
      const option = suggestions[Math.min(highlighted, suggestions.length - 1)]!;
      const exact = draft.trim() === `/${option.name}`;
      if (event.key === 'Tab' || (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !exact)) {
        event.preventDefault();
        complete(option);
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        dismissedFor = draft;
        return;
      }
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void send();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      onClose();
    }
  }

  function viaKey(m: ChatMessage): string {
    return `${m.via?.id ?? ''}|${m.local ? 'local' : ''}`;
  }

  /** Group consecutive messages from one sender (and one plugin, if any) within two minutes under one name line. */
  function continues(index: number): boolean {
    if (index === 0) return false;
    const prev = messages[index - 1]!;
    const cur = messages[index]!;
    return prev.sender.identity === cur.sender.identity && viaKey(prev) === viaKey(cur) && cur.t - prev.t < 2 * 60_000;
  }

  function nameFor(m: ChatMessage): string {
    if (m.local && m.via) return m.via.name;
    if (m.self) return 'You';
    return m.sender.name?.trim() || 'Someone';
  }

  function viaTitle(m: ChatMessage): string {
    const person = m.self ? 'you' : m.sender.name?.trim() || 'someone';
    return m.local
      ? `A private answer from the ${m.via!.name} plugin (${m.via!.id}). Only you can see it.`
      : `Posted by the ${m.via!.name} plugin (${m.via!.id}) for ${person}`;
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
      <article
        class="chat-msg"
        class:self={m.self}
        class:continues={continues(i)}
        class:plugin={m.via !== null}
        class:local={m.local}
        data-testid="chat-msg"
        data-relayed={m.relayed ? 'true' : undefined}
        data-via={m.via?.id}
        data-local={m.local ? 'true' : undefined}
      >
        {#if !continues(i)}
          <div class="chat-meta">
            <span class="chat-name" title={m.local ? viaTitle(m) : (m.sender.name ?? m.sender.identity)}>{nameFor(m)}</span>
            <time class="chat-time" datetime={new Date(m.t).toISOString()}>{formatChatTime(m.t, now(), locale)}</time>
          </div>
          {#if m.via}
            <!-- Its own line under the name: the plugin label wraps, never
                 clips, and never squeezes the (ellipsizing) person name. -->
            <div class="chat-via" title={viaTitle(m)} data-testid="chat-via">
              <span class="chat-puzzle" aria-hidden="true">{@html pluginIconSvg('puzzle', 11)}</span>
              {#if m.local}
                <span>Only you can see this</span>
              {:else}
                <span>via {m.via.name}</span>
              {/if}
            </div>
          {/if}
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
    {#if suggestions.length > 0}
      <ul class="chat-suggestions" id={suggestionsId} role="listbox" aria-label="Commands" data-testid="chat-suggestions">
        {#each suggestions as option, i (option.name)}
          <li
            id={`${suggestionsId}-${option.name}`}
            class="chat-suggestion"
            class:active={i === highlighted}
            role="option"
            aria-selected={i === highlighted}
            data-testid="chat-suggestion"
            onmousedown={(e) => {
              // mousedown, not click: keep focus in the composer.
              e.preventDefault();
              complete(option);
            }}
            onmouseenter={() => (highlighted = i)}
          >
            <span class="chat-suggestion-line">
              <span class="chat-suggestion-name">/{option.name}</span>
              {#if option.usage}<span class="chat-suggestion-usage">{option.usage}</span>{/if}
            </span>
            <span class="chat-suggestion-desc">{option.description}</span>
            <span class="chat-suggestion-plugin">
              <span class="chat-puzzle" aria-hidden="true">{@html pluginIconSvg('puzzle', 10)}</span>{option.pluginName}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
    <textarea
      bind:this={composerEl}
      bind:value={draft}
      class="chat-input"
      rows="1"
      placeholder={canSend ? 'Message everyone' : 'Reconnecting…'}
      aria-label="Message"
      aria-autocomplete="list"
      aria-controls={suggestions.length > 0 ? suggestionsId : undefined}
      aria-activedescendant={suggestions.length > 0 ? `${suggestionsId}-${suggestions[Math.min(highlighted, suggestions.length - 1)]!.name}` : undefined}
      aria-describedby={composerError ? 'chat-composer-error' : undefined}
      disabled={!canSend}
      maxlength={CHAT_LIMITS.maxTextChars + 200}
      onkeydown={onComposerKeydown}
      data-testid="chat-input"
    ></textarea>
    {#if composerError}
      <p class="chat-error" id="chat-composer-error" role="alert" data-testid="chat-error">{composerError}</p>
    {/if}
    <div class="chat-compose-row">
      <span class="chat-count" class:over={overLimit} aria-live={overLimit ? 'polite' : 'off'}>
        {#if remaining < 200}{remaining}{/if}
      </span>
      <button type="submit" class="chat-send" disabled={!sendable} data-testid="chat-send">Send</button>
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
    flex-direction: column;
    gap: 6px;
  }
  .chat-input {
    width: 100%;
    box-sizing: border-box;
    resize: none;
    min-height: 36px;
    max-height: 120px;
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
  .chat-compose-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .chat-count {
    font-size: 11px;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    font-variant-numeric: tabular-nums;
    min-height: 1em;
  }
  .chat-count.over {
    color: var(--warning, #f0b429);
  }
  .chat-send {
    border: 0;
    border-radius: 8px;
    padding: 6px 14px;
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

  /* Plugin posts (I-7b): the via line sits under the name and wraps, never clips. */
  .chat-via {
    display: flex;
    align-items: center;
    gap: 3px;
    margin: -1px 0 2px;
    font-size: 11px;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    overflow-wrap: anywhere;
    min-width: 0;
  }
  .chat-suggestion-plugin {
    display: inline-flex;
    align-items: center;
    gap: 3px;
  }
  .chat-puzzle {
    display: inline-flex;
    flex: none;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
  }
  .chat-msg.local .chat-text {
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
  }

  .chat-suggestions {
    list-style: none;
    margin: 0;
    padding: 4px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 180px;
    overflow-y: auto;
    border-radius: 9px;
    border: 1px solid var(--hairline-strong, rgba(255, 255, 255, 0.1));
    background: var(--surface-raised, #1f2024);
  }
  .chat-suggestion {
    display: flex;
    flex-direction: column;
    gap: 1px;
    padding: 6px 8px;
    border-radius: 6px;
    cursor: pointer;
    min-width: 0;
  }
  .chat-suggestion.active {
    background: var(--fill-strong, rgba(255, 255, 255, 0.08));
  }
  .chat-suggestion-line {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    align-items: baseline;
  }
  .chat-suggestion-name {
    font-weight: 600;
    color: var(--text-strong, rgba(255, 255, 255, 0.88));
  }
  .chat-suggestion-usage,
  .chat-suggestion-desc,
  .chat-suggestion-plugin {
    font-size: 11.5px;
    color: var(--text-dim, rgba(255, 255, 255, 0.62));
    overflow-wrap: anywhere;
  }
  .chat-suggestion-usage {
    font-family: var(--font-mono, ui-monospace, monospace);
  }
  .chat-suggestion-desc {
    color: var(--text-soft, rgba(255, 255, 255, 0.75));
  }
  .chat-error {
    margin: 0;
    font-size: 11.5px;
    line-height: 1.4;
    color: var(--warning, #f0b429);
    overflow-wrap: anywhere;
  }
</style>
