import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync } from 'node:fs';

// Static-source assertions, matching the pattern in shareControl.test.ts:
// these check the *shape* of the wiring (gating, absence of forbidden
// integrations) directly against the source text, rather than exercising
// the real Sentry SDK against a network in a headless unit test.
const sentryReporting = readFileSync(new URL('../src/sentryReporting.ts', import.meta.url), 'utf8');
const main = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');
const allSrcFiles = [
  'sentryReporting.ts',
  'sensitiveStrings.ts',
  'main.ts',
  'connection.ts',
  'context.ts',
].map((name) => readFileSync(new URL(`../src/${name}`, import.meta.url), 'utf8'));

test('Sentry.init is gated on VITE_SENTRY_DSN and is a no-op when it is unset', () => {
  assert.match(sentryReporting, /const dsn = sentryDsn\(\);\s*\n\s*if \(!dsn\) return false;/);
  // The gate must run before Sentry.init is ever called.
  const dsnCheckIndex = sentryReporting.indexOf('if (!dsn) return false;');
  const initCallIndex = sentryReporting.indexOf('Sentry.init(');
  assert.ok(dsnCheckIndex > -1 && initCallIndex > -1, 'expected both the DSN guard and Sentry.init call to be present');
  assert.ok(dsnCheckIndex < initCallIndex, 'the DSN guard must appear before Sentry.init is called');
});

test('VITE_SENTRY_DSN is read from import.meta.env, matching the VITE_PETAL_BACKEND_URL convention', () => {
  assert.match(sentryReporting, /VITE_SENTRY_DSN/);
  assert.match(sentryReporting, /import\.meta as ViteImportMeta/);
});

test('Session Replay and Tracing/Performance integrations are never referenced anywhere in src, regardless of DSN', () => {
  for (const source of allSrcFiles) {
    assert.doesNotMatch(source, /replayIntegration/);
    assert.doesNotMatch(source, /browserTracingIntegration/);
    assert.doesNotMatch(source, /browserProfilingIntegration/);
    assert.doesNotMatch(source, /replayCanvasIntegration/);
  }
});

test('tracesSampleRate is pinned to 0 and sendDefaultPii is explicitly false (not left to SDK default)', () => {
  assert.match(sentryReporting, /tracesSampleRate:\s*0\b/);
  assert.match(sentryReporting, /sendDefaultPii:\s*false\b/);
});

test('maxBreadcrumbs is pinned to a small bounded value', () => {
  assert.match(sentryReporting, /maxBreadcrumbs:\s*MAX_BREADCRUMBS/);
  assert.match(sentryReporting, /const MAX_BREADCRUMBS = 50;/);
});

test('every breadcrumb and event passes through the sensitive-string scrub before Sentry can send it', () => {
  assert.match(sentryReporting, /beforeBreadcrumb:\s*\(breadcrumb\)\s*=>\s*scrubBreadcrumb\(breadcrumb, registry\)/);
  assert.match(sentryReporting, /beforeSend:\s*\(event\)\s*=>\s*scrubEvent\(event, registry\)/);
  assert.match(sentryReporting, /scrubbed\.message = registry\.scrub\(scrubbed\.message\)/);
});

test('main.ts mirrors uncaught errors into the local session log independent of whether Sentry is configured', () => {
  assert.match(main, /installGlobalErrorMirror\(logEvent\)/);
  // installGlobalErrorMirror must be called unconditionally (not behind a DSN
  // check) -- it must not be inside an `if` guarding initSentry.
  const mirrorCallIndex = main.indexOf('installGlobalErrorMirror(logEvent)');
  const precedingLines = main.slice(0, mirrorCallIndex).split('\n').slice(-3).join('\n');
  assert.doesNotMatch(precedingLines, /if\s*\(/);
});

test('main.ts initializes Sentry near the __petalHarness debug-hook setup', () => {
  const hookIndex = main.indexOf('__petalHarness');
  const initIndex = main.indexOf('initSentry(logEvent)');
  assert.ok(hookIndex > -1 && initIndex > -1);
  assert.ok(initIndex > hookIndex, 'initSentry should be wired after the __petalHarness hook is assigned');
  assert.ok(initIndex - hookIndex < 1500, 'initSentry should be wired close to the __petalHarness setup, not far away');
});
