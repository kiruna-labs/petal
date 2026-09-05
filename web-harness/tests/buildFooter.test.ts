import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { resolveBuildVersion } from '../src/buildInfo';
import { escapeRegExp } from '../src/escapeRegExp.mjs';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const main = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');
const style = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const viteConfig = readFileSync(new URL('../vite.config.ts', import.meta.url), 'utf8');
const liveVerifier = readFileSync(new URL('../../scripts/verify-web-harness-live.sh', import.meta.url), 'utf8');
const browserVerifier = readFileSync(new URL('../../scripts/verify-web-harness-browser.mjs', import.meta.url), 'utf8');
const releaseWorkflow = readFileSync(new URL('../../.github/workflows/release.yml', import.meta.url), 'utf8');
// #671 extracted the inline version-lockstep Node script release.yml used to
// carry directly into scripts/version-lockstep.mjs (release.yml now just
// calls it) -- the literal field/path names below moved there with it.
const versionLockstepScript = readFileSync(
  new URL('../../scripts/version-lockstep.mjs', import.meta.url),
  'utf8'
);
const webPackage = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')) as { version: string };
const desktopPackage = JSON.parse(
  readFileSync(new URL('../../apps/desktop/package.json', import.meta.url), 'utf8')
) as { version: string };

test('root footer has the accessible OS-aware desktop download anchor', () => {
  const footer = /<footer id="build-version"[^>]*>(?<body>[\s\S]*?)<\/footer>/.exec(html)?.groups?.body ?? '';
  assert.match(
    footer,
    /<a\s+id="desktop-download"\s+class="web-status-bar__download"\s+href="https:\/\/app\.petal\.live\/api\/download\?platform=macos"\s+rel="noreferrer"\s*>Download Petal for macOS<\/a>/
  );
  assert.match(footer, /id="build-version-text"/);
  assert.doesNotMatch(footer, /Download Petal for macOS.*#build-version-text/);
});

test('main updates the version child and selects a deterministic desktop download platform', () => {
  assert.match(main, /querySelector<HTMLElement>\('#build-version-text'\)/);
  assert.match(main, /querySelector<HTMLAnchorElement>\('#desktop-download'\)/);
  assert.match(main, /api\/download\?platform=\$\{platform\}/);
  assert.match(main, /Download Petal for Windows/);
  assert.match(main, /Download Petal for macOS/);
  assert.match(main, /buildVersion\.textContent\s*=\s*`v\$\{buildInfo\.version\} · \$\{buildInfo\.commit\} · \$\{buildInfo\.buildDate\}`/);
  assert.doesNotMatch(main, /querySelector<HTMLElement>\('#build-version'\)[\s\S]*?textContent/);
});

test('web and desktop release mirrors are equal and nonzero', () => {
  assert.equal(webPackage.version, desktopPackage.version);
  assert.notEqual(webPackage.version, '0.0.0');
  assert.equal(
    resolveBuildVersion(webPackage.version, { status: 'present', version: desktopPackage.version }),
    webPackage.version
  );
});

test('version resolver allows isolated builds but rejects invalid or drifting metadata', () => {
  assert.equal(
    resolveBuildVersion('0.7.10', { status: 'missing' }, { allowMissingDesktopMetadata: true }),
    '0.7.10'
  );
  assert.throws(
    () => resolveBuildVersion('0.7.10', { status: 'missing' }),
    /missing outside an isolated Vercel build/
  );
  assert.throws(
    () => resolveBuildVersion(undefined, { status: 'present', version: '0.7.10' }),
    /web-harness version/
  );
  assert.throws(
    () => resolveBuildVersion('0.7.10', { status: 'unreadable', message: 'EACCES' }),
    /could not be read: EACCES/
  );
  assert.throws(
    () => resolveBuildVersion('0.7.10', { status: 'present', version: undefined }),
    /valid semantic version/
  );
  assert.throws(
    () => resolveBuildVersion('0.7.10', { status: 'present', version: '0.0.0' }),
    /must not be 0\.0\.0/
  );
  assert.throws(
    () => resolveBuildVersion('not-a-version', { status: 'present', version: '0.7.10' }),
    /valid semantic version/
  );
  assert.throws(
    () => resolveBuildVersion('0.7.10', { status: 'present', version: 'not-a-version' }),
    /desktop version/
  );
  assert.throws(
    () => resolveBuildVersion('0.7.10', { status: 'present', version: '0.7.9' }),
    /does not match/
  );

  for (const invalidVersion of [
    '1.2.3-..',
    '1.2.3-01',
    '1.2.3-rc..1',
    '1.2.3-rc.01',
    '1.2.3+',
    '1.2.3+build..1',
    '1.2.3+build_',
  ]) {
    assert.throws(
      () => resolveBuildVersion(invalidVersion, { status: 'present', version: '0.7.10' }),
      /valid semantic version/,
      invalidVersion
    );
  }

  assert.equal(
    resolveBuildVersion('1.2.3+build.1', { status: 'present', version: '1.2.3+build.1' }),
    '1.2.3+build.1'
  );
});

test('footer CSS wraps, preserves full copy, and exposes keyboard-safe touch targets', () => {
  assert.match(style, /\.web-status-bar\s*\{[\s\S]*display:\s*flex;[\s\S]*flex-wrap:\s*wrap;/);
  assert.match(style, /\.web-status-bar__download\s*\{[\s\S]*min-height:\s*44px;/);
  assert.match(style, /\.web-status-bar__download:focus-visible\s*\{/);
  assert.match(style, /safe-area-inset-bottom/);
  assert.match(style, /@media\s*\(max-width:\s*420px\)[\s\S]*\.web-status-bar\s*\{[\s\S]*flex-direction:\s*column;/);
  assert.doesNotMatch(style, /\.web-status-bar__download\s*\{[^}]*text-overflow\s*:/);
});

test('release workflow guards the web package and both lockfile mirrors', () => {
  // #671 moved the inline lockstep script out of release.yml (which now just
  // calls it) into scripts/version-lockstep.mjs -- check the file that
  // actually carries these fields/paths now.
  assert.match(releaseWorkflow, /version-lockstep\.mjs/);
  assert.match(versionLockstepScript, /web-harness\/package\.json/);
  assert.match(versionLockstepScript, /web-harness\/package-lock\.json/);
  assert.match(versionLockstepScript, /desktopLock/);
  assert.match(versionLockstepScript, /desktopLockPackage/);
  assert.match(versionLockstepScript, /webPackage/);
  assert.match(versionLockstepScript, /webLock/);
  assert.match(versionLockstepScript, /webLockPackage/);
});

test('live and browser verifiers use actual SPA assets and keep browser checks opt-in', () => {
  assert.match(viteConfig, /error\.code === 'ENOENT'/);
  assert.match(viteConfig, /process\.env\.VERCEL === '1'/);
  assert.match(liveVerifier, /replace\(\/\\s\+\/g, " "\)/);
  assert.match(liveVerifier, /assets\/meeting-\[\^"\\\\s\]\+/);
  assert.match(liveVerifier, /PETAL_EXPECTED_VERSION:-/);
  assert.match(liveVerifier, /strictSemver\.mjs/);
  assert.match(liveVerifier, /isStrictSemVer/);
  assert.match(browserVerifier, /PETAL_BROWSER_URL/);
  assert.match(browserVerifier, /\[320, 380, 400, 420\]/);
  assert.match(browserVerifier, /build-version-text/);
  assert.match(browserVerifier, /api\/download/);
  assert.match(browserVerifier, /scrollWidth/);
});

test('browser verifier escapes semver build metadata in the expected version regex', () => {
  const expectedVersion = '1.2.3+build.1';
  const versionPattern = new RegExp(`^v${escapeRegExp(expectedVersion)} · .+ · \\d{4}-\\d{2}-\\d{2}$`);

  assert.match('v1.2.3+build.1 · dev · 2026-07-22', versionPattern);
  assert.doesNotMatch('v1x2x3+buildx1 · dev · 2026-07-22', versionPattern);
});
