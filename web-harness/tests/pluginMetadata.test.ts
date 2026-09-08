import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  PLUGINS_METADATA_KEY,
  PLUGIN_STATE_LIMITS,
  diffPluginState,
  mergePluginMetadata,
  pluginsFromMetadata,
} from '@petal/shared/plugin-host/metadata';

const fixture = JSON.parse(readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')) as {
  pluginStateMetadata: { key: string; metadata: string; entries: Record<string, unknown>; limits: Record<string, number> };
  pluginStateChangedEvent: { fields: string[]; example: Record<string, unknown> };
};

test('pluginsFromMetadata reads the contract example and drops malformed entries', () => {
  assert.equal(PLUGINS_METADATA_KEY, fixture.pluginStateMetadata.key);
  assert.deepEqual(pluginsFromMetadata(fixture.pluginStateMetadata.metadata), fixture.pluginStateMetadata.entries);
  assert.deepEqual(pluginsFromMetadata(undefined), {});
  assert.deepEqual(pluginsFromMetadata('not json'), {});
  assert.deepEqual(pluginsFromMetadata('{"plugins":[]}'), {});
  for (const [k, v] of Object.entries(fixture.pluginStateMetadata.limits)) {
    assert.equal((PLUGIN_STATE_LIMITS as Record<string, number>)[k], v, k);
  }
  assert.deepEqual(Object.keys(fixture.pluginStateChangedEvent.example).sort(), fixture.pluginStateChangedEvent.fields);
});

test('mergePluginMetadata preserves unrelated keys, removes on null, drops the key when empty', () => {
  const start = '{"petalIdentityPaletteIndex":2,"petalWindowKinds":{"42":"window"}}';
  const one = mergePluginMetadata(start, 'petal.reactions', { v: '1.0.0', src: 'builtin' });
  assert.deepEqual(JSON.parse(one), {
    petalIdentityPaletteIndex: 2,
    petalWindowKinds: { '42': 'window' },
    plugins: { 'petal.reactions': { v: '1.0.0', src: 'builtin' } },
  });
  const two = mergePluginMetadata(one, 'petal.chat', { v: '1.2.0', src: 'registry', state: { unread: 3 } });
  assert.deepEqual(pluginsFromMetadata(two), {
    'petal.reactions': { v: '1.0.0', src: 'builtin' },
    'petal.chat': { v: '1.2.0', src: 'registry', state: { unread: 3 } },
  });
  const cleared = mergePluginMetadata(two, 'petal.chat', { v: '1.2.0', src: 'registry', state: null });
  assert.deepEqual(pluginsFromMetadata(cleared)['petal.chat'], { v: '1.2.0', src: 'registry' }, 'null state clears state');
  const removed = mergePluginMetadata(mergePluginMetadata(cleared, 'petal.chat', null), 'petal.reactions', null);
  assert.deepEqual(JSON.parse(removed), { petalIdentityPaletteIndex: 2, petalWindowKinds: { '42': 'window' } }, 'empty plugins key is dropped');
  assert.throws(() => mergePluginMetadata(start, 'reactions', null), /invalid plugin id/);
});

test('mergePluginMetadata enforces the per-plugin and total budgets', () => {
  const big = 'x'.repeat(PLUGIN_STATE_LIMITS.perPluginStateBytes);
  assert.throws(() => mergePluginMetadata(undefined, 'petal.chat', { v: '1.0.0', src: 'builtin', state: big }), /exceeds 2048/);
  let meta: string | undefined;
  const state = 'y'.repeat(1900);
  assert.throws(() => {
    for (let i = 0; i < 6; i++) meta = mergePluginMetadata(meta, `petal.p${i}`, { v: '1.0.0', src: 'builtin', state });
  }, /exceeds 8192/);
});

test('diffPluginState reports only changed state, including removals', () => {
  const before = { 'petal.chat': { v: '1.0.0', src: 'registry' as const, state: { unread: 1 } }, 'petal.x': { v: '1.0.0', src: 'dev' as const } };
  const after = { 'petal.chat': { v: '1.0.0', src: 'registry' as const, state: { unread: 2 } }, 'petal.y': { v: '1.0.0', src: 'dev' as const, state: 1 } };
  assert.deepEqual(
    diffPluginState(before, after).sort((a, b) => a.pluginId.localeCompare(b.pluginId)),
    [
      { pluginId: 'petal.chat', value: { unread: 2 } },
      { pluginId: 'petal.y', value: 1 },
    ],
    'petal.x had no state, so its disappearance is not a state change',
  );
  assert.deepEqual(diffPluginState(after, {}), [{ pluginId: 'petal.chat', value: undefined }, { pluginId: 'petal.y', value: undefined }]);
});
