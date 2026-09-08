import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { PLUGIN_TOPIC_PREFIX, base64ToBytes, bytesToBase64, parsePluginTopic, pluginTopic } from '@petal/shared/plugin-host/topics';
import { PLUGIN_LIMITS } from '@petal/shared/plugin-host/rateLimit';

const fixture = JSON.parse(readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')) as {
  topics: { pluginPrefix: string };
  pluginTopicVectors: Array<{ topic: string; pluginId: string | null; sub: string | null }>;
  pluginLimits: Record<string, number>;
  pluginDataEvent: { fields: string[]; example: Record<string, unknown> };
};

test('plugin topic prefix and parse vectors match the contract fixture', () => {
  assert.equal(PLUGIN_TOPIC_PREFIX, fixture.topics.pluginPrefix);
  for (const v of fixture.pluginTopicVectors) {
    const parsed = parsePluginTopic(v.topic);
    if (v.pluginId === null) assert.equal(parsed, null, `should reject ${v.topic}`);
    else assert.deepEqual(parsed, { pluginId: v.pluginId, sub: v.sub }, v.topic);
  }
});

test('pluginTopic builds what parsePluginTopic accepts, and refuses bad parts', () => {
  assert.equal(pluginTopic('petal.reactions', null), 'plugin/petal.reactions');
  assert.equal(pluginTopic('petal.reactions', 'emoji'), 'plugin/petal.reactions/emoji');
  assert.deepEqual(parsePluginTopic(pluginTopic('acme.x', 'a-b')), { pluginId: 'acme.x', sub: 'a-b' });
  assert.throws(() => pluginTopic('reactions', null));
  assert.throws(() => pluginTopic('petal.reactions', 'Emoji'));
  assert.throws(() => pluginTopic('petal.reactions', 'a/b'));
});

test('limits are pinned by the contract and the event example carries exactly the contract fields', () => {
  for (const [k, v] of Object.entries(fixture.pluginLimits)) {
    assert.equal((PLUGIN_LIMITS as Record<string, number>)[k], v, k);
  }
  assert.deepEqual(Object.keys(fixture.pluginDataEvent.example).sort(), fixture.pluginDataEvent.fields);
  const decoded = new TextDecoder().decode(base64ToBytes(fixture.pluginDataEvent.example.payloadBase64 as string));
  assert.deepEqual(JSON.parse(decoded), { e: '👍', t: 1788640000000 });
});

test('base64 helpers round-trip arbitrary bytes', () => {
  const bytes = new Uint8Array(70000).map((_, i) => (i * 7) % 256);
  assert.deepEqual(base64ToBytes(bytesToBase64(bytes)), bytes);
  assert.equal(bytesToBase64(new Uint8Array()), '');
});
