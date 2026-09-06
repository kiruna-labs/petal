// Publish a built Petal release to Vercel Blob (issue #102/#103/#104/#671).
//
// Invoked by .github/workflows/release.yml after macOS and Windows `tauri build`.
// Uploads, at STABLE public paths (no random suffix, overwrite-in-place
// EXCEPT the versioned artifacts, which never collide across releases):
//   - Petal_<version>_universal.dmg              the human download (/api/download 302s here)
//   - Petal_<version>_universal.app.tar.gz (+.sig)  the auto-updater artifact + minisign
//                                                 signature (versioned since #671 -- was a
//                                                 single unversioned pathname that a NEW
//                                                 release silently overwrote, destroying the
//                                                 previous release's rollback artifact)
//   - latest.json                                the combined Tauri updater manifest
//                                                 (/api/updater serves this)
//
// The manifest is written last and refuses to publish a version <= the live one
// (#671 item 5), so a platform artifact cannot be advertised before its bytes
// and signature are present.
//
// The backend only READS these blobs; this script is the only writer. The
// DMG and Windows installer pathnames are contracts used by backend download
// routing; updater artifact pathnames are referenced literally by latest.json.
//
// Env:
//   BLOB_READ_WRITE_TOKEN  Vercel Blob RW token (same store the backend reads)
//   VERSION                e.g. "0.1.0"
//   TAG                    e.g. "v0.1.0" (used as release notes fallback)
//   TAG_ANNOTATION          the annotated tag's message, if any (release notes source,
//                           #671 item 7) -- read by release.yml from the real git
//                           checkout, since this script runs from a scratch npm dir
//                           with no .git of its own (see BUNDLE_DIR note below)
//   BUNDLE_DIR             path to `.../release/bundle` from the universal build
//   WINDOWS_BUNDLE_DIR     staging directory containing the verified .exe/.sig
// Run with NODE_PATH pointing at a node_modules that has @vercel/blob.

import { put, list } from '@vercel/blob';
import { readFile, readdir, mkdtemp, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import {
  assertUpdaterTarballIsStapled,
  isDowngradeOrSame,
  updaterTarballPathname,
  windowsInstallerPathname,
  buildUpdaterManifest,
  resolveReleaseNotes,
  valueIsBakedInSlice,
} from './publish-blob-lib.mjs';

const execFileAsync = promisify(execFile);

const token = requireEnv('BLOB_READ_WRITE_TOKEN');
const version = requireEnv('VERSION');
const tag = process.env.TAG || `v${version}`;
const tagAnnotation = process.env.TAG_ANNOTATION || '';
const bundleDir = requireEnv('BUNDLE_DIR');
// Optional: absent means a macOS-only publish (Windows lane paused, user
// directive 2026-08-25) -- latest.json then carries no windows-x86_64 entry.
const windowsBundleDir = process.env.WINDOWS_BUNDLE_DIR || '';

function requireEnv(name) {
  const v = process.env[name];
  if (!v) {
    console.error(`publish-blob: missing required env ${name}`);
    process.exit(1);
  }
  return v;
}

// Find exactly one file in `dir` whose name ends with `suffix`.
async function findOne(dir, suffix) {
  let entries;
  try {
    entries = await readdir(dir);
  } catch (e) {
    throw new Error(`cannot read ${dir}: ${e.message}`);
  }
  const hits = entries.filter((f) => f.endsWith(suffix));
  if (hits.length === 0) throw new Error(`no *${suffix} found in ${dir}`);
  if (hits.length > 1) throw new Error(`multiple *${suffix} in ${dir}: ${hits.join(', ')}`);
  return path.join(dir, hits[0]);
}

async function upload(pathname, data, contentType) {
  // latest.json is the one pathname that is OVERWRITTEN each release and read
  // through the Blob CDN by /api/updater; the default cache-control is 30
  // days. Keep it to 60 s so a release is visible within a minute (the 0.9.7
  // publish was followed by minutes of the CDN still serving 0.9.4). The
  // versioned installers never change, so they keep the default.
  const cacheControlMaxAge = pathname === 'latest.json' ? 60 : undefined;
  const { url } = await put(pathname, data, {
    access: 'public',
    addRandomSuffix: false,
    allowOverwrite: true,
    contentType,
    token,
    ...(cacheControlMaxAge !== undefined ? { cacheControlMaxAge } : {}),
  });
  console.log(`  uploaded ${pathname} -> ${url}`);
  return url;
}

// Guard against a real bug hit shipping v0.4.0 (2026-07-05): if the build
// environment's Petal.app carries `com.apple.provenance` (or other) extended
// attributes, `tar` (unless COPYFILE_DISABLE=1 is set) writes a companion
// AppleDouble sidecar entry (`._<name>`) for every single archived file.
// `tar tzf`/BSD tar's own listing hides these transparently, so they're easy
// to miss by eye -- but Tauri's Rust-based updater plugin extracts every
// archive member literally and chokes trying to unpack one
// ("failed to unpack `._Petal.app`"), silently bricking auto-update for
// every user already on a previous version. Refuse to publish a tarball
// that contains one.
async function verifyCleanTarball(tarPath) {
  // `tar tzf` on macOS transparently hides AppleDouble sidecars from its own
  // listing, so use Python's tarfile (stdlib, always present on this build
  // Mac) to see every literal archive member the way Tauri's extractor will.
  let pyStdout;
  try {
    ({ stdout: pyStdout } = await execFileAsync('python3', [
      '-c',
      "import sys, tarfile; print('\\n'.join(tarfile.open(sys.argv[1], 'r:gz').getnames()))",
      tarPath,
    ]));
  } catch (e) {
    throw new Error(`AppleDouble gate: could not inspect ${tarPath} with python3 tarfile: ${e.message}`);
  }
  const appleDoubleEntries = pyStdout
    .split('\n')
    .filter((name) => path.basename(name).startsWith('._'));
  if (appleDoubleEntries.length > 0) {
    throw new Error(
      `AppleDouble gate: refusing to publish ${path.basename(tarPath)}; it contains ` +
        `${appleDoubleEntries.length} macOS metadata sidecar entries (e.g. ${appleDoubleEntries[0]}) ` +
        `that will fail to unpack in Tauri's updater. Rebuild with COPYFILE_DISABLE=1 set.`
    );
  }
  console.log(`  AppleDouble gate: OK (0 sidecar entries)`);
}

async function verifyUniversalApp(bundleDir) {
  const executablePath = path.join(bundleDir, 'macos', 'Petal.app', 'Contents', 'MacOS', 'desktop');
  let stdout;
  try {
    ({ stdout } = await execFileAsync('lipo', ['-archs', executablePath]));
  } catch (e) {
    throw new Error(`universal gate: lipo could not read architectures for ${executablePath}: ${e.message}`);
  }

  const archs = stdout.trim().split(/\s+/).filter(Boolean);
  const hasArm64 = archs.includes('arm64');
  const hasX86 = archs.includes('x86_64');
  if (!hasArm64 || !hasX86) {
    throw new Error(
      `universal gate: refusing to publish ${path.basename(executablePath)} as universal; ` +
        `expected x86_64 and arm64, got: ${archs.join(' ') || '(none)'}`
    );
  }
  console.log(`  universal gate: OK (${archs.join(' ')})`);
}

// GitHub #915: the built app must carry the Apple Events automation
// entitlement, or the shared-browser-window Open URL feature silently never
// works -- the hardened runtime denies osascript's Apple Events with no
// prompt at all when the entitlement is absent, and nothing else in this
// publisher would ever notice. `scripts/verify-universal-app.sh` (a
// SEPARATE script, run as its own CI step before this one -- see
// docs/RELEASING.md) checks the same thing for the CI/local release-guard
// path; this gate exists so `publish-blob.mjs` itself refuses a bundle
// missing the entitlement even if invoked on its own.
async function verifyEntitlements(bundleDir) {
  const appPath = path.join(bundleDir, 'macos', 'Petal.app');
  let stdout;
  try {
    ({ stdout } = await execFileAsync('codesign', ['-d', '--entitlements', ':-', appPath]));
  } catch (e) {
    throw new Error(`entitlements gate: codesign could not read entitlements for ${appPath}: ${e.message}`);
  }
  const entitlementKey = 'com.apple.security.automation.apple-events';
  if (!new RegExp(`<key>${entitlementKey.replace(/\./g, '\\.')}</key>\\s*<true/>`).test(stdout)) {
    throw new Error(
      `entitlements gate: refusing to publish ${path.basename(appPath)}; missing ${entitlementKey} set to ` +
        `true (#915 -- the shared-browser-window Open URL feature needs Apple Events to read a shared ` +
        `browser window's URL). Add it to Entitlements.plist and rebuild.`
    );
  }
  console.log(`  entitlements gate: OK (${entitlementKey} = true)`);
}

// Shared per-slice baked-value check (#874). A universal binary's arch
// slices are independently compiled, so `strings` on the FAT executable --
// what the Sentry/PostHog gates used to do -- passes if the value appears in
// EITHER slice. That means an x86_64 slice built without
// PETAL_SENTRY_DSN/PETAL_POSTHOG_KEY/PETAL_BACKEND_URL would still publish
// clean off the arm64 slice alone, shipping Intel users a crash-blind (or, for
// the backend URL, join-broken) binary. Worse, `strings | grep` on the
// x86_64 slice alone is *itself* unreliable: LLVM sometimes materializes a
// string literal as a sequence of inline `movabs $imm64` immediates rather
// than a contiguous byte run, so a whole-value grep finds zero matches for a
// genuinely-baked value (see docs/RELEASING.md's "Known local-build
// gotchas"). `valueIsBakedInSlice` (publish-blob-lib.mjs) handles both
// shapes; this function does the I/O: `lipo -archs` to enumerate slices,
// `lipo -thin` each one into a scratch temp file, and check it.
//
// `expectedValue`, when given, is checked with the full contiguous-or-chunked
// matcher. When no concrete expected value is available (the Sentry/PostHog
// env vars are secrets not read by this script, only baked at build time),
// `fallbackPattern` is matched against `strings` output for that slice
// instead -- contiguous-only, since chunk reconstruction needs a concrete
// value to split into chunks. That fallback is noted in the OK line so a
// publish log never silently under-claims what it actually verified.
async function verifyValueBakedInAllSlices(bundleDir, { gateName, expectedValue, fallbackPattern, remediation }) {
  const executablePath = path.join(bundleDir, 'macos', 'Petal.app', 'Contents', 'MacOS', 'desktop');
  let archsStdout;
  try {
    ({ stdout: archsStdout } = await execFileAsync('lipo', ['-archs', executablePath]));
  } catch (e) {
    throw new Error(`${gateName}: lipo could not read architectures for ${executablePath}: ${e.message}`);
  }
  const archs = archsStdout.trim().split(/\s+/).filter(Boolean);
  if (archs.length === 0) {
    throw new Error(`${gateName}: lipo reported no architectures for ${executablePath}`);
  }

  const tmpDir = await mkdtemp(path.join(os.tmpdir(), 'petal-publish-blob-'));
  let usedFallback = false;
  try {
    const perArchDetail = [];
    for (const arch of archs) {
      const slicePath = path.join(tmpDir, `slice-${arch}`);
      try {
        await execFileAsync('lipo', ['-thin', arch, executablePath, '-output', slicePath]);
      } catch (e) {
        throw new Error(`${gateName}: lipo could not thin the ${arch} slice: ${e.message}`);
      }

      if (expectedValue) {
        const sliceBuffer = await readFile(slicePath);
        const result = valueIsBakedInSlice(sliceBuffer, expectedValue);
        if (!result.baked) {
          throw new Error(
            `${gateName}: refusing to publish ${path.basename(executablePath)}; the ${arch} slice does ` +
              `not carry the expected value (${result.detail}). ${remediation}`
          );
        }
        perArchDetail.push(`${arch}: ${result.detail}`);
      } else {
        usedFallback = true;
        let stringsStdout;
        try {
          // -a: scan the WHOLE slice, not just macOS `strings`' default
          // loaded/initialized-data sections. Confirmed load-bearing while
          // validating this gate against the real 0.9.1 universal binary
          // (#874 task 4): the PostHog token's x86_64 movabs-immediate bytes
          // live outside the sections plain `strings` covers by default --
          // `strings` (no -a) finds 0 hits there while the literal bytes are
          // genuinely present (`strings -a` finds them). Without -a this
          // fallback would misreport a baked value as missing.
          ({ stdout: stringsStdout } = await execFileAsync('strings', ['-a', slicePath], {
            maxBuffer: 200 * 1024 * 1024,
          }));
        } catch (e) {
          throw new Error(`${gateName}: could not run strings on the ${arch} slice: ${e.message}`);
        }
        if (!fallbackPattern.test(stringsStdout)) {
          // A chunked (movabs) bake NEVER matches this contiguous fallback --
          // confirmed on the real 0.9.1 x86_64 slice, whose PostHog token is
          // chunk-only (#874). So this failure has two readings: the value is
          // genuinely missing, OR it is baked chunked and only the expected
          // env value would let us prove it. Say so, or the operator chases a
          // phantom missing bake.
          throw new Error(
            `${gateName}: refusing to publish ${path.basename(executablePath)}; the ${arch} slice has no ` +
              `contiguous string matching ${fallbackPattern}. If this build WAS made with the value set, ` +
              `export the expected value in the environment and re-run -- chunked (movabs) bakes on this ` +
              `slice are only verifiable by chunk reconstruction, which needs the concrete value (#874). ` +
              `${remediation}`
          );
        }
        perArchDetail.push(`${arch}: contiguous`);
      }
    }
    const suffix = usedFallback
      ? ' [no expected value in env -- chunked reconstruction skipped, contiguous-only check per slice]'
      : '';
    console.log(`  ${gateName}: OK (${perArchDetail.join('; ')})${suffix}`);
  } finally {
    await rm(tmpDir, { recursive: true, force: true });
  }
}

// Guard against a real gap confirmed live (#681, 2026-08-06): Petal's release
// posture builds and publishes locally (cloud CI is manual-only), and
// `logging.rs:1427 sentry_dsn()` reads `option_env!("PETAL_SENTRY_DSN")`,
// baked in at compile time by `build.rs:88-92`. Nothing stopped a local build
// that forgot to export PETAL_SENTRY_DSN from publishing anyway -- shipping a
// binary with crash reporting silently compiled out and no signal that it
// happened.
//
// Per-slice (#874): the old version ran `strings` over the FAT binary, which
// passes on an arm64-only bake. `PETAL_SENTRY_DSN` is a secret this script
// does not otherwise need, so when it IS present in env the full
// contiguous-or-chunked check runs against the real value; when it's absent
// (the common case -- CI/local builds legitimately don't set it), each slice
// still gets the regex fallback so at least contiguous presence is checked
// per-arch rather than fat-binary-wide.
async function verifySentryDsn(bundleDir) {
  await verifyValueBakedInAllSlices(bundleDir, {
    gateName: 'Sentry DSN gate',
    expectedValue: process.env.PETAL_SENTRY_DSN?.trim() || null,
    fallbackPattern: /ingest.*sentry\.io/i,
    remediation:
      'This build was compiled without PETAL_SENTRY_DSN set for that slice, so crash reporting is ' +
      'silently a no-op on it. Rebuild locally with PETAL_SENTRY_DSN=<dsn> set for BOTH targets ' +
      '(see docs/RELEASING.md) before publishing.',
  });
}

// Same class as the Sentry gate: a release built without PETAL_POSTHOG_KEY
// ships with product events compiled off and no signal that it happened.
// The source contains the four-character prefix check `phc_`; a real project
// token is much longer, so require that full shape in the binary. Per-slice
// for the same reason as the Sentry gate above.
async function verifyPosthogKey(bundleDir) {
  await verifyValueBakedInAllSlices(bundleDir, {
    gateName: 'PostHog key gate',
    expectedValue: process.env.PETAL_POSTHOG_KEY?.trim() || null,
    fallbackPattern: /phc_[A-Za-z0-9]{20,}/,
    remediation:
      'This build was compiled without PETAL_POSTHOG_KEY set for that slice, so product events are ' +
      'silently a no-op on it. Rebuild with PETAL_POSTHOG_KEY set for BOTH targets (see ' +
      'docs/RELEASING.md).',
  });
}

// NEW gate (#874): PETAL_BACKEND_URL previously had no publish-time check at
// all -- only `build.rs`'s compile-time panic, which fires per cargo
// invocation and cannot see a stale or partially-cached SECOND target (e.g.
// an x86_64 target dir left over from before the env var was set, or built
// in a separate step that forgot to re-export it). That is the exact 0.8.2
// failure mode: a published, notarized, signed release where every join
// failed with "no token backend is configured". Unlike the Sentry/PostHog
// secrets, the expected value is a public constant (docs/RELEASING.md's
// documented release recipe always bakes the same production backend), so
// this gate always runs the full contiguous-or-chunked check on every slice
// -- there is no fallback path.
const PETAL_BACKEND_URL_EXPECTED = 'https://app.petal.live';
async function verifyBackendUrl(bundleDir) {
  await verifyValueBakedInAllSlices(bundleDir, {
    gateName: 'Backend URL gate',
    expectedValue: PETAL_BACKEND_URL_EXPECTED,
    fallbackPattern: null,
    remediation:
      `This build's slice does not carry PETAL_BACKEND_URL=${PETAL_BACKEND_URL_EXPECTED}. Rebuild ` +
      'with PETAL_BACKEND_URL set for BOTH targets (see docs/RELEASING.md) -- this is exactly how ' +
      '0.8.2 shipped with every join broken.',
  });
}

// Guard against a VERIFIED defect in the LOCAL release procedure (2026-08-10):
// `docs/RELEASING.md` step 1 built `Petal.app.tar.gz`, step 2 notarized and
// stapled the `.app`, and step 6 published the tarball from step 1 -- nothing
// regenerated it in between, so the shipped auto-update artifact contained an
// UNSTAPLED app, which fails an offline Gatekeeper check on the user's Mac.
// `docs/RELEASING.md` step 2b now re-tars after stapling; this gate is what
// makes forgetting it impossible.
//
// CI is NOT affected, and this gate does not block it. Verified against
// tauri-cli v2.11.4 (`crates/tauri-bundler/src/bundle.rs`): `bundle_project()`
// runs every package type in its loop first -- `PackageType::MacOsBundle` ->
// `macos::app::bundle_project()`, which signs and then notarizes+staples
// inline whenever APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID are set -- and only
// AFTER that loop calls `updater_bundle::bundle_project()`, whose
// `bundle_update_macos()` tars the `.app` path on disk. So CI's tarball is
// made from the already-stapled bundle. The local build passes no APPLE_ID,
// so nothing staples during the build at all -- hence the manual step.
//
// Presence of a ticket is not sufficient evidence: a stale ticket from a
// previous version would still be present and still be wrong. So validate the
// on-disk app first (proves a ticket valid for THIS cdhash), then compare the
// ticket bytes carried inside the tarball against it.
async function verifyStapledInsideTarball(bundleDir, tarPath) {
  const appPath = path.join(bundleDir, 'macos', 'Petal.app');
  // `xcrun stapler staple <app>` writes the ticket here (file magic `s8ch`).
  // Not to be confused with the code signature's own
  // `Contents/_CodeSignature/CodeResources`.
  const ticketRelPath = path.join('Contents', 'CodeResources');

  try {
    await execFileAsync('xcrun', ['stapler', 'validate', appPath]);
  } catch (e) {
    const detail = `${e.stderr || ''}${e.stdout || ''}${e.message || ''}`;
    if (e.code === 'ENOENT' || /unable to find utility|command not found|xcrun: error: invalid active/i.test(detail)) {
      // A missing/broken tool must never read as "unstapled" -- distinct error.
      throw new Error(
        `staple gate: could not RUN \`xcrun stapler validate\`, so the staple could not be ` +
          `checked either way: ${detail.trim()}`
      );
    }
    throw new Error(
      `staple gate: refusing to publish; \`xcrun stapler validate ${appPath}\` failed, so the ` +
        `built app carries no notarization ticket valid for its own cdhash. Notarize + staple ` +
        `the .app (docs/RELEASING.md step 2) before publishing. stapler said: ${detail.trim()}`
    );
  }

  let diskTicket;
  try {
    diskTicket = await readFile(path.join(appPath, ticketRelPath));
  } catch (e) {
    throw new Error(
      `staple gate: \`stapler validate\` passed but ${path.join(appPath, ticketRelPath)} could ` +
        `not be read: ${e.message}`
    );
  }
  const diskTicketSha256 = createHash('sha256').update(diskTicket).digest('hex');

  // python3's tarfile (stdlib, always present on the build Mac) reads the
  // archive member literally, the same way `verifyCleanTarball` does.
  const member = `Petal.app/${ticketRelPath}`;
  let pyStdout;
  try {
    ({ stdout: pyStdout } = await execFileAsync('python3', [
      '-c',
      'import sys, tarfile, hashlib\n' +
        "t = tarfile.open(sys.argv[1], 'r:gz')\n" +
        'try:\n' +
        '    m = t.getmember(sys.argv[2])\n' +
        'except KeyError:\n' +
        "    print('MISSING'); raise SystemExit(0)\n" +
        'f = t.extractfile(m)\n' +
        "print(hashlib.sha256(f.read()).hexdigest() if f is not None else 'MISSING')\n",
      tarPath,
      member,
    ]));
  } catch (e) {
    throw new Error(`staple gate: could not inspect ${tarPath} with python3 tarfile: ${e.message}`);
  }
  const raw = pyStdout.trim();
  const tarTicketSha256 = raw === 'MISSING' ? null : raw;

  assertUpdaterTarballIsStapled({
    tarballName: path.basename(tarPath),
    diskTicketSha256,
    tarTicketSha256,
  });
  console.log(`  staple gate: OK (tarball carries the stapled ticket, sha256 ${diskTicketSha256.slice(0, 12)}…)`);
}

// Refuse to publish a downgrade (#671 item 5). Fetches the currently-live
// manifest the same way backend/lib/blob.ts's findBlobByPathname +
// fetchBlobJson do -- list() by exact pathname, then a plain fetch of the
// blob's public CDN url -- since this script already talks to the same
// Blob store with the same token, and doesn't need a live HTTP round trip
// through app.petal.live to see what's actually published. Fails closed if
// the live version is >= the version being published now: nothing else
// stops publishing 0.8.3 over 0.8.4 today.
async function verifyNotDowngrade(newVersion) {
  const { blobs } = await list({ prefix: 'latest.json', token });
  const liveBlob = blobs.find((b) => b.pathname === 'latest.json');
  if (!liveBlob) {
    console.log('  downgrade gate: OK (no live manifest published yet)');
    return;
  }
  const res = await fetch(liveBlob.url);
  if (!res.ok) {
    throw new Error(`downgrade gate: could not fetch live latest.json (${res.status} ${res.statusText})`);
  }
  const liveManifest = await res.json();
  const liveVersion = liveManifest?.version;
  if (typeof liveVersion !== 'string') {
    throw new Error(`downgrade gate: live latest.json has no "version" string: ${JSON.stringify(liveManifest)}`);
  }
  if (isDowngradeOrSame(liveVersion, newVersion)) {
    throw new Error(
      `downgrade gate: refusing to publish ${newVersion}; live manifest is already at ${liveVersion}. ` +
        `Publishing an older or equal version would brick auto-update for everyone already on ${liveVersion}.`
    );
  }
  console.log(`  downgrade gate: OK (live ${liveVersion} -> publishing ${newVersion})`);
}

// Deploy-freshness gate (user directive 2026-08-22: the web app must always
// ship in sync with native). A native release published while meet.petal.live
// or app.petal.live still serves an older commit splits the product across
// two versions of `main` -- exactly what happened when 0.9.1 native shipped
// against a meet.petal.live still built from d227ce4d (0.9.0-era, missing an
// honest-telemetry fix). Delegates to scripts/verify-deploy-freshness.sh
// (which compares each live deployment's build commit against origin/main's
// web-harness/shared/contracts + backend/contracts subtrees) and fails
// closed on ANY failure, including an unreachable deployment or a missing
// build-info endpoint. Runs FIRST: a stale deploy should abort the publish
// before any artifact inspection. Remediation, not override: deploy the
// stale service(s) (scripts/deploy-web-harness.sh --prod --yes; cd backend
// && vercel --prod --yes -e PETAL_DEPLOY_COMMIT=$(git rev-parse HEAD)),
// then rerun.
async function verifyLiveDeploysFresh() {
  const script = process.env.PETAL_VERIFY_DEPLOY_FRESHNESS_SCRIPT ||
    fileURLToPath(new URL('verify-deploy-freshness.sh', import.meta.url));
  try {
    const { stdout, stderr } = await execFileAsync('bash', [script]);
    if (stdout.trim()) console.log(stdout.trim().replace(/^/gm, '    '));
    if (stderr.trim()) console.error(stderr.trim().replace(/^/gm, '    '));
  } catch (e) {
    if (e.stdout?.trim()) console.log(e.stdout.trim().replace(/^/gm, '    '));
    if (e.stderr?.trim()) console.error(e.stderr.trim().replace(/^/gm, '    '));
    throw new Error(
      'deploy-freshness gate: refusing to publish; a live web deployment is stale or unverifiable. ' +
        'Deploy the stale service(s) shown above, then rerun. The web app must always ship in sync with native.'
    );
  }
  console.log('  deploy-freshness gate: OK (meet.petal.live + app.petal.live match origin/main)');
}

// PETAL_PUBLISH_DRY_RUN=1 (release.yml publish=false): run every gate below
// against the freshly built artifacts, then stop before the first upload.
// The first real 0.9.5 publish failed at the PostHog-key gate -- a gate no dry
// run had ever reached, because the publisher only ran on publish (#916).
// The live-deploy freshness gate is skipped in a dry run: nothing has been
// promoted, so production is expected to be behind the tree under test.
const DRY_RUN = process.env.PETAL_PUBLISH_DRY_RUN === '1';
if (DRY_RUN) {
  console.log('  publish DRY RUN: gates only, nothing will be uploaded; deploy-freshness gate skipped (no promote in a dry run)');
} else {
  await verifyLiveDeploysFresh();
}

const dmgPath = await findOne(path.join(bundleDir, 'dmg'), '.dmg');
const tarPath = await findOne(path.join(bundleDir, 'macos'), '.app.tar.gz');
const sigPath = await findOne(path.join(bundleDir, 'macos'), '.app.tar.gz.sig');
const windowsInstallerPath = windowsBundleDir ? await findOne(windowsBundleDir, '-setup.exe') : '';
const windowsSignaturePath = windowsBundleDir ? await findOne(windowsBundleDir, '.exe.sig') : '';

if (windowsBundleDir) {
  if (!path.basename(windowsInstallerPath).includes(`_${version}_`)) {
    throw new Error(
      `Windows release artifact ${path.basename(windowsInstallerPath)} does not contain release version ${version}`
    );
  }
  if (path.basename(windowsSignaturePath) !== `${path.basename(windowsInstallerPath)}.sig`) {
    throw new Error(
      `Windows updater signature ${path.basename(windowsSignaturePath)} does not match ` +
        `installer ${path.basename(windowsInstallerPath)}`
    );
  }
}

console.log(`publish-blob: ${tag} (version ${version})`);
console.log(`  dmg: ${dmgPath}`);
console.log(`  tar: ${tarPath}`);
console.log(`  sig: ${sigPath}`);
if (windowsBundleDir) {
  console.log(`  windows installer: ${windowsInstallerPath}`);
  console.log(`  windows sig: ${windowsSignaturePath}`);
} else {
  console.log('  windows: none (macOS-only publish; latest.json will omit windows-x86_64)');
}

await verifyUniversalApp(bundleDir);
await verifyEntitlements(bundleDir);
await verifyCleanTarball(tarPath);
await verifySentryDsn(bundleDir);
await verifyPosthogKey(bundleDir);
await verifyBackendUrl(bundleDir);
await verifyStapledInsideTarball(bundleDir, tarPath);
await verifyNotDowngrade(version);

if (DRY_RUN) {
  console.log(`  publish DRY RUN: all publish gates passed for ${version}; stopping before upload.`);
  process.exit(0);
}

// Human download — stable name so /api/download can find it by prefix+suffix.
const dmgUrl = await upload(
  `Petal_${version}_universal.dmg`,
  await readFile(dmgPath),
  'application/x-apple-diskimage'
);

// Updater artifact. One verified universal tarball serves both arch keys.
// Versioned pathname (#671 item 2) -- see the file-header comment for why.
const tarUrl = await upload(
  updaterTarballPathname(version),
  await readFile(tarPath),
  'application/gzip'
);

const signature = (await readFile(sigPath, 'utf8')).trim();
if (!signature) throw new Error(`macOS updater signature is empty: ${sigPath}`);

// Windows NSIS is intentionally unsigned by Authenticode for now. Its Tauri
// updater signature is still mandatory and is generated from these exact bytes.
let windowsUrl = '';
let windowsSignature = '';
if (windowsBundleDir) {
  windowsSignature = (await readFile(windowsSignaturePath, 'utf8')).trim();
  if (!windowsSignature) throw new Error(`Windows updater signature is empty: ${windowsSignaturePath}`);
  windowsUrl = await upload(
    windowsInstallerPathname(version),
    await readFile(windowsInstallerPath),
    'application/vnd.microsoft.portable-executable'
  );
}

// One combined manifest is written last, after every platform artifact upload.
const manifest = buildUpdaterManifest({
  version,
  notes: resolveReleaseNotes(tagAnnotation, tag),
  pubDate: new Date().toISOString(),
  darwinUrl: tarUrl,
  darwinSignature: signature,
  windowsUrl,
  windowsSignature,
});

await upload('latest.json', JSON.stringify(manifest, null, 2) + '\n', 'application/json');

console.log(`publish-blob: done. dmg=${dmgUrl}${windowsUrl ? ` windows=${windowsUrl}` : ' (macOS-only)'}`);
