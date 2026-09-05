import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

import {
  AI_CHAT_ACTIVE_BADGE_LABEL,
  AI_CHAT_ACTIVE_DISCLOSURE,
  AI_CHAT_API_KEY_URL,
  AI_CHAT_CONSENT_DESCRIPTION,
  AI_CHAT_CONSENT_SHARED_WINDOW_WARNING,
  AI_CHAT_CONSENT_TITLE,
  AI_CHAT_END_REASON_MESSAGES,
  AI_CHAT_HEADER_LABEL,
  AI_CHAT_HEADER_START_ACTION,
  AI_CHAT_HEADER_STOP_ACTION,
  AI_CHAT_HEADER_STOP_LABEL,
  AI_CHAT_MENU_ITEM_ID,
  AI_CHAT_MENU_ITEM_LABEL,
  AI_CHAT_STOP_MENU_ITEM_LABEL,
  AI_CHAT_CONTROL_ALLOW_ONCE_LABEL,
  AI_CHAT_CONTROL_ALLOW_SESSION_LABEL,
  AI_CHAT_CONTROL_REJECT_LABEL,
  aiChatControlDetailRows,
  aiChatEndReasonMessage,
  aiChatEndToastVariant,
  aiChatHeaderAction,
  aiChatHeaderControlVisible,
  aiChatHeaderLabel,
  aiChatHeaderTitle,
  aiChatHeaderWarning,
  aiChatHoverTabOptionsTitle,
  aiChatLocalHoldsPttFloor,
  aiChatPttFloorTakenByOther,
  aiChatRemotePttDisabled,
  aiChatRemotePttLabel,
  aiChatStatusLabel,
  appendTranscriptDelta,
  closeOpenTurns,
  formatAiChatCountdown,
  hoverTabAiChatNextActiveState,
  isNormalAiChatEnd,
  type AiChatHeaderVisibilityInput,
  type AiChatTranscriptTurn
} from '../src/lib/data/aiChat.ts';
import { buildHoverTabMenuEntries } from '../src/lib/data/hoverTabMenu.ts';
import { COMMANDS, EVENTS, type AiChatEndReason } from '../src/lib/ipc.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (relative: string) => readFileSync(resolve(__dirname, relative), 'utf8');

const stateRsSource = read('../src-tauri/src/ai_chat/state.rs');
const panelSource = read('../src/lib/components/AiChatPanel.svelte');
const settingsSource = read('../src/lib/components/Settings.svelte');
const hoverTabSource = read('../src/routes/hover-tab/+page.svelte');
const meetingSource = read('../src/routes/meeting/[room]/+page.svelte');
const remoteHeaderSource = read('../src/lib/components/RemoteWindowHeader.svelte');
const surfaceSource = read('../src/routes/compositor/surface/+page.svelte');
// #844: the transcript/typed-input UI moved out of RemoteWindowHeader.svelte's
// old in-webview popover (always covered by the video NSView) into this
// separate native overlay route.
const aiChatOverlaySource = read('../src/routes/compositor/ai-chat/+page.svelte');
const ipcSource = read('../src/lib/ipc.ts');
const hoverTabMenuSource = read('../src/lib/data/hoverTabMenu.ts');
const aiChatDataSource = read('../src/lib/data/aiChat.ts');
const webAiChatSource = read('../../../web-harness/src/aiChat.ts');

const ALL_REASONS: AiChatEndReason[] = [
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
  'error'
];

function kebab(variant: string): string {
  return variant.replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase();
}

/** The `user_message()` arms as Rust actually writes them, keyed by wire token. */
function rustUserMessages(): Map<string, string> {
  const body = stateRsSource.slice(
    stateRsSource.indexOf('pub fn user_message'),
    stateRsSource.indexOf('/// Lifecycle of one session')
  );
  const map = new Map<string, string>();
  for (const match of body.matchAll(/EndReason::(\w+) => "([^"]*)"/g)) {
    map.set(kebab(match[1]), match[2]);
  }
  return map;
}

// ---- reason -> copy parity with Rust ---------------------------------------

test('every end reason maps to the exact sentence Rust would render', () => {
  const rust = rustUserMessages();
  assert.equal(rust.size, ALL_REASONS.length, 'parsed a different arm count than the token set');
  for (const reason of ALL_REASONS) {
    assert.equal(
      aiChatEndReasonMessage(reason),
      rust.get(reason),
      `desktop copy for "${reason}" has drifted from EndReason::user_message()`
    );
  }
});

test('the copy table covers every token and none of it is blank', () => {
  for (const reason of ALL_REASONS) {
    const message = AI_CHAT_END_REASON_MESSAGES[reason];
    assert.ok(message && message.length > 0, `"${reason}" has no user copy`);
  }
  assert.equal(new Set(Object.values(AI_CHAT_END_REASON_MESSAGES)).size, ALL_REASONS.length);
});

test('normal ends match Rust is_normal(): stopped and time-limit only', () => {
  const isNormalBody = stateRsSource.slice(
    stateRsSource.indexOf('pub fn is_normal'),
    stateRsSource.indexOf('/// Short, user-facing sentence')
  );
  assert.match(isNormalBody, /EndReason::Stopped \| EndReason::TimeLimit/);
  for (const reason of ALL_REASONS) {
    const expected = reason === 'stopped' || reason === 'time-limit';
    assert.equal(isNormalAiChatEnd(reason), expected, `is_normal mismatch for "${reason}"`);
  }
});

test('normal ends toast as information, failures as degraded', () => {
  assert.equal(aiChatEndToastVariant('stopped'), 'info');
  assert.equal(aiChatEndToastVariant('time-limit'), 'info');
  assert.equal(aiChatEndToastVariant('quota'), 'degraded');
  assert.equal(aiChatEndToastVariant('not-shared'), 'degraded');
  assert.equal(aiChatEndToastVariant('error'), 'degraded');
});

// ---- countdown --------------------------------------------------------------

test('countdown renders m:ss and never goes negative', () => {
  assert.equal(formatAiChatCountdown(300), '5:00');
  assert.equal(formatAiChatCountdown(299), '4:59');
  assert.equal(formatAiChatCountdown(61), '1:01');
  assert.equal(formatAiChatCountdown(9), '0:09');
  assert.equal(formatAiChatCountdown(0), '0:00');
  assert.equal(formatAiChatCountdown(-7), '0:00');
  assert.equal(formatAiChatCountdown(Number.NaN), '0:00');
});

test('status label is the copy the panel shows', () => {
  assert.equal(aiChatStatusLabel('connecting'), 'Connecting…');
  assert.equal(aiChatStatusLabel('live'), 'Live');
  assert.equal(aiChatStatusLabel('ended'), 'Ended');
});

// ---- transcript coalescing --------------------------------------------------

function fold(
  deltas: ReadonlyArray<{ role: 'user' | 'assistant'; text: string; final: boolean }>,
  maxTurns?: number
): AiChatTranscriptTurn[] {
  let turns: AiChatTranscriptTurn[] = [];
  for (const delta of deltas) turns = appendTranscriptDelta(turns, delta, maxTurns);
  return turns;
}

test('consecutive non-final deltas of the same role land in one bubble', () => {
  const turns = fold([
    { role: 'assistant', text: 'The ', final: false },
    { role: 'assistant', text: 'build ', final: false },
    { role: 'assistant', text: 'failed.', final: false }
  ]);
  assert.equal(turns.length, 1);
  assert.equal(turns[0].text, 'The build failed.');
  assert.equal(turns[0].final, false);
});

test('a role change starts a new bubble', () => {
  const turns = fold([
    { role: 'user', text: "what's on screen?", final: false },
    { role: 'assistant', text: 'A test log.', final: false }
  ]);
  assert.deepEqual(
    turns.map((t) => [t.role, t.text]),
    [
      ['user', "what's on screen?"],
      ['assistant', 'A test log.']
    ]
  );
});

test('an EMPTY final closes the open bubble instead of opening a blank one', () => {
  // This is exactly what `ServerEvent::TurnComplete` emits:
  // emit_transcript(app, id, "assistant", "", true).
  const turns = fold([
    { role: 'assistant', text: 'Done.', final: false },
    { role: 'assistant', text: '', final: true }
  ]);
  assert.equal(turns.length, 1, 'TurnComplete must not append an empty bubble');
  assert.equal(turns[0].text, 'Done.');
  assert.equal(turns[0].final, true);
});

test('an empty final with nothing open is a no-op', () => {
  assert.deepEqual(fold([{ role: 'assistant', text: '', final: true }]), []);
});

test('an empty non-final delta changes nothing', () => {
  const turns = fold([
    { role: 'assistant', text: 'Hi', final: false },
    { role: 'assistant', text: '', final: false }
  ]);
  assert.equal(turns.length, 1);
  assert.equal(turns[0].text, 'Hi');
});

test('a closed bubble never absorbs later text', () => {
  const turns = fold([
    { role: 'assistant', text: 'First reply.', final: false },
    { role: 'assistant', text: '', final: true },
    { role: 'assistant', text: 'Second reply.', final: false }
  ]);
  assert.deepEqual(
    turns.map((t) => t.text),
    ['First reply.', 'Second reply.']
  );
});

test('a non-empty final both appends and closes', () => {
  const turns = fold([
    { role: 'user', text: 'open the ', final: false },
    { role: 'user', text: 'log', final: true }
  ]);
  assert.equal(turns.length, 1);
  assert.equal(turns[0].text, 'open the log');
  assert.equal(turns[0].final, true);
});

test('turn ids are unique and monotonic so keyed rendering cannot collide', () => {
  const turns = fold([
    { role: 'user', text: 'a', final: true },
    { role: 'assistant', text: 'b', final: true },
    { role: 'user', text: 'c', final: true }
  ]);
  assert.deepEqual(
    turns.map((t) => t.id),
    [1, 2, 3]
  );
});

test('the transcript is capped, dropping the oldest turns', () => {
  const deltas = Array.from({ length: 8 }, (_, i) => ({
    role: (i % 2 === 0 ? 'user' : 'assistant') as 'user' | 'assistant',
    text: `m${i}`,
    final: true
  }));
  const turns = fold(deltas, 3);
  assert.equal(turns.length, 3);
  assert.deepEqual(
    turns.map((t) => t.text),
    ['m5', 'm6', 'm7']
  );
});

test('appendTranscriptDelta never mutates its input', () => {
  const original: AiChatTranscriptTurn[] = [
    { id: 1, role: 'assistant', text: 'hello', final: false }
  ];
  const snapshot = JSON.parse(JSON.stringify(original));
  appendTranscriptDelta(original, { role: 'assistant', text: ' world', final: true });
  assert.deepEqual(original, snapshot);
});

test('closeOpenTurns ends every open bubble so two PTT presses do not merge', () => {
  const before: AiChatTranscriptTurn[] = [
    { id: 1, role: 'assistant', text: 'ok', final: true },
    { id: 2, role: 'user', text: 'first press', final: false }
  ];
  const after = closeOpenTurns(before);
  assert.ok(after.every((turn) => turn.final));
  assert.equal(before[1].final, false, 'closeOpenTurns must not mutate its input');

  const next = appendTranscriptDelta(after, { role: 'user', text: 'second press', final: false });
  assert.equal(next.length, 3);
  assert.equal(next[2].text, 'second press');
});

// ---- hover-tab button & state (#736) ----------------------------------------

test('the native options menu exposes AI chat only when enabled on a shared window', () => {
  const hidden = buildHoverTabMenuEntries('automatic', false, false, 'cursorPreserving', true, true, false);
  assert.equal(hidden.some((entry) => entry.kind === 'ai-chat'), false);

  const visible = buildHoverTabMenuEntries('automatic', true, false, 'cursorPreserving', true, true, false);
  const entry = visible.find((item) => item.kind === 'ai-chat');
  assert.deepEqual(entry, {
    kind: 'ai-chat',
    id: AI_CHAT_MENU_ITEM_ID,
    text: AI_CHAT_MENU_ITEM_LABEL,
    enabled: true,
    checked: false
  });
});

test('hover tab AI chat button state updates live on session events, including remote session end', () => {
  const windowId = 42;

  // Initial state idle (false) -> connecting event arrives for hovered window -> active (true)
  let active = false;
  active = hoverTabAiChatNextActiveState(
    active,
    { windowId, state: { phase: 'connecting' } },
    windowId
  );
  assert.equal(active, true);

  // Live phase maintains active state
  active = hoverTabAiChatNextActiveState(
    active,
    { windowId, state: { phase: 'live' } },
    windowId
  );
  assert.equal(active, true);

  // Session ends remotely (e.g. time-limit, remote stop, error) -> active reverts to false
  active = hoverTabAiChatNextActiveState(
    active,
    { windowId, state: { phase: 'ended', reason: 'time-limit' } },
    windowId
  );
  assert.equal(active, false);

  // Event for a DIFFERENT window does not disturb current window's active state
  active = true;
  active = hoverTabAiChatNextActiveState(
    active,
    { windowId: 999, state: { phase: 'ended', reason: 'stopped' } },
    windowId
  );
  assert.equal(active, true, 'event for another window must be ignored');

  // Countdown tick event (no `state` property) does not alter active state
  active = hoverTabAiChatNextActiveState(
    active,
    { windowId, secondsLeft: 240 },
    windowId
  );
  assert.equal(active, true);
});

test('the native options menu labels an active AI chat session as Stop', () => {
  const entries = buildHoverTabMenuEntries('automatic', true, false, 'cursorPreserving', true, true, true);
  assert.deepEqual(entries.find((entry) => entry.kind === 'ai-chat'), {
    kind: 'ai-chat',
    id: AI_CHAT_MENU_ITEM_ID,
    text: AI_CHAT_STOP_MENU_ITEM_LABEL,
    enabled: true,
    checked: true
  });
});

test('the hover tab starts a session and re-emits refusals for the main window', () => {
  assert.match(hoverTabSource, /COMMANDS\.aiChatStart/);
  assert.match(hoverTabSource, /COMMANDS\.aiChatSettings/);
  // A refusal must never be swallowed: both the structured `started: false`
  // path and a thrown invoke have to reach the user. #736 restructured this
  // to a positive-branch-first `if (outcome.started) {...} else {...}` so the
  // success path can also flip the button's own active-state tracking.
  assert.match(hoverTabSource, /if \(outcome\.started\)/);
  assert.match(hoverTabSource, /EVENTS\.aiChatRefused/);
  assert.match(hoverTabSource, /reason: outcome\.reason \?\? 'error'/);
});

test('the meeting route renders the refusal and end reasons as toasts', () => {
  assert.match(meetingSource, /EVENTS\.aiChatRefused/);
  assert.match(meetingSource, /aiChatEndReasonMessage/);
  assert.match(meetingSource, /aiChatEndToastVariant/);
  assert.match(meetingSource, /unlistenAiChatRefused\?\.\(\)/);
  assert.match(meetingSource, /aiChatToast\.dispose\(\)/);
});

// ---- push-to-talk cannot get stuck open -------------------------------------

test('push-to-talk ends on every release path, not just a clean pointerup', () => {
  for (const handler of [
    'onpointerdown={startPtt}',
    'onpointerup={endPtt}',
    'onpointerleave={endPtt}',
    'onpointercancel={endPtt}',
    'onlostpointercapture={endPtt}',
    'onblur={endPtt}'
  ]) {
    assert.ok(panelSource.includes(handler), `missing PTT handler: ${handler}`);
  }
  // Released outside the button / window deactivated / tab hidden.
  assert.match(panelSource, /window\.addEventListener\('pointerup', endPtt\)/);
  assert.match(panelSource, /window\.addEventListener\('pointercancel', endPtt\)/);
  assert.match(panelSource, /window\.addEventListener\('blur', endPtt\)/);
  assert.match(panelSource, /document\.addEventListener\('visibilitychange', endPttIfHidden\)/);
  // Unmount, and the terminal phase, must both close the mic too.
  assert.match(panelSource, /onDestroy\(\(\) => \{[\s\S]*?endPtt\(\);/);
  assert.match(panelSource, /case 'ended':\s*\n\s*endPtt\(\);/);
  // Idempotent, so the redundant paths cost one command call.
  assert.match(panelSource, /function endPtt\(\) \{\s*\n\s*if \(!pttActive\) return;/);
  assert.match(panelSource, /COMMANDS\.aiChatPttStart/);
  assert.match(panelSource, /COMMANDS\.aiChatPttEnd/);
});

test('the panel subscribes with the standard teardown helper and stops via aiChatStop', () => {
  assert.match(panelSource, /listenUntilDestroy<AiChatStateEvent>/);
  assert.match(panelSource, /listenUntilDestroy<AiChatTranscriptEvent>/);
  assert.match(panelSource, /COMMANDS\.aiChatStop/);
});

test('transcript text wraps rather than clipping (no truncation, ever)', () => {
  const turnRule = panelSource.slice(panelSource.indexOf('  .turn {'));
  assert.match(turnRule.slice(0, 400), /white-space: pre-wrap;/);
  assert.match(turnRule.slice(0, 400), /overflow-wrap: anywhere;/);
  assert.doesNotMatch(panelSource, /text-overflow:\s*ellipsis/);
  assert.doesNotMatch(panelSource, /white-space:\s*nowrap/);
});

// ---- settings ---------------------------------------------------------------

test('the settings section states BOTH consequences of opting in', () => {
  // Consequence 1: what you get -- the control on EVERY shared window, not
  // only your own. A user who reads only this line must not be surprised to
  // find the button on someone else's window.
  assert.equal(
    AI_CHAT_CONSENT_DESCRIPTION,
    'Adds an AI chat button to every shared window in your meetings — the ones you share and the ones other people share.'
  );
  // Consequence 2: what you give up. Names who, whose window, and what leaves
  // the machine, with no hedging.
  assert.equal(
    AI_CHAT_CONSENT_SHARED_WINDOW_WARNING,
    'It also lets anyone in your meetings start AI chat on a window you share. That window’s content and the room’s voice are sent to Google.'
  );
  for (const softener of [/\bmay\b/i, /\bcould\b/i, /\bmight\b/i, /\bhelp(s|ful)?\b/i]) {
    assert.doesNotMatch(AI_CHAT_CONSENT_SHARED_WINDOW_WARNING, softener);
  }
  assert.match(AI_CHAT_CONSENT_SHARED_WINDOW_WARNING, /Google/);
  assert.match(AI_CHAT_CONSENT_SHARED_WINDOW_WARNING, /voice/);

  // Both are rendered, and the title no longer claims the switch only covers
  // your own windows.
  assert.match(settingsSource, /AI_CHAT_CONSENT_DESCRIPTION/);
  assert.match(settingsSource, /AI_CHAT_CONSENT_SHARED_WINDOW_WARNING/);
  assert.match(settingsSource, /AI_CHAT_CONSENT_TITLE/);
  assert.doesNotMatch(settingsSource, /Allow AI chat on my shared windows/);
  assert.match(settingsSource, /<h2 class="section-title">AI chat<\/h2>/);
  assert.match(settingsSource, /COMMANDS\.aiChatSetEnabled/);
  assert.match(settingsSource, /COMMANDS\.aiChatSetApiKey/);
  assert.match(settingsSource, /COMMANDS\.aiChatSettings/);
});

test('the second consequence is not styled as fine print, and never clips', () => {
  // It is the half a user would not expect, so it is lifted off the muted
  // ramp -- and it wraps like every other description rather than ellipsizing.
  const warningRule = settingsSource.slice(settingsSource.indexOf('.consent-warning {'));
  assert.match(warningRule.slice(0, 200), /color: var\(--warning\)/);
  assert.doesNotMatch(warningRule.slice(0, 200), /text-overflow/);
  assert.doesNotMatch(warningRule.slice(0, 200), /white-space:\s*nowrap/);
});

test('the key is write-only: rendered from hasApiKey, never echoed back', () => {
  assert.match(settingsSource, /\{#if aiChat\.hasApiKey\}/);
  assert.match(settingsSource, /Key saved/);
  assert.match(settingsSource, /handleAiChatRemoveKey/);
  assert.match(settingsSource, /type="password"/);
  // The draft box binds to a local scratch value that is cleared on save; it is
  // never seeded from the settings object (which carries no key to seed from).
  assert.match(settingsSource, /bind:value=\{aiChatKeyDraft\}/);
  assert.doesNotMatch(settingsSource, /value=\{aiChat\.apiKey/);
});

test('the key field carries the cost and data-use notes and the key link', () => {
  assert.equal(AI_CHAT_API_KEY_URL, 'https://aistudio.google.com/apikey');
  assert.match(settingsSource, /AI_CHAT_COST_NOTE/);
  assert.match(settingsSource, /AI_CHAT_KEY_DATA_USE_NOTE/);
  assert.match(settingsSource, /openAiChatKeyPage/);
});

test('the master toggle defaults to OFF', () => {
  assert.match(settingsSource, /let aiChat = \$state<AiChatSettings>\(\{ enabled: false, hasApiKey: false \}\)/);
});

test('the native options menu label TOGGLES to Stop while a session is running', () => {
  const entries = buildHoverTabMenuEntries('automatic', true, false, 'cursorPreserving', true, true, true);
  assert.equal(entries.find((entry) => entry.kind === 'ai-chat')?.text, AI_CHAT_STOP_MENU_ITEM_LABEL);
});

// ---- remote-window header control -------------------------------------------
//
// One opt-in gates the whole feature: the hover-tab button on your own shares
// AND this control on windows other people share. These tests exist because
// both halves of that sentence are easy to regress independently.

function visibilityInput(
  overrides: Partial<AiChatHeaderVisibilityInput> = {}
): AiChatHeaderVisibilityInput {
  return {
    settingEnabled: true,
    nativeSource: true,
    windowId: 42,
    ownerIdentity: 'user_bob',
    ...overrides
  };
}

test('the master switch gates the remote-window control, not a second setting', () => {
  assert.equal(aiChatHeaderControlVisible(visibilityInput()), true);
  assert.equal(
    aiChatHeaderControlVisible(visibilityInput({ settingEnabled: false })),
    false,
    'opted out means the control is absent on other people’s windows too'
  );

  // ...and it is the SAME flag the hover-tab options action reads, not a parallel one.
  assert.equal(
    buildHoverTabMenuEntries('automatic', true, false, 'cursorPreserving', true, false, false).some(
      (entry) => entry.kind === 'ai-chat'
    ),
    false
  );
  assert.equal(
    buildHoverTabMenuEntries('automatic', true, false, 'cursorPreserving', true, true, false).some(
      (entry) => entry.kind === 'ai-chat'
    ),
    true
  );
  assert.match(hoverTabSource, /COMMANDS\.aiChatSettings/);
  assert.match(surfaceSource, /COMMANDS\.aiChatSettings/);
  // Exactly one enable command exists across the app -- if a second opt-in is
  // ever added, this is where it shows up first.
  assert.equal((ipcSource.match(/ai_chat_set_enabled/g) ?? []).length, 1);
  // The shared AI-chat master switch remains in aiChat.ts; hoverTabMenu.ts
  // only receives the resolved enabled/active state for menu construction.
  assert.match(aiChatDataSource, /aiChatHeaderControlVisible/);
});

test('the control is hidden for a window shared by a browser peer', () => {
  // A browser sharer has no window pixels and no accessibility tree, so it can
  // never host a session; the button could only ever fail there. `nativeSource`
  // comes from the sharer's `petalWindowScales` metadata, which only the native
  // publisher writes.
  assert.equal(aiChatHeaderControlVisible(visibilityInput({ nativeSource: false })), false);
  // Both conditions are required, so a browser share stays hidden even with the
  // setting on, and an opted-out user stays hidden even on a native share.
  assert.equal(
    aiChatHeaderControlVisible(visibilityInput({ nativeSource: false, settingEnabled: false })),
    false
  );
});

test('the control refuses to address a request it cannot route', () => {
  // A request is keyed by (windowId, ownerIdentity); window ids are unique only
  // per owner, so a missing owner would let it land on someone else's window.
  assert.equal(aiChatHeaderControlVisible(visibilityInput({ ownerIdentity: '' })), false);
  assert.equal(aiChatHeaderControlVisible(visibilityInput({ ownerIdentity: '   ' })), false);
  assert.equal(aiChatHeaderControlVisible(visibilityInput({ windowId: 0 })), false);
  assert.equal(aiChatHeaderControlVisible(visibilityInput({ windowId: -1 })), false);
  assert.equal(aiChatHeaderControlVisible(visibilityInput({ windowId: Number.NaN })), false);
});

test('the remote control TOGGLES: stop is never harder to reach than start', () => {
  assert.equal(aiChatHeaderLabel(false), AI_CHAT_HEADER_LABEL);
  assert.equal(aiChatHeaderLabel(true), AI_CHAT_HEADER_STOP_LABEL);
  assert.equal(aiChatHeaderAction(false), AI_CHAT_HEADER_START_ACTION);
  assert.equal(aiChatHeaderAction(true), AI_CHAT_HEADER_STOP_ACTION);

  // The click path publishes a request rather than hosting locally -- a
  // receiver never runs the session.
  assert.match(surfaceSource, /COMMANDS\.aiChatRequestStart/);
  assert.match(surfaceSource, /COMMANDS\.aiChatRequestStop/);
  assert.match(surfaceSource, /stopping \? COMMANDS\.aiChatRequestStop : COMMANDS\.aiChatRequestStart/);
  assert.doesNotMatch(surfaceSource, /COMMANDS\.aiChatStart\b/);
  // Both directions carry the owner, never the window id alone.
  assert.match(surfaceSource, /windowId,\n\s+ownerIdentity\n\s+\}\);/);
});

test('a refusal shows on the button, but never on top of a live session', () => {
  assert.equal(aiChatHeaderTitle(false, 'quota'), aiChatEndReasonMessage('quota'));
  assert.equal(aiChatHeaderTitle(true, 'quota'), AI_CHAT_HEADER_STOP_ACTION);
  assert.equal(aiChatHeaderTitle(false, null), AI_CHAT_HEADER_START_ACTION);

  // A normal end is informational; only a real failure reads as a warning.
  assert.equal(aiChatHeaderWarning(false, 'stopped'), false);
  assert.equal(aiChatHeaderWarning(false, 'time-limit'), false);
  assert.equal(aiChatHeaderWarning(false, 'busy'), true);
  assert.equal(aiChatHeaderWarning(true, 'error'), false);
});

test('hover tab options title exposes an active AI chat error', () => {
  assert.equal(aiChatHoverTabOptionsTitle(null), undefined);
  assert.equal(aiChatHoverTabOptionsTitle('quota'), aiChatEndReasonMessage('quota'));
});

test('the live badge says the same thing on desktop and web, verbatim', () => {
  const web = webAiChatSource.match(
    /AI_CHAT_ACTIVE_DISCLOSURE =\s*\n?\s*'([^']*)'/
  )?.[1];
  assert.ok(web, 'could not read the web client’s disclosure sentence');
  assert.equal(AI_CHAT_ACTIVE_DISCLOSURE, web);
  assert.match(AI_CHAT_ACTIVE_DISCLOSURE, /Google/);
});

test('the badge is disclosed for the whole session, at every width', () => {
  // The header is the only surface this webview owns (the decoded video NSView
  // covers everything below the 44px strip), so a live session must pin the
  // header open -- an idle-collapse would hide the disclosure.
  assert.match(remoteHeaderSource, /aiChatDisclosureHeld/);
  assert.match(
    remoteHeaderSource,
    /if \(focused \|\| !autoHide \|\| aiChatDisclosureHeld\) return;/
  );
  assert.match(remoteHeaderSource, /if \(focused \|\| aiChatDisclosureHeld\) \{/);

  // No media query may hide the badge -- unlike .debug-btn/.open-url-btn, which
  // legitimately drop out when the window gets narrow.
  const styles = remoteHeaderSource.slice(remoteHeaderSource.indexOf('<style>'));
  for (const query of styles.split('@media').slice(1)) {
    assert.doesNotMatch(
      query,
      /\.ai-chat-badge\s*(,[^{]*)?\{[^}]*display:\s*none/,
      'a breakpoint hides the AI chat disclosure badge'
    );
  }

  // Two complete labels, one per width band -- never a clipped version of the
  // other, and neither ellipsizes.
  assert.match(remoteHeaderSource, /AI_CHAT_ACTIVE_DISCLOSURE\}<\/span>/);
  assert.match(remoteHeaderSource, /AI_CHAT_ACTIVE_BADGE_LABEL\}<\/span>/);
  assert.notEqual(AI_CHAT_ACTIVE_BADGE_LABEL, AI_CHAT_ACTIVE_DISCLOSURE);
  const badgeStart = styles.indexOf('.ai-chat-badge {');
  const badgeRule = styles.slice(badgeStart, styles.indexOf('}', badgeStart));
  assert.doesNotMatch(badgeRule, /text-overflow/);
  // It must not shrink either -- a chip that shrinks is a chip that clips.
  assert.match(badgeRule, /flex: 0 0 auto/);
  assert.match(badgeRule, /white-space: nowrap/);
});

test('the full disclosure only renders at a width it was MEASURED to fit', () => {
  // Headless-Chromium measurement (real font and sizes): the full sentence is
  // a 408px chip and the rest of a live-session header is 474px (76px fixed
  // chrome + the 394.9px right cluster -- #675 removed the Collapse button,
  // which dropped the cluster from 502px), so it needs roughly 1007px before
  // the 120px title floor is eaten (down from 1114px pre-#675). The
  // breakpoint carries margin over that. A copy change here without a
  // re-measure is the regression this guards -- the sentence must not
  // silently clip the title.
  const styles = remoteHeaderSource.slice(remoteHeaderSource.indexOf('<style>'));
  const swap = styles.match(/@(?:media|container) \(max-width: (\d+)px\) \{\s*\.ai-chat-badge-full \{\s*display: none;/);   // header ladder is container-query based since #918
  assert.ok(swap, 'the badge must swap to its short label at a measured width');
  assert.ok(
    Number(swap[1]) >= 1007,
    `full-sentence badge kept until ${swap[1]}px, but it needs 1007px to fit`
  );
  // 62 characters was what 408px was measured from; a materially longer
  // sentence would need a new measurement, not just a new breakpoint.
  assert.ok(
    AI_CHAT_ACTIVE_DISCLOSURE.length <= 70,
    'the disclosure grew past the measured width -- re-measure the breakpoint'
  );
  // The short label has to survive the 300px MIN_RESIZE_CONTENT_WIDTH floor.
  assert.ok(AI_CHAT_ACTIVE_BADGE_LABEL.length <= 16);
});

test('the badge takes its room from Debug and Open URL, never from the title', () => {
  // Measured at 620px: with every control still present, adding the badge left
  // the window title exactly 0px wide — the user could no longer see which
  // window, or whose, they were looking at. Two secondary controls yield for
  // the duration of the session instead, and come back when it ends.
  assert.match(remoteHeaderSource, /class:ai-chat-live=\{aiChatDisclosureHeld\}/);
  assert.match(
    remoteHeaderSource,
    /\.header\.ai-chat-live \.debug-btn,\s*\n\s*\.header\.ai-chat-live \.open-url-btn \{\s*\n\s*display: none;/
  );
  // The title itself keeps its own rules; nothing here may shrink it further.
  assert.doesNotMatch(remoteHeaderSource, /\.header\.ai-chat-live \.title/);
});

test('the header button stops mousedown so the native drag cannot eat its click', () => {
  // `compositor_start_drag` begins a native window-drag on mousedown, which
  // swallows the following mouseup/click. Every header button guards against it.
  const button = remoteHeaderSource.slice(
    remoteHeaderSource.indexOf('class="header-btn ai-chat-btn"')
  );
  assert.match(button.slice(0, 600), /onmousedown=\{stopMouseDown\}/);
  assert.match(button.slice(0, 600), /aria-pressed=\{aiChatActive\}/);
});

test('the receiver commands and event exist in Rust with the names ipc.ts uses', () => {
  // ipc.ts is the authoritative registry, so a rename on either side has to
  // fail here rather than as a dead button at runtime.
  const commandsRs = read('../src-tauri/src/ai_chat/commands.rs');
  const topicRs = read('../src-tauri/src/ai_chat/topic.rs');
  const libRs = read('../src-tauri/src/lib.rs');

  for (const command of ['ai_chat_request_start', 'ai_chat_request_stop', 'ai_chat_remote_session']) {
    assert.match(ipcSource, new RegExp(`'${command}'`), `ipc.ts is missing ${command}`);
    assert.match(commandsRs, new RegExp(`pub fn ${command}\\(`), `Rust is missing ${command}`);
    assert.match(libRs, new RegExp(`ai_chat::commands::${command}`), `${command} is not registered`);
  }
  assert.match(topicRs, /EVENT_REMOTE_STATE: &str = "ai-chat-remote-state"/);
  assert.match(ipcSource, /aiChatRemoteState: 'ai-chat-remote-state'/);

  // Both request commands are addressed by (window_id, owner_identity) —
  // Tauri camel-cases these, which is what the frontend passes.
  assert.match(commandsRs, /pub fn ai_chat_request_start\(\s*app: AppHandle,\s*window_id: u32,\s*owner_identity: String/);
  assert.match(commandsRs, /pub fn ai_chat_remote_session\(window_id: u32, owner_identity: String\)/);
});

test('ask and listen are ONE shape, in Rust and in ipc.ts', () => {
  // A narrowed command return silently drops a refusal that landed before the
  // surface mounted (an earlier revision did exactly that), so the command
  // must hand back `topic::RemoteState` itself rather than a reshaped copy.
  const commandsRs = read('../src-tauri/src/ai_chat/commands.rs');
  const topicRs = read('../src-tauri/src/ai_chat/topic.rs');
  assert.match(
    commandsRs,
    /pub fn ai_chat_remote_session\(window_id: u32, owner_identity: String\) -> Option<super::topic::RemoteState>/
  );

  const state = topicRs.slice(
    topicRs.indexOf('pub struct RemoteState {'),
    topicRs.indexOf('impl RemoteState')
  );
  const rustFields = ['window_id', 'owner_identity', 'active', 'started_by', 'seconds_left', 'active_speaker', 'error'];
  for (const field of rustFields) {
    assert.match(state, new RegExp(`pub ${field}:`), `RemoteState lost ${field}`);
  }
  assert.match(
    topicRs,
    /#\[serde\(rename_all = "camelCase"\)\]\s*\npub struct RemoteState \{/,
    'RemoteState must serialize camelCase'
  );
  // Rust renames to camelCase, so every field has to exist on the TS type.
  const tsBlock = ipcSource.slice(
    ipcSource.indexOf('export interface AiChatRemoteSessionState {'),
    ipcSource.indexOf('/** `EVENTS.aiChatRefused` payload')
  );
  for (const field of rustFields) {
    const camel = field.replace(/_([a-z])/g, (_, c) => c.toUpperCase());
    assert.match(tsBlock, new RegExp(`\\b${camel}\\??:`), `ipc.ts is missing ${camel}`);
  }

  // Both halves are declared with the same type -- that is the invariant.
  assert.match(ipcSource, /\[COMMANDS\.aiChatRemoteSession\]: AiChatRemoteSessionState \| null;/);
  assert.match(ipcSource, /\[EVENTS\.aiChatRemoteState\]: AiChatRemoteSessionState;/);
});

test('the control asks for current state on mount, not only on the next event', () => {
  // The surface webview is re-navigated whenever the sharer's metadata changes
  // (compositor.rs `refresh_header_webview`), so it can mount into an already
  // running session. Waiting for the next event would leave it undisclosed.
  assert.match(surfaceSource, /COMMANDS\.aiChatRemoteSession/);
  assert.match(surfaceSource, /EVENTS\.aiChatRemoteState/);
  assert.match(surfaceSource, /void refreshAiChatSession\(\);/);
  // Events for another window or another owner must be ignored.
  assert.match(surfaceSource, /event\.payload\.windowId !== windowId/);
  assert.match(surfaceSource, /event\.payload\.ownerIdentity !== ownerIdentity/);
  // ...and the listener is torn down with the panel.
  assert.match(surfaceSource, /unlistenAiChatRemoteState\?\.\(\)/);
});

test('a settings read that fails leaves the feature OFF', () => {
  // An unknown setting is not consent.
  assert.match(surfaceSource, /catch \{[^}]*aiChatEnabled = false;/);
});

test('the narrow-window overflow menu carries the action with its full label', () => {
  // Below 470px the button is replaced by the labelled native popup (#497), so
  // the stop path survives; the menu ENTRY is absent, not disabled, when the
  // switch is off.
  assert.match(remoteHeaderSource, /@(?:media|container) \(max-width: 470px\)[\s\S]{0,400}\.ai-chat-btn \{\s*display: none;/);
  assert.match(surfaceSource, /id: 'remote-window-ai-chat'/);
  assert.match(surfaceSource, /ensureModeMenu\(aiChatVisible\)/);
  assert.match(surfaceSource, /menuHasAiChat === withAiChat/);
  assert.match(surfaceSource, /aiChatModeItem\.setText\(aiChatHeaderAction\(aiChatActive\)\)/);
});

// ---- window control approval card (#658 phase 3) ---------------------------

const controlGateSource = read('../src-tauri/src/ai_chat/control_gate.rs');

test('the card shows exactly what the model asked for, never a summary of it', () => {
  // Consent to "type some text" is not consent. The literal string and the
  // resolved element are what the human is agreeing to, so both are rendered.
  const typed = aiChatControlDetailRows({ literalText: 'wire $500 to 1234' });
  assert.deepEqual(typed, [{ label: 'Text to type', value: 'wire $500 to 1234' }]);

  const clicked = aiChatControlDetailRows({ element: 'AXButton \u201cSend\u201d' });
  assert.deepEqual(clicked, [{ label: 'Element', value: 'AXButton \u201cSend\u201d' }]);

  // Empty text is still shown as a row — "type nothing" is a real, visible
  // answer, unlike a missing field.
  assert.deepEqual(aiChatControlDetailRows({ literalText: '' }), [
    { label: 'Text to type', value: '' }
  ]);
  assert.deepEqual(aiChatControlDetailRows({}), []);
});

test('per-action approval is the default and the session grant is the escalation', () => {
  // The one-shot answer is the primary button and passes sessionScope: false;
  // the session-wide grant is a separate, secondary control.
  assert.match(panelSource, /answerControl\(false\)/);
  assert.match(panelSource, /answerControl\(true\)/);
  assert.match(panelSource, /class="control-allow"[\s\S]{0,400}AI_CHAT_CONTROL_ALLOW_ONCE_LABEL/);
  assert.match(panelSource, /class="control-session"[\s\S]{0,400}AI_CHAT_CONTROL_ALLOW_SESSION_LABEL/);
  // Ordering matters: the per-action answer must come first in the DOM.
  assert.ok(
    panelSource.indexOf('AI_CHAT_CONTROL_ALLOW_ONCE_LABEL}\n') <
      panelSource.indexOf('AI_CHAT_CONTROL_ALLOW_SESSION_LABEL}\n'),
    'allow-once must be offered before the session-wide grant'
  );
  assert.equal(AI_CHAT_CONTROL_ALLOW_ONCE_LABEL, 'Allow once');
  assert.equal(AI_CHAT_CONTROL_ALLOW_SESSION_LABEL, 'Allow for this session');
  assert.equal(AI_CHAT_CONTROL_REJECT_LABEL, 'Reject');
});

test('every answer names BOTH the session and the request it answers', () => {
  // Without the pair, a click on a card the model had already replaced would
  // authorize whatever replaced it.
  assert.match(panelSource, /sessionId: request\.sessionId/);
  assert.match(panelSource, /requestId: request\.requestId/);
  // ... and Rust enforces it rather than trusting the UI to.
  assert.match(controlGateSource, /if session_id != state\.session_id/);
  assert.match(controlGateSource, /pending\.request_id == request_id/);
});

test('the card never auto-dismisses and never outlives its session', () => {
  // Only an explicit resolution for THIS request clears it...
  assert.match(
    panelSource,
    /controlRequest\.requestId === event\.payload\.requestId/
  );
  // ...and there is no timer that could make it disappear on its own.
  assert.doesNotMatch(panelSource, /setTimeout\([^)]*controlRequest/);
  // A finished session drops it, because answering it later could only
  // authorize something that no longer exists.
  assert.match(panelSource, /case 'ended':[\s\S]{0,600}controlRequest = null/);
});

test('a refusal is visibly sticky and its way back is a deliberate click', () => {
  assert.match(panelSource, /controlRefusedSessionId/);
  assert.match(panelSource, /AI_CHAT_CONTROL_REJECTED_NOTE/);
  assert.match(panelSource, /aiChatControlResume/);
});

test('approval-card text wraps rather than clipping (no truncation, ever)', () => {
  // The literal text can be 2000 characters. It scrolls; it is never cut off.
  const card = panelSource.slice(panelSource.indexOf('.control-card {'));
  assert.match(card, /\.control-value \{[\s\S]*?white-space: pre-wrap/);
  assert.match(card, /\.control-value \{[\s\S]*?overflow-y: auto/);
  assert.match(card, /min-height: 26px/);
  assert.doesNotMatch(card, /text-overflow: ellipsis/);
  assert.doesNotMatch(card, /white-space: nowrap/);
});

// ---- remote push-to-talk + transcript relay (#664) --------------------------

test('remote PTT floor logic: who holds it, who is locked out, and the button label in every state', () => {
  assert.equal(aiChatLocalHoldsPttFloor(null, 'alice'), false);
  assert.equal(aiChatLocalHoldsPttFloor('alice', null), false);
  assert.equal(aiChatLocalHoldsPttFloor('alice', 'alice'), true);
  assert.equal(aiChatLocalHoldsPttFloor('bob', 'alice'), false);

  assert.equal(aiChatPttFloorTakenByOther(null, 'alice'), false);
  assert.equal(aiChatPttFloorTakenByOther('alice', 'alice'), false);
  assert.equal(aiChatPttFloorTakenByOther('bob', 'alice'), true);
  // A not-yet-loaded local identity must not read as "nobody has it" -- the
  // floor genuinely is held by that other identity, so it must still lock
  // the button rather than let a slow roster read open a false window.
  assert.equal(aiChatPttFloorTakenByOther('bob', null), true);

  assert.equal(aiChatRemotePttLabel(true, false, 'alice'), 'Listening — release to send');
  assert.equal(aiChatRemotePttLabel(false, true, 'bob'), 'bob is talking');
  assert.equal(aiChatRemotePttLabel(false, false, null), 'Hold to talk');
  // Pressed always wins the label, even if floorTakenByOther is also true --
  // that combination shouldn't occur (the button is disabled first), but the
  // label must never claim someone else is talking while WE are holding it.
  assert.equal(aiChatRemotePttLabel(true, true, 'bob'), 'Listening — release to send');
});

test('the remote PTT button never disables MID-PRESS, even once the floor reads as taken by someone else', () => {
  // A Fable review of #664 found the earlier `!active || floorTakenByOther`
  // condition could disable the button while the user's pointer was still
  // down -- e.g. a `state` echo of the user's OWN grant racing an
  // unresolved local identity, which aiChatPttFloorTakenByOther treats as
  // "someone else has it". WKWebView does not dispatch pointer events to a
  // control that goes disabled mid-gesture, so the pointerup (and the
  // pttEnd it fires) would never arrive -- a floor wedged open with no
  // self-heal. The button must stay enabled through a still-held press;
  // the reconciliation effect (tested separately) is what actually ends a
  // genuinely lost race.
  assert.equal(aiChatRemotePttDisabled(true, false, false), false);
  assert.equal(aiChatRemotePttDisabled(true, true, false), true);
  // The exact regression: floor reads taken, but WE are the one pressing.
  assert.equal(aiChatRemotePttDisabled(true, true, true), false);
  assert.equal(aiChatRemotePttDisabled(false, false, false), true);
  assert.equal(aiChatRemotePttDisabled(false, false, true), true);
});

test('remote PTT commands and the remote transcript event are part of the IPC registry', () => {
  assert.equal(COMMANDS.aiChatRequestPttStart, 'ai_chat_request_ptt_start');
  assert.equal(COMMANDS.aiChatRequestPttEnd, 'ai_chat_request_ptt_end');
  assert.equal(EVENTS.aiChatRemoteTranscript, 'ai-chat-remote-transcript');
});

test('the remote PTT button shares the same derivations as the pure floor logic', () => {
  assert.match(
    remoteHeaderSource,
    /aiChatFloorTakenByOther = \$derived\(\s*aiChatPttFloorTakenByOther\(aiChatActiveSpeaker, localIdentity\)/
  );
  assert.match(
    remoteHeaderSource,
    /aiChatPttLabel = \$derived\(\s*aiChatRemotePttLabel\(aiChatPttPressed, aiChatFloorTakenByOther, aiChatActiveSpeaker\)/
  );
  assert.match(
    remoteHeaderSource,
    /aiChatPttDisabled = \$derived\(\s*aiChatRemotePttDisabled\(aiChatActive, aiChatFloorTakenByOther, aiChatPttPressed\)/
  );
  assert.match(remoteHeaderSource, /disabled=\{aiChatPttDisabled\}/);
});

// A Fable review of #664 demonstrated four mutations that pass every regex
// above -- e.g. keep every handler ATTACHED but drop the actual invoke/publish
// call inside it -- because a loose "does this token appear anywhere in the
// file" check can't see WHICH function a line lives in. These tests slice out
// each function body precisely so a swap or a dropped call inside it fails.
function functionBody(source: string, name: string, nextName: string): string {
  const start = source.indexOf(`function ${name}(`);
  const end = source.indexOf(`function ${nextName}(`, start);
  assert.ok(start >= 0 && end > start, `could not locate ${name}() before ${nextName}()`);
  return source.slice(start, end);
}

test('the press/release lifecycle actually starts and stops the floor and its global guards, not just wires the button', () => {
  const startBody = functionBody(remoteHeaderSource, 'startAiChatPtt', 'endAiChatPtt');
  assert.match(startBody, /if \(aiChatPttPressed \|\| aiChatFloorTakenByOther \|\| !aiChatActive\) return;/);
  assert.match(startBody, /aiChatPttPressed = true;/);
  assert.match(startBody, /addGlobalPttGuards\(\);/);
  assert.match(startBody, /onPttStart\?\.\(\);/);
  assert.doesNotMatch(startBody, /onPttEnd\?\.\(\);/);

  const endBody = functionBody(remoteHeaderSource, 'endAiChatPtt', 'endAiChatPttIfHidden');
  assert.match(endBody, /if \(!aiChatPttPressed\) return;/);
  assert.match(endBody, /aiChatPttPressed = false;/);
  assert.match(endBody, /removeGlobalPttGuards\(\);/);
  assert.match(endBody, /onPttEnd\?\.\(\);/);
  assert.doesNotMatch(endBody, /onPttStart\?\.\(\);/);
});

test('global PTT guards cover every escape hatch a compositor retire can hit, and are torn down on release', () => {
  // Element-local handlers cover a pointerup landing outside the button, but
  // a compositor RETIRE hides the panel's NSPanel without destroying the
  // webview -- the button's own handlers never fire, only visibilitychange.
  const addBody = functionBody(remoteHeaderSource, 'addGlobalPttGuards', 'removeGlobalPttGuards');
  assert.match(addBody, /window\.addEventListener\('pointerup', endAiChatPtt\)/);
  assert.match(addBody, /window\.addEventListener\('pointercancel', endAiChatPtt\)/);
  assert.match(addBody, /window\.addEventListener\('blur', endAiChatPtt\)/);
  assert.match(addBody, /document\.addEventListener\('visibilitychange', endAiChatPttIfHidden\)/);

  const removeBody = remoteHeaderSource.slice(
    remoteHeaderSource.indexOf('function removeGlobalPttGuards('),
    remoteHeaderSource.indexOf('// Keep the newest turn in view')
  );
  assert.match(removeBody, /window\.removeEventListener\('pointerup', endAiChatPtt\)/);
  assert.match(removeBody, /window\.removeEventListener\('pointercancel', endAiChatPtt\)/);
  assert.match(removeBody, /window\.removeEventListener\('blur', endAiChatPtt\)/);
  assert.match(removeBody, /document\.removeEventListener\('visibilitychange', endAiChatPttIfHidden\)/);

  // Still present as element-local handlers too (belt-and-braces).
  assert.match(remoteHeaderSource, /onpointerdown=\{startAiChatPtt\}/);
  assert.match(remoteHeaderSource, /onpointerup=\{endAiChatPtt\}/);
  assert.match(remoteHeaderSource, /onpointerleave=\{endAiChatPtt\}/);
  assert.match(remoteHeaderSource, /onpointercancel=\{endAiChatPtt\}/);
  assert.match(remoteHeaderSource, /onblur=\{endAiChatPtt\}/);
});

test('a lost floor race force-releases a held press instead of leaving it wedged behind a disabled button', () => {
  assert.match(
    remoteHeaderSource,
    /\$effect\(\(\) => \{\s*if \(aiChatPttPressed && aiChatFloorTakenByOther\) endAiChatPtt\(\);\s*\}\);/
  );
});

test('a dead session force-releases a held floor too', () => {
  assert.match(
    remoteHeaderSource,
    /if \(aiChatActive\) return;[\s\S]{0,320}if \(aiChatPttPressed\) endAiChatPtt\(\);/
  );
});

test('#844: RemoteWindowHeader no longer renders a transcript -- that moved to the ai-chat overlay route', () => {
  // Pins the migration itself: a regression that re-adds the old in-webview
  // transcript markup (always covered by the video NSView -- #844's whole
  // premise) must fail here, not just pass silently because nothing checks.
  assert.doesNotMatch(remoteHeaderSource, /transcriptTurns/);
  assert.doesNotMatch(remoteHeaderSource, /aiChatTranscriptEl/);
  assert.doesNotMatch(remoteHeaderSource, /ai-chat-remote-panel/);
});

test('the ai-chat overlay renders every turn with a role label, auto-scrolls to the newest, and never truncates the text', () => {
  assert.match(aiChatOverlaySource, /\{#each turns as turn \(turn\.id\)\}/);
  assert.match(aiChatOverlaySource, /turn\.role === 'assistant' \? 'AI' : 'You & room'/);
  assert.match(aiChatOverlaySource, /bind:this=\{transcriptEl\}/);
  assert.match(
    aiChatOverlaySource,
    /void turns\.length;\s*const el = transcriptEl;\s*if \(el\) el\.scrollTop = el\.scrollHeight;/
  );
  const styles = aiChatOverlaySource.slice(aiChatOverlaySource.indexOf('.turn-text {'));
  assert.match(styles, /\.turn-text \{[\s\S]*?white-space: pre-wrap/);
  assert.match(styles, /\.turn-text \{[\s\S]*?overflow-wrap: anywhere/);
});

function surfaceFunctionBody(name: string, nextName: string): string {
  const start = surfaceSource.indexOf(`function ${name}(`);
  const end = surfaceSource.indexOf(`function ${nextName}(`, start);
  assert.ok(start >= 0 && end > start, `could not locate ${name}() before ${nextName}()`);
  return surfaceSource.slice(start, end);
}

test('remote PTT invokes fire the RIGHT command in each direction, guarded like every other remote command', () => {
  // Scoped to each function body precisely: a swap (start invoking the end
  // command, or vice versa) would still match a file-wide "COMMANDS.aiChat
  // RequestPttStart appears somewhere" check but fails here.
  const startBody = surfaceFunctionBody('onAiChatPttStart', 'onAiChatPttEnd');
  assert.match(startBody, /if \(!Number\.isFinite\(windowId\) \|\| windowId <= 0 \|\| !ownerIdentity\) return;/);
  assert.match(startBody, /invoke\(COMMANDS\.aiChatRequestPttStart, \{ windowId, ownerIdentity \}\)/);
  assert.doesNotMatch(startBody, /COMMANDS\.aiChatRequestPttEnd/);

  const endBody = surfaceFunctionBody('onAiChatPttEnd', 'onToggleAiChat');
  assert.match(endBody, /if \(!Number\.isFinite\(windowId\) \|\| windowId <= 0 \|\| !ownerIdentity\) return;/);
  assert.match(endBody, /invoke\(COMMANDS\.aiChatRequestPttEnd, \{ windowId, ownerIdentity \}\)/);
  assert.doesNotMatch(endBody, /COMMANDS\.aiChatRequestPttStart/);
});

test('surface page no longer listens for the remote transcript -- the ai-chat overlay owns it directly now', () => {
  // #844: this child-webview panel CAN use `listen` (unlike the ai-chat
  // overlay -- see that route's own doc comment on why it can't), and it
  // still does for ai-chat-remote-state (session active/error/PTT floor).
  // But accumulating the transcript here became dead code once the overlay
  // fetches/receives it directly; dormant code doesn't merge (CLAUDE.md).
  assert.doesNotMatch(surfaceSource, /listen<AiChatRemoteTranscriptEvent>/);
  assert.doesNotMatch(surfaceSource, /aiChatTranscriptTurns/);
  assert.match(surfaceSource, /listen<AiChatRemoteSessionState>\(EVENTS\.aiChatRemoteState/);
});

test('the ai-chat overlay applies a pushed remote-transcript delta, scoped to this window and owner', () => {
  // Unlike the surface panel, this child overlay webview cannot rely on
  // Tauri's `listen` on macOS (see the route's doc comment) -- Rust pushes
  // deltas in via `window.__petalAiChatRemoteTranscript`, exposed here as
  // `applyRemoteTranscript`.
  const fnBody = aiChatOverlaySource.slice(
    aiChatOverlaySource.indexOf('function applyRemoteTranscript('),
    aiChatOverlaySource.indexOf('async function sendText()')
  );
  assert.match(fnBody, /if \(payload\.windowId !== windowId \|\| payload\.ownerIdentity !== ownerIdentity\) return;/);
  assert.match(fnBody, /turns = appendTranscriptDelta\(turns, payload, AI_CHAT_TRANSCRIPT_MAX_TURNS\);/);
  // Wired to the real eval-injection target Rust calls.
  assert.match(
    aiChatOverlaySource,
    /__petalAiChatRemoteTranscript = applyRemoteTranscript;/
  );
});

test('a failed or empty local-identity read retries instead of leaving the PTT floor logic permanently blind', () => {
  const body = surfaceSource.slice(
    surfaceSource.indexOf('async function refreshAiChatLocalIdentity'),
    surfaceSource.indexOf('function onAiChatPttStart')
  );
  // A resolved identity is kept; an empty/failed read schedules another
  // attempt rather than settling for null forever (an unresolved identity
  // reads as "someone else holds the floor" even for our OWN grant).
  assert.match(body, /if \(identity\) \{\s*aiChatLocalIdentity = identity;\s*return;\s*\}/);
  assert.match(body, /if \(attemptsLeft <= 0\) return;/);
  assert.match(body, /setTimeout\(\(\) => void refreshAiChatLocalIdentity\(attemptsLeft - 1\), 500\);/);
});

test('a fresh remote session clears whatever transcript the previous one left behind', () => {
  // Otherwise a new conversation could open showing a stale exchange from
  // whoever used AI chat on this window before. #844: this now lives in the
  // ai-chat overlay's own applyRemoteState, alongside the session state it
  // reacts to (the surface panel no longer accumulates a transcript at all).
  const fnBody = aiChatOverlaySource.slice(
    aiChatOverlaySource.indexOf('function applyRemoteState('),
    aiChatOverlaySource.indexOf('function applyRemoteTranscript(')
  );
  assert.match(fnBody, /if \(payload\.active && session\?\.active !== true\) turns = \[\];/);
});

test('floating native AI chat panel route and Rust panel lifecycle', () => {
  const panelPageTs = read('../src/routes/ai-chat-panel/+page.ts');
  const panelPageSvelte = read('../src/routes/ai-chat-panel/+page.svelte');
  const panelRs = read('../src-tauri/src/ai_chat/panel.rs');
  const libRs = read('../src-tauri/src/lib.rs');

  // Route configuration
  assert.match(panelPageTs, /export const ssr = false;/);
  assert.match(panelPageTs, /export const prerender = true;/);
  assert.match(panelPageSvelte, /function handleEnded\(reason: AiChatEndReason\)/);
  assert.match(panelPageSvelte, /endMessage = aiChatEndReasonMessage\(reason\);/);
  assert.match(panelPageSvelte, /<AiChatPanel \{endMessage\} onEnded=\{handleEnded\} \/>/);

  // The active disclosure is true only while media is actually live, and the
  // terminal header copy must remain distinct from live status.
  assert.match(panelSource, /\{#if phase === 'live'\}\s*<p class="disclosure">/);
  assert.doesNotMatch(panelSource, /<p class="disclosure">[^<]*<\/p>\s*\{#if phase === 'live'\}/);
  assert.match(panelSource, /phase === 'ended' && endMessage/);
  assert.match(panelSource, /aiChatStatusLabel\(phase\)/);

  // Rust panel configuration: can_become_key_window, no_activate, accept_first_mouse, raise_panel_only
  assert.match(panelRs, /can_become_key_window: true/);
  assert.match(panelRs, /\.no_activate\(true\)/);
  assert.match(panelRs, /\.accept_first_mouse\(true\)/);
  assert.match(panelRs, /raise_panel_only/);
  assert.match(panelRs, /calculate_ai_chat_panel_position/);

  // Setup in lib.rs
  assert.match(libRs, /create_ai_chat_panel/);
  assert.match(libRs, /ai_chat_panel_present/);
  assert.match(libRs, /ai_chat_panel_dismiss/);
});

test('panel async reads are scoped to the session generation that requested them', () => {
  const connecting = panelSource.slice(
    panelSource.indexOf("case 'connecting':"),
    panelSource.indexOf("case 'live':")
  );
  assert.match(connecting, /const connectingGeneration = \+\+sessionGeneration;/);
  assert.match(connecting, /const connectingWindowId = payload\.windowId;/);
  assert.match(connecting, /ownerAppName = null;/);
  assert.match(
    connecting,
    /sessionGeneration !== connectingGeneration \|\|\s*windowId !== connectingWindowId/
  );

  const live = panelSource.slice(
    panelSource.indexOf("case 'live':"),
    panelSource.indexOf("case 'ended':")
  );
  assert.match(live, /const liveGeneration = sessionGeneration;/);
  assert.match(live, /const liveWindowId = payload\.windowId;/);
  assert.match(live, /sessionGeneration !== liveGeneration \|\| windowId !== liveWindowId/);

  const refresh = panelSource.slice(
    panelSource.indexOf('async function refreshControlStatus'),
    panelSource.indexOf('async function sendTypedText')
  );
  assert.match(refresh, /const requestedWindowId = windowId;/);
  assert.match(refresh, /const requestedGeneration = sessionGeneration;/);
  assert.match(
    refresh,
    /windowId !== requestedWindowId \|\|\s*sessionGeneration !== requestedGeneration/
  );
});
