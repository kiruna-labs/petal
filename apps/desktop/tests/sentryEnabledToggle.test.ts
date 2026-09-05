import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// `session.svelte.ts` uses Svelte 5 runes ($state) at module scope, so it
// cannot be imported and executed directly under plain `node --test` (no
// Svelte compilation in this test runner) -- same constraint
// `factoryReset.test.ts`'s `resetOnboarding` test already works around by
// asserting against the raw source text instead of importing/running it.
const sessionStoreSource = readFileSync(
  new URL('../src/lib/stores/session.svelte.ts', import.meta.url),
  'utf8'
);

test('sentryEnabled defaults to true and is part of the persisted session shape', () => {
  assert.match(sessionStoreSource, /sentryEnabled:\s*boolean;/);
  assert.match(sessionStoreSource, /sentryEnabled:\s*true/);
});

test('updateSentryEnabled persists the new value and syncs it to Rust via invoke', () => {
  const fnMatch = sessionStoreSource.match(
    /export function updateSentryEnabled\(enabled: boolean\) \{[\s\S]*?\n\}/
  );
  assert.ok(fnMatch, 'updateSentryEnabled must be exported from session.svelte.ts');
  const fnBody = fnMatch[0];

  // Updates the in-memory rune state.
  assert.match(fnBody, /session\.sentryEnabled = enabled;/);
  // Persists to localStorage, same as every other session mutator.
  assert.match(fnBody, /persist\(session\);/);
  // Calls the real Rust command, gated by hasTauriBridge() like
  // updateRemoteControlDefault's setRemoteControlAllowed call, with the
  // right command name and arg shape.
  assert.match(fnBody, /hasTauriBridge\(\)/);
  assert.match(
    fnBody,
    /invoke\(COMMANDS\.setSentryEnabled,\s*\{\s*enabled\s*\}\)\.catch\(\(\) => \{\}\);/
  );
});
