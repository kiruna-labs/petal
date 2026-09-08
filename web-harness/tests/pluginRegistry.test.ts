// Pins the plugin-registry verify chain to contracts/plugin-registry/ (a
// signed sample index + bundle under a throwaway test key). The desktop's
// Rust client (plugins/registry.rs) pins the same files.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { parseMinisignPublicKey, parseMinisignSignature } from '@petal/shared/plugin-host/minisign';
import { availableUpdates, bundlePath, installableVersion, parseRegistryIndex, verifyRegistryBundle as verifyBundleWith, verifyRegistryIndex as verifyIndexWith } from '@petal/shared/plugin-host/registry';
import { generateMinisignKeypair, registryCrypto, sha256Hex, signMinisign, verifyMinisign } from '../src/plugins/minisign.ts';

const verifyRegistryIndex = (i: string, s: string, k: string) => verifyIndexWith(registryCrypto, i, s, k);
const verifyRegistryBundle = (b: Uint8Array, s: string, k: string, e: { id: string; entry: any }) => verifyBundleWith(registryCrypto, b, s, k, e);

const dir = new URL('../../contracts/plugin-registry/', import.meta.url);
const read = (p: string) => readFileSync(new URL(p, dir), 'utf8');
const pub = read('test.pub');
const indexText = read('index.json');
const indexSig = read('index.json.minisig');
const bundleText = read('plugins/petal.test-hello/1.0.0/bundle.json');
const bundleSig = read('plugins/petal.test-hello/1.0.0/bundle.json.minisig');
const enc = (s: string) => new TextEncoder().encode(s);

test('the fixture index verifies against the fixture key, and every tamper is caught', () => {
  const ok = verifyRegistryIndex(indexText, indexSig, pub);
  assert.ok(ok.ok, JSON.stringify(ok));
  if (!ok.ok) return;
  assert.equal(ok.index.plugins.length, 2);
  assert.match(ok.trustedComment, /file:index.json/);

  const tampered = verifyRegistryIndex(indexText.replace('"verified": false', '"verified": true'), indexSig, pub);
  assert.equal(tampered.ok, false);
  const altComment = verifyRegistryIndex(indexText, indexSig.replace('trusted comment: timestamp', 'trusted comment: timestamq'), pub);
  assert.equal(altComment.ok, false);
  if (!altComment.ok) assert.match(altComment.reason, /trusted comment/);

  const other = generateMinisignKeypair('other');
  const wrongKey = verifyRegistryIndex(indexText, indexSig, other.publicKeyText);
  assert.equal(wrongKey.ok, false);
  if (!wrongKey.ok) assert.match(wrongKey.reason, /key id/);
});

test('minisign parsing: shapes, algorithm, and a sign/verify round trip in the prehashed format', () => {
  const pk = parseMinisignPublicKey(pub);
  assert.equal(pk.key.length, 32);
  assert.equal(pk.keyIdHex.length, 16);
  const sig = parseMinisignSignature(indexSig);
  assert.equal(sig.algorithm, 'ED');
  assert.equal(sig.signature.length, 64);
  assert.throws(() => parseMinisignPublicKey('untrusted comment: x\nAAAA\n'), /not an Ed25519/);
  assert.throws(() => parseMinisignSignature('one\ntwo\n'), /expected 4 lines/);

  const kp = generateMinisignKeypair('round trip');
  const data = enc('hello registry');
  const sigText = signMinisign(kp.secretKey, kp.keyId, data, 'trusted:yes');
  assert.deepEqual(verifyMinisign(kp.publicKeyText, sigText, data), { ok: true, trustedComment: 'trusted:yes' });
  assert.equal(verifyMinisign(kp.publicKeyText, sigText, enc('hello registrY')).ok, false);
});

test('the fixture bundle passes the full chain and each broken link is named', () => {
  const parsed = parseRegistryIndex(indexText);
  assert.ok(parsed.ok);
  if (!parsed.ok) return;
  const hello = parsed.index.plugins.find((p) => p.id === 'petal.test-hello')!;
  const entry = hello.versions[0]!;
  const bytes = enc(bundleText);
  assert.equal(sha256Hex(bytes), entry.sha256);
  assert.equal(bytes.byteLength, entry.size);
  const ok = verifyRegistryBundle(bytes, bundleSig, pub, { id: hello.id, entry });
  assert.ok(ok.ok, JSON.stringify(ok));
  if (!ok.ok) return;
  assert.equal(ok.manifest.id, 'petal.test-hello');
  assert.match(ok.source, /__petalRegister/);

  const flipped = new Uint8Array(bytes);
  flipped[flipped.length - 3] ^= 1;
  const badSha = verifyRegistryBundle(flipped, bundleSig, pub, { id: hello.id, entry });
  assert.equal(badSha.ok, false);
  if (!badSha.ok) assert.match(badSha.reason, /sha256/);
  const wrongVersion = verifyRegistryBundle(bytes, bundleSig, pub, { id: hello.id, entry: { ...entry, version: '2.0.0' } });
  assert.equal(wrongVersion.ok, false);
  if (!wrongVersion.ok) assert.match(wrongVersion.reason, /version/);
  const wrongId = verifyRegistryBundle(bytes, bundleSig, pub, { id: 'acme.other', entry });
  assert.equal(wrongId.ok, false);
  if (!wrongId.ok) assert.match(wrongId.reason, /expected acme.other/);
  const wrongSize = verifyRegistryBundle(bytes, bundleSig, pub, { id: hello.id, entry: { ...entry, size: entry.size + 1 } });
  assert.equal(wrongSize.ok, false);
});

test('parseRegistryIndex refuses malformed registries as a whole', () => {
  const good = JSON.parse(indexText);
  const mutate = (fn: (i: any) => void) => {
    const copy = JSON.parse(JSON.stringify(good));
    fn(copy);
    return parseRegistryIndex(JSON.stringify(copy));
  };
  assert.equal(mutate((i) => (i.schemaVersion = 2)).ok, false);
  assert.equal(mutate((i) => (i.plugins[0].versions[0].bundleUrl = 'http://plugins.example.test/x')).ok, false, 'http refused');
  assert.equal(mutate((i) => (i.plugins[0].versions[0].bundleUrl = 'http://localhost:8787/x')).ok, true, 'localhost http allowed for dev');
  assert.equal(mutate((i) => (i.plugins[1].id = i.plugins[0].id)).ok, false, 'duplicate id');
  assert.equal(mutate((i) => (i.plugins[0].latest = '9.9.9')).ok, false, 'latest not among versions');
  assert.equal(mutate((i) => (i.plugins[0].versions[0].sha256 = 'XYZ')).ok, false);
  assert.equal(mutate((i) => i.plugins[0].versions[0].permissions.push('fs:read')).ok, false, 'unknown permission');
  assert.equal(parseRegistryIndex('nope').ok, false);
  assert.equal(bundlePath('petal.test-hello', '1.0.0'), 'plugins/petal.test-hello/1.0.0/bundle.json');
  assert.throws(() => bundlePath('../x', '1.0.0'));
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
