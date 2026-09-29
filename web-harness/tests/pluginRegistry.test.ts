// Pins the registry index MODEL (shared/plugin-host/registry.ts) to
// contracts/plugin-registry/: the sample index parses, installability and
// updates behave, every case in invalid-index-cases.json is rejected, and
// every case in unsupported-permission-cases.json keeps the index but blocks
// only that entry -- the same files the Rust validator (plugins::registry)
// iterates, so the two implementations cannot drift apart silently.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  REGISTRY_EPOCH_ISO,
  availableUpdates,
  bundlePath,
  installableVersion,
  isAcceptableGeneratedAt,
  isNotRolledBack,
  isRegistryUrl,
  parseRegistryIndex,
} from '@petal/shared/plugin-host/registry';
import { classifyPermission } from '@petal/shared/plugin-host/manifest';

const dir = new URL('../../contracts/plugin-registry/', import.meta.url);
const read = (p: string) => readFileSync(new URL(p, dir), 'utf8');
const indexText = read('index.json');

function applyPointer(doc: any, pointer: string, value: unknown, del: boolean): void {
  const parts = pointer.split('/').slice(1).map((p) => p.replace(/~1/g, '/').replace(/~0/g, '~'));
  let cur = doc;
  for (const part of parts.slice(0, -1)) cur = cur[Array.isArray(cur) ? Number(part) : part];
  const last = parts[parts.length - 1]!;
  const key = Array.isArray(cur) ? Number(last) : last;
  if (del) {
    if (Array.isArray(cur)) cur.splice(key as number, 1);
    else delete cur[key];
  } else cur[key] = value;
}

test('the fixture index parses; every shared invalid case is rejected as a whole', () => {
  const ok = parseRegistryIndex(indexText);
  assert.ok(ok.ok, JSON.stringify(ok));
  if (ok.ok) assert.equal(ok.index.plugins.length, 2);
  const cases = JSON.parse(read('invalid-index-cases.json')).cases as Array<{ name: string; path: string; value?: unknown; delete?: boolean }>;
  assert.ok(cases.length >= 15);
  for (const c of cases) {
    const doc = JSON.parse(indexText);
    applyPointer(doc, c.path, c.value, c.delete === true);
    const result = parseRegistryIndex(JSON.stringify(doc));
    assert.equal(result.ok, false, `expected rejection: ${c.name}`);
  }
  assert.equal(parseRegistryIndex('nope').ok, false);
});

test('registry urls are https, or http only on a real loopback host', () => {
  assert.equal(isRegistryUrl('https://plugins.example.test/'), true);
  assert.equal(isRegistryUrl('http://localhost:8787/x'), true);
  assert.equal(isRegistryUrl('http://127.0.0.1/x'), true);
  assert.equal(isRegistryUrl('http://localhost.attacker.example/x'), false);
  assert.equal(isRegistryUrl('http://127.0.0.1.attacker.example/x'), false);
  assert.equal(isRegistryUrl('http://plugins.example.test/'), false);
  assert.equal(bundlePath('petal.test-hello', '1.0.0'), 'plugins/petal.test-hello/1.0.0/bundle.json');
  assert.throws(() => bundlePath('../x', '1.0.0'));
});

test('freshness: RFC 3339 on/after the registry epoch, and never older than the last accepted index', () => {
  assert.equal(isAcceptableGeneratedAt('2026-09-08T12:00:00.000Z'), true);
  assert.equal(isAcceptableGeneratedAt('2026-09-08T12:00:00Z'), true);
  assert.equal(isAcceptableGeneratedAt('1970-01-01T00:00:00.000Z'), false);
  assert.equal(isAcceptableGeneratedAt('not a date at all'), false);
  assert.equal(isAcceptableGeneratedAt('2026-09-08 12:00:00'), false);
  assert.equal(Date.parse(REGISTRY_EPOCH_ISO), Date.UTC(2026, 0, 1));
  const seen = { generatedAtMs: 100, signedAtS: 10 };
  assert.equal(isNotRolledBack(null, seen), true);
  assert.equal(isNotRolledBack(seen, { generatedAtMs: 100, signedAtS: 10 }), true);
  assert.equal(isNotRolledBack(seen, { generatedAtMs: 99, signedAtS: 11 }), false);
  assert.equal(isNotRolledBack(seen, { generatedAtMs: 101, signedAtS: 9 }), false);
});

test('installableVersion honours verified + host compatibility; availableUpdates reports new permissions', () => {
  const parsed = parseRegistryIndex(indexText);
  assert.ok(parsed.ok);
  if (!parsed.ok) return;
  const [hello, unverified] = parsed.index.plugins;
  assert.equal(installableVersion(hello!, '9.9.9')?.version, '1.0.0');
  assert.equal(installableVersion(hello!, '0.0.1'), null, 'host too old');
  assert.equal(installableVersion(unverified!, '9.9.9'), null, 'unverified never installable');
  const updates = availableUpdates([{ id: 'petal.test-hello', version: '0.9.0', permissions: ['meeting:read'] }], parsed.index, '9.9.9');
  assert.equal(updates.length, 1);
  assert.equal(updates[0]!.to.version, '1.0.0');
  assert.deepEqual(updates[0]!.newPermissions, hello!.versions[0]!.permissions.filter((p) => p !== 'meeting:read'));
  assert.deepEqual(availableUpdates([{ id: 'petal.test-hello', version: '1.0.0', permissions: [] }], parsed.index, '9.9.9'), []);
});

test('every shared unsupported-permission case keeps the index, blocks only that entry, and never offers it', () => {
  const fixture = JSON.parse(read('unsupported-permission-cases.json')) as {
    path: string;
    cases: Array<{ name: string; value: unknown; unsupported: string[] }>;
  };
  assert.ok(fixture.cases.length >= 5);
  const baseline = parseRegistryIndex(indexText);
  assert.ok(baseline.ok);
  if (!baseline.ok) return;
  assert.deepEqual(baseline.index.plugins[0]!.versions[0]!.unsupportedPermissions, []);
  assert.ok(installableVersion(baseline.index.plugins[0]!, '9.9.9'), 'the unmodified entry is installable');
  for (const c of fixture.cases) {
    const doc = JSON.parse(indexText);
    applyPointer(doc, fixture.path, c.value, false);
    const result = parseRegistryIndex(JSON.stringify(doc));
    assert.ok(result.ok, `${c.name}: the index must still parse: ${JSON.stringify(result)}`);
    if (!result.ok) continue;
    const plugin = result.index.plugins[0]!;
    assert.deepEqual(plugin.versions[0]!.unsupportedPermissions, c.unsupported, c.name);
    assert.deepEqual(plugin.versions[0]!.permissions, c.value, `${c.name}: listed permissions kept as-is`);
    assert.equal(installableVersion(plugin, '9.9.9'), null, `${c.name}: needs a newer Petal`);
    assert.deepEqual(availableUpdates([{ id: plugin.id, version: '0.0.1', permissions: [] }], result.index, '9.9.9'), [], `${c.name}: never offered as an update`);
    assert.deepEqual(result.index.plugins[1], baseline.index.plugins[1], `${c.name}: other plugins unaffected`);
  }
});

test('classifyPermission: known, unsupported (future or reserved), malformed', () => {
  for (const known of ['meeting:read', 'storage', 'net:fetch:user-urls', 'net:fetch:*.example.com', 'net:fetch:localhost:8787']) {
    assert.equal(classifyPermission(known), 'known', known);
  }
  for (const unsupported of ['frames:read', 'future:thing', 'telepathy', 'future:fetch:hooks.example.com', 'future:' + 'x'.repeat(57)]) {
    assert.equal(classifyPermission(unsupported), 'unsupported', unsupported);
  }
  for (const malformed of [
    '',
    42,
    null,
    'Meeting:Read',
    'meeting read',
    '1meeting',
    'meeting::read',
    'meeting:',
    'net:fetch:*',
    'net:fetch:*.*',
    'net:fetch:https://x.example.com',
    'future:' + 'x'.repeat(58),
  ]) {
    assert.equal(classifyPermission(malformed), 'malformed', JSON.stringify(malformed));
  }
});
