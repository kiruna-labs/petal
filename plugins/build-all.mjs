#!/usr/bin/env node
// Build every first-party plugin (or one directory) and pack each into the
// deterministic `dist/bundle.json` that (a) the two clients compile in as
// built-ins and (b) the marketplace publisher signs and uploads.
//
//   node plugins/build-all.mjs            # every plugins/<id>/ with a manifest.json
//   node plugins/build-all.mjs reactions  # one plugin
//   node plugins/build-all.mjs <dir> --out <file> --no-build
//
// "Deterministic" = the same sources always give byte-identical bundle.json:
// keys sorted, two-space JSON, LF newlines, no timestamps. The registry's
// sha256 and the client's verify chain depend on that.
//
// Full manifest validation lives in shared/plugin-host/manifest.ts (TS) and
// runs in the clients and in web-harness tests; this script only checks what
// it needs to locate the entry and refuses obvious path games.

import { readFile, readdir, stat, writeFile, mkdir } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const ID_RE = /^[a-z0-9]+(\.[a-z0-9-]+)+$/;
const ENTRY_RE = /^[A-Za-z0-9_.-]+\.js$/;
const MAX_BUNDLE_BYTES = 2 * 1024 * 1024;

export function sortKeysDeep(value) {
  if (Array.isArray(value)) return value.map(sortKeysDeep);
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((k) => [k, sortKeysDeep(value[k])]),
    );
  }
  return value;
}

/** Pure: manifest + entry source -> canonical bundle.json text. */
export function packBundle(manifest, entrySource) {
  if (!manifest || typeof manifest !== 'object') throw new Error('manifest must be an object');
  if (!ID_RE.test(String(manifest.id))) throw new Error(`invalid plugin id: ${manifest.id}`);
  if (!ENTRY_RE.test(String(manifest.entry))) throw new Error(`invalid entry: ${manifest.entry}`);
  if (typeof entrySource !== 'string' || entrySource.length === 0) throw new Error('entry source is empty');
  const bundle = sortKeysDeep({ manifest, files: { [manifest.entry]: entrySource } });
  const text = JSON.stringify(bundle, null, 2).replace(/\r\n/g, '\n') + '\n';
  const bytes = Buffer.byteLength(text, 'utf8');
  if (bytes > MAX_BUNDLE_BYTES) throw new Error(`bundle is ${bytes} bytes; limit is ${MAX_BUNDLE_BYTES}`);
  return text;
}

async function exists(path) {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

function run(cmd, args, cwd) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(cmd, args, { cwd, stdio: 'inherit' });
    child.on('exit', (code) => (code === 0 ? resolvePromise() : reject(new Error(`${cmd} ${args.join(' ')} exited ${code}`))));
    child.on('error', reject);
  });
}

async function buildOne(dir, { build = true, out } = {}) {
  const manifestPath = join(dir, 'manifest.json');
  const manifest = JSON.parse(await readFile(manifestPath, 'utf8'));
  if (build && (await exists(join(dir, 'vite.config.ts')))) {
    // Resolve vite from the plugins workspace so plugins need no devDependencies of their own.
    await run(process.execPath, [join(here, 'node_modules', 'vite', 'bin', 'vite.js'), 'build', '--config', join(dir, 'vite.config.ts')], dir);
  }
  const distEntry = join(dir, 'dist', manifest.entry);
  const srcEntry = join(dir, manifest.entry);
  const entryPath = (await exists(distEntry)) ? distEntry : srcEntry;
  if (!(await exists(entryPath))) throw new Error(`${manifest.id}: no built entry at ${distEntry} or ${srcEntry}`);
  const text = packBundle(manifest, await readFile(entryPath, 'utf8'));
  const outPath = out ?? join(dir, 'dist', 'bundle.json');
  await mkdir(dirname(outPath), { recursive: true });
  await writeFile(outPath, text);
  return { id: manifest.id, version: manifest.version, outPath, bytes: Buffer.byteLength(text, 'utf8') };
}

async function discover() {
  const entries = await readdir(here, { withFileTypes: true });
  const dirs = [];
  for (const e of entries) {
    if (!e.isDirectory() || e.name === 'sdk' || e.name === 'node_modules') continue;
    if (await exists(join(here, e.name, 'manifest.json'))) dirs.push(join(here, e.name));
  }
  return dirs;
}

async function main(argv) {
  const args = argv.slice(2);
  const noBuild = args.includes('--no-build');
  const outIdx = args.indexOf('--out');
  const out = outIdx === -1 ? undefined : resolve(args[outIdx + 1]);
  const positional = args.filter((a, i) => !a.startsWith('--') && i !== outIdx + 1);
  const targets = positional.length > 0 ? positional.map((p) => (p.includes('/') ? resolve(p) : join(here, p))) : await discover();
  if (out && targets.length !== 1) throw new Error('--out requires exactly one plugin directory');
  for (const dir of targets) {
    const r = await buildOne(dir, { build: !noBuild, out });
    console.log(`packed ${r.id}@${r.version} -> ${r.outPath} (${r.bytes} bytes)`);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv).catch((e) => {
    console.error(`build-all: ${e.message}`);
    process.exit(1);
  });
}
