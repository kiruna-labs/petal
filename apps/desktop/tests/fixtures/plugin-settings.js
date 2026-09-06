import { mount } from 'svelte';
import PluginSettingsRows from '../../src/lib/plugins/PluginSettingsRows.svelte';

const manifest = (over) => ({
  manifestVersion: 1,
  id: 'petal.reactions',
  version: '1.0.0',
  name: 'Reactions',
  description: 'React with an emoji that floats up over the meeting for everyone.',
  apiVersion: 1,
  minHostVersion: '0.9.7',
  scope: 'meeting',
  entry: 'plugin.js',
  permissions: ['meeting:read', 'data:publish', 'ui:toolbar-button', 'ui:popover', 'ui:overlay'],
  ...over,
});

const store = new Map();
const storage = {
  getItem: (k) => store.get(k) ?? null,
  setItem: (k, v) => store.set(k, v),
  removeItem: (k) => store.delete(k),
};
window.__pluginSettingsStorage = store;

mount(PluginSettingsRows, {
  target: document.querySelector('#app'),
  props: {
    storage,
    installed: [
      { manifest: manifest(), source: 'builtin', enabledByDefault: true, source_js: '' },
      {
        // Worst case the manifest validator allows: 24-char name, 140-char description, long permission list.
        manifest: manifest({
          id: 'acme.webhook-notifier-pro',
          name: 'Webhook Notifier Deluxe',
          version: '12.34.56',
          scope: 'local',
          description:
            'Posts a message to a web address you choose whenever a meeting starts, ends, or somebody joins, so your team chat stays in sync.',
          permissions: ['meeting:read', 'storage', 'net:fetch:user-urls', 'net:fetch:hooks.slack.com', 'ui:settings', 'ui:toast', 'clipboard:write'],
        }),
        source: 'registry',
        enabledByDefault: false,
        source_js: '',
      },
    ],
  },
});
document.body.dataset.ready = 'true';
