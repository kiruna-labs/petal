import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { LoadedPlugin } from '@petal/shared/plugin-host/broker';
import type { PluginManifest } from '@petal/shared/plugin-host/manifest';
import { badgeText, buttonKey, fitButtonLabel, placePopover, toolbarButtonModels } from '@petal/shared/plugin-host/surfaces';
import {
  PLUGIN_ENABLED_STORAGE_KEY,
  PLUGIN_KV_STORAGE_PREFIX,
  clearPluginStorage,
  isPluginEnabled,
  permissionLabel,
  pluginSettingsRows,
  readEnabledOverrides,
  writeEnabledOverride,
} from '@petal/shared/plugin-host/settingsModel';
import { PLUGIN_ICON_NAMES, pluginIconSvg } from '@petal/shared/plugin-host/icons';

const manifest: PluginManifest = {
  manifestVersion: 1,
  id: 'petal.reactions',
  version: '1.2.0',
  name: 'Reactions',
  description: 'Emoji.',
  apiVersion: 1,
  minHostVersion: '0.1.0',
  scope: 'meeting',
  entry: 'plugin.js',
  permissions: ['meeting:read', 'data:publish', 'ui:toolbar-button', 'ui:popover'],
  contributes: { toolbarButtons: [{ id: 'react', label: 'React', icon: 'smile', opens: 'popover:picker' }], surfaces: { popover: { id: 'picker' } } },
};
const plugin: LoadedPlugin = { manifest, granted: manifest.permissions, source: 'builtin' };

class MemoryStorage {
  map = new Map<string, string>();
  get length() {
    return this.map.size;
  }
  key(i: number) {
    return [...this.map.keys()][i] ?? null;
  }
  getItem(k: string) {
    return this.map.get(k) ?? null;
  }
  setItem(k: string, v: string) {
    this.map.set(k, v);
  }
  removeItem(k: string) {
    this.map.delete(k);
  }
}

test('toolbar models come only from plugins holding ui:toolbar-button, with patches applied', () => {
  const patches = new Map([[buttonKey('petal.reactions', 'react'), { badge: 120, disabled: true }]]);
  const models = toolbarButtonModels([plugin, { ...plugin, granted: ['meeting:read'] }], patches);
  assert.equal(models.length, 1);
  assert.equal(models[0]!.ariaLabel, 'React (Reactions)');
  assert.equal(models[0]!.opens, 'popover:picker');
  assert.equal(models[0]!.disabled, true);
  assert.equal(badgeText(models[0]!.badge), '99+');
  assert.equal(badgeText(0), null);
  assert.equal(badgeText(7), '7');
  assert.equal(fitButtonLabel('  Way too long label here '), 'Way too long l');
});

test('placePopover prefers above the anchor and clamps to the viewport', () => {
  const vp = { width: 400, height: 700 };
  const above = placePopover({ left: 180, top: 640, width: 52, height: 52 }, { width: 296, height: 64 }, vp);
  assert.deepEqual(above, { left: 58, top: 568, width: 296, height: 64 });
  const nearLeft = placePopover({ left: 4, top: 640, width: 52, height: 52 }, { width: 296, height: 64 }, vp);
  assert.equal(nearLeft.left, 8);
  const noRoomAbove = placePopover({ left: 180, top: 10, width: 52, height: 52 }, { width: 296, height: 64 }, vp);
  assert.equal(noRoomAbove.top, 70);
  const tooWide = placePopover({ left: 0, top: 300, width: 10, height: 10 }, { width: 900, height: 64 }, vp);
  assert.equal(tooWide.width, 384);
});

test('enabled overrides round-trip and built-in defaults apply without one', () => {
  const storage = new MemoryStorage();
  assert.deepEqual(readEnabledOverrides(storage), {});
  const installed = { manifest, enabledByDefault: true };
  assert.equal(isPluginEnabled(installed, {}), true);
  writeEnabledOverride(storage, 'petal.reactions', false);
  assert.equal(isPluginEnabled(installed, readEnabledOverrides(storage)), false);
  storage.setItem(PLUGIN_ENABLED_STORAGE_KEY, 'not json');
  assert.deepEqual(readEnabledOverrides(storage), {});
  storage.setItem(PLUGIN_ENABLED_STORAGE_KEY, JSON.stringify({ a: true, b: 'yes' }));
  assert.deepEqual(readEnabledOverrides(storage), { a: true });
});

test('clearPluginStorage removes the enabled map and every plugin KV key, nothing else', () => {
  const storage = new MemoryStorage();
  storage.setItem(PLUGIN_ENABLED_STORAGE_KEY, '{}');
  storage.setItem(`${PLUGIN_KV_STORAGE_PREFIX}petal.chat.v1`, '{}');
  storage.setItem('petal.favoriteRooms.v1', '[]');
  clearPluginStorage(storage);
  assert.deepEqual([...storage.map.keys()], ['petal.favoriteRooms.v1']);
});

test('settings rows are sorted, labelled, and every permission has plain copy', () => {
  const rows = pluginSettingsRows(
    [
      { manifest: { ...manifest, id: 'p.z', name: 'Zeta' }, source: 'dev', enabledByDefault: true, source_js: '' },
      { manifest, source: 'builtin', enabledByDefault: false, source_js: '' },
    ],
    { 'p.z': false },
  );
  assert.deepEqual(rows.map((r) => r.name), ['Reactions', 'Zeta']);
  assert.equal(rows[0]!.sourceLabel, 'Built-in');
  assert.equal(rows[0]!.canUninstall, false);
  assert.equal(rows[0]!.enabled, false);
  assert.equal(rows[1]!.sourceLabel, 'Dev');
  assert.equal(rows[1]!.enabled, false);
  for (const p of rows[0]!.permissions) assert.notEqual(p.label, p.id, `missing copy for ${p.id}`);
  assert.equal(permissionLabel('net:fetch:hooks.slack.com'), 'Contact hooks.slack.com');
});

test('plugin icons are a closed set with a safe fallback', () => {
  assert.ok(PLUGIN_ICON_NAMES.includes('smile'));
  assert.match(pluginIconSvg('smile', 20), /^<svg width="20"/);
  assert.equal(pluginIconSvg('<img onerror=x>'), pluginIconSvg('puzzle'));
});
