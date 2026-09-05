// AI chat (#656) — the pure frontend logic behind the three phase-1 surfaces
// (Settings section, hover-tab menu entry, in-meeting session panel). No Tauri
// dependency, so all of it is unit-testable without a webview.
//
// The reason -> copy table below MIRRORS Rust's `EndReason::user_message()`
// (src-tauri/src/ai_chat/state.rs). Rust owns the wording; this is the desktop
// renderer for the same closed token set, never a second vocabulary. If the
// Rust copy changes, change it here in the same commit — `aiChat.test.ts`
// reads state.rs and fails when the two drift.

import type { AiChatEndReason } from '$lib/ipc';
import { SPARKLE_GLYPH } from '@petal/shared/ui/icons';

/**
 * Verbatim mirror of `EndReason::user_message()`. Declared as a total
 * `Record` so a new token added to `AiChatEndReason` fails the typecheck here
 * rather than rendering as a blank toast.
 */
/** Compact label for the header warning chip — the full reason rides the
 * tooltip (`aiChatEndReasonMessage`), so the chip stays narrow on the top bar. */
export const AI_CHAT_UNAVAILABLE_LABEL = 'AI chat unavailable';

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
  error: 'AI chat stopped unexpectedly.'
};

/** User-facing sentence for a reason token. Never returns an empty string. */
export function aiChatEndReasonMessage(reason: AiChatEndReason): string {
  return AI_CHAT_END_REASON_MESSAGES[reason] ?? AI_CHAT_END_REASON_MESSAGES.error;
}

/**
 * Mirrors `EndReason::is_normal()`. A normal end is informational — the user
 * asked for it, or the session simply ran out of time — so it must not be
 * styled as a failure.
 */
export function isNormalAiChatEnd(reason: AiChatEndReason): boolean {
  return reason === 'stopped' || reason === 'time-limit';
}

/** Toast variant for an end/refusal: neutral for normal ends, amber otherwise. */
export function aiChatEndToastVariant(reason: AiChatEndReason): 'info' | 'degraded' {
  return isNormalAiChatEnd(reason) ? 'info' : 'degraded';
}

/** m:ss countdown. Clamps negatives/NaN to 0:00 so the label can never read "-1:59". */
export function formatAiChatCountdown(secondsLeft: number): string {
  const total = Number.isFinite(secondsLeft) ? Math.max(0, Math.floor(secondsLeft)) : 0;
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${minutes}:${String(seconds).padStart(2, '0')}`;
}

export type AiChatRole = 'user' | 'assistant';

/** One rendered bubble. `final` means the turn is closed and takes no more text. */
export interface AiChatTranscriptTurn {
  id: number;
  role: AiChatRole;
  text: string;
  final: boolean;
}

/** The shape `EVENTS.aiChatTranscript` delivers, minus the routing fields. */
export interface AiChatTranscriptDelta {
  role: AiChatRole;
  text: string;
  final: boolean;
}

/** Bubbles retained in the panel. Older turns fall off the top rather than growing forever. */
export const AI_CHAT_TRANSCRIPT_MAX_TURNS = 60;

/**
 * A typed turn's length cap. Mirrors `wire::MAX_USER_TEXT_CHARS` on the Rust
 * side (the real enforcement point, for both local and remote sends) — this
 * copy exists only so the input box can show a live count and disable Send
 * before round-tripping an obviously-too-long message.
 */
export const AI_CHAT_TEXT_MAX_CHARS = 600;

function nextTurnId(turns: readonly AiChatTranscriptTurn[]): number {
  return turns.length === 0 ? 1 : turns[turns.length - 1].id + 1;
}

function capped(turns: AiChatTranscriptTurn[], maxTurns: number): AiChatTranscriptTurn[] {
  return turns.length > maxTurns ? turns.slice(turns.length - maxTurns) : turns;
}

/**
 * Fold one transcript delta into the turn list.
 *
 * Coalescing rules — consecutive non-final deltas of the SAME role append to
 * the same bubble, and `final: true` closes it:
 * - a non-final delta extends the last turn when that turn is the same role and
 *   still open, otherwise it opens a new turn;
 * - a final delta closes the last open turn of that role (appending its text
 *   first, if any). `TurnComplete` sends `text: ''` with `final: true`, so an
 *   empty final must NEVER open a bubble — that would leave a permanent empty
 *   assistant bubble after every single reply;
 * - an empty non-final delta is a no-op.
 *
 * Pure: returns a new array and never mutates `turns`.
 */
export function appendTranscriptDelta(
  turns: readonly AiChatTranscriptTurn[],
  delta: AiChatTranscriptDelta,
  maxTurns: number = AI_CHAT_TRANSCRIPT_MAX_TURNS
): AiChatTranscriptTurn[] {
  const last = turns.length > 0 ? turns[turns.length - 1] : null;
  const canExtend = last !== null && last.role === delta.role && !last.final;

  if (canExtend) {
    const head = turns.slice(0, -1);
    const merged: AiChatTranscriptTurn = {
      ...(last as AiChatTranscriptTurn),
      text: (last as AiChatTranscriptTurn).text + delta.text,
      final: delta.final || (last as AiChatTranscriptTurn).final
    };
    return capped([...head, merged], maxTurns);
  }

  if (delta.text.length === 0) {
    // Nothing to show and no open turn of this role to close.
    return turns.slice();
  }

  const opened: AiChatTranscriptTurn = {
    id: nextTurnId(turns),
    role: delta.role,
    text: delta.text,
    final: delta.final
  };
  return capped([...turns, opened], maxTurns);
}

/**
 * Close every open turn. Called when push-to-talk starts: a new spoken turn
 * begins, so the previous user bubble must not keep absorbing text. Without
 * this, two PTT presses with no reply between them merge into one bubble.
 */
export function closeOpenTurns(
  turns: readonly AiChatTranscriptTurn[]
): AiChatTranscriptTurn[] {
  if (turns.every((turn) => turn.final)) return turns.slice();
  return turns.map((turn) => (turn.final ? turn : { ...turn, final: true }));
}

/** Status line for the panel header. */
export function aiChatStatusLabel(phase: 'connecting' | 'live' | 'ended'): string {
  if (phase === 'connecting') return 'Connecting…';
  return phase === 'live' ? 'Live' : 'Ended';
}

// ---- Hover-tab AI chat state (#736) -----------------------------------------

/**
 * Update active status of AI chat when an `EVENTS.aiChatState` event arrives (#736).
 *
 * Listens directly to session events so that sessions ending remotely (timeout,
 * remote stop, error) update the hover tab button state immediately without
 * leaving a stale "Stop AI chat" label.
 */
export function hoverTabAiChatNextActiveState(
  currentActive: boolean,
  event: Partial<import('$lib/ipc').AiChatStateEvent>,
  currentWindowId: number | null
): boolean {
  if (currentWindowId === null || event.windowId !== currentWindowId) {
    return currentActive;
  }
  if (!event.state) {
    return currentActive;
  }
  if (event.state.phase === 'connecting' || event.state.phase === 'live') {
    return true;
  }
  if (event.state.phase === 'ended') {
    return false;
  }
  return currentActive;
}

// ---- Hover-tab menu entry (#656 phase 1) ------------------------------------

export const AI_CHAT_MENU_ITEM_ID = 'ai-chat-start';
/**
 * #847: native Tauri `CheckMenuItem`s have no icon option (checked
 * `CheckMenuItemOptions` directly), so the sparkle icon this issue asks for
 * lands as a glyph in the label text here -- the SVG icon (`shared/ui/icons.ts`)
 * is used on every other, webview-rendered AI-chat affordance instead.
 */
export const AI_CHAT_MENU_ITEM_LABEL = `${SPARKLE_GLYPH} Start AI chat on this window`;
/**
 * The stop half of the toggle. The hover tab must be able to END a session, not
 * only begin one: it is reachable over any window at any time, while the
 * session panel is not reachable in pill mode. Stopping something that streams
 * your screen to a third party must never be the harder path to find.
 */
export const AI_CHAT_STOP_MENU_ITEM_LABEL = `${SPARKLE_GLYPH} Stop AI chat on this window`;

// ---- Remote-window header control -------------------------------------------
//
// The SAME master switch gates this control and the hover-tab entry: opting in
// means the feature exists on every shared window, yours and other people's.
// Off means it is absent everywhere, not greyed out.
//
// A session always runs on the machine that owns the window's pixels and
// accessibility tree, so a receiver never hosts one — this control only ever
// publishes a request on `petal.ai-chat` and lets the owner decide.

/** Compact label; the header is horizontally tight and never truncates. */
export const AI_CHAT_HEADER_LABEL = 'AI chat';
/** Label while a session is live. The stop path must be as findable as start. */
export const AI_CHAT_HEADER_STOP_LABEL = 'Stop AI chat';
/** Accessible name / tooltip for each half of the toggle. */
export const AI_CHAT_HEADER_START_ACTION = 'Start AI chat on this window';
export const AI_CHAT_HEADER_STOP_ACTION = 'Stop AI chat on this window';

/**
 * The persistent session badge. Verbatim mirror of the web client's
 * `AI_CHAT_ACTIVE_DISCLOSURE` (web-harness/src/aiChat.ts) — one sentence, both
 * facts, in the same order on both clients. `aiChat.test.ts` pins the parity.
 */
export const AI_CHAT_ACTIVE_DISCLOSURE =
  'AI chat is live. This window and room voice are sent to Google.';

/** Short form for the badge chip itself, when the full sentence has no room. */
export const AI_CHAT_ACTIVE_BADGE_LABEL = 'AI chat live';

/**
 * What a receiver's header needs to know to decide whether to offer the
 * control. Deliberately the raw inputs the surface webview already has, so the
 * rule can be tested without a webview.
 */
export interface AiChatHeaderVisibilityInput {
  /** The one master switch (`ai_chat_settings.enabled`). */
  settingEnabled: boolean;
  /**
   * True only for a window shared by a NATIVE Petal client. A browser peer has
   * no window pixels and no accessibility tree, so it cannot host a session at
   * all — offering the control there would be a button that can only fail.
   *
   * On the compositor surface this is the `remoteControl=1` query param
   * (`compositor.rs`'s `header_query_string`), which is set from the sharer's
   * `petalWindowScales` participant metadata. Only the native publisher writes
   * that key — `web-harness/src/trackNames.ts`'s `mergeSharedSourceMetadata`
   * writes `petalWindowKinds` and nothing else — so its absence is exactly
   * "this share came from a browser". For a native sharer it is merely late
   * (the header re-renders when the metadata lands), which fails safe: the
   * control appears a moment later rather than appearing and then refusing.
   */
  nativeSource: boolean;
  /** Guards the id the request would be addressed to. */
  windowId: number;
  /** Guards the owner the request would be addressed to. */
  ownerIdentity: string;
}

/**
 * Whether the AI chat control may appear on a remote window's header.
 *
 * Pure, and the single authority for both the button and its badge — a badge
 * that could outlive the button would be a disclosure the user cannot act on.
 */
export function aiChatHeaderControlVisible(input: AiChatHeaderVisibilityInput): boolean {
  return (
    input.settingEnabled &&
    input.nativeSource &&
    Number.isFinite(input.windowId) &&
    input.windowId > 0 &&
    input.ownerIdentity.trim().length > 0
  );
}

/** Button label for the current state. Always the full word — never an icon alone. */
export function aiChatHeaderLabel(active: boolean): string {
  return active ? AI_CHAT_HEADER_STOP_LABEL : AI_CHAT_HEADER_LABEL;
}

/** Accessible name for the current state. */
export function aiChatHeaderAction(active: boolean): string {
  return active ? AI_CHAT_HEADER_STOP_ACTION : AI_CHAT_HEADER_START_ACTION;
}

/**
 * Tooltip for the control. A refusal is surfaced on the button itself, because
 * that is where the user just clicked — but only while nothing is running, so a
 * stale error can never sit on top of a live session's own label.
 */
export function aiChatHeaderTitle(active: boolean, error: AiChatEndReason | null): string {
  if (!active && error) return aiChatEndReasonMessage(error);
  return aiChatHeaderAction(active);
}

/**
 * Whether the tooltip/label should read as a warning. A normal end (the user
 * stopped it, or it ran out of time) is informational and must not be styled
 * as a failure.
 */
export function aiChatHeaderWarning(active: boolean, error: AiChatEndReason | null): boolean {
  return !active && !!error && !isNormalAiChatEnd(error);
}

/**
 * Tooltip status for the hover tab. Only surfaces a string when an AI-chat
 * error is active.
 */
export function aiChatHoverTabOptionsTitle(error: AiChatEndReason | null): string | undefined {
  return error ? aiChatEndReasonMessage(error) : undefined;
}

// ---- Remote push-to-talk floor (#664) ---------------------------------------

/**
 * Whether the LOCAL participant currently holds the remote PTT floor, per the
 * session's own activeSpeaker echo -- there is no optimistic local floor
 * state (unlike the local panel's own PTT button), since the owner's `state`
 * broadcast is the only source of truth for who holds a REMOTE floor.
 */
export function aiChatLocalHoldsPttFloor(
  activeSpeaker: string | null,
  localIdentity: string | null
): boolean {
  return !!activeSpeaker && !!localIdentity && activeSpeaker === localIdentity;
}

/** Whether someone ELSE holds the floor, so the local PTT button must stay disabled. */
export function aiChatPttFloorTakenByOther(
  activeSpeaker: string | null,
  localIdentity: string | null
): boolean {
  return !!activeSpeaker && !aiChatLocalHoldsPttFloor(activeSpeaker, localIdentity);
}

/** Label for the remote PTT button across its three states. */
export function aiChatRemotePttLabel(
  pressed: boolean,
  floorTakenByOther: boolean,
  activeSpeaker: string | null
): string {
  if (pressed) return 'Listening — release to send';
  if (floorTakenByOther) return `${activeSpeaker} is talking`;
  return 'Hold to talk';
}

/**
 * Whether the remote PTT button is disabled. Deliberately does NOT disable
 * while `pressed` is true, even once `floorTakenByOther` reads true --
 * a `state` echo of our OWN grant can race an unresolved local identity
 * (which reads as "someone else has it"), and disabling a control mid-press
 * can swallow its pointerup in a webview that doesn't dispatch pointer
 * events to disabled elements, wedging the button and the mic open with no
 * self-heal. A genuinely lost floor race is handled by force-releasing the
 * press instead (see the RemoteWindowHeader reconciliation effect), not by
 * this attribute.
 */
export function aiChatRemotePttDisabled(
  active: boolean,
  floorTakenByOther: boolean,
  pressed: boolean
): boolean {
  return !active || (floorTakenByOther && !pressed);
}

// ---- Window-control approval card (#658 phase 3) ----------------------------

/**
 * Heading on the approval card. Names the actor and the target plainly — no
 * "would you like to allow…" softening, because the thing being asked for is
 * an AI typing and clicking inside a real application.
 */
export const AI_CHAT_CONTROL_HEADING = 'The AI wants to act on this window';

/**
 * The default answer, and the one offered first: it authorizes EXACTLY this
 * action. Petal never pre-selects the session-wide grant.
 */
export const AI_CHAT_CONTROL_ALLOW_ONCE_LABEL = 'Allow once';
/**
 * The escalation. Deliberately worded as covering everything that follows, and
 * placed on its own row below the per-action answer so it cannot be mistaken
 * for the default.
 */
export const AI_CHAT_CONTROL_ALLOW_SESSION_LABEL = 'Allow for this session';
export const AI_CHAT_CONTROL_REJECT_LABEL = 'Reject';
/** Shown after a refusal, since refusal is sticky for the rest of the session. */
export const AI_CHAT_CONTROL_REJECTED_NOTE =
  'Window control is off for the rest of this session.';
export const AI_CHAT_CONTROL_RESUME_LABEL = 'Allow the AI to ask again';
/** Persistent disclosure while Rust reports a session-wide grant. */
export const AI_CHAT_CONTROL_GRANTED_NOTE =
  'The AI has standing access to this window for this session.';
export const AI_CHAT_CONTROL_REVOKE_LABEL = 'Revoke access';
export const AI_CHAT_CONTROL_STALE_NOTE =
  'That control request is no longer current. Nothing changed.';
export const AI_CHAT_PTT_REFUSED_NOTE =
  'Talk could not start because the session ended or someone else has the floor.';
/** Send-failure surface for a typed message: the draft is kept for retry,
 * so the failure must not be silent (a Send that appears to do nothing). */
export const AI_CHAT_TEXT_SEND_FAILED_NOTE =
  'Your message could not be sent. It is still in the box — try again.';

/** Label for the row that shows what would actually happen. */
export type AiChatControlDetailRow = { label: string; value: string };

/**
 * The rows the card renders under its summary.
 *
 * Pure, and deliberately never abbreviates: whatever the model asked to type is
 * shown in full (the panel scrolls if it is long) and a resolved element is
 * named by role and title. A card that summarised "some text" would be asking
 * for consent to something the human cannot see.
 */
export function aiChatControlDetailRows(detail: {
  literalText?: string;
  element?: string;
}): AiChatControlDetailRow[] {
  const rows: AiChatControlDetailRow[] = [];
  if (typeof detail.literalText === 'string') {
    rows.push({ label: 'Text to type', value: detail.literalText });
  }
  if (typeof detail.element === 'string' && detail.element.length > 0) {
    rows.push({ label: 'Element', value: detail.element });
  }
  return rows;
}

// ---- Settings copy ----------------------------------------------------------

/**
 * Title of the one master switch. It gates the feature EVERYWHERE — the
 * hover-tab entry on your own shares and the header control on windows other
 * people share — so it must not be worded as if it only covered your own
 * windows ("Allow AI chat on my shared windows" did, and read as one-sided).
 */
export const AI_CHAT_CONSENT_TITLE = 'Turn on AI chat';

/**
 * Consequence 1: what you GET. One switch, every shared window, both
 * directions — so nobody has to discover the second half by finding a button
 * they did not expect on someone else's window.
 */
export const AI_CHAT_CONSENT_DESCRIPTION =
  'Adds an AI chat button to every shared window in your meetings — the ones you share and the ones other people share.';

/**
 * Consequence 2: what you GIVE UP. Deliberately plain and non-negotiable: it
 * names WHO can start a session, on WHOSE window, and WHAT leaves the machine,
 * in that order, with no hedging and no "may"/"could" softening. This is the
 * half a user would otherwise not expect, so it is rendered as its own line
 * with emphasis rather than trailing the sentence above.
 */
export const AI_CHAT_CONSENT_SHARED_WINDOW_WARNING =
  'It also lets anyone in your meetings start AI chat on a window you share. That window’s content and the room’s voice are sent to Google.';

/** Bring-your-own-key cost guidance. An estimate, and labelled as one. */
export const AI_CHAT_COST_NOTE = 'Costs roughly 2–4¢ per minute of AI chat.';

/** Data-use disclosure for free-tier keys. */
export const AI_CHAT_KEY_DATA_USE_NOTE =
  'Free-tier keys may allow Google to use content to improve their models.';

/** Where a user gets a key. */
export const AI_CHAT_API_KEY_URL = 'https://aistudio.google.com/apikey';
