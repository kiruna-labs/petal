// Rendered-test fixture for plugin PROVENANCE chrome: boots the REAL shared
// plugin host, with the REAL stylesheet and the REAL UI font, against the
// worst case `validateManifest` still accepts -- the longest allowed plugin
// name (24 chars) in a popover that declares a hostile width. The host's own
// "· plugin" caption must stay fully visible there, so the test measures the
// rendered box instead of reading CSS (kiruna-labs/petal#71 review, finding 1).
import '../../../src/fonts.css';
import '../../../../shared/ui/tokens.css';
import '../../../../shared/ui/plugin-provenance.css';
import { createPluginHost } from '../../../../shared/plugin-host/host.ts';
import type { PluginHostAdapter } from '../../../../shared/plugin-host/host.ts';
import type { LoadedPlugin, PluginSource } from '../../../../shared/plugin-host/broker.ts';
import { MANIFEST_LIMITS, validateManifest } from '../../../../shared/plugin-host/manifest.ts';

/** A plugin that does nothing: this fixture only measures HOST-drawn chrome. */
const INERT_SOURCE = 'export default { activate() {} };';

const probe = { menus: [] as string[], errors: [] as string[] };
(window as unknown as { __probe: typeof probe }).__probe = probe;

const adapter: PluginHostAdapter = {
  meeting: {
    self: () => ({ identity: 'me', name: 'Me', isLocal: true, speaking: false, micMuted: false }),
    participants: () => [],
    room: () => ({ label: 'Provenance room', phase: 'connected' }),
  },
  async publishData() {},
  // Required by PluginHostAdapter; this fixture measures host-drawn chrome
  // only and joins no room, so advertising is a no-op here.
  async publishPluginEntry() {},
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
  toast: () => {},
  async fetch() {
    throw Object.assign(new Error('no'), { code: 'unavailable' });
  },
  async clipboardWriteText() {},
  log: () => {},
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
  onPluginMenu: (pluginId, at) => probe.menus.push(`${pluginId}@${Math.round(at.x)},${Math.round(at.y)}`),
  onButtonsChanged(buttons) {
    controls.innerHTML = '';
    for (const b of buttons) {
      const btn = document.createElement('button');
      btn.id = `btn-${b.pluginId}-${b.buttonId}`;
      btn.textContent = b.label;
      btn.addEventListener('click', () => host.activateButton(b.pluginId, b.buttonId, btn));
      controls.appendChild(btn);
    }
  },
  warn: (m) => probe.errors.push(m),
});

function manifest(id: string, name: string, width: number): Record<string, unknown> {
  return {
    manifestVersion: 1,
    id,
    version: '1.0.0',
    name,
    description: '',
    apiVersion: 1,
    minHostVersion: '0.1.0',
    scope: 'local',
    entry: 'plugin.js',
    permissions: ['ui:toolbar-button', 'ui:popover'],
    contributes: {
      toolbarButtons: [{ id: 'open', label: 'Open', icon: 'smile', opens: 'popover:p' }],
      surfaces: { popover: { id: 'p', width, height: 64 } },
    },
  };
}

// `validateManifest` accepts ANY positive width, so 1 is the adversarial floor.
const cases: { id: string; name: string; width: number; source: PluginSource }[] = [
  { id: 'petal.reactions', name: 'Reactions', width: 296, source: 'builtin' },
  { id: 'acme.hostile', name: 'W'.repeat(MANIFEST_LIMITS.nameMaxLength), width: 1, source: 'registry' },
  { id: 'dev.local-plugin', name: 'Local Build', width: 100, source: 'dev' },
];
for (const c of cases) {
  const v = validateManifest(manifest(c.id, c.name, c.width));
  if (!v.ok) {
    probe.errors.push(`${c.id}: ${v.errors.join('; ')}`);
    continue;
  }
  const plugin: LoadedPlugin = { manifest: v.manifest, granted: v.manifest.permissions, source: c.source };
  host.load(plugin, INERT_SOURCE);
}
(window as unknown as { __host: typeof host }).__host = host;
document.body.dataset.ready = 'true';
