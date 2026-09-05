import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { tokenEndpointPlugin } from './server/tokenPlugin';
import { type DesktopMetadata, resolveBuildVersion } from './src/buildInfo';
import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

type PackageMetadata = { version?: unknown };

function readPackage(path: URL, source: string): PackageMetadata {
  try {
    const parsed = JSON.parse(readFileSync(path, 'utf8')) as unknown;
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
      throw new Error(`${source} must contain a JSON object`);
    }
    return parsed as PackageMetadata;
  } catch (error) {
    const wrapped = new Error(`Unable to read ${source}: ${error instanceof Error ? error.message : String(error)}`);
    if (typeof error === 'object' && error !== null && 'code' in error) {
      (wrapped as Error & { code?: unknown }).code = error.code;
    }
    throw wrapped;
  }
}

function packageVersion(pkg: PackageMetadata, source: string): unknown {
  if (!Object.prototype.hasOwnProperty.call(pkg, 'version')) {
    throw new Error(`${source} is missing its version field`);
  }
  return pkg.version;
}

function readDesktopMetadata(path: URL): DesktopMetadata {
  try {
    return {
      status: 'present',
      version: packageVersion(readPackage(path, 'apps/desktop/package.json'), 'apps/desktop/package.json'),
    };
  } catch (error) {
    if (typeof error === 'object' && error !== null && 'code' in error && error.code === 'ENOENT') {
      return { status: 'missing' };
    }
    throw error;
  }
}

function fullCommit(): string | null {
  // Vercel's build sandbox does not check out `.git` for a plain `vercel
  // --prod` CLI deploy, so `git rev-parse` silently fails there (confirmed
  // live: the footer had been rendering "dev" in production this whole
  // time). PETAL_DEPLOY_COMMIT is the reliable source when it's set (pass
  // `-b PETAL_DEPLOY_COMMIT=$(git rev-parse HEAD)` on deploy, see
  // docs/RELEASING.md); git itself is the fallback for local dev/build,
  // where `.git` really is present.
  const fromEnv = process.env.PETAL_DEPLOY_COMMIT?.trim();
  if (fromEnv) return fromEnv;
  try {
    return execSync('git rev-parse HEAD', { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

function buildInfo(commit: string | null) {
  const webPackageUrl = new URL('./package.json', import.meta.url);
  const desktopPackageUrl = new URL('../apps/desktop/package.json', import.meta.url);
  const webPackage = readPackage(webPackageUrl, 'web-harness/package.json');
  const version = resolveBuildVersion(
    packageVersion(webPackage, 'web-harness/package.json'),
    readDesktopMetadata(desktopPackageUrl),
    { allowMissingDesktopMetadata: process.env.VERCEL === '1' }
  );

  return {
    version,
    // Non-git deployments still render a stable footer.
    commit: commit ? commit.slice(0, 7) : 'dev',
    buildDate: new Date().toISOString().slice(0, 10),
  };
}

// Emits /build-info.json into the static output so
// scripts/verify-deploy-freshness.sh can confirm the LIVE deployment was
// actually built from the commit it claims, rather than a stale `main`
// (backend/web-harness deploy separately from `git push` -- see that
// script's header for the two incidents this closes). Full SHA, unlike the
// footer's short one, so ancestry can be checked unambiguously with
// `git merge-base --is-ancestor`.
function buildInfoFile(commit: string | null) {
  return {
    name: 'petal-build-info-file',
    apply: 'build' as const,
    generateBundle() {
      this.emitFile({
        type: 'asset' as const,
        fileName: 'build-info.json',
        source: JSON.stringify({ commit }),
      });
    },
  };
}

// Single dev server, single port -- the token-minting middleware is wired
// in here via configureServer (see server/tokenPlugin.ts) instead of a
// second process, so joining a room never requires running anything besides
// `npm run dev`.
const commit = fullCommit();

// #788: web crash reporting defaults ON for every build path (owner-decided
// posture, priority rubric 2026-08-24 -- "always have the data"). The default
// lives HERE, not in vercel.json's buildCommand, because Vercel rejects a
// buildCommand over 256 characters (learned deploying the inline form). An
// explicit VITE_SENTRY_DSN env var still wins; deploy-web-harness.sh's
// post-build gate verifies the DSN actually landed in the bundle either way.
const DEFAULT_SENTRY_DSN =
  'https://0e3aed022eea70d6e9c68b1804253e69@o4510882392899584.ingest.us.sentry.io/4511711774375937';

export default defineConfig({
  define: {
    __PETAL_BUILD_INFO__: JSON.stringify(buildInfo(commit)),
    'import.meta.env.VITE_SENTRY_DSN': JSON.stringify(
      process.env.VITE_SENTRY_DSN || DEFAULT_SENTRY_DSN
    ),
  },
  plugins: [svelte(), tokenEndpointPlugin(), buildInfoFile(commit)],
  // `@petal/shared` -> repo-root shared/ package (design tokens, shared
  // components, shared logic). One location consumed by BOTH this client and
  // the desktop app — never duplicate files into per-client copies.
  //
  // Resolved through `web-harness/shared`, a symlink to `../shared`, rather
  // than `../shared` directly (#662): `vercel deploy` uploads only the
  // invocation cwd, never a parent, so a deploy invoked from `web-harness/`
  // never carries `../shared` to Vercel's remote build at all -- every
  // deploy since shared/ was introduced (96f1bfd3) failed with "Cannot find
  // module '@petal/shared/...'". `scripts/deploy-web-harness.sh` dereferences
  // the symlink into a real copy before uploading, so the deploy stays
  // self-contained without duplicating shared/ in the repo itself.
  resolve: {
    alias: {
      '@petal/shared': resolve(import.meta.dirname, 'shared'),
    },
    // Vite resolves the `shared` symlink to its real, outside-the-project
    // target before serving it, so the dev server still needs the same
    // outside-root allowance the old `../shared` alias needed.
    preserveSymlinks: false,
  },
  server: {
    fs: {
      allow: ['..'],
    },
  },
  build: {
    rollupOptions: {
      input: {
        meeting: resolve(import.meta.dirname, 'index.html'),
        fidelity: resolve(import.meta.dirname, 'fidelity.html'),
      },
    },
  },
});
