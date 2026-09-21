// Meeting chat: wire model, validation, and the in-memory store both clients
// render from. Chat is a HOST surface (plugins/README.md §2.7, decision 2 as
// amended 2026-09-14): the host owns the topic, stamps every sender from the
// authenticated LiveKit participant, and in I-7b exposes `petal.chat` to
// plugins on top of this model. Wire shapes and limits are pinned in
// contracts/petal-contracts.json (`topics.chat`, `chatMessages`, `chatLimits`,
// `chatDataEvent`) and mirrored by apps/desktop/src-tauri/src/chat.rs.

export const CHAT_TOPIC = 'petal.chat';
export const CHAT_WIRE_VERSION = 1;

/**
 * When a joiner asks peers for history, relative to the room reporting
 * `connected`. The first reliable publish right after connect can race the
 * publisher data channel's negotiation and never leave the client (seen in
 * the live smoke: the same request sent seconds later is answered at once),
 * so the request repeats until a `history` reply arrives. Peers dedupe by
 * message id, so a repeated request costs a few hundred bytes at most.
 */
export const CHAT_HISTORY_REQUEST_DELAYS_MS: readonly number[] = [0, 1500, 4000];

export const CHAT_LIMITS = {
  /** Characters after normalization; longer messages are refused before publish. */
  maxTextChars: 2000,
  /** Bytes of one data packet; a history reply is trimmed to fit. */
  maxPayloadBytes: 8192,
  /** Messages a peer relays to a late joiner. */
  historyMessages: 50,
  /** Inbound packets accepted per sender per second before dropping. */
  inboundPerSenderPerSecond: 10,
  /** Messages kept in memory per meeting; the oldest fall off. */
  retainedMessages: 500,
} as const;

const MESSAGE_ID_RE = /^[A-Za-z0-9_-]{8,64}$/;
// Everything C0 except \n (and \t, which normalizes to a space), plus C1 and the
// bidi override/isolate range: text a peer typed never rearranges what the
// drawer shows around it (same rule as plugin manifest names).
const FORBIDDEN_RE = /[\u0000-\u0008\u000B-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/;

export interface ChatMsgWire {
  v: 1;
  type: 'msg';
  id: string;
  text: string;
  /** Sender's clock, unix ms. Ordering hint only; the receiver's arrival order wins ties. */
  t: number;
}

export interface ChatHistoryReqWire {
  v: 1;
  type: 'history-req';
}

export interface ChatHistoryEntry {
  id: string;
  text: string;
  t: number;
  senderIdentity: string;
  senderName: string | null;
}

export interface ChatHistoryWire {
  v: 1;
  type: 'history';
  messages: ChatHistoryEntry[];
}

export type ChatWire = ChatMsgWire | ChatHistoryReqWire | ChatHistoryWire;

export interface ChatSender {
  identity: string;
  name: string | null;
}

export interface ChatMessage {
  id: string;
  text: string;
  t: number;
  sender: ChatSender;
  self: boolean;
  /** Arrived in a peer's history reply: the sender fields are that peer's claim, not an authenticated stamp. */
  relayed: boolean;
}

export function isChatMessageId(value: unknown): value is string {
  return typeof value === 'string' && MESSAGE_ID_RE.test(value);
}

export function newChatMessageId(): string {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  if (c?.randomUUID) return c.randomUUID().replace(/-/g, '');
  let out = '';
  for (let i = 0; i < 32; i++) out += Math.floor(Math.random() * 16).toString(16);
  return out;
}

/**
 * What a typed message becomes before it is sent: CRLF -> LF, tabs -> spaces,
 * trimmed, at most two consecutive line breaks. Null when nothing sendable
 * remains, when it is too long, or when it carries forbidden characters.
 */
export function normalizeChatText(input: string): string | null {
  if (typeof input !== 'string') return null;
  const text = input.replace(/\r\n?/g, '\n').replace(/\t/g, ' ').replace(/\n{3,}/g, '\n\n').trim();
  if (text.length === 0 || text.length > CHAT_LIMITS.maxTextChars) return null;
  if (FORBIDDEN_RE.test(text)) return null;
  return text;
}

function isValidTimestamp(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= 9_999_999_999_999;
}

function isValidText(value: unknown): value is string {
  return (
    typeof value === 'string' &&
    value.length > 0 &&
    value.length <= CHAT_LIMITS.maxTextChars &&
    value.trim().length > 0 &&
    !FORBIDDEN_RE.test(value)
  );
}

function parseHistoryEntry(value: unknown): ChatHistoryEntry | null {
  if (!value || typeof value !== 'object') return null;
  const e = value as Record<string, unknown>;
  if (!isChatMessageId(e.id) || !isValidText(e.text) || !isValidTimestamp(e.t)) return null;
  if (typeof e.senderIdentity !== 'string' || e.senderIdentity.length === 0 || e.senderIdentity.length > 256) return null;
  const senderName =
    e.senderName === null || e.senderName === undefined
      ? null
      : typeof e.senderName === 'string' && e.senderName.length <= 128
        ? e.senderName
        : null;
  return { id: e.id, text: e.text, t: e.t, senderIdentity: e.senderIdentity, senderName };
}

/** Strict: anything not exactly one of the three shapes is null (dropped, never partially trusted). */
export function parseChatPayload(payload: Uint8Array | string): ChatWire | null {
  let raw: unknown;
  try {
    const text = typeof payload === 'string' ? payload : new TextDecoder().decode(payload);
    if (text.length > CHAT_LIMITS.maxPayloadBytes) return null;
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!raw || typeof raw !== 'object' || (raw as { v?: unknown }).v !== CHAT_WIRE_VERSION) return null;
  const m = raw as Record<string, unknown>;
  switch (m.type) {
    case 'msg':
      if (!isChatMessageId(m.id) || !isValidText(m.text) || !isValidTimestamp(m.t)) return null;
      return { v: 1, type: 'msg', id: m.id, text: m.text, t: m.t };
    case 'history-req':
      return { v: 1, type: 'history-req' };
    case 'history': {
      if (!Array.isArray(m.messages) || m.messages.length > CHAT_LIMITS.historyMessages) return null;
      const messages: ChatHistoryEntry[] = [];
      for (const entry of m.messages) {
        const parsed = parseHistoryEntry(entry);
        if (!parsed) return null;
        messages.push(parsed);
      }
      return { v: 1, type: 'history', messages };
    }
    default:
      return null;
  }
}

export function encodeChatWire(wire: ChatWire): Uint8Array<ArrayBuffer> {
  return new TextEncoder().encode(JSON.stringify(wire));
}

/** Every chat packet is reliable; a history reply goes to the requester only. */
export function chatPublishOptions(
  wire: ChatWire,
  requester?: string,
): { topic: string; reliable: true; destinationIdentities?: string[] } {
  return wire.type === 'history' && requester
    ? { topic: CHAT_TOPIC, reliable: true, destinationIdentities: [requester] }
    : { topic: CHAT_TOPIC, reliable: true };
}

export function chatMessageWire(text: string, now = Date.now()): ChatMsgWire {
  return { v: 1, type: 'msg', id: newChatMessageId(), text, t: now };
}

/** One-line notice for the toast shown while the drawer is closed. */
export function chatNoticeText(sender: ChatSender, text: string, maxChars = 80): string {
  const name = sender.name?.trim() || 'Someone';
  const flat = text.replace(/\s+/g, ' ').trim();
  const room = Math.max(8, maxChars - name.length - 2);
  const body = flat.length > room ? `${flat.slice(0, room - 1).trimEnd()}…` : flat;
  return `${name}: ${body}`;
}

export function formatChatTime(t: number, now = Date.now(), locale?: string): string {
  const d = new Date(t);
  const sameDay = new Date(now).toDateString() === d.toDateString();
  return sameDay
    ? d.toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' })
    : d.toLocaleString(locale, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
}

export type ChatReceiveResult = 'added' | 'duplicate';

export interface ChatStore {
  readonly messages: readonly ChatMessage[];
  readonly unread: number;
  readonly open: boolean;
  /** An authenticated inbound `msg` (or one of our own echoed back). */
  receive(wire: ChatMsgWire, sender: ChatSender): ChatReceiveResult;
  /** What we send; recorded locally at once so the drawer never waits on the network. */
  sent(wire: ChatMsgWire, self: ChatSender): ChatMessage;
  /** The reply to a peer's `history-req`, or null when we have nothing to relay. */
  historyReply(): ChatHistoryWire | null;
  /** A peer's `history` reply; returns how many messages were new. */
  mergeHistory(wire: ChatHistoryWire, relayer: ChatSender): number;
  setOpen(open: boolean): void;
  clear(): void;
  onChange(cb: () => void): () => void;
}

export function createChatStore(selfIdentity: () => string | null): ChatStore {
  let messages: ChatMessage[] = [];
  let unread = 0;
  let open = false;
  const ids = new Set<string>();
  const listeners = new Set<() => void>();

  function notify(): void {
    for (const cb of listeners) cb();
  }

  function insert(message: ChatMessage): void {
    ids.add(message.id);
    // Sorted by the sender's clock, arrival order as the tiebreaker; a late
    // history merge lands where it belongs instead of at the bottom.
    let i = messages.length;
    while (i > 0 && messages[i - 1].t > message.t) i--;
    messages = [...messages.slice(0, i), message, ...messages.slice(i)];
    if (messages.length > CHAT_LIMITS.retainedMessages) {
      const dropped = messages.slice(0, messages.length - CHAT_LIMITS.retainedMessages);
      messages = messages.slice(dropped.length);
      for (const d of dropped) ids.delete(d.id);
    }
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
    receive(wire, sender) {
      if (ids.has(wire.id)) return 'duplicate';
      const self = sender.identity === selfIdentity();
      insert({ id: wire.id, text: wire.text, t: wire.t, sender, self, relayed: false });
      if (!self && !open) unread++;
      notify();
      return 'added';
    },
    sent(wire, self) {
      const message: ChatMessage = { id: wire.id, text: wire.text, t: wire.t, sender: self, self: true, relayed: false };
      if (!ids.has(wire.id)) {
        insert(message);
        notify();
      }
      return message;
    },
    historyReply() {
      if (messages.length === 0) return null;
      const recent = messages.slice(-CHAT_LIMITS.historyMessages);
      let entries: ChatHistoryEntry[] = recent.map((m) => ({
        id: m.id,
        text: m.text,
        t: m.t,
        senderIdentity: m.sender.identity,
        senderName: m.sender.name,
      }));
      // Trim oldest-first until the packet fits the byte cap.
      while (entries.length > 0 && encodeChatWire({ v: 1, type: 'history', messages: entries }).length > CHAT_LIMITS.maxPayloadBytes) {
        entries = entries.slice(1);
      }
      return entries.length > 0 ? { v: 1, type: 'history', messages: entries } : null;
    },
    mergeHistory(wire, relayer) {
      let added = 0;
      const me = selfIdentity();
      for (const e of wire.messages) {
        if (ids.has(e.id)) continue;
        // A relayed entry is that peer's claim about who said what. Entries
        // the relayer attributes to ITSELF are as good as authenticated.
        const relayed = e.senderIdentity !== relayer.identity;
        insert({
          id: e.id,
          text: e.text,
          t: e.t,
          sender: { identity: e.senderIdentity, name: e.senderName },
          self: e.senderIdentity === me,
          relayed,
        });
        added++;
      }
      // History is what happened before we arrived: it never counts as unread.
      if (added > 0) notify();
      return added;
    },
    setOpen(next) {
      if (open === next) return;
      open = next;
      if (open) unread = 0;
      notify();
    },
    clear() {
      messages = [];
      ids.clear();
      unread = 0;
      notify();
    },
    onChange(cb) {
      listeners.add(cb);
      return () => {
        listeners.delete(cb);
      };
    },
  };
}
