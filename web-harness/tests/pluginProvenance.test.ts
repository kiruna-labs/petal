import { test } from 'node:test';
import assert from 'node:assert/strict';

import { MANIFEST_LIMITS } from '@petal/shared/plugin-host/manifest';
import { PLUGIN_LIMITS } from '@petal/shared/plugin-host/rateLimit';
import {
  LONGEST_NAME_TOAST_LENGTH,
  PLUGIN_MENU_DISABLE,
  pluginCaption,
  pluginDisabledToast,
  pluginMenuModel,
  pluginProvenanceTitle,
} from '@petal/shared/plugin-host/provenance';
import { toolbarButtonModels } from '@petal/shared/plugin-host/surfaces';
import type { LoadedPlugin } from '@petal/shared/plugin-host/broker';

test('provenance copy names the plugin and the menu offers exactly "turn off"', () => {
  assert.equal(pluginProvenanceTitle('Reactions'), 'Reactions plugin');
  assert.equal(pluginCaption('Reactions'), 'Reactions · plugin');
  const menu = pluginMenuModel('Reactions');
  assert.equal(menu.heading, 'Reactions · plugin');
  assert.deepEqual(menu.items, [{ id: PLUGIN_MENU_DISABLE, label: 'Turn off Reactions' }]);
});

test('the disabled toast fits the toast budget for every valid manifest name', () => {
  const longest = 'x'.repeat(MANIFEST_LIMITS.nameMaxLength);
  assert.ok(pluginDisabledToast(longest).length <= PLUGIN_LIMITS.toastMaxChars);
  assert.equal(LONGEST_NAME_TOAST_LENGTH, pluginDisabledToast(longest).length);
  assert.match(pluginDisabledToast('Reactions'), /Reactions is off\. Turn it back on in Settings → Plugins\./);
});

test('toolbar button models carry the plugin name for the badge tooltip', () => {
  const plugin: LoadedPlugin = {
    manifest: {
      manifestVersion: 1,
      id: 'petal.reactions',
      version: '1.0.0',
      name: 'Reactions',
      description: '',
      apiVersion: 1,
      minHostVersion: '0.1.0',
      scope: 'meeting',
      entry: 'plugin.js',
      permissions: ['ui:toolbar-button'],
      contributes: { toolbarButtons: [{ id: 'react', label: 'React', icon: 'smile' }] },
    },
    granted: ['ui:toolbar-button'],
    source: 'builtin',
  };
  assert.equal(toolbarButtonModels([plugin], new Map())[0]!.pluginName, 'Reactions');
});
