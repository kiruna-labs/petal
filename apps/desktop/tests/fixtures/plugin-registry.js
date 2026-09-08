import { mount } from 'svelte';
import { mockIPC } from '@tauri-apps/api/mocks';
import PluginRegistryBrowser from '../../src/lib/plugins/PluginRegistryBrowser.svelte';
import indexText from '../../../../contracts/plugin-registry/index.json?raw';

// The real contract fixture index, served as the Rust command would return it.
const index = JSON.parse(indexText);
window.__calls = [];
window.__TAURI_INTERNALS__ = window.__TAURI_INTERNALS__ || {}; // hasTauriBridge()
mockIPC((command, payload = {}) => {
  window.__calls.push({ command, payload });
  switch (command) {
    case 'plugin_registry_status':
      return { configured: true, url: 'https://plugins.example.test' };
    case 'plugin_registry_index':
      return { index, trustedComment: 'test', registryUrl: 'https://plugins.example.test' };
    case 'plugin_install_from_registry':
      if (payload.pluginId === 'petal.test-hello') {
        return { version: payload.version, enabled: true, source: 'registry', grantedPermissions: [], installedAtMs: 1 };
      }
      throw new Error('registry refused');
    default:
      return undefined;
  }
});

window.__installed = [];
mount(PluginRegistryBrowser, {
  target: document.querySelector('#app'),
  props: {
    installed: {},
    hostVersion: '9.9.9',
    onInstalled: (id, version) => window.__installed.push(`${id}@${version}`),
  },
});
document.body.dataset.ready = 'true';
