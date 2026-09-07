import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

import { packBundle, sortKeysDeep } from './build-all.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = join(here, '..', 'web-harness', 'tests', 'fixtures', 'plugins', 'hello');
const run = promisify(execFile);

test('packBundle is deterministic and sorts keys deeply', () => {
  const a = packBundle({ id: 'p.x', entry: 'plugin.js', b: 1, a: { z: 1, y: 2 } }, 'x');
  const b = packBundle({ a: { y: 2, z: 1 }, entry: 'plugin.js', id: 'p.x', b: 1 }, 'x');
  assert.equal(a, b);
  assert.ok(a.endsWith('\n'));
  assert.deepEqual(sortKeysDeep({ b: [{ d: 1, c: 2 }], a: 0 }), { a: 0, b: [{ c: 2, d: 1 }] });
});

test('packBundle refuses path games and empty sources', () => {
  assert.throws(() => packBundle({ id: 'p.x', entry: '../plugin.js' }, 'x'), /invalid entry/);
  assert.throws(() => packBundle({ id: 'nope', entry: 'plugin.js' }, 'x'), /invalid plugin id/);
  assert.throws(() => packBundle({ id: 'p.x', entry: 'plugin.js' }, ''), /empty/);
});

test('the CLI packs the hello fixture without a vite build', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'petal-plugin-pack-'));
  const out = join(dir, 'bundle.json');
  await run(process.execPath, [join(here, 'build-all.mjs'), fixture, '--no-build', '--out', out]);
  const bundle = JSON.parse(await readFile(out, 'utf8'));
  assert.equal(bundle.manifest.id, 'petal.test-hello');
  assert.match(bundle.files['plugin.js'], /__petalRegister|definePlugin/);
});
