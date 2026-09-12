// Rendered-test fixture: boots the REAL shared plugin host in a real browser
// with the hello fixture plugin and exposes what happened on `window.__probe`
// so the test can assert (a) the plugin activated, (b) its toast reached the
// host, (c) the sandbox leaked no Tauri/host globals, (d) a toolbar button
// was drawn and works, (e) the reactions popover opens and a pick reaches
// the overlay frame.
import { createPluginHost } from '@petal/shared/plugin-host/host.ts';
import { validateManifest } from '@petal/shared/plugin-host/manifest.ts';
import type { PluginHostAdapter } from '@petal/shared/plugin-host/host.ts';
import type { ToolbarButtonModel } from '@petal/shared/plugin-host/surfaces.ts';
import { parseBundle } from '@petal/shared/plugin-host/bundle.ts';
import helloManifestText from './hello/manifest.json?raw';
import helloSource from './hello/plugin.js?raw';
import reactionsBundleText from '../../../plugins/builtins/petal.reactions/bundle.json?raw';

const probe = {
  toasts: [] as string[],
  logs: [] as string[],
  frameEvents: [] as string[],
  buttons: [] as ToolbarButtonModel[],
  publishes: [] as string[],
  errors: [] as string[],
  adverts: [] as string[],
  advertRejections: [] as string[],
  menus: [] as string[],
};
// Simulates the budget only the CLIENT side can enforce (the 8 KB `plugins`
// total in Rust / mergePluginMetadata): reject any state-bearing entry while
// set. The host must roll its cached advert back so a later readvertise
// republishes the last ACCEPTED entry, not the rejected one.
const control = { rejectStateAdverts: true };
(window as unknown as { __control: typeof control }).__control = control;
(window as unknown as { __probe: typeof probe }).__probe = probe;

const adapter: PluginHostAdapter = {
  meeting: {
    self: () => ({ identity: 'me', name: 'Me Myself', isLocal: true, speaking: false, micMuted: false }),
    participants: () => [{ identity: 'me', name: 'Me Myself', isLocal: true, speaking: false, micMuted: false }],
    room: () => ({ label: 'Sandbox room', phase: 'connected' }),
  },
  async publishData(plugin, params) {
    probe.publishes.push(`${plugin.manifest.id}:${params.sub}:${new TextDecoder().decode(params.payload)}`);
  },
  async publishPluginEntry(pluginId, entry) {
    probe.adverts.push(`${pluginId}=${JSON.stringify(entry)}`);
    if (control.rejectStateAdverts && entry && entry.state !== undefined) {
      probe.advertRejections.push(`${pluginId}=${JSON.stringify(entry)}`);
      throw Object.assign(new Error('plugins metadata exceeds 8192 bytes'), { code: 'invalid' });
    }
  },
  storage: {
    async get() {
      return undefined;
    },
    async set() {},
    async delete() {},
    async keys() {
      return [];
    },
  },
  toast: (_id, text) => probe.toasts.push(text),
  async fetch() {
    throw Object.assign(new Error('no'), { code: 'unavailable' });
  },
  async clipboardWriteText() {},
  log: (id, level, args) => probe.logs.push(`${id}:${level}:${args.join(' ')}`),
  onFrameEvent: (id, event, payload) => {
    probe.frameEvents.push(`${id}:${event}`);
    if (event === 'error') probe.errors.push(JSON.stringify(payload));
  },
};

const controls = document.getElementById('controls')!;
const host = createPluginHost({
  document,
  adapter,
  hostVersion: '9.9.9',
  mounts: {
    logic: document.getElementById('logic')!,
    overlay: document.getElementById('overlay')!,
    popoverLayer: document.getElementById('popovers')!,
  },
  onPluginMenu(pluginId, at) {
    probe.menus.push(`${pluginId}@${Math.round(at.x)},${Math.round(at.y)}`);
  },
  onButtonsChanged(buttons) {
    probe.buttons = buttons;
    controls.innerHTML = '';
    for (const b of buttons) {
      const btn = document.createElement('button');
      btn.id = `btn-${b.pluginId}-${b.buttonId}`;
      btn.textContent = b.label;
      btn.addEventListener('click', () => host.activateButton(b.pluginId, b.buttonId, btn));
      controls.appendChild(btn);
    }
  },
  warn: (m) => probe.logs.push(`warn:${m}`),
});

const hello = validateManifest(JSON.parse(helloManifestText));
if (hello.ok) host.load({ manifest: hello.manifest, granted: hello.manifest.permissions, source: 'builtin' }, helloSource);
else probe.errors.push(hello.errors.join('; '));
// The real vendored built-in, exactly as the clients load it.
const reactions = parseBundle(reactionsBundleText);
if (reactions.ok) host.load({ manifest: reactions.bundle.manifest, granted: reactions.bundle.manifest.permissions, source: 'builtin' }, reactions.bundle.source);
else probe.errors.push(reactions.error);
(window as unknown as { __host: typeof host }).__host = host;
document.body.dataset.ready = 'true';
