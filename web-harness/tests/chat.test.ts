// shared/logic/chat.ts: the wire parser is strict and pinned to the contract
// vectors, the store dedupes, orders, counts unread, and relays history
// within the byte cap. Runs under node --test + tsx.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  CHAT_LIMITS,
  CHAT_TOPIC,
  chatMessageWire,
  chatNoticeText,
  chatPostWire,
  chatPrivateNoticeText,
  classifyChatInput,
  matchingChatCommands,
  type ChatCommandOption,
  chatPublishOptions,
  createChatStore,
  encodeChatWire,
  isChatMessageId,
  newChatMessageId,
  normalizeChatText,
  parseChatPayload,
  type ChatHistoryWire,
  type ChatMsgWire,
} from '@petal/shared/logic/chat';

const contracts = JSON.parse(readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')) as {
  topics: { chat: string };
  chatLimits: typeof CHAT_LIMITS;
  chatMessages: Array<{ name: string; reliable: boolean; direct: boolean; message: Record<string, unknown>; fields: string[]; entryFields?: string[] }>;
  chatRejectedPayloads: Array<{ name: string; payload: unknown }>;
  chatDataEvent: { example: { payloadBase64: string } };
};

const alex = { identity: 'alex-1a2b', name: 'Alex' };
const mira = { identity: 'mira-9f8e', name: 'Mira' };

function msg(id: string, text: string, t: number): ChatMsgWire {
  return { v: 1, type: 'msg', id, text, t };
}

test('topic and limits match the contract', () => {
  assert.equal(CHAT_TOPIC, contracts.topics.chat);
  assert.deepEqual({ ...CHAT_LIMITS }, contracts.chatLimits);
});

test('every contract vector parses back to itself with exactly the pinned fields', () => {
  assert.deepEqual(
    contracts.chatMessages.map((v) => v.name),
    ['msg', 'history-req', 'history', 'post', 'history-posts'],
  );
  for (const vector of contracts.chatMessages) {
    assert.deepEqual(Object.keys(vector.message).sort(), vector.fields, vector.name);
    const parsed = parseChatPayload(JSON.stringify(vector.message));
    assert.deepEqual(parsed, vector.message, vector.name);
    assert.ok(parsed);
    const options = chatPublishOptions(parsed, 'joiner-1');
    assert.equal(options.topic, CHAT_TOPIC);
    assert.equal(options.reliable, vector.reliable);
    assert.equal(options.destinationIdentities !== undefined, vector.direct, `${vector.name} direct`);
    if (vector.entryFields) {
      for (const entry of vector.message.messages as Record<string, unknown>[]) {
        assert.deepEqual(Object.keys(entry).sort(), vector.entryFields);
      }
    }
  }
  const example = Buffer.from(contracts.chatDataEvent.example.payloadBase64, 'base64').toString('utf8');
  assert.deepEqual(parseChatPayload(example), contracts.chatMessages[0].message);
});

test('every rejected vector is dropped whole', () => {
  assert.ok(contracts.chatRejectedPayloads.length >= 8);
  const generated = [
    { name: 'text too long', payload: { ...contracts.chatMessages[0].message, text: 'x'.repeat(CHAT_LIMITS.maxTextChars + 1) } },
    {
      name: 'history over the cap',
      payload: { v: 1, type: 'history', messages: Array.from({ length: CHAT_LIMITS.historyMessages + 1 }, () => (contracts.chatMessages[2].message.messages as unknown[])[0]) },
    },
  ];
  for (const vector of [...contracts.chatRejectedPayloads, ...generated]) {
    const text = typeof vector.payload === 'string' ? vector.payload : JSON.stringify(vector.payload);
    assert.equal(parseChatPayload(text), null, vector.name);
    assert.equal(parseChatPayload(new TextEncoder().encode(text)), null, vector.name);
  }
  // Extra fields are tolerated but never read: a decoy sender does not survive parsing.
  const decoy = { ...contracts.chatMessages[0].message, senderIdentity: 'mallory', sender: { name: 'Admin' } };
  assert.deepEqual(parseChatPayload(JSON.stringify(decoy)), contracts.chatMessages[0].message);
});

test('normalizeChatText trims, unifies line breaks, and refuses the unsendable', () => {
  assert.equal(normalizeChatText('  hi\r\nthere\t!  '), 'hi\nthere !');
  assert.equal(normalizeChatText('a\n\n\n\n\nb'), 'a\n\nb');
  assert.equal(normalizeChatText('   \n  '), null);
  assert.equal(normalizeChatText('x'.repeat(CHAT_LIMITS.maxTextChars)), 'x'.repeat(CHAT_LIMITS.maxTextChars));
  assert.equal(normalizeChatText('x'.repeat(CHAT_LIMITS.maxTextChars + 1)), null);
  assert.equal(normalizeChatText('pay \u202eevil'), null);
  assert.equal(normalizeChatText('bell\u0007'), null);
  assert.equal(normalizeChatText('emoji 👍 and ünïcode are fine'), 'emoji 👍 and ünïcode are fine');
});

test('message ids are opaque, url-safe, and unique', () => {
  const ids = new Set(Array.from({ length: 200 }, () => newChatMessageId()));
  assert.equal(ids.size, 200);
  for (const id of ids) assert.ok(isChatMessageId(id), id);
  assert.ok(!isChatMessageId('short'));
  assert.ok(!isChatMessageId('has space here'));
  const wire = chatMessageWire('hello', 42);
  assert.equal(wire.t, 42);
  assert.ok(isChatMessageId(wire.id));
});

test('store: receive orders by sender clock, dedupes, and counts unread only while closed', () => {
  const store = createChatStore(() => alex.identity);
  const changes: number[] = [];
  store.onChange(() => changes.push(store.messages.length));

  assert.equal(store.receive(msg('m-mira-0001', 'second', 200), mira), 'added');
  assert.equal(store.receive(msg('m-mira-0002', 'first', 100), mira), 'added');
  assert.equal(store.receive(msg('m-mira-0001', 'second again', 200), mira), 'duplicate');
  assert.deepEqual(
    store.messages.map((m) => m.text),
    ['first', 'second'],
  );
  assert.equal(store.unread, 2, 'closed drawer counts peers');

  // Our own echo (the SFU sends our packet back to us on some paths) is self and never unread.
  assert.equal(store.receive(msg('m-alex-0001', 'mine', 300), alex), 'added');
  assert.equal(store.messages[2].self, true);
  assert.equal(store.unread, 2);

  store.setOpen(true);
  assert.equal(store.unread, 0);
  store.receive(msg('m-mira-0003', 'while open', 400), mira);
  assert.equal(store.unread, 0, 'open drawer: nothing unread');
  store.setOpen(false);
  store.receive(msg('m-mira-0004', 'after close', 500), mira);
  assert.equal(store.unread, 1);
  assert.equal(changes.length, 7, 'every mutation notifies exactly once; duplicates do not');
});

test('store: sent() records locally first and the echo is a duplicate', () => {
  const store = createChatStore(() => alex.identity);
  const wire = chatMessageWire('hi all', 1000);
  const local = store.sent(wire, alex);
  assert.equal(local.self, true);
  assert.equal(store.messages.length, 1);
  assert.equal(store.receive(wire, alex), 'duplicate');
  assert.equal(store.unread, 0);
});

test('store: history reply relays the newest messages within the byte cap; merge marks relayed claims', () => {
  const store = createChatStore(() => alex.identity);
  assert.deepEqual(store.historyReply(), { history: null, posts: null }, 'nothing to relay yet');
  for (let i = 0; i < CHAT_LIMITS.historyMessages + 10; i++) {
    store.receive(msg(`m-mira-${String(i).padStart(4, '0')}`, `message ${i}`, 1000 + i), mira);
  }
  const reply = store.historyReply().history;
  assert.ok(reply);
  assert.equal(reply.messages.length, CHAT_LIMITS.historyMessages);
  assert.equal(reply.messages[0].text, 'message 10', 'oldest dropped first');
  assert.equal(reply.messages.at(-1)?.senderIdentity, mira.identity);
  assert.ok(encodeChatWire(reply).length <= CHAT_LIMITS.maxPayloadBytes);

  // Long texts: the reply trims until it fits rather than exceeding the packet cap.
  const big = createChatStore(() => alex.identity);
  for (let i = 0; i < 20; i++) big.receive(msg(`m-big-${String(i).padStart(6, '0')}`, 'y'.repeat(1500), 1 + i), mira);
  const bigReply = big.historyReply().history;
  assert.ok(bigReply);
  assert.ok(bigReply.messages.length < 20 && bigReply.messages.length >= 4);
  assert.ok(encodeChatWire(bigReply).length <= CHAT_LIMITS.maxPayloadBytes);
  assert.equal(bigReply.messages.at(-1)?.id, 'm-big-000019', 'newest kept');

  // A late joiner merges: entries the relayer attributes to itself are not
  // "relayed"; entries about others are; none count as unread; our own are self.
  const joiner = createChatStore(() => 'theo-0000');
  const history: ChatHistoryWire = {
    v: 1,
    type: 'history',
    messages: [
      { id: 'm-mira-0100', text: 'from mira', t: 5, senderIdentity: mira.identity, senderName: 'Mira' },
      { id: 'm-alex-0100', text: 'from alex', t: 6, senderIdentity: alex.identity, senderName: 'Alex' },
      { id: 'm-theo-0100', text: 'from me earlier', t: 7, senderIdentity: 'theo-0000', senderName: 'Theo' },
    ],
  };
  assert.equal(joiner.mergeHistory(history, mira), 3);
  assert.equal(joiner.mergeHistory(history, alex), 0, 'second responder adds nothing');
  assert.deepEqual(
    joiner.messages.map((m) => [m.relayed, m.self]),
    [
      [false, false],
      [true, false],
      [true, true],
    ],
  );
  assert.equal(joiner.unread, 0);
});

test('store: retains at most the configured number of messages', () => {
  const store = createChatStore(() => alex.identity);
  for (let i = 0; i < CHAT_LIMITS.retainedMessages + 5; i++) {
    store.receive(msg(`m-mira-${String(i).padStart(6, '0')}`, `m${i}`, i), mira);
  }
  assert.equal(store.messages.length, CHAT_LIMITS.retainedMessages);
  assert.equal(store.messages[0].text, 'm5');
  assert.equal(store.receive(msg('m-mira-000000', 'm0 again', 0), mira), 'added', 'a dropped id may come back');
});

test('chatNoticeText is one line, names the sender, and never exceeds the budget', () => {
  assert.equal(chatNoticeText(mira, 'hello\nthere'), 'Mira: hello there');
  assert.equal(chatNoticeText({ identity: 'x', name: null }, 'hi'), 'Someone: hi');
  const long = chatNoticeText(mira, 'w'.repeat(500));
  assert.ok(long.length <= 80, `${long.length}`);
  assert.ok(long.endsWith('…'));
});

const timer = { id: 'petal.timer', name: 'Timer' };

test('plugin posts: received with via, count as unread from others, never from self', () => {
  const store = createChatStore(() => alex.identity);
  const fromMira = chatPostWire('⏱ Timer started: 5 min', timer, 100);
  assert.equal(store.receive(fromMira, mira), 'added');
  assert.deepEqual(store.messages[0]!.via, timer);
  assert.equal(store.messages[0]!.local, false);
  assert.equal(store.unread, 1);
  const mine = chatPostWire("⏱ Time's up", timer, 200);
  store.sent(mine, alex);
  assert.equal(store.messages[1]!.self, true);
  assert.deepEqual(store.messages[1]!.via, timer);
  assert.equal(store.unread, 1, 'our own plugin post is not unread');
  assert.equal(store.receive(mine, alex), 'duplicate', 'the echo of our own post collapses');
  // Round trip through the strict parser keeps the stamp.
  assert.deepEqual(parseChatPayload(encodeChatWire(fromMira)), fromMira);
});

test('private answers are local: shown, never relayed, never unread', () => {
  const store = createChatStore(() => alex.identity);
  store.receive(msg('m-mira-0001', 'typed by mira', 100), mira);
  store.setOpen(true);
  store.setOpen(false);
  const answer = store.notice('Usage: /timer 5m', timer, alex, 150);
  assert.equal(answer.local, true);
  assert.equal(answer.self, true);
  assert.deepEqual(answer.via, timer);
  assert.equal(store.unread, 0);
  store.sent(chatPostWire('⏱ Timer started: 5 min', timer, 200), alex);
  const reply = store.historyReply();
  assert.deepEqual(reply.history?.messages.map((m) => m.text), ['typed by mira'], 'typed messages only');
  assert.deepEqual(reply.posts?.messages.map((m) => [m.text, m.via.id]), [['⏱ Timer started: 5 min', 'petal.timer']], 'posts, without the private answer');
  assert.ok(encodeChatWire(reply.posts!).length <= CHAT_LIMITS.maxPayloadBytes);
});

test('history-posts merge keeps the via stamp and the relayed rule; a history packet never carries posts', () => {
  const joiner = createChatStore(() => 'theo-0000');
  const posts = {
    v: 1 as const,
    type: 'history-posts' as const,
    messages: [
      { id: 'p-mira-0001', text: 'Timer started: standup, 5 min', t: 5, senderIdentity: mira.identity, senderName: 'Mira', via: timer },
      { id: 'p-alex-0001', text: 'Timer cancelled: review', t: 6, senderIdentity: alex.identity, senderName: 'Alex', via: timer },
    ],
  };
  assert.equal(joiner.mergeHistory(posts, mira), 2);
  assert.deepEqual(joiner.messages.map((m) => [m.via?.id, m.relayed]), [['petal.timer', false], ['petal.timer', true]]);
  assert.equal(joiner.unread, 0);
  // A `history` entry with a via field is read as typed (the field is
  // ignored), which is why posts travel in their own packet.
  const parsed = parseChatPayload(JSON.stringify({ v: 1, type: 'history', messages: [{ ...posts.messages[0], via: timer }] }));
  assert.ok(parsed && parsed.type === 'history');
  assert.equal('via' in parsed.messages[0]!, false);
});

test('composer: /name args is a command, // escapes a leading slash, and an unknown /word is refused', () => {
  assert.deepEqual(classifyChatInput('   '), { kind: 'empty' });
  assert.deepEqual(classifyChatInput('hello'), { kind: 'message', text: 'hello' });
  assert.deepEqual(classifyChatInput('/timer 5m  standup '), { kind: 'command', name: 'timer', args: '5m  standup' });
  assert.deepEqual(classifyChatInput('/timer'), { kind: 'command', name: 'timer', args: '' });
  assert.deepEqual(classifyChatInput('  /timer\n5m'), { kind: 'command', name: 'timer', args: '5m' });
  assert.deepEqual(classifyChatInput('//shrug'), { kind: 'message', text: '/shrug' });
  assert.deepEqual(classifyChatInput('//usr/local/bin'), { kind: 'message', text: '/usr/local/bin' });
  assert.deepEqual(classifyChatInput('/usr/local/bin'), { kind: 'invalid-command', token: 'usr/local/bin' });
  assert.deepEqual(classifyChatInput('/Timer 5m'), { kind: 'invalid-command', token: 'Timer' });
  assert.deepEqual(classifyChatInput('/ spaced out'), { kind: 'message', text: '/ spaced out' }, 'a lone slash then a space is text');
  assert.equal(classifyChatInput('/timer ' + 'x'.repeat(501)).kind, 'invalid');
  assert.equal(classifyChatInput('/timer \u202eevil').kind, 'invalid');
  assert.equal(classifyChatInput('x'.repeat(2001)).kind, 'invalid');
});

test('autocomplete lists matching commands only while the draft is a bare /prefix', () => {
  const option = (name: string): ChatCommandOption => ({ name, usage: '', description: name, pluginId: 'p.x', pluginName: 'X', source: 'builtin' });
  const options = [option('timer'), option('poll'), option('tip')];
  assert.deepEqual(matchingChatCommands(options, '/').map((o) => o.name), ['poll', 'timer', 'tip']);
  assert.deepEqual(matchingChatCommands(options, '/ti').map((o) => o.name), ['timer', 'tip']);
  assert.deepEqual(matchingChatCommands(options, '/timer').map((o) => o.name), ['timer']);
  assert.deepEqual(matchingChatCommands(options, '/timer '), [], 'arguments started: no list');
  assert.deepEqual(matchingChatCommands(options, 'hi /ti'), []);
  assert.deepEqual(matchingChatCommands(options, '//'), []);
});

test('notices for plugin posts and private answers name the plugin', () => {
  assert.equal(chatNoticeText(mira, "⏱ Time's up", 80, timer), "Timer (Mira): ⏱ Time's up");
  assert.equal(chatPrivateNoticeText(timer, 'Usage: /timer 5m'), 'Timer: Usage: /timer 5m');
  assert.ok(chatPrivateNoticeText(timer, 'w'.repeat(500)).length <= 80);
});
