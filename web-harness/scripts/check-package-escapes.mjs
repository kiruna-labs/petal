#!/usr/bin/env node
//
// #93: no module in web-harness/ may IMPORT a file outside web-harness/ by a
// relative path. `scripts/deploy-web-harness.sh` stages a copy of this package
// alone (symlinks dereferenced, #662), so a specifier that walks out of the
// package resolves to nothing there -- `shared/` reached as
// `../../../../shared/...` instead of through the `@petal/shared` alias is
// exactly how `scripts/ci-local.sh` went red on main, as a TS2307 buried in
// the isolated-deploy simulation. This runs first in `npm run build` so the
// same mistake fails in one second with a fix, in every place that build runs.
//
// Reaching shared/ or plugins/ through the in-package `shared`/`plugins`
// symlinks is fine and is the supported route: `rsync -L` turns them into real
// directories inside the staged copy. Only paths that leave the package break.
// Runtime `new URL('../../contracts/...', import.meta.url)` reads in tests are
// NOT module specifiers and are not checked: the staged build never runs the
// test suite, and those files really are read from a full checkout.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const PACKAGE_ROOT = resolve(process.argv[2] ?? fileURLToPath(new URL('..', import.meta.url)));
const SKIP_DIRS = new Set(['node_modules', 'dist', '.vercel', '.turbo', '.git', 'coverage']);
const SCANNED = new Set(['.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs', '.svelte', '.css', '.html']);

/** Module-specifier syntax only -- see the header note on `new URL`. */
const SPECIFIER_PATTERNS = [
  // import x from '...' / export * from '...' / import '...'
  /\b(?:import|export)\s[^;'"`]*?from\s*['"]([^'"]+)['"]/g,
  /\bimport\s*['"]([^'"]+)['"]/g,
  // import('...') and require('...')
  /\b(?:import|require)\s*\(\s*['"]([^'"]+)['"]\s*\)/g,
  // CSS @import '...' / @import url('...')
  /@import\s+(?:url\(\s*)?['"]([^'"]+)['"]/g,
  // <script src="..."> / <link href="..."> in the fixture + entry HTML
  /<(?:script|link|img)\b[^>]*?\b(?:src|href)\s*=\s*["']([^"']+)["']/gi,
];

// Comments are blanked so a commented-out import can't fail the build. `//` is
// only a comment in JS-like sources: in CSS/HTML it is the middle of `https://`,
// and truncating there would hide a real specifier later on the same line.
function stripComments(source, { lineComments }) {
  let out = '';
  let inBlock = false;
  for (const line of source.split(/\r?\n/)) {
    let kept = '';
    for (let i = 0; i < line.length; i += 1) {
      if (inBlock) {
        if (line[i] === '*' && line[i + 1] === '/') {
          inBlock = false;
          i += 1;
        }
        continue;
      }
      if (line[i] === '/' && line[i + 1] === '*') {
        inBlock = true;
        i += 1;
        continue;
      }
      if (lineComments && line[i] === '/' && line[i + 1] === '/') break;
      kept += line[i];
    }
    out += `${kept}\n`;
  }
  return out;
}

/** Walk real directories only: `shared`/`plugins` are symlinks out of the package. */
function* sourceFiles(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    if (entry.isSymbolicLink()) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name)) continue;
      yield* sourceFiles(full);
      continue;
    }
    if (!entry.isFile()) continue;
    const dot = entry.name.lastIndexOf('.');
    if (dot > 0 && SCANNED.has(entry.name.slice(dot))) yield full;
  }
}

/** Lexical, NOT realpath: `shared` is a symlink whose target is outside on purpose. */
function escapes(fromFile, specifier) {
  const target = resolve(dirname(fromFile), specifier.split('?')[0]);
  const rel = relative(PACKAGE_ROOT, target);
  return rel === '..' || rel.startsWith(`..${sep}`);
}

function lineOf(source, index) {
  return source.slice(0, index).split('\n').length;
}

if (!statSync(PACKAGE_ROOT, { throwIfNoEntry: false })?.isDirectory()) {
  console.error(`check-package-escapes: not a directory: ${PACKAGE_ROOT}`);
  process.exit(2);
}

const failures = [];
for (const file of sourceFiles(PACKAGE_ROOT)) {
  const jsLike = /\.(?:ts|tsx|js|jsx|mjs|cjs|svelte)$/.test(file);
  const source = stripComments(readFileSync(file, 'utf8'), { lineComments: jsLike });
  const seen = new Set();
  for (const pattern of SPECIFIER_PATTERNS) {
    pattern.lastIndex = 0;
    let match;
    while ((match = pattern.exec(source)) !== null) {
      const specifier = match[1];
      if (!specifier.startsWith('./') && !specifier.startsWith('../')) continue;
      if (!escapes(file, specifier)) continue;
      const key = `${match.index}:${specifier}`;
      if (seen.has(key)) continue;
      seen.add(key);
      failures.push({
        file: relative(PACKAGE_ROOT, file),
        line: lineOf(source, match.index),
        specifier,
      });
    }
  }
}

if (failures.length > 0) {
  console.error(
    '\nWEB-HARNESS PACKAGE-ESCAPE GATE BLOCKED: a module reaches outside web-harness/ by a relative path (#93).\n'
  );
  for (const failure of failures.sort((a, b) => a.file.localeCompare(b.file) || a.line - b.line)) {
    console.error(`  web-harness/${failure.file}:${failure.line}  imports  '${failure.specifier}'`);
  }
  console.error(
    [
      '',
      'The deploy stages web-harness/ ALONE (scripts/deploy-web-harness.sh, #662), so a',
      'specifier that walks out of the package resolves to nothing there and the build',
      'fails with a confusing TS2307 inside the isolated-deploy simulation.',
      '',
      "Fix: import shared/ through the alias -- '@petal/shared/<path>' (vite.config.ts +",
      "tsconfig.json paths) -- and reach plugins/ through the in-package 'plugins'",
      'symlink. Both survive staging because rsync -L materializes them in the copy.',
      '',
    ].join('\n')
  );
  process.exit(1);
}

console.log(`check-package-escapes: OK -- no relative import escapes ${relative(process.cwd(), PACKAGE_ROOT) || '.'}`);
