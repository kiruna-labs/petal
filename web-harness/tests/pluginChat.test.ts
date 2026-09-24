// The plugin side of meeting chat (I-7b): the manifest's chatCommands and the
// chat:* vocabulary, the broker's chat.post / chat.respond / command delivery,
// the one-owner-per-name command resolver, and the vendored Timer built-in.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { createPluginBroker, type HostAdapter, type LoadedPlugin } from '@petal/shared/plugin-host/broker';
import { parseBundle } from '@petal/shared/plugin-host/bundle';
import { resolveChatCommands } from '@petal/shared/plugin-host/chatCommands';
import { isPermission, validateManifest, type PluginManifest } from '@petal/shared/plugin-host/manifest';
import { METHOD_PERMISSIONS, EVENT_PERMISSIONS } from '@petal/shared/plugin-host/permissions';
import { PROTOCOL_VERSION, type Envelope } from '@petal/shared/plugin-host/protocol';
import { PERMISSION_LABELS } from '@petal/shared/plugin-host/settingsModel';
import { CHAT_COMMAND_LIMITS } from '@petal/shared/logic/chat';

class FakeFrame {
  sent: Envelope[] = [];
  postMessage(message: unknown): void {
    this.sent.push(structuredClone(message) as Envelope);
  }
  responses(): Extract<Envelope, { kind: 'res' }>[] {
    return this.sent.filter((e): e is Extract<Envelope, { kind: 'res' }> => e.kind === 'res');
  }
  events(name: string): Extract<Envelope, { kind: 'evt' }>[] {
    return this.sent.filter((e): e is Extract<Envelope, { kind: 'evt' }> => e.kind === 'evt' && e.event === name);
  }
}

const tick = () => new Promise((r) => setImmediate(r));
const req = (id: number, method: string, params: unknown = {}) => ({ v: PROTOCOL_VERSION, kind: 'req', id, method, params }) as const;

function timerManifest(over: Partial<PluginManifest> = {}): PluginManifest {
  return {
    manifestVersion: 1,
    id: 'petal.timer',
    version: '1.0.0',
    name: 'Timer',
    description: 'Countdowns',
    apiVersion: 1,
    minHostVersion: '0.1.0',
    scope: 'meeting',
    entry: 'plugin.js',
    permissions: ['chat:commands', 'chat:post'],
    contributes: { chatCommands: [{ name: 'timer', description: 'Start a countdown', usage: '5m [label]' }] },
    ...over,
  };
}

function loaded(manifest: PluginManifest, over: Partial<LoadedPlugin> = {}): LoadedPlugin {
  return { manifest, granted: manifest.permissions, source: 'builtin', ...over };
}

function chatAdapter() {
  const calls: string[] = [];
  let now = 1_000_000;
  const adapter: HostAdapter = {
    meeting: {
      self: () => ({ identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false }),
      participants: () => [],
      room: () => ({ label: 'r', phase: 'connected' }),
    },
    async publishData() {},
    async setState() {},
    storage: { get: async () => undefined, set: async () => {}, delete: async () => {}, keys: async () => [] },
    ui: { setButton() {}, openSurface() {}, closeSurface() {}, toast() {} },
    async fetch() {
      return { status: 200, headers: {}, body: '' };
    },
    async clipboardWriteText() {},
    log() {},
    chat: {
      async post(plugin, text) {
        calls.push(`post:${plugin.manifest.id}:${text}`);
      },
      respond(plugin, text) {
        calls.push(`respond:${plugin.manifest.id}:${text}`);
      },
    },
  };
  return { adapter, calls, clock: { now: () => now, advance: (ms: number) => (now += ms) } };
}

test('vocabulary: chat:post and chat:commands are permissions with consent copy; chat:read is reserved', () => {
  assert.ok(isPermission('chat:post'));
  assert.ok(isPermission('chat:commands'));
  assert.ok(!isPermission('chat:read'));
  assert.equal(typeof PERMISSION_LABELS['chat:post'], 'string');
  assert.equal(typeof PERMISSION_LABELS['chat:commands'], 'string');
  assert.equal(METHOD_PERMISSIONS['chat.post'], 'chat:post');
  assert.equal(METHOD_PERMISSIONS['chat.respond'], 'chat:commands');
  assert.equal(EVENT_PERMISSIONS['chat.command'], 'chat:commands');

  const reserved = validateManifest({ ...timerManifest(), permissions: ['chat:read'], contributes: undefined });
  assert.equal(reserved.ok, false);
  assert.match(!reserved.ok ? reserved.errors.join('\n') : '', /"chat:read" is not supported by this host/);
});

test('manifest: chatCommands need chat:commands, valid names, printable one-line text, and no duplicates; chat:post is meeting-only', () => {
  assert.equal(validateManifest(timerManifest()).ok, true);

  const errorsOf = (m: unknown) => {
    const r = validateManifest(m);
    return r.ok ? [] : r.errors;
  };
  assert.ok(errorsOf(timerManifest({ permissions: ['chat:post'] })).some((e) => /chatCommands requires permission "chat:commands"/.test(e)));
  assert.ok(errorsOf(timerManifest({ scope: 'local' })).some((e) => /"chat:post" requires scope "meeting"/.test(e)));
  assert.equal(validateManifest(timerManifest({ scope: 'local', permissions: ['chat:commands'] })).ok, true, 'a local plugin may own a command');

  const bad = (chatCommands: unknown) => errorsOf(timerManifest({ contributes: { chatCommands } as PluginManifest['contributes'] }));
  assert.ok(bad([{ name: 'Timer', description: 'x' }]).some((e) => /\.name must match/.test(e)), 'uppercase');
  assert.ok(bad([{ name: '1timer', description: 'x' }]).some((e) => /\.name must match/.test(e)));
  assert.ok(bad([{ name: 't'.repeat(21), description: 'x' }]).some((e) => /\.name must match/.test(e)));
  assert.ok(bad([{ name: 'timer', description: 'x' }, { name: 'timer', description: 'y' }]).some((e) => /duplicate command "\/timer"/.test(e)));
  assert.ok(bad([{ name: 'timer', description: '' }]).some((e) => /description must be 1\.\.60/.test(e)));
  assert.ok(bad([{ name: 'timer', description: 'x'.repeat(61) }]).some((e) => /description must be 1\.\.60/.test(e)));
  assert.ok(bad([{ name: 'timer', description: 'line\nbreak' }]).some((e) => /description must be/.test(e)));
  assert.ok(bad([{ name: 'timer', description: 'pay \u202eevil' }]).some((e) => /description must be/.test(e)), 'bidi override');
  assert.ok(bad([{ name: 'timer', description: 'x', usage: 'u'.repeat(41) }]).some((e) => /usage must be at most 40/.test(e)));
  assert.ok(bad(Array.from({ length: 9 }, (_, i) => ({ name: `c${i}`, description: 'x' }))).some((e) => /at most 8 commands/.test(e)));
});

test('broker: chat.post is permission-gated, normalized, rate-limited, and reaches the adapter with the plugin', async () => {
  const { adapter, calls, clock } = chatAdapter();
  const broker = createPluginBroker({ adapter, hostVersion: '0.9.29', now: clock.now });
  const frame = new FakeFrame();
  broker.attach(loaded(timerManifest()), frame);

  broker.handleMessage({ source: frame, data: req(1, 'chat.post', { text: '  ⏱ Timer started: 5 min \n' }) });
  await tick();
  assert.deepEqual(calls, ['post:petal.timer:⏱ Timer started: 5 min']);
  assert.equal(frame.responses().at(-1)!.ok, true);

  broker.handleMessage({ source: frame, data: req(2, 'chat.post', { text: '' }) });
  broker.handleMessage({ source: frame, data: req(3, 'chat.post', { text: 'x'.repeat(2001) }) });
  broker.handleMessage({ source: frame, data: req(4, 'chat.post', { text: 'spoof \u202e' }) });
  broker.handleMessage({ source: frame, data: req(5, 'chat.post', {}) });
  await tick();
  for (const id of [2, 3, 4, 5]) {
    const res = frame.responses().find((r) => r.id === id)!;
    assert.equal(res.ok, false, `request ${id}`);
    assert.equal(!res.ok && res.error.code, 'invalid', `request ${id}`);
  }

  // Burst of 3 (one already spent), then rate-limited; refills at 0.5/s.
  for (let id = 10; id < 14; id++) broker.handleMessage({ source: frame, data: req(id, 'chat.post', { text: `m${id}` }) });
  await tick();
  // Refusals answer before the posts that await the adapter, so compare by request id.
  const burst = frame.responses().filter((r) => r.id >= 10).sort((a, b) => a.id - b.id);
  assert.deepEqual(burst.map((r) => (r.ok ? 'ok' : r.error.code)), ['ok', 'ok', 'rate-limited', 'rate-limited']);
  clock.advance(2000);
  broker.handleMessage({ source: frame, data: req(20, 'chat.post', { text: 'later' }) });
  await tick();
  assert.equal(frame.responses().at(-1)!.ok, true);

  const denied = new FakeFrame();
  broker.attach(loaded(timerManifest({ id: 'acme.quiet', permissions: ['chat:commands'] })), denied);
  broker.handleMessage({ source: denied, data: req(1, 'chat.post', { text: 'hi' }) });
  await tick();
  const d = denied.responses().at(-1)!;
  assert.equal(!d.ok && d.error.code, 'denied');
  assert.ok(!calls.some((c) => c.startsWith('post:acme.quiet')));
});

test('broker: chat.post without a chat host is unavailable, not internal', async () => {
  const { adapter } = chatAdapter();
  const broker = createPluginBroker({ adapter: { ...adapter, chat: undefined }, hostVersion: '0.9.29' });
  const frame = new FakeFrame();
  broker.attach(loaded(timerManifest()), frame);
  broker.handleMessage({ source: frame, data: req(1, 'chat.post', { text: 'hi' }) });
  await tick();
  const res = frame.responses().at(-1)!;
  assert.equal(!res.ok && res.error.code, 'unavailable');
});

test('broker: a command reaches only the owner\'s logic frame; chat.respond is one answer per run, by the owner, within the window', async () => {
  const { adapter, calls, clock } = chatAdapter();
  const broker = createPluginBroker({ adapter, hostVersion: '0.9.29', now: clock.now });
  const timer = loaded(timerManifest());
  const logic = new FakeFrame();
  const surface = new FakeFrame();
  broker.attach(timer, logic);
  broker.attach(timer, surface, { surface: { id: 's', kind: 'popover' } });
  const other = new FakeFrame();
  broker.attach(loaded(timerManifest({ id: 'acme.other', contributes: { chatCommands: [{ name: 'other', description: 'x' }] } })), other);

  const id1 = broker.deliverChatCommand('petal.timer', { name: 'timer', args: '5m standup', invoker: { identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false } });
  assert.ok(id1);
  const evt = logic.events('chat.command');
  assert.equal(evt.length, 1);
  assert.deepEqual(evt[0]!.payload, { commandId: id1, name: 'timer', args: '5m standup', invoker: null }, 'no meeting:read -> no invoker');
  assert.equal(surface.events('chat.command').length, 0, 'surfaces never see commands');
  assert.equal(other.events('chat.command').length, 0, 'other plugins never see it');

  // Another plugin cannot answer the timer's run; the owner answers once.
  broker.handleMessage({ source: other, data: req(1, 'chat.respond', { commandId: id1, text: 'hijack' }) });
  broker.handleMessage({ source: logic, data: req(2, 'chat.respond', { commandId: id1, text: 'Usage: /timer 5m' }) });
  broker.handleMessage({ source: logic, data: req(3, 'chat.respond', { commandId: id1, text: 'again' }) });
  broker.handleMessage({ source: logic, data: req(4, 'chat.respond', { commandId: 'cmd-999', text: 'made up' }) });
  await tick();
  assert.deepEqual(calls, ['respond:petal.timer:Usage: /timer 5m']);
  const codes = (f: FakeFrame) => f.responses().map((r) => (r.ok ? 'ok' : r.error.code));
  assert.deepEqual(codes(other), ['invalid']);
  assert.deepEqual(codes(logic), ['ok', 'invalid', 'invalid']);

  // The answer window closes.
  const id2 = broker.deliverChatCommand('petal.timer', { name: 'timer', args: '', invoker: null })!;
  clock.advance(CHAT_COMMAND_LIMITS.respondWindowMs + 1);
  broker.handleMessage({ source: logic, data: req(5, 'chat.respond', { commandId: id2, text: 'too late' }) });
  await tick();
  assert.equal(codes(logic).at(-1), 'invalid');

  // meeting:read -> the invoker is included; no chat:commands or no logic frame -> null.
  const reader = new FakeFrame();
  broker.attach(loaded(timerManifest({ id: 'acme.reader', permissions: ['chat:commands', 'meeting:read'] })), reader);
  broker.deliverChatCommand('acme.reader', { name: 'x', args: '', invoker: { identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false } });
  assert.equal((reader.events('chat.command')[0]!.payload as { invoker: { identity: string } }).invoker.identity, 'me');
  const noPerm = new FakeFrame();
  broker.attach(loaded(timerManifest({ id: 'acme.noperm', permissions: ['chat:post'], contributes: undefined })), noPerm);
  assert.equal(broker.deliverChatCommand('acme.noperm', { name: 'x', args: '', invoker: null }), null);
  assert.equal(broker.deliverChatCommand('acme.absent', { name: 'x', args: '', invoker: null }), null);
});

test('resolveChatCommands: one owner per name, built-in before installed before dev, then by id; ungranted plugins own nothing', () => {
  const cmd = (name: string) => ({ chatCommands: [{ name, description: `${name} desc`, usage: 'u' }] });
  const plugins = [
    loaded(timerManifest({ id: 'dev.timer', name: 'Dev Timer', contributes: cmd('timer') }), { source: 'dev' }),
    loaded(timerManifest({ id: 'zeta.timer', name: 'Zeta Timer', contributes: cmd('timer') }), { source: 'registry' }),
    loaded(timerManifest({ id: 'alpha.poll', name: 'Poll', contributes: cmd('poll') }), { source: 'registry' }),
    loaded(timerManifest({ id: 'beta.poll', name: 'Poll 2', contributes: cmd('poll') }), { source: 'registry' }),
    loaded(timerManifest({ id: 'petal.timer', contributes: cmd('timer') }), { source: 'builtin' }),
    loaded(timerManifest({ id: 'acme.ungranted', contributes: cmd('ask') }), { granted: ['chat:post'] }),
  ];
  const { commands, conflicts } = resolveChatCommands(plugins);
  assert.deepEqual(
    commands.map((c) => `/${c.name}:${c.pluginId}:${c.source}`),
    ['/poll:alpha.poll:registry', '/timer:petal.timer:builtin'],
  );
  assert.deepEqual(commands[1], { name: 'timer', usage: 'u', description: 'timer desc', pluginId: 'petal.timer', pluginName: 'Timer', source: 'builtin' });
  assert.deepEqual(
    conflicts.map((c) => `${c.name}:${c.winner}>${c.losers.join(',')}`).sort(),
    ['poll:alpha.poll>beta.poll', 'timer:petal.timer>zeta.timer,dev.timer'],
  );
});

test('the vendored Timer built-in validates on this host and owns /timer', () => {
  const text = readFileSync(new URL('../../plugins/builtins/petal.timer/bundle.json', import.meta.url), 'utf8');
  const parsed = parseBundle(text);
  assert.ok(parsed.ok, parsed.ok ? '' : parsed.error);
  if (!parsed.ok) return;
  const m = parsed.bundle.manifest;
  assert.equal(m.id, 'petal.timer');
  assert.deepEqual(m.permissions, ['chat:commands', 'chat:post']);
  assert.equal(m.scope, 'meeting');
  assert.deepEqual(m.contributes?.chatCommands?.map((c) => c.name), ['timer']);
  assert.match(parsed.bundle.source, /petal\.chat\.onCommand\('timer'/);
  assert.doesNotMatch(parsed.bundle.source, /chat\.on\(|onMessage/, 'it never reads chat');
});
