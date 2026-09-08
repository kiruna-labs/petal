// Rendered-test fixture for #37: boots the REAL shared plugin host with two
// plugins -- `hello` (well-behaved, the positive control) and `escape`, which
// navigates its own frame to another origin the moment it learns the roster.
//
// Two host pages share this module: selfnav.html (no embedder CSP, so the
// navigation succeeds and host.ts's second-`load` gate is what must stop the
// leak) and selfnav-csp.html (the embedder sets `frame-src 'none'`, the policy
// the desktop app and meet.petal.live ship, so the navigation never happens).
//
// `?leak=<url>` is the attacker origin; the test picks it so the two origins
// really differ.
import { createPluginHost } from '../../../shared/plugin-host/host.ts';
import { validateManifest } from '../../../shared/plugin-host/manifest.ts';
import type { PluginHostAdapter } from '../../../shared/plugin-host/host.ts';
import type { ToolbarButtonModel } from '../../../shared/plugin-host/surfaces.ts';
import helloManifestText from './hello/manifest.json?raw';
import helloSource from './hello/plugin.js?raw';
import escapeManifestText from './escape/manifest.json?raw';
import escapeSource from './escape/plugin.js?raw';

const probe = {
  logs: [] as string[],
  frameEvents: [] as string[],
  /** What the navigated-to page reports: `landed:<query>` and `received:<envelope>`. */
  leaks: [] as string[],
  errors: [] as string[],
};
(window as unknown as { __probe: typeof probe }).__probe = probe;

// The attacker page talks back to this page (it cannot reach anything else).
window.addEventListener('message', (event: MessageEvent) => {
  const data = event.data as { __leak?: unknown; detail?: unknown } | null;
  if (!data || typeof data !== 'object' || typeof data.__leak !== 'string') return;
  probe.leaks.push(`${data.__leak}:${String(data.detail ?? '')}`);
});

const adapter: PluginHostAdapter = {
  meeting: {
    self: () => ({ identity: 'me', name: 'Me Myself', isLocal: true, speaking: false, micMuted: false }),
    participants: () => [{ identity: 'me', name: 'Me Myself', isLocal: true, speaking: false, micMuted: false }],
    room: () => ({ label: 'Self-navigation room', phase: 'connected' }),
  },
  async publishData() {},
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
  log: (id, level, args) => probe.logs.push(`${id}:${level}:${args.join(' ')}`),
  onFrameEvent: (id, event, payload) => {
    probe.frameEvents.push(`${id}:${event}`);
    if (event === 'error') probe.errors.push(JSON.stringify(payload));
  },
};

const host = createPluginHost({
  document,
  adapter,
  hostVersion: '9.9.9',
  mounts: {
    logic: document.getElementById('logic')!,
    overlay: document.getElementById('overlay')!,
    popoverLayer: document.getElementById('popovers')!,
  },
  onButtonsChanged: (_buttons: ToolbarButtonModel[]) => {},
  warn: (m) => probe.logs.push(`warn:${m}`),
});

const leakUrl = new URLSearchParams(window.location.search).get('leak') ?? '';
for (const [text, source] of [
  [helloManifestText, helloSource],
  // replaceAll, not replace: the placeholder is named in the fixture's own
  // comment as well as in its code.
  [escapeManifestText, escapeSource.replaceAll('__PETAL_LEAK_URL__', leakUrl)],
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
