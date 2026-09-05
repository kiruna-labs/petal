// `petal.ai-chat` session wiring (#657): receive, publish, expire, and the
// push-to-talk safety net. The pure contract logic lives in aiChat.ts.
//
// This client never HOSTS a session — the Gemini session always runs on the
// sharer's machine — so it only ever publishes `startRequest`/`stopRequest`
// and `pttStart`/`pttEnd`, and only ever consumes `state`/`transcript`.
//
// A STUCK-OPEN PTT is the worst failure this module can produce: the host
// keeps streaming the room's microphone to a third-party API after the user
// believes they let go. Every path that can lose a pointerup therefore ends in
// `releaseAllPtt` — pointer cancel/leave/blur (aiChatPanel.ts), tab hide,
// pagehide, window blur, disconnect, and teardown.

import type { HarnessContext } from './context.ts';
import {
  aiChatPublishOptions,
  aiChatSessionKey,
  createAiChatSessions,
  encodeAiChatMessage,
  parseAiChatPayload,
  AI_CHAT_REJECTION_REASONS,
  type AiChatSessionState,
} from './aiChat.ts';
import { AI_CHAT_STATE_HEARTBEAT_MS, AI_CHAT_TOPIC, type AiChatMessage } from './trackNames.ts';

/** How often stale sessions are swept. Finer than the staleness window itself
 * so a crashed host's badge clears promptly rather than up to 5s late. */
export const AI_CHAT_EXPIRY_SWEEP_MS = 1_000;

export interface AiChatController {
  handlePayload: (payload: Uint8Array, senderIdentity?: string, topic?: string) => void;
  sessionFor: (windowId: number, ownerIdentity: string) => AiChatSessionState | null;
  requestStart: (windowId: number, ownerIdentity: string) => void;
  requestStop: (windowId: number, ownerIdentity: string) => void;
  pttStart: (windowId: number, ownerIdentity: string) => void;
  pttEnd: (windowId: number, ownerIdentity: string) => void;
  /** Type a message into the session. Unlike PTT, never touches the floor --
   * a typed message has no "who's speaking" ambiguity to arbitrate. */
  sendText: (windowId: number, ownerIdentity: string, text: string) => void;
  /** True while THIS client holds the floor for that window. */
  localPttHeld: (windowId: number, ownerIdentity: string) => boolean;
  /** Release every floor this client holds. Idempotent. */
  releaseAllPtt: (reason: string) => void;
  onChange: (listener: () => void) => () => void;
  ownerLeft: (ownerIdentity: string) => void;
  reset: () => void;
  destroy: () => void;
}

export function setupAiChat(ctx: HarnessContext): AiChatController {
  const sessions = createAiChatSessions();
  const listeners = new Set<() => void>();
  /** Session keys this client currently holds the PTT floor for. */
  const heldFloors = new Set<string>();
  let sweepTimer: ReturnType<typeof setInterval> | null = null;

  function notify() {
    for (const listener of Array.from(listeners)) {
      try {
        listener();
      } catch {
        // A broken UI listener must not stop the others, and must never stop
        // a PTT release from completing.
      }
    }
  }

  function publish(message: AiChatMessage): void {
    const room = ctx.state.room;
    if (!room) return;
    const options = aiChatPublishOptions();
    room.localParticipant
      .publishData(encodeAiChatMessage(message), options)
      .catch((err: unknown) => {
        ctx.ui.logEvent(
          `ai chat publish failed (${message.type}): ${(err as Error)?.message ?? err}`,
          'warn',
        );
      });
  }

  function handlePayload(payload: Uint8Array, senderIdentity?: string, topic?: string): void {
    if (topic !== AI_CHAT_TOPIC) return;
    const message = parseAiChatPayload(payload);
    if (!message) {
      ctx.ui.logEvent('ai chat: dropped malformed packet', 'warn');
      return;
    }
    // Sender identity comes from the authenticated LiveKit participant, never
    // from the payload -- the same invariant as telepointer and draw.
    const result = sessions.applyMessage(message, senderIdentity, Date.now());
    if (result.rejected) {
      ctx.ui.logEvent(
        `ai chat: dropped ${message.type} from ${senderIdentity ?? '(unknown)'} -- ${AI_CHAT_REJECTION_REASONS[result.rejected]}`,
        'warn',
      );
      return;
    }
    if (result.changed) notify();
  }

  function requestStart(windowId: number, ownerIdentity: string): void {
    publish({ v: 1, type: 'startRequest', windowId, ownerIdentity });
    ctx.ui.logEvent(`ai chat: requested start on window ${windowId}`);
  }

  function requestStop(windowId: number, ownerIdentity: string): void {
    // Stopping while still holding the floor would leave the host's mic tap
    // open on its side of the exchange. Let go first, always.
    pttEnd(windowId, ownerIdentity);
    publish({ v: 1, type: 'stopRequest', windowId, ownerIdentity });
    ctx.ui.logEvent(`ai chat: requested stop on window ${windowId}`);
  }

  function pttStart(windowId: number, ownerIdentity: string): void {
    const key = aiChatSessionKey(windowId, ownerIdentity);
    if (heldFloors.has(key)) return;
    heldFloors.add(key);
    publish({ v: 1, type: 'pttStart', windowId, ownerIdentity });
    notify();
  }

  function pttEnd(windowId: number, ownerIdentity: string): void {
    const key = aiChatSessionKey(windowId, ownerIdentity);
    if (!heldFloors.delete(key)) return;
    publish({ v: 1, type: 'pttEnd', windowId, ownerIdentity });
    notify();
  }

  function sendText(windowId: number, ownerIdentity: string, text: string): void {
    const trimmed = text.trim();
    if (!trimmed) return;
    publish({ v: 1, type: 'sendText', windowId, ownerIdentity, text: trimmed });
    ctx.ui.logEvent(`ai chat: sent a typed message on window ${windowId}`);
  }

  function releaseAllPtt(reason: string): void {
    if (heldFloors.size === 0) return;
    for (const key of Array.from(heldFloors)) {
      const session = sessions.entries().find(
        (candidate) => aiChatSessionKey(candidate.windowId, candidate.ownerIdentity) === key,
      );
      heldFloors.delete(key);
      // Fall back to parsing the key so a floor is still released for a
      // session that expired underneath us -- the host does not know that.
      const separator = key.lastIndexOf(':');
      const ownerIdentity = session?.ownerIdentity ?? key.slice(0, separator);
      const windowId = session?.windowId ?? Number(key.slice(separator + 1));
      if (!ownerIdentity || !Number.isSafeInteger(windowId)) continue;
      publish({ v: 1, type: 'pttEnd', windowId, ownerIdentity });
    }
    ctx.ui.logEvent(`ai chat: released push-to-talk (${reason})`, 'warn');
    notify();
  }

  function ownerLeft(ownerIdentity: string): void {
    const cleared = sessions.removeOwner(ownerIdentity);
    if (cleared.length === 0) return;
    for (const key of cleared) heldFloors.delete(key);
    ctx.ui.logEvent(`ai chat: cleared ${cleared.length} session(s) -- owner left`, 'warn');
    notify();
  }

  function sweep(): void {
    const cleared = sessions.expireStale(Date.now());
    if (cleared.length === 0) return;
    for (const key of cleared) heldFloors.delete(key);
    ctx.ui.logEvent(
      `ai chat: cleared ${cleared.length} session(s) -- no owner heartbeat for ${
        AI_CHAT_STATE_HEARTBEAT_MS / 1000
      }s x3`,
      'warn',
    );
    notify();
  }

  // --- Push-to-talk safety net ---------------------------------------------
  // A pointerup that never arrives (tab switch, OS window switch, page unload)
  // must not leave the floor held.
  const onVisibilityChange = () => {
    if (typeof document !== 'undefined' && document.visibilityState === 'hidden') {
      releaseAllPtt('page hidden');
    }
  };
  const onWindowBlur = () => releaseAllPtt('window blurred');
  const onPageHide = () => releaseAllPtt('page hidden');

  if (typeof document !== 'undefined' && typeof document.addEventListener === 'function') {
    document.addEventListener('visibilitychange', onVisibilityChange);
  }
  if (typeof globalThis.addEventListener === 'function') {
    globalThis.addEventListener('blur', onWindowBlur);
    globalThis.addEventListener('pagehide', onPageHide);
  }

  if (typeof setInterval === 'function') {
    sweepTimer = setInterval(sweep, AI_CHAT_EXPIRY_SWEEP_MS);
    // Same reason as remoteWindowHeader's freshness timer: a bare interval
    // keeps a Node test process alive forever.
    (sweepTimer as unknown as { unref?: () => void })?.unref?.();
  }

  return {
    handlePayload,
    sessionFor: (windowId, ownerIdentity) => sessions.get(windowId, ownerIdentity),
    requestStart,
    requestStop,
    pttStart,
    pttEnd,
    sendText,
    localPttHeld: (windowId, ownerIdentity) =>
      heldFloors.has(aiChatSessionKey(windowId, ownerIdentity)),
    releaseAllPtt,
    onChange(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    ownerLeft,
    reset() {
      releaseAllPtt('session reset');
      sessions.clear();
      heldFloors.clear();
      notify();
    },
    destroy() {
      releaseAllPtt('teardown');
      sessions.clear();
      heldFloors.clear();
      listeners.clear();
      if (sweepTimer !== null) clearInterval(sweepTimer);
      sweepTimer = null;
      if (typeof document !== 'undefined' && typeof document.removeEventListener === 'function') {
        document.removeEventListener('visibilitychange', onVisibilityChange);
      }
      if (typeof globalThis.removeEventListener === 'function') {
        globalThis.removeEventListener('blur', onWindowBlur);
        globalThis.removeEventListener('pagehide', onPageHide);
      }
    },
  };
}
