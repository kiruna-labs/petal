import { test } from 'node:test';
import assert from 'node:assert/strict';

import { MANIFEST_LIMITS, isButtonLabel, isPrintableDisplayText, validateManifest } from '@petal/shared/plugin-host/manifest';
import { PLUGIN_LIMITS } from '@petal/shared/plugin-host/rateLimit';
import {
  PLUGIN_MENU_DISABLE,
  PLUGIN_SOURCE_WORD,
  pluginCaption,
  pluginDisabledToast,
  pluginMenuModel,
  pluginProvenanceTitle,
  type PluginReEnablePath,
} from '@petal/shared/plugin-host/provenance';
import { POPOVER_SIZE, popoverContentSize, toolbarButtonModels } from '@petal/shared/plugin-host/surfaces';
import type { LoadedPlugin, PluginSource } from '@petal/shared/plugin-host/broker';

function manifestInput(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    manifestVersion: 1,
    id: 'acme.demo',
    version: '1.0.0',
    name: 'Demo',
    description: '',
    apiVersion: 1,
    minHostVersion: '0.1.0',
    scope: 'local',
    entry: 'plugin.js',
    permissions: [],
    ...over,
  };
}

test('provenance copy names the plugin, where it came from, and offers exactly "turn off"', () => {
  assert.equal(pluginProvenanceTitle('Reactions', 'builtin'), 'Reactions · built-in plugin');
  assert.equal(pluginCaption('Reactions', 'builtin'), 'Reactions · built-in plugin');
  assert.equal(pluginCaption('Reactions', 'registry'), 'Reactions · installed plugin');
  assert.equal(pluginCaption('Reactions', 'dev'), 'Reactions · dev plugin');
  const menu = pluginMenuModel('Reactions', 'registry');
  assert.equal(menu.heading, 'Reactions · installed plugin');
  assert.deepEqual(menu.items, [{ id: PLUGIN_MENU_DISABLE, label: 'Turn off Reactions' }]);
});

test('a sideloaded plugin cannot claim to be the built-in one of the same name', () => {
  // Both call themselves "Reactions"; only the host's `source` separates them.
  assert.notEqual(pluginCaption('Reactions', 'registry'), pluginCaption('Reactions', 'builtin'));
  for (const source of Object.keys(PLUGIN_SOURCE_WORD) as PluginSource[]) {
    assert.match(pluginCaption('Reactions', source), new RegExp(`${PLUGIN_SOURCE_WORD[source]} plugin$`));
  }
});

test('the disabled toast fits the budget and points only at UI the client has', () => {
  const longest = 'x'.repeat(MANIFEST_LIMITS.nameMaxLength);
  for (const where of ['settings', 'reload'] as PluginReEnablePath[]) {
    assert.ok(pluginDisabledToast(longest, where).length <= PLUGIN_LIMITS.toastMaxChars, `${where} toast over budget`);
  }
  assert.equal(pluginDisabledToast('Reactions', 'settings'), 'Reactions is off. Turn it back on in Settings → Plugins.');
  // The web client has no plugins sheet (I-10), so it must never say "Settings".
  assert.equal(pluginDisabledToast('Reactions', 'reload'), 'Reactions is off. Reload this page to bring it back.');
  assert.doesNotMatch(pluginDisabledToast('Reactions', 'reload'), /Settings/);
});

test('toolbar button models carry the plugin name AND the host-owned source', () => {
  const plugin = (name: string, source: PluginSource): LoadedPlugin => ({
    manifest: {
      manifestVersion: 1,
      id: 'petal.reactions',
      version: '1.0.0',
      name,
      description: '',
      apiVersion: 1,
      minHostVersion: '0.1.0',
      scope: 'meeting',
      entry: 'plugin.js',
      permissions: ['ui:toolbar-button'],
      contributes: { toolbarButtons: [{ id: 'react', label: 'React', icon: 'smile' }] },
    },
    granted: ['ui:toolbar-button'],
    source,
  });
  const builtin = toolbarButtonModels([plugin('Reactions', 'builtin')], new Map())[0]!;
  assert.equal(builtin.pluginName, 'Reactions');
  assert.equal(builtin.pluginSource, 'builtin');
  // The badge is aria-hidden, so provenance has to reach a screen reader here.
  assert.equal(builtin.ariaLabel, 'React (Reactions · built-in plugin)');
  assert.equal(toolbarButtonModels([plugin('Reactions', 'registry')], new Map())[0]!.ariaLabel, 'React (Reactions · installed plugin)');
});

test('a declared popover size is clamped so the host caption always has room', () => {
  // `validateManifest` accepts any positive number, so the clamp is the guard.
  assert.deepEqual(popoverContentSize({ width: 1, height: 1 }), { width: POPOVER_SIZE.minWidth, height: POPOVER_SIZE.minHeight });
  assert.deepEqual(popoverContentSize({ width: 100, height: 64 }), { width: POPOVER_SIZE.minWidth, height: 64 });
  assert.deepEqual(popoverContentSize({}), { width: POPOVER_SIZE.defaultWidth, height: POPOVER_SIZE.defaultHeight });
  // A caption-safe declared size is honoured unchanged.
  assert.deepEqual(popoverContentSize({ width: 296, height: 64 }), { width: 296, height: 64 });
});

test('plugin-authored display text may not carry bidi or control characters', () => {
  // U+202E renders "Petal<RLO> nigulp" as "PETALNIGULP ·": the host's own
  // word "plugin" ends up reversed INSIDE the name. Reject, never sanitize.
  const hostile = [
    'Petal\u202e nigulp', // RIGHT-TO-LEFT OVERRIDE
    'Petal\nSystem', // newline: forges a second line
    'Petal\rSystem',
    'Petal\u0007', // C0 control
    'Petal\u009b', // C1 control
    'Petal\u2066 nigulp', // LEFT-TO-RIGHT ISOLATE
    'Petal\u200f', // RIGHT-TO-LEFT MARK
    'Petal\ufeff', // zero-width no-break space
  ];
  for (const name of hostile) {
    assert.equal(isPrintableDisplayText(name), false, `${JSON.stringify(name)} must not pass the printable guard`);
    const result = validateManifest(manifestInput({ name }));
    assert.equal(result.ok, false, `${JSON.stringify(name)} must be refused as a plugin name`);
    assert.ok(
      result.ok === false && result.errors.some((e) => e.includes('printable')),
      `expected a printable-text error, got ${result.ok === false ? result.errors.join('; ') : ''}`,
    );
    assert.equal(
      isButtonLabel(name.slice(0, MANIFEST_LIMITS.buttonLabelMaxLength)),
      false,
      `${JSON.stringify(name)} must not pass as a button label either`,
    );
  }
  // Ordinary names, accents, and emoji (ZWJ sequences included) still pass.
  for (const name of ['Reactions', 'Café Notes', '\u{1f468}\u200d\u{1f469}\u200d\u{1f467} Family', 'Petal System']) {
    assert.equal(isPrintableDisplayText(name), true, name);
    assert.equal(validateManifest(manifestInput({ name })).ok, true, name);
  }
});
