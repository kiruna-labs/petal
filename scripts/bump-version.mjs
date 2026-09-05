#!/usr/bin/env node
// One-command version bump across every field that must move in lockstep for
// a Petal release (issue #671 item 6). Writes the same nine fields
// scripts/version-lockstep.mjs verifies:
//   1. apps/desktop/src-tauri/tauri.conf.json  "version"
//   2. apps/desktop/src-tauri/Cargo.toml       [package] version
//   3. apps/desktop/package.json               "version"
//   4. web-harness/package.json                "version"
//   5. apps/desktop/package-lock.json          top-level "version"
//   6. apps/desktop/package-lock.json          packages[""].version
//   7. web-harness/package-lock.json           top-level "version"
//   8. web-harness/package-lock.json           packages[""].version
//   9. apps/desktop/src-tauri/Cargo.lock       the `name = "desktop"` entry's
//      version -- the field the PRE-EXISTING lockstep gate never checked.
//      Skip it and `cargo build` silently rewrites Cargo.lock mid-build to
//      match Cargo.toml, which can trip
//      scripts/run-with-source-provenance.sh --require-clean's clean-tree
//      check.
//
// Deliberately uses targeted string replacement, NOT JSON.parse + stringify,
// for every file: the lockfiles and Cargo.lock are large, and a full
// parse/serialize round-trip risks reordering keys or reformatting far
// beyond the one field this script owns to touch, producing a diff no
// reviewer could usefully read. Only a scoped, position-bounded replace
// touches the exact version string -- and every replacement is verified
// afterwards (by re-parsing the JSON, or re-running the same extractor
// scripts/version-lockstep.mjs uses) so a wrong match fails loudly instead
// of silently corrupting the file.
//
// Usage:
//   node scripts/bump-version.mjs 0.8.4
//   node scripts/bump-version.mjs 0.8.4 --dry-run       # print the plan, write nothing
//   node scripts/bump-version.mjs 0.8.4 --root /path    # target a different checkout (tests)
//
// Does NOT commit, tag, or push -- that's the caller's job, e.g.:
//   node scripts/bump-version.mjs 0.8.4 && git add -A && \
//     git commit -m 'chore(release): bump version to 0.8.4' && \
//     git tag -a v0.8.4 -m '...' && git push --tags

import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  DEFAULT_ROOT,
  FILES,
  checkLockstep,
  extractTauriVersion,
  extractPackageJsonVersion,
  extractCargoPackageVersion,
  extractCargoLockDesktopVersion,
  extractPackageLockVersions,
} from './version-lockstep.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const VERSION_RE = /^\d+\.\d+\.\d+$/;

function replaceOnce(text, pattern, replacement, description) {
  if (!pattern.test(text)) {
    throw new Error(`bump-version: pattern not found for ${description}`);
  }
  return text.replace(pattern, replacement);
}

function replaceAt(text, index, length, replacement) {
  return text.slice(0, index) + replacement + text.slice(index + length);
}

// package-lock.json (npm lockfileVersion 3): the top-level "version" is the
// FIRST "version" field in the file (it always precedes "lockfileVersion" in
// a valid npm lockfile). The root package's own version is the first
// "version" field found inside its `packages: { "": { ... } }` entry --
// bounded to a small window right after that entry opens, so it can never
// wander into a `node_modules/...` entry's own "version" field later in the
// file (there are many of those, at the same literal version string, and
// touching one would be silently wrong).
function replacePackageLockVersions(text, version, label) {
  const lockfileVersionIdx = text.indexOf('"lockfileVersion"');
  if (lockfileVersionIdx === -1) throw new Error(`${label}: no "lockfileVersion" marker found`);

  const topMatch = text.slice(0, lockfileVersionIdx).match(/"version": "[^"]+"/);
  if (!topMatch) throw new Error(`${label}: no top-level "version" field found before lockfileVersion`);
  let result = replaceAt(text, topMatch.index, topMatch[0].length, `"version": "${version}"`);

  // Recompute against `result`: the replacement length may differ from the
  // original, shifting every later offset.
  const packagesIdx = result.indexOf('"packages": {');
  if (packagesIdx === -1) throw new Error(`${label}: no "packages" object found`);
  const rootEntryIdx = result.indexOf('"": {', packagesIdx);
  if (rootEntryIdx === -1) throw new Error(`${label}: no packages[''] root entry found`);
  const WINDOW = 2000;
  const window = result.slice(rootEntryIdx, Math.min(result.length, rootEntryIdx + WINDOW));
  const rootMatch = window.match(/"version": "[^"]+"/);
  if (!rootMatch) throw new Error(`${label}: no "version" field found inside packages[''] entry`);
  const absoluteIdx = rootEntryIdx + rootMatch.index;
  result = replaceAt(result, absoluteIdx, rootMatch[0].length, `"version": "${version}"`);

  return result;
}

function replaceCargoLockDesktopVersion(text, version, label) {
  const nameIdx = text.indexOf('name = "desktop"');
  if (nameIdx === -1) throw new Error(`${label}: no \`name = "desktop"\` entry found`);
  const match = text.slice(nameIdx).match(/^version = "[^"]+"/m);
  if (!match) throw new Error(`${label}: no version line following \`name = "desktop"\``);
  const absoluteIdx = nameIdx + match.index;
  return replaceAt(text, absoluteIdx, match[0].length, `version = "${version}"`);
}

// Each target: how to rewrite it, and how to re-extract the version(s) it
// should now hold (reusing scripts/version-lockstep.mjs's own extractors, so
// "did the write succeed" is checked the exact same way the lockstep gate
// checks it).
const TARGETS = [
  {
    rel: FILES.tauriConf,
    write: (text, version) => replaceOnce(text, /"version": "[^"]+"/, `"version": "${version}"`, FILES.tauriConf),
    verify: (text, label) => [['tauri', extractTauriVersion(text, label)]],
  },
  {
    rel: FILES.cargoToml,
    write: (text, version) => replaceOnce(text, /^version = "[^"]+"/m, `version = "${version}"`, FILES.cargoToml),
    verify: (text, label) => [['cargo', extractCargoPackageVersion(text, label)]],
  },
  {
    rel: FILES.desktopPackage,
    write: (text, version) =>
      replaceOnce(text, /"version": "[^"]+"/, `"version": "${version}"`, FILES.desktopPackage),
    verify: (text, label) => [['package', extractPackageJsonVersion(text, label)]],
  },
  {
    rel: FILES.webPackage,
    write: (text, version) => replaceOnce(text, /"version": "[^"]+"/, `"version": "${version}"`, FILES.webPackage),
    verify: (text, label) => [['webPackage', extractPackageJsonVersion(text, label)]],
  },
  {
    rel: FILES.desktopLock,
    write: (text, version) => replacePackageLockVersions(text, version, FILES.desktopLock),
    verify: (text, label) => {
      const v = extractPackageLockVersions(text, label);
      return [
        ['desktopLock', v.top],
        ['desktopLockPackage', v.rootPackage],
      ];
    },
  },
  {
    rel: FILES.webLock,
    write: (text, version) => replacePackageLockVersions(text, version, FILES.webLock),
    verify: (text, label) => {
      const v = extractPackageLockVersions(text, label);
      return [
        ['webLock', v.top],
        ['webLockPackage', v.rootPackage],
      ];
    },
  },
  {
    rel: FILES.cargoLock,
    write: (text, version) => replaceCargoLockDesktopVersion(text, version, FILES.cargoLock),
    verify: (text, label) => [['cargoLockDesktop', extractCargoLockDesktopVersion(text, label)]],
  },
];

function parseArgs(argv) {
  const args = argv.slice(2);
  const dryRun = args.includes('--dry-run');
  const rootFlagIdx = args.indexOf('--root');
  const root = rootFlagIdx !== -1 ? path.resolve(args[rootFlagIdx + 1]) : DEFAULT_ROOT;
  const version = args.find((a) => VERSION_RE.test(a));
  return { version, dryRun, root };
}

async function main() {
  const { version, dryRun, root } = parseArgs(process.argv);
  if (!version) {
    console.error('usage: node scripts/bump-version.mjs <MAJOR.MINOR.PATCH> [--dry-run] [--root <path>]');
    process.exitCode = 64;
    return;
  }

  const results = [];
  for (const target of TARGETS) {
    const filePath = path.join(root, target.rel);
    const before = await readFile(filePath, 'utf8');
    const after = target.write(before, version);

    // Verify against the IN-MEMORY result before writing anything, using the
    // same extractors the lockstep gate uses -- this is what makes --dry-run
    // a real check, not just a print statement, and what catches a wrong
    // match before a single byte hits disk.
    const found = target.verify(after, target.rel);
    for (const [field, value] of found) {
      if (value !== version) {
        throw new Error(
          `bump-version: wrote ${target.rel} but re-extracting "${field}" afterwards found ` +
            `${JSON.stringify(value)}, not ${JSON.stringify(version)} -- refusing to leave a corrupt file`
        );
      }
    }

    results.push({ rel: target.rel, filePath, before, after, changed: before !== after });
  }

  for (const r of results) {
    const verb = r.changed ? 'updated' : `already at ${version}`;
    console.log(`${dryRun ? '[dry-run] ' : ''}${r.rel}: ${verb}`);
  }

  if (dryRun) {
    console.log(`bump-version: dry run OK -- all 9 fields would read ${version}`);
    return;
  }

  for (const r of results) {
    if (r.changed) await writeFile(r.filePath, r.after, 'utf8');
  }

  // Final on-disk confirmation via the real lockstep gate (belt-and-braces:
  // the in-memory verify above already proved this, but this also catches a
  // write that silently failed or targeted the wrong path).
  const { mismatches } = await checkLockstep(version, root);
  if (mismatches.length > 0) {
    console.error(`bump-version: post-write lockstep check FAILED: ${JSON.stringify(mismatches)}`);
    process.exitCode = 1;
    return;
  }

  console.log(`bump-version: all 9 fields now read ${version}`);
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url));
if (isMain) {
  main().catch((err) => {
    console.error(`bump-version: ${err.message}`);
    process.exitCode = 1;
  });
}
