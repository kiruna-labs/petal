#!/usr/bin/env node
// Regenerates the plugin-registry contract fixtures with a FRESH test keypair:
//   test.pub, index.json(.minisig), plugins/petal.test-hello/1.0.0/bundle.json(.minisig)
// The secret half is deliberately discarded -- nothing needs to re-sign these
// files, and a committed private key (even a test one) is a habit to avoid.
// Run from the repo root:  node contracts/plugin-registry/gen-fixtures.mjs
// (resolves @noble/* through web-harness/node_modules; run `npm ci` there first).
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '../..');
const wh = resolve(repoRoot, 'web-harness');
const load = (p) => import(pathToFileURL(resolve(wh, 'node_modules', p)).href);
const ed = await load('@noble/ed25519/index.js');
const { sha512, sha256 } = await load('@noble/hashes/sha2.js');
const { blake2b } = await load('@noble/hashes/blake2.js');
ed.hashes.sha512 = sha512;

// Canonical bundle form, mirror of packBundle in kiruna-labs/petal-plugins build-all.mjs
// (keys sorted deeply, two-space JSON, LF, trailing newline). plugins/builtins/builtins.test.mjs
// pins the same form for the vendored built-ins.
function sortKeysDeep(value) {
  if (Array.isArray(value)) return value.map(sortKeysDeep);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().map((k) => [k, sortKeysDeep(value[k])]));
  return value;
}
function packBundle(manifest, entrySource) {
  return JSON.stringify(sortKeysDeep({ manifest, files: { [manifest.entry]: entrySource } }), null, 2).replace(/\r\n/g, '\n') + '\n';
}

// --- minisign primitives (same format the Rust `minisign-verify` crate reads; kept inline so this
// script has no TS dependency) ---
const b64 = (bytes) => Buffer.from(bytes).toString('base64');
function signMinisign(secretKey, keyId, data, trustedComment) {
  const signature = ed.sign(blake2b(data, { dkLen: 64 }), secretKey);
  const trusted = new TextEncoder().encode(trustedComment);
  const globalMessage = new Uint8Array(64 + trusted.length);
  globalMessage.set(signature, 0);
  globalMessage.set(trusted, 64);
  const globalSignature = ed.sign(globalMessage, secretKey);
  const blob = new Uint8Array(74);
  blob.set([0x45, 0x44], 0);
  blob.set(keyId, 2);
  blob.set(signature, 10);
  return `untrusted comment: signature from petal plugin registry (TEST FIXTURE)\n${b64(blob)}\ntrusted comment: ${trustedComment}\n${b64(globalSignature)}\n`;
}
function encodePublicKey(keyId, pk) {
  const raw = new Uint8Array(42);
  raw.set([0x45, 0x64], 0);
  raw.set(keyId, 2);
  raw.set(pk, 10);
  return `untrusted comment: petal plugin registry TEST public key -- fixtures only, never trusted by a real build\n${b64(raw)}\n`;
}

const secretKey = ed.utils.randomSecretKey();
const publicKey = ed.getPublicKey(secretKey);
const keyId = crypto.getRandomValues(new Uint8Array(8));
const registryUrl = 'https://plugins.example.test';

// Bundle: the hello fixture plugin (web-harness/tests/fixtures/plugins/hello).
const helloDir = resolve(wh, 'tests/fixtures/plugins/hello');
const manifest = JSON.parse(await readFile(resolve(helloDir, 'manifest.json'), 'utf8'));
const bundleText = packBundle(manifest, await readFile(resolve(helloDir, 'plugin.js'), 'utf8'));
const bundleBytes = new TextEncoder().encode(bundleText);
const sha = Buffer.from(sha256(bundleBytes)).toString('hex');
const path = `plugins/${manifest.id}/${manifest.version}`;

const index = {
  schemaVersion: 1,
  generatedAt: '2026-09-08T12:00:00.000Z',
  plugins: [
    {
      id: manifest.id,
      name: manifest.name,
      description: manifest.description,
      publisher: 'petal',
      latest: manifest.version,
      versions: [
        {
          version: manifest.version,
          minHostVersion: manifest.minHostVersion,
          apiVersion: manifest.apiVersion,
          permissions: manifest.permissions,
          bundleUrl: `${registryUrl}/${path}/bundle.json`,
          sigUrl: `${registryUrl}/${path}/bundle.json.minisig`,
          sha256: sha,
          size: bundleBytes.byteLength,
          verified: true,
          scan: { tool: 'petal-scan', reportSha256: 'a'.repeat(64), at: '2026-09-08T11:00:00.000Z' },
        },
      ],
    },
    {
      // Listed but not installable from the UI: verified:false is the hook for the scanner.
      id: 'acme.unverified-thing',
      name: 'Unverified Thing',
      description: 'A third-party plugin awaiting review.',
      publisher: 'acme',
      latest: '0.1.0',
      versions: [
        {
          version: '0.1.0',
          minHostVersion: '0.9.7',
          apiVersion: 1,
          permissions: ['meeting:read', 'ui:toast'],
          bundleUrl: `${registryUrl}/plugins/acme.unverified-thing/0.1.0/bundle.json`,
          sigUrl: `${registryUrl}/plugins/acme.unverified-thing/0.1.0/bundle.json.minisig`,
          sha256: 'b'.repeat(64),
          size: 1234,
          verified: false,
          scan: null,
        },
      ],
    },
  ],
};
const indexText = JSON.stringify(index, null, 2) + '\n';

await mkdir(resolve(here, path), { recursive: true });
await writeFile(resolve(here, 'test.pub'), encodePublicKey(keyId, publicKey));
await writeFile(resolve(here, 'index.json'), indexText);
await writeFile(resolve(here, 'index.json.minisig'), signMinisign(secretKey, keyId, new TextEncoder().encode(indexText), `timestamp:1788868800\tfile:index.json\tprehashed`));
await writeFile(resolve(here, path, 'bundle.json'), bundleText);
await writeFile(resolve(here, path, 'bundle.json.minisig'), signMinisign(secretKey, keyId, bundleBytes, `timestamp:1788868800\tfile:bundle.json\tprehashed`));
console.log(`fixtures written; key id ${Buffer.from(keyId).toString('hex')}, bundle sha256 ${sha.slice(0, 12)}…`);
