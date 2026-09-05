#!/usr/bin/env node
// Version lockstep gate (issue #671 item 6).
//
// Every Petal release needs the version string to agree across nine places.
// This used to be checked ONLY inline inside release.yml's "Verify version
// lockstep" step, covering eight file-derived fields:
//   tauri.conf.json, Cargo.toml, apps/desktop/package.json,
//   web-harness/package.json, apps/desktop/package-lock.json (top-level +
//   packages['']), web-harness/package-lock.json (top-level + packages['']).
//
// That inline check is extracted here so it is importable/reusable, and a
// REAL ninth field is added that the old check never looked at:
// Cargo.lock's own `desktop` package version entry. Skip it and `cargo
// build` silently rewrites Cargo.lock mid-build to match Cargo.toml, which
// can trip scripts/run-with-source-provenance.sh --require-clean's
// clean-tree check during a release build.
//
// CLI:
//   node scripts/version-lockstep.mjs                     # self-check: expect
//                                                          # every field to match
//                                                          # tauri.conf.json's own
//                                                          # version (used by
//                                                          # scripts/ci-local.sh,
//                                                          # which has no tag)
//   node scripts/version-lockstep.mjs 0.8.4                # verify every field
//                                                          # equals exactly 0.8.4
//                                                          # (used by release.yml,
//                                                          # against the tag)
//   node scripts/version-lockstep.mjs 0.8.4 /path/to/repo  # against a different
//                                                          # checkout root (tests)

import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
export const DEFAULT_ROOT = path.resolve(__dirname, '..');

export const FILES = {
  tauriConf: 'apps/desktop/src-tauri/tauri.conf.json',
  cargoToml: 'apps/desktop/src-tauri/Cargo.toml',
  cargoLock: 'apps/desktop/src-tauri/Cargo.lock',
  desktopPackage: 'apps/desktop/package.json',
  desktopLock: 'apps/desktop/package-lock.json',
  webPackage: 'web-harness/package.json',
  webLock: 'web-harness/package-lock.json',
};

export function extractTauriVersion(text, label = FILES.tauriConf) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    throw new Error(`${label}: invalid JSON (${e.message})`);
  }
  if (typeof parsed.version !== 'string') {
    throw new Error(`${label}: no top-level "version" string`);
  }
  return parsed.version;
}

// package.json has the same shape as tauri.conf.json for this purpose: one
// top-level JSON "version" string.
export function extractPackageJsonVersion(text, label = 'package.json') {
  return extractTauriVersion(text, label);
}

export function extractCargoPackageVersion(text, label = FILES.cargoToml) {
  const match = text.match(/^version = "([^"]+)"/m);
  if (!match) throw new Error(`${label}: no top-level "version = ..." line found`);
  return match[1];
}

// Cargo.lock lists many packages, some of which may coincidentally sit at
// the same version string as `desktop` itself -- only the `[[package]] name
// = "desktop"` entry belongs to this repo.
export function extractCargoLockDesktopVersion(text, label = FILES.cargoLock) {
  const nameIndex = text.indexOf('name = "desktop"');
  if (nameIndex === -1) throw new Error(`${label}: no \`name = "desktop"\` entry found`);
  const match = text.slice(nameIndex).match(/^version = "([^"]+)"/m);
  if (!match) throw new Error(`${label}: no version line following \`name = "desktop"\``);
  return match[1];
}

export function extractPackageLockVersions(text, label = 'package-lock.json') {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    throw new Error(`${label}: invalid JSON (${e.message})`);
  }
  if (typeof parsed.version !== 'string') {
    throw new Error(`${label}: no top-level "version" string`);
  }
  const rootPackageVersion = parsed.packages?.['']?.version;
  if (typeof rootPackageVersion !== 'string') {
    throw new Error(`${label}: no packages[''].version string`);
  }
  return { top: parsed.version, rootPackage: rootPackageVersion };
}

export async function readVersions(root = DEFAULT_ROOT) {
  const read = (rel) => readFile(path.join(root, rel), 'utf8');

  const [tauriRaw, cargoRaw, cargoLockRaw, desktopPkgRaw, desktopLockRaw, webPkgRaw, webLockRaw] =
    await Promise.all([
      read(FILES.tauriConf),
      read(FILES.cargoToml),
      read(FILES.cargoLock),
      read(FILES.desktopPackage),
      read(FILES.desktopLock),
      read(FILES.webPackage),
      read(FILES.webLock),
    ]);

  const desktopLock = extractPackageLockVersions(desktopLockRaw, FILES.desktopLock);
  const webLock = extractPackageLockVersions(webLockRaw, FILES.webLock);

  return {
    tauri: extractTauriVersion(tauriRaw, FILES.tauriConf),
    cargo: extractCargoPackageVersion(cargoRaw, FILES.cargoToml),
    cargoLockDesktop: extractCargoLockDesktopVersion(cargoLockRaw, FILES.cargoLock),
    package: extractPackageJsonVersion(desktopPkgRaw, FILES.desktopPackage),
    desktopLock: desktopLock.top,
    desktopLockPackage: desktopLock.rootPackage,
    webPackage: extractPackageJsonVersion(webPkgRaw, FILES.webPackage),
    webLock: webLock.top,
    webLockPackage: webLock.rootPackage,
  };
}

// Returns [] if every field equals `expected`; otherwise an array of
// {field, value} mismatches.
export function findMismatches(versions, expected) {
  return Object.entries(versions)
    .filter(([, value]) => value !== expected)
    .map(([field, value]) => ({ field, value }));
}

export async function checkLockstep(expected, root = DEFAULT_ROOT) {
  const versions = await readVersions(root);
  const mismatches = findMismatches(versions, expected);
  return { versions, mismatches };
}

async function main() {
  const [, , versionArg, rootArg] = process.argv;
  const root = rootArg ? path.resolve(rootArg) : DEFAULT_ROOT;

  let expected = versionArg;
  if (!expected) {
    // Self-check mode (no tag to compare against, e.g. scripts/ci-local.sh):
    // tauri.conf.json is the source of truth every other field must agree
    // with.
    const tauriPath = path.join(root, FILES.tauriConf);
    expected = extractTauriVersion(await readFile(tauriPath, 'utf8'), FILES.tauriConf);
    console.log(`version-lockstep: self-check mode, expecting ${expected} (from ${FILES.tauriConf})`);
  }

  const { versions, mismatches } = await checkLockstep(expected, root);
  if (mismatches.length > 0) {
    console.error(`version-lockstep: mismatch (expected ${expected}): ${JSON.stringify(mismatches)}`);
    console.error(`version-lockstep: full field dump: ${JSON.stringify(versions)}`);
    process.exitCode = 1;
    return;
  }
  console.log(`Version lockstep verified: ${expected}`);
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url));
if (isMain) {
  main().catch((err) => {
    console.error(`version-lockstep: ${err.message}`);
    process.exitCode = 1;
  });
}
