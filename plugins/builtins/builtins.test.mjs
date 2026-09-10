import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const MAX_BUNDLE_BYTES = 2 * 1024 * 1024;

// Mirror of packBundle's canonical form in kiruna-labs/petal-plugins build-all.mjs.
function sortKeysDeep(value) {
  if (Array.isArray(value)) return value.map(sortKeysDeep);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.keys(value).sort().map((k) => [k, sortKeysDeep(value[k])]));
  }
  return value;
}

test('every vendored bundle is canonical, self-consistent, and pinned to a plugins-repo commit', async () => {
  const sources = JSON.parse(await readFile(join(here, 'SOURCES.json'), 'utf8'));
  const dirs = (await readdir(here, { withFileTypes: true })).filter((e) => e.isDirectory()).map((e) => e.name);
  assert.ok(dirs.length > 0, 'at least one built-in is vendored');
  assert.deepEqual(Object.keys(sources).sort(), dirs.sort(), 'SOURCES.json lists exactly the vendored bundles');
  for (const id of dirs) {
    const text = await readFile(join(here, id, 'bundle.json'), 'utf8');
    assert.ok(Buffer.byteLength(text, 'utf8') <= MAX_BUNDLE_BYTES, `${id}: bundle within the registry size cap`);
    const bundle = JSON.parse(text);
    assert.equal(JSON.stringify(sortKeysDeep(bundle), null, 2) + '\n', text, `${id}: bundle.json is in canonical form (never hand-edited)`);
    assert.equal(bundle.manifest.id, id, `${id}: directory name equals the manifest id`);
    assert.equal(typeof bundle.files[bundle.manifest.entry], 'string', `${id}: entry file present`);
    assert.ok(bundle.files[bundle.manifest.entry].length > 0, `${id}: entry file non-empty`);
    const pin = sources[id];
    assert.equal(pin.repo, 'kiruna-labs/petal-plugins', `${id}: vendored from the plugins repo`);
    assert.match(pin.commit, /^[0-9a-f]{40}$/, `${id}: pinned to a full commit sha`);
    assert.equal(pin.version, bundle.manifest.version, `${id}: SOURCES.json version equals the manifest version`);
  }
});
