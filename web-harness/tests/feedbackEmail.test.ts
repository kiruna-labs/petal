import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  FEEDBACK_EMAIL_MAX_CHARS,
  feedbackEmailError,
  isValidFeedbackEmail,
  normalizedFeedbackEmail,
  rememberedFeedbackEmail,
  rememberFeedbackEmail,
  type FeedbackEmailStorage,
} from '@petal/shared/logic/feedbackEmail';

// -------------------------------------------------------------------------
// #245: the reply address every feedback form requires. A format check only
// -- nothing is ever sent to the address to confirm it. Mirrors
// apps/desktop/tests/feedbackEmail.test.ts's coverage of the same shared
// module (shared/logic/feedbackEmail.ts).
// -------------------------------------------------------------------------

test('#245: a well-formed address is accepted and comes back trimmed', () => {
  assert.equal(normalizedFeedbackEmail('riley@example.org'), 'riley@example.org');
  assert.equal(normalizedFeedbackEmail('  riley@example.org \n'), 'riley@example.org');
  assert.equal(normalizedFeedbackEmail('first.last+petal@mail.example.co.uk'), 'first.last+petal@mail.example.co.uk');
  // Case is kept as typed; the provider decides what case means.
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
    'riley.example.org', // no @
    'riley@@example.org', // two @
    'riley@mail@example.org',
    '@example.org', // empty local part
    'riley@example', // no dot in the domain
    'riley@', // no domain
    'riley@.org', // empty label either side of the dot
    'riley@example.',
    'riley@example..org',
    'riley @example.org', // spaces
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
    'riley@exa\u00admple.org',
  ]) {
    assert.equal(normalizedFeedbackEmail(value), null, `${JSON.stringify(value)} must be rejected`);
    assert.equal(isValidFeedbackEmail(value), false);
  }
});

test('#245: at most 254 characters', () => {
  const domain = '@example.org';
  const longest = `${'a'.repeat(FEEDBACK_EMAIL_MAX_CHARS - domain.length)}${domain}`;
  assert.equal(FEEDBACK_EMAIL_MAX_CHARS, 254);
  assert.equal(longest.length, 254);
  assert.equal(isValidFeedbackEmail(longest), true);
  assert.equal(isValidFeedbackEmail(`a${longest}`), false);
  // Measured after trimming, so surrounding whitespace never tips it over.
  assert.equal(isValidFeedbackEmail(`  ${longest}  `), true);
});

test('#245: the inline error tells an empty, a too-long, and a malformed field apart', () => {
  assert.equal(feedbackEmailError(''), 'Enter your email address.');
  assert.equal(feedbackEmailError('   '), 'Enter your email address.');
  assert.equal(feedbackEmailError('<>'), 'Enter your email address.');
  assert.equal(feedbackEmailError(`${'a'.repeat(250)}@example.org`), 'This email address is too long.');
  assert.equal(feedbackEmailError('riley@example'), 'Enter a valid email address, like name@example.com.');
  assert.equal(feedbackEmailError(' riley@example.org '), null);
  assert.equal(feedbackEmailError('<mailto:riley@example.org>'), null);
});

test('#245: the last address is remembered normalized, only when well-formed, and storage can never break the form', () => {
  const values = new Map<string, string>();
  const storage: FeedbackEmailStorage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => void values.set(key, value),
  };
  const key = 'test-feedback-email';
  assert.equal(rememberedFeedbackEmail(storage, key), '');
  rememberFeedbackEmail(storage, key, 'not an address');
  assert.equal(values.size, 0);
  rememberFeedbackEmail(storage, key, ' <mailto:riley@example.org> ');
  assert.equal(values.get(key), 'riley@example.org');
  assert.equal(rememberedFeedbackEmail(storage, key), 'riley@example.org');
  // A stored value that is no longer well-formed is never prefilled.
  values.set(key, 'riley@example');
  assert.equal(rememberedFeedbackEmail(storage, key), '');

  const throwing: FeedbackEmailStorage = {
    getItem() { throw new Error('SecurityError'); },
    setItem() { throw new Error('QuotaExceededError'); },
  };
  assert.equal(rememberedFeedbackEmail(throwing, key), '');
  assert.doesNotThrow(() => rememberFeedbackEmail(throwing, key, 'riley@example.org'));
  assert.equal(rememberedFeedbackEmail(null, key), '');
  assert.doesNotThrow(() => rememberFeedbackEmail(undefined, key, 'riley@example.org'));
});
