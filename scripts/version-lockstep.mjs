#!/usr/bin/env node
// Version lockstep gate (issue #671 item 6; SBOM mirrors added for #131).
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
// #131 adds a TENTH place, split across three committed CycloneDX manifests:
// sbom/desktop-npm.cdx.json, sbom/desktop-rust.cdx.json and
// sbom/web-harness-npm.cdx.json each embed the product version in their
// metadata.component. `scripts/bump-version.mjs` deliberately does NOT
// regenerate them (that needs node_modules in four npm roots plus
// cargo-cyclonedx -- far heavier than a version bump), so nothing used to
// tell the releaser they had gone stale and the SBOM workflow went red on
// every push from 0.9.9 onwards. Checking them here puts the failure where
// release.yml's "Verify version lockstep" step already runs, before a build
// rather than after a push. sbom/backend-npm.cdx.json and
// sbom/site-npm.cdx.json are NOT checked: backend and site carry their own
// independent versions (0.1.0 / 0.0.1) and are not release mirrors.
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

// The committed SBOMs that mirror the product version (#131). Keyed by the
// lockstep field name they contribute, so a mismatch names the file that
// drifted. Deliberately excludes sbom/backend-npm.cdx.json and
// sbom/site-npm.cdx.json -- those roots version independently of a release.
export const SBOM_FILES = {
  sbomDesktopNpm: 'sbom/desktop-npm.cdx.json',
  sbomDesktopRust: 'sbom/desktop-rust.cdx.json',
  sbomWebHarnessNpm: 'sbom/web-harness-npm.cdx.json',
};

export const SBOM_FIELDS = Object.keys(SBOM_FILES);

// Printed verbatim when an SBOM field is among the mismatches, so the fix is
// actionable without reading issue #131. Mentions the prereqs because
// generate-sbom.sh exits early without them.
export const SBOM_REGENERATE_HINT = [
  'version-lockstep: the committed SBOMs under sbom/ embed the product version and are stale.',
  'version-lockstep: bump-version.mjs does not regenerate them (too heavy for a bump). Fix with:',
  'version-lockstep:',
  'version-lockstep:     bash scripts/generate-sbom.sh',
  'version-lockstep:',
  'version-lockstep: prerequisites: cargo-cyclonedx 0.5.x (cargo install cargo-cyclonedx),',
  'version-lockstep: npm 11, and node_modules present in apps/desktop, backend, web-harness',
  "version-lockstep: and site ('npm ci --ignore-scripts' in each is enough).",
  'version-lockstep: then commit the regenerated sbom/*.cdx.json alongside the version bump.',
].join('\n');

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

// CycloneDX purls embed the version after the last "@" of the name portion,
// ahead of any ?qualifiers or #subpath: pkg:npm/desktop@0.9.15,
// pkg:cargo/desktop@0.9.15?download_url=file://. -- and scoped npm names put
// an earlier "@" in the path, so take the LAST one.
export function extractPurlVersion(purl) {
  if (typeof purl !== 'string') return undefined;
  const namePart = purl.split('#')[0].split('?')[0];
  const at = namePart.lastIndexOf('@');
  if (at === -1) return undefined;
  return namePart.slice(at + 1) || undefined;
}

// The product version a committed CycloneDX SBOM claims (#131). The manifest
// describes its own root component in metadata.component; the version is
// repeated in that component's purl, so cross-check the two rather than
// trusting one field -- a hand-edit of only one of them is exactly the kind
// of half-fixed manifest this gate exists to reject.
export function extractSbomProductVersion(text, label = 'sbom/*.cdx.json') {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    throw new Error(`${label}: invalid JSON (${e.message})`);
  }
  const component = parsed?.metadata?.component;
  if (!component || typeof component !== 'object') {
    throw new Error(`${label}: no metadata.component (not a CycloneDX SBOM?)`);
  }
  if (typeof component.version !== 'string') {
    throw new Error(`${label}: no metadata.component.version string`);
  }
  const purlVersion = extractPurlVersion(component.purl);
  if (purlVersion !== undefined && purlVersion !== component.version) {
    throw new Error(
      `${label}: internally inconsistent -- metadata.component.version is ` +
        `${JSON.stringify(component.version)} but its purl says ${JSON.stringify(purlVersion)}. ` +
        `Regenerate rather than hand-edit: bash scripts/generate-sbom.sh`
    );
  }
  return component.version;
}

// `includeSboms: false` restricts the result to the nine fields
// bump-version.mjs itself writes -- it deliberately does not regenerate the
// SBOMs, so its own post-write self-check must not demand them (#131).
export async function readVersions(root = DEFAULT_ROOT, { includeSboms = true } = {}) {
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

  const sboms = {};
  if (includeSboms) {
    const rawSboms = await Promise.all(
      SBOM_FIELDS.map(async (field) => {
        const rel = SBOM_FILES[field];
        try {
          return await read(rel);
        } catch (e) {
          if (e?.code === 'ENOENT') {
            throw new Error(`${rel}: missing -- generate it with: bash scripts/generate-sbom.sh`);
          }
          throw e;
        }
      })
    );
    SBOM_FIELDS.forEach((field, i) => {
      sboms[field] = extractSbomProductVersion(rawSboms[i], SBOM_FILES[field]);
    });
  }

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
    ...sboms,
  };
}

// Returns [] if every field equals `expected`; otherwise an array of
// {field, value} mismatches.
export function findMismatches(versions, expected) {
  return Object.entries(versions)
    .filter(([, value]) => value !== expected)
    .map(([field, value]) => ({ field, value }));
}

export async function checkLockstep(expected, root = DEFAULT_ROOT, options = {}) {
  const versions = await readVersions(root, options);
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
    if (mismatches.some((m) => SBOM_FIELDS.includes(m.field))) {
      console.error(SBOM_REGENERATE_HINT);
    }
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
