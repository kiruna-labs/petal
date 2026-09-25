// Browser chat host (plugins/README.md §2.7, chat as a HOST surface): owns the
// shared store, mounts the shared ChatDrawer beside the tiles, drives the
// Chat control's badge, publishes on `petal.chat`, and answers late joiners'
// history requests. The connection's topic dispatcher hands inbound packets
// to `onData` with the authenticated participant; nothing in the payload is
// trusted for attribution (shared/logic/chat.ts).
//
// `.svelte.ts` so the drawer's props can be runes: the mounted component reads
// `view` through getters and re-renders when the store changes.
import type { Participant as LkParticipant, Room } from 'livekit-client';
import { mount, tick, unmount } from 'svelte';
import ChatDrawer from '@petal/shared/ui/components/ChatDrawer.svelte';
import {
  CHAT_HISTORY_REQUEST_DELAYS_MS,
  chatMessageWire,
  chatNoticeText,
  chatPublishOptions,
  createChatStore,
  encodeChatWire,
  parseChatPayload,
  type ChatMessage,
  type ChatSender,
  type ChatWire,
} from '@petal/shared/logic/chat';
import type { HarnessContext } from '../context.ts';
import { displayNameForParticipant } from '../tiles.ts';

export interface ChatHook {
  roomConnected(room: Room): void;
  roomDisconnected(): void;
  /** Inbound `petal.chat` packet from the connection's topic dispatcher. */
  onData(payload: Uint8Array, participant: LkParticipant | undefined, senderIdentity: string | undefined): void;
  readonly open: boolean;
  setOpen(open: boolean): void;
  /** Automation seam (live smoke driver): what the drawer currently shows. */
  messages(): readonly ChatMessage[];
  send(text: string): Promise<void>;
}

export function setupChat(ctx: HarnessContext): ChatHook {
  const { dom, ui } = ctx;
  const doc = document;

  function required<T extends HTMLElement>(el: Element | null, what: string): T {
    if (!el) throw new Error(`chat: ${what} must exist in index.html`);
    return el as T;
  }
  const button = required<HTMLButtonElement>(doc.getElementById('ctl-chat'), '#ctl-chat');
  const badge = required<HTMLElement>(doc.getElementById('ctl-chat-badge'), '#ctl-chat-badge');
  const body = required<HTMLElement>(dom.meetingScreen.querySelector('.meeting-body'), '.meeting-body');

  const aside = doc.createElement('aside');
  aside.id = 'chat-drawer';
  aside.className = 'chat-aside';
  aside.hidden = true;
  body.appendChild(aside);

  let room: Room | null = null;
  let historyTimers: ReturnType<typeof setTimeout>[] = [];
  let historyAnswered = false;

  function stopHistoryRequests(): void {
    for (const t of historyTimers) clearTimeout(t);
    historyTimers = [];
  }
  /** Ask peers what was said before we arrived; repeats on a short backoff until one answers. */
  function requestHistory(): void {
    stopHistoryRequests();
    historyAnswered = false;
    for (const delay of CHAT_HISTORY_REQUEST_DELAYS_MS) {
      historyTimers.push(
        setTimeout(() => {
          if (!historyAnswered && room?.state === 'connected') void publish({ v: 1, type: 'history-req' }).catch(() => {});
        }, delay),
      );
    }
  }

  const store = createChatStore(() => room?.localParticipant.identity ?? null);

  // Reactive mirror the mounted drawer reads through prop getters.
  let view = $state.raw<{ messages: readonly ChatMessage[]; canSend: boolean }>({ messages: [], canSend: false });
  function refreshView(): void {
    view = { messages: store.messages, canSend: room?.state === 'connected' };
  }

  let drawer: Record<string, unknown> | null = null;

  function nameFor(p: LkParticipant | undefined, identity: string): string | null {
    if (p) return displayNameForParticipant(p) || null;
    const known = room ? [room.localParticipant, ...room.remoteParticipants.values()].find((x) => x.identity === identity) : undefined;
    return known ? displayNameForParticipant(known) || null : null;
  }

  function self(): ChatSender {
    const lp = room?.localParticipant;
    return { identity: lp?.identity ?? '', name: lp ? displayNameForParticipant(lp) || null : null };
  }

  async function publish(wire: ChatWire, requester?: string): Promise<void> {
    if (!room || room.state !== 'connected') throw new Error('not connected');
    await room.localParticipant.publishData(encodeChatWire(wire), chatPublishOptions(wire, requester));
  }

  async function send(text: string): Promise<void> {
    const wire = chatMessageWire(text);
    store.sent(wire, self());
    try {
      await publish(wire);
    } catch (e) {
      ui.logEvent(`chat send failed: ${e instanceof Error ? e.message : String(e)}`, 'warn');
      ui.showToast("Couldn't send your message. Check your connection and try again.");
    }
  }

  function renderControl(): void {
    const unread = store.unread;
    const open = store.open;
    button.setAttribute('aria-pressed', open ? 'true' : 'false');
    button.setAttribute('aria-expanded', open ? 'true' : 'false');
    button.classList.toggle('on', open);
    button.setAttribute('aria-label', open ? 'Close chat' : unread > 0 ? `Open chat, ${unread} unread` : 'Open chat');
    badge.hidden = open || unread === 0;
    badge.textContent = unread > 99 ? '99+' : String(unread);
    aside.hidden = !open;
    dom.meetingScreen.classList.toggle('chat-open', open);
  }

  function renderDrawer(): void {
    refreshView();
    if (!store.open) {
      if (drawer) {
        void unmount(drawer);
        drawer = null;
      }
      return;
    }
    if (!drawer) {
      drawer = mount(ChatDrawer, {
        target: aside,
        props: {
          get messages() {
            return view.messages;
          },
          get canSend() {
            return view.canSend;
          },
          onSend: (text: string) => send(text),
          onClose: () => store.setOpen(false),
        },
      });
      // Opening the drawer is an explicit act: put the caret in the composer,
      // once it has rendered (mount does not run effects, so bind:this is not
      // set yet). Not on touch screens, where focus raises the soft keyboard
      // over the messages the reader opened chat to see (#246).
      if (!window.matchMedia?.('(pointer: coarse)').matches) {
        void tick().then(() => (drawer as { focusComposer?: () => void } | null)?.focusComposer?.());
      }
    }
  }

  store.onChange(() => {
    renderControl();
    renderDrawer();
  });
  button.addEventListener('click', () => store.setOpen(!store.open));
  renderControl();

  return {
    roomConnected(newRoom) {
      room = newRoom;
      requestHistory();
      refreshView();
    },
    roomDisconnected() {
      room = null;
      stopHistoryRequests();
      store.setOpen(false);
      store.clear();
      renderControl();
      renderDrawer();
    },
    onData(payload, participant, senderIdentity) {
      // No authenticated sender = nothing the drawer may attribute; drop.
      const identity = participant?.identity ?? senderIdentity;
      if (!identity) return;
      const wire = parseChatPayload(payload);
      if (!wire) return;
      const sender: ChatSender = { identity, name: nameFor(participant, identity) };
      switch (wire.type) {
        case 'msg': {
          const result = store.receive(wire, sender);
          if (result === 'added' && identity !== room?.localParticipant.identity && !store.open) {
            ui.showToast(chatNoticeText(sender, wire.text));
          }
          break;
        }
        case 'history-req': {
          const reply = store.historyReply();
          if (reply) void publish(reply, identity).catch(() => {});
          break;
        }
        case 'history':
          historyAnswered = true;
          stopHistoryRequests();
          store.mergeHistory(wire, sender);
          break;
      }
    },
    get open() {
      return store.open;
    },
    setOpen(open) {
      store.setOpen(open);
    },
    messages() {
      return store.messages;
    },
    send,
  };
}
