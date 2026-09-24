// Meeting chat: wire model, validation, and the in-memory store both clients
// render from. Chat is a HOST surface (plugins/README.md §2.7, decision 2 as
// amended 2026-09-14): the host owns the topic, stamps every sender from the
// authenticated LiveKit participant, and exposes `petal.chat` to plugins on
// top of this model (I-7b). Wire shapes and limits are pinned in
// contracts/petal-contracts.json (`topics.chat`, `chatMessages`, `chatLimits`,
// `chatDataEvent`) and mirrored by apps/desktop/src-tauri/src/chat.rs.
//
// Plugin posts (I-7b) are their own wire type, `post`, never a field on `msg`:
// a client that predates them drops the unknown type instead of showing a
// plugin's words as if the person had typed them. History relays them in a
// separate `history-posts` packet for the same reason.

import { CHAT_COMMAND_NAME_RE, PLUGIN_ID_RE, isPrintableDisplayText } from '../plugin-host/manifest.ts';

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
  /** Messages a peer relays to a late joiner (per history packet). */
  historyMessages: 50,
  /** Inbound packets accepted per sender per second before dropping. */
  inboundPerSenderPerSecond: 10,
  /** Messages kept in memory per meeting; the oldest fall off. */
  retainedMessages: 500,
  /** A plugin name carried in `via`; the same budget as a manifest name. */
  viaNameMaxChars: 24,
} as const;

/** Local, never on the wire: slash commands typed in the composer. */
export const CHAT_COMMAND_LIMITS = {
  /** Characters of the text after `/name`. */
  argsMaxChars: 500,
  /** How long a plugin may answer one invocation privately (`chat.respond`). */
  respondWindowMs: 60_000,
} as const;

const MESSAGE_ID_RE = /^[A-Za-z0-9_-]{8,64}$/;
// Everything C0 except \n (and \t, which normalizes to a space), plus C1 and the
// bidi override/isolate range: text a peer typed never rearranges what the
// drawer shows around it (same rule as plugin manifest names).
const FORBIDDEN_RE = /[\u0000-\u0008\u000B-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/;

/** Which plugin posted a message, as the POSTING client stamped it. */
export interface ChatVia {
  id: string;
  name: string;
}

export interface ChatMsgWire {
  v: 1;
  type: 'msg';
  id: string;
  text: string;
  /** Sender's clock, unix ms. Ordering hint only; the receiver's arrival order wins ties. */
  t: number;
}

/** A message a plugin posted on its user's behalf. Always carries `via`. */
export interface ChatPostWire {
  v: 1;
  type: 'post';
  id: string;
  text: string;
  t: number;
  via: ChatVia;
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

export interface ChatHistoryPostEntry extends ChatHistoryEntry {
  via: ChatVia;
}

export interface ChatHistoryWire {
  v: 1;
  type: 'history';
  messages: ChatHistoryEntry[];
}

export interface ChatHistoryPostsWire {
  v: 1;
  type: 'history-posts';
  messages: ChatHistoryPostEntry[];
}

export type ChatWire = ChatMsgWire | ChatPostWire | ChatHistoryReqWire | ChatHistoryWire | ChatHistoryPostsWire;

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
  /** Posted by a plugin on `sender`'s behalf; null for what a person typed. */
  via: ChatVia | null;
  /** A plugin's private answer to this user's own command: shown here only, never sent or relayed. */
  local: boolean;
}

/** One slash command the composer can run, as the host resolved it (host.ts `chatCommands`). */
export interface ChatCommandOption {
  name: string;
  usage: string;
  description: string;
  pluginId: string;
  pluginName: string;
  source: 'builtin' | 'registry' | 'dev';
}

export type ChatCommandResult = { ok: true } | { ok: false; message: string };

export function isChatMessageId(value: unknown): value is string {
  return typeof value === 'string' && MESSAGE_ID_RE.test(value);
}

export function isChatVia(value: unknown): value is ChatVia {
  if (!value || typeof value !== 'object') return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.id === 'string' &&
    v.id.length <= 64 &&
    PLUGIN_ID_RE.test(v.id) &&
    typeof v.name === 'string' &&
    v.name.trim().length > 0 &&
    v.name.length <= CHAT_LIMITS.viaNameMaxChars &&
    // The manifest-name rule: one line, no bidi controls, so a `via` sits
    // beside the sender's name exactly as written.
    isPrintableDisplayText(v.name)
  );
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

export type ChatInput =
  | { kind: 'empty' }
  | { kind: 'message'; text: string }
  | { kind: 'command'; name: string; args: string }
  /** Starts with `/` but the word after it cannot be a command name. */
  | { kind: 'invalid-command'; token: string }
  /** Too long, or carries control/bidi characters. */
  | { kind: 'invalid'; reason: string };

/**
 * How the composer reads what was typed. `/name args` is a command; a leading
 * `//` sends a message that starts with one `/` (the IRC convention), so a
 * path or a literal slash never becomes a command by accident, and an
 * unknown command is never sent to everyone as text.
 */
export function classifyChatInput(raw: string): ChatInput {
  const trimmed = raw.replace(/\r\n?/g, '\n').trim();
  if (trimmed.length === 0) return { kind: 'empty' };
  if (trimmed.startsWith('//')) {
    const text = normalizeChatText(trimmed.slice(1));
    return text ? { kind: 'message', text } : { kind: 'invalid', reason: 'Messages are up to 2000 characters of plain text.' };
  }
  if (trimmed.startsWith('/') && trimmed.length > 1 && !/\s/.test(trimmed[1]!)) {
    const match = /^\/(\S+)(?:\s+([\s\S]*))?$/.exec(trimmed)!;
    const token = match[1]!;
    if (!CHAT_COMMAND_NAME_RE.test(token)) return { kind: 'invalid-command', token };
    const args = (match[2] ?? '').replace(/\t/g, ' ').trim();
    if (args.length > CHAT_COMMAND_LIMITS.argsMaxChars) {
      return { kind: 'invalid', reason: `Command text is up to ${CHAT_COMMAND_LIMITS.argsMaxChars} characters.` };
    }
    if (FORBIDDEN_RE.test(args)) return { kind: 'invalid', reason: 'Commands take plain text only.' };
    return { kind: 'command', name: token, args };
  }
  const text = normalizeChatText(trimmed);
  return text ? { kind: 'message', text } : { kind: 'invalid', reason: 'Messages are up to 2000 characters of plain text.' };
}

/** Autocomplete: while the draft is `/` plus a partial name (no space yet), the commands it could become. */
export function matchingChatCommands(options: readonly ChatCommandOption[], draft: string): ChatCommandOption[] {
  const match = /^\/([a-z0-9-]*)$/.exec(draft.trimStart());
  if (!match) return [];
  const prefix = match[1]!;
  return options.filter((o) => o.name.startsWith(prefix)).sort((a, b) => a.name.localeCompare(b.name));
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

function parseHistoryPostEntry(value: unknown): ChatHistoryPostEntry | null {
  const base = parseHistoryEntry(value);
  const via = (value as { via?: unknown } | null)?.via;
  if (!base || !isChatVia(via)) return null;
  return { ...base, via: { id: via.id, name: via.name } };
}

function parseEntries<T>(value: unknown, parse: (v: unknown) => T | null): T[] | null {
  if (!Array.isArray(value) || value.length > CHAT_LIMITS.historyMessages) return null;
  const out: T[] = [];
  for (const entry of value) {
    const parsed = parse(entry);
    if (!parsed) return null;
    out.push(parsed);
  }
  return out;
}

/** Strict: anything not exactly one of the known shapes is null (dropped, never partially trusted). */
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
    case 'post':
      if (!isChatMessageId(m.id) || !isValidText(m.text) || !isValidTimestamp(m.t) || !isChatVia(m.via)) return null;
      return { v: 1, type: 'post', id: m.id, text: m.text, t: m.t, via: { id: m.via.id, name: m.via.name } };
    case 'history-req':
      return { v: 1, type: 'history-req' };
    case 'history': {
      const messages = parseEntries(m.messages, parseHistoryEntry);
      return messages ? { v: 1, type: 'history', messages } : null;
    }
    case 'history-posts': {
      const messages = parseEntries(m.messages, parseHistoryPostEntry);
      return messages ? { v: 1, type: 'history-posts', messages } : null;
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
  return (wire.type === 'history' || wire.type === 'history-posts') && requester
    ? { topic: CHAT_TOPIC, reliable: true, destinationIdentities: [requester] }
    : { topic: CHAT_TOPIC, reliable: true };
}

export function chatMessageWire(text: string, now = Date.now()): ChatMsgWire {
  return { v: 1, type: 'msg', id: newChatMessageId(), text, t: now };
}

export function chatPostWire(text: string, via: ChatVia, now = Date.now()): ChatPostWire {
  return { v: 1, type: 'post', id: newChatMessageId(), text, t: now, via: { id: via.id, name: via.name } };
}

function oneLineNotice(name: string, text: string, maxChars: number): string {
  const flat = text.replace(/\s+/g, ' ').trim();
  const room = Math.max(8, maxChars - name.length - 2);
  const body = flat.length > room ? `${flat.slice(0, room - 1).trimEnd()}…` : flat;
  return `${name}: ${body}`;
}

/** One-line notice for the toast shown while the drawer is closed: "Mira: hello", or "Timer (Mira): …" for a plugin post. */
export function chatNoticeText(sender: ChatSender, text: string, maxChars = 80, via: ChatVia | null = null): string {
  const person = sender.name?.trim() || 'Someone';
  return oneLineNotice(via ? `${via.name} (${person})` : person, text, maxChars);
}

/** The toast for a plugin's private answer to your own command while the drawer is closed: "Timer: …". */
export function chatPrivateNoticeText(via: ChatVia, text: string, maxChars = 80): string {
  return oneLineNotice(via.name, text, maxChars);
}

export function formatChatTime(t: number, now = Date.now(), locale?: string): string {
  const d = new Date(t);
  const sameDay = new Date(now).toDateString() === d.toDateString();
  return sameDay
    ? d.toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' })
    : d.toLocaleString(locale, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
}

export type ChatReceiveResult = 'added' | 'duplicate';

export interface ChatHistoryReply {
  history: ChatHistoryWire | null;
  posts: ChatHistoryPostsWire | null;
}

export interface ChatStore {
  readonly messages: readonly ChatMessage[];
  readonly unread: number;
  readonly open: boolean;
  /** An authenticated inbound `msg` or `post` (or one of our own echoed back). */
  receive(wire: ChatMsgWire | ChatPostWire, sender: ChatSender): ChatReceiveResult;
  /** What we send (typed, or posted by one of our plugins); recorded locally at once. */
  sent(wire: ChatMsgWire | ChatPostWire, self: ChatSender): ChatMessage;
  /** A plugin's private answer to one of our commands. Local only: never sent, relayed, or unread. */
  notice(text: string, via: ChatVia, self: ChatSender, now?: number): ChatMessage;
  /** The replies to a peer's `history-req`: typed messages and plugin posts in separate packets. */
  historyReply(): ChatHistoryReply;
  /** A peer's `history` or `history-posts` reply; returns how many messages were new. */
  mergeHistory(wire: ChatHistoryWire | ChatHistoryPostsWire, relayer: ChatSender): number;
  setOpen(open: boolean): void;
  clear(): void;
  onChange(cb: () => void): () => void;
}

function trimToPacket<W extends ChatHistoryWire | ChatHistoryPostsWire>(build: (entries: W['messages']) => W, entries: W['messages']): W | null {
  let kept = entries;
  // Oldest first, until the packet fits the byte cap.
  while (kept.length > 0 && encodeChatWire(build(kept)).length > CHAT_LIMITS.maxPayloadBytes) kept = kept.slice(1) as W['messages'];
  return kept.length > 0 ? build(kept) : null;
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
    while (i > 0 && messages[i - 1]!.t > message.t) i--;
    messages = [...messages.slice(0, i), message, ...messages.slice(i)];
    if (messages.length > CHAT_LIMITS.retainedMessages) {
      const dropped = messages.slice(0, messages.length - CHAT_LIMITS.retainedMessages);
      messages = messages.slice(dropped.length);
      for (const d of dropped) ids.delete(d.id);
    }
  }

  function entry(m: ChatMessage): ChatHistoryEntry {
    return { id: m.id, text: m.text, t: m.t, senderIdentity: m.sender.identity, senderName: m.sender.name };
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
      const via = wire.type === 'post' ? { id: wire.via.id, name: wire.via.name } : null;
      insert({ id: wire.id, text: wire.text, t: wire.t, sender, self, relayed: false, via, local: false });
      if (!self && !open) unread++;
      notify();
      return 'added';
    },
    sent(wire, self) {
      const via = wire.type === 'post' ? { id: wire.via.id, name: wire.via.name } : null;
      const message: ChatMessage = { id: wire.id, text: wire.text, t: wire.t, sender: self, self: true, relayed: false, via, local: false };
      if (!ids.has(wire.id)) {
        insert(message);
        notify();
      }
      return message;
    },
    notice(text, via, self, now = Date.now()) {
      const message: ChatMessage = {
        id: `local-${newChatMessageId()}`,
        text,
        t: now,
        sender: self,
        self: true,
        relayed: false,
        via: { id: via.id, name: via.name },
        local: true,
      };
      insert(message);
      notify();
      return message;
    },
    historyReply() {
      const shared = messages.filter((m) => !m.local);
      const typed = shared.filter((m) => !m.via).slice(-CHAT_LIMITS.historyMessages);
      const posted = shared.filter((m) => m.via).slice(-CHAT_LIMITS.historyMessages);
      return {
        history: trimToPacket<ChatHistoryWire>((e) => ({ v: 1, type: 'history', messages: e }), typed.map(entry)),
        posts: trimToPacket<ChatHistoryPostsWire>(
          (e) => ({ v: 1, type: 'history-posts', messages: e }),
          posted.map((m) => ({ ...entry(m), via: { id: m.via!.id, name: m.via!.name } })),
        ),
      };
    },
    mergeHistory(wire, relayer) {
      let added = 0;
      const me = selfIdentity();
      for (const e of wire.messages) {
        if (ids.has(e.id)) continue;
        // A relayed entry is that peer's claim about who said what. Entries
        // the relayer attributes to ITSELF are as good as authenticated.
        const via = 'via' in e ? { id: (e as ChatHistoryPostEntry).via.id, name: (e as ChatHistoryPostEntry).via.name } : null;
        insert({
          id: e.id,
          text: e.text,
          t: e.t,
          sender: { identity: e.senderIdentity, name: e.senderName },
          self: e.senderIdentity === me,
          relayed: e.senderIdentity !== relayer.identity,
          via,
          local: false,
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
