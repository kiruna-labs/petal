import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createPluginBroker, type HostAdapter, type LoadedPlugin } from '@petal/shared/plugin-host/broker';
import type { PluginManifest } from '@petal/shared/plugin-host/manifest';
import { PROTOCOL_VERSION, type Envelope } from '@petal/shared/plugin-host/protocol';

class FakeFrame {
  sent: Envelope[] = [];
  postMessage(message: unknown): void {
    this.sent.push(message as Envelope);
  }
  last(): Envelope {
    return this.sent[this.sent.length - 1]!;
  }
  responses(): Extract<Envelope, { kind: 'res' }>[] {
    return this.sent.filter((e): e is Extract<Envelope, { kind: 'res' }> => e.kind === 'res');
  }
}

const manifest: PluginManifest = {
  manifestVersion: 1,
  id: 'petal.reactions',
  version: '1.0.0',
  name: 'Reactions',
  description: '',
  apiVersion: 1,
  minHostVersion: '0.1.0',
  scope: 'meeting',
  entry: 'plugin.js',
  permissions: ['meeting:read', 'data:publish', 'ui:toolbar-button', 'ui:popover', 'ui:toast'],
  contributes: {
    toolbarButtons: [{ id: 'react', label: 'React', icon: 'smile', opens: 'popover:picker' }],
    surfaces: { popover: { id: 'picker' } },
  },
};

function makeAdapter() {
  const calls: string[] = [];
  const warns: string[] = [];
  const adapter: HostAdapter = {
    meeting: {
      self: () => ({ identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false }),
      participants: () => [{ identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false }],
      room: () => ({ label: 'Eng sync', phase: 'connected' }),
    },
    stateSnapshot: () => ({ alex: { mood: 'happy' } }),
    shares: () => [{ ownerIdentity: 'alex', windowId: 'w1', title: 'vscode', sourceUrl: null, kind: 'window' }],
    async publishData(plugin, params) {
      calls.push(`publish:${plugin.manifest.id}:${params.sub}:${params.reliable}:${params.payload.byteLength}`);
    },
    async setState(_plugin, value) {
      calls.push(`state:${JSON.stringify(value)}`);
    },
    storage: {
      async get(_id, key) {
        calls.push(`storage.get:${key}`);
        return 'v';
      },
      async set(_id, key) {
        calls.push(`storage.set:${key}`);
      },
      async delete(_id, key) {
        calls.push(`storage.delete:${key}`);
      },
      async keys() {
        return ['k'];
      },
    },
    ui: {
      setButton: (_id, buttonId, patch) => calls.push(`setButton:${buttonId}:${JSON.stringify(patch)}`),
      openSurface: (_id, s) => calls.push(`open:${s}`),
      closeSurface: (_id, s) => calls.push(`close:${s}`),
      toast: (_id, text) => calls.push(`toast:${text}`),
    },
    async fetch(_plugin, params) {
      calls.push(`fetch:${params.method}:${params.url}`);
      return { status: 200, headers: {}, body: 'ok' };
    },
    async clipboardWriteText(text) {
      calls.push(`clip:${text}`);
    },
    log: (_id, level, args) => calls.push(`log:${level}:${args.join(' ')}`),
    onFrameEvent: (_id, event) => calls.push(`frame:${event}`),
  };
  return { adapter, calls, warns };
}

function plugin(over: Partial<LoadedPlugin> = {}): LoadedPlugin {
  return { manifest, granted: manifest.permissions, source: 'builtin', ...over };
}

function req(id: number, method: string, params: unknown = {}) {
  return { v: PROTOCOL_VERSION, kind: 'req', id, method, params } as const;
}

const tick = () => new Promise((r) => setImmediate(r));

test('attach sends a permission-shaped init', () => {
  const { adapter } = makeAdapter();
  const broker = createPluginBroker({ adapter, hostVersion: '0.10.0' });
  const frame = new FakeFrame();
  broker.attach(plugin(), frame);
  const init = frame.last();
  assert.equal(init.kind, 'evt');
  if (init.kind !== 'evt') return;
  assert.equal(init.event, 'init');
  const p = init.payload as Record<string, unknown>;
  assert.equal(p.pluginId, 'petal.reactions');
  assert.equal(p.hostVersion, '0.10.0');
  assert.deepEqual(p.hostSupports, { native: false, frames: false });
  assert.ok(p.meeting, 'meeting:read grants the meeting snapshot');
  assert.deepEqual(p.state, { alex: { mood: 'happy' } });
  assert.equal(p.shares, null, 'no shares:read -> no shares');
  assert.equal(p.surface, null);

  const limited = new FakeFrame();
  broker.attach(plugin({ granted: ['ui:toast'] }), limited);
  const lp = (limited.last() as { payload: Record<string, unknown> }).payload;
  assert.equal(lp.meeting, null);
  assert.equal(lp.state, null);
});

test('requests round-trip through the adapter; unknown sources are ignored', async () => {
  const { adapter, calls } = makeAdapter();
  const broker = createPluginBroker({ adapter, hostVersion: '0.10.0' });
  const frame = new FakeFrame();
  broker.attach(plugin(), frame);

  assert.equal(broker.handleMessage({ source: new FakeFrame(), data: req(1, 'log') }), false);
  assert.equal(broker.handleMessage({ source: frame, data: req(1, 'data.publish', { sub: 'emoji', payload: new Uint8Array([1, 2]), reliable: false }) }), true);
  await tick();
  assert.deepEqual(calls, ['publish:petal.reactions:emoji:false:2']);
  assert.deepEqual(frame.responses().at(-1), { v: 1, kind: 'res', id: 1, ok: true, result: undefined });

  broker.handleMessage({ source: frame, data: req(2, 'ui.setButton', { buttonId: 'react', patch: { badge: 3, junk: 1 } }) });
  broker.handleMessage({ source: frame, data: req(3, 'ui.openSurface', { surfaceId: 'picker' }) });
  broker.handleMessage({ source: frame, data: req(4, 'log', { level: 'info', args: ['hi', 1] }) });
  await tick();
  assert.ok(calls.includes('setButton:react:{"badge":3}'));
  assert.ok(calls.includes('open:picker'));
  assert.ok(calls.includes('log:info:hi 1'));
});

test('denied, invalid and rate-limited paths return typed errors and never reach the adapter', async () => {
  const { adapter, calls } = makeAdapter();
  const warns: string[] = [];
  const broker = createPluginBroker({ adapter, hostVersion: '0.10.0', warn: (m) => warns.push(m) });
  const frame = new FakeFrame();
  broker.attach(plugin({ granted: ['data:publish'] }), frame);

  broker.handleMessage({ source: frame, data: req(1, 'storage.get', { key: 'k' }) }); // no storage permission
  broker.handleMessage({ source: frame, data: req(2, 'storage.get', { key: 'k' }) }); // denied again -> warn once
  broker.handleMessage({ source: frame, data: req(3, 'ui.openSurface', { surfaceId: 'nope' }) }); // undeclared surface
  broker.handleMessage({ source: frame, data: req(4, 'data.publish', { payload: 'not bytes' }) });
  broker.handleMessage({ source: frame, data: req(5, 'data.publish', { payload: new Uint8Array(20000), reliable: true }) });
  broker.handleMessage({ source: frame, data: req(6, 'net.fetch', { url: 'https://evil.com/' }) });
  broker.handleMessage({ source: frame, data: req(7, 'nosuch.method') });
  broker.handleMessage({ source: frame, data: { v: 99, kind: 'req' } });
  await tick();

  const byId = new Map(frame.responses().map((r) => [r.id, r]));
  assert.equal(byId.get(1)!.ok, false);
  assert.equal((byId.get(1) as { error: { code: string } }).error.code, 'denied');
  assert.equal((byId.get(3) as { error: { code: string } }).error.code, 'invalid');
  assert.equal((byId.get(4) as { error: { code: string } }).error.code, 'invalid');
  assert.match((byId.get(5) as { error: { message: string } }).error.message, /exceeds 16384 bytes/);
  assert.equal((byId.get(6) as { error: { code: string } }).error.code, 'denied');
  assert.equal((byId.get(7) as { error: { code: string } }).error.code, 'invalid');
  assert.deepEqual(calls, [], 'adapter never touched');
  assert.equal(warns.filter((w) => /storage.get denied/.test(w)).length, 1, 'denial logged once per method');
  assert.equal(warns.filter((w) => /malformed envelope/.test(w)).length, 1);

  // Reliable quota: 10/s then rate-limited.
  for (let i = 0; i < 12; i++) {
    broker.handleMessage({ source: frame, data: req(100 + i, 'data.publish', { payload: new Uint8Array(1), reliable: true }) });
  }
  await tick();
  const codes = frame.responses().filter((r) => r.id >= 100).map((r) => (r.ok ? 'ok' : r.error.code));
  assert.equal(codes.filter((c) => c === 'ok').length, 10);
  assert.equal(codes.filter((c) => c === 'rate-limited').length, 2);
});

test('events are permission-gated and data only reaches the logic frame of the right plugin', () => {
  const { adapter } = makeAdapter();
  const broker = createPluginBroker({ adapter, hostVersion: '0.10.0' });
  const logic = new FakeFrame();
  const surface = new FakeFrame();
  const other = new FakeFrame();
  const blind = new FakeFrame();
  broker.attach(plugin(), logic);
  broker.attach(plugin(), surface, { surface: { id: 'picker', kind: 'popover' } });
  broker.attach(plugin({ manifest: { ...manifest, id: 'petal.chat' } }), other);
  broker.attach(plugin({ granted: ['ui:toast'] }), blind);

  const sender = { identity: 'alex', name: 'Alex', isLocal: false, speaking: false, micMuted: false };
  broker.deliverData('petal.reactions', { sub: 'emoji', sender, payload: new Uint8Array([1]) });
  broker.broadcast('meeting.phase', { label: 'x', phase: 'connected' });

  const events = (f: FakeFrame) => f.sent.filter((e) => e.kind === 'evt').map((e) => (e as { event: string }).event);
  assert.deepEqual(events(logic), ['init', 'data.message', 'meeting.phase']);
  assert.deepEqual(events(surface), ['init', 'meeting.phase'], 'surfaces never get raw data');
  assert.deepEqual(events(other), ['init', 'meeting.phase'], 'other plugin never sees our topic');
  assert.deepEqual(events(blind), ['init'], 'no meeting:read -> no meeting events, no data:publish -> no data');

  broker.detachPlugin('petal.reactions');
  assert.deepEqual(broker.pluginIds(), ['petal.chat']);
  assert.equal(broker.handleMessage({ source: logic, data: req(1, 'log') }), false);
});

test('frame lifecycle events reach the adapter', () => {
  const { adapter, calls } = makeAdapter();
  const broker = createPluginBroker({ adapter, hostVersion: '0.10.0' });
  const frame = new FakeFrame();
  broker.attach(plugin(), frame);
  broker.handleMessage({ source: frame, data: { v: 1, kind: 'evt', event: 'ready', payload: {} } });
  broker.handleMessage({ source: frame, data: { v: 1, kind: 'evt', event: 'error', payload: { message: 'boom' } } });
  assert.deepEqual(calls, ['frame:ready', 'frame:error']);
});
