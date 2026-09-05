// `petal.ai-chat` (#657) — the web half of the contract.
//
// The authorization matrix is driven FROM contracts/petal-contracts.json, the
// same fixture apps/desktop/src-tauri/src/ai_chat/wire.rs's tests read. That is
// what stops the two implementations drifting: a change on either side that
// the fixture does not sanction fails here.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  AI_CHAT_ACTIVE_DISCLOSURE,
  AI_CHAT_END_REASON_MESSAGES,
  aiChatEndReasonMessage,
  aiChatPublishOptions,
  aiChatSessionKey,
  appendAiChatTranscriptDelta,
  authorizeAiChatMessage,
  closeAiChatOpenTurns,
  createAiChatSessions,
  encodeAiChatMessage,
  formatAiChatCountdown,
  isNormalAiChatEnd,
  parseAiChatPayload,
  type AiChatTranscriptTurn,
} from '../src/aiChat.ts';
import {
  AI_CHAT_MISSED_HEARTBEATS_BEFORE_STALE,
  AI_CHAT_STALE_AFTER_MS,
  AI_CHAT_STATE_HEARTBEAT_MS,
  AI_CHAT_TOPIC,
  AI_TRACK_PREFIX,
  aiTrackName,
  aiTrackWindowId,
  cameraWindowId,
  isAiTrackName,
  trackNameForCamera,
  trackNameForWindow,
  type AiChatEndReason,
  type AiChatMessage,
} from '../src/trackNames.ts';
import { windowIdFromTrackName } from '../src/telepointer.ts';
import { discoverWindowPublications, type RoomLike } from '../src/publicationReconcile.ts';

const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8'),
) as {
  topics: { aiChat: string };
  aiTracks: Array<{ windowId: number; trackName: string }>;
  aiChatMessages: Array<{
    name: string;
    reliable: boolean;
    authorizedSenders: 'any-participant' | 'window-owner-only' | 'self-only';
    message: AiChatMessage & Record<string, unknown>;
  }>;
  aiChatEndReasons: string[];
};

const OWNER = 'owner-alice';

function message(body: Partial<AiChatMessage> & { type: AiChatMessage['type'] }): AiChatMessage {
  return { v: 1, windowId: 42, ownerIdentity: OWNER, ...body } as AiChatMessage;
}

// ---------------------------------------------------------------------------
// Track-name namespace
// ---------------------------------------------------------------------------

test('ai track names match the shared native/web fixture', () => {
  assert.equal(AI_CHAT_TOPIC, contractFixture.topics.aiChat);
  for (const vector of contractFixture.aiTracks) {
    assert.equal(aiTrackName(vector.windowId), vector.trackName);
    assert.ok(isAiTrackName(vector.trackName), vector.trackName);
    assert.equal(aiTrackWindowId(vector.trackName), vector.windowId);
    assert.ok(vector.trackName.startsWith(AI_TRACK_PREFIX));
  }
});

test('ai track names never collide with the window or camera namespaces', () => {
  // A misclassified assistant track showing up as a participant tile, a
  // shared window, or an "unknown track" is exactly what this namespace
  // exists to prevent -- so assert non-collision in BOTH directions.
  assert.equal(isAiTrackName(trackNameForWindow(42)), false);
  assert.equal(isAiTrackName(trackNameForCamera('alice')), false);
  assert.equal(isAiTrackName('petal-window-42'), false);
  assert.equal(isAiTrackName('petal-camera-web-tester'), false);
  assert.equal(isAiTrackName(''), false);
  assert.equal(isAiTrackName(null), false);
  assert.equal(isAiTrackName(undefined), false);

  assert.equal(aiTrackName(42).startsWith('petal-window-'), false);
  assert.equal(aiTrackName(42).startsWith('petal-camera-'), false);

  // The window-id parsers must refuse it rather than treating the trailing
  // number as a shareable window.
  assert.equal(windowIdFromTrackName(aiTrackName(42)), null);
  assert.equal(windowIdFromTrackName(trackNameForWindow(42)), 42);

  // And it must not derive a camera synthetic id by accident either.
  assert.notEqual(cameraWindowId(aiTrackName(42)), cameraWindowId(trackNameForWindow(42)));
});

test('ai tracks are never discovered as shared-window publications', () => {
  const room: RoomLike = {
    remoteParticipants: new Map([
      [
        OWNER,
        {
          identity: OWNER,
          trackPublications: new Map([
            ['sid-window', { trackSid: 'sid-window', trackName: 'petal-window-42', kind: 'video', isSubscribed: true }],
            // Same numeric id, assistant namespace, and (deliberately) claiming
            // to be video: the classification must not depend on `kind`.
            ['sid-ai', { trackSid: 'sid-ai', trackName: aiTrackName(42), kind: 'video', isSubscribed: true }],
          ]),
        },
      ],
    ]),
  };

  const found = discoverWindowPublications(room);
  assert.deepEqual(
    found.map((entry) => entry.trackSid),
    ['sid-window'],
  );
});

test('aiTrackWindowId rejects malformed and out-of-range suffixes', () => {
  assert.equal(aiTrackWindowId('petal-ai-window-0'), null);
  assert.equal(aiTrackWindowId('petal-ai-window-abc'), null);
  assert.equal(aiTrackWindowId('petal-ai-audio-42'), null);
  assert.equal(aiTrackWindowId('petal-window-42'), null);
  assert.equal(aiTrackWindowId(aiTrackName(0xffff_ffff)), 0xffff_ffff);
});

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

test('every pinned aiChat fixture parses and round-trips', () => {
  assert.ok(contractFixture.aiChatMessages.length > 0);
  for (const vector of contractFixture.aiChatMessages) {
    const parsed = parseAiChatPayload(JSON.stringify(vector.message));
    assert.ok(parsed, `fixture '${vector.name}' does not parse`);
    assert.deepEqual(parsed, vector.message, vector.name);
    // Reliable, always: session state and transcript lines must not be dropped.
    assert.equal(vector.reliable, true, vector.name);
    assert.deepEqual(aiChatPublishOptions(), { reliable: true, topic: AI_CHAT_TOPIC }, vector.name);
    assert.deepEqual(
      parseAiChatPayload(encodeAiChatMessage(parsed)),
      parsed,
      `${vector.name} does not survive encode/decode`,
    );
  }
});

test('the pinned end-reason vocabulary is closed and fully rendered', () => {
  const pinned = contractFixture.aiChatEndReasons as AiChatEndReason[];
  const rendered = Object.keys(AI_CHAT_END_REASON_MESSAGES);
  assert.deepEqual([...rendered].sort(), [...pinned].sort(), 'reason vocabulary drifted');

  const seen = new Set<string>();
  for (const reason of pinned) {
    const copy = aiChatEndReasonMessage(reason);
    assert.ok(copy.length > 0, `${reason} has empty copy`);
    assert.ok(seen.add(copy), `duplicate copy for ${reason}`);
  }
  // Mirrors EndReason::is_normal(): only these two are ordinary conclusions.
  assert.deepEqual(pinned.filter((reason) => isNormalAiChatEnd(reason)), ['stopped', 'time-limit']);
});

test('the end-reason copy is verbatim identical to the Rust source of truth', () => {
  // state.rs owns the wording; this table is the web renderer for the same
  // tokens, never a second vocabulary. Read the Rust file so the two cannot
  // silently diverge.
  const rust = readFileSync(
    new URL('../../apps/desktop/src-tauri/src/ai_chat/state.rs', import.meta.url),
    'utf8',
  );
  const pairs = [...rust.matchAll(/EndReason::(\w+)\s*=>\s*"([^"]+)"/g)].map(
    ([, variant, copy]) => [variant, copy] as const,
  );
  const kebab = (variant: string) =>
    variant.replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase() as AiChatEndReason;

  const fromRust = new Map<string, string>();
  for (const [variant, copy] of pairs) {
    const token = kebab(variant);
    if (!contractFixture.aiChatEndReasons.includes(token)) continue;
    fromRust.set(token, copy);
  }
  assert.equal(fromRust.size, contractFixture.aiChatEndReasons.length, 'did not read every reason from state.rs');
  for (const [token, copy] of fromRust) {
    assert.equal(AI_CHAT_END_REASON_MESSAGES[token as AiChatEndReason], copy, token);
  }
});

test('an unknown error token is dropped rather than rendered raw', () => {
  const parsed = parseAiChatPayload(
    JSON.stringify({ v: 1, type: 'state', windowId: 42, ownerIdentity: OWNER, active: false, error: 'meltdown' }),
  );
  assert.ok(parsed);
  assert.equal(parsed.type === 'state' && parsed.error, undefined);
});

test('malformed packets are rejected instead of half-applied', () => {
  const bad = [
    '{not-json',
    JSON.stringify({ v: 1, type: 'nope', windowId: 42, ownerIdentity: OWNER }),
    JSON.stringify({ v: 1, type: 'state', windowId: 42, ownerIdentity: OWNER }), // no `active`
    JSON.stringify({ v: 1, type: 'state', windowId: 0, ownerIdentity: OWNER, active: true }),
    JSON.stringify({ v: 1, type: 'state', windowId: 42, ownerIdentity: '  ', active: true }),
    JSON.stringify({ v: 1, type: 'transcript', windowId: 42, ownerIdentity: OWNER, role: 'system', text: 'x' }),
    JSON.stringify({ v: 1, type: 'transcript', windowId: 42, ownerIdentity: OWNER, role: 'user', text: 7 }),
    JSON.stringify([{ v: 1, type: 'startRequest' }]),
  ];
  for (const payload of bad) assert.equal(parseAiChatPayload(payload), null, payload);
});

test('transcript final defaults to false when the sender omits it', () => {
  const parsed = parseAiChatPayload(
    JSON.stringify({ v: 1, type: 'transcript', windowId: 42, ownerIdentity: OWNER, role: 'user', text: 'hi' }),
  );
  assert.ok(parsed);
  assert.equal(parsed.type === 'transcript' && parsed.final, false);
});

// ---------------------------------------------------------------------------
// Authorization matrix — driven from the fixture, all three sender classes
// ---------------------------------------------------------------------------

test('the authorization matrix enforces exactly what the fixture records', () => {
  for (const vector of contractFixture.aiChatMessages) {
    const parsed = parseAiChatPayload(JSON.stringify(vector.message));
    assert.ok(parsed, vector.name);
    const owner = parsed.ownerIdentity;

    switch (vector.authorizedSenders) {
      case 'any-participant':
        // Anyone in the room may ASK; the owner decides whether to act.
        assert.equal(authorizeAiChatMessage(parsed, 'someone-else'), null, vector.name);
        assert.equal(authorizeAiChatMessage(parsed, owner), null, vector.name);
        break;

      case 'window-owner-only':
        assert.equal(authorizeAiChatMessage(parsed, owner), null, vector.name);
        // A peer must not be able to announce a session that is not running,
        // or put words in the assistant's mouth on someone else's window.
        assert.equal(authorizeAiChatMessage(parsed, 'someone-else'), 'notWindowOwner', vector.name);
        assert.equal(authorizeAiChatMessage(parsed, ''), 'notWindowOwner', vector.name);
        assert.equal(authorizeAiChatMessage(parsed, undefined), 'notWindowOwner', vector.name);
        break;

      case 'self-only':
        assert.equal(authorizeAiChatMessage(parsed, 'someone-else'), null, vector.name);
        assert.equal(authorizeAiChatMessage(parsed, owner), null, vector.name);
        // An unauthenticated sender may not claim the floor for anyone.
        assert.equal(authorizeAiChatMessage(parsed, ''), 'notSelf', vector.name);
        assert.equal(authorizeAiChatMessage(parsed, undefined), 'notSelf', vector.name);
        break;

      default:
        assert.fail(`unknown authorizedSenders '${vector.authorizedSenders}' in '${vector.name}'`);
    }
  }
});

test('the fixture covers all three sender classes', () => {
  // Otherwise the matrix test above could pass vacuously for a class the
  // fixture happens not to exercise.
  const classes = new Set(contractFixture.aiChatMessages.map((vector) => vector.authorizedSenders));
  assert.deepEqual([...classes].sort(), ['any-participant', 'self-only', 'window-owner-only']);
});

test('a wrong wire version is rejected outright, for every kind', () => {
  for (const type of ['startRequest', 'stopRequest', 'state', 'pttStart', 'pttEnd', 'transcript'] as const) {
    const wrongVersion = { ...message({ type }), v: 2 } as unknown as AiChatMessage;
    assert.equal(authorizeAiChatMessage(wrongVersion, OWNER), 'unsupportedVersion', type);
  }
});

test('pttEnd is authorized under the same self-only rule as pttStart', () => {
  assert.equal(authorizeAiChatMessage(message({ type: 'pttEnd' }), 'peer-bob'), null);
  assert.equal(authorizeAiChatMessage(message({ type: 'pttEnd' }), ''), 'notSelf');
});

// ---------------------------------------------------------------------------
// Transcript coalescing
// ---------------------------------------------------------------------------

function texts(turns: readonly AiChatTranscriptTurn[]): string[] {
  return turns.map((turn) => `${turn.role}:${turn.text}${turn.final ? '.' : '…'}`);
}

test('consecutive non-final deltas of the same role coalesce into one bubble', () => {
  let turns: AiChatTranscriptTurn[] = [];
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: 'The build ', final: false });
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: 'failed on ', final: false });
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: 'line 12.', final: false });
  assert.deepEqual(texts(turns), ['assistant:The build failed on line 12.…']);
  assert.equal(turns.length, 1);
});

test('final: true closes the bubble so the next delta opens a new one', () => {
  let turns: AiChatTranscriptTurn[] = [];
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: 'Yes', final: false });
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: '.', final: true });
  assert.deepEqual(texts(turns), ['assistant:Yes..']);
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: 'And also', final: false });
  assert.deepEqual(texts(turns), ['assistant:Yes..', 'assistant:And also…']);
});

test('a role change opens a new bubble even mid-stream', () => {
  let turns: AiChatTranscriptTurn[] = [];
  turns = appendAiChatTranscriptDelta(turns, { role: 'user', text: 'what is this', final: false });
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: 'a build log', final: false });
  assert.deepEqual(texts(turns), ['user:what is this…', 'assistant:a build log…']);
  assert.notEqual(turns[0].id, turns[1].id);
});

test('an empty final never opens a bubble', () => {
  // Turn-complete arrives as text:'' + final:true. Opening a bubble for it
  // would leave a permanent empty assistant bubble after every reply.
  let turns = appendAiChatTranscriptDelta([], { role: 'assistant', text: '', final: true });
  assert.deepEqual(turns, []);
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: '', final: false });
  assert.deepEqual(turns, []);
});

test('an empty final closes the open bubble of that role', () => {
  let turns = appendAiChatTranscriptDelta([], { role: 'assistant', text: 'done', final: false });
  turns = appendAiChatTranscriptDelta(turns, { role: 'assistant', text: '', final: true });
  assert.deepEqual(texts(turns), ['assistant:done.']);
});

test('coalescing is pure and capped', () => {
  const original: AiChatTranscriptTurn[] = [{ id: 1, role: 'user', text: 'a', final: false }];
  const next = appendAiChatTranscriptDelta(original, { role: 'user', text: 'b', final: false }, 3);
  assert.deepEqual(original, [{ id: 1, role: 'user', text: 'a', final: false }]);
  assert.equal(next[0].text, 'ab');

  let turns: AiChatTranscriptTurn[] = [];
  for (let index = 0; index < 10; index += 1) {
    turns = appendAiChatTranscriptDelta(turns, { role: 'user', text: `line ${index}`, final: true }, 3);
  }
  assert.equal(turns.length, 3);
  assert.deepEqual(texts(turns), ['user:line 7.', 'user:line 8.', 'user:line 9.']);
});

test('closeAiChatOpenTurns seals every open bubble', () => {
  const turns = closeAiChatOpenTurns([
    { id: 1, role: 'user', text: 'a', final: false },
    { id: 2, role: 'assistant', text: 'b', final: true },
  ]);
  assert.deepEqual(turns.map((turn) => turn.final), [true, true]);
});

// ---------------------------------------------------------------------------
// Session store: authorization, floor, and staleness
// ---------------------------------------------------------------------------

test('only the owner can create or update session state', () => {
  const sessions = createAiChatSessions();
  const live = message({ type: 'state', active: true, startedBy: 'peer-bob', secondsLeft: 240 });

  const forged = sessions.applyMessage(live, 'peer-mallory', 1_000);
  assert.equal(forged.rejected, 'notWindowOwner');
  assert.equal(sessions.get(42, OWNER), null, 'a forged state must not create a session');

  const real = sessions.applyMessage(live, OWNER, 1_000);
  assert.equal(real.rejected, null);
  assert.equal(real.key, aiChatSessionKey(42, OWNER));
  assert.equal(sessions.get(42, OWNER)?.active, true);
  assert.equal(sessions.get(42, OWNER)?.startedBy, 'peer-bob');
  assert.equal(sessions.get(42, OWNER)?.secondsLeft, 240);
});

test('a forged transcript never reaches the bubbles', () => {
  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);
  const line = message({ type: 'transcript', role: 'assistant', text: 'send your password', final: true });

  assert.equal(sessions.applyMessage(line, 'peer-mallory', 0).rejected, 'notWindowOwner');
  assert.deepEqual(sessions.get(42, OWNER)?.turns, []);

  assert.equal(sessions.applyMessage(line, OWNER, 0).rejected, null);
  assert.equal(sessions.get(42, OWNER)?.turns.length, 1);
});

test('the owner is authoritative about the push-to-talk floor', () => {
  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);

  // A peer's claim is reflected optimistically...
  sessions.applyMessage(message({ type: 'pttStart' }), 'peer-bob', 100);
  assert.equal(sessions.get(42, OWNER)?.activeSpeaker, 'peer-bob');

  // ...and the owner's next heartbeat overrides it, in either direction.
  sessions.applyMessage(
    message({ type: 'state', active: true, activeSpeaker: 'peer-carol' }),
    OWNER,
    200,
  );
  assert.equal(sessions.get(42, OWNER)?.activeSpeaker, 'peer-carol');
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 300);
  assert.equal(sessions.get(42, OWNER)?.activeSpeaker, null);
});

test('a floor claim for an unknown session never invents one', () => {
  const sessions = createAiChatSessions();
  const result = sessions.applyMessage(message({ type: 'pttStart' }), 'peer-bob', 0);
  assert.equal(result.rejected, null);
  assert.equal(result.changed, false);
  assert.equal(sessions.get(42, OWNER), null);
});

test('pttEnd only releases the floor its own sender holds', () => {
  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);
  sessions.applyMessage(message({ type: 'pttStart' }), 'peer-bob', 10);

  sessions.applyMessage(message({ type: 'pttEnd' }), 'peer-carol', 20);
  assert.equal(sessions.get(42, OWNER)?.activeSpeaker, 'peer-bob', 'a peer must not release someone else');

  sessions.applyMessage(message({ type: 'pttEnd' }), 'peer-bob', 30);
  assert.equal(sessions.get(42, OWNER)?.activeSpeaker, null);
});

test('start/stop requests carry no receiver-visible state', () => {
  const sessions = createAiChatSessions();
  for (const type of ['startRequest', 'stopRequest'] as const) {
    const result = sessions.applyMessage(message({ type }), 'peer-bob', 0);
    assert.equal(result.rejected, null, type);
    assert.equal(result.changed, false, type);
  }
  assert.deepEqual(sessions.entries(), []);
});

test('a session expires after three missed heartbeats', () => {
  // A crashed host stops heartbeating without ever sending active:false.
  // Without expiry the room keeps showing a phantom "AI chat live" badge.
  assert.equal(AI_CHAT_STATE_HEARTBEAT_MS, 5_000);
  assert.equal(AI_CHAT_MISSED_HEARTBEATS_BEFORE_STALE, 3);
  assert.equal(AI_CHAT_STALE_AFTER_MS, 15_000);

  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);

  // Two missed heartbeats: a hiccup, not a death. Still live.
  assert.deepEqual(sessions.expireStale(AI_CHAT_STATE_HEARTBEAT_MS * 2), []);
  assert.equal(sessions.get(42, OWNER)?.active, true);

  // Exactly at the boundary is still tolerated; past it, the session is gone.
  assert.deepEqual(sessions.expireStale(AI_CHAT_STALE_AFTER_MS), []);
  assert.deepEqual(sessions.expireStale(AI_CHAT_STALE_AFTER_MS + 1), [aiChatSessionKey(42, OWNER)]);
  assert.equal(sessions.get(42, OWNER), null);
  assert.deepEqual(sessions.entries(), []);
});

test('a heartbeat refreshes the staleness clock but a transcript does not', () => {
  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 10_000);
  assert.deepEqual(sessions.expireStale(20_000), [], 'the 10s heartbeat should have reset the clock');

  // `state` is the heartbeat; transcript traffic alone must not keep a session
  // that has stopped reporting itself alive.
  sessions.applyMessage(
    message({ type: 'transcript', role: 'assistant', text: 'still here', final: true }),
    OWNER,
    24_000,
  );
  assert.deepEqual(sessions.expireStale(25_001), [aiChatSessionKey(42, OWNER)]);
});

test('an owner disconnect clears their sessions immediately', () => {
  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);
  sessions.applyMessage(
    { v: 1, type: 'state', windowId: 7, ownerIdentity: 'owner-bob', active: true },
    'owner-bob',
    0,
  );

  assert.deepEqual(sessions.removeOwner(OWNER), [aiChatSessionKey(42, OWNER)]);
  assert.equal(sessions.get(42, OWNER), null);
  assert.equal(sessions.get(7, 'owner-bob')?.active, true, 'another owner must be untouched');
  assert.deepEqual(sessions.removeOwner('nobody'), []);
});

test('sessions are keyed by owner AND window id', () => {
  // A raw CGWindowID is only unique on the machine that produced it, so two
  // sharers can legitimately both be window 42.
  const sessions = createAiChatSessions();
  sessions.applyMessage(message({ type: 'state', active: true }), OWNER, 0);
  sessions.applyMessage(
    { v: 1, type: 'state', windowId: 42, ownerIdentity: 'owner-bob', active: false, error: 'busy' },
    'owner-bob',
    0,
  );
  assert.equal(sessions.get(42, OWNER)?.active, true);
  assert.equal(sessions.get(42, 'owner-bob')?.active, false);
  assert.equal(sessions.get(42, 'owner-bob')?.error, 'busy');
  assert.equal(sessions.entries().length, 2);
});

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

test('the countdown never renders a negative or NaN time', () => {
  assert.equal(formatAiChatCountdown(240), '4:00');
  assert.equal(formatAiChatCountdown(61), '1:01');
  assert.equal(formatAiChatCountdown(0), '0:00');
  assert.equal(formatAiChatCountdown(-5), '0:00');
  assert.equal(formatAiChatCountdown(Number.NaN), '0:00');
});

test('the session disclosure names both the window content and the voice', () => {
  // The session-visibility rule: it must be unmistakable that window content
  // AND room voice are going to a third-party API.
  assert.match(AI_CHAT_ACTIVE_DISCLOSURE, /window/i);
  assert.match(AI_CHAT_ACTIVE_DISCLOSURE, /voice/i);
  assert.match(AI_CHAT_ACTIVE_DISCLOSURE, /Google/);
});
