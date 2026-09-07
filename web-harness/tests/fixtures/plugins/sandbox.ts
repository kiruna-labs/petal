// Rendered-test fixture: boots the REAL shared plugin host in a real browser
// with the hello fixture plugin and exposes what happened on `window.__probe`
// so the test can assert (a) the plugin activated, (b) its toast reached the
// host, (c) the sandbox leaked no Tauri/host globals, (d) a toolbar button
// was drawn and works, (e) the reactions popover opens and a pick reaches
// the overlay frame.
import { createPluginHost } from '../../../shared/plugin-host/host.ts';
import { validateManifest } from '../../../shared/plugin-host/manifest.ts';
import type { PluginHostAdapter } from '../../../shared/plugin-host/host.ts';
import type { ToolbarButtonModel } from '../../../shared/plugin-host/surfaces.ts';
import helloManifestText from './hello/manifest.json?raw';
import helloSource from './hello/plugin.js?raw';
import reactionsManifestText from '../../../plugins/reactions/manifest.json?raw';
import reactionsSource from '../../../plugins/reactions/plugin.js?raw';

const probe = {
  toasts: [] as string[],
  logs: [] as string[],
  frameEvents: [] as string[],
  buttons: [] as ToolbarButtonModel[],
  publishes: [] as string[],
  errors: [] as string[],
};
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
  async setState() {},
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

for (const [text, source] of [
  [helloManifestText, helloSource],
  [reactionsManifestText, reactionsSource],
] as const) {
  const v = validateManifest(JSON.parse(text));
  if (!v.ok) {
    probe.errors.push(v.errors.join('; '));
    continue;
  }
  host.load({ manifest: v.manifest, granted: v.manifest.permissions, source: 'builtin' }, source);
}
(window as unknown as { __host: typeof host }).__host = host;
document.body.dataset.ready = 'true';
