// `petal.ai-chat` — the browser half of the AI-chat data-channel contract
// (#657). Pure and DOM-free so the security boundary and the staleness rules
// are unit-testable; `aiChatSession.ts` does the LiveKit/DOM wiring.
//
// The native counterpart is apps/desktop/src-tauri/src/ai_chat/wire.rs
// (+ state.rs for the reason -> copy table). Both sides assert against
// contracts/petal-contracts.json (`topics.aiChat`, `aiTracks`,
// `aiChatMessages`, `aiChatEndReasons`), which is what stops them drifting.
//
// The Gemini session always runs on the SHARER's machine — only they have the
// window's pixels and its accessibility tree. This client is therefore always
// a driver/observer: it asks, it holds the floor, it renders. It never hosts,
// so it never authors `state` or `transcript`.

import {
  AI_CHAT_STALE_AFTER_MS,
  AI_CHAT_TOPIC,
  AI_CHAT_VERSION,
  type AiChatEndReason,
  type AiChatMessage,
  type AiChatTranscriptRole,
} from './trackNames.ts';

const AI_CHAT_TYPES = new Set([
  'startRequest',
  'stopRequest',
  'state',
  'pttStart',
  'pttEnd',
  'transcript',
  'sendText',
]);

const AI_CHAT_END_REASONS = new Set<string>([
  'stopped',
  'time-limit',
  'disabled',
  'not-shared',
  'busy',
  'rate-limited',
  'hosted-unavailable',
  'offline',
  'mint-failed',
  'model-unavailable',
  'quota',
  'error',
]);

const MAX_WINDOW_ID = 0xffff_ffff;

/**
 * Verbatim mirror of Rust's `EndReason::user_message()`
 * (src-tauri/src/ai_chat/state.rs) and of the desktop renderer's copy table
 * (apps/desktop/src/lib/data/aiChat.ts). Rust owns the wording; this is the
 * web renderer for the same closed token set, never a second vocabulary.
 *
 * Declared as a total `Record` so a token added to `AiChatEndReason` fails the
 * typecheck here rather than rendering as an empty status line.
 */
export const AI_CHAT_END_REASON_MESSAGES: Record<AiChatEndReason, string> = {
  stopped: 'AI chat ended.',
  'time-limit': 'AI chat reached its time limit.',
  disabled: 'AI chat is turned off for this window.',
  'not-shared': 'That window is no longer being shared.',
  busy: 'An AI chat is already running for this window.',
  'rate-limited': 'Too many AI chat sessions just now. Try again shortly.',
  'hosted-unavailable': 'AI chat is temporarily unavailable.',
  offline: 'Could not reach the AI chat service.',
  'mint-failed': 'Could not start AI chat.',
  'model-unavailable': 'This AI model is unavailable — update Petal.',
  quota: 'The AI chat quota for this key is used up.',
  error: 'AI chat stopped unexpectedly.',
};

/** User-facing sentence for a reason token. Never returns an empty string. */
export function aiChatEndReasonMessage(reason: AiChatEndReason): string {
  return AI_CHAT_END_REASON_MESSAGES[reason] ?? AI_CHAT_END_REASON_MESSAGES.error;
}

/** Mirrors `EndReason::is_normal()` — an ordinary conclusion, not a failure. */
export function isNormalAiChatEnd(reason: AiChatEndReason): boolean {
  return reason === 'stopped' || reason === 'time-limit';
}

/**
 * The session-visibility line. It must be unmistakable that the shared window's
 * pixels AND the room's voice are going to a third-party API, so this names
 * both, in that order, with no hedging. Mirrors the desktop consent copy.
 */
export const AI_CHAT_ACTIVE_DISCLOSURE =
  'AI chat is live. This window and room voice are sent to Google.';

/** m:ss countdown. Clamps negatives/NaN so the label can never read "-1:59". */
export function formatAiChatCountdown(secondsLeft: number): string {
  const total = Number.isFinite(secondsLeft) ? Math.max(0, Math.floor(secondsLeft)) : 0;
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`;
}

// ---------------------------------------------------------------------------
// Wire encode / decode
// ---------------------------------------------------------------------------

/**
 * Reliable, always. Session state and transcript lines must not be dropped;
 * lossy is reserved for continuous streams like pointer moves.
 */
export function aiChatPublishOptions(): { reliable: boolean; topic: typeof AI_CHAT_TOPIC } {
  return { reliable: true, topic: AI_CHAT_TOPIC };
}

export function encodeAiChatMessage(message: AiChatMessage): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(message));
}

function validWindowId(value: unknown): value is number {
  return (
    typeof value === 'number' &&
    Number.isSafeInteger(value) &&
    value >= 1 &&
    value <= MAX_WINDOW_ID
  );
}

/**
 * Structural parse only. It answers "is this a well-formed v1 message", never
 * "may this sender send it" — that is `authorizeAiChatMessage`, and callers
 * must run both.
 */
export function parseAiChatPayload(payload: Uint8Array | string): AiChatMessage | null {
  let text: string;
  try {
    text = typeof payload === 'string' ? payload : new TextDecoder().decode(payload);
  } catch {
    return null;
  }

  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }

  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const candidate = raw as Record<string, unknown>;
  if (typeof candidate.type !== 'string' || !AI_CHAT_TYPES.has(candidate.type)) return null;
  if (!validWindowId(candidate.windowId)) return null;
  if (typeof candidate.ownerIdentity !== 'string') return null;
  const ownerIdentity = candidate.ownerIdentity.trim();
  if (!ownerIdentity) return null;
  // A wrong `v` is not a parse failure -- it is an authorization failure, so
  // it is carried through and rejected by authorizeAiChatMessage with a
  // reason the caller can log. Anything non-numeric is malformed, though.
  if (typeof candidate.v !== 'number' || !Number.isInteger(candidate.v)) return null;
  const v = candidate.v as 1;
  const base = { v, windowId: candidate.windowId, ownerIdentity };

  switch (candidate.type) {
    case 'startRequest':
    case 'stopRequest':
    case 'pttStart':
    case 'pttEnd':
      return { ...base, type: candidate.type };

    case 'state': {
      if (typeof candidate.active !== 'boolean') return null;
      const message: AiChatMessage = { ...base, type: 'state', active: candidate.active };
      if (candidate.startedBy !== undefined) {
        if (typeof candidate.startedBy !== 'string') return null;
        message.startedBy = candidate.startedBy;
      }
      if (candidate.secondsLeft !== undefined) {
        if (
          typeof candidate.secondsLeft !== 'number' ||
          !Number.isFinite(candidate.secondsLeft) ||
          candidate.secondsLeft < 0
        ) {
          return null;
        }
        message.secondsLeft = candidate.secondsLeft;
      }
      if (candidate.activeSpeaker !== undefined) {
        if (typeof candidate.activeSpeaker !== 'string') return null;
        message.activeSpeaker = candidate.activeSpeaker;
      }
      if (candidate.error !== undefined) {
        // An unknown token is dropped rather than rendered raw: the vocabulary
        // is closed, and a future peer's new reason must not leak prose into
        // the UI. The rest of the state is still usable.
        if (typeof candidate.error !== 'string') return null;
        if (AI_CHAT_END_REASONS.has(candidate.error)) {
          message.error = candidate.error as AiChatEndReason;
        }
      }
      return message;
    }

    case 'transcript': {
      if (candidate.role !== 'user' && candidate.role !== 'assistant') return null;
      if (typeof candidate.text !== 'string') return null;
      if (candidate.final !== undefined && typeof candidate.final !== 'boolean') return null;
      return {
        ...base,
        type: 'transcript',
        role: candidate.role as AiChatTranscriptRole,
        text: candidate.text,
        final: candidate.final === true,
      };
    }

    case 'sendText': {
      if (typeof candidate.text !== 'string') return null;
      return { ...base, type: 'sendText', text: candidate.text };
    }

    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Authorization — the whole security boundary of this topic
// ---------------------------------------------------------------------------

/** Mirrors Rust's `wire::Rejection`. */
export type AiChatRejection = 'unsupportedVersion' | 'notWindowOwner' | 'notSelf';

export const AI_CHAT_REJECTION_REASONS: Record<AiChatRejection, string> = {
  unsupportedVersion: 'unsupported petal.ai-chat version',
  notWindowOwner: 'only the window owner may author this message',
  notSelf: 'a participant may only speak for themselves',
};

/**
 * The authorization matrix, mirroring `ai_chat::wire::authorize` exactly.
 *
 * `senderIdentity` is the AUTHENTICATED LiveKit participant identity of the
 * packet's sender — never a value read out of the payload. That is what stops
 * forged *attribution*; the matrix below is what stops forged *authority*:
 *
 * | message                       | accepted from                    |
 * |-------------------------------|----------------------------------|
 * | `startRequest` / `stopRequest`| any current participant          |
 * | `state`, `transcript`         | the window owner ONLY            |
 * | `pttStart` / `pttEnd`         | the speaker themselves ONLY      |
 *
 * Returns `null` when the message is allowed, otherwise the rejection reason
 * (so the caller can log precisely rather than dropping silently).
 */
export function authorizeAiChatMessage(
  message: AiChatMessage,
  senderIdentity: string | null | undefined,
): AiChatRejection | null {
  if (message.v !== AI_CHAT_VERSION) return 'unsupportedVersion';
  const sender = typeof senderIdentity === 'string' ? senderIdentity : '';

  switch (message.type) {
    // Anyone in the room may ASK. The owner decides whether to act, and
    // enforces its own preconditions (feature enabled, window actually shared).
    case 'startRequest':
    case 'stopRequest':
      return null;

    // Session truth and transcript may only come from the host, or a peer
    // could fake a running session or put words in the assistant's mouth.
    case 'state':
    case 'transcript':
      return sender === message.ownerIdentity ? null : 'notWindowOwner';

    // You may only claim or release the floor for yourself. The sender IS the
    // speaker by construction — the host attributes the floor to the
    // authenticated sender, so there is nothing in the payload to disagree
    // with. Rejecting an empty/unauthenticated sender is kept explicit so a
    // future payload-carried speaker field cannot quietly become
    // authoritative.
    case 'pttStart':
    case 'pttEnd':
      return sender ? null : 'notSelf';

    // Anyone in the room may send a typed turn -- same "ask, owner acts"
    // shape as start/stop, not PTT's "only for yourself": there is no floor
    // to misattribute, the host attributes the turn to the authenticated
    // sender exactly like it does for PTT's speaker.
    case 'sendText':
      return null;

    default:
      return 'unsupportedVersion';
  }
}

// ---------------------------------------------------------------------------
// Transcript coalescing
// ---------------------------------------------------------------------------

/** One rendered bubble. `final` means the turn is closed and takes no more text. */
export interface AiChatTranscriptTurn {
  id: number;
  role: AiChatTranscriptRole;
  text: string;
  final: boolean;
}

export interface AiChatTranscriptDelta {
  role: AiChatTranscriptRole;
  text: string;
  final: boolean;
}

/** Bubbles retained per session. Older turns fall off rather than growing forever. */
export const AI_CHAT_TRANSCRIPT_MAX_TURNS = 60;

function nextTurnId(turns: readonly AiChatTranscriptTurn[]): number {
  return turns.length === 0 ? 1 : turns[turns.length - 1].id + 1;
}

function capped(turns: AiChatTranscriptTurn[], maxTurns: number): AiChatTranscriptTurn[] {
  return turns.length > maxTurns ? turns.slice(turns.length - maxTurns) : turns;
}

/**
 * Fold one transcript delta into the turn list. Mirrors the desktop renderer's
 * `appendTranscriptDelta` so both clients bubble a stream identically.
 *
 * - a non-final delta extends the last turn when that turn is the same role
 *   and still open, otherwise it opens a new turn;
 * - a final delta closes the last open turn of that role (appending its text
 *   first, if any). A turn-complete signal arrives as `text: ''` with
 *   `final: true`, so an empty final must NEVER open a bubble — that would
 *   leave a permanent empty assistant bubble after every reply;
 * - an empty non-final delta is a no-op.
 *
 * Pure: returns a new array and never mutates `turns`.
 */
export function appendAiChatTranscriptDelta(
  turns: readonly AiChatTranscriptTurn[],
  delta: AiChatTranscriptDelta,
  maxTurns: number = AI_CHAT_TRANSCRIPT_MAX_TURNS,
): AiChatTranscriptTurn[] {
  const last = turns.length > 0 ? turns[turns.length - 1] : null;
  if (last !== null && last.role === delta.role && !last.final) {
    const merged: AiChatTranscriptTurn = {
      ...last,
      text: last.text + delta.text,
      final: delta.final || last.final,
    };
    return capped([...turns.slice(0, -1), merged], maxTurns);
  }

  if (delta.text.length === 0) return turns.slice();

  return capped(
    [...turns, { id: nextTurnId(turns), role: delta.role, text: delta.text, final: delta.final }],
    maxTurns,
  );
}

/**
 * Close every open turn. Called when push-to-talk starts: a new spoken turn
 * begins, so the previous bubble must not keep absorbing text. Without this,
 * two PTT presses with no reply between them merge into one bubble.
 */
export function closeAiChatOpenTurns(
  turns: readonly AiChatTranscriptTurn[],
): AiChatTranscriptTurn[] {
  if (turns.every((turn) => turn.final)) return turns.slice();
  return turns.map((turn) => (turn.final ? turn : { ...turn, final: true }));
}

// ---------------------------------------------------------------------------
// Per-window session state
// ---------------------------------------------------------------------------

export interface AiChatSessionState {
  windowId: number;
  ownerIdentity: string;
  active: boolean;
  startedBy: string | null;
  secondsLeft: number | null;
  activeSpeaker: string | null;
  error: AiChatEndReason | null;
  /** When the owner's most recent `state` landed — the heartbeat clock. */
  lastStateAtMs: number;
  turns: AiChatTranscriptTurn[];
}

export function aiChatSessionKey(windowId: number, ownerIdentity: string): string {
  return `${ownerIdentity}:${windowId}`;
}

export interface AiChatApplyResult {
  /** Non-null when the message was dropped; the caller logs the reason. */
  rejected: AiChatRejection | null;
  /** Session key touched, when the message changed observable state. */
  key: string | null;
  changed: boolean;
}

export interface AiChatSessions {
  applyMessage(
    message: AiChatMessage,
    senderIdentity: string | null | undefined,
    nowMs: number,
  ): AiChatApplyResult;
  /**
   * Clear sessions whose owner has stopped heartbeating. Returns the keys
   * cleared so the caller can tear down UI for exactly those.
   */
  expireStale(nowMs: number): string[];
  /** Clear every session owned by a participant who left. */
  removeOwner(ownerIdentity: string): string[];
  get(windowId: number, ownerIdentity: string): AiChatSessionState | null;
  entries(): AiChatSessionState[];
  clear(): void;
}

/**
 * Receiver-side session store.
 *
 * Staleness is the load-bearing part: a host that crashes mid-session stops
 * heartbeating without ever sending `active: false`, and without expiry the
 * room would keep showing an "AI chat live" badge for a session that no longer
 * exists. Owner disconnect (`removeOwner`) and missed heartbeats
 * (`expireStale`) are the two ways that can happen, and both clear the entry
 * outright rather than merely dimming it.
 */
export function createAiChatSessions(
  options: { maxTurns?: number; staleAfterMs?: number } = {},
): AiChatSessions {
  const maxTurns = options.maxTurns ?? AI_CHAT_TRANSCRIPT_MAX_TURNS;
  const staleAfterMs = options.staleAfterMs ?? AI_CHAT_STALE_AFTER_MS;
  const sessions = new Map<string, AiChatSessionState>();

  function ensure(
    message: AiChatMessage,
    nowMs: number,
  ): { key: string; session: AiChatSessionState } {
    const key = aiChatSessionKey(message.windowId, message.ownerIdentity);
    let session = sessions.get(key);
    if (!session) {
      session = {
        windowId: message.windowId,
        ownerIdentity: message.ownerIdentity,
        active: false,
        startedBy: null,
        secondsLeft: null,
        activeSpeaker: null,
        error: null,
        lastStateAtMs: nowMs,
        turns: [],
      };
      sessions.set(key, session);
    }
    return { key, session };
  }

  return {
    applyMessage(message, senderIdentity, nowMs) {
      const rejected = authorizeAiChatMessage(message, senderIdentity);
      if (rejected) return { rejected, key: null, changed: false };

      switch (message.type) {
        case 'state': {
          const { key, session } = ensure(message, nowMs);
          session.active = message.active;
          session.startedBy = message.startedBy ?? null;
          session.secondsLeft = message.secondsLeft ?? null;
          // The owner is authoritative about the floor, so a `state` always
          // supersedes any optimistic local guess from a peer's pttStart.
          session.activeSpeaker = message.activeSpeaker ?? null;
          session.error = message.error ?? null;
          session.lastStateAtMs = nowMs;
          return { rejected: null, key, changed: true };
        }

        case 'transcript': {
          const { key, session } = ensure(message, nowMs);
          session.turns = appendAiChatTranscriptDelta(
            session.turns,
            { role: message.role, text: message.text, final: message.final },
            maxTurns,
          );
          return { rejected: null, key, changed: true };
        }

        case 'pttStart': {
          // Optimistic only, and only for a session we already know about: the
          // owner's next `state` (<=5s away) overwrites it. Showing the floor
          // holder a beat early is better than a 5s lag, but this must never
          // invent a session the owner has not announced.
          const key = aiChatSessionKey(message.windowId, message.ownerIdentity);
          const session = sessions.get(key);
          if (!session || !session.active) return { rejected: null, key: null, changed: false };
          const speaker = typeof senderIdentity === 'string' ? senderIdentity : '';
          if (!speaker || session.activeSpeaker === speaker) {
            return { rejected: null, key: null, changed: false };
          }
          session.activeSpeaker = speaker;
          // A new spoken turn begins; the previous bubble must stop absorbing.
          session.turns = closeAiChatOpenTurns(session.turns);
          return { rejected: null, key, changed: true };
        }

        case 'pttEnd': {
          const key = aiChatSessionKey(message.windowId, message.ownerIdentity);
          const session = sessions.get(key);
          const speaker = typeof senderIdentity === 'string' ? senderIdentity : '';
          if (!session || session.activeSpeaker !== speaker) {
            return { rejected: null, key: null, changed: false };
          }
          session.activeSpeaker = null;
          return { rejected: null, key, changed: true };
        }

        // Requests carry no receiver-visible state: only the owner acts on
        // them, and it answers with `state`.
        case 'startRequest':
        case 'stopRequest':
        default:
          return { rejected: null, key: null, changed: false };
      }
    },

    expireStale(nowMs) {
      const cleared: string[] = [];
      for (const [key, session] of sessions) {
        if (nowMs - session.lastStateAtMs <= staleAfterMs) continue;
        cleared.push(key);
        sessions.delete(key);
      }
      return cleared;
    },

    removeOwner(ownerIdentity) {
      const cleared: string[] = [];
      for (const [key, session] of sessions) {
        if (session.ownerIdentity !== ownerIdentity) continue;
        cleared.push(key);
        sessions.delete(key);
      }
      return cleared;
    },

    get(windowId, ownerIdentity) {
      return sessions.get(aiChatSessionKey(windowId, ownerIdentity)) ?? null;
    },

    entries() {
      return Array.from(sessions.values());
    },

    clear() {
      sessions.clear();
    },
  };
}
