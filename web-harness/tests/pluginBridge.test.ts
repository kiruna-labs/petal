import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createPluginBroker, toCloneable, type HostAdapter, type LoadedPlugin } from '@petal/shared/plugin-host/broker';
import type { PluginManifest } from '@petal/shared/plugin-host/manifest';
import { PROTOCOL_VERSION, type Envelope } from '@petal/shared/plugin-host/protocol';

class FakeFrame {
  sent: Envelope[] = [];
  postMessage(message: unknown, _origin?: string, transfer?: Transferable[]): void {
    // Behave like the real thing: structured-clone the payload (throws DataCloneError on proxies/functions).
    this.sent.push(structuredClone(message, transfer ? { transfer } : undefined) as Envelope);
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
  broker.handleMessage({ source: frame, data: req(5, 'ui.setButton', { buttonId: 'react', patch: { label: 'Fourteen chars' } }) });
  await tick();
  assert.ok(calls.includes('setButton:react:{"badge":3}'));
  assert.ok(calls.includes('setButton:react:{"label":"Fourteen chars"}'), 'a label at the 14-char limit passes through whole');
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
  // A SharedArrayBuffer-backed view is rejected at the boundary rather than
  // handed to a transport that only accepts ArrayBuffer-backed bytes.
  broker.handleMessage({ source: frame, data: req(8, 'data.publish', { payload: new Uint8Array(new SharedArrayBuffer(4)), reliable: true }) });
  broker.handleMessage({ source: frame, data: req(6, 'net.fetch', { url: 'https://evil.com/' }) });
  broker.handleMessage({ source: frame, data: req(7, 'nosuch.method') });
  broker.handleMessage({ source: frame, data: { v: 99, kind: 'req' } });
  await tick();
  {
    // UI text must never truncate: an over-long (or empty) setButton label is
    // rejected as invalid; the host never clips it and the adapter never sees it.
    const { adapter: uiAdapter, calls: uiCalls } = makeAdapter();
    const uiBroker = createPluginBroker({ adapter: uiAdapter, hostVersion: '0.10.0' });
    const uiFrame = new FakeFrame();
    uiBroker.attach(plugin(), uiFrame);
    uiBroker.handleMessage({ source: uiFrame, data: req(1, 'ui.setButton', { buttonId: 'react', patch: { label: 'Fifteen chars!!' } }) });
    uiBroker.handleMessage({ source: uiFrame, data: req(2, 'ui.setButton', { buttonId: 'react', patch: { label: '' } }) });
    await tick();
    const codes = uiFrame.responses().map((r) => (r.ok ? 'ok' : r.error.code));
    assert.deepEqual(codes, ['invalid', 'invalid']);
    assert.deepEqual(uiCalls.filter((c) => c.startsWith('setButton')), []);
  }
  {
    // #37: a patch is the one path an icon reaches host chrome without going
    // through validateManifest, and it used to take any string. Same rule as
    // the manifest (isIconName), and REFUSED rather than substituted.
    const { adapter: iconAdapter, calls: iconCalls } = makeAdapter();
    const iconBroker = createPluginBroker({ adapter: iconAdapter, hostVersion: '0.10.0' });
    const iconFrame = new FakeFrame();
    iconBroker.attach(plugin(), iconFrame);
    for (const [id, icon] of [
      [1, '<svg onload=alert(1)>'],
      [2, 'constructor'.toUpperCase()],
      [3, '__proto__'],
      [4, ''],
      [5, 42],
    ] as const) {
      iconBroker.handleMessage({ source: iconFrame, data: req(id, 'ui.setButton', { buttonId: 'react', patch: { icon } }) });
    }
    iconBroker.handleMessage({ source: iconFrame, data: req(6, 'ui.setButton', { buttonId: 'react', patch: { icon: 'smile' } }) });
    await tick();
    assert.deepEqual(
      iconFrame.responses().map((r) => (r.ok ? 'ok' : r.error.code)),
      ['invalid', 'invalid', 'invalid', 'invalid', 'invalid', 'ok'],
    );
    assert.deepEqual(iconCalls.filter((c) => c.startsWith('setButton')), ['setButton:react:{"icon":"smile"}']);
  }

  const byId = new Map(frame.responses().map((r) => [r.id, r]));
  assert.equal(byId.get(1)!.ok, false);
  assert.equal((byId.get(1) as { error: { code: string } }).error.code, 'denied');
  assert.equal((byId.get(3) as { error: { code: string } }).error.code, 'invalid');
  assert.equal((byId.get(4) as { error: { code: string } }).error.code, 'invalid');
  assert.match((byId.get(5) as { error: { message: string } }).error.message, /exceeds 16384 bytes/);
  assert.equal((byId.get(6) as { error: { code: string } }).error.code, 'denied');
  assert.equal((byId.get(7) as { error: { code: string } }).error.code, 'invalid');
  assert.equal((byId.get(8) as { error: { code: string } }).error.code, 'invalid');
  assert.match((byId.get(8) as { error: { message: string } }).error.message, /SharedArrayBuffer/);
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

test('payloads from a reactive proxy still reach the frame (DataCloneError seen live on desktop)', () => {
  const { adapter } = makeAdapter();
  // Svelte 5 $state hands out Proxies; structuredClone rejects them.
  const proxied = new Proxy([{ identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false }], {});
  assert.throws(() => structuredClone(proxied), 'precondition: a bare Proxy is not cloneable');
  adapter.meeting!.participants = () => proxied;
  const warns: string[] = [];
  const broker = createPluginBroker({ adapter, hostVersion: '0.10.0', warn: (m) => warns.push(m) });
  const frame = new FakeFrame();
  broker.attach(plugin(), frame);
  assert.deepEqual(warns, []);
  const init = frame.last() as { event: string; payload: { meeting: { participants: unknown[] } } };
  assert.equal(init.event, 'init');
  assert.equal(init.payload.meeting.participants.length, 1);
  broker.broadcast('meeting.participant-joined', new Proxy({ identity: 'alex' }, {}));
  assert.equal((frame.last() as { event: string }).event, 'meeting.participant-joined');
  assert.deepEqual(warns, []);
});

test('toCloneable copies an own __proto__ key without touching the copy\'s prototype', () => {
  // JSON.parse produces an own, enumerable `__proto__`; plain assignment would
  // hit Object.prototype's setter, silently dropping the key and re-pointing
  // the copy's prototype. structuredClone keeps it as data, so we must too.
  const source = JSON.parse('{"a":1,"__proto__":{"polluted":true}}') as Record<string, unknown>;
  const out = toCloneable(source) as Record<string, unknown>;
  assert.deepEqual(Object.keys(out).sort(), ['__proto__', 'a']);
  assert.equal(Object.getPrototypeOf(out), Object.prototype);
  assert.equal((Object.prototype as Record<string, unknown>).polluted, undefined);
  assert.deepEqual(out.__proto__, { polluted: true });
});

test('toCloneable keeps binary payloads intact and drops functions', () => {
  const bytes = new Uint8Array([1, 2, 3]);
  const out = toCloneable({ a: bytes, f: () => 1, n: 1, nested: new Proxy({ x: [1, { y: 2 }] }, {}) }) as Record<string, unknown>;
  assert.equal(out.a, bytes, 'same Uint8Array instance');
  assert.ok(!('f' in out));
  assert.deepEqual(out.nested, { x: [1, { y: 2 }] });
});
