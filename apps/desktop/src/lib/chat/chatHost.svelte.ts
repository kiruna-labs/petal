// Desktop chat host (plugins/README.md §2.7, chat as a HOST surface): owns the
// shared store, listens to Rust's `chat-data` events, publishes through
// `chat_publish`, and answers late joiners' history requests. The route
// renders `ChatDrawer` from the reactive mirror this exposes. All parsing and
// attribution rules live in shared/logic/chat.ts; Rust only transports and
// stamps the sender.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { COMMANDS, EVENTS, hasTauriBridge, type ChatDataEvent } from '$lib/ipc';
import {
  CHAT_HISTORY_REQUEST_DELAYS_MS,
  chatMessageWire,
  chatNoticeText,
  chatPostWire,
  chatPrivateNoticeText,
  createChatStore,
  encodeChatWire,
  parseChatPayload,
  type ChatMessage,
  type ChatSender,
  type ChatVia,
  type ChatWire,
} from '@petal/shared/logic/chat';
import { bridgeFailure } from '@petal/shared/plugin-host/broker';
import { base64ToBytes, bytesToBase64 } from '@petal/shared/plugin-host/topics';

export interface ChatHostDeps {
  selfIdentity: () => string | null;
  selfName: () => string | null;
  /** Presence lookup so a message from a peer whose LiveKit name is blank still gets the roster name. */
  participantName: (identity: string) => string | null;
  /** Shown while the drawer is closed: "Mira: hello there", or a send failure. */
  onNotice: (text: string) => void;
}

export interface ChatHost {
  readonly messages: readonly ChatMessage[];
  readonly unread: number;
  readonly open: boolean;
  setOpen(open: boolean): void;
  toggle(): void;
  send(text: string): Promise<void>;
  /** A loaded plugin posts for everyone (plugin host `chat.post`); always stamped with `via`. */
  post(via: ChatVia, text: string): Promise<void>;
  /** A plugin's private answer to the local user's command; shown here only. */
  notice(via: ChatVia, text: string): void;
  /** The room is connected: ask peers for what was said before we arrived (once per connection). */
  onConnected(): void;
  onDisconnected(): void;
  dispose(): void;
}

export function createChatHost(deps: ChatHostDeps): ChatHost {
  const store = createChatStore(deps.selfIdentity);
  let messages = $state.raw<readonly ChatMessage[]>([]);
  let unread = $state(0);
  let open = $state(false);
  store.onChange(() => {
    messages = store.messages;
    unread = store.unread;
    open = store.open;
  });

  let unlisten: UnlistenFn | undefined;
  let disposed = false;
  let historyTimers: ReturnType<typeof setTimeout>[] = [];
  let historyAnswered = false;
  let connected = false;

  function stopHistoryRequests(): void {
    for (const t of historyTimers) clearTimeout(t);
    historyTimers = [];
  }

  function self(): ChatSender {
    return { identity: deps.selfIdentity() ?? '', name: deps.selfName() };
  }

  async function publish(wire: ChatWire, requester?: string): Promise<void> {
    if (!hasTauriBridge()) return;
    const direct = (wire.type === 'history' || wire.type === 'history-posts') && requester;
    await invoke(COMMANDS.chatPublish, {
      payloadBase64: bytesToBase64(encodeChatWire(wire)),
      destinationIdentities: direct ? [requester] : undefined,
    });
  }

  function onData(event: ChatDataEvent): void {
    const wire = parseChatPayload(base64ToBytes(event.payloadBase64));
    if (!wire) return;
    const sender: ChatSender = {
      identity: event.senderIdentity,
      name: event.senderName ?? deps.participantName(event.senderIdentity),
    };
    switch (wire.type) {
      case 'msg':
      case 'post': {
        const result = store.receive(wire, sender);
        if (result === 'added' && sender.identity !== deps.selfIdentity() && !store.open) {
          deps.onNotice(chatNoticeText(sender, wire.text, 80, wire.type === 'post' ? wire.via : null));
        }
        break;
      }
      case 'history-req': {
        const reply = store.historyReply();
        if (reply.history) void publish(reply.history, sender.identity).catch(() => {});
        if (reply.posts) void publish(reply.posts, sender.identity).catch(() => {});
        break;
      }
      case 'history':
      case 'history-posts':
        historyAnswered = true;
        stopHistoryRequests();
        store.mergeHistory(wire, sender);
        break;
    }
  }

  if (hasTauriBridge()) {
    listen<ChatDataEvent>(EVENTS.chatData, (event) => onData(event.payload))
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch(() => {});
  }

  return {
    get messages() {
      return messages;
    },
    get unread() {
      return unread;
    },
    get open() {
      return open;
    },
    setOpen(next) {
      store.setOpen(next);
    },
    toggle() {
      store.setOpen(!store.open);
    },
    async send(text) {
      const wire = chatMessageWire(text);
      store.sent(wire, self());
      try {
        await publish(wire);
      } catch (e) {
        console.error('chat_publish failed', e);
        deps.onNotice("Couldn't send your message. Check your connection and try again.");
      }
    },
    async post(via, text) {
      if (!connected) throw bridgeFailure('unavailable', 'not connected to a meeting');
      const wire = chatPostWire(text, via);
      store.sent(wire, self());
      await publish(wire);
    },
    notice(via, text) {
      store.notice(text, via, self());
      if (!store.open) deps.onNotice(chatPrivateNoticeText(via, text));
    },
    onConnected() {
      if (connected) return;
      connected = true;
      // Repeats on a short backoff until a peer answers: the first publish
      // right after connect can race the data channel (shared/logic/chat.ts).
      stopHistoryRequests();
      historyAnswered = false;
      for (const delay of CHAT_HISTORY_REQUEST_DELAYS_MS) {
        historyTimers.push(
          setTimeout(() => {
            if (!historyAnswered && connected) void publish({ v: 1, type: 'history-req' }).catch(() => {});
          }, delay),
        );
      }
    },
    onDisconnected() {
      connected = false;
      stopHistoryRequests();
    },
    dispose() {
      disposed = true;
      stopHistoryRequests();
      unlisten?.();
      unlisten = undefined;
    },
  };
}
