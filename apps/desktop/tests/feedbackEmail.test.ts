import assert from 'node:assert/strict';
import test from 'node:test';

import {
  FEEDBACK_EMAIL_MAX_CHARS,
  feedbackEmailError,
  isValidFeedbackEmail,
  normalizedFeedbackEmail,
  feedbackEmailStorage,
  rememberFeedbackEmail,
  rememberedFeedbackEmail
} from '../src/lib/feedback/email.ts';
import { STORAGE_KEYS } from '../src/lib/data/storageKeys.ts';

// -------------------------------------------------------------------------
// #245: the reply address every feedback form requires. A format check only
// -- nothing is ever sent to the address to confirm it. Mirrors
// web-harness/tests/feedbackEmail.test.ts's coverage of the same shared
// module (shared/logic/feedbackEmail.ts), through this app's re-export.
// -------------------------------------------------------------------------

test('#245: a well-formed address is accepted and comes back trimmed', () => {
  assert.equal(normalizedFeedbackEmail('riley@example.org'), 'riley@example.org');
  assert.equal(normalizedFeedbackEmail('  riley@example.org \n'), 'riley@example.org');
  assert.equal(normalizedFeedbackEmail('first.last+petal@mail.example.co.uk'), 'first.last+petal@mail.example.co.uk');
  assert.equal(normalizedFeedbackEmail('Riley@Example.ORG'), 'Riley@Example.ORG');
  // Deliberately loose: internationalized domains and local parts,
  // plus-addressing, and quoted local parts all pass.
  assert.equal(normalizedFeedbackEmail('riley@bücher.example'), 'riley@bücher.example');
  assert.equal(normalizedFeedbackEmail('rílęy@example.org'), 'rílęy@example.org');
  assert.equal(normalizedFeedbackEmail('"riley.q"@example.org'), '"riley.q"@example.org');
});

test('#245: a pasted address is unwrapped -- one pair of angle brackets and a leading mailto:', () => {
  for (const [value, expected] of [
    ['<riley@example.org>', 'riley@example.org'],
    [' < riley@example.org > ', 'riley@example.org'],
    ['mailto:riley@example.org', 'riley@example.org'],
    ['MAILTO:riley@example.org', 'riley@example.org'],
    ['<mailto:riley@example.org>', 'riley@example.org'],
  ] as const) {
    assert.equal(normalizedFeedbackEmail(value), expected, JSON.stringify(value));
  }
  // Only ONE pair, and only surrounding ones.
  assert.equal(normalizedFeedbackEmail('<<riley@example.org>>'), null);
  assert.equal(normalizedFeedbackEmail('Riley <riley@example.org>'), null);
});

test('#245: malformed addresses are rejected', () => {
  for (const value of [
    '',
    '   ',
    null,
    undefined,
    'riley.example.org',
    'riley@@example.org',
    'riley@mail@example.org',
    '@example.org',
    'riley@example',
    'riley@',
    'riley@.org',
    'riley@example.',
    'riley@example..org',
    'riley @example.org',
    'riley@exa mple.org',
    'riley@example.org\tx',
    'riley@example.org,', // list and display-name/comment syntax
    'riley@example.org; kai@example.org',
    'riley(work)@example.org',
    'riley@example.org>',
    '<riley@example.org',
    'riley/x@example.org',
    'riley\\x@example.org',
    'ri\u200bley@example.org', // zero-width characters pasted along with it
    'riley@example.org\u200b',
    'riley\u2060@example.org',
    'riley@exa\u00admple.org'
  ]) {
    assert.equal(normalizedFeedbackEmail(value), null, `${JSON.stringify(value)} must be rejected`);
    assert.equal(isValidFeedbackEmail(value), false);
  }
});

test('#245: at most 254 characters', () => {
  const domain = '@example.org';
  const longest = `${'a'.repeat(FEEDBACK_EMAIL_MAX_CHARS - domain.length)}${domain}`;
  assert.equal(longest.length, 254);
  assert.equal(isValidFeedbackEmail(longest), true);
  assert.equal(isValidFeedbackEmail(`a${longest}`), false);
});

test('#245: the inline error tells an empty, a too-long, and a malformed field apart', () => {
  assert.equal(feedbackEmailError(''), 'Enter your email address.');
  assert.equal(feedbackEmailError('<>'), 'Enter your email address.');
  assert.equal(feedbackEmailError(`${'a'.repeat(250)}@example.org`), 'This email address is too long.');
  assert.equal(feedbackEmailError('riley@example'), 'Enter a valid email address, like name@example.com.');
  assert.equal(feedbackEmailError('riley@example.org'), null);
  assert.equal(feedbackEmailError('<mailto:riley@example.org>'), null);
});

class MemoryStorage {
  values = new Map<string, string>();
  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }
}

test('#245: the last address is remembered normalized under the factory-reset key, and only when well-formed', () => {
  const storage = new MemoryStorage();
  const key = STORAGE_KEYS.feedbackEmail;
  assert.equal(rememberedFeedbackEmail(storage, key), '');

  rememberFeedbackEmail(storage, key, 'not an address');
  assert.equal(storage.values.size, 0);

  rememberFeedbackEmail(storage, key, '  <mailto:riley@example.org> ');
  assert.equal(storage.getItem(STORAGE_KEYS.feedbackEmail), 'riley@example.org');
  assert.equal(rememberedFeedbackEmail(storage, key), 'riley@example.org');

  // A value that is no longer well-formed is never prefilled.
  storage.setItem(key, 'riley@example');
  assert.equal(rememberedFeedbackEmail(storage, key), '');
});

test('#245: storage that is missing or throws never breaks the form', () => {
  const key = STORAGE_KEYS.feedbackEmail;
  assert.equal(rememberedFeedbackEmail(undefined, key), '');
  assert.equal(rememberedFeedbackEmail(null, key), '');
  rememberFeedbackEmail(undefined, key, 'riley@example.org');
  const throwing = {
    getItem(): string | null {
      throw new Error('SecurityError');
    },
    setItem(): void {
      throw new Error('QuotaExceededError');
    }
  };
  assert.equal(rememberedFeedbackEmail(throwing, key), '');
  assert.doesNotThrow(() => rememberFeedbackEmail(throwing, key, 'riley@example.org'));
});

test('#245: the storage accessor reports no storage where reading localStorage itself throws', () => {
  // Blocked site data makes the `localStorage` getter throw a SecurityError.
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    get() {
      throw new Error('SecurityError');
    }
  });
  try {
    assert.equal(feedbackEmailStorage(), null);
  } finally {
    if (previous) Object.defineProperty(globalThis, 'localStorage', previous);
    else delete (globalThis as { localStorage?: unknown }).localStorage;
  }
});
