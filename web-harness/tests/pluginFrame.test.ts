import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';

import { PLUGIN_FRAME_CSP, PLUGIN_FRAME_SANDBOX, buildPluginFrameSrcdoc, escapeInlineScript } from '@petal/shared/plugin-host/frame';
import { FRAME_RUNTIME_SOURCE } from '@petal/shared/plugin-host/frameRuntime';
import { PROTOCOL_VERSION } from '@petal/shared/plugin-host/protocol';

test('srcdoc carries the locked-down CSP, the runtime, then the plugin module', () => {
  const doc = buildPluginFrameSrcdoc({ pluginId: 'petal.reactions', source: 'console.log("x</script><!--")', runtime: 'RUNTIME' });
  assert.ok(doc.includes(`content="${PLUGIN_FRAME_CSP}"`));
  assert.match(PLUGIN_FRAME_CSP, /connect-src 'none'/);
  assert.equal(PLUGIN_FRAME_SANDBOX, 'allow-scripts');
  assert.ok(doc.indexOf('<script>RUNTIME</script>') < doc.indexOf('<script type="module">'));
  assert.ok(!doc.includes('x</script>'), 'closing tag inside plugin source is neutralised');
  assert.ok(doc.includes('x<\\/script><\\!--'));
  assert.equal(escapeInlineScript('a</SCRIPT>b'), 'a<\\/SCRIPT>b');
});

/**
 * Run the frame runtime under node:vm with a fake window whose parent is a
 * fake host. Proves the bootstrap sequence (init -> ready -> activate), the
 * request/response round trip, snapshot maintenance, and that messages not
 * from the parent are ignored.
 */
function bootFrame() {
  const hostInbox: Array<{ env: Record<string, unknown>; transfer: unknown[] }> = [];
  const listeners: Array<(ev: unknown) => void> = [];
  const parent = { postMessage: (env: Record<string, unknown>, _origin: string, transfer: unknown[] = []) => hostInbox.push({ env, transfer }) };
  const win: Record<string, unknown> = {
    parent,
    addEventListener: (_type: string, cb: (ev: unknown) => void) => listeners.push(cb),
  };
  const sandbox: Record<string, unknown> = {
    window: win,
    document: { body: { tag: 'body' } },
    TextEncoder,
    TextDecoder,
    Uint8Array,
    Promise,
    Map,
    Set,
    Array,
    JSON,
    Error,
    String,
    Object,
    console,
  };
  vm.createContext(sandbox);
  vm.runInContext(FRAME_RUNTIME_SOURCE, sandbox);
  const deliver = (data: unknown, source: unknown = parent, ports: unknown[] = []) => {
    for (const cb of listeners) cb({ source, data, origin: 'tauri://localhost', ports });
  };
  return { win, hostInbox, deliver, parent };
}

test('frame runtime: init -> ready -> activate, then requests and events flow', async () => {
  const { win, hostInbox, deliver } = bootFrame();
  const seen: string[] = [];
  let petalRef: any;
  (win as any).__petalRegister({
    activate(petal: any) {
      petalRef = petal;
      seen.push(`activate:${petal.plugin.id}:${petal.meeting.participants().length}`);
      petal.meeting.on('participant-joined', (p: any) => seen.push(`joined:${p.identity}`));
      petal.data.on('emoji', (m: any) => seen.push(`emoji:${m.sender.identity}:${m.json().e}`));
      petal.data.on('other', () => seen.push('WRONG SUB'));
    },
  });
  assert.equal(hostInbox.length, 0, 'nothing leaves the frame before init');

  deliver({ v: PROTOCOL_VERSION, kind: 'evt', event: 'init', payload: {
    pluginId: 'petal.reactions', version: '1.0.0', apiVersion: 1, scope: 'meeting', grantedPermissions: ['meeting:read'],
    hostVersion: '0.10.0', hostSupports: { native: false, frames: false },
    meeting: { self: { identity: 'me' }, participants: [{ identity: 'me' }], room: { label: 'r', phase: 'connected' } },
    state: null, shares: null, surface: null,
  } });
  await new Promise((r) => setImmediate(r));
  assert.equal(hostInbox[0]!.env.event, 'ready');
  assert.equal(hostInbox[1]!.env.event, 'activated');
  assert.deepEqual(seen, ['activate:petal.reactions:1']);

  // Not from the parent -> ignored.
  deliver({ v: PROTOCOL_VERSION, kind: 'evt', event: 'meeting.participant-joined', payload: { identity: 'spoof' } }, {});
  deliver({ v: PROTOCOL_VERSION, kind: 'evt', event: 'meeting.participant-joined', payload: { identity: 'alex' } });
  assert.deepEqual(seen.slice(1), ['joined:alex']);
  assert.equal(petalRef.meeting.participants().length, 2);

  const bytes = new TextEncoder().encode(JSON.stringify({ e: '👍' }));
  deliver({ v: PROTOCOL_VERSION, kind: 'evt', event: 'data.message', payload: { sub: 'emoji', sender: { identity: 'alex' }, payload: bytes } });
  assert.equal(seen.at(-1), 'emoji:alex:👍');

  // Request/response round trip.
  const pending = petalRef.storage.get('k');
  const reqEnv = hostInbox.at(-1)!.env;
  assert.equal(reqEnv.kind, 'req');
  assert.equal(reqEnv.method, 'storage.get');
  deliver({ v: PROTOCOL_VERSION, kind: 'res', id: reqEnv.id, ok: true, result: 'value' });
  assert.equal(await pending, 'value');

  const failing = petalRef.ui.toast('x');
  const failEnv = hostInbox.at(-1)!.env;
  deliver({ v: PROTOCOL_VERSION, kind: 'res', id: failEnv.id, ok: false, error: { code: 'denied', message: 'no' } });
  await assert.rejects(failing, (e: any) => e.code === 'denied');

  // Objects publish as JSON bytes; Uint8Array passes through.
  void petalRef.data.publish('emoji', { e: 'x' }, { reliable: false });
  const pub = hostInbox.at(-1)!.env as any;
  assert.equal(pub.method, 'data.publish');
  assert.ok(pub.params.payload instanceof Uint8Array);
  assert.equal(pub.params.reliable, false);
});

test('frame runtime: a surface frame mounts instead of activating', async () => {
  const { win, hostInbox, deliver } = bootFrame();
  const seen: string[] = [];
  (win as any).__petalRegister({
    activate() {
      seen.push('WRONG activate');
    },
    mountSurface(_petal: any, surface: any) {
      seen.push(`mount:${surface.kind}:${surface.id}:${surface.root.tag}`);
      surface.channel.postMessage({ hello: 1 });
    },
  });
  const port = { posted: [] as unknown[], postMessage(m: unknown) { this.posted.push(m); }, onmessage: null, close() {} };
  deliver({ v: PROTOCOL_VERSION, kind: 'evt', event: 'init', payload: {
    pluginId: 'p.x', version: '1.0.0', apiVersion: 1, scope: 'local', grantedPermissions: [], hostVersion: '0.10.0',
    hostSupports: { native: false, frames: false }, meeting: null, state: null, shares: null, surface: { id: 'picker', kind: 'popover' },
  } }, undefined, [port]);
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(seen, ['mount:popover:picker:body']);
  assert.deepEqual(port.posted, [{ hello: 1 }], 'channel messages reach the transferred port');
  assert.equal(hostInbox.at(-1)!.env.event, 'activated');
});

test('frame runtime: activate errors are reported, not swallowed', async () => {
  const { win, hostInbox, deliver } = bootFrame();
  (win as any).__petalRegister({ activate() { throw new Error('kaboom'); } });
  deliver({ v: PROTOCOL_VERSION, kind: 'evt', event: 'init', payload: { pluginId: 'p.x', version: '1.0.0', apiVersion: 1, scope: 'local', grantedPermissions: [], hostVersion: '0.10.0', hostSupports: {}, meeting: null, state: null, shares: null, surface: null } });
  await new Promise((r) => setImmediate(r));
  const err = hostInbox.at(-1)!.env as any;
  assert.equal(err.event, 'error');
  assert.equal(err.payload.message, 'kaboom');
});
